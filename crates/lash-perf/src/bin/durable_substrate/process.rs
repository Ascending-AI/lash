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

use lash_core_execution::runtime::actor::waits::PinnedKey;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, HostWaitKind, KeyName,
    ProcessEngine, ProcessInfraError, ProcessOutcome, ProcessRecord, StepRequest, ToolCallOutput,
    ToolCancellation,
};
use lash_sansio::sync::MutexExt as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The engine's kind.
pub const KIND: &str = "durable-substrate-bench";

/// What the engine saw, by the process's token.
#[derive(Debug)]
pub enum Seen {
    /// A key was pinned; the host may resolve it.
    Pinned(Instant, PinnedKey),
    /// A resolution reached the engine.
    Resolved(Instant),
}

/// Where engines report, by the token in their start payload.
#[derive(Default)]
pub struct ProcessBoard {
    feeds: Mutex<HashMap<String, mpsc::UnboundedSender<Seen>>>,
}

impl ProcessBoard {
    /// Follow the process started with `token`.
    pub fn follow(&self, token: &str) -> mpsc::UnboundedReceiver<Seen> {
        let (send, receive) = mpsc::unbounded_channel();
        self.feeds.lock_recover().insert(token.to_owned(), send);
        receive
    }

    fn report(&self, token: &str, seen: Seen) {
        if let Some(feed) = self.feeds.lock_recover().get(token) {
            let _ = feed.send(seen);
        }
    }
}

/// The start payload of a process that awaits `waits` keys.
pub fn payload(token: &str, waits: usize) -> Value {
    json!({ "token": token, "waits": waits })
}

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core_execution::PluginError::Session(error.to_string()))
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
        kind: HostWaitKind::Custom,
        deadline: Some(Duration::from_secs(3_600)),
    }
}

fn ended(text: &str) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::success(
        json!({ "ended": text }),
    )))
}

#[async_trait::async_trait]
impl ProcessEngine for BenchEngine {
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
            EngineEvent::Started { payload } => {
                json!({ "token": payload["token"], "waits": payload["waits"], "done": 0 })
            }
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let token = script["token"].as_str().unwrap_or_default().to_owned();
        let waits = script["waits"].as_u64().unwrap_or(1);
        let done = script["done"].as_u64().unwrap_or(0);
        let action = match event {
            EngineEvent::Started { .. } => pin(0),
            EngineEvent::KeyPinned { name, key } => {
                self.board.report(&token, Seen::Pinned(Instant::now(), key));
                EngineAction::AwaitExternal { name }
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
            "the bench engine stores no artifact `{artifact_ref}`"
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

/// The engine runs no step.
#[derive(Debug, Default)]
pub struct NoSteps;

#[async_trait::async_trait]
impl ProcessSteps for NoSteps {
    async fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        _now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Err(StepRefusal::Refused {
            step: format!("{step:?}"),
            reason: "the bench engine runs no step".to_owned(),
        })
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
    }

    fn body(
        &self,

        _cx: &lash_core_execution::ActorContext,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        lash_core_execution::runtime::actor::round::member_body({
            Box::new(|_token| {
                Box::pin(async {
                    lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
                })
            })
        })
    }
}
