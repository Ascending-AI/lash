//! The tool-round methods of the context: what the deleted controller
//! trait's Run-record family became.

use super::super::ActorContext;

/// The tool-round methods of the context: what the deleted controller trait's
/// Run-record family became. Their names keep the meaning that survives.
impl ActorContext {
    /// Record one record of a logical Run under `name`, and answer it.
    ///
    /// # Errors
    ///
    /// The record's refusal.
    pub async fn record_run_record(
        &self,
        _name: String,
        _step: crate::RunRecordStep<'_>,
    ) -> Result<lash_core_store::tool_run::RunJournalEntry, crate::RuntimeEffectControllerError>
    {
        todo!("L4 (FIG-5174): record a Run record as a run_records row")
    }

    /// Register a short record now, exposing its notification to the owner.
    pub fn start_run_record<'run>(
        &'run self,
        _name: String,
        _step: crate::RunRecordStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_run::RunJournalEntry> {
        todo!("L4 (FIG-5174): start a Run record as an admitted execution")
    }

    /// Register one independently completing attempt now.
    pub fn start_run_attempt<'run>(
        &'run self,
        _name: String,
        _step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        todo!("L4 (FIG-5174): start a Run attempt through admit, run_body and settle")
    }

    /// Register one declared-start launch and discharge.
    pub fn start_run_prepare<'run>(
        &'run self,
        _name: String,
        _step: crate::tool_dispatch::RunStartPrepareStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_dispatch::RunStartPrepared> {
        todo!("L4 (FIG-5174): start a declared-start preparation as an admitted execution")
    }

    /// Register a retry backoff now, its deadline recorded before it starts.
    pub fn start_run_retry(&self, _backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        todo!("L4 (FIG-5174): record a retry with its due time and register the due source")
    }

    /// Arm a call's source before its attempt receives the completion key.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn arm_run_source(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): arm a Run source as a pinned wait")
    }

    /// Attach the process terminal using the exact source admitted by the
    /// Run.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn attach_run_process_terminal(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): attach a process-terminal source as a process_terminal wait")
    }

    /// Race the Run's sources, the turn's cancel and the selectable
    /// notifications through L5's `race`.
    ///
    /// # Errors
    ///
    /// The race's refusal.
    pub async fn await_run_sources(
        &self,
        _subscriptions: Vec<crate::tool_run::SourceSubscription>,
        _selectable: Vec<crate::tool_dispatch::SelectKey>,
        _cancel: crate::TurnCancelWait,
    ) -> Result<crate::tool_dispatch::RunSourceWake, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): race Run sources through waits::race")
    }

    /// The index of the first of `keys` to complete.
    ///
    /// # Errors
    ///
    /// The race's refusal.
    pub async fn select_run_sources(
        &self,
        _keys: Vec<crate::tool_dispatch::SelectKey>,
    ) -> Result<usize, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): select the first completed Run source")
    }

    /// Cancel at the source authority and return its winning seal.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn cancel_run_source(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): cancel a Run source, first resolution wins")
    }

    /// Issue the store half of an intent realization as a store-local effect.
    ///
    /// # Errors
    ///
    /// The realization's refusal.
    pub async fn issue_run_realization<'run>(
        &'run self,
        _request: crate::tool_dispatch::RealizationRequest,
    ) -> Result<crate::tool_dispatch::IssuedRealization<'run>, crate::RuntimeEffectControllerError>
    {
        todo!("L4 (FIG-5174): realize an intent as a store-local effect of its call's outcome")
    }

    /// Attach to previously issued realization work.
    ///
    /// # Errors
    ///
    /// The realization's refusal.
    pub async fn attach_run_realization<'run>(
        &'run self,
        _invocation_id: String,
    ) -> Result<
        crate::tool_dispatch::RunSelectable<'run, crate::tool_dispatch::RealizationReceipt>,
        crate::RuntimeEffectControllerError,
    > {
        todo!("L4 (FIG-5174): read a realization from its call's committed outcome")
    }

    /// The tool-round effects: `ToolAttempt` and `RestoreRunMaterial`
    /// (admitted executions), `PresentToolResult` (a write in
    /// `round.present+model.start`), `Trigger`, `IngestTriggerOccurrence`
    /// and `AdmitTriggerDelivery` (store-local effects of their call's
    /// outcome). Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn tool_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): run a tool-round effect as an admitted execution or a round write")
    }
}
