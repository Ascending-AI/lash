//! Logical receipts are projections of accepted Run records.

use crate::store::{SessionCommitStore, ToolCompletionReceipt, ToolRequestReceipt};
use crate::tool_run::{
    BusinessReceipt, LogicalTerminal, ObservationPermit, ObservedFact, RunEvent, RunEventOrdinal,
    RunJournalEntry, RunRecord, RunTraceFacts,
};
use crate::{
    AdmittedScope, EffectOpener, RunRecordStep, RuntimeEffectController,
    RuntimeEffectControllerError, ScopedEffectController, ToolCallId,
};
use lash_trace::{
    DurableTraceScope, TraceAnchor, TraceCause, TraceContext, TraceEvent, TraceScopeId,
    TraceScopeOwner, TraceToolOwner, TraceToolTerminal, TraceTransitionKind,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

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
}

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
            observations.observe(&entry.record).await?;
        }
        Ok(entry)
    }

    pub async fn record_schedule(
        &self,
        engine: &dyn RuntimeEffectController,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<RunJournalEntry, RuntimeEffectControllerError> {
        let (body, observations) = self.recording_step(engine, step)?;
        let entry = engine.record_run_schedule(name, body).await?;
        if let Some(observations) = observations {
            observations.observe(&entry.record).await?;
        }
        Ok(entry)
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
        let owner = match bound.parent.as_ref().map(|scope| &scope.scope.owner) {
            Some(TraceScopeOwner::Run { session_id, run }) => TraceToolOwner::Run {
                session_id: session_id.clone(),
                run: run.clone(),
            },
            _ => tool_owner(&EffectOpener::for_scope(&bound.admitted).map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    error.to_string(),
                )
            })?),
        };
        let observations = RunObservations {
            runtime: runtime.clone(),
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
                        let scope = TraceScopeId::admission(TraceScopeOwner::Tool {
                            owner: owner.clone(),
                            call_id: member.call_id.to_string(),
                        });
                        let cause = match bound.parent.as_ref().map(|scope| &scope.anchor) {
                            Some(TraceAnchor::Context(context)) => {
                                TraceCause::Parent(context.clone())
                            }
                            _ => TraceCause::Root,
                        };
                        admissions.insert(
                            member.call_id.clone(),
                            DurableTraceScope {
                                scope,
                                cause,
                                anchor: TraceAnchor::Untraced,
                                started_at_ms: at_ms,
                            },
                        );
                    }
                }
            }
            entry.record.trace = Some(RunTraceFacts {
                at_ms,
                owner,
                admissions,
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

pub(crate) fn tool_owner(opener: &EffectOpener) -> TraceToolOwner {
    match opener {
        EffectOpener::Turn {
            session_id,
            turn_id,
        } => TraceToolOwner::Turn {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
        EffectOpener::Process { process_id } => TraceToolOwner::Process {
            process_id: process_id.clone(),
        },
        EffectOpener::SessionOperation {
            session_id,
            operation_id,
        } => TraceToolOwner::Operation {
            session_id: session_id.clone(),
            operation_id: operation_id.clone(),
        },
    }
}

struct RunObservations {
    runtime: super::TraceRuntime,
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
    async fn observe(&self, record: &RunRecord) -> Result<(), RuntimeEffectControllerError> {
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
                    self.emit(
                        &receipt.record,
                        &member.call_id,
                        None,
                        receipt.record.requested_at_ms,
                        TraceTransitionKind::Started,
                        receipt.permit().as_ref(),
                    );
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
                self.complete(store.as_ref(), trace, call_id, terminal, event)
                    .await?;
            }
            if let RunEvent::Presented { call_id, .. } = event {
                self.complete(
                    store.as_ref(),
                    trace,
                    call_id,
                    TraceToolTerminal::Final,
                    event,
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
    ) -> Result<(), RuntimeEffectControllerError> {
        let key = request_key(&trace.owner, call_id)?;
        let Some(request) = store.tool_request_receipt(&key).await? else {
            return Err(crate::StoreError::Backend(format!(
                "Run call {call_id} has no accepted receipt"
            ))
            .into());
        };
        let receipt = store
            .record_tool_completion(&ToolCompletionReceipt {
                owner: request.owner.clone(),
                request_key: request.request_key.clone(),
                payload_digest: request.payload_digest.clone(),
                result: serde_json::to_value(event).map_err(encoding_error)?,
                intent_outcomes: serde_json::Value::Null,
                completed_at_ms: trace.at_ms,
            })
            .await?;
        self.emit(
            &request,
            call_id,
            Some(terminal),
            receipt.record.completed_at_ms,
            TraceTransitionKind::Terminal,
            receipt.permit().as_ref(),
        );
        Ok(())
    }

    fn emit(
        &self,
        request: &ToolRequestReceipt,
        call_id: &ToolCallId,
        terminal: Option<TraceToolTerminal>,
        at_ms: u64,
        transition: TraceTransitionKind,
        permit: Option<&lash_trace::EmissionPermit>,
    ) {
        let Some(scope) = &request.scope else {
            return;
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
