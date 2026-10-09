//! The process workload: a host engine with a durable wait between two
//! steps, contributed to the core by a plugin, as a host ships an engine.
//!
//! The engine emits `before`, runs its own step `before`, pins a custom key
//! with a deadline and awaits it, then emits `after`, runs its own step
//! `after` and ends with a count of the transitions it took. Its state is
//! the only thing that carries the count, so a terminal on another node
//! with the full count shows the engine state survived the move. `advance`
//! is pure; the step bodies write their witness entries. The steps are the
//! engine's own bodies ([`EngineSteps`]), which lash runs under its pinned
//! `Repeatable` policy.

use std::sync::Arc;
use std::time::Duration;

use lash::plugins::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, EngineStepKind, EngineStepRun,
    EngineSteps, HostWaitKind, KeyName, Material, MaterialOwner, MaterialRole, PluginDeclaration,
    PluginDefinition, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngine, ProcessEngineContributionContext, ProcessEngineRegistration, ProcessInfraError,
    SessionPlugin, SettledOutput, StepName, StepRequest,
};
use lash::process::{ProcessEventType, ProcessOutcome};
use lash::tools::{ToolCallOutput, ToolCancellation};
use serde_json::{Value, json};

use crate::events::{Event, report};
use crate::witness::Witness;

/// The engine's kind.
pub const KIND: &str = "lash-facade-failover";
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
    ProcessInfraError::new(PluginError::Session(error.to_string()))
}

fn step(name: &str) -> StepRequest {
    StepRequest::Engine {
        step: StepName(name.to_owned()),
        kind: EngineStepKind::new(name),
        input: json!({ "step": name }),
    }
}

fn event_type(name: &str) -> Result<ProcessEventType, ProcessInfraError> {
    serde_json::from_value(json!({
        "name": name,
        "payload_schema": { "type": "object" },
        "semantics": {},
    }))
    .map_err(infra)
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
pub struct FailoverEngine;

#[async_trait::async_trait]
impl ProcessEngine for FailoverEngine {
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

    fn program_identity(&self, _payload: &Value) -> Option<lash::plugins::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash::process::ProcessExecutionEnvSpec,
    ) -> Result<Option<Value>, PluginError> {
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
    ) -> Result<Vec<lash::persistence::ArtifactName>, PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> Result<(), lash::persistence::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash::persistence::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), PluginError> {
        Err(PluginError::Session(format!(
            "the runbook engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash::process::ProcessDefinitionRef,
    ) -> Result<lash::process::ProcessDefinitionResolution, lash::process::ProcessDefinitionRefusal>
    {
        Ok(lash::process::ProcessDefinitionResolution::new(
            lash::process::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// The engine's own steps, each writing its witness entry.
#[derive(Debug)]
pub struct FailoverSteps {
    witness: Witness,
}

#[async_trait::async_trait]
impl EngineSteps for FailoverSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![EngineStepKind::new(BEFORE), EngineStepKind::new(AFTER)]
    }

    async fn run(
        &self,
        run: EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> SettledOutput {
        let witness = &self.witness;
        let tool = run.kind.0.clone();
        // A step's identity is its process and its kind: each runs once per
        // process in an uncut run.
        let call = format!("{}/{}", run.process, run.kind.0);
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
        SettledOutput::Completed(Material::journal_local(
            MaterialOwner::Process {
                process_id: run.process,
            },
            MaterialRole::AttemptOutput,
            json!({ "ok": true }).to_string(),
        ))
    }
}

/// The plugin that contributes the engine and its steps to the core.
pub struct FailoverEnginePlugin {
    witness: Witness,
}

impl FailoverEnginePlugin {
    /// The plugin, its steps writing to `witness`.
    #[must_use]
    pub fn new(witness: Witness) -> Arc<dyn PluginFactory> {
        Arc::new(Self { witness })
    }
}

struct NoSessionPlugin;

impl SessionPlugin for NoSessionPlugin {
    fn id(&self) -> &'static str {
        KIND
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}

impl PluginFactory for FailoverEnginePlugin {
    fn id(&self) -> &'static str {
        KIND
    }

    fn process_engine_contributions(
        &self,
        _context: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<ProcessEngineRegistration>, PluginError> {
        Ok(vec![
            ProcessEngineRegistration::accepting(Arc::new(FailoverEngine)).with_engine_steps(
                Arc::new(FailoverSteps {
                    witness: self.witness.clone(),
                }),
            ),
        ])
    }

    fn build(
        &self,
        _context: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(NoSessionPlugin))
    }
}

impl PluginDefinition for FailoverEnginePlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(KIND)
    }
}
