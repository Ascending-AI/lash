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
//!    closing twin, the opener waits at each held group's drain barrier until
//!    no committed child still owes its drain. It does not await the ranks
//!    on the group's cursor: its own close ended the caller's interest, and
//!    the index refuses a caller's await of a closed group (FIG-3676);
//! 3. every closed group's settled ranks are incorporated through the
//!    journaled `IncorporateGroupSettlements` record.
//!
//! A group whose consumer was cancelled is held too: the cancel closes it,
//! and a rank that lands after that close — a committed loser's drain, a
//! cancelled attempt's captured usage — is still the opener's to incorporate.
//!
//! # The tool-call limit
//!
//! A group's tool calls are admitted against the session's recorded
//! `max_tool_calls` before anything of the group is journaled or dispatched
//! (ADR 0099 §9, FIG-4546). The limit is read from the policy the execution
//! recorded — a run's snapshot, a process's environment — so a replay, a
//! redrive and a reopen all judge the same call against the same number.
//!
//! * A **cell** counts every tool call it makes: the limit is its total.
//! * A **process** counts the calls it holds: accepted, running, or settled
//!   and still required. A group consumed to exhaustion releases its calls,
//!   and so does a group the process holds after an early decision once
//!   every loser of it has settled: its ranks are incorporated and it is
//!   closed. A call still running is held, and the limit never waits for it.
//!
//! The group that passes the limit is refused whole with a typed
//! [`ToolCallLimitExceeded`](crate::ToolCallLimitExceeded): the program's
//! failure, not the host's. Nothing is queued, paced or split.
//!
//! Step 3 runs after finalization rather than only inside it because the
//! host's own close spawns a finalizer with no opener steps (the closing
//! driver's `GroupOnlyFinalization`), and either finalizer may be the one that
//! records step 2. Incorporation is idempotent through the ledger and the
//! journaled prefix record, so running it here makes the opener's accounting
//! independent of which finalizer won.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::execution_context::RuntimeExecutionContext;
use crate::runtime::effect::LoserPolicy;
use crate::runtime::effect::executor::RuntimeEffectControllerError;

/// The groups an opener holds after their consumer stopped early, in the
/// order they were handed over, and the tool calls every group it formed
/// holds.
#[derive(Debug, Default)]
pub struct OpenerGroupRegistry {
    outstanding: Vec<crate::EffectGroupHandle>,
    /// Tool calls held per group key, from formation to release (§9).
    reserved: std::collections::BTreeMap<String, usize>,
    run: Option<crate::tool_run::RunTransfer>,
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
/// Cloning shares both; [`OpenerState::default`] starts a fresh opener.
#[derive(Clone, Debug, Default)]
pub struct OpenerState {
    pub(crate) ledger: Arc<std::sync::Mutex<super::IncorporationLedger>>,
    pub(crate) groups: Arc<std::sync::Mutex<OpenerGroupRegistry>>,
}

impl OpenerState {
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

    /// Capture the logical Run's state without closing or consuming a group.
    #[must_use]
    pub fn snapshot(&self) -> crate::store::RunOpenerState {
        let registry = self.groups.lock_recover();
        crate::store::RunOpenerState {
            incorporation: self.ledger_snapshot(),
            groups: registry
                .outstanding
                .iter()
                .map(|handle| crate::store::RunOpenerGroup {
                    group_key: handle.group_key().to_string(),
                    children: handle.children(),
                    consumed: handle.consumed(),
                    held_tool_calls: registry
                        .reserved
                        .get(handle.group_key())
                        .copied()
                        .unwrap_or(0),
                })
                .collect(),
        }
    }

    /// Reattach a continuation's groups before the successor executes. A
    /// malformed cursor refuses recovery rather than losing a held group.
    pub fn from_snapshot(
        snapshot: crate::store::RunOpenerState,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let mut registry = OpenerGroupRegistry::default();
        for group in snapshot.groups {
            let handle = crate::EffectGroupHandle::restored(
                group.group_key,
                group.children,
                group.consumed,
            )?;
            if group.held_tool_calls != 0 {
                registry
                    .reserved
                    .insert(handle.group_key().to_string(), group.held_tool_calls);
            }
            registry.outstanding.push(handle);
        }
        Ok(Self {
            ledger: Arc::new(std::sync::Mutex::new(snapshot.incorporation)),
            groups: Arc::new(std::sync::Mutex::new(registry)),
        })
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

    /// The tool calls each group the opener holds reserves, for a segment
    /// handover beside [`outstanding_groups_snapshot`](Self::outstanding_groups_snapshot).
    #[must_use]
    pub fn held_tool_calls_snapshot(&self) -> std::collections::BTreeMap<String, usize> {
        let registry = self.opener_groups.lock_recover();
        registry
            .outstanding
            .iter()
            .filter_map(|handle| {
                let calls = registry.reserved.get(handle.group_key())?;
                Some((handle.group_key().to_string(), *calls))
            })
            .collect()
    }

    /// Reattach the groups a predecessor segment handed over, with the tool
    /// calls each one holds. The successor is the same opener, so the calls
    /// its predecessor accepted are still held (§9: replay reuses the
    /// reservation, and never counts it twice).
    pub fn restore_outstanding_groups(
        &self,
        handles: Vec<crate::EffectGroupHandle>,
        held_tool_calls: &std::collections::BTreeMap<String, usize>,
    ) {
        let mut registry = self.opener_groups.lock_recover();
        for handle in &handles {
            if let Some(calls) = held_tool_calls.get(handle.group_key()) {
                registry
                    .reserved
                    .insert(handle.group_key().to_string(), *calls);
            }
        }
        registry.outstanding = handles;
    }

    /// The tool calls this context's cell has made so far, by group key
    /// (FIG-4546), for a segment boundary inside the cell: the successor
    /// segment runs the rest of the same cell, so it counts on from these.
    #[must_use]
    pub fn cell_tool_calls_snapshot(&self) -> std::collections::BTreeMap<String, usize> {
        self.cell_tool_calls.lock_recover().clone()
    }

    /// Resume counting a cell's tool calls from what its predecessor segment
    /// made.
    pub fn restore_cell_tool_calls(&self, made: std::collections::BTreeMap<String, usize>) {
        *self.cell_tool_calls.lock_recover() = made;
    }

    /// The prefix every group key this opener forms carries: `{scope}:group:`.
    pub(crate) fn own_group_key_prefix(&self) -> String {
        format!("{}:group:", self.execution_scope_id())
    }

    /// The key of the group a language command forms (FIG-3586): the opener's
    /// group prefix, then the command's own positional key. Under the prefix,
    /// so the opener's end finishes it like any group it formed; and scoped,
    /// so two openers' commands never share a group row.
    pub(crate) fn command_group_key(&self, command: &crate::CommandReplayKey) -> String {
        format!("{}{command}", self.own_group_key_prefix())
    }

    /// Admit `calls` tool calls of the group `group_key` against the
    /// session's recorded `max_tool_calls` (ADR 0099 §9, FIG-4546), before
    /// the group is opened or any child dispatched.
    ///
    /// A cell counts every call it makes; a process counts the calls it
    /// holds, and stops counting a held group once all of it has settled. A key
    /// already admitted reuses its admission: a group formed again on replay
    /// is the same calls, never more. A fresh group that does not fit is
    /// refused whole with
    /// [`MaxToolCallsExceeded`](crate::RuntimeErrorCode::MaxToolCallsExceeded)
    /// and its typed [`ToolCallLimitExceeded`](crate::ToolCallLimitExceeded)
    /// cause, and nothing of it is journaled.
    pub(crate) async fn reserve_tool_calls(
        &self,
        group_key: &str,
        calls: usize,
    ) -> Result<(), RuntimeEffectControllerError> {
        if calls == 0 {
            return Ok(());
        }
        let limit = self.max_tool_calls();
        let exceeded = if self.process_id().is_none() {
            let mut made = self.cell_tool_calls.lock_recover();
            if made.contains_key(group_key) {
                return Ok(());
            }
            let counted = made.values().sum::<usize>();
            if counted.saturating_add(calls) <= limit.get() {
                made.insert(group_key.to_string(), calls);
                return Ok(());
            }
            crate::ToolCallLimitExceeded {
                scope: crate::ToolCallLimitScope::Cell,
                limit,
                counted,
                requested: calls,
            }
        } else {
            loop {
                let held = {
                    let mut registry = self.opener_groups.lock_recover();
                    if registry.reserved.contains_key(group_key) {
                        return Ok(());
                    }
                    let held = registry.reserved.values().sum::<usize>();
                    if held.saturating_add(calls) <= limit.get() {
                        registry.reserved.insert(group_key.to_string(), calls);
                        return Ok(());
                    }
                    held
                };
                // Over the limit: a held group whose calls have all settled
                // is no longer held, so release the oldest such and try
                // again. A call still running is held: the limit refuses.
                if !self.release_oldest_settled_group().await? {
                    break crate::ToolCallLimitExceeded {
                        scope: crate::ToolCallLimitScope::Process,
                        limit,
                        counted: held,
                        requested: calls,
                    };
                }
            }
        };
        *self.tool_call_limit_refusal.lock_recover() = Some(exceeded);
        Err(RuntimeEffectControllerError::max_tool_calls_exceeded(
            exceeded,
        ))
    }

    /// The latest `max_tool_calls` refusal this execution met, typed. A
    /// language runtime whose run failed on the refusal reads it here to
    /// report the failure with its typed cause.
    #[must_use]
    pub fn tool_call_limit_refusal(&self) -> Option<crate::ToolCallLimitExceeded> {
        *self.tool_call_limit_refusal.lock_recover()
    }

    /// Release the oldest group this process holds, if every child of it has
    /// already settled (ADR 0099 §9): incorporate its ranks, close it and
    /// stop counting its tool calls. Answers `false`, releasing nothing, when
    /// the process holds no group or its oldest still has a child running.
    ///
    /// It never waits. A held group is one whose consumer stopped early, so
    /// the aggregate that formed it has already answered and nothing replays
    /// or continues from it; once its last rank is recorded none of its calls
    /// is running and none is required, so they are no longer held. A group
    /// with a child still running is held, and waiting for it would be the
    /// queue the limit does not have: the caller refuses instead. Settlement
    /// is durable and only ever moves forward, so a group this released is
    /// settled on every replay, and a replay releases the same groups at the
    /// same point.
    async fn release_oldest_settled_group(&self) -> Result<bool, RuntimeEffectControllerError> {
        let Some(mut handle) = ({
            let mut registry = self.opener_groups.lock_recover();
            (!registry.outstanding.is_empty()).then(|| registry.outstanding.remove(0))
        }) else {
            return Ok(false);
        };
        let hold = |handle| {
            self.opener_groups
                .lock_recover()
                .outstanding
                .insert(0, handle)
        };
        let controller = self.dispatch.effect_controller.controller();
        let last_rank = match u64::try_from(handle.children()) {
            Ok(last_rank) => last_rank,
            Err(_) => {
                let group_key = handle.group_key().to_string();
                hold(handle);
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("effect group {group_key} has more children than ranks"),
                ));
            }
        };
        match controller
            .read_group_settlement(handle.group_key(), last_rank)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                hold(handle);
                return Ok(false);
            }
            Err(error) => {
                hold(handle);
                return Err(error);
            }
        }
        // Every rank is recorded, so each of these answers without waiting.
        let cancel = self.cancellation_token.clone().unwrap_or_default();
        while !handle.is_exhausted() {
            if let Err(error) = controller
                .await_next_settlement(&mut handle, self.turn_cancel_wait(cancel.child_token()))
                .await
            {
                hold(handle);
                return Err(error);
            }
        }
        if let Err(error) = self.incorporate_group_prefix(&handle).await {
            hold(handle);
            return Err(error);
        }
        self.release_group_work(handle.group_key());
        if let Err(error) = controller
            .close_effect_group(handle, LoserPolicy::RunToCompletion)
            .await
        {
            tracing::warn!(
                error = %error,
                "closing a released effect group failed; the close is retryable and the \
                 opener's end resumes whatever closing is recorded"
            );
        }
        Ok(true)
    }

    /// Release the tool calls `group_key` holds: its opener no longer
    /// depends on them. A cell's total is not a holding, so nothing of it is
    /// released: a call a cell made stays made.
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
        let held = std::mem::take(&mut self.opener_groups.lock_recover().outstanding);
        let scoped = self.dispatch.effect_controller.clone();
        let controller = scoped.controller();
        let mut closed = OpenerGroupsClosed::default();
        let mut remaining = Vec::with_capacity(held.len());
        for handle in held {
            let group = (handle.group_key().to_string(), handle.children());
            controller
                .close_effect_group(handle, LoserPolicy::Cancel)
                .await?;
            closed.groups.push(group.0.clone());
            remaining.push(group);
        }
        for (group_key, children) in remaining {
            // Past the last rank: every committed child of the closed group
            // has seated, or retirement released the wait.
            let past_every_rank = u64::try_from(children)
                .ok()
                .and_then(|children| children.checked_add(1))
                .ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!("effect group {group_key} has more children than ranks"),
                    )
                })?;
            controller
                .await_group_child_drain_admission(&group_key, past_every_rank)
                .await?;
        }
        for group_key in &closed.groups {
            self.incorporate_group_outcome(group_key).await?;
            let mut rank = 1;
            while let Some(settled) = controller.read_group_settlement(group_key, rank).await? {
                if let Ok(crate::RuntimeEffectOutcome::ToolInvocationDeferred { completion }) =
                    settled.outcome
                {
                    self.abandon_deferred_tool_completion(*completion).await?;
                }
                rank += 1;
            }
        }
        // The opener has ended: nothing it formed is required by it any more.
        self.opener_groups.lock_recover().reserved.clear();
        Ok(closed)
    }
}

#[cfg(test)]
#[path = "opener_groups_tests.rs"]
mod tests;

impl RuntimeExecutionContext<'_> {
    /// Keep the Run's acknowledged cut beside the opener's phase state.
    /// The caller quiesces and retains the coordinator before handing it here.
    /// No issued handle or future is stored in this registry.
    ///
    /// # Errors
    /// An owner, admitted environment, segment or capture refusal.
    pub fn retain_run_continuation(
        &self,
        transfer: crate::tool_run::RunTransfer,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let owner = self
            .process_id()
            .map(|id| crate::EffectOpener::process(id.clone()))
            .ok_or(crate::tool_run::ContinuationRefusal::ForeignOwner)?;
        if transfer.owner != owner {
            return Err(crate::tool_run::ContinuationRefusal::ForeignOwner);
        }
        self.validate_process_run(&transfer, false)?;
        transfer.check_capture(&crate::tool_run::Cut::request(transfer.reason).observe(0))?;
        self.opener_groups.lock_recover().run = Some(transfer);
        Ok(())
    }

    /// Take the carried receipts to rebuild the successor's coordinator.
    pub fn take_run_continuation(&self) -> Option<crate::tool_run::RunTransfer> {
        self.opener_groups.lock_recover().run.take()
    }

    /// Capture a quiesced Run without closing the process's logical opener.
    ///
    /// # Errors
    /// A stale, unretained or unacknowledged continuation is refused.
    pub fn run_continuation_snapshot(
        &self,
    ) -> Result<Option<crate::tool_run::RunTransfer>, crate::tool_run::ContinuationRefusal> {
        let transfer = self.opener_groups.lock_recover().run.clone();
        if let Some(transfer) = &transfer {
            self.validate_process_run(transfer, false)?;
            transfer.check_capture(&crate::tool_run::Cut::request(transfer.reason).observe(0))?;
        }
        Ok(transfer)
    }

    /// Restore only the Run of this process and its admitted successor.
    /// The coordinator subsequently reads material through successor leases.
    ///
    /// # Errors
    /// An owner, admitted environment, segment or capture refusal.
    pub fn restore_run_continuation(
        &self,
        transfer: crate::tool_run::RunTransfer,
        successor: crate::tool_run::SegmentOrdinal,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let owner = self
            .process_id()
            .map(|id| crate::EffectOpener::process(id.clone()))
            .ok_or(crate::tool_run::ContinuationRefusal::ForeignOwner)?;
        let admitted = self
            .process_event_context()
            .and_then(|context| context.execution_write_authority.segment())
            .ok_or(crate::tool_run::ContinuationRefusal::MissingSegmentAuthority)?;
        if admitted != successor {
            return Err(crate::tool_run::ContinuationRefusal::SegmentFrontier);
        }
        self.validate_process_run(&transfer, true)?;
        transfer.check_capture(&crate::tool_run::Cut::request(transfer.reason).observe(0))?;
        // Keep the predecessor reference in the codec for the coordinator's
        // adoption. The registry validates authority without advancing twice.
        transfer
            .clone()
            .adopt(&owner, transfer.ledger()?.lifecycle(), successor)?;
        self.opener_groups.lock_recover().run = Some(transfer);
        Ok(())
    }
    fn validate_process_run(
        &self,
        transfer: &crate::tool_run::RunTransfer,
        adopting: bool,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let authority = self
            .process_event_context()
            .and_then(|context| context.execution_write_authority.segment())
            .ok_or(crate::tool_run::ContinuationRefusal::MissingSegmentAuthority)?;
        if !adopting && transfer.from != authority {
            return Err(crate::tool_run::ContinuationRefusal::SegmentFrontier);
        }
        if transfer.environment.as_ref() != self.inherited_process_execution_env_ref().as_ref() {
            return Err(crate::tool_run::ContinuationRefusal::ForeignEnvironment);
        }
        Ok(())
    }
}
