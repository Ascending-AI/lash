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

use lash_core_execution::ToolCallOutput;
use lash_core_execution::ToolCancellation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_core_store::tool_run::{MaterialOwner, MaterialRole};
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::KeyName;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::ProcessOutcome;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_protocol_types::{ExecutionLimit, ExecutionPolicy, ToolId};
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
/// carries: start, step settled, key pinned, timed out, step settled.
pub const TRANSITIONS: u64 = 5;

/// The start payload of a process that waits `wait` between its steps.
#[must_use]
pub fn payload(wait: Duration) -> Value {
    json!({ "wait_ms": u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) })
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

/// The admitted identity of the engine's own witness body.
fn step_tool(step: &StepRequest) -> ToolId {
    match step {
        StepRequest::Tool { .. } => unreachable!("the worker engine issues only engine steps"),
        StepRequest::Engine { kind, .. } => kind.admitted_tool(KIND),
    }
}

fn step(name: &str) -> StepRequest {
    StepRequest::Engine {
        step: StepName(name.to_owned()),
        kind: lash_execution_contracts::process::EngineStepKind::new(name),
        input: json!({ "step": name }),
    }
}

/// The runbook's process engine.
#[derive(Debug, Default)]
pub struct WorkerEngine;

#[async_trait::async_trait]
impl ProcessEngine for WorkerEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::PerformedEffects, lash_core::ProcessEnvRefusal>
    {
        Ok(lash_execution_contracts::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

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

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
                "step-before",
                EngineAction::Steps {
                    steps: vec![step(BEFORE)],
                    wake: None,
                },
            ),
            EngineEvent::StepSettled { .. } if phase == "step-before" => (
                "pin",
                EngineAction::PinKey {
                    name: KeyName(WAIT.to_owned()),
                    bound: lash_core::ParkBound::Within(Duration::from_millis(
                        script["wait_ms"].as_u64().unwrap_or(1_000),
                    )),
                },
            ),
            EngineEvent::KeyPinned { name, .. } => {
                ("await", EngineAction::AwaitExternal { name, site: None })
            }
            EngineEvent::ExternalTimedOut { .. } | EngineEvent::ExternalResolved { .. } => (
                "step-after",
                EngineAction::Steps {
                    steps: vec![step(AFTER)],
                    wake: None,
                },
            ),
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
                pages: None,
                requirements: Default::default(),
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<
        Vec<lash_core_execution::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the runbook engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
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

#[async_trait::async_trait]
impl ProcessSteps for WorkerSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        if !matches!(step, StepRequest::Engine { kind, .. } if [BEFORE, AFTER].contains(&kind.0.as_str()))
        {
            return Err(StepRefusal::Refused {
                step: step.step().0.clone(),
                reason: "the worker's witness bodies are its before and after engine steps".into(),
            });
        }
        Ok(StepAdmission::new(
            step_tool(step),
            ExecutionPolicy::Once,
            ExecutionLimit::starting_at(now_ms, Duration::from_secs(60), Duration::from_secs(60)),
            None,
        ))
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_store::tool_run::Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_store::tool_run::SettledOutput {
        lash_core_store::tool_run::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        lash_core_execution::runtime::actor::round::member_body({
            let call = execution.call();
            let witness = self.witness.clone();
            let process = process.id.clone();
            let tool = step.step().0.clone();
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
                    SettledOutput::Completed(Material::journal_local(
                        MaterialOwner::Process {
                            process_id: process,
                        },
                        MaterialRole::AttemptOutput,
                        output,
                    ))
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// P3: the witness workload's own steps never bypass an empty effect declaration.
    #[test]
    fn worker_witness_steps_are_engine_steps() {
        for name in [BEFORE, AFTER] {
            assert!(
                matches!(step(name), StepRequest::Engine { .. }),
                "{name} is the engine's own witness body"
            );
        }
    }
}
