//! Retain sealed nested calls beyond the lifetime of an opaque tool body.
use super::ActorContext;
use lash_durable::domain::model_calls::OwnedModelCall;
use lash_durable::domain::{DomainWrite, ModelCallOwner, RunFactKind, RunFactWrite, TurnWrite};
use lash_durable::{ActorTx, DurableError};
use lash_sansio::sync::MutexExt as _;

pub(super) struct ModelCallDebts {
    attempts: Vec<lash_trace::TraceAttemptId>,
    calls: Vec<OwnedModelCall>,
}

impl ActorContext {
    pub(super) async fn stage_model_calls(
        &self,
        tx: &mut ActorTx,
    ) -> Result<ModelCallDebts, DurableError> {
        let terminal = tx
            .domain()
            .iter()
            .any(|write| matches!(write, DomainWrite::Turn(TurnWrite::Terminal { .. })));
        let command = tx
            .domain()
            .iter()
            .any(|write| matches!(write, DomainWrite::SessionCommit(_)));
        let outcomes: Vec<_> = tx
            .domain()
            .iter()
            .filter_map(|write| match write {
                DomainWrite::RunFact(RunFactWrite::Append {
                    owner,
                    run,
                    call: Some(call),
                    kind: RunFactKind::XOutcome,
                    ..
                }) => Some(ModelCallOwner {
                    owner: owner.clone(),
                    run: *run,
                    call: call.clone(),
                }),
                _ => None,
            })
            .collect();
        let belongs = |enclosing: Option<&ModelCallOwner>| {
            terminal
                || match enclosing {
                    Some(owner) => outcomes.contains(owner),
                    None => command,
                }
        };
        let attempts: Vec<_> = self
            .inner
            .model_attempts
            .lock_recover()
            .iter()
            .filter(|attempt| belongs(attempt.model_call_owner()))
            .cloned()
            .collect();
        let mut calls = Vec::<OwnedModelCall>::new();
        for attempt in &attempts {
            // The body may already have been dropped. Its provider's cooperative
            // terminal still owns the partial usage and must precede the outcome.
            attempt.settle_model_tasks().await;
            for call in attempt.model_calls() {
                if let Some(existing) = calls.iter_mut().find(|stored| stored.key == call.key) {
                    *existing = call;
                } else {
                    calls.push(call);
                }
            }
        }
        if command || terminal {
            let tasks = std::mem::take(&mut *self.inner.model_tasks.lock_recover());
            for task in tasks {
                let _ = task.await;
            }
            calls.extend(self.inner.model_calls.lock_recover().clone());
        }
        for call in &calls {
            call.stage(tx)?;
        }
        if terminal || command || !outcomes.is_empty() {
            let scope = self.execution_scope();
            let session = match scope {
                crate::ExecutionScope::Process { process_id } => {
                    Some(crate::SessionId::prefixed("process:", process_id.as_str()))
                }
                scope => scope.session_id().cloned(),
            };
            if let Some(session) = session {
                let owner = format!("owned:{}", scope.id());
                let mut pins = self
                    .durable_reads()?
                    .model_call_pins(&session, &owner)
                    .await?;
                if terminal && let crate::ExecutionScope::Turn { turn_id, .. } = scope {
                    pins = self
                        .durable_reads()?
                        .model_call_pins(
                            &session,
                            &lash_durable::domain::ModelCallId::turn_owner(turn_id),
                        )
                        .await?;
                }
                for pin in pins {
                    if belongs(pin.enclosing.as_ref()) && !tx.domain().iter().any(|write| matches!(write, DomainWrite::ModelCall(call) if call.key == pin.key)) {
                        OwnedModelCall { key: pin.key.clone(), fact: pin.unobserved() }.stage(tx)?;
                    }
                }
            }
        }
        Ok(ModelCallDebts {
            attempts: attempts
                .iter()
                .map(crate::EffectAttempt::trace_id)
                .collect(),
            calls,
        })
    }

    pub(super) fn ack_model_calls(&self, debts: ModelCallDebts) {
        self.inner
            .model_attempts
            .lock_recover()
            .retain(|attempt| !debts.attempts.contains(&attempt.trace_id()));
        self.inner
            .model_calls
            .lock_recover()
            .retain(|call| !debts.calls.iter().any(|written| written.key == call.key));
    }
}
