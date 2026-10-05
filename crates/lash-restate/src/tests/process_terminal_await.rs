//! Direct terminal awaits record their observed value once without arming a source.

use super::*;
use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _};
use lash_restate_test::protocol::MessageType;

const PROBE: &str = "TerminalAwaitProbe";
const BOUND: Duration = Duration::from_secs(60);

#[derive(Serialize, serde::Deserialize)]
struct Input {
    process_id: ProcessId,
}

#[restate_sdk::workflow]
trait TerminalAwaitProbe {
    async fn run(input: Json<Input>) -> HandlerResult<Json<ProcessAwaitOutput>>;
}

struct TerminalAwaitProbeImpl {
    registry: Arc<lash_core::testing::ProcessRegistryFaults>,
    attachments: Arc<dyn lash_core::AttachmentReferrers>,
}

impl TerminalAwaitProbe for TerminalAwaitProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Input>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        let controller = RestateRuntimeEffectController::new_for_test(ctx);
        let command = ProcessCommand::Await {
            process_id: input.process_id,
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
            _ => {
                return Err(
                    TerminalError::new("await returned a different command outcome").into(),
                );
            }
        };
        Ok(Json(*output))
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
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                tool_realizer: Arc::new(crate::tests::NoIntentsRealizer),
                effect_host: &host,

                admin: crate::RestateAdminClient::new(match &server {
                    Some(_) => connection.clone(),
                    None => RestateConnection::new(required("RESTATE_ADMIN_URL")),
                }),
                materials: stores.tool_material_store(),
                attachments: stores.attachment_referrers(),

                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    Arc::new(NoRun),
                    registry.clone(),
                    stores.process_continuations(),
                ),
                session_shifts: crate::RestateSessionShiftsSlot::new(),
                build_generation: lash_core::engine::BuildGeneration::for_test("terminal-await"),
                namespace: crate::RestateNamespace::default(),
                fleet: crate::object_state::FleetView::default(),
            },
        )
        .bind(
            TerminalAwaitProbeImpl {
                registry: Arc::clone(&registry),
                attachments: stores.attachment_referrers(),
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
}

async fn sqlite() -> Arc<dyn lash_core::StoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    )
}

async fn terminal_shape() {
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
            &Input { process_id },
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
    terminal_shape().await;
}
