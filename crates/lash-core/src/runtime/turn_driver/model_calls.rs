//! Staging and reading the turn's retained model-call facts.
use super::*;

impl RuntimeTurnDriver<'_> {
    pub(super) fn queue_model_call(
        &mut self,
        call: u32,
        end: lash_core_store::model_calls::ModelCallEnd,
        record: crate::LlmCallRecord,
    ) {
        self.pending_model_calls
            .push(lash_core_store::model_calls::ModelCallFact {
                call,
                call_id: record.call_id.clone(),
                end,
                record,
            });
    }

    pub(super) async fn recorded_model_call(
        &self,
        id: &crate::LlmCallId,
    ) -> Result<Option<crate::LlmCallRecord>, RuntimeError> {
        if let Some(fact) = self
            .pending_model_calls
            .iter()
            .find(|fact| &fact.call_id == id)
        {
            return Ok(Some(fact.record.clone()));
        }
        let read = async {
            self.scoped_effect_controller
                .durable_reads()?
                .model_calls(
                    &self.session_id,
                    &lash_durable::domain::ModelCallId::turn_owner(&self.turn_id),
                )
                .await
        }
        .await
        .map_err(|error| {
            RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, error.to_string())
        })?;
        Ok(read
            .into_iter()
            .find(|record| &record.call_id == id)
            .map(|record| record.record))
    }

    pub(super) async fn publish_retained_model_calls(&self, event_tx: &TurnObserver) {
        let read = async {
            self.scoped_effect_controller
                .durable_reads()?
                .model_calls(
                    &self.session_id,
                    &lash_durable::domain::ModelCallId::turn_owner(&self.turn_id),
                )
                .await
        }
        .await;
        match read {
            Ok(records) => {
                for record in records {
                    event_tx.publish(RuntimeStreamEvent::Turn(model_call_activity(
                        &self.scoped_effect_controller,
                        &self.turn_id,
                        &record.call_id,
                    )));
                }
            }
            Err(error) => {
                tracing::warn!(session_id = %self.session_id, %error, "committed model-call notifications could not be read")
            }
        }
    }

    pub(super) fn publish_committed_model_calls(&mut self, event_tx: &TurnObserver) {
        for id in std::mem::take(&mut self.staged_model_calls) {
            event_tx.publish(RuntimeStreamEvent::Turn(model_call_activity(
                &self.scoped_effect_controller,
                &self.turn_id,
                &id,
            )));
        }
    }
}
