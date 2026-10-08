//! The runtime boundaries the generated world delivers beside its turns: a
//! tool attempt, an exec-code run and a durable effect.
//!
//! A tool attempt and an exec-code run cross the durable engine's effect
//! dispatch: their envelope runs on an [`ActorContext`] over the world's
//! engine, under the boundary's own admitted scope, as a turn's effect does,
//! with a scripted no-network outcome. A durable effect is a real turn on an
//! engine of its own: its tool body runs once and commits its outcome, the
//! node serving the turn stops in the next model call, and a fresh node
//! resumes the turn from its committed rows. The resumed turn must be served
//! the recorded outcome, never run the body again.

use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::runtime::RuntimeAttribution;
use lash_core::{
    AdmittedScope, EffectAddress, ExecResponse, ExecutionScope, PreparedToolCall,
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ToolAttemptLaunch, ToolCallOutput, ToolCallRecord, ToolId,
};
use lash_core_execution::ActorContext;
use lash_sansio::SessionId;

use super::*;
use crate::trace::value_digest;

pub(crate) const EFFECT_SCOPE_ID: &str = "lash-sim-runtime-boundaries";

/// The controller a tool or exec-code boundary's effect runs on, as its
/// observation names it: the durable engine's effect dispatch.
const RUNTIME_EFFECT_CONTROLLER: &str = "runtime_effect_controller";

/// The tool a durable-effect boundary's turn calls.
const OPAQUE_EFFECT_TOOL: &str = "sim_opaque_effect";

/// How long a durable-effect turn may take to reach its cut.
const DURABLE_EFFECT_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// The scope a tool or exec-code boundary's effect runs under: a turn of the
/// boundary's session, as a session's own turn runs its effects.
fn boundary_effect_scope(event: &BoundaryEvent) -> ExecutionScope {
    ExecutionScope::turn(
        SessionId::fixture(event.actor_alias.clone()),
        lash_core::TurnId::fixture(format!("{EFFECT_SCOPE_ID}:{}", event.boundary_id)),
    )
}

#[derive(Debug)]
pub struct RuntimeBoundaryError {
    message: String,
}

impl RuntimeBoundaryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RuntimeBoundaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeBoundaryError {}

impl From<FixedScriptRunnerError> for RuntimeBoundaryError {
    fn from(error: FixedScriptRunnerError) -> Self {
        Self::new(error.to_string())
    }
}

pub(crate) struct RuntimeBoundaryHarness {
    seed: u64,
    /// The engine tool and exec-code effects run on, opened on first use.
    effects: Option<crate::backend::SimEngine>,
    /// Each durable effect's engine, by durable key.
    durable_engines: BTreeMap<String, crate::backend::SimEngine>,
}

impl RuntimeBoundaryHarness {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed,
            effects: None,
            durable_engines: BTreeMap::new(),
        }
    }

    /// Every store the harness's engines wrote, by label, for the global
    /// invariants.
    pub(crate) fn stores(
        &self,
    ) -> impl Iterator<Item = (String, &lash_sqlite_store::SqliteStoreSet)> {
        self.effects
            .iter()
            .map(|engine| ("effects".to_owned(), engine.stores()))
            .chain(
                self.durable_engines
                    .iter()
                    .map(|(key, engine)| (format!("durable/{key}"), engine.stores())),
            )
    }

    pub(crate) async fn deliver(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        match event.kind {
            BoundaryKind::Tool => self.complete_tool(event).await,
            BoundaryKind::ExecCode => self.execute_code(event).await,
            BoundaryKind::DurableEffect => self.complete_durable_effect(event).await,
            kind => Err(RuntimeBoundaryError::new(format!(
                "runtime boundary harness does not own {kind}"
            ))),
        }
    }

    /// The engine every tool and exec-code effect runs on.
    async fn effects_engine(&mut self) -> Result<crate::backend::SimEngine, RuntimeBoundaryError> {
        if let Some(engine) = &self.effects {
            return Ok(engine.clone());
        }
        let engine = crate::backend::SimEngine::new(self.seed).await?;
        self.effects = Some(engine.clone());
        Ok(engine)
    }

    /// Run `envelope` once on the engine's effect dispatch under `scope`,
    /// with a local executor that answers `scripted`, counting its calls.
    async fn run_once(
        &mut self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        scripted: RuntimeEffectOutcome,
    ) -> Result<(RuntimeEffectOutcome, usize), RuntimeBoundaryError> {
        let engine = self.effects_engine().await?;
        let context = ActorContext::detached(engine.backend())
            .scoped(AdmittedScope::new(scope.clone()))
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = Arc::clone(&calls);
        let local = RuntimeEffectLocalExecutor::testing(move |_| async move {
            executor_calls.fetch_add(1, Ordering::SeqCst);
            Ok(scripted)
        });
        let outcome = match &envelope.command {
            RuntimeEffectCommand::ExecCode { .. } => context.vm_effect(envelope, local).await,
            _ => context.tool_effect(envelope, local).await,
        }
        .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        Ok((outcome, calls.load(Ordering::SeqCst)))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn complete_tool(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let tool_name = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("sim_tool")
            .to_string();
        let output = event
            .payload
            .get("output")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let args = json!({
            "boundary_id": event.boundary_id,
            "session": event.actor_alias,
        });
        let call = PreparedToolCall {
            call_id: lash_core::ToolCallId::fixture(&event.boundary_id),
            provider_call_id: None,
            tool_id: ToolId::from(format!("tool:{tool_name}")),
            tool_name: tool_name.clone(),
            args: args.clone(),
            replay: None,
            prepared_payload: json!({"prepared_by": "lash-sim"}),
        };
        let scope = boundary_effect_scope(event);
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                EffectAddress::new(
                    scope.clone(),
                    format!("tool/{}/{}", event.actor_alias, event.boundary_id),
                )
                .expect("tool boundary carries an admitted effect scope"),
                RuntimeAttribution::for_session(SessionId::fixture(event.actor_alias.clone())),
                format!("tool-attempt:{}", event.boundary_id),
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: Box::new(call),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let scripted = RuntimeEffectOutcome::ToolAttempt {
            launch: Box::new(ToolAttemptLaunch::Done {
                record: Box::new(ToolCallRecord {
                    call_id: lash_core::ToolCallId::fixture(&event.boundary_id),
                    provider_call_id: None,
                    tool: tool_name.clone(),
                    args,
                    output: ToolCallOutput::success(output.clone()),
                }),
                intents: lash_core::ToolIntents::default(),
            }),
            triggers: Vec::new(),
        };
        let (outcome, execution_count) = self
            .run_once(&scope, envelope, scripted)
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("tool effect failed: {err}")))?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
            return Err(RuntimeBoundaryError::new(
                "tool effect dispatch returned a non-tool-attempt outcome",
            ));
        };
        let ToolAttemptLaunch::Done { record, .. } = *launch;
        Ok(json!({
            "session": event.actor_alias,
            "tool_output": output,
            "tool_name": tool_name,
            "tool_call_id": event.boundary_id,
            "execution_count": execution_count,
            "runtime_tool_output": record.output,
            "runtime_tool_record": record,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ToolAttempt.as_str(),
                "controller": RUNTIME_EFFECT_CONTROLLER,
                "local_executor_called": execution_count > 0,
            },
        }))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn execute_code(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let output = event
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let exit_code = event
            .payload
            .get("exit_code")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let code = format!("sim_exec('{}')", event.boundary_id);
        let scope = boundary_effect_scope(event);
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                EffectAddress::new(
                    scope.clone(),
                    format!("exec/{}/{}", event.actor_alias, event.boundary_id),
                )
                .expect("exec boundary carries an admitted effect scope"),
                RuntimeAttribution::for_session(SessionId::fixture(event.actor_alias.clone())),
                format!("exec-code:{}", event.boundary_id),
            ),
            RuntimeEffectCommand::ExecCode { code },
        );
        let response = ExecResponse {
            output_archive: None,
            observations: vec![lash_core::Observation {
                text: output.clone(),
                value: json!(output),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: (exit_code != 0).then(|| {
                lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Program,
                    format!("exit code {exit_code}"),
                )
            }),
            degraded_bindings: Vec::new(),
            terminal_finish: Some(json!({
                "output": output,
                "exit_code": exit_code,
            })),
            terminal_finish_retained: None,
            suspended: false,
        };
        let (outcome, execution_count) = self
            .run_once(
                &scope,
                envelope,
                RuntimeEffectOutcome::ExecCode {
                    result: Box::new(Ok(response)),
                },
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("exec-code effect failed: {err}")))?;
        Ok(json!({
            "session": event.actor_alias,
            "exec_output": output,
            "exit_code": exit_code,
            "execution_count": execution_count,
            "runtime_effect_outcome": outcome,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ExecCode.as_str(),
                "controller": RUNTIME_EFFECT_CONTROLLER,
                "local_executor_called": execution_count > 0,
            },
        }))
    }

    /// A durable effect under crash and redrive. The effect's turn calls the
    /// opaque effect tool, whose body runs and commits its outcome with the
    /// tool round; the turn's next model call never answers, and the node
    /// serving it stops there. A fresh node, whose tool body would produce
    /// `redrive_result`, resumes the turn from its committed rows: the
    /// recorded effect must not run again, so the resumed turn is served the
    /// first run's outcome.
    pub async fn complete_durable_effect(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let durable_key = event
            .payload
            .get("durable_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let requested_result = event
            .payload
            .get("result")
            .cloned()
            .unwrap_or_else(|| json!({"completed": true}));
        let redrive_result = event
            .payload
            .get("redrive_result")
            .cloned()
            .unwrap_or_else(|| json!({"completed": false}));
        let effect_id = event
            .payload
            .get("runtime_effect")
            .and_then(|value| value.get("effect_id"))
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let args = json!({
            "durable_key": durable_key,
            "session": event.actor_alias,
        });
        let engine = crate::backend::SimEngine::new(self.seed).await?;
        self.durable_engines
            .insert(durable_key.clone(), engine.clone());
        let session_id = SessionId::fixture(event.actor_alias.clone());
        let turn_id = lash_core::TurnId::fixture(format!("{EFFECT_SCOPE_ID}:{durable_key}"));

        // The first node: its turn's body runs once, and its next model call
        // stops the turn where the node is cut.
        let first_calls = Arc::new(AtomicUsize::new(0));
        let cut_reached = Arc::new(tokio::sync::Notify::new());
        let first = durable_effect_core(
            &engine,
            "first",
            &durable_key,
            cut_provider(&effect_id, &args, Arc::clone(&cut_reached)),
            OpaqueEffectTool::new(requested_result.clone(), Arc::clone(&first_calls)),
        )?;
        let session = crate::open_created_session(DURABLE_EFFECT_MODEL, &first, session_id.clone())
            .await
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        session
            .durable()
            .send(lash::TurnInput::text(format!(
                "run the durable effect {durable_key}"
            )))
            .id(turn_id.clone())
            .await
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        tokio::time::timeout(DURABLE_EFFECT_BOUND, cut_reached.notified())
            .await
            .map_err(|_| {
                RuntimeBoundaryError::new(format!(
                    "durable effect `{durable_key}` never reached the model call after its tool round"
                ))
            })?;
        first
            .shutdown()
            .await
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        drop(session);
        drop(first);

        // The redrive: a fresh node resumes the turn from its rows.
        let redrive_calls = Arc::new(AtomicUsize::new(0));
        let redrive = durable_effect_core(
            &engine,
            "redrive",
            &durable_key,
            finishing_provider(),
            OpaqueEffectTool::new(redrive_result, Arc::clone(&redrive_calls)),
        )?;
        let resumed = redrive
            .session(session_id.clone())
            .durable()
            .await
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        let output =
            tokio::time::timeout(DURABLE_EFFECT_BOUND, resumed.attach_id(turn_id).output())
                .await
                .map_err(|_| {
                    RuntimeBoundaryError::new(format!(
                        "durable effect `{durable_key}` was not resumed by the redrive"
                    ))
                })?
                .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        redrive
            .shutdown()
            .await
            .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;
        let [record] = output.result.tool_calls.as_slice() else {
            return Err(RuntimeBoundaryError::new(format!(
                "durable effect `{durable_key}` resumed with {} tool calls, expected one",
                output.result.tool_calls.len()
            )));
        };
        let first_calls = first_calls.load(Ordering::SeqCst);
        let redrive_calls = redrive_calls.load(Ordering::SeqCst);
        let first_digest = value_digest(&requested_result);
        let redrive_digest = value_digest(&record.output.value_for_projection());
        let replayed = redrive_calls == 0;
        Ok(json!({
            "durable_key": durable_key,
            "result_digest": first_digest,
            "redrive_result_digest": redrive_digest,
            "redrive_served_recorded_result": first_digest == redrive_digest,
            "execution_count": first_calls + redrive_calls,
            "replay_count": usize::from(replayed),
            "replayed": replayed,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ToolAttempt.as_str(),
                "effect_id": effect_id,
                "controller": DURABLE_EFFECT_CONTROLLER,
                "local_executor_called": first_calls > 0,
                "redrive_local_executor_called": redrive_calls > 0,
            },
            "runtime_tool_record": {
                "provider_call_id": record.provider_call_id,
                "tool": record.tool,
                "args": record.args,
                "output": record.output,
            },
        }))
    }
}

/// The model a durable effect's turn calls.
const DURABLE_EFFECT_MODEL: &str = "lash-sim-durable-effect";

/// The controller a durable effect runs on, as its observation names it: a
/// tool round of a turn on the durable engine.
pub(crate) const DURABLE_EFFECT_CONTROLLER: &str = "durable_turn_tool_round";

/// A core over `engine` that serves `provider` and `tool` as the
/// durable-effect boot `boot` of `durable_key`: each boot is its own node,
/// so the redrive's fences the first's.
fn durable_effect_core(
    engine: &crate::backend::SimEngine,
    boot: &str,
    durable_key: &str,
    provider: ProviderHandle,
    tool: OpaqueEffectTool,
) -> Result<lash::LashCore, RuntimeBoundaryError> {
    lash::LashCore::standard_builder(engine.backend())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder(DURABLE_EFFECT_MODEL)
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| RuntimeBoundaryError::new(error.to_string()))?,
        )
        .tools(Arc::new(tool) as Arc<dyn lash_core::ToolProvider>)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            format!("lash-sim-durable-effect-{boot}"),
            durable_key.to_owned(),
        ))
        .map_err(|err| RuntimeBoundaryError::new(err.to_string()))
}

/// The first boot's model: it calls the opaque effect, and its next call
/// signals `cut_reached` and never answers, so its node is stopped there.
fn cut_provider(
    effect_id: &str,
    args: &Value,
    cut_reached: Arc<tokio::sync::Notify>,
) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    let effect_call = tool_call_llm_response(effect_id, OPAQUE_EFFECT_TOOL, &args.to_string());
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-durable-effect-first")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            let cut_reached = Arc::clone(&cut_reached);
            let effect_call = effect_call.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(effect_call);
                }
                cut_reached.notify_one();
                std::future::pending::<()>().await;
                Err(LlmTransportError::new("the cut model call never answers"))
            }
        })
        .build()
        .into_handle()
}

/// The redrive's model: the resumed turn's model call answers at once.
fn finishing_provider() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-durable-effect-redrive")
        .complete(|_request| async { Ok(text_llm_response("done")) })
        .build()
        .into_handle()
}

/// The opaque effect: its body answers `result` and counts its runs.
struct OpaqueEffectTool {
    result: Value,
    calls: Arc<AtomicUsize>,
}

impl OpaqueEffectTool {
    fn new(result: Value, calls: Arc<AtomicUsize>) -> Self {
        Self { result, calls }
    }

    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{OPAQUE_EFFECT_TOOL}"),
            OPAQUE_EFFECT_TOOL,
            "Run an opaque durable effect.",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .expect("valid declared tool schemas")
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for OpaqueEffectTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == OPAQUE_EFFECT_TOOL).then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() != OPAQUE_EFFECT_TOOL {
            return lash_core::ToolOutcome::err_fmt(format_args!("unknown tool {}", call.name()))
                .into();
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(self.result.clone()).into()
    }
}
