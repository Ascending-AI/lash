#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]
#![cfg(feature = "restate")]

//! The Restate deployment's live end-to-end test. The service binds no turn
//! workflow of its own: a chat message goes through the session's `send()`,
//! and lash's `LashSession`/`LashTurn` services, bound by
//! `RestateEngine::endpoint_builder`, drive the turn.

#[cfg(test)]
mod restate_tests {
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;

    use std::sync::Mutex;

    use axum::Json;
    use axum::extract::{Path as AxumPath, State};
    use lash::TurnId;
    use serde_json::json;

    use crate::board::BoardState;
    use crate::db::AppDb;
    use crate::demo_plugin::{DemoPlugin, DemoPluginConfig};
    use crate::effect_groups::{
        AgentServiceEffectGroupExecutors, AgentServiceEffectGroupWorkflow,
        AgentServiceEffectGroupWorkflowImpl, EffectGroupRunReport, EffectGroupRunTerminal,
        get_effect_group, run_effect_group,
    };
    use crate::routes::{SendMessageRequest, send_message, settings};
    use crate::state::{AgentServiceDurability, AppStateData};
    use axum::Router;
    use axum::routing::{get, post};
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;
    use lash::runtime::{AwaitEventResolver, ExecutionScope};
    use lash::{
        AwaitEventWaitIdentity, CancellationToken, LashCore, PluginBinding, Resolution,
        ResolveOutcome,
    };
    use lash_restate::RestateEffectHost;

    const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

    #[test]
    #[ignore = "requires a running Restate server; set RESTATE_INGRESS_URL and run with --ignored"]
    fn live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end_durable_await_cancel() {
        std::thread::Builder::new()
            .name("agent-service-restate-e2e".to_string())
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .thread_stack_size(STACK_BUDGET_BYTES)
                    .enable_all()
                    .build()
                    .expect("build live Restate E2E runtime")
                    .block_on(run_live_restate_e2e())
            })
            .expect("spawn live Restate E2E thread")
            .join()
            .expect("live Restate E2E thread");
    }

    async fn run_live_restate_e2e() {
        // This test is `#[ignore]`d, so it only runs when the recipe asked for
        // it. Skipping on an absent ingress URL would report the live E2E green
        // having exercised nothing; fail instead, as the workbench recovery
        // scenarios do.
        let ingress_url = std::env::var("RESTATE_INGRESS_URL")
            .expect("RESTATE_INGRESS_URL must be set by the agent-service Restate E2E recipe");
        let admin_url = std::env::var("RESTATE_ADMIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9070".to_string());
        let bind_addr: SocketAddr = std::env::var("AGENT_SERVICE_E2E_ENDPOINT_BIND")
            .unwrap_or_else(|_| "127.0.0.1:19080".to_string())
            .parse()
            .expect("valid AGENT_SERVICE_E2E_ENDPOINT_BIND");
        let endpoint_url = std::env::var("AGENT_SERVICE_E2E_ENDPOINT_URL")
            .unwrap_or_else(|_| format!("http://{bind_addr}"));

        let temp = tempfile::tempdir().expect("tempdir");
        let http = reqwest::Client::new();
        let harness = live_restate_test_state(temp.path(), ingress_url.clone()).await;
        let state = harness.state.clone();
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .expect("bind Restate endpoint");
        let local_addr = listener.local_addr().expect("endpoint local addr");
        let local_probe_addr = if local_addr.ip().is_unspecified() {
            SocketAddr::from(([127, 0, 0, 1], local_addr.port()))
        } else {
            local_addr
        };
        let endpoint = harness
            .backend
            .endpoint_builder(harness.process_worker.clone())
            .bind(AgentServiceEffectGroupWorkflowImpl.serve())
            .build();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            restate_sdk::http_server::HttpServer::new(endpoint)
                .serve_with_cancel(listener, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind agent-service E2E HTTP surface");
        let app_addr = app_listener
            .local_addr()
            .expect("agent-service E2E HTTP address");
        let app = Router::new()
            .route("/api/settings", get(settings))
            .route("/api/effect-groups", post(run_effect_group))
            .route("/api/effect-groups/{run_id}", get(get_effect_group))
            .with_state(state.clone());
        let (app_shutdown_tx, app_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let app_server = tokio::spawn(async move {
            axum::serve(app_listener, app)
                .with_graceful_shutdown(async {
                    let _ = app_shutdown_rx.await;
                })
                .await
                .expect("serve agent-service E2E HTTP surface");
        });
        let _ = harness
            .backend
            .process_deployment()
            .process_work()
            .admit_pending_processes("agent_service_e2e_startup")
            .await
            .expect("drive startup recovery");

        wait_for_endpoint_socket(local_probe_addr).await;
        register_restate_deployment(&admin_url, &endpoint_url).await;

        let chat = state
            .with_db(|db| db.create_chat("Restate E2E", "mock-model", None))
            .await
            .expect("create chat");
        // The chat route itself: the session's `send()` takes the message and
        // `LashSession` drives the turn in a Restate handler.
        let request: SendMessageRequest = serde_json::from_value(json!({
            "text": "play the next move",
            "board": BoardState {
                cells: vec![None; 9],
                turn: "O".to_string(),
            },
            "model": null,
            "model_variant": null,
        }))
        .expect("send-message request");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            Box::pin(send_message(
                State(state.clone()),
                AxumPath(chat.id.clone()),
                Json(request),
            )),
        )
        .await
        .expect("the chat route answers")
        .expect("send message through the chat route");
        let turn_id = TurnId::from(
            response
                .headers()
                .get("x-lash-turn-id")
                .expect("turn id response header")
                .to_str()
                .expect("turn id header text"),
        );
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("the chat stream ends")
        .expect("chat stream body");
        let stream = String::from_utf8_lossy(&body).into_owned();
        let messages = state
            .with_db({
                let chat_id = chat.id.clone();
                move |db| db.list_messages(&chat_id)
            })
            .await
            .expect("list messages");
        assert!(
            messages.iter().any(|message| message.role() == "assistant"
                && message.text().contains("done via Restate E2E")),
            "assistant message was not persisted through the Restate-driven turn {turn_id}; messages={messages:?}; stream={stream}"
        );

        let group_run_id = format!("agent-service-e2e-{}", uuid::Uuid::new_v4());
        let group_url = format!("http://{app_addr}/api/effect-groups/{group_run_id}");
        let settings_response = http
            .get(format!("http://{app_addr}/api/settings"))
            .send()
            .await
            .expect("preflight agent-service settings surface");
        assert!(
            settings_response.status().is_success(),
            "agent-service settings preflight failed: {}",
            settings_response.status()
        );
        let missing_response = http
            .get(&group_url)
            .send()
            .await
            .expect("preflight absent effect group through agent-service HTTP surface");
        let missing_status = missing_response.status();
        let missing_body = missing_response
            .text()
            .await
            .expect("read absent effect-group response");
        assert!(!missing_status.is_success());
        assert!(
            missing_body.contains("does not exist"),
            "absent group response did not name the missing run: {missing_status} {missing_body}"
        );

        let group_response = http
            .post(format!("http://{app_addr}/api/effect-groups"))
            .json(&json!({ "run_id": group_run_id.clone() }))
            .send()
            .await
            .expect("run effect group through agent-service HTTP surface");
        let group_status = group_response.status();
        let group_body = group_response
            .text()
            .await
            .expect("read effect-group HTTP response");
        assert!(
            group_status.is_success(),
            "agent-service effect-group request failed: {group_status} {group_body}"
        );
        let group_report: EffectGroupRunReport =
            serde_json::from_str(&group_body).expect("decode effect-group report");
        assert_eq!(group_report.child_count, 3);
        assert!(group_report.group_admitted);
        assert!(group_report.children_dispatched);
        assert_eq!(group_report.first_settlement_rank, 1);
        assert_eq!(group_report.settlements.len(), 3);
        assert_eq!(
            group_report.settlements[0].terminal,
            EffectGroupRunTerminal::Completed
        );
        assert!(
            group_report.settlements[1..]
                .iter()
                .all(|settlement| settlement.terminal == EffectGroupRunTerminal::Cancelled)
        );
        assert_eq!(group_report.cancelled_losers, 2);
        assert!(group_report.group_terminal);

        let durable_response = http
            .get(&group_url)
            .send()
            .await
            .expect("read durable effect-group report through agent-service HTTP surface");
        assert!(durable_response.status().is_success());
        let durable_report: EffectGroupRunReport = durable_response
            .json()
            .await
            .expect("decode durable effect-group report");
        assert_eq!(durable_report, group_report);

        let duplicate_response = http
            .post(format!("http://{app_addr}/api/effect-groups"))
            .json(&json!({ "run_id": group_run_id }))
            .send()
            .await
            .expect("repeat effect-group request through agent-service HTTP surface");
        let duplicate_status = duplicate_response.status();
        let duplicate_body = duplicate_response
            .text()
            .await
            .expect("read duplicate effect-group response");
        assert!(!duplicate_status.is_success());
        assert!(
            duplicate_body.contains("already exists"),
            "duplicate group response did not name the identity fence: {duplicate_status} {duplicate_body}"
        );

        let unchanged_report: EffectGroupRunReport = http
            .get(&group_url)
            .send()
            .await
            .expect("read unchanged effect-group report")
            .json()
            .await
            .expect("decode unchanged effect-group report");
        assert_eq!(unchanged_report, group_report);
        println!(
            "AGENT_SERVICE_EFFECT_GROUP group={} settings=OK preflight=ABSENT ranks={:?} cancelled_losers={} durable_read=MATCH duplicate=REFUSED unchanged=MATCH terminal=PASS",
            group_report.group_key, group_report.settlements, group_report.cancelled_losers
        );

        // The group report above proves that Restate recorded two cancelled
        // members. This separate wait proves the underlying await-event
        // terminal itself is durable: a fresh observer sees `Cancelled`, a
        // second await returns immediately, and a late completion cannot win.
        let wait_host = RestateEffectHost::new(
            ingress_url,
            lash_restate::RestateAuthorityId::new("agent-service-effect-group-test").unwrap(),
        );
        let wait_scope = ExecutionScope::turn(
            format!("agent-service-await-session-{}", uuid::Uuid::new_v4()),
            "cancelled-await-event",
        );
        let wait_key = wait_host
            .await_event_key(
                &wait_scope,
                AwaitEventWaitIdentity::Custom {
                    key: "worked-cancelled-await".to_string(),
                },
            )
            .await
            .expect("derive public Restate await-event key");
        let cancellation = CancellationToken::new();
        let pending_host = wait_host.clone();
        let pending_key = wait_key.clone();
        let pending_cancellation = cancellation.clone();
        let pending_wait = tokio::spawn(async move {
            pending_host
                .await_await_event(&pending_key, pending_cancellation, None)
                .await
        });

        let pending_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert!(
                !pending_wait.is_finished(),
                "await-event finished before cancellation"
            );
            match wait_host.peek_await_event(&wait_key).await {
                Ok(None) => break,
                Ok(Some(terminal)) => {
                    panic!("await-event terminalized before cancellation: {terminal:?}")
                }
                Err(_) if std::time::Instant::now() < pending_deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("await-event did not become observable: {error}"),
            }
        }

        cancellation.cancel();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), pending_wait)
            .await
            .expect("cancelled await-event must settle promptly")
            .expect("join cancelled await-event")
            .expect("cancelled await-event returns its durable terminal");
        assert_eq!(cancelled, Resolution::Cancelled);
        assert_eq!(
            wait_host
                .peek_await_event(&wait_key)
                .await
                .expect("read cancelled await-event terminal"),
            Some(Resolution::Cancelled)
        );

        let reawaited = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_host.await_await_event(&wait_key, CancellationToken::new(), None),
        )
        .await
        .expect("durably terminal await-event must not remain dangling")
        .expect("re-await cancelled await-event");
        assert_eq!(reawaited, Resolution::Cancelled);
        assert_eq!(
            wait_host
                .resolve_await_event(&wait_key, Resolution::Ok(json!({ "completion": "late" })),)
                .await
                .expect("attempt late await-event completion"),
            ResolveOutcome::AlreadyResolved {
                terminal: Resolution::Cancelled,
            }
        );
        assert_eq!(
            wait_host
                .peek_await_event(&wait_key)
                .await
                .expect("re-read cancelled await-event terminal"),
            Some(Resolution::Cancelled)
        );
        println!(
            "AGENT_SERVICE_AWAIT_EVENT_CANCELLATION wait={} pending=OBSERVED cancel_return=CANCELLED durable_peek=CANCELLED reawait=CANCELLED late_completion=REFUSED terminal=PASS",
            wait_key.promise_key()
        );

        let _ = app_shutdown_tx.send(());
        app_server.await.expect("agent-service E2E HTTP task");
        let _ = shutdown_tx.send(());
        server.await.expect("endpoint server task");
    }

    struct LiveRestateTestHarness {
        state: AppStateData,
        process_worker: lash::durability::DurableProcessWorker,
        backend: Arc<lash_restate::RestateEngine>,
    }

    async fn live_restate_test_state(
        data_dir: &Path,
        ingress_url: String,
    ) -> LiveRestateTestHarness {
        let app_db = Arc::new(Mutex::new(
            AppDb::open(&data_dir.join("app.db")).expect("open app db"),
        ));
        let provider = lash::testing::TestProvider::builder()
            .kind("mock-provider")
            .complete(|_request| async {
                let text = r#"<typescript>
const play_center_once = async () => {
  const state = await board.read({});
  if (state.turn == "O" && state.legal_moves.includes(4)) {
    const move = await board.play({ cell: 4 });
    return { before: state, move: move, played: true };
  }
  return { before: state, played: false };
};
const handle = await processes.start({ definition: play_center_once });
const result = await handle;
finish("done via Restate E2E");
</typescript>"#;
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let stores = lash_sqlite_store::SqliteStoreSet::open(data_dir.join("lash-sessions"))
            .await
            .expect("open the SQLite store set");
        let backend = Arc::new(lash_restate::RestateEngine::new(
            Arc::new(stores),
            lash::restate::config(
                ingress_url,
                lash_restate::RestateAuthorityId::new("agent-service-restate-test").unwrap(),
            ),
        ));
        // The worked example keeps its Sleep-only resolver as the deployment's
        // one answer, so no tool-child host is installed here — the same shape
        // the conformance suites use.
        backend
            .restate_effect_host()
            .register_group_executors(Arc::new(AgentServiceEffectGroupExecutors))
            .expect("register worked effect-group resolver");
        let lash_backend = lash::Backend::new(backend.clone());
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            &lash_backend,
        );
        let core = LashCore::rlm_builder(
            lash_backend,
            lash::TurnBudget::Unbounded,
            factory,
        )
            .provider(provider)
            .model(
                lash::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("valid mock model spec"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            // The `processes` module is catalogue presence, not an ability bit
            // (ADR 0095): the scripted cell below authors `processes.start`.
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
            ))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-test",
                "test",
            ))
            .expect("build test core");
        let demo_factory = DemoPlugin::factory(&DemoPluginConfig {
            db: Arc::clone(&app_db),
        });
        let process_worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config_with_plugins([demo_factory])
                .expect("process worker config"),
        )
        .expect("valid test native substrate config");
        let state = AppStateData::from_shared_db(
            core,
            backend.turn_work_driver(),
            app_db,
            "mock-model".to_string(),
            None,
            AgentServiceDurability::Restate,
            std::env::var("RESTATE_INGRESS_URL").ok(),
        );
        LiveRestateTestHarness {
            state,
            process_worker,
            backend,
        }
    }

    async fn wait_for_endpoint_socket(addr: SocketAddr) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Restate endpoint did not open a TCP listener at {addr}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    async fn register_restate_deployment(admin_url: &str, endpoint_url: &str) {
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .expect("build Restate admin client");
        let response = client
            .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
            .json(&json!({
                "uri": endpoint_url,
                "force": true,
                "breaking": true,
            }))
            .send()
            .await
            .expect("register deployment with Restate admin API");
        assert!(
            response.status().is_success(),
            "Restate deployment registration failed: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }
}
