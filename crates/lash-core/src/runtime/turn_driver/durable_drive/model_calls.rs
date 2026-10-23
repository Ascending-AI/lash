//! The existing transaction's model-call writes and head projection.
use super::*;

impl RuntimeDrive {
    pub(super) async fn record_unsent_model_call(
        &mut self,
        cx: &ActorContext,
        id: lash_sansio::EffectId,
        error: crate::LlmCallError,
    ) -> Result<(), TurnError> {
        use lash_core_store::model_calls::{ModelCallEnd, ModelCallEndReason};
        let row = cx
            .session_projection()
            .await?
            .and_then(|projection| projection.turn);
        let call = row
            .as_ref()
            .and_then(|row| row.phase.model())
            .map_or(self.driver.model_call_ordinal, |pin| pin.call);
        let reserved = cx.durable_reads()?.model_call_pins(&self.driver.session_id, &lash_durable::domain::ModelCallId::turn_owner(&self.driver.turn_id)).await?
            .into_iter().find(|pin| matches!(pin.key.call, lash_durable::domain::ModelCallId::Turn { ordinal, .. } if ordinal == call));
        let record = reserved.map_or_else(
            || crate::LlmCallRecord {
                call_id: crate::LlmCallId(format!(
                    "{}:turn:{}:llm:{}",
                    self.driver.session_id,
                    self.driver.turn_id,
                    self.machine.protocol_iteration()
                )),
                label: None,
                attempts: Vec::new(),
                replay_drops: Vec::new(),
            },
            |pin| pin.unobserved().record,
        );
        let reason = if error.code
            == Some(crate::FailureCode::lash(
                crate::TurnFailureCode::ModelTotalExceeded,
            )) {
            ModelCallEndReason::Attempt(crate::provider::AttemptEndReason::ModelDeadline)
        } else {
            ModelCallEndReason::Unsent
        };
        self.driver
            .queue_model_call(call, ModelCallEnd::Unanswered { reason }, record);
        // Preparation is not a transport attempt. The machine records its
        // typed stop cause in the turn terminal and emits the original typed
        // failure code for the host's TurnIssue; the call ledger retains only
        // attempts that were actually dispatched (FIG-6570, S1).
        self.machine.handle_response(Response::LlmComplete {
            id,
            result: Err(error),
            text_streamed: false,
        });
        Ok(())
    }

    pub(super) fn stage_sealed_model_calls(
        &mut self,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<(), TurnError> {
        use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
        use lash_durable::domain::{DomainWrite, ModelCallId, ModelCallWrite, PromptCallKey};
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        for fact in std::mem::take(&mut self.driver.pending_model_calls) {
            let record = fact
                .stage(&mut staging)
                .map_err(|error| TurnError::Exec(error.to_string()))?;
            tx.write(DomainWrite::ModelCall(ModelCallWrite {
                key: PromptCallKey {
                    session: self.driver.session_id.clone(),
                    call: ModelCallId::Turn {
                        run: self.driver.turn_id.clone(),
                        ordinal: fact.call,
                    },
                },
                call_id: fact.call_id.clone(),
                record,
            }));
            self.driver.staged_model_calls.push(fact);
        }
        tx.stage_all(staging);
        Ok(())
    }

    /// The transaction that staged the sealed calls failed: they await the
    /// next commit that settles them again, ahead of any sealed since.
    pub(super) fn unstage_sealed_model_calls(&mut self) {
        let mut staged = std::mem::take(&mut self.driver.staged_model_calls);
        staged.append(&mut self.driver.pending_model_calls);
        self.driver.pending_model_calls = staged;
    }
}

pub(super) async fn project_usage(
    cx: &ActorContext,
    driver: &mut RuntimeTurnDriver<'_>,
    machine: &TurnMachine,
    head: &SessionHead,
) -> Result<(), TurnError> {
    // Project the retained calls once over the head this turn opened.
    // Nested calls share the turn read, while retaining their own keys.
    let owner = lash_durable::domain::ModelCallId::turn_owner(&driver.turn_id);
    let records = cx
        .durable_reads()?
        .model_calls(&driver.session_id, &owner)
        .await?;
    let pins = cx
        .durable_reads()?
        .model_call_pins(&driver.session_id, &owner)
        .await?;
    let mut usage = head.state().token_usage.clone();
    let mut add = |record: &crate::LlmCallRecord| -> Result<(), TurnError> {
        let added = record
            .usage()
            .and_then(|added| usage.checked_add(&added))
            .map_err(|error| {
                runtime(crate::runtime::runtime_error_from_store_commit(
                    crate::StoreError::TokenUsageAccountingOverflow {
                        usage_source: "turn model calls".to_owned(),
                        model: driver
                            .policy
                            .llm_profile_config()
                            .model
                            .wire_model()
                            .to_owned(),
                        counter: error.counter(),
                    },
                ))
            })?;
        usage = added;
        Ok(())
    };
    for record in &records {
        add(&record.record)?;
    }
    for fact in &driver.pending_model_calls {
        add(&fact.record)?;
    }
    for pin in pins {
        if !records.iter().any(|record| record.call_id == pin.call_id)
            && !driver
                .pending_model_calls
                .iter()
                .any(|fact| fact.call_id == pin.call_id)
        {
            add(&pin.unobserved().record)?;
        }
    }
    let state = driver.turn_pipeline.state_mut();
    state.token_usage = usage;
    if let Some(prompt) = machine.last_call_usage().cloned().and_then(nonzero_usage) {
        state.last_prompt_usage = Some(prompt);
    }
    Ok(())
}

/// The host observer's immutable terminal facts, derived from exactly the
/// settlement the turn's commit carries. Dropped on any unacknowledged commit.
pub(super) struct CommittedFollow {
    sink: lash_durable::runner::WakeSink,
    actor: lash_durable::ActorKey,
    run: lash_vocabulary::identity::TurnId,
    inputs: Vec<lash_vocabulary::identity::InputId>,
    outcome: crate::store::RunCommittedOutcome,
}

impl CommittedFollow {
    pub(super) fn new(
        cx: &ActorContext,
        run: &lash_vocabulary::identity::TurnId,
        ingress: Option<&crate::store::IngressSettlement>,
        outcome: crate::store::RunCommittedOutcome,
    ) -> Self {
        Self {
            sink: cx.backend().stores().wake_sink(),
            actor: cx.actor().clone(),
            run: run.clone(),
            inputs: ingress
                .into_iter()
                .flat_map(|ingress| &ingress.completed_inputs)
                .flat_map(|input| input.input_ids.iter().cloned())
                .collect(),
            outcome,
        }
    }

    pub(super) fn publish(self) {
        self.sink
            .turn_committed(&self.actor, self.run, self.inputs, self.outcome);
    }
}
