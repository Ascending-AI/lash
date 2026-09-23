//! The opener-owned side of an effect group's lifecycle (ADR 0099 §6, §7;
//! FIG-3397).
//!
//! A consumer that stops before a group is exhausted — `Promise.race` at its
//! first settlement, `Promise.any` at its first success, `Promise.all` at its
//! first rejection — leaves losers running, exactly as a losing promise keeps
//! running in ECMA-262 (§0 *live*). The group then belongs to the **opener**,
//! not to the aggregate that opened it: this module is where the opener keeps
//! it until its own end, and the one place that end closes it.
//!
//! # One state per opener, shared by every phase
//!
//! A turn builds a fresh [`RuntimeExecutionContext`] per phase — one per code
//! cell, one per tool batch — and a process builds one per segment. What must
//! outlive a phase is the opener's, so [`OpenerState`] is two shared handles
//! the owner creates once and hands to every phase context:
//!
//! * the [`IncorporationLedger`](super::IncorporationLedger), so a rank the
//!   consumer incorporated in the cell that ran the race is not incorporated
//!   (and its usage charged) a second time when the turn's end incorporates
//!   the losers (§6, §13);
//! * the registry of groups whose consumer stopped early, with each group's
//!   own cursor, so the end knows which groups still hold unconsumed ranks.
//!
//! # Opener end
//!
//! [`RuntimeExecutionContext::close_opener_groups`] is §7 from the opener's
//! side, run by every final turn exit and every process terminal — success,
//! failure and cancellation alike — and never by worker loss or a segment
//! handover:
//!
//! 1. every group the opener still holds is closed under
//!    [`LoserPolicy::Cancel`], which durably records `closing` before any
//!    cancel decision is issued and cancel-decides each child whose final
//!    record has not committed (§4); a committed child keeps its authority to
//!    drain its intents;
//! 2. on a tier with a closing seam, the unsettled groups the journal holds
//!    under the opener's scope whose keys the opener formed are finished too:
//!    a `live` one — a turn resumed after a crash serves its completed cells
//!    from the journal, so the groups those cells opened are known to the
//!    journal and not to this process — is closed the same way, and a
//!    `closing` one — an earlier end recorded it and failed before it
//!    finished — resumes. Every group the end handles is then finalized with
//!    the opener's own steps. On Restate, whose engine-side index is the
//!    closing twin, the opener waits out each held group's remaining ranks;
//! 3. every closed group's settled ranks are incorporated through the
//!    journaled `IncorporateGroupSettlements` record.
//!
//! A group whose consumer was cancelled is held too: the cancel closes it,
//! and a rank that lands after that close — a committed loser's drain, a
//! cancelled attempt's captured usage — is still the opener's to incorporate.
//!
//! # Retirement under a live opener
//!
//! A held group's units count against the opener's bound (§9) until the group
//! retires. When a new group would pass the bound, the opener retires its
//! oldest held group as a whole — its losers run to their own terminals, their
//! ranks are incorporated, the group is closed — and tries again; the bound
//! refuses only when nothing is left to retire.
//!
//! Step 3 runs after finalization rather than only inside it because the
//! host's own close spawns a finalizer with no opener steps (the closing
//! driver's `GroupOnlyFinalization`), and either finalizer may be the one that
//! records step 2. Incorporation is idempotent through the ledger and the
//! journaled prefix record, so running it here makes the opener's accounting
//! independent of which finalizer won.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

use super::execution_context::RuntimeExecutionContext;
use super::settlement_incorporation::ContextFinalizationSteps;
use crate::runtime::effect::LoserPolicy;
use crate::runtime::effect::executor::RuntimeEffectControllerError;

/// How much effect-group work one logical opener may retain at once
/// (ADR 0099 §9).
///
/// The unit is the **unique child execution**: a group reserves one unit per
/// child it admits — tool invocation or timer — from the moment it is accepted
/// until its opener no longer depends on it. Operand positions are not host
/// work: a handle written at two positions is one child, and the
/// position-to-child mapping lives in the VM (§10 L4, §11 clause 1). A child
/// counts while it is accepted but unclaimed, running, closing, or settled and
/// still required — settled-but-unconsumed ranks and unincorporated facts are
/// exactly the retained state §9 refuses to leave unbounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenerWorkBound {
    max_retained_children: std::num::NonZeroUsize,
}

impl OpenerWorkBound {
    /// The default: 1024 unique children per opener — forty times the
    /// standard protocol's widest single tool batch, and far beyond what one
    /// cell's aggregates retain in practice.
    pub const DEFAULT: Self = Self {
        max_retained_children: match std::num::NonZeroUsize::new(1024) {
            Some(value) => value,
            None => unreachable!(),
        },
    };

    /// A bound of `max_retained_children` unique children per opener.
    #[must_use]
    pub const fn new(max_retained_children: std::num::NonZeroUsize) -> Self {
        Self {
            max_retained_children,
        }
    }

    /// How many unique children one opener may retain at once.
    #[must_use]
    pub const fn max_retained_children(self) -> usize {
        self.max_retained_children.get()
    }
}

impl Default for OpenerWorkBound {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The groups an opener holds after their consumer stopped early, in the
/// order they were handed over, and the work every group it formed reserves.
#[derive(Debug, Default)]
pub struct OpenerGroupRegistry {
    outstanding: Vec<crate::EffectGroupHandle>,
    /// Children reserved per group key, from formation to release (§9).
    reserved: std::collections::BTreeMap<String, usize>,
    bound: OpenerWorkBound,
}

impl OpenerGroupRegistry {
    /// Group keys and cursors of every group the opener still holds.
    #[must_use]
    pub fn outstanding(&self) -> Vec<(String, usize, usize)> {
        self.outstanding
            .iter()
            .map(|handle| {
                (
                    handle.group_key().to_string(),
                    handle.children(),
                    handle.consumed(),
                )
            })
            .collect()
    }
}

/// What an opener shares across the phase contexts it builds: the once-only
/// incorporation ledger and the registry of groups it still holds.
///
/// Cloning shares both; [`OpenerState::new`] starts a fresh opener.
#[derive(Clone, Debug, Default)]
pub struct OpenerState {
    pub(crate) ledger: Arc<std::sync::Mutex<super::IncorporationLedger>>,
    pub(crate) groups: Arc<std::sync::Mutex<OpenerGroupRegistry>>,
}

impl OpenerState {
    /// A fresh opener that may retain at most `bound` of group work.
    #[must_use]
    pub fn new(bound: OpenerWorkBound) -> Self {
        Self {
            ledger: Arc::default(),
            groups: Arc::new(std::sync::Mutex::new(OpenerGroupRegistry {
                bound,
                ..OpenerGroupRegistry::default()
            })),
        }
    }

    /// What the opener has incorporated so far.
    #[must_use]
    pub fn ledger_snapshot(&self) -> super::IncorporationLedger {
        self.ledger.lock_recover().clone()
    }

    /// Adopt a ledger a replayed boundary carried: the incorporations a
    /// journal-served phase made before its worker died (ADR 0099 §6).
    pub fn absorb_ledger(&self, ledger: super::IncorporationLedger) {
        self.ledger.lock_recover().absorb(ledger);
    }

    /// Whether the opener still holds a group cursor its end must close.
    #[must_use]
    pub fn holds_groups(&self) -> bool {
        !self.groups.lock_recover().outstanding.is_empty()
    }
}

/// What an opener's end did with the groups it held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenerGroupsClosed {
    /// Every group this end closed or finalized, in the order it handled them.
    pub groups: Vec<String>,
    /// Groups the closing seam reported `Pending`: an obligation this process
    /// cannot finish (a live lease elsewhere, no executor here) keeps them
    /// `closing`, recorded and discoverable (§7, W12).
    pub pending: Vec<String>,
}

impl<'run> RuntimeExecutionContext<'run> {
    /// The opener state this context incorporates against and hands groups to.
    #[must_use]
    pub fn opener_state(&self) -> OpenerState {
        OpenerState {
            ledger: Arc::clone(&self.incorporation_ledger),
            groups: Arc::clone(&self.opener_groups),
        }
    }

    /// Share `state` with this phase context: the owner of the opener (a turn
    /// driver, a process segment) creates it once and passes it to every
    /// context it builds.
    #[must_use]
    pub fn with_opener_state(mut self, state: OpenerState) -> Self {
        self.incorporation_ledger = state.ledger;
        self.opener_groups = state.groups;
        self
    }

    /// Wire the host's closing seam the opener's end finalizes through.
    #[must_use]
    pub fn with_group_closing(
        mut self,
        closing: Option<Arc<dyn crate::StoreEffectGroupClosing>>,
    ) -> Self {
        self.group_closing = closing;
        self
    }

    /// Hand a group whose consumer stopped before exhaustion to the opener.
    ///
    /// The handle carries the consumer's cursor, so the prefix the consumer
    /// already incorporated is the prefix the end starts after.
    pub(crate) fn retain_outstanding_group(&self, handle: crate::EffectGroupHandle) {
        self.opener_groups.lock_recover().outstanding.push(handle);
    }

    /// The cursors of every group the opener holds, for a segment handover
    /// (ADR 0099 §8, §9): a successor segment reattaches them rather than
    /// declining the boundary while losers are unsettled, and closes them at
    /// the process terminal.
    #[must_use]
    pub fn outstanding_groups_snapshot(&self) -> Vec<crate::EffectGroupHandle> {
        self.opener_groups
            .lock_recover()
            .outstanding
            .iter()
            .filter_map(|handle| {
                crate::EffectGroupHandle::restored(
                    handle.group_key(),
                    handle.children(),
                    handle.consumed(),
                )
                .ok()
            })
            .collect()
    }

    /// Reattach the groups a predecessor segment handed over. Their
    /// reservations come with them: the successor is the same opener, so the
    /// work its predecessor accepted is still retained (§9: replay reuses the
    /// reservation).
    pub fn restore_outstanding_groups(&self, handles: Vec<crate::EffectGroupHandle>) {
        let mut registry = self.opener_groups.lock_recover();
        for handle in &handles {
            registry
                .reserved
                .insert(handle.group_key().to_string(), handle.children());
        }
        registry.outstanding = handles;
    }

    /// The opener's start after a worker death: recover the children of every
    /// live group under its scope that nothing here is running (ADR 0099 W5).
    /// Worker loss is not an opener end, so this never cancels anything; on a
    /// tier without a closing seam it has nothing to do — Restate's children
    /// are `call` children its engine keeps running.
    pub async fn recover_opener_groups(&self) -> Result<usize, RuntimeEffectControllerError> {
        let Some(closing) = self.group_closing.clone() else {
            return Ok(0);
        };
        let scope = self
            .dispatch
            .effect_controller
            .scoped()
            .execution_scope()
            .clone();
        if closing
            .read_unsettled_groups(&scope)
            .await?
            .iter()
            .all(|group| group.closing)
        {
            return Ok(0);
        }
        self.republish_execution_env(&scope).await?;
        closing.recover_live_groups(&scope).await
    }

    /// Republish the opener's execution environment before a child no process
    /// is running is driven: a recovered child resolves the environment its
    /// request recorded and never invents one (ADR 0099 §3). The publish is
    /// content-addressed and idempotent, so this is the same reference the
    /// group's formation published, and an opener whose environment store did
    /// not survive its worker still resolves it. Without it, a recovered or
    /// finalized child can settle as a missing-environment failure.
    async fn republish_execution_env(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.captured_process_execution_env_ref(&crate::ArtifactOwner::execution(scope.clone()))
            .await
            .map(drop)
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "the opener could not republish its execution environment before \
                         driving a child no process is running: {error}"
                    ),
                )
            })
    }

    /// The prefix of every group key this opener forms (see
    /// `tool_child_group_key`): the groups its end is responsible for, as
    /// against a foreign group under the same scope — an operator's, or one a
    /// queue drain's end owns.
    fn own_group_prefix(&self) -> String {
        format!("{}:group:", self.execution_scope_id())
    }

    /// Reserve `children` units of this opener's retained work for
    /// `group_key` (ADR 0099 §9), before the group is opened or any child
    /// dispatched.
    ///
    /// A key this opener already reserved reuses its reservation, and a group
    /// the journal has already accepted is admitted over the bound: accepted
    /// work is never retroactively refused because the budget changed. A fresh
    /// group that does not fit is refused whole, with
    /// [`EffectGroupOpenerBoundExceeded`](crate::RuntimeErrorCode::EffectGroupOpenerBoundExceeded),
    /// and nothing of it is journaled.
    pub(crate) async fn reserve_group_work(
        &self,
        group_key: &str,
        children: usize,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (retained, limit) = loop {
            let (retained, limit) = {
                let mut registry = self.opener_groups.lock_recover();
                if registry.reserved.contains_key(group_key) {
                    return Ok(());
                }
                let retained = registry.reserved.values().sum::<usize>();
                let limit = registry.bound.max_retained_children();
                if retained.saturating_add(children) <= limit {
                    registry.reserved.insert(group_key.to_string(), children);
                    return Ok(());
                }
                (retained, limit)
            };
            // Over the bound: retire the oldest group the opener still holds
            // and try again. Only when nothing is left to retire is the bound
            // a refusal.
            if !self.retire_oldest_held_group().await? {
                break (retained, limit);
            }
        };
        let accepted = match &self.group_closing {
            Some(closing) => closing.read_group_lifecycle(group_key).await?.is_some(),
            None => false,
        };
        if accepted {
            self.opener_groups
                .lock_recover()
                .reserved
                .insert(group_key.to_string(), children);
            return Ok(());
        }
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectGroupOpenerBoundExceeded,
            format!(
                "effect group {group_key} would add {children} child(ren) to the {retained} this \
                 opener already retains, past its bound of {limit}; the group is refused whole \
                 and nothing of it is dispatched (ADR 0099 §9)"
            ),
        ))
    }

    /// Retire the oldest group this opener holds, as a whole, while the opener
    /// stays live (ADR 0099 §9): wait out its remaining ranks — its losers run
    /// to their own terminals, nothing is cancelled — incorporate them, close
    /// it and release its units. Answers `false` when the opener holds no
    /// group.
    ///
    /// A held group is one whose consumer stopped early, so the aggregate that
    /// formed it has already answered: nothing replays or continues from it,
    /// and its recorded settlements are the identity fence a reopen is served
    /// from. Retirement is reached only when a new group would pass the bound,
    /// which is a fact of the opener's own deterministic history, so a replay
    /// retires the same groups at the same point and issues the same commands.
    async fn retire_oldest_held_group(&self) -> Result<bool, RuntimeEffectControllerError> {
        let Some(mut handle) = ({
            let mut registry = self.opener_groups.lock_recover();
            (!registry.outstanding.is_empty()).then(|| registry.outstanding.remove(0))
        }) else {
            return Ok(false);
        };
        let controller = self.dispatch.effect_controller.controller();
        let cancel = self.cancellation_token.clone().unwrap_or_default();
        while !handle.is_exhausted() {
            if let Err(error) = controller
                .await_next_settlement(&mut handle, cancel.child_token())
                .await
            {
                self.opener_groups
                    .lock_recover()
                    .outstanding
                    .insert(0, handle);
                return Err(error);
            }
        }
        if let Err(error) = self.incorporate_group_prefix(&handle).await {
            self.opener_groups
                .lock_recover()
                .outstanding
                .insert(0, handle);
            return Err(error);
        }
        self.release_group_work(handle.group_key());
        if let Err(error) = controller
            .close_effect_group(handle, LoserPolicy::RunToCompletion)
            .await
        {
            tracing::warn!(
                error = %error,
                "closing a retired effect group failed; the close is retryable and the \
                 opener's end resumes whatever closing is recorded"
            );
        }
        Ok(true)
    }

    /// Release `group_key`'s reservation: its opener no longer depends on it.
    pub(crate) fn release_group_work(&self, group_key: &str) {
        self.opener_groups.lock_recover().reserved.remove(group_key);
    }

    /// The opener's end for its effect groups (ADR 0099 §7): close every group
    /// it holds under `Cancel`, finalize, and incorporate each group's settled
    /// ranks. See the module documentation for the order and why step 3 runs
    /// after finalization.
    ///
    /// The context's closing seam decides the tier's path: the SQL and native
    /// tiers answer one, Restate answers `None` because its engine-side group
    /// index is the twin. Called with the opener's own execution context,
    /// because step 2 of finalization incorporates into that context's ledger
    /// and token sink.
    pub async fn close_opener_groups(
        &self,
    ) -> Result<OpenerGroupsClosed, RuntimeEffectControllerError> {
        let closing = self.group_closing.clone();
        let held = std::mem::take(&mut self.opener_groups.lock_recover().outstanding);
        let scoped = self.dispatch.effect_controller.scoped();
        let scope = scoped.execution_scope().clone();
        let controller = scoped.controller();
        let mut closed = OpenerGroupsClosed::default();
        let mut remaining = Vec::with_capacity(held.len());
        for handle in held {
            let cursor = (
                handle.group_key().to_string(),
                handle.children(),
                handle.consumed(),
            );
            controller
                .close_effect_group(handle, LoserPolicy::Cancel)
                .await?;
            closed.groups.push(cursor.0.clone());
            remaining.push(cursor);
        }
        match closing {
            Some(closing) => {
                // Every unsettled group this opener formed, beyond the ones it
                // holds: a `live` one a crashed incarnation's cell opened, a
                // `closing` one an earlier end recorded and never finished.
                // A foreign group under the same scope — an operator's, or one
                // a queue drain's end owns — is not this opener's to finish.
                let own_prefix = self.own_group_prefix();
                for group in closing.read_unsettled_groups(&scope).await? {
                    if !group.group_key.starts_with(&own_prefix)
                        || closed.groups.contains(&group.group_key)
                    {
                        continue;
                    }
                    if !group.closing {
                        let handle = crate::EffectGroupHandle::restored(
                            group.group_key.clone(),
                            group.children,
                            0,
                        )?;
                        controller
                            .close_effect_group(handle, LoserPolicy::Cancel)
                            .await?;
                    }
                    closed.groups.push(group.group_key);
                }
                if !closed.groups.is_empty() {
                    // Finalization's first step may drive a child no process
                    // is running.
                    self.republish_execution_env(&scope).await?;
                }
                let steps = ContextFinalizationSteps::new(self);
                for group_key in &closed.groups {
                    if let crate::GroupFinalizationReport::Pending { .. } =
                        closing.finalize_group(group_key, &steps).await?
                    {
                        closed.pending.push(group_key.clone());
                    }
                }
            }
            None => {
                // Restate: the close seated a cancelled rank for every child
                // whose final record had not committed, and a committed child
                // ranks once its drain finishes. Waiting out the remaining
                // ranks on the cursor the consumer left is step 1 seen from
                // the opener: every protected obligation is finished before
                // the opener's outcome and accounting commit.
                for (group_key, children, consumed) in remaining {
                    let mut handle =
                        crate::EffectGroupHandle::restored(group_key, children, consumed)?;
                    while !handle.is_exhausted() {
                        controller
                            .await_next_settlement(&mut handle, CancellationToken::new())
                            .await?;
                    }
                }
            }
        }
        for group_key in &closed.groups {
            self.incorporate_group_outcome(group_key).await?;
        }
        // The opener has ended: nothing it formed is required by it any more.
        self.opener_groups.lock_recover().reserved.clear();
        Ok(closed)
    }
}

#[cfg(test)]
#[path = "opener_groups_tests.rs"]
mod tests;
