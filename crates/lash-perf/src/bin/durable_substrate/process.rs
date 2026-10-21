//! The parked process: a host engine that pins a custom key, awaits it,
//! and, once a host resolves it, pins the next, `waits` times, then ends.
//! It runs no step, so what the bench times between a resolution and the
//! next transition is the substrate's wake, claim and advance alone.
//!
//! The engine reports each pinned key and each transition it sees to the
//! bench's [`ProcessBoard`]; `advance` is otherwise pure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::ToolCallOutput;
use lash_core_execution::ToolCancellation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::KeyName;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::ProcessOutcome;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;
use lash_execution_contracts::waits::PinnedKey;
use lash_protocol_types::sync::MutexExt as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The engine's kind.
pub const KIND: &str = "durable-substrate-bench";

/// What the engine saw, by the process's token.
#[derive(Debug)]
pub enum Seen {
    StepStarted(Instant, String),
    StepSettled(Instant, String),
    TimerDue(Instant),
    /// A key was pinned; the host may resolve it.
    Pinned(Instant, PinnedKey),
    /// A resolution reached the engine.
    Resolved(Instant),
}

/// Where engines report, by the token in their start payload.
#[derive(Default)]
pub struct ProcessBoard {
    feeds: Mutex<HashMap<String, mpsc::UnboundedSender<Seen>>>,
    barriers: Mutex<HashMap<String, Arc<tokio::sync::Barrier>>>,
}

impl ProcessBoard {
    /// Follow the process started with `token`.
    pub fn follow(&self, token: &str) -> mpsc::UnboundedReceiver<Seen> {
        let (send, receive) = mpsc::unbounded_channel();
        self.feeds.lock_recover().insert(token.to_owned(), send);
        receive
    }

    pub fn barrier(&self, token: &str, count: usize) -> Arc<tokio::sync::Barrier> {
        let barrier = Arc::new(tokio::sync::Barrier::new(count + 1));
        self.barriers
            .lock_recover()
            .insert(token.to_owned(), Arc::clone(&barrier));
        barrier
    }

    fn report(&self, token: &str, seen: Seen) {
        if let Some(feed) = self.feeds.lock_recover().get(token) {
            let _ = feed.send(seen);
        }
    }
}

/// Fixed populations: one discriminant drives the fixture and its exporter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Waits,
    Steps,
    Timers,
    Fanout,
    ReadyFanout,
}

/// The start payload of a process that awaits `waits` keys.
pub fn payload(token: &str, waits: usize) -> Value {
    json!({ "token": token, "waits": waits, "mode": Mode::Waits })
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

/// The bench's process engine.
pub struct BenchEngine {
    board: Arc<ProcessBoard>,
}

impl BenchEngine {
    /// An engine that reports to `board`.
    pub fn new(board: Arc<ProcessBoard>) -> Self {
        Self { board }
    }
}

impl std::fmt::Debug for BenchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BenchEngine")
    }
}

fn pin(index: u64) -> EngineAction {
    EngineAction::PinKey {
        name: KeyName(format!("w{index}")),
        bound: lash_vocabulary::tool_contract::ParkBound::Within(Duration::from_secs(3_600)),
    }
}

fn ended(text: &str) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
        json!({ "ended": text }),
    )))
}

#[async_trait::async_trait]
impl ProcessEngine for BenchEngine {
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
            EngineEvent::Started { payload } => {
                json!({ "token": payload["token"], "waits": payload["waits"], "mode": payload["mode"], "done": 0 })
            }
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let token = script["token"].as_str().unwrap_or_default().to_owned();
        let waits = script["waits"].as_u64().unwrap_or(1);
        let done = script["done"].as_u64().unwrap_or(0);
        let mode: Mode = serde_json::from_value(script["mode"].clone()).map_err(infra)?;
        let action = match event {
            EngineEvent::Started { .. } if matches!(mode, Mode::Fanout | Mode::ReadyFanout) => {
                let mut steps = Vec::new();
                for i in 0..waits {
                    if let EngineAction::Steps { steps: one, .. } = probe_step(i)? {
                        steps.extend(one);
                    }
                }
                EngineAction::Steps { steps, wake: None }
            }
            EngineEvent::StepSettled { step, .. }
                if matches!(mode, Mode::Fanout | Mode::ReadyFanout) =>
            {
                self.board
                    .report(&token, Seen::StepSettled(Instant::now(), step.0));
                script["done"] = json!(done + 1);
                if done + 1 == waits {
                    ended("fanout done")
                } else {
                    EngineAction::Idle
                }
            }
            EngineEvent::Started { .. } if mode == Mode::Steps => probe_step(0)?,
            EngineEvent::StepSettled { step, .. } if mode == Mode::Steps => {
                self.board
                    .report(&token, Seen::StepSettled(Instant::now(), step.0));
                script["done"] = json!(done + 1);
                if done + 1 < waits {
                    probe_step(done + 1)?
                } else {
                    ended("steps done")
                }
            }
            EngineEvent::Started { .. } | EngineEvent::Woke if mode == Mode::Timers => {
                if done > 0 {
                    self.board.report(&token, Seen::Resolved(Instant::now()));
                }
                script["done"] = json!(done + 1);
                if done >= waits {
                    ended("timers done")
                } else {
                    let due = Instant::now() + Duration::from_millis(100);
                    self.board.report(&token, Seen::TimerDue(due));
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(infra)?
                        .as_millis();
                    EngineAction::Sleep {
                        until: lash_durable::DurableInstant((now + 100) as i64),
                        site: None,
                    }
                }
            }
            EngineEvent::Started { .. } => pin(0),
            EngineEvent::KeyPinned { name, key } => {
                self.board.report(&token, Seen::Pinned(Instant::now(), key));
                EngineAction::AwaitExternal { name, site: None }
            }
            EngineEvent::ExternalResolved { .. } => {
                self.board.report(&token, Seen::Resolved(Instant::now()));
                let done = done + 1;
                script["done"] = json!(done);
                if done < waits {
                    pin(done)
                } else {
                    ended("resolved")
                }
            }
            EngineEvent::ExternalTimedOut { .. } => ended("timed out"),
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the bench engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            _ => EngineAction::Idle,
        };
        let bytes = serde_json::to_vec(&script).map_err(infra)?;
        Ok((
            EngineState {
                staged: Vec::new(),
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
                "the bench engine stores no artifact `{artifact_ref}`"
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

/// The benchmark's admitted Once step reports its entry and completes null.
pub struct BenchSteps {
    pub board: Arc<ProcessBoard>,
}

#[async_trait::async_trait]
impl ProcessSteps for BenchSteps {
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
        let _ = step;
        Ok(StepAdmission::new(
            lash_vocabulary::tool_contract::ToolId::new("probe"),
            lash_vocabulary::tool_contract::ExecutionPolicy::Once,
            lash_protocol_types::ExecutionLimit::starting_at(
                now_ms,
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
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
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        let board = Arc::clone(&self.board);
        let owner = process.id.clone();
        let step = step.step().0.clone();
        let token = match process.input.as_ref() {
            lash_execution_contracts::process::ProcessInput::Engine { payload, .. } => {
                payload["token"].as_str().map(str::to_owned)
            }
            _ => None,
        };
        let Some(token) = token else {
            return lash_core_execution::runtime::actor::round::member_body(Box::new(|_| {
                Box::pin(async { lash_core_store::tool_run::SettledOutput::Interrupted })
            }));
        };
        lash_core_execution::runtime::actor::round::member_body(Box::new(move |_cancel| {
            Box::pin(async move {
                board.report(&token, Seen::StepStarted(Instant::now(), step));
                let barrier = board.barriers.lock_recover().get(&token).cloned();
                if let Some(barrier) = barrier {
                    barrier.wait().await;
                }
                lash_core_store::tool_run::SettledOutput::Completed(
                    lash_core_store::tool_run::Material::journal_local(
                        lash_core_store::tool_run::MaterialOwner::Process { process_id: owner },
                        lash_core_store::tool_run::MaterialRole::AttemptOutput,
                        "null".to_owned(),
                    ),
                )
            })
        }))
    }
}

fn probe_step(n: u64) -> Result<EngineAction, ProcessInfraError> {
    Ok(EngineAction::Steps {
        steps: vec![StepRequest::Tool {
            step: lash_execution_contracts::process::StepName(format!("probe-{n}")),
            effect: lash_vocabulary::legacy_names::EffectName::new("probe").map_err(infra)?,
            contract: lash_vocabulary::operations::ContractId::new("probe"),
            input: json!({}),
            site: None,
            matched: None,
        }],
        wake: None,
    })
}
