//! The process workload: a host engine with a durable wait between two
//! `Once` steps.
//!
//! The engine emits `before`, runs the `Once` step `before`, pins a custom
//! key with a deadline and awaits it, then emits `after`, runs the `Once`
//! step `after` and ends with a count of the transitions it took. Its state
//! is the only thing that carries the count, so a terminal on another node
//! with the full count shows the engine state survived the move. `advance`
//! is pure; the step bodies write their witness entries.

use std::time::Duration;

use lash_core_execution::runtime::actor::round::{BodyOutput, ToolBody};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, HostWaitKind, KeyName,
    ProcessEngine, ProcessEventType, ProcessInfraError, ProcessOutcome, ProcessRecord, StepName,
    StepRequest, ToolCallId, ToolCallOutput, ToolCancellation,
};
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole,
};
use lash_sansio::{ExecutionLimit, ExecutionPolicy, ToolId};
use serde_json::{Value, json};

use crate::events::{Event, report};
use crate::witness::Witness;

/// The engine's kind.
pub const KIND: &str = "lash-postgres-workers";
/// The step before the wait.
pub const BEFORE: &str = "before";
/// The step after the wait.
pub const AFTER: &str = "after";
/// The pinned key's name.
const WAIT: &str = "wait";
/// The transitions an uncut run takes, and so the count its terminal
/// carries: start, emitted, step settled, key pinned, timed out, emitted,
/// step settled.
pub const TRANSITIONS: u64 = 7;

/// The start payload of a process that waits `wait` between its steps.
#[must_use]
pub fn payload(wait: Duration) -> Value {
    json!({ "wait_ms": u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) })
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core_execution::PluginError::Session(error.to_string()))
}

fn step(name: &str) -> StepRequest {
    StepRequest {
        step: StepName(name.to_owned()),
        tool: ToolId::new(name),
        input: json!({ "step": name }),
    }
}

fn event_type(name: &str) -> Result<ProcessEventType, ProcessInfraError> {
    Ok(ProcessEventType {
        name: name.to_owned(),
        payload_schema: lash_sansio::JsonSchema::admit(json!({ "type": "object" }))
            .map_err(infra)?,
        semantics: Default::default(),
    })
}

/// The event types the engine emits, which its registration declares.
///
/// # Errors
///
/// A payload schema is refused.
pub fn declared_event_types() -> Result<Vec<ProcessEventType>, ProcessInfraError> {
    Ok(vec![event_type(BEFORE)?, event_type(AFTER)?])
}

/// The runbook's process engine.
#[derive(Debug, Default)]
pub struct WorkerEngine;

#[async_trait::async_trait]
impl ProcessEngine for WorkerEngine {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core_execution::ProcessExecutionEnvSpec,
    ) -> Result<Option<Value>, lash_core_execution::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let mut script: Value = match &event {
            EngineEvent::Started { payload } => json!({ "wait_ms": payload["wait_ms"], "n": 0 }),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        script["n"] = json!(script["n"].as_u64().unwrap_or(0) + 1);
        let phase = script["phase"].as_str().unwrap_or_default().to_owned();
        let (next, action) = match event {
            EngineEvent::Started { .. } => (
                "emit-before",
                EngineAction::Emit {
                    event_type: event_type(BEFORE)?,
                    payload: json!({ "n": script["n"] }),
                },
            ),
            EngineEvent::Emitted if phase == "emit-before" => {
                ("step-before", EngineAction::Steps(vec![step(BEFORE)]))
            }
            EngineEvent::StepSettled { .. } if phase == "step-before" => (
                "pin",
                EngineAction::PinKey {
                    name: KeyName(WAIT.to_owned()),
                    kind: HostWaitKind::Custom,
                    deadline: Some(Duration::from_millis(
                        script["wait_ms"].as_u64().unwrap_or(1_000),
                    )),
                },
            ),
            EngineEvent::KeyPinned { name, .. } => ("await", EngineAction::AwaitExternal { name }),
            EngineEvent::ExternalTimedOut { .. } | EngineEvent::ExternalResolved { .. } => (
                "emit-after",
                EngineAction::Emit {
                    event_type: event_type(AFTER)?,
                    payload: json!({ "n": script["n"] }),
                },
            ),
            EngineEvent::Emitted if phase == "emit-after" => {
                ("step-after", EngineAction::Steps(vec![step(AFTER)]))
            }
            EngineEvent::StepSettled { .. } if phase == "step-after" => (
                "ended",
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
                    json!({ "transitions": script["n"] }),
                ))),
            ),
            EngineEvent::Cancelled { origin, .. } => (
                "ended",
                EngineAction::Terminal(ProcessOutcome::from_tool_output(
                    ToolCallOutput::cancelled(
                        ToolCancellation::runtime("the runbook engine answered its cancel")
                            .with_origin(origin),
                    ),
                )),
            ),
            _ => (phase.as_str(), EngineAction::Idle),
        };
        script["phase"] = json!(next);
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                format: self.state_format(),
                bytes,
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<Vec<lash_core_execution::ArtifactName>, lash_core_execution::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core_execution::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core_execution::PluginError> {
        Err(lash_core_execution::PluginError::Session(format!(
            "the runbook engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash_core_execution::ProcessDefinitionRef,
    ) -> Result<
        lash_core_execution::ProcessDefinitionResolution,
        lash_core_execution::ProcessDefinitionRefusal,
    > {
        Ok(lash_core_execution::ProcessDefinitionResolution::new(
            lash_core_execution::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// The engine's steps: both `Once`, each writing its witness entry.
#[derive(Debug)]
pub struct WorkerSteps {
    witness: Witness,
}

impl WorkerSteps {
    /// Steps that write to `witness`.
    #[must_use]
    pub fn new(witness: Witness) -> Self {
        Self { witness }
    }
}

impl ProcessSteps for WorkerSteps {
    fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        let _ = step;
        Ok(StepAdmission {
            policy: ExecutionPolicy::Once,
            limit: ExecutionLimit::starting_at(
                now_ms,
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
        })
    }

    fn body(&self, process: &ProcessRecord, step: &StepRequest, call: &ToolCallId) -> ToolBody {
        let witness = self.witness.clone();
        let process = process.id.clone();
        let tool = step.tool.as_str().to_owned();
        let call = call.to_string();
        Box::new(move |_token| {
            Box::pin(async move {
                witness.entered(&call, &tool).await;
                report(
                    witness.node(),
                    Event::Body {
                        call: call.clone(),
                        tool: tool.clone(),
                        phase: "entered".to_owned(),
                    },
                );
                witness.returned(&call, &tool).await;
                report(
                    witness.node(),
                    Event::Body {
                        call,
                        tool,
                        phase: "returned".to_owned(),
                    },
                );
                let output = json!({ "ok": true }).to_string();
                #[expect(clippy::expect_used, reason = "a JSON object always encodes")]
                let material = MaterialPayload::new(
                    MaterialOwner::Process {
                        process_id: process,
                    },
                    MaterialRole::AttemptOutput,
                    None,
                    output.clone(),
                )
                .reference(MaterialLocation::JournalLocal)
                .expect("a step's output encodes");
                BodyOutput {
                    outcome: AttemptOutcome::Completed(material),
                    material: Some(output),
                }
            })
        })
    }
}
