//! The turn drive contract used by the durable phase runner.

use super::*;

/// One turn an owner runs: its machine and the in-memory work around it. The
/// phase runner polls the machine through it, commits at the turn's labels
/// and hands each effect to the method that answers it. A model call holds
/// its receipt until the phase runner commits it, then derives its answer.
#[async_trait::async_trait]
pub trait TurnDrive: Send {
    /// The turn's machine.
    fn machine(&mut self) -> &mut TurnMachine;

    /// Settle a logical model call with no response from a send on this
    /// activation. Record its unsent failure before answering the machine.
    fn model_unsent(&mut self, id: EffectId, error: crate::LlmCallError);

    /// Answer an effect that commits nothing of its own: an emit, progress,
    /// a log, a plugin checkpoint, an environment sync or a tool report.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the effect cannot be answered.
    async fn local(&mut self, cx: &ActorContext, effect: Effect) -> Result<(), TurnError>;

    /// The receipt of the model call the machine waits on that no
    /// derivation has answered: the provider's outcome as
    /// [`model_call`](Self::model_call) returned it, until `model.answer`
    /// commits it and the phase runner replaces it with the receipt as it
    /// committed, or the receipt a restore at `model.answer` found. Every
    /// derivation reads the committed receipt, never what only the live
    /// outcome held. `None` once a derivation or a cancel settled it.
    fn owed_answer(&mut self) -> &mut Option<ModelAnswer>;

    /// Derive the machine's model call `id` from
    /// [`owed_answer`](Self::owed_answer), a committed receipt: finish its
    /// recorded stream, run its recorded response hooks and answer the
    /// machine with the result. An abort leaves the receipt owed.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the derivation aborts the turn.
    async fn derive_answer(&mut self, cx: &ActorContext, id: EffectId) -> Result<(), TurnError>;

    /// Settle the machine's model call `id`, which an immediate cancel
    /// stopped before any derivation answered it, with
    /// [`owed_answer`](Self::owed_answer): an outcome the cancel stopped
    /// live, a receipt the commit that read the cancel recorded, or one a
    /// cancel restored from the rows. Its record is recorded, its stream is
    /// finished for its recorded reason, as a cleanup no failure holds back,
    /// and an unsuccessful outcome answers the machine; a completion is
    /// dropped unanswered, and no response hook runs. Nothing owed settles
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the outcome cannot be recorded.
    async fn settle_cancelled_answer(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
    ) -> Result<(), TurnError>;

    /// Run `attempt.ordinal` of the pinned model call `id`, bounded by
    /// `attempt.limit` (its deadline is the pinned one, never refreshed), and
    /// hold its outcome as [`owed_answer`](Self::owed_answer), the receipt the
    /// phase runner commits before any derivation answers the machine with
    /// it. Deltas it streams go to the session's live stream; only the
    /// receipt is durable.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the call aborts the turn rather than answering it.
    ///
    /// `admitted` is the call as `model.start` admitted it: every attempt,
    /// and a resend on any owner, sends its template's literals with its
    /// attachment slots filled afresh (WIRE-SLOTS), and reads the response
    /// under its recorded response context. `request` attributes and traces
    /// the call; it never reaches the provider.
    /// When `attempt.cancel` fires, settle the call with its already-sealed attempts
    /// before returning. Its cancellation does not authorize another send.
    async fn model_call(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        admitted: &lash_sansio::llm::types::AdmittedSend,
        attempt: ModelCallAttempt,
    ) -> Result<(), TurnError>;

    /// Prepare model call `call`, new to the turn, whose request the machine
    /// built as `request` and waits on as `id`, for its admission (ADR 0133 §6): compose its
    /// prompt sections and lower them into it, take the protocol's
    /// before-call decision, and lower it to its request template. It reads the turn's last commit and what the
    /// turn published since, all of which `model.start` commits with the
    /// call. A resend of an admitted call never prepares; a crash before
    /// admission prepares the call again, so what it runs must be
    /// repeat-safe until the call is admitted.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the call aborts the turn rather than settling.
    async fn prepare_call(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        call: u32,
        request: Arc<LlmRequest>,
    ) -> Result<PreparedCall, TurnError>;

    /// The steering input the turn's checkpoints delivered, with its
    /// application evidence (ADR 0101 §5.1). Every phase commits it with its
    /// checkpoint and binds it to the turn's run, so a resumed turn settles
    /// it with its commit and no checkpoint delivers it again.
    fn delivered_inputs(&self) -> Vec<crate::AdmittedTurnInputs> {
        Vec::new()
    }

    /// The decisions the turn's before-turn callbacks made when it was
    /// prepared. Every phase commits them with its checkpoint, and a resume
    /// serves them: no before-turn callback runs once a phase has committed.
    fn before_turn(&self) -> Vec<crate::plugin::RecordedTurnContribution> {
        Vec::new()
    }

    /// The trace scope the turn's admission retained. Every phase commits it
    /// with its checkpoint, and a resume's admission reads it back: the turn
    /// is admitted once, and every node's records of it share its trace
    /// (FIG-5363). `None` for a drive that admits no trace scope.
    /// The configuration the turn's run resolved and the tool surface it
    /// installed last (FIG-6021). Every phase commits it with its checkpoint,
    /// staging only what no earlier phase stored, and a resume and a cancel
    /// restore serve it: no configuration producer runs once a phase has
    /// committed. `None` for a drive that resolves no configuration.
    fn run_config(&self) -> Option<super::super::run_config::RecordedRunConfig> {
        None
    }

    /// Record that a phase committed the nodes `stored` names of
    /// [`run_config`](Self::run_config).
    fn run_config_stored(&mut self, _stored: &super::super::run_config::RunConfigStored) {}

    fn trace_scope(&self) -> Option<lash_trace::DurableTraceScope> {
        None
    }

    /// The plugin namespaces the turn's run changed that its rows do not
    /// record yet (FIG-5301): each phase commits them beside its checkpoint,
    /// the pending checkpoint-callback decisions among them, and a resume
    /// reinstalls the run's rows over the head before the machine restores.
    /// An unchanged namespace is never written. Empty for a drive that holds
    /// no plugins.
    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        Vec::new()
    }

    /// Record that a phase committed `written`, what
    /// [`run_changes`](Self::run_changes) named.
    fn run_changes_committed(&self, _written: &[lash_durable::domain::TurnNamespaceWrite]) {}

    /// Write into `tx`, the transaction that consumes cell `id`'s answer,
    /// the cell's handoff (FIG-5887 §6): with `working`, the session's
    /// complete execution state after the cell as the turn's working state,
    /// and the retirement of the root the cell named when it ended: the one
    /// that records its end, or its last park when the turn holds the end
    /// in memory (FIG-5999). A drive whose session holds no code executor
    /// writes nothing.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the state does not capture.
    async fn consume_cell(
        &mut self,
        _cx: &ActorContext,
        _tx: &mut ActorTx,
        _id: EffectId,
        _working: bool,
    ) -> Result<(), TurnError> {
        Ok(())
    }

    /// Whether the end of cell `id`, which answered, is held in memory for
    /// the commit that consumes its answer (FIG-5999): a successor that
    /// loses that commit runs the cell to its end again, so what the turn
    /// emits from the answer waits for the commit.
    fn answered_cell_end_held(&self, _id: EffectId) -> bool {
        false
    }

    /// Record that the phase whose transaction carried the last
    /// [`consume_cell`](Self::consume_cell) committed: its working capture
    /// is installed.
    async fn phase_committed(&mut self) {}

    /// The session's code executor: what publishes the heap pages of the
    /// execution root a phase commit names (FIG-6418). None for a drive
    /// whose session holds none.
    fn code_executor(&self) -> Option<std::sync::Arc<dyn crate::plugin::CodeExecutorPlugin>> {
        None
    }

    /// Where the session's live replay stands now, encoded for a model
    /// pin: a fresh call pins it before its first attempt streams, and a
    /// re-send reads back what the earlier attempts streamed after it
    /// (FIG-5399). Empty for a drive that streams to no live replay.
    fn live_stream_cursor(&self) -> String {
        String::new()
    }

    /// Restart the session's live stream before attempt `pin.attempt` of
    /// the call `id` is re-sent, so no one sees an abandoned attempt's
    /// partial text joined to the new attempt's: the stream retracts what
    /// the call's earlier attempts streamed (a `ModelAttemptReset`), and
    /// the re-sent attempt streams under ids of its own. Observers keep
    /// their cursors only while the live replay still holds everything
    /// after the pin's `stream_from`, the boundary pinned before the first
    /// attempt streamed; otherwise the stream restarts with a gap (the live
    /// replay store's `invalidate_session`).
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the live stream cannot restart; the call is not
    /// sent.
    async fn restart_live_stream(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        pin: &ModelPin,
    ) -> Result<(), TurnError>;

    /// The tools the turn's rounds run: the turn's catalog, which pins each
    /// call at admission and gives each member its body (L4, FIG-5174).
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the turn's catalog does not resolve.
    fn tools(
        &mut self,
    ) -> Result<Arc<dyn lash_core_execution::runtime::actor::round::RoundTools>, TurnError>;

    /// Hand `held`, the model response that asked for the cell this drive
    /// runs next, to the cell's first save (FIG-6000): `false` when the
    /// cell's executor commits nothing with it, and the turn commits it on
    /// its own before the cell starts.
    fn hold_response(&mut self, held: crate::HeldResponse) -> bool {
        let _ = held;
        false
    }

    /// Run the code cell of effect `id` from its latest snapshot, or from the
    /// start, to its end, and answer the machine. The cell files its
    /// snapshots under its own execution. The turn's `Tools` phase that
    /// re-delivers it committed before it started (`model.done`), or commits
    /// with the cell's first save, or, for a response the turn held
    /// ([`hold_response`](Self::hold_response)) that no save carried, with
    /// the commit that consumes the cell's answer.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the cell could not reach its end; ownership loss
    /// among them.
    async fn exec_cell(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        cell: CodeCell,
    ) -> Result<CellExit, TurnError>;

    /// The tool calls of the cell that last answered the machine, taken
    /// once: the turn's next commit records them as a settled round of its
    /// own (FIG-5330). Empty for a drive that runs no cell.
    fn answered_cell_calls(&mut self) -> CellToolCalls {
        CellToolCalls::default()
    }

    /// The cell of effect `id`, in `language`, is the one the turn's `Tools`
    /// phase holds: retain its first start, now, unless the drive retains
    /// one for it already (a resume, or a phase staged again). The phase's
    /// checkpoint carries it ([`cell_start`](Self::cell_start)), so every
    /// owner closes the cell's one span from it (§3.4).
    fn begin_cell(&mut self, _id: EffectId, _language: &str) {}

    /// The first start of the cell the turn's phase holds and that has not
    /// answered, which the phase's checkpoint retains. `None` for a drive
    /// that runs no cell.
    fn cell_start(&self) -> Option<CellStart> {
        None
    }

    /// The closing record of the cell of effect `id`, which answered the
    /// machine, taken once: published once the commit that consumes the
    /// answer is acknowledged (P1).
    fn answered_cell_close(&mut self, _id: EffectId) -> Option<CellClose> {
        None
    }

    /// The cell the turn's phase was about to hold is lost with this
    /// execution: no commit held it, so it has no span to close (P3).
    fn forget_cell(&mut self) {}

    /// The cell the turn's phase holds, which never answered, for the
    /// terminal that discards it: its close, published once that terminal's
    /// commit is acknowledged, at that owner's clock, and the calls it
    /// admitted that have no outcome, which that owner closes `discarded`
    /// (§3.8). `None` when no anchored cell is open, or nothing observes it.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the cell's call ledger does not read.
    async fn discarded_cell(
        &mut self,
        _cx: &ActorContext,
    ) -> Result<Option<DiscardedCell>, TurnError> {
        Ok(None)
    }

    /// The tool calls the cell of effect `id` completed before the turn
    /// stopped on it, read from the cell's latest snapshot and the outcomes
    /// it committed since: what a turn that ends without the cell's answer
    /// records of it (FIG-5330). A call the cell had not finished is not
    /// among them. Empty for a drive that runs no cell.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the cell's snapshot or records do not read.
    async fn stopped_cell_calls(
        &mut self,
        _cx: &ActorContext,
        _id: crate::EffectId,
    ) -> Result<CellToolCalls, TurnError> {
        Ok(CellToolCalls::default())
    }

    /// `calls` are the tool calls of the turn's last cell, which answered
    /// with no commit since: `turn.commit` records them, and the finish that
    /// follows reports them beside the recorded ones (FIG-5330).
    fn finishing_cell_calls(&mut self, _calls: &CellToolCalls) {}

    /// The turn accepted an `Immediate` cancel while its cell ran, and the
    /// phase runner dropped the cell: stop what the cell started that its
    /// future does not own, its tool bodies' cooperative stop among it.
    fn stop_cell(&mut self);

    /// The turn is about to park: wait for every model call whose tail is
    /// still read after its cell was accepted (FIG-6433), each bounded by
    /// its call's deadline, so parking never cuts a call the provider bills.
    /// A drive that runs no such call waits for nothing.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when a drained call's usage cannot be recorded.
    async fn settle_call_drains(&mut self) -> Result<(), TurnError> {
        Ok(())
    }

    /// The turn's commit to its session once its machine is done: the next
    /// revision of `head`, the session's head as the owner loaded it, which
    /// `turn.commit` publishes with the turn's terminal, and what the turn's
    /// after-turn callbacks decided over its outcome. The store refuses it
    /// once the head is elsewhere. A drive that publishes live activity
    /// answers once everything the turn published reached the live stream,
    /// so the terminal `turn.commit` writes is never durable ahead of the
    /// activity its readers expect beside it (FIG-5507).
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the commit cannot be built.
    async fn finish(
        &mut self,
        cx: &ActorContext,
        done: TurnDone,
        head: &SessionHead,
    ) -> Result<TurnCommit, TurnError>;

    /// The turn's `turn.commit` was acknowledged: publish what the turn held
    /// back for it, its after-turn callbacks' state and its outcome among it
    /// (ADR 0122, FIG-5800). A drive whose commit was refused, or whose
    /// acknowledgement was lost, is dropped without it and publishes none of
    /// what it held.
    async fn committed(&mut self);
}
