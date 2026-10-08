use super::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

pub(super) fn measure_runtime_perf_phase<T>(
    name: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<(T, (String, RuntimePerfPhaseRunResult))> {
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    let value = f()?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        value,
        (
            name.to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(started),
                allocations: alloc_delta(before_alloc, after_alloc),
                rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
            },
        ),
    ))
}

pub(super) async fn measure_runtime_perf_async_phase<T, F>(
    name: &'static str,
    future: F,
) -> anyhow::Result<(T, (String, RuntimePerfPhaseRunResult))>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    let value = future.await?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        value,
        (
            name.to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(started),
                allocations: alloc_delta(before_alloc, after_alloc),
                rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
            },
        ),
    ))
}

pub(super) async fn run_once_turn_checkpoint(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let mut run = RunRecorder::start(RuntimePerfScenario::TurnCheckpoint, chat_turns);
    let configs = run.build(async { Ok(CheckpointConfigs::new()) }).await?;
    let seed_messages = run.seed(async { Ok(checkpoint_messages()) }).await?;

    for turn_index in 0..chat_turns {
        run.turn(
            turn_index,
            async {
                let mut phase_profile = BTreeMap::new();

                let llm_phase = measure_checkpoint_phase("standard_llm_checkpoint", || {
                    checkpoint_pending_llm(&configs, &seed_messages, turn_index)
                })?;
                phase_profile.insert(llm_phase.0, llm_phase.1);

                let tools_phase =
                    measure_checkpoint_phase("standard_parallel_tools_checkpoint", || {
                        checkpoint_pending_parallel_tools(&configs, &seed_messages, turn_index)
                    })?;
                phase_profile.insert(tools_phase.0, tools_phase.1);

                let exec_phase = measure_checkpoint_phase("rlm_exec_checkpoint", || {
                    checkpoint_pending_exec(&configs, &seed_messages, turn_index)
                })?;
                phase_profile.insert(exec_phase.0, exec_phase.1);

                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
                        ..TurnTail::default()
                    },
                })
            },
            async {
                tokio::task::yield_now().await;
                Ok(())
            },
        )
        .await?;
    }

    run.export(async { serde_json::to_vec(&seed_messages).map_err(anyhow::Error::from) })
        .await?;

    Ok(run.finish(RunTail {
        session_nodes: seed_messages.len(),
        active_path_messages: seed_messages.len(),
        ..RunTail::default()
    }))
}

const CHECKPOINT_STATE_BINDINGS: usize = 300;
const CHECKPOINT_STATE_BODY_BYTES: usize = 3 * 1024 + 512;

/// A checkpoint fixture edited by a plugin task on the served session actor.
/// Capture remains outside the task so the curve measures each structural step.
pub(super) struct CheckpointBindingFixture {
    fixture: Arc<tokio::sync::Mutex<lash_protocol_rlm::RlmCheckpointPerfFixture>>,
    backend: lash::Backend,
    factory: Arc<dyn lash_core::plugin::PluginFactory>,
    session_id: SessionId,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CheckpointAssignmentError {
    SessionRequired,
    Execution {
        reason: lash_core::ExecCodeFailureReason,
        message: String,
    },
}

impl std::fmt::Display for CheckpointAssignmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionRequired => formatter.write_str("checkpoint task requires a session"),
            Self::Execution { message, .. } => formatter.write_str(message),
        }
    }
}

struct AssignCheckpointBinding;
impl lash_core::plugin::PluginOperation for AssignCheckpointBinding {
    const NAME: &'static str = "checkpoint.assign";
    const DESCRIPTION: &'static str = "Edit one benchmark checkpoint binding";
    const SESSION_PARAM: lash_core::plugin::SessionParam =
        lash_core::plugin::SessionParam::Required;
    type Args = (usize, usize);
    type Output = ();
    type Error = CheckpointAssignmentError;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &CheckpointAssignmentError) -> lash_core::plugin::PluginFailureClass {
        lash_core::plugin::PluginFailureClass::Terminal
    }
}
impl lash_core::plugin::PluginTask for AssignCheckpointBinding {}

impl CheckpointBindingFixture {
    pub(super) async fn new(
        dialect: Arc<dyn lash_protocol_rlm::Dialect>,
        bindings: usize,
        bytes: usize,
    ) -> anyhow::Result<Self> {
        let backend = durable_backend(Arc::new(sqlite_memory_stores().await?))?;
        let fixture = Arc::new(tokio::sync::Mutex::new(
            lash_protocol_rlm::RlmCheckpointPerfFixture::new(dialect, &backend, bindings, bytes)
                .await?,
        ));
        let task_fixture = Arc::clone(&fixture);
        let task_backend = backend.clone();
        let spec = lash_core::plugin::PluginSpec::new()
            .with_plugin_task_value::<AssignCheckpointBinding, _, _>(move |ctx, (index, turn)| {
                let fixture = Arc::clone(&task_fixture);
                let backend = task_backend.clone();
                async move {
                    let session_id = ctx
                        .session_id
                        .ok_or(CheckpointAssignmentError::SessionRequired)?;
                    let invocation = lash_core::runtime::causal::turn_effect_invocation(
                        ctx.scoped_effect_controller.execution_scope(),
                        &session_id,
                        &lash_core::TurnId::fixture(format!("checkpoint-{turn}")),
                        turn,
                        0,
                        lash_core::sansio::EffectId(0),
                        lash_core::RuntimeEffectKind::ExecCode,
                    );
                    let execution = lash_core::testing::TestExecutionContextBuilder::new(
                        lash_core::testing::TestExecutionPorts::lent(
                            &backend,
                            ctx.scoped_effect_controller,
                        ),
                    )
                    .runtime_parent_invocation(invocation.into_runtime_invocation())
                    .build()
                    .into_runtime();
                    fixture
                        .lock()
                        .await
                        .assign_one(index, turn, execution)
                        .await
                        .map_err(|error| {
                            let failure = error.to_exec_code_failure();
                            CheckpointAssignmentError::Execution {
                                reason: failure.reason,
                                message: failure.message,
                            }
                        })
                }
            });
        let factory: Arc<dyn lash_core::plugin::PluginFactory> =
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("checkpoint_benchmark"),
                spec,
            ));
        let core = super::super::harness::checkpoint_benchmark_core(
            backend.clone(),
            Arc::clone(&factory),
        )?;
        let session_id = SessionId::fixture(format!("checkpoint-worker-{}", uuid::Uuid::new_v4()));
        let creation = lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                "mock-model",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        );
        core.session(session_id.clone()).create(creation).await?;
        core.shutdown().await?;
        Ok(Self {
            fixture,
            backend,
            factory,
            session_id,
        })
    }

    pub(super) async fn assign(&self, index: usize, turn: usize) -> anyhow::Result<()> {
        // The structural work collector is process-wide. Join the served
        // node before capture so task settlement cannot enter its hash samples.
        let core = super::super::harness::checkpoint_benchmark_core(
            self.backend.clone(),
            Arc::clone(&self.factory),
        )?;
        let session = core.session(self.session_id.clone()).open().await?;
        let answer = session
            .plugin_operations()
            .start_task::<AssignCheckpointBinding>(
                (index, turn),
                format!("checkpoint-binding:{index}:{turn}"),
            )
            .await?
            .result()
            .await;
        let close = session.close().await;
        let shutdown = core.shutdown().await;
        answer?;
        close?;
        shutdown?;
        Ok(())
    }

    pub(super) async fn capture(
        &self,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, lash_core::SessionError> {
        self.fixture.lock().await.capture().await
    }

    pub(super) async fn acknowledge_capture(&self) {
        self.fixture.lock().await.acknowledge_capture();
    }
}

pub(super) async fn run_once_checkpoint_state_hot_paths(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::CheckpointStateHotPaths;
    let mut run = RunRecorder::start(scenario, chat_turns);

    let (fixture, store, mut runtime_state) = run
        .build(async {
            // Cells run on the production effect controller over a memory
            // store set; their captured state is what is measured.
            let fixture = CheckpointBindingFixture::new(
                std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
                CHECKPOINT_STATE_BINDINGS,
                CHECKPOINT_STATE_BODY_BYTES,
            )
            .await?;
            let runtime_state = RuntimeSessionState {
                session_id: SessionId::from("runtime-perf-checkpoint-state"),
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ))
            };
            let store = memory_perf_store(&runtime_state.session_id).await?;
            store
                .admit_session(&runtime_perf_session_create_request(
                    &runtime_state.session_id,
                ))
                .await?;
            Ok((fixture, store, runtime_state))
        })
        .await?;

    let (initial_component_count, initial_capture_phase) = run
        .seed(async {
            let (initial_snapshot, initial_capture_phase) =
                measure_runtime_perf_async_phase("checkpoint_state.initial_capture", async {
                    fixture.capture().await.map_err(anyhow::Error::from)
                }).await?;
            if initial_snapshot.root().is_none() {
                anyhow::bail!("checkpoint-state fixture omitted its root");
            }
            let initial_component_count =
                changed_execution_state_components(&initial_snapshot)?.len();
            if initial_component_count != CHECKPOINT_STATE_BINDINGS {
                anyhow::bail!(
                    "checkpoint-state fixture captured {initial_component_count} components, expected {CHECKPOINT_STATE_BINDINGS}"
                );
            }
            lash_core::testing::stage_execution_state_components(
                &mut runtime_state,
                initial_snapshot.clone(),
            )?;
            let initial_commit = RuntimeCommit::persisted_state_for_test_with_budget(
                &runtime_state,
                lash_core::CommitBudget::bounded(8 * 1024 * 1024, 2_048),
            );
            let initial_result = store.commit_runtime_state(initial_commit).await?;
            runtime_state.apply_persisted_commit_result(initial_result);
            fixture.acknowledge_capture().await;
            Ok((initial_component_count, initial_capture_phase))
        })
        .await?;

    let mut last_checkpoint_bytes = 0_u64;
    let mut last_changed_components = 0_u64;
    let mut last_hydrated_bytes = 0_u64;
    for turn_index in 0..chat_turns {
        fixture.assign(turn_index, turn_index).await?;
        run.turn(
            turn_index,
            async {
        let mut phase_profile = BTreeMap::new();
        if turn_index == 0 {
            phase_profile.insert(
                initial_capture_phase.0.clone(),
                initial_capture_phase.1.clone(),
            );
        }

        let (snapshot, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.incremental_capture", async {
                fixture.capture().await.map_err(anyhow::Error::from)
            }).await?;
        phase_profile.insert(phase.0, phase.1);
        fixture.acknowledge_capture().await;
        if snapshot.root().is_none() {
            anyhow::bail!("incremental checkpoint capture omitted its root");
        }
        last_changed_components = snapshot.leaves()
            .values()
            .filter(|component| {
                matches!(
                    component,
                    lash_core::plugin::LeafChange::Changed(_)
                )
            })
            .count() as u64;
        if last_changed_components != 1 {
            anyhow::bail!(
                "incremental checkpoint captured {last_changed_components} changed components, expected 1"
            );
        }
        lash_core::testing::stage_execution_state_components(&mut runtime_state, snapshot)?;
        let commit = RuntimeCommit::persisted_state_for_test_with_budget(
            &runtime_state,
            lash_core::CommitBudget::bounded(8 * 1024 * 1024, 2_048),
        );

        let (budget_measurement, phase) =
            measure_runtime_perf_phase("checkpoint_state.measure_budget", || {
                lash_core::testing::measure_runtime_commit_budget(&commit)
                    .map_err(anyhow::Error::from)
            })?;
        phase_profile.insert(phase.0, phase.1);
        last_checkpoint_bytes = budget_measurement.checkpoint_bytes as u64;

        let (commit_result, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.component_commit", async {
                store
                    .commit_runtime_state(commit)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        runtime_state.apply_persisted_commit_result(commit_result);

        let (loaded_execution_state, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.component_load", async {
                let persisted = store
                    .load_session_window(
                        &runtime_state.session_id,
                        lash_core::store::WindowSelector::Current,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("checkpoint-state commit was not loadable"))?;
                execution_state_from_checkpoint(
                    persisted
                        .checkpoint
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("loaded session omitted checkpoint"))?,
                )
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        last_hydrated_bytes = (loaded_execution_state.root.len()
            + loaded_execution_state
                .components
                .values()
                .map(|v| v.len())
                .sum::<usize>()) as u64;

        let (_, phase) = measure_runtime_perf_async_phase("checkpoint_state.execution_restore", async {
            lash_protocol_rlm::RlmCheckpointPerfFixture::restore(&lash_protocol_rlm::TypescriptDialect, &loaded_execution_state).await
                .map_err(anyhow::Error::from)
        }).await?;
        phase_profile.insert(phase.0, phase.1);

                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
                        ..TurnTail::default()
                    },
                })
            },
            async {
                tokio::task::yield_now().await;
                Ok(())
            },
        )
        .await?;
    }

    run.export(async { Ok(()) }).await?;
    let mut extra_counters = BTreeMap::new();
    extra_counters.insert(
        "execution_state_bindings".to_string(),
        CHECKPOINT_STATE_BINDINGS as u64,
    );
    extra_counters.insert(
        "execution_state_components".to_string(),
        initial_component_count as u64,
    );
    extra_counters.insert(
        "incremental_changed_components".to_string(),
        last_changed_components,
    );
    extra_counters.insert("checkpoint_bytes".to_string(), last_checkpoint_bytes);
    extra_counters.insert(
        "hydrated_execution_state_bytes".to_string(),
        last_hydrated_bytes,
    );

    Ok(run.finish(RunTail {
        extra_counters,
        ..RunTail::default()
    }))
}

fn changed_execution_state_components(
    snapshot: &lash_core::plugin::ExecutionStateCapture,
) -> anyhow::Result<BTreeMap<lash_core::plugin::ExecutionLeafName, Arc<[u8]>>> {
    snapshot
        .leaves()
        .iter()
        .map(|(key, component)| match component {
            lash_core::plugin::LeafChange::Changed(body) => Ok((key.clone(), body.clone())),
            lash_core::plugin::LeafChange::Unchanged => {
                anyhow::bail!("initial checkpoint-state component `{key}` was unchanged")
            }
        })
        .collect()
}

fn execution_state_from_checkpoint(
    checkpoint: &lash_core::HydratedSessionCheckpoint,
) -> anyhow::Result<lash_core::plugin::HydratedExecutionState> {
    let root = checkpoint
        .component(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .and_then(lash_core::HydratedCheckpointComponent::body_arc)
        .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted execution-state root"))?;
    let mut components = BTreeMap::new();
    for key in checkpoint.components.keys() {
        let Some(leaf) = lash_core::plugin::ExecutionLeafName::parse(key) else {
            continue;
        };
        let body = checkpoint
            .component(key)
            .and_then(lash_core::HydratedCheckpointComponent::body_arc)
            .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted body for `{key}`"))?;
        components.insert(leaf, body);
    }
    Ok(lash_core::plugin::HydratedExecutionState { root, components })
}

struct CheckpointConfigs {
    llm: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
    tools: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
    exec: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
}

impl CheckpointConfigs {
    fn new() -> Self {
        Self {
            llm: Arc::new(CheckpointDriver::Llm),
            tools: Arc::new(CheckpointDriver::Tools),
            exec: Arc::new(CheckpointDriver::Exec),
        }
    }

    fn llm_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.llm))
    }

    fn tools_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.tools))
    }

    fn exec_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.exec))
    }
}

#[derive(Clone, Copy)]
enum CheckpointDriver {
    Llm,
    Tools,
    Exec,
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for CheckpointDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        match self {
            Self::Llm => vec![DriverAction::Start(PendingWork::Llm {
                request: match ctx.project_llm_request(false) {
                    Ok(request) => request,
                    Err(error) => {
                        return lash_sansio::sansio::stored_history_refusal_actions(error);
                    }
                },
                driver_state: None,
            })],
            Self::Tools => vec![DriverAction::Start(PendingWork::WaitingForToolResults {
                settled: None,
                calls: checkpoint_tool_calls(ctx.protocol_iteration()),
                expansion: Default::default(),
            })],
            Self::Exec => vec![DriverAction::Start(PendingWork::Exec {
                language: "code".to_string(),
                code: checkpoint_exec_code(ctx.protocol_iteration()),
                driver_state: lash_core::ProtocolDriverState::new(
                    "runtime_perf_checkpoint",
                    serde_json::json!({
                        "phase": "exec_code",
                        "ip": ctx.protocol_iteration(),
                        "stack": (0..64).map(|index| serde_json::json!({
                            "slot": index,
                            "value": format!("checkpoint-stack-value-{index}")
                        })).collect::<Vec<_>>(),
                    }),
                ),
            })],
        }
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        _llm_response: LlmResponse,
        _calls: &lash_sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::AssistantMessage {
                text: "runtime perf benchmark ok".to_string(),
            },
        ))]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::AssistantMessage {
                text: "runtime perf benchmark ok".to_string(),
            },
        ))]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        _result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::FinalValue {
                value: serde_json::json!("runtime perf benchmark ok"),
            },
        ))]
    }
}

fn checkpoint_config(
    protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "mock-model".to_string(),
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                    lash_sansio::llm::capability::CacheRetention::Short,
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(8),
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        session_id: SessionId::from("runtime-perf-turn-checkpoint"),
        agent_frame_id: "runtime-perf-turn-frame".to_string(),
        turn_id: TurnId::from("runtime-perf-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

pub(crate) fn checkpoint_messages() -> Vec<Message> {
    (0usize..36)
        .map(|index| {
            let role = if index.is_multiple_of(2) {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            checkpoint_message(
                format!("checkpoint-msg-{index}"),
                role,
                format!(
                    "Historical checkpoint profiler message {index}. This payload is intentionally long enough to make TurnCheckpoint serialization include realistic prompt and transcript bytes. The current topic is standard and RLM turn-effect replay across LLM, tool, checkpoint, sleep, and ExecCode boundaries."
                ),
            )
        })
        .collect()
}

pub(super) fn checkpoint_message(id: String, role: MessageRole, content: String) -> Message {
    Message {
        id: id.clone(),
        role,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), content, None)]),
        origin: None,
        reply_marker: None,
    }
}

fn measure_checkpoint_phase(
    name: &'static str,
    f: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<(String, RuntimePerfPhaseRunResult)> {
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    f()?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        name.to_string(),
        RuntimePerfPhaseRunResult {
            samples: 1,
            duration_ms: elapsed_ms(started),
            allocations: alloc_delta(before_alloc, after_alloc),
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
        },
    ))
}

fn checkpoint_pending_llm(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.llm_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint llm scenario produced no effect"))?;
    let Effect::LlmCall { id, .. } = effect else {
        anyhow::bail!("checkpoint llm scenario expected LlmCall effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored =
        TurnMachine::restore_from_checkpoint(configs.llm_config(), checkpoint, None)?;
    assert_restored_llm(&mut restored, id)?;
    restored.handle_response(Response::LlmComplete {
        id,
        result: Ok(LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "runtime perf benchmark ok".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
        text_streamed: false,
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_pending_parallel_tools(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.tools_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint tools scenario produced no effect"))?;
    let Effect::ToolCalls { id, calls, .. } = effect else {
        anyhow::bail!("checkpoint tools scenario expected ToolCalls effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored =
        TurnMachine::restore_from_checkpoint(configs.tools_config(), checkpoint, None)?;
    assert_restored_tool_batch(&mut restored, id, calls.len())?;
    restored.handle_response(Response::ToolResults {
        id,
        results: calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| completed_checkpoint_tool(index, call))
            .collect(),
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_pending_exec(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.exec_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint exec scenario produced no effect"))?;
    let Effect::ExecCode { id, code, .. } = effect else {
        anyhow::bail!("checkpoint exec scenario expected ExecCode effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored =
        TurnMachine::restore_from_checkpoint(configs.exec_config(), checkpoint, None)?;
    assert_restored_exec(&mut restored, id, &code)?;
    restored.handle_response(Response::ExecResult {
        id,
        result: Ok(ExecResponse {
            output_archive: None,
            observations: vec![lash_core::Observation {
                text: "checkpoint observation: resumed after ExecCode effect boundary".to_string(),
                value: serde_json::json!(
                    "checkpoint observation: resumed after ExecCode effect boundary"
                ),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            tool_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            degraded_bindings: Vec::new(),
            terminal_finish: Some(serde_json::json!("runtime perf benchmark ok")),
            terminal_finish_retained: None,
            suspended: false,
        }),
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_machine(
    config: TurnMachineConfig,
    seed_messages: &[Message],
    turn_index: usize,
) -> TurnMachine {
    let mut messages = seed_messages.to_vec();
    messages.push(checkpoint_message(
        format!("checkpoint-live-turn-{turn_index}"),
        MessageRole::User,
        format!(
            "Durability checkpoint profiler live turn {}",
            turn_index + 1
        ),
    ));
    TurnMachine::new(config, messages, Default::default(), turn_index)
}

fn checkpoint_tool_calls(protocol_iteration: usize) -> Vec<PendingToolCall> {
    (0..24)
        .map(|index| PendingToolCall {
            call_id: lash_sansio::ToolCallId::fixture(&format!(
                "checkpoint-call-{protocol_iteration}-{index}"
            )),
            provider_call_id: None,
            tool_name: format!("checkpoint_parallel_tool_{}", index % 6),
            args: serde_json::json!({
                "index": index,
                "protocol_iteration": protocol_iteration,
                "payload": format!("synthetic parallel durability payload {index}")
            }),
            replay: None,
        })
        .collect()
}

fn completed_checkpoint_tool(index: usize, call: PendingToolCall) -> CompletedToolCall {
    let output = match index % 4 {
        0 => ToolCallOutput::success(serde_json::json!({
            "ok": true,
            "index": index,
            "payload": call.args,
        })),
        1 => ToolCallOutput::failure(ToolFailure::tool(
            ToolFailureClass::Execution,
            "checkpoint_tool_failed",
            format!("synthetic failure for {}", call.call_id),
        )),
        2 => ToolCallOutput::cancelled(ToolCancellation::runtime(format!(
            "synthetic cancellation for {}",
            call.call_id
        ))),
        _ => ToolCallOutput::success(serde_json::json!({
            "ok": true,
            "index": index,
            "large": "x".repeat(128),
        })),
    };
    CompletedToolCall {
        call_id: call.call_id.clone(),
        provider_call_id: call.provider_call_id.clone(),
        tool_name: call.tool_name.clone(),
        args: call.args,
        model_return: ModelToolReturn::from_output(call.tool_name.clone(), &output),
        output,
        intent_outcomes: Vec::new(),
        replay: call.replay,
    }
}

fn checkpoint_exec_code(protocol_iteration: usize) -> String {
    format!(
        r#"process benchmark_echo_process(tool: Tools, value: str, ordinal: int) {{
  result = await tool.benchmark_echo({{ value: value, ordinal: ordinal }})?
  finish result
}}

print("checkpoint turn {protocol_iteration}")
first = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 1)
second = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 2)
third = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 3)
fanout = await {{
  a: first,
  b: second,
  c: third
}}
finish fanout.a?.value"#
    )
}

fn assert_restored_llm(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::LlmCall { id, .. }) if id == expected_id => Ok(()),
        Some(_) => anyhow::bail!("restored checkpoint did not replay LlmCall"),
        None => anyhow::bail!("restored checkpoint had no LlmCall"),
    }
}

fn assert_restored_tool_batch(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
    expected_calls: usize,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::ToolCalls { id, calls, .. })
            if id == expected_id && calls.len() == expected_calls =>
        {
            Ok(())
        }
        Some(_) => anyhow::bail!("restored checkpoint did not replay matching ToolCalls"),
        None => anyhow::bail!("restored checkpoint had no ToolCalls"),
    }
}

fn assert_restored_exec(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
    expected_code: &str,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::ExecCode { id, code, .. }) if id == expected_id && code == expected_code => {
            Ok(())
        }
        Some(_) => anyhow::bail!("restored checkpoint did not replay matching ExecCode"),
        None => anyhow::bail!("restored checkpoint had no ExecCode"),
    }
}

fn drain_checkpoint_machine(machine: &mut TurnMachine) {
    while machine.poll_effect().is_some() {}
}

fn next_checkpoint_effect(machine: &mut TurnMachine) -> Option<Effect> {
    loop {
        match machine.poll_effect()? {
            // The profiled checkpoints carry the environment this sync
            // records.
            Effect::SyncExecutionEnvironment { id } => {
                machine.handle_response(lash_core::sansio::Response::ExecutionEnvironmentSynced {
                    id,
                    result: Ok(lash_core::sansio::ExecutionEnvironmentSync::default()),
                });
            }
            Effect::Emit(_)
            | Effect::ReportToolCalls { .. }
            | Effect::Log { .. }
            | Effect::Progress { .. }
            | Effect::Done { .. } => continue,
            effect => return Some(effect),
        }
    }
}

pub(crate) async fn run_once_embed(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let mut run = RunRecorder::start(scenario, chat_turns);
    let (store, session, turn_entry) = run
        .build(async {
            let (core, store_factory, turn_entry) = build_embed_core(scenario).await?;
            let session_id = SessionId::fixture(format!("runtime-perf-{}", scenario.name()));
            let session = core
                .create_and_open_session(
                    session_id.clone(),
                    lash::SessionCreation::root(
                        lash::plugins::SessionToolAccess::ambient(),
                        core.session_spec(),
                    ),
                )
                .await
                .with_context(|| format!("open embed session for {}", scenario.name()))?;
            let store = store_factory
                .session_store(&session_id)
                .ok_or_else(|| anyhow::anyhow!("embed session store was not opened"))?;
            Ok((store, session, turn_entry))
        })
        .await?;
    run.seed(async { Ok(()) }).await?;

    for turn_index in 0..chat_turns {
        run.turn(
            turn_index,
            async {
                let cancel = CancellationToken::new();
                let turn_id = TurnId::fixture(format!("runtime-perf-embed-{}", turn_index + 1));
                let turn = runtime_perf_timed(
                    scenario,
                    turn_index,
                    "run_turn",
                    Some(cancel.clone()),
                    turn_entry.run(
                        &session,
                        lash_core::TurnInput::text(benchmark_prompt(scenario, turn_index)),
                        Some(&turn_id),
                        cancel,
                    ),
                )
                .await
                .with_context(|| {
                    format!(
                        "run embed runtime perf scenario {} turn {}",
                        scenario.name(),
                        turn_index + 1
                    )
                })?;
                validate_runtime_perf_turn(scenario, turn_index, &turn)?;
                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        turn_usage: turn.usage,
                        ..TurnTail::default()
                    },
                })
            },
            async { Ok(()) },
        )
        .await?;
    }

    let read_view = run.export(async { Ok(session.read_view()) }).await?;

    Ok(run.finish(RunTail {
        session_nodes: store.graph_node_count(),
        active_path_messages: read_view.messages().len(),
        ..RunTail::default()
    }))
}
pub(crate) fn sum_phase_profiles<'a>(
    profiles: impl IntoIterator<Item = &'a BTreeMap<String, RuntimePerfPhaseRunResult>>,
) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
    let mut totals: BTreeMap<String, RuntimePerfPhaseRunResult> = BTreeMap::new();
    for profile in profiles {
        for (phase, metrics) in profile {
            let entry = totals
                .entry(phase.clone())
                .or_insert_with(|| RuntimePerfPhaseRunResult {
                    samples: 0,
                    duration_ms: 0.0,
                    allocations: zero_allocation_delta(),
                    rss_growth_kb: Some(0),
                });
            entry.samples += metrics.samples;
            entry.duration_ms = round3(entry.duration_ms + metrics.duration_ms);
            entry.allocations = sum_allocation_deltas([&entry.allocations, &metrics.allocations]);
            entry.rss_growth_kb = sum_optional_i64(entry.rss_growth_kb, metrics.rss_growth_kb);
        }
    }
    totals
}

pub(crate) fn mean_phase_profiles<'a>(
    profiles: impl IntoIterator<Item = &'a BTreeMap<String, RuntimePerfPhaseRunResult>>,
) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
    let profiles = profiles.into_iter().collect::<Vec<_>>();
    if profiles.is_empty() {
        return BTreeMap::new();
    }
    let count = profiles.len() as f64;
    let sums = sum_phase_profiles(profiles);
    sums.into_iter()
        .map(|(phase, metrics)| {
            (
                phase,
                RuntimePerfPhaseRunResult {
                    samples: ((metrics.samples as f64) / count).round() as usize,
                    duration_ms: round3(metrics.duration_ms / count),
                    allocations: scale_allocation_delta(&metrics.allocations, count),
                    rss_growth_kb: metrics
                        .rss_growth_kb
                        .map(|value| ((value as f64) / count).round() as i64),
                },
            )
        })
        .collect()
}

pub(crate) fn sum_allocation_deltas<'a>(
    deltas: impl IntoIterator<Item = &'a RuntimePerfAllocationDelta>,
) -> RuntimePerfAllocationDelta {
    let mut total = zero_allocation_delta();
    for delta in deltas {
        total.allocations += delta.allocations;
        total.deallocations += delta.deallocations;
        total.reallocations += delta.reallocations;
        total.bytes_allocated += delta.bytes_allocated;
        total.bytes_deallocated += delta.bytes_deallocated;
        total.bytes_reallocated += delta.bytes_reallocated;
        total.net_live_bytes += delta.net_live_bytes;
    }
    total
}

pub(crate) fn mean_allocation_delta<'a>(
    deltas: impl IntoIterator<Item = &'a RuntimePerfAllocationDelta>,
) -> RuntimePerfAllocationDelta {
    let deltas = deltas.into_iter().collect::<Vec<_>>();
    if deltas.is_empty() {
        return zero_allocation_delta();
    }
    let count = deltas.len() as f64;
    scale_allocation_delta(&sum_allocation_deltas(deltas), count)
}

pub(crate) fn scale_allocation_delta(
    delta: &RuntimePerfAllocationDelta,
    divisor: f64,
) -> RuntimePerfAllocationDelta {
    RuntimePerfAllocationDelta {
        allocations: ((delta.allocations as f64) / divisor).round() as usize,
        deallocations: ((delta.deallocations as f64) / divisor).round() as usize,
        reallocations: ((delta.reallocations as f64) / divisor).round() as usize,
        bytes_allocated: ((delta.bytes_allocated as f64) / divisor).round() as usize,
        bytes_deallocated: ((delta.bytes_deallocated as f64) / divisor).round() as usize,
        bytes_reallocated: ((delta.bytes_reallocated as f64) / divisor).round() as isize,
        net_live_bytes: ((delta.net_live_bytes as f64) / divisor).round() as i64,
    }
}

pub(crate) fn zero_allocation_delta() -> RuntimePerfAllocationDelta {
    RuntimePerfAllocationDelta {
        allocations: 0,
        deallocations: 0,
        reallocations: 0,
        bytes_allocated: 0,
        bytes_deallocated: 0,
        bytes_reallocated: 0,
        net_live_bytes: 0,
    }
}

pub(crate) fn mean_token_usage<'a>(usages: impl IntoIterator<Item = &'a LlmUsage>) -> LlmUsage {
    let usages = usages.into_iter().collect::<Vec<_>>();
    if usages.is_empty() {
        return LlmUsage::default();
    }
    let count = usages.len() as i64;
    LlmUsage {
        input_tokens: usages.iter().map(|usage| usage.input_tokens).sum::<i64>() / count,
        output_tokens: usages.iter().map(|usage| usage.output_tokens).sum::<i64>() / count,
        cache_read_input_tokens: usages
            .iter()
            .map(|usage| usage.cache_read_input_tokens)
            .sum::<i64>()
            / count,
        cache_write_input_tokens: usages
            .iter()
            .map(|usage| usage.cache_write_input_tokens)
            .sum::<i64>()
            / count,
        reasoning_output_tokens: usages
            .iter()
            .map(|usage| usage.reasoning_output_tokens)
            .sum::<i64>()
            / count,
    }
}

pub(crate) fn mean_option_i64(values: impl IntoIterator<Item = Option<i64>>) -> Option<i64> {
    let values = values.into_iter().flatten().collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        Some((values.iter().sum::<i64>() as f64 / values.len() as f64).round() as i64)
    }
}

pub(crate) fn sum_optional_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}
pub(crate) fn phase_name(phase: RuntimeTurnPhase) -> &'static str {
    match phase {
        RuntimeTurnPhase::BeforeTurnHooks => "before_turn_hooks",
        RuntimeTurnPhase::PromptBuild => "prompt_build",
        RuntimeTurnPhase::EffectLoop => "effect_loop",
        RuntimeTurnPhase::PreparedTurn => "prepared_turn",
        RuntimeTurnPhase::CommittedTurn => "committed_turn",
        RuntimeTurnPhase::PostCommitDelivery => "post_commit_delivery",
    }
}

pub(crate) fn allocator_stats() -> Stats {
    crate::GLOBAL_ALLOCATOR.stats()
}

pub(crate) fn alloc_delta(before: Stats, after: Stats) -> RuntimePerfAllocationDelta {
    let diff = after - before;
    RuntimePerfAllocationDelta {
        allocations: diff.allocations,
        deallocations: diff.deallocations,
        reallocations: diff.reallocations,
        bytes_allocated: diff.bytes_allocated,
        bytes_deallocated: diff.bytes_deallocated,
        bytes_reallocated: diff.bytes_reallocated,
        net_live_bytes: diff.bytes_allocated as i64 - diff.bytes_deallocated as i64,
    }
}
