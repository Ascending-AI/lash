//! Commands that replay by re-execution (ADR 0103): run on every pass, never
//! journaled, with any row an earlier build left at their address discarded.

use super::{
    AwaitEventBackend, EffectReplayRowStore, EffectRun, ExecutionScope,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    StoreEffectReplayDriver,
};

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// Run a command that replays by re-execution (ADR 0103): no claim, no
    /// row, no lease. The local executor runs on every pass, live or replay,
    /// exactly as Restate's direct local call does, and the nested effects it
    /// issues come back through this driver on their own replay keys, so they
    /// are journaled on the first pass and served from the journal after.
    ///
    /// Strict replay mode does not apply: there is no row to find. A group
    /// member is refused, because the group's envelope-hash fence lives on a
    /// row this path never writes.
    pub(super) async fn reexecute_effect(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<EffectRun, RuntimeEffectControllerError> {
        super::super::group::refuse_unhonored_group_membership(
            envelope.group.as_deref(),
            "re-executed on replay",
        )?;
        // A row here predates the cutover. An `in_progress` one from a worker
        // that crashed mid-cell would otherwise wedge every quiescence gate
        // on the scope (drain end, quiescent retirement): nothing claims it
        // again, so nothing would ever finalize it.
        let journal_identity = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        self.row_store
            .discard_reexecuted_row(journal_identity.key(), envelope.invocation.replay_key())
            .await?;
        local_executor
            .execute(envelope)
            .await
            .map(EffectRun::Terminal)
    }
}
