//! Terminal awaits record their observed value once and replay it after prune.

use super::*;
use lash_core::{
    ProcessLifecycle as _, ProcessQuery as _, ProcessRegistrar as _, ProcessRetention as _,
};
use lash_restate_test::protocol::MessageType;
use restate_sdk::context::{ContextPromises as _, SharedWorkflowContext};

const PROBE: &str = "TerminalAwaitProbe";
const BOUND: Duration = Duration::from_secs(60);

#[derive(Serialize, serde::Deserialize)]
struct Input {
    process_id: ProcessId,
    park: bool,
    attach: bool,
}

#[restate_sdk::workflow]
trait TerminalAwaitProbe {
    async fn run(input: Json<Input>) -> HandlerResult<Json<ProcessAwaitOutput>>;
    #[shared]
    async fn release(input: Json<()>) -> HandlerResult<Json<()>>;
}

struct TerminalAwaitProbeImpl {
    registry: Arc<lash_core::testing::ProcessRegistryFaults>,
    attachments: Arc<dyn lash_core::AttachmentReferrers>,
    observed: tokio::sync::mpsc::UnboundedSender<ProcessId>,
    attempts: Arc<AtomicUsize>,
}

impl TerminalAwaitProbe for TerminalAwaitProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Input>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let controller = RestateRuntimeEffectController::new_for_test(ctx);
        let key = test_restate_await_event_key(
            &durable_turn_scope("session", "turn"),
            lash_core::AwaitEventWaitIdentity::Custom {
                key: format!("terminal-await:{}", input.process_id),
            },
        )
        .expect("the terminal wait key");
        let command = if input.attach {
            ProcessCommand::AttachTerminal {
                process_id: input.process_id.clone(),
                key: key.clone(),
            }
        } else {
            ProcessCommand::Await {
                process_id: input.process_id.clone(),
            }
        };
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Process, "terminal-await"),
                    RuntimeEffectCommand::process(command),
                ),
                registry_local_executor(self.registry.clone())
                    .with_process_attachments(Arc::clone(&self.attachments))
                    .with_process_turn_cancellation(
                        lash_core::facade_support::ProcessTurnCancellation::new(
                            tokio_util::sync::CancellationToken::new(),
                            durable_turn_scope("session", "turn"),
                        ),
                    ),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let output = match outcome.into_process().map_err(TerminalError::from_error)? {
            ProcessEffectOutcome::Await { output } => output,
            ProcessEffectOutcome::AttachTerminal if input.attach => {
                let resolution = controller
                    .context()
                    .await_event(
                        &crate::services::DEFAULT_NAMESPACE,
                        crate::durable_wait::RestateDurableWaitAwaitRequest {
                            key: key.clone(),
                            deadline: None,
                        },
                        key.key_id.clone(),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await?;
                let lash_core::Resolution::Ok(value) = resolution else {
                    return Err(TerminalError::new("the terminal wait did not resolve").into());
                };
                Box::new(serde_json::from_value(value).map_err(TerminalError::from_error)?)
            }
            _ => {
                return Err(
                    TerminalError::new("await returned a different command outcome").into(),
                );
            }
        };
        let _ = self.observed.send(input.process_id);
        if input.park {
            controller.context().promise::<String>("release").await?;
        }
        Ok(Json(*output))
    }

    async fn release(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<()>> {
        ctx.resolve_promise("release", "released".to_string());
        Ok(Json(()))
    }
}

struct NoRun;

#[async_trait::async_trait]
impl RestateProcessRunner for NoRun {
    fn executable_generation(
        &self,
        _: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _: &SegmentStarted,
        _: ProcessId,
        _: ProcessRegistration,
        _: ProcessExecutionContext,
        _: ScopedEffectController<'_>,
        _: Option<lash_core::SegmentHandover>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        panic!("the await laws register externally owned children")
    }
}

struct World {
    ingress: RestateIngressClient,
    server: Option<lash_restate_test::RestateTestServer>,
    registry: Arc<lash_core::testing::ProcessRegistryFaults>,
    observed: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ProcessId>>,
    attempts: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    serving: Option<tokio::task::JoinHandle<()>>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "service-gated laws read their injected addresses"
)]
fn required(name: &str) -> String {
    std::env::var(name).expect(name)
}

impl World {
    async fn new(stores: Arc<dyn lash_core::StoreSet>, live: bool) -> Self {
        let server = (!live).then(|| {
            lash_restate_test::RestateTestServer::new(
                lash_restate_test::ServerConfig::default()
                    .with_seed(4307)
                    .always_replay(true),
            )
            .expect("the always-replaying server double")
        });
        let connection = match &server {
            Some(server) => {
                RestateConnection::with_transport(server.ingress_url(), server.transport())
            }
            None => RestateConnection::new(required("RESTATE_INGRESS_URL")),
        };
        let ingress = RestateIngressClient::new(connection.clone());
        let registry = Arc::new(lash_core::testing::ProcessRegistryFaults::new(
            stores.process_registry(),
        ));
        let (observed_tx, observed) = tokio::sync::mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let host = Arc::new(RestateEffectHost::new_for_test(connection));
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                effect_host: &host,
                ingress: ingress.clone(),
                attachments: stores.attachment_referrers(),
                sessions: stores.session_store_factory(),
                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    Arc::new(NoRun),
                    registry.clone(),
                    stores.process_continuations(),
                ),
                session_driver: crate::RestateSessionDriverSlot::new(),
                build_generation: lash_core::engine::BuildGeneration::for_test("terminal-await"),
                namespace: crate::RestateNamespace::default(),
                fleet: crate::object_state::FleetView::default(),
            },
        )
        .bind(
            TerminalAwaitProbeImpl {
                registry: Arc::clone(&registry),
                attachments: stores.attachment_referrers(),
                observed: observed_tx,
                attempts: Arc::clone(&attempts),
            }
            .serve(),
        )
        .build();
        let (shutdown, serving) = if let Some(server) = &server {
            server
                .register(endpoint)
                .await
                .expect("register the endpoint on the double");
            (None, None)
        } else {
            let listener = tokio::net::TcpListener::bind(required("PA_BIND"))
                .await
                .expect("bind the live endpoint");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let serving = tokio::spawn(async move {
                crate::serve_endpoint(
                    listener,
                    endpoint,
                    crate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
                    async {
                        let _ = rx.await;
                    },
                )
                .await;
            });
            crate::RestateAdminClient::new(RestateConnection::new(required("RESTATE_ADMIN_URL")))
                .register_deployment(&required("PA_URL"), true)
                .await
                .expect("register the live endpoint");
            (Some(tx), Some(serving))
        };
        Self {
            ingress,
            server,
            registry,
            observed: tokio::sync::Mutex::new(observed),
            attempts,
            shutdown,
            serving,
        }
    }

    async fn terminal(&self, output: &ProcessAwaitOutput) -> ProcessId {
        let record = self
            .registry
            .register_process(external_registration())
            .await
            .expect("register the child");
        self.registry
            .complete_process(
                &record.id,
                output.clone(),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("record the child terminal");
        self.ingress
            .call_lash_workflow::<_, ()>(
                "LashProcessWorkflow",
                record.id.as_str(),
                "complete_terminal",
                &RestateProcessCompleteRequest {
                    process_id: record.id.clone(),
                    output: output.clone(),
                },
            )
            .await
            .expect("publish the terminal for the slow-path control");
        record.id
    }

    fn assert_fast_journal(&self, key: &str, output: &ProcessAwaitOutput) {
        let server = self
            .server
            .as_ref()
            .expect("journal inspection is on the double");
        let invocation = server
            .invocations()
            .into_iter()
            .find(|v| v.target == format!("{PROBE}/{key}/run"))
            .expect("the await invocation");
        let journal = server.journal(&invocation.id).expect("the await journal");
        assert_eq!(
            journal
                .iter()
                .filter(|e| e.ty == MessageType::RunCommand)
                .count(),
            1,
            "the observed outcome has one recorded step: {journal:?}"
        );
        let recorded = journal
            .iter()
            .filter_map(|entry| entry.run_completion())
            .map(|result| {
                serde_json::from_slice::<serde_json::Value>(
                    &result.expect("the observation succeeds"),
                )
                .expect("the recorded JSON")
            })
            .collect::<Vec<_>>();
        assert_eq!(recorded.len(), 1, "one observation completion");
        assert_eq!(
            recorded[0]["Ok"]["output"],
            serde_json::to_value(output).expect("the terminal JSON"),
            "the journal records the entire terminal outcome"
        );
        assert!(
            !journal
                .iter()
                .any(|e| e.ty == MessageType::OneWayCallCommand),
            "terminal await must not arm an attach: {journal:?}"
        );
        for entry in journal.iter().filter(|e| e.ty == MessageType::CallCommand) {
            assert!(
                entry
                    .payload
                    .windows(b"peek_turn_gate".len())
                    .any(|w| w == b"peek_turn_gate"),
                "only the turn-control observation is called, with no awakeable, resolve, peek or unregister: {entry:?}"
            );
        }
    }

    async fn finish(self) {
        if let Some(tx) = self.shutdown {
            let _ = tx.send(());
        }
        if let Some(serving) = self.serving {
            serving.await.expect("the endpoint stops");
        }
    }

    async fn wait_for_park(&self, key: &str) {
        tokio::time::timeout(BOUND, async {
            loop {
                let parked = match &self.server {
                    Some(server) => server
                        .find_invocation(PROBE, key, "run", "suspended")
                        .is_some(),
                    None => crate::RestateAdminClient::new(RestateConnection::new(required(
                        "RESTATE_ADMIN_URL",
                    )))
                    .workflow_invocation_status(PROBE, key, "run")
                    .await
                    .expect("read the live waiter")
                    .is_some_and(|invocation| invocation.status.as_str() == "suspended"),
                };
                if parked {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the waiter suspends before prune");
    }
}

async fn sqlite() -> Arc<dyn lash_core::StoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    )
}

async fn postgres() -> (tempfile::TempDir, Arc<dyn lash_core::StoreSet>) {
    let storage =
        lash_postgres_store::PostgresStorage::connect(&required("LASH_POSTGRES_DATABASE_URL"))
            .await
            .expect("PostgreSQL stores");
    let directory = tempfile::tempdir().expect("attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            directory.path(),
        )),
    ));
    (directory, stores)
}

async fn terminal_shape(attach: bool) {
    let world = World::new(sqlite().await, false).await;
    let terminal = process_success(serde_json::json!({ "terminal": "recorded" }));
    let process_id = world.terminal(&terminal).await;
    let reads = world.registry.process_point_reads();
    let output = tokio::time::timeout(
        BOUND,
        world.ingress.call_workflow_json::<_, ProcessAwaitOutput>(
            PROBE,
            "shape",
            "run",
            &Input {
                process_id,
                park: false,
                attach,
            },
        ),
    )
    .await
    .expect("the await completes")
    .expect("the terminal is returned");
    assert_eq!(output, terminal);
    assert_eq!(
        world.registry.process_point_reads() - reads,
        1,
        "one live terminal observation"
    );
    world.assert_fast_journal("shape", &terminal);
    world.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_await_journals_the_observed_output_once_without_wait_calls() {
    terminal_shape(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_attachment_journals_the_observed_output_once_without_wait_calls() {
    terminal_shape(true).await;
}

async fn replay_after_prune(stores: Arc<dyn lash_core::StoreSet>, live: bool) {
    let world = World::new(stores, live).await;
    for attach in [false, true] {
        for (label, terminal) in [
            (
                "success",
                process_success(serde_json::json!({ "preserved": [1, 2, 3] })),
            ),
            (
                "failure",
                process_failure(
                    lash_core::ToolFailureClass::Execution,
                    "recorded_failure",
                    "the recorded failure",
                    Some(serde_json::json!({"detail": 7})),
                ),
            ),
        ] {
            let key = format!("prune-{label}-{}", uuid::Uuid::new_v4());
            let process_id = world.terminal(&terminal).await;
            let live_reads = world.registry.process_point_reads();
            let attempts = world.attempts.load(Ordering::SeqCst);
            let ingress = world.ingress.clone();
            let input = Input {
                process_id: process_id.clone(),
                park: true,
                attach,
            };
            let awaited_key = key.clone();
            let waiter = tokio::spawn(async move {
                ingress
                    .call_workflow_json::<_, ProcessAwaitOutput>(PROBE, &awaited_key, "run", &input)
                    .await
            });
            tokio::time::timeout(BOUND, async {
                let mut observed = world.observed.lock().await;
                while observed.recv().await.expect("the endpoint stays live") != process_id {}
            })
            .await
            .expect("the first await observes its terminal");
            world.wait_for_park(&key).await;
            if !live {
                world.assert_fast_journal(&key, &terminal);
            }
            let reads = world.registry.process_point_reads();
            let ended = world
                .registry
                .get_process(&process_id)
                .await
                .expect("the terminal is retained")
                .expect("the terminal record");
            world
                .registry
                .prune_terminal_processes(
                    ended.updated_at_ms.saturating_add(1),
                    None,
                    lash_core::ProjectionWatermark::NoProjector,
                )
                .await
                .expect("prune the terminal child");
            assert!(
                matches!(
                    world.registry.get_process(&process_id).await,
                    Err(PluginError::ProcessNoLongerRetained { .. })
                ),
                "the child was actually pruned"
            );
            world
                .registry
                .set_process_read_error(Some(PluginError::Session(
                    "replay must not read the registry".to_string(),
                )));
            let replay_reads = world.registry.process_point_reads();
            let attempts_at_prune = world.attempts.load(Ordering::SeqCst);
            world
                .ingress
                .call_workflow_json::<_, ()>(PROBE, &key, "release", &())
                .await
                .expect("release the durable promise");
            let output = tokio::time::timeout(BOUND, waiter)
                .await
                .expect("the replay completes")
                .expect("the waiter task")
                .expect("replay returns an outcome");
            assert_eq!(
                output, terminal,
                "replay preserves the entire terminal output after pruning"
            );
            assert_eq!(
                world.registry.process_point_reads(),
                replay_reads,
                "replay never re-reads the registry"
            );
            assert!(
                world.attempts.load(Ordering::SeqCst) - attempts >= 2,
                "the handler actually replayed"
            );
            assert!(
                world.attempts.load(Ordering::SeqCst) > attempts_at_prune,
                "the handler replays after pruning"
            );
            assert_eq!(reads - live_reads, 1, "one live terminal observation");
            world.registry.set_process_read_error(None);
        }
    }
    world.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_await_replays_after_pruning_on_sqlite() {
    replay_after_prune(sqlite().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL through the process-await service gate"]
async fn terminal_await_replays_after_pruning_on_postgres() {
    let (_directory, stores) = postgres().await;
    replay_after_prune(stores, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live Restate through the process-await service gate"]
async fn live_terminal_await_replays_after_pruning_on_sqlite() {
    replay_after_prune(sqlite().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live Restate and PostgreSQL through the process-await service gate"]
async fn live_terminal_await_replays_after_pruning_on_postgres() {
    let (_directory, stores) = postgres().await;
    replay_after_prune(stores, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_terminal_and_revoked_session_keep_the_durable_wait_path() {
    for attach in [false, true] {
        control_paths(attach).await;
    }
}

async fn control_paths(attach: bool) {
    let world = World::new(sqlite().await, false).await;
    let cancelled = process_cancellation("the child was cancelled".to_string(), None);
    let process_id = world.terminal(&cancelled).await;
    let output = tokio::time::timeout(
        BOUND,
        world.ingress.call_workflow_json::<_, ProcessAwaitOutput>(
            PROBE,
            "cancelled",
            "run",
            &Input {
                process_id,
                park: false,
                attach,
            },
        ),
    )
    .await
    .expect("the cancelled await completes")
    .expect("the cancelled terminal is returned");
    assert_eq!(output, cancelled);
    let server = world.server.as_ref().expect("the double");
    let invocation = server
        .find_invocation(PROBE, "cancelled", "run", "completed")
        .expect("the cancelled await");
    assert!(
        server
            .journal(&invocation.id)
            .expect("the journal")
            .iter()
            .any(|e| e.ty == MessageType::OneWayCallCommand),
        "a cancelled child still arms the attach"
    );

    let process_id = world
        .terminal(&process_success(serde_json::json!("completed")))
        .await;
    world
        .ingress
        .call_lash_object::<_, ()>("LashDurableWaitIndex", "session", "revoke_all", &())
        .await
        .expect("revoke the waiting session");
    let refused = tokio::time::timeout(
        BOUND,
        world.ingress.call_workflow_json::<_, ProcessAwaitOutput>(
            PROBE,
            "revoked",
            "run",
            &Input {
                process_id,
                park: false,
                attach,
            },
        ),
    )
    .await
    .expect("the revoked await ends");
    assert!(
        refused.is_err(),
        "a revoked session cannot receive a terminal"
    );
    let invocation = server
        .find_invocation(PROBE, "revoked", "run", "completed")
        .expect("the revoked await");
    assert!(
        server
            .journal(&invocation.id)
            .expect("the journal")
            .iter()
            .any(|e| e.ty == MessageType::OneWayCallCommand),
        "revocation keeps the ordinary wait's refusal path"
    );
    world.finish().await;
}
