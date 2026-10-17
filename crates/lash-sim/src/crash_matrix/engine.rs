//! The deployment's process engine and its steps.
//!
//! [`SimProcessEngine`] is a pure state machine; its start payload names what
//! a process does (`act`):
//!
//! - `root` runs a `Once` and a `Repeatable` step, pins a custom key, awaits the key's resolution, then
//!   awaits the process `await` with a one-second deadline, and ends when
//!   that wait times out or the process ends;
//! - `hold` idles until it is cancelled.
//!
//! Every process answers its cancel with its terminal. A counter makes every
//! committed state distinct. The step bodies write their entries to the
//! world's ledger before anything else, then take [`BODY`] of virtual time.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::ProcessId;
use lash_core_execution::ToolCallOutput;
use lash_core_execution::ToolCancellation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_core_store::tool_run::{MaterialOwner, MaterialRole};
use lash_durable::domain::OwnerKey;
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

use super::world::{BodyEntry, World};

/// The engine's kind.
pub const KIND: &str = "lash-sim";
/// The `Once` step a root runs.
pub const ONCE: &str = "sim_once";
/// The `Repeatable` step a root runs.
pub const AGAIN: &str = "sim_again";
/// How long a root awaits the process it names.
pub const AWAIT_MS: u64 = 1_000;
/// How long a root's key stays open.
pub const KEY_MS: u64 = 30_000;
/// The pinned key's name.
const KEY: &str = "answer";
/// How long a step body runs once it entered: a host that watches the
/// ledger acts while the body still runs.
pub const BODY: Duration = Duration::from_millis(50);

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

fn step(name: &str, tool: &str) -> StepRequest {
    StepRequest::Tool {
        step: StepName(name.to_owned()),
        effect: lash_core_execution::testing::effect(tool),
        input: json!({ "step": name }),
        site: None,
        matched: None,
    }
}

/// A root's start payload: it awaits `await` once its steps and its key are
/// done.
#[must_use]
pub fn root(tag: &str, await_process: &ProcessId) -> Value {
    json!({ "tag": tag, "act": "root", "await": await_process.as_str() })
}

/// A holding process's start payload.
#[must_use]
pub fn hold(tag: &str) -> Value {
    json!({ "tag": tag, "act": "hold" })
}

/// A process's start payload that ends with its success as soon as it
/// starts.
#[must_use]
pub fn ends_at_once(tag: &str) -> Value {
    json!({ "tag": tag, "act": "ends_at_once" })
}

fn ended(value: Value) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
        value,
    )))
}

/// The deployment's process engine.
#[derive(Debug, Default)]
pub struct SimProcessEngine;

#[async_trait::async_trait]
impl ProcessEngine for SimProcessEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
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
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        script["n"] = json!(script["n"].as_u64().unwrap_or(0) + 1);
        let root = script["act"] == "root";
        let action = match event {
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the simulator's engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            EngineEvent::Started { .. } if script["act"] == "ends_at_once" => {
                ended(json!({ "ended": true, "tag": script["tag"] }))
            }
            EngineEvent::Started { .. } if root => EngineAction::Steps {
                steps: vec![step("once", ONCE), step("again", AGAIN)],
                wake: None,
            },
            EngineEvent::StepSettled { .. } if root => {
                let settled = script["settled"].as_u64().unwrap_or(0) + 1;
                script["settled"] = json!(settled);
                if settled == 2 {
                    EngineAction::PinKey {
                        name: KeyName(KEY.to_owned()),
                        bound: lash_core_execution::ParkBound::Within(Duration::from_millis(
                            KEY_MS,
                        )),
                    }
                } else {
                    EngineAction::Idle
                }
            }
            EngineEvent::KeyPinned { .. } if root => EngineAction::AwaitExternal {
                name: KeyName(KEY.to_owned()),
                site: None,
            },
            EngineEvent::ExternalResolved { .. } | EngineEvent::ExternalTimedOut { .. } if root => {
                script["key"] = json!(matches!(event, EngineEvent::ExternalResolved { .. }));
                EngineAction::AwaitProcess {
                    process: ProcessId::parse(script["await"].as_str().unwrap_or_default())
                        .map_err(infra)?,
                    bound: lash_core_execution::ParkBound::Within(Duration::from_millis(AWAIT_MS)),
                    site: None,
                }
            }
            EngineEvent::ProcessWaitTimedOut { .. } if root => {
                ended(json!({ "timed_out": true, "key": script["key"] }))
            }
            EngineEvent::ProcessEnded { .. } if root => {
                ended(json!({ "timed_out": false, "key": script["key"] }))
            }
            _ => EngineAction::Idle,
        };
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                format: self.state_format(),
                bytes,
                pages: None,
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
                "the simulator's engine stores no artifact `{artifact_ref}`"
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

fn policy(tool: &ToolId) -> ExecutionPolicy {
    if tool.as_str() == AGAIN {
        ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN.saturating_add(2), 0, 0)
    } else {
        ExecutionPolicy::Once
    }
}

/// The engine's steps: each body notes its entry, runs [`BODY`] and
/// completes.
pub struct SimSteps {
    world: Arc<World>,
}

impl SimSteps {
    /// Steps that write to `world`'s ledger.
    #[must_use]
    pub fn new(world: Arc<World>) -> Self {
        Self { world }
    }
}

#[async_trait::async_trait]
impl ProcessSteps for SimSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Ok(StepAdmission::new(
            lash_core_execution::testing::step_tool(step, KIND),
            policy(&lash_core_execution::testing::step_tool(step, KIND)),
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
            let world = Arc::clone(&self.world);
            let process = process.id.clone();
            let tool = lash_core_execution::testing::step_tool(step, KIND);
            let call = call.clone();
            Box::new(move |_token| {
                Box::pin(async move {
                    let owner = OwnerKey::Process(process.clone());
                    let admitted = world.admitted(&owner, &call).await;
                    world.ledger().enter(
                        &owner,
                        &call,
                        BodyEntry {
                            tool: tool.as_str().to_owned(),
                            policy: policy(&tool),
                            attempt: 1,
                            at_ms: world.now_ms(),
                            admitted,
                        },
                    );
                    world.sleep(BODY).await;
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
