use super::*;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

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
    let value = crate::perf_support::async_operations::observe(name, future).await?;
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

const CHECKPOINT_STATE_BINDINGS: usize = 300;
const CHECKPOINT_STATE_BODY_BYTES: usize = 3 * 1024 + 512;

/// A checkpoint fixture edited by a plugin task on the served session actor.
/// Capture remains outside the task so the curve measures each structural step.
pub(super) struct CheckpointBindingFixture {
    fixture: Arc<tokio::sync::Mutex<lash_protocol_rlm::CodeModeCheckpointPerfFixture>>,
    backend: lash::Backend,
    workers: lash_vm_client::WorkerPool,
    spools: Arc<lash_vm_broker::kernel::pages::SpoolRoot>,
    factory: Arc<dyn lash_core::plugin::PluginFactory>,
    kernel: Arc<dyn lash_core::plugin::PluginFactory>,
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

fn checkpoint_setup_error(error: impl std::fmt::Display) -> CheckpointAssignmentError {
    CheckpointAssignmentError::Execution {
        reason: lash_core::ExecCodeFailureReason::Session,
        message: error.to_string(),
    }
}

/// The checkpoint cells have no model round. Their host explicitly records
/// the ordinary dialect services they are lowered against instead.
fn checkpoint_packages(
    backend: &lash::Backend,
    services: &lash_vm_runtime::dialect_services::Services,
) -> Result<lash_core::plugin::packages::RecordedPackages, CheckpointAssignmentError> {
    use lash_core::plugin::packages::{PackageProvider, RecordedPackages};
    use {
        lash_kernel_value::PublicationContext, lash_kernel_value::PublicationDomain,
        lash_kernel_value::PublicationScope, lash_kernel_value::PublisherInstance,
        lash_kernel_value::names::Segment, lash_vocabulary::AcceptedAdvertisement,
    };
    let consumer = PublicationContext {
        domain: PublicationDomain(backend.binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("checkpoint-benchmark".to_owned()),
        root: Segment::new("checkpoint").map_err(checkpoint_setup_error)?,
    };
    let mut packages = RecordedPackages {
        consumer: Some(consumer.clone()),
        ..Default::default()
    };
    let config = lash_core::OwnerConfigView::default();
    for root in services.roots(&config) {
        for contribution in services
            .advertise(
                &config,
                &PublicationContext {
                    root,
                    ..consumer.clone()
                },
            )
            .map_err(checkpoint_setup_error)?
        {
            packages.routes.extend(contribution.routes);
            packages
                .snapshot
                .advertisements
                .push(AcceptedAdvertisement {
                    publication: contribution.publication,
                    advertisement: contribution.advertisement,
                });
        }
    }
    Ok(packages)
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
    const ERROR_VERSION: lash_kernel_value::providers::FormatVersion =
        lash_kernel_value::providers::FormatVersion::ONE;
    fn error_class(_: &CheckpointAssignmentError) -> lash_core::plugin::PluginFailureClass {
        lash_core::plugin::PluginFailureClass::Terminal
    }
}
impl lash_core::plugin::PluginTask for AssignCheckpointBinding {}

impl CheckpointBindingFixture {
    /// A fixture whose cells run on `backend`, the store its captures
    /// publish to.
    pub(super) async fn new(
        dialect: lash_protocol_rlm::CellDialect,
        bindings: usize,
        bytes: usize,
        backend: lash::Backend,
    ) -> anyhow::Result<Self> {
        let mut worker_config = lash_vm_client::service::Service::default().config().clone();
        worker_config
            .entry
            .args
            .push("--count-heap-work".to_owned());
        let service = lash_vm_client::service::Service::new(worker_config);
        // Each edit releases its node. Its successor must advertise the same
        // machine library as the cells whose phases it will reclaim.
        let kernel: Arc<dyn lash_core::plugin::PluginFactory> =
            Arc::new(lash_vm_runtime::KernelProcessPluginFactory::new(
                service.clone(),
                dialect.name(),
                service.config().run_bounds,
            ));
        let workers = service.pool()?;
        let spools = service.spools()?;
        let fixture = Arc::new(tokio::sync::Mutex::new(
            lash_protocol_rlm::CodeModeCheckpointPerfFixture::new(
                &dialect, service, bindings, bytes,
            )
            .await?,
        ));
        let services = lash_vm_runtime::dialect_services::Services::new(
            lash_vm_runtime::dialect_services::Dialect::TypeScript,
            lash_core::testing::test_tools_provider(),
            std::time::Duration::from_secs(30),
            Arc::new(|| 0),
        )?;
        let task_fixture = Arc::clone(&fixture);
        let task_backend = backend.clone();
        let spec = lash_core::plugin::PluginSpec::new()
            .with_plugin_task_value::<AssignCheckpointBinding, _, _>(move |ctx, (index, turn)| {
                let fixture = Arc::clone(&task_fixture);
                let backend = task_backend.clone();
                let services = Arc::clone(&services);
                async move {
                    let session_id = ctx
                        .session_id
                        .clone()
                        .ok_or(CheckpointAssignmentError::SessionRequired)?;
                    let invocation = lash_core::runtime::causal::turn_effect_invocation(
                        ctx.execution_scope(),
                        &session_id,
                        &lash_vocabulary::identity::TurnId::fixture(format!("checkpoint-{turn}")),
                        turn,
                        0,
                        lash_core::sansio::EffectId(0),
                        lash_core::RuntimeEffectKind::ExecCode,
                    );
                    let packages = checkpoint_packages(&backend, services.as_ref())?;
                    let mut built = lash_core::testing::TestExecutionContextBuilder::new(
                        lash_core::facade_support::lent_test_execution_ports(
                            &backend,
                            lash_core::facade_support::plugin_task_actor(&ctx).clone(),
                        ),
                    )
                    .session_id(session_id.clone())
                    .provider_registration(services.registration())
                    .runtime_parent_invocation(invocation.into_runtime_invocation())
                    .build();
                    let dispatch = Arc::make_mut(&mut built.dispatch);
                    dispatch.routes = dispatch.routes.clone().with_packages(packages.clone());
                    let execution = built.into_runtime();
                    let claim = execution
                        .execution_claim()
                        .map_err(checkpoint_setup_error)?;
                    let store = backend.package_store();
                    for accepted in &packages.snapshot.advertisements {
                        if let lash_kernel_value::PackageAdvertisement::Enumerated(package) =
                            &accepted.advertisement
                        {
                            store
                                .put_package(
                                    &claim,
                                    &lash_core_store::packages::PackageRecord {
                                        id: package.identity(),
                                        package: package.clone(),
                                    },
                                )
                                .await
                                .map_err(checkpoint_setup_error)?;
                        }
                        store
                            .publish(&claim, &accepted.publication)
                            .await
                            .map_err(checkpoint_setup_error)?;
                    }
                    Box::pin(fixture.lock().await.assign_one(index, turn, execution))
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
            Arc::clone(&kernel),
        )?;
        let session_id = SessionId::fixture(format!("checkpoint-worker-{}", uuid::Uuid::new_v4()));
        let creation = lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ));
        core.session(session_id.clone()).create(creation).await?;
        core.shutdown().await?;
        Ok(Self {
            fixture,
            workers,
            spools,
            backend,
            factory,
            kernel,
            session_id,
        })
    }

    pub(super) fn pages_sealed(&self) -> u64 {
        self.workers
            .measurements()
            .counters
            .worker_work
            .pages_sealed
    }

    pub(super) fn work_collector(&self) -> anyhow::Result<lash_core::perf_witness::Collector> {
        let spools = Arc::clone(&self.spools);
        Ok(lash_core::perf_witness::Collector::install()?
            .with_spool_bytes(move || spools.spooled_bytes()))
    }

    pub(super) async fn assign(&self, index: usize, turn: usize) -> anyhow::Result<()> {
        // Join the served node before capture so every edit's page counters
        // are complete and its actor claim has been released.
        let core = super::super::harness::checkpoint_benchmark_core(
            self.backend.clone(),
            Arc::clone(&self.factory),
            Arc::clone(&self.kernel),
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
    ) -> Result<lash_core::plugin::ExecutionGraphCapture, lash_core::SessionError> {
        self.fixture.lock().await.capture().await
    }

    /// Commits `commit` through `publisher`'s session actor, as the turn's
    /// head commit does: the pages it published.
    pub(super) async fn commit(
        &self,
        publisher: &mut lash_protocol_rlm::CodeModeCheckpointPerfPublisher,
        commit: &RuntimeCommit,
    ) -> Result<Vec<lash_core_store::checkpoint_graph::EncodedNode>, lash_core::SessionError> {
        self.fixture.lock().await.commit(publisher, commit).await
    }

    /// Reads, from the next cell on, the head `state` reloaded after a
    /// commit through `publisher`.
    pub(super) async fn read_head(
        &self,
        publisher: &lash_protocol_rlm::CodeModeCheckpointPerfPublisher,
        state: &RuntimeSessionState,
    ) -> anyhow::Result<()> {
        self.fixture.lock().await.read_head(
            publisher,
            &state.session_id,
            state.head_revision,
            head_checkpoint(state)?,
        )?;
        Ok(())
    }
}

pub(super) async fn run_once_checkpoint_state_hot_paths(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::CheckpointStateHotPaths;
    let mut run = RunRecorder::start(scenario, chat_turns);

    let (fixture, store, mut publisher, mut runtime_state) = run
        .build(async {
            let runtime_state = RuntimeSessionState {
                session_id: SessionId::from("runtime-perf-checkpoint-state"),
                ..RuntimeSessionState::fixture(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ))
            };
            let (store, backend) = memory_perf_store(&runtime_state.session_id).await?;
            // Cells run on the production effect controller over the memory
            // store set the checkpoint commits to; their captured state is
            // what is measured.
            let fixture = CheckpointBindingFixture::new(
                lash_protocol_rlm::CellDialect::typescript(),
                CHECKPOINT_STATE_BINDINGS,
                CHECKPOINT_STATE_BODY_BYTES,
                backend.clone(),
            )
            .await?;
            store
                .admit_session(&runtime_perf_session_create_request(
                    &runtime_state.session_id,
                ))
                .await?;
            let publisher = lash_protocol_rlm::CodeModeCheckpointPerfPublisher::open(
                &backend,
                &runtime_state.session_id,
            )
            .await?;
            Ok((fixture, store, publisher, runtime_state))
        })
        .await?;

    let (initial_pages, initial_spool_bytes, initial_capture_phase) = run
        .seed(async {
            let collector = fixture.work_collector()?;
            let (initial_snapshot, initial_capture_phase) =
                measure_runtime_perf_async_phase("checkpoint_state.initial_capture", async {
                    fixture.capture().await.map_err(anyhow::Error::from)
                })
                .await?;
            lash_core::testing::stage_execution_graph(&mut runtime_state, initial_snapshot);
            let initial_commit = RuntimeCommit::persisted_state_for_test_with_budget(
                &runtime_state,
                lash_core::CommitBudget::bounded(8 * 1024 * 1024, 2_048),
            );
            let published = fixture.commit(&mut publisher, &initial_commit).await?;
            let initial_pages = staged_pages(&published)?;
            let initial_spool_bytes = collector.snapshot().spool_bytes;
            drop(collector);
            anyhow::ensure!(
                initial_pages > 0 && initial_spool_bytes > 0,
                "checkpoint-state seed must seal heap pages"
            );
            runtime_state = reloaded(store.clone(), &runtime_state.session_id).await?;
            fixture.read_head(&publisher, &runtime_state).await?;
            Ok((initial_pages, initial_spool_bytes, initial_capture_phase))
        })
        .await?;

    let mut last_checkpoint_bytes = 0_u64;
    let mut metric_samples = BTreeMap::<String, Vec<f64>>::new();
    let mut last_hydrated_bytes = 0_u64;
    for turn_index in 0..chat_turns {
        let collector = fixture.work_collector()?;
        let before_pages = fixture.pages_sealed();
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
        let pages_sealed = fixture.pages_sealed() - before_pages;
        let spool_bytes = collector.snapshot().spool_bytes;
        drop(collector);
        // One binding changes. Its pages and index paths must be smaller
        // than sealing the entire retained population of 300 bindings.
        anyhow::ensure!(pages_sealed > 0 && pages_sealed < initial_pages,
            "one binding sealed {pages_sealed} pages versus {initial_pages} seed pages");
        anyhow::ensure!(spool_bytes > 0 && spool_bytes < initial_spool_bytes,
            "one binding spooled {spool_bytes} bytes versus {initial_spool_bytes} seed bytes");
        for (name, value) in [("changed_bindings", 1), ("pages_sealed", pages_sealed), ("spool_bytes", spool_bytes)] {
            metric_samples.entry(name.to_owned()).or_default().push(value as f64);
        }

        lash_core::testing::stage_execution_graph(&mut runtime_state, snapshot);
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

        let (_, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.component_commit", async {
                fixture
                    .commit(&mut publisher, &commit)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        runtime_state = reloaded(store.clone(), &runtime_state.session_id).await?;
        fixture.read_head(&publisher, &runtime_state).await?;
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
        last_hydrated_bytes = loaded_execution_state.descriptor.len() as u64;

        let (_, phase) = measure_runtime_perf_async_phase("checkpoint_state.execution_restore", async {
            lash_protocol_rlm::CodeModeCheckpointPerfFixture::restore(&lash_protocol_rlm::CellDialect::typescript(), &loaded_execution_state).await
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

    run.export(async { publisher.close().await.map_err(anyhow::Error::from) })
        .await?;
    let mut extra_counters = BTreeMap::new();
    extra_counters.insert(
        "execution_state_bindings".to_string(),
        CHECKPOINT_STATE_BINDINGS as u64,
    );
    extra_counters.insert("initial_pages".to_string(), initial_pages);
    extra_counters.insert("initial_spool_bytes".to_string(), initial_spool_bytes);
    extra_counters.insert("checkpoint_bytes".to_string(), last_checkpoint_bytes);
    extra_counters.insert(
        "hydrated_execution_state_bytes".to_string(),
        last_hydrated_bytes,
    );

    Ok(run.finish(RunTail {
        extra_counters,
        metric_samples,
        ..RunTail::default()
    }))
}

/// Fresh semantic pages published by the seed. Bindings share packed
/// pages; there is no one-leaf-per-binding representation.
pub(super) fn staged_pages(
    published: &[lash_core_store::checkpoint_graph::EncodedNode],
) -> anyhow::Result<u64> {
    let mut pages = 0;
    for node in published {
        if lash_core_store::checkpoint_graph::NodeKind::SEMANTIC.contains(&node.envelope()?.kind.0)
        {
            pages += 1;
        }
    }
    Ok(pages)
}

/// The root of `state`'s head revision: its checkpoint.
pub(super) fn head_checkpoint(
    state: &RuntimeSessionState,
) -> anyhow::Result<lash_core_store::checkpoint_graph::NodeId> {
    state
        .checkpoint_ref
        .as_ref()
        .and_then(|checkpoint| {
            lash_core_store::checkpoint_graph::NodeId::from_hex(checkpoint.as_str())
        })
        .ok_or_else(|| anyhow::anyhow!("the session's head names no checkpoint"))
}

/// `session_id`'s state as `store` holds it after a head commit.
pub(super) async fn reloaded(
    store: Arc<dyn lash_core::RuntimeStore>,
    session_id: &SessionId,
) -> anyhow::Result<RuntimeSessionState> {
    lash::persistence::load_session_window_state(
        &lash::persistence::SessionStore::new(store, session_id.clone())?,
        lash::persistence::WindowSelector::Current,
    )
    .await?
    .map(|loaded| loaded.state)
    .ok_or_else(|| anyhow::anyhow!("session {session_id} was not loadable"))
}

fn execution_state_from_checkpoint(
    checkpoint: &lash_core::HydratedSessionCheckpoint,
) -> anyhow::Result<lash_core::plugin::HydratedExecutionGraph> {
    let execution = checkpoint
        .execution
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted its execution state"))?;
    Ok(lash_core::plugin::HydratedExecutionGraph {
        root: execution.root,
        descriptor: execution
            .descriptor
            .clone()
            .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted its descriptor"))?,
    })
}

pub(crate) async fn prepare_embed(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<EmbedRun> {
    let mut run = RunRecorder::start(scenario, chat_turns);
    let (core, store, session, turn_entry) = run
        .build(async {
            let (core, store_factory, turn_entry) = build_embed_core(scenario).await?;
            let session_id = SessionId::fixture(format!("runtime-perf-{}", scenario.name()));
            let session = core
                .create_and_open_session(
                    session_id.clone(),
                    lash::SessionCreation::root(core.session_spec()),
                )
                .await
                .with_context(|| format!("open embed session for {}", scenario.name()))?;
            let store = store_factory
                .session_store(&session_id)
                .ok_or_else(|| anyhow::anyhow!("embed session store was not opened"))?;
            // The core owns the serving node. Its session handle alone cannot
            // keep an engine alive to answer sends. Hold it through export.
            Ok((core, store, session, turn_entry))
        })
        .await?;
    run.seed(async { Ok(()) }).await?;

    Ok(EmbedRun {
        run,
        scenario,
        chat_turns,
        core,
        store,
        session,
        turn_entry,
    })
}

/// Owns the serving core through every send and the final export.
pub(crate) struct EmbedRun {
    run: RunRecorder,
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    core: super::super::harness::BenchmarkCore,
    store: Arc<super::super::store::RuntimePerfStore>,
    session: lash::LashSession,
    turn_entry: TurnEntry,
}

impl EmbedRun {
    pub(crate) async fn finish(self) -> anyhow::Result<RuntimePerfRunResult> {
        let Self {
            mut run,
            scenario,
            chat_turns,
            core: _core,
            store,
            session,
            turn_entry,
        } = self;
        for turn_index in 0..chat_turns {
            let turn = run
                .turn(
                    turn_index,
                    async {
                        let cancel = CancellationToken::new();
                        let turn_id =
                            TurnId::fixture(format!("runtime-perf-embed-{}", turn_index + 1));
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
                        Ok(TurnRun {
                            tail: TurnTail {
                                turn_usage: turn.usage.clone(),
                                ..TurnTail::default()
                            },
                            value: turn,
                        })
                    },
                    async { Ok(()) },
                )
                .await?;
            run.validation(async {
                validate_runtime_perf_turn(
                    scenario,
                    turn_index,
                    &turn.outcome,
                    &session
                        .durable()
                        .read()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("missing committed head"))?
                        .to_snapshot(),
                )?;
                Ok(())
            })
            .await?;
        }

        let read_view = run
            .export(async {
                session
                    .durable()
                    .read()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("missing committed head"))
            })
            .await?;

        Ok(run.finish(RunTail {
            session_nodes: store.graph_node_count(),
            active_path_messages: read_view.messages().len(),
            ..RunTail::default()
        }))
    }
}

pub(crate) async fn run_once_embed(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    prepare_embed(scenario, chat_turns).await?.finish().await
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
                    rss_growth_kb: None,
                });
            entry.samples += metrics.samples;
            entry.duration_ms = round3(entry.duration_ms + metrics.duration_ms);
            entry.allocations = sum_allocation_deltas([&entry.allocations, &metrics.allocations]);
            entry.rss_growth_kb = sum_optional_i64(entry.rss_growth_kb, metrics.rss_growth_kb);
        }
    }
    totals
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

pub(super) fn checkpoint_message(id: String, role: MessageRole, content: String) -> Message {
    Message {
        id: id.clone(),
        role,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), content, None)]),
        origin: None,
        reply_marker: None,
    }
}
