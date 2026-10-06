//! Logical receipts are projections of accepted Run records.

use crate::store::{SessionCommitStore, ToolCompletionReceipt, ToolRequestReceipt};
use crate::tool_run::{
    BusinessReceipt, LogicalTerminal, MaterialEntry, ObservationPermit, ObservedFact, RunEvent,
    RunEventOrdinal, RunJournalEntry, RunTraceFacts,
};
use crate::{
    AdmittedScope, EffectOpener, RunRecordStep, RuntimeEffectController,
    RuntimeEffectControllerError, ScopedEffectController, ToolCallId,
};
use lash_trace::{
    DurableTraceScope, TraceContext, TraceEvent, TraceScopeOwner, TraceToolOwner,
    TraceToolTerminal, TraceTransitionKind,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct BoundRun {
    frontier: super::JournalFrontier,
    parent: Option<DurableTraceScope>,
    admitted: AdmittedScope,
}

/// The scope-bound engine's Run-record hook. SQL first writers own emission
/// permits; no handler-local freshness or producer callback grants one.
#[derive(Default)]
pub struct RunRecordObserver {
    bound: Mutex<Option<BoundRun>>,
    accepted: AcceptedRequests,
}

/// Request receipts this invocation accepted, by request key, until their
/// terminal completes. A terminal whose admission another invocation
/// observed reads the stored receipt instead.
type AcceptedRequests = Arc<Mutex<BTreeMap<String, ToolRequestReceipt>>>;

impl RunRecordObserver {
    pub(crate) fn bind(&self, controller: &ScopedEffectController<'_>) {
        *self
            .bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(BoundRun {
            frontier: controller.frontier().clone(),
            parent: controller.trace_scope().cloned(),
            admitted: controller.admitted_scope().clone(),
        });
    }

    /// The trace scope a Run-routed call was admitted under, anchored as its
    /// request receipt retained it: what work the call launches descends
    /// from. `None` when nothing is bound or traced, or the call's admission
    /// was never observed.
    pub(crate) async fn tool_scope(&self, call_id: &ToolCallId) -> Option<DurableTraceScope> {
        let bound = self
            .bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let opener = EffectOpener::for_scope(&bound.admitted).ok()?;
        let key = request_key(&TraceToolOwner::from(&opener), call_id).ok()?;
        let accepted = self
            .accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        let request = match accepted {
            Some(request) => request,
            None => bound
                .frontier
                .runtime()?
                .tool_receipts()?
                .tool_request_receipt(&key)
                .await
                .ok()??,
        };
        request.scope
    }

    /// Journal the body with original observation facts, then project only
    /// the accepted entry returned by the engine, including served entries.
    pub async fn record(
        &self,
        engine: &dyn RuntimeEffectController,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<RunJournalEntry, RuntimeEffectControllerError> {
        let (body, observations) = self.recording_step(engine, step)?;
        let entry = engine.record_run_record(name, body).await?;
        if let Some(observations) = observations {
            observations.observe(&entry).await?;
        }
        Ok(entry)
    }

    pub fn start_record<'run>(
        &'run self,
        engine: &'run dyn RuntimeEffectController,
        name: String,
        step: RunRecordStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, RunJournalEntry> {
        let (body, observations) = match self.recording_step(engine, step) {
            Ok(parts) => parts,
            Err(error) => {
                return crate::tool_dispatch::RunStepHandle {
                    body: Box::pin(std::future::ready(())),
                    result: crate::tool_dispatch::RunSelectable {
                        key: Box::pin(std::future::ready(Err(error.clone()))),
                        value: Box::pin(std::future::ready(Err(error))),
                    },
                };
            }
        };
        let crate::tool_dispatch::RunStepHandle { body, result } =
            engine.start_run_record(name, body);
        crate::tool_dispatch::RunStepHandle {
            body,
            result: crate::tool_dispatch::RunSelectable {
                key: result.key,
                value: Box::pin(async move {
                    let entry = result.value.await?;
                    if let Some(observations) = observations {
                        observations.observe(&entry).await?;
                    }
                    Ok(entry)
                }),
            },
        }
    }

    fn recording_step<'run>(
        &self,
        engine: &dyn RuntimeEffectController,
        step: RunRecordStep<'run>,
    ) -> Result<(RunRecordStep<'run>, Option<RunObservations>), RuntimeEffectControllerError> {
        let bound = self
            .bound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(bound) = bound else {
            return Ok((step, None));
        };
        let runtime = bound.frontier.runtime().unwrap_or_default();
        let opener = EffectOpener::for_scope(&bound.admitted).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                error.to_string(),
            )
        })?;
        let owner = TraceToolOwner::from(&opener);
        let observations = RunObservations {
            runtime: runtime.clone(),
            accepted: Arc::clone(&self.accepted),
        };
        let issue = super::StepIssue::new(
            bound.frontier,
            engine.attempt_observation(),
            bound.parent.clone(),
        );
        let body: RunRecordStep<'run> = Box::pin(async move {
            let live = issue.begin_native();
            let mut entry = step.await?;
            let at_ms = runtime.clock().timestamp_ms();
            let mut admissions = BTreeMap::new();
            for event in &entry.record.events {
                if let RunEvent::Admitted { round } = event {
                    for member in &round.members {
                        admissions.insert(
                            member.call_id.clone(),
                            super::tool_trace_scope(
                                &opener,
                                bound.parent.as_ref(),
                                &member.call_id,
                                at_ms,
                            ),
                        );
                    }
                }
            }
            entry.record.trace = Some(RunTraceFacts {
                at_ms,
                owner,
                admissions,
                projections: entry
                    .record
                    .trace
                    .take()
                    .map(|trace| trace.projections)
                    .unwrap_or_default(),
            });
            runtime.body(bound.parent, &live).observe(|| {
                (
                    TraceContext::default(),
                    TraceEvent::JournaledEffectSettled {
                        effect_name: "run_record".into(),
                        effect_kind: "run_record".into(),
                        status: lash_trace::TraceJournaledEffectStatus::Completed,
                    },
                )
            });
            Ok(entry)
        });
        Ok((body, Some(observations)))
    }
}

struct RunObservations {
    runtime: super::TraceRuntime,
    accepted: AcceptedRequests,
}

struct ReceiptTransition<'a> {
    terminal: Option<TraceToolTerminal>,
    at_ms: u64,
    transition: TraceTransitionKind,
    permit: Option<&'a lash_trace::EmissionPermit>,
    projection: Option<&'a serde_json::Value>,
}

fn request_key(
    owner: &TraceToolOwner,
    call_id: &ToolCallId,
) -> Result<String, RuntimeEffectControllerError> {
    Ok(format!(
        "{}:{call_id}",
        serde_json::to_string(owner).map_err(encoding_error)?
    ))
}

impl RunObservations {
    async fn observe(&self, entry: &RunJournalEntry) -> Result<(), RuntimeEffectControllerError> {
        let record = &entry.record;
        let Some(trace) = &record.trace else {
            return Ok(());
        };
        let Some(store) = self.runtime.tool_receipts() else {
            return Ok(());
        };
        for (offset, event) in record.events.iter().enumerate() {
            let ordinal = RunEventOrdinal(record.first.0 + offset as u64);
            if let RunEvent::Admitted { round } = event {
                for member in &round.members {
                    let Some(scope) = trace.admissions.get(&member.call_id) else {
                        continue;
                    };
                    let TraceScopeOwner::Tool { owner, .. } = &scope.scope.owner else {
                        continue;
                    };
                    let mut scope = scope.clone();
                    let candidate = self.runtime.scopes().propose(&scope.scope, &scope.cause);
                    scope.anchor = candidate.anchor();
                    let offered = ToolRequestReceipt {
                        owner: owner.clone(),
                        request_key: request_key(owner, &member.call_id)?,
                        payload_digest: admission_digest(member)?,
                        payload: serde_json::to_value(member).map_err(encoding_error)?,
                        scope: Some(scope),
                        context: self.runtime.base_context().clone(),
                        requested_at_ms: trace.at_ms,
                    };
                    let receipt = store.record_tool_request(&offered).await;
                    candidate.settle(match &receipt {
                        Ok(receipt) if receipt.changed => {
                            lash_trace::TraceCandidateOutcome::Selected
                        }
                        Ok(_) => lash_trace::TraceCandidateOutcome::Reused,
                        Err(_) => lash_trace::TraceCandidateOutcome::Refused,
                    });
                    let receipt = receipt?;
                    self.accepted
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(receipt.record.request_key.clone(), receipt.record.clone());
                    self.emit(
                        &receipt.record,
                        &member.call_id,
                        ReceiptTransition {
                            terminal: None,
                            at_ms: receipt.record.requested_at_ms,
                            transition: TraceTransitionKind::Started,
                            permit: receipt.permit().as_ref(),
                            projection: trace.projections.get(&member.call_id),
                        },
                    )?;
                }
            }
            for fact in ObservationPermit::for_recorded(ordinal, event) {
                let ObservedFact::Logical(BusinessReceipt::Terminal { call_id, terminal }) =
                    fact.fact()
                else {
                    continue;
                };
                let terminal = match terminal {
                    LogicalTerminal::Final => continue,
                    LogicalTerminal::Denied => TraceToolTerminal::Denied,
                    LogicalTerminal::Cancelled => TraceToolTerminal::Cancelled,
                    LogicalTerminal::Aborted => TraceToolTerminal::Aborted,
                };
                self.complete(store.as_ref(), trace, call_id, terminal, event, Vec::new())
                    .await?;
            }
            if let RunEvent::Presented {
                call_id,
                presentation,
                ..
            } = event
            {
                // The presentation the record owns retains the call's
                // realized intents.
                let outcomes = presentation
                    .as_ref()
                    .and_then(|reference| {
                        entry.materials.iter().find_map(|material| match material {
                            MaterialEntry::Available {
                                reference: held,
                                payload,
                            } if held == reference => {
                                crate::tool_dispatch::presented_intent_outcomes(&payload.text)
                            }
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                self.complete(
                    store.as_ref(),
                    trace,
                    call_id,
                    TraceToolTerminal::Final,
                    event,
                    outcomes,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn complete(
        &self,
        store: &dyn SessionCommitStore,
        trace: &RunTraceFacts,
        call_id: &ToolCallId,
        terminal: TraceToolTerminal,
        event: &RunEvent,
        intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let key = request_key(&trace.owner, call_id)?;
        let accepted = self
            .accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        let request = match accepted {
            Some(request) => request,
            None => store.tool_request_receipt(&key).await?.ok_or_else(|| {
                crate::StoreError::Backend(format!("Run call {call_id} has no accepted receipt"))
            })?,
        };
        let receipt = store
            .record_tool_completion(&ToolCompletionReceipt {
                owner: request.owner.clone(),
                request_key: request.request_key.clone(),
                payload_digest: request.payload_digest.clone(),
                result: serde_json::to_value(event).map_err(encoding_error)?,
                intent_outcomes: serde_json::to_value(&intent_outcomes).map_err(encoding_error)?,
                completed_at_ms: trace.at_ms,
            })
            .await?;
        // Counted once, under the completion's first writer.
        for outcome in &intent_outcomes {
            let Some(kind) = outcome.kind() else {
                continue;
            };
            match outcome {
                crate::ToolIntentExecutionOutcome::Executed { .. } => {
                    crate::operational_metrics::record_tool_intent_executed(
                        self.runtime.metrics(),
                        receipt.permit().as_ref(),
                        kind.as_str(),
                    );
                }
                crate::ToolIntentExecutionOutcome::Refused { refusal, .. } => {
                    crate::operational_metrics::record_tool_intent_refused(
                        self.runtime.metrics(),
                        receipt.permit().as_ref(),
                        kind.as_str(),
                        refusal.code().as_ref(),
                    );
                }
                crate::ToolIntentExecutionOutcome::ProtocolRefused { .. } => {}
            }
        }
        self.emit(
            &request,
            call_id,
            ReceiptTransition {
                terminal: Some(terminal),
                at_ms: receipt.record.completed_at_ms,
                transition: TraceTransitionKind::Terminal,
                permit: receipt.permit().as_ref(),
                projection: trace.projections.get(call_id),
            },
        )?;
        Ok(())
    }

    fn emit(
        &self,
        request: &ToolRequestReceipt,
        call_id: &ToolCallId,
        observation: ReceiptTransition<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let ReceiptTransition {
            terminal,
            at_ms,
            transition,
            permit,
            projection,
        } = observation;
        let Some(scope) = &request.scope else {
            return Ok(());
        };
        self.runtime.unreplayed(Some(scope.clone())).transition(
            permit,
            at_ms,
            transition,
            0,
            || {
                let name = request
                    .payload
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (
                    request.context.clone(),
                    TraceEvent::ToolReceipt {
                        call_id: call_id.clone(),
                        name,
                        started_at_ms: request.requested_at_ms,
                        terminal,
                    },
                )
            },
        );
        if let Some(projection) = projection {
            let (context, mut event): (TraceContext, TraceEvent) =
                serde_json::from_value(projection.clone()).map_err(encoding_error)?;
            if let TraceEvent::ToolCallCompleted { duration_ms, .. } = &mut event {
                *duration_ms = at_ms.saturating_sub(request.requested_at_ms);
            }
            self.runtime.unreplayed(Some(scope.clone())).transition(
                permit,
                at_ms,
                transition,
                1,
                || (context, event),
            );
        }
        Ok(())
    }
}

fn encoding_error(error: serde_json::Error) -> RuntimeEffectControllerError {
    crate::StoreError::Backend(error.to_string()).into()
}

fn admission_digest(
    member: &crate::tool_run::AdmittedCall,
) -> Result<String, RuntimeEffectControllerError> {
    let mut identity = member.clone();
    identity.request.location = crate::tool_run::MaterialLocation::JournalLocal;
    let mut replies = identity.checks.replies().to_vec();
    for reply in &mut replies {
        if let crate::tool_run::BeforeCheckVerdict::Cached { result } = &mut reply.verdict {
            result.location = crate::tool_run::MaterialLocation::JournalLocal;
        }
    }
    identity.checks = crate::tool_run::CheckRecord::reduce(replies);
    Ok(lash_trace::sha256_hex(
        serde_json::to_vec(&identity).map_err(encoding_error)?,
    ))
}
