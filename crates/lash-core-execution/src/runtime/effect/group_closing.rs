//! The durable closing seam: the host-owned driver that runs a recorded
//! `closing` group through finalization (FIG-3410, ADR 0099 §7).
//!
//! Closing is a durable fact, not a process-local flag. When a caller releases
//! its group the host writes `{"type":"closing","disposition":..,"finalized":0}`
//! to the group row's `lifecycle` column *before* any admission stops or any
//! cancel decision is issued — §7's first sentence — and then runs a four-step
//! finalization, each step recorded by a guarded transition on that same row:
//!
//! 1. **Obligations**: every accepted child is ranked. Cancel-decided children
//!    are decided here if they were not decided at close (idempotent — an
//!    `AlreadyDecided` answer is as good as a fresh `Decided`), this host's own
//!    still-running children are awaited — bounded by the drain budget for a
//!    cancel-decided child, unbounded for a `RunToCompletion` one — and a drain
//!    pass discharges committed-but-undrained children and runs children no
//!    process is running. The step is complete only when the unsettled read is
//!    empty; a child owed elsewhere (a live lease on another process, no
//!    executor) leaves the group `closing` and the report `Pending`.
//! 2. **Outcome and accounting**: incorporate every settled rank of the group
//!    into the opener's execution context through
//!    [`RuntimeExecutionContext::incorporate_tool_settlement`](crate::RuntimeExecutionContext::incorporate_tool_settlement)
//!    with `SettlementSource::GroupRank { group_key, rank, child_replay_key }` —
//!    the `IncorporationLedger` the applicator charges against makes a resumed
//!    re-run of this step return every rank already incorporated, which is the
//!    W10 property. Supplied as [`OpenerFinalizationSteps`].
//! 3. **Parent end**: the opener's own end-record step, supplied the same way.
//! 4. **Retirement**: the lifecycle goes `settled` and the process-local entry
//!    is reaped through the same guarded check a settlement applies — never
//!    unconditionally, so a reopen that renewed interest in between keeps its
//!    entry.
//!
//! A crash between a step and its recording re-runs the step, which is why
//! every step is idempotent and the cursor counts *completed* steps. A
//! redriven turn calls [`StoreEffectGroupClosing::resume_closing_groups`] at
//! its exit; no new `work_kind` exists for this because the journal row is
//! already the queue.
//!
//! # Tier shape
//!
//! The trait is object-safe and exists once over the store-backed
//! effect-replay driver, so both SQL tiers answer it through one type; the
//! native tier answers it against its in-memory group table with the same
//! phase vocabulary. The Restate tier holds no group row — its engine-side
//! `EffectGroupIndex` `Closed`/`Retired` states are the twin — so it answers
//! [`EffectHost::effect_group_closing`](crate::EffectHost::effect_group_closing)
//! with `None`, and laws written against this seam return early there the way
//! the drain suite does on a tier with no drain.

use super::executor::RuntimeEffectControllerError;
use super::group_journal::EffectGroupLifecycle;
use crate::ExecutionScope;

/// What one finalization run did with the group it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupFinalizationReport {
    /// The group reached `settled`: every obligation is ranked, the opener's
    /// steps committed, and its live retention was retired.
    Settled {
        /// The group that settled.
        group_key: String,
    },
    /// The group is still `closing`: step 1 found children this process cannot
    /// finish — under a live lease owned elsewhere, or with no executor on
    /// this host — and left them queued. `closing` remains durably recorded;
    /// a later `resume` retries.
    Pending {
        /// The group still closing.
        group_key: String,
        /// How many children remain unsettled.
        unsettled: usize,
    },
}

/// The opener's own finalization steps — ADR 0099 §7 steps 2 and 3 — performed
/// by the opener's exit path and recorded on the group lifecycle by the
/// finalizer that runs them.
///
/// Both methods must be idempotent: a crash between the step and its recording
/// re-runs the step on the next resume, and a step that cannot be run twice
/// would make a recorded cursor the only thing standing between a replayed
/// finalization and a double commit.
#[async_trait::async_trait]
pub trait OpenerFinalizationSteps: Send + Sync {
    /// §7 step 2: commit the group's outcome and its accounting — incorporate
    /// every settled rank into the opener's execution context through
    /// [`RuntimeExecutionContext::incorporate_tool_settlement`](crate::RuntimeExecutionContext::incorporate_tool_settlement)
    /// under `SettlementSource::GroupRank { group_key, rank, child_replay_key }`.
    /// The applicator's `IncorporationLedger` already makes a re-run charge no
    /// rank twice; the method-level idempotence requirement covers whatever a
    /// real implementation does beyond the applicator.
    async fn commit_outcome_and_accounting(
        &self,
        group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError>;

    /// §7 step 3: record the parent's end.
    async fn record_parent_end(&self, group_key: &str) -> Result<(), RuntimeEffectControllerError>;
}

/// A group closed by its consumer alone owes no opener steps — the turn or
/// process exit that owns the opener (FIG-3397) supplies the real
/// implementation.
///
/// The real implementation cannot live on this driver's own finalizer:
/// `close_effect_group` reaches the controller through a command channel and
/// the finalization runs on a spawned `'static` host task, while the
/// [`RuntimeExecutionContext`](crate::RuntimeExecutionContext) the applicator
/// belongs to is `'run`-bound to the opener's turn (`ToolDispatchContext`
/// holds `RuntimeEffectControllerHandle<'run>` and
/// `DirectCompletionClient<'run>`). A `closing` group can also outlive that
/// context outright — a `Pending` group settles under a later turn or another
/// process — so the context must be supplied by the exit path that owns it,
/// which is FIG-3397's obligation.
pub struct GroupOnlyFinalization;

#[async_trait::async_trait]
impl OpenerFinalizationSteps for GroupOnlyFinalization {
    async fn commit_outcome_and_accounting(
        &self,
        _group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    async fn record_parent_end(
        &self,
        _group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }
}

/// The host-owned driver that finishes what `close` records.
///
/// A host obtains one beside its [`StoreEffectGroupDrain`](super::group_drain::StoreEffectGroupDrain),
/// over the same journal, the same owner identity, and the same registered
/// resolver. Where the drain is an operator's pass over a group whose caller
/// is gone, this seam is the §7 obligation: the recorded `closing` fact and
/// the cursor that finishes it.
#[async_trait::async_trait]
pub trait StoreEffectGroupClosing: Send + Sync {
    /// The lifecycle the group row currently holds, or `None` for a group the
    /// journal does not hold. This is the durable read the W9–W12 laws and a
    /// resuming turn answer "is closing recorded yet" through.
    async fn read_group_lifecycle(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupLifecycle>, RuntimeEffectControllerError>;

    /// Run `group_key` through the remaining finalization steps.
    ///
    /// Resumable by construction: the recorded cursor names the first
    /// incomplete step and each step is skipped if the cursor already records
    /// it. A `Live` group is refused — nothing has been closed, so there is
    /// nothing to finalize — and a `Settled` one reports `Settled` without
    /// touching anything.
    async fn finalize_group(
        &self,
        group_key: &str,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<GroupFinalizationReport, RuntimeEffectControllerError>;

    /// Finalize every `closing` group under `scope`, in journal order — the
    /// retained-recovery entry a redriven turn calls at its exit.
    async fn resume_closing_groups(
        &self,
        scope: &ExecutionScope,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<Vec<GroupFinalizationReport>, RuntimeEffectControllerError>;

    /// Whether `scope`'s journal is quiescent — no `in_progress` effect row,
    /// no group still short of a journaled child, no unresolved promise.
    ///
    /// The same proof the `WhenQuiescent` retirement gate takes, exposed as a
    /// read so an owner's end can be withheld while the scope still owes work
    /// (FIG-3419: a queue drain ends only over a quiescent scope). Unlike the
    /// gate this is a read, not a fence: the caller is deciding whether to
    /// write an end fact, not deleting the scope, so it needs the answer
    /// without the exclusion a retirement holds.
    async fn scope_is_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeEffectControllerError>;
}
