#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]
#![cfg(feature = "restate")]

use lash::sync::MutexExt;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use lash::observe::SessionCursor;
use lash::rlm::RlmTurnBuilderExt as _;
use lash::{TurnInput, TurnOutput};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::db::{ChatMessage, ChatModelSelection};
use crate::routes::{
    ChannelTurnEvents, StreamItem, TurnPersistenceState, assistant_text_for_persistence,
    model_spec_for_chat_selection, spawn_live_replay_forwarder, wait_for_live_replay_flush,
};
use crate::state::{AppError, AppResult, AppStateData};

#[cfg(feature = "restate")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AgentServiceTurnWorkflowRequest {
    turn_id: String,
    chat_id: String,
    text: String,
    model: String,
    model_variant: Option<String>,
}

impl AgentServiceTurnWorkflowRequest {
    fn new(
        turn_id: String,
        chat_id: String,
        text: String,
        model: String,
        model_variant: Option<String>,
    ) -> Self {
        Self {
            turn_id,
            chat_id,
            text,
            model,
            model_variant,
        }
    }
}

#[cfg(feature = "restate")]
#[restate_sdk::workflow]
pub(crate) trait AgentServiceTurnWorkflow {
    async fn run(
        request: restate_sdk::serde::Json<AgentServiceTurnWorkflowRequest>,
    ) -> restate_sdk::errors::HandlerResult<restate_sdk::serde::Json<()>>;
}

#[cfg(feature = "restate")]
pub(crate) struct AgentServiceTurnWorkflowImpl {
    state: AppStateData,
}

impl AgentServiceTurnWorkflowImpl {
    pub(crate) fn new(state: AppStateData) -> Self {
        Self { state }
    }
}

#[cfg(feature = "restate")]
impl AgentServiceTurnWorkflow for AgentServiceTurnWorkflowImpl {
    async fn run(
        &self,
        ctx: restate_sdk::prelude::WorkflowContext<'_>,
        restate_sdk::serde::Json(request): restate_sdk::serde::Json<
            AgentServiceTurnWorkflowRequest,
        >,
    ) -> restate_sdk::errors::HandlerResult<restate_sdk::serde::Json<()>> {
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        run_restate_chat_turn_and_persist(self.state.clone(), request, &controller)
            .await
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        Ok(restate_sdk::serde::Json(()))
    }
}

#[cfg(feature = "restate")]
pub(crate) async fn send_message_restate(
    state: AppStateData,
    chat_id: String,
    text: String,
    user_message: ChatMessage,
    model_selection: ChatModelSelection,
) -> AppResult<Response> {
    let turn_id = uuid::Uuid::new_v4().to_string();
    state
        .with_db({
            let turn_id = turn_id.clone();
            let user_message = user_message.clone();
            move |db| {
                let item = StreamItem::Message {
                    message: user_message,
                };
                db.insert_turn_event(&turn_id, &item, item.is_done())
            }
        })
        .await?;

    let turn_model = model_spec_for_chat_selection(&model_selection)?;
    let replay_cursor = state
        .open_session(&chat_id, turn_model.clone())
        .await?
        .observe()
        .current_observation()
        .cursor;

    let request = AgentServiceTurnWorkflowRequest::new(
        turn_id.clone(),
        chat_id.clone(),
        text,
        model_selection.model,
        model_selection.model_variant,
    );
    // The workflow id is the stable turn id. The app does not persist a
    // finished/running work-item row; Restate owns in-flight replay and the
    // app outbox stores only product-visible rows keyed by turn_id.
    let ingress = state
        .restate_ingress_url()
        .map(str::to_string)
        .ok_or_else(|| AppError::internal("Restate ingress URL is not configured"))?;
    let url = format!(
        "{}/AgentServiceTurnWorkflow/{}/run/send",
        ingress.trim_end_matches('/'),
        turn_id
    );
    let response = state
        .restate_http()
        .post(url)
        .json(&request)
        .send()
        .await
        .map_err(|err| AppError::internal(format!("Restate workflow submit failed: {err}")))?;
    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "Restate workflow submit failed with status {}",
            response.status()
        )));
    }

    stream_turn_outbox(state, chat_id, turn_id, replay_cursor, turn_model).await
}

#[cfg(feature = "restate")]
async fn stream_turn_outbox(
    state: AppStateData,
    chat_id: String,
    turn_id: String,
    replay_cursor: SessionCursor,
    model: lash::ModelSpec,
) -> AppResult<Response> {
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    let (item_tx, mut item_rx) = mpsc::channel::<StreamItem>(64);
    let replay_session = state.open_session(&chat_id, model).await?;
    let mut replay = spawn_live_replay_forwarder(replay_session, replay_cursor, item_tx.clone());
    let stream_turn_id = turn_id.clone();
    tokio::spawn(async move {
        let tx_for_replay = tx.clone();
        tokio::spawn(async move {
            while let Some(item) = item_rx.recv().await {
                if write_stream_item(&tx_for_replay, &item).await.is_err() {
                    break;
                }
            }
        });
        let mut last_id = 0_i64;
        loop {
            match state
                .with_db({
                    let turn_id = stream_turn_id.clone();
                    move |db| db.list_turn_events_after(&turn_id, last_id)
                })
                .await
            {
                Ok(events) => {
                    let mut done = false;
                    for event in events {
                        last_id = event.id;
                        if event.is_done {
                            done = true;
                        }
                        let mut line = event.item_json;
                        line.push('\n');
                        if tx.send(Ok(Bytes::from(line))).await.is_err() {
                            return;
                        }
                    }
                    if done {
                        wait_for_live_replay_flush(&mut replay).await;
                        return;
                    }
                }
                Err(err) => {
                    replay.abort();
                    let mut line = json!({
                        "type": "error",
                        "message": err.message,
                    })
                    .to_string();
                    line.push('\n');
                    let _ = tx.send(Ok(Bytes::from(line))).await;
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-lash-turn-id", &turn_id)
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("valid streaming response"))
}

async fn write_stream_item(
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    item: &StreamItem,
) -> Result<(), ()> {
    let mut line = serde_json::to_string(item).unwrap_or_else(|err| {
        json!({
            "type": "error",
            "message": err.to_string(),
        })
        .to_string()
    });
    line.push('\n');
    tx.send(Ok(Bytes::from(line))).await.map_err(|_| ())
}

#[cfg(feature = "restate")]
async fn run_restate_chat_turn_and_persist(
    state: AppStateData,
    request: AgentServiceTurnWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<
        '_,
        restate_sdk::prelude::WorkflowContext<'_>,
    >,
) -> AppResult<()> {
    let turn_model = model_spec_for_chat_selection(&ChatModelSelection {
        model: request.model.clone(),
        model_variant: request.model_variant.clone(),
    })?;
    let session = state.open_session(&request.chat_id, turn_model).await?;
    let turn_state = Arc::new(Mutex::new(TurnPersistenceState::default()));
    let ui_events = ChannelTurnEvents::outbox(
        state.clone(),
        request.chat_id.clone(),
        request.turn_id.clone(),
        Arc::clone(&turn_state),
    );

    let input = TurnInput::text(request.text.clone());
    let output = session
        .turn(input)
        .turn_id(request.turn_id.clone())
        .require_finish()?
        // Durable in-flight work crosses the EffectHost boundary; the terminal
        // product row below is derived from Lash's TurnOutput.
        .effects(controller)
        .stream_to(&ui_events)
        .await;

    match output {
        Ok(output) => {
            let assistant_text = assistant_text_for_persistence(
                &TurnOutput {
                    result: output,
                    activities: Vec::new(),
                },
                turn_state.lock_recover().assistant_prose(),
            );
            let message = state
                .with_db({
                    let chat_id = request.chat_id.clone();
                    move |db| db.insert_message(&chat_id, "assistant", &assistant_text)
                })
                .await?;
            state
                .with_db({
                    let turn_id = request.turn_id.clone();
                    move |db| {
                        let item = StreamItem::Message { message };
                        db.insert_turn_event(&turn_id, &item, item.is_done())
                    }
                })
                .await?;
        }
        Err(err) => {
            state
                .with_db({
                    let turn_id = request.turn_id.clone();
                    let message = err.to_string();
                    move |db| {
                        let item = StreamItem::Error { message };
                        db.insert_turn_event(&turn_id, &item, item.is_done())
                    }
                })
                .await?;
        }
    }
    state
        .with_db({
            let turn_id = request.turn_id;
            move |db| {
                let item = StreamItem::Done;
                db.insert_turn_event(&turn_id, &item, item.is_done())
            }
        })
        .await?;
    Ok(())
}

#[cfg(all(test, feature = "restate"))]
mod restate_tests {
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::board::BoardState;
    use crate::db::AppDb;
    use crate::demo_plugin::{DemoPlugin, DemoPluginConfig};
    use crate::state::AgentServiceDurability;
    use lash::LashCore;
    use lash::PluginBinding;
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;
    use lash_restate::{
        LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
        LashDurableWaitWorkflowImpl, LashProcessWorkflow,
    };

    const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

    #[test]
    #[ignore = "requires a running Restate server; set RESTATE_INGRESS_URL and run with --ignored"]
    fn live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end() {
        std::thread::Builder::new()
            .name("agent-service-restate-e2e".to_string())
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .thread_stack_size(STACK_BUDGET_BYTES)
                    .enable_all()
                    .build()
                    .expect("build live Restate E2E runtime")
                    .block_on(
                        live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end_async(
                        ),
                    )
            })
            .expect("spawn live Restate E2E thread")
            .join()
            .expect("live Restate E2E thread");
    }

    async fn live_restate_ingress_runs_agent_turn_and_process_workflow_end_to_end_async() {
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
        let endpoint = restate_sdk::endpoint::Endpoint::builder()
            .bind(AgentServiceTurnWorkflowImpl::new(state.clone()).serve())
            .bind(
                harness
                    .process_deployment
                    .workflow(harness.process_worker.clone())
                    .serve(),
            )
            .bind(LashDurableWaitWorkflowImpl.serve())
            .bind(LashDurableWaitIndexImpl.serve())
            .build();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            restate_sdk::http_server::HttpServer::new(endpoint)
                .serve_with_cancel(listener, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        harness
            .process_deployment
            .process_work()
            .claim_and_run_pending("agent_service_e2e_startup")
            .await
            .expect("drive startup recovery");

        wait_for_endpoint_socket(local_probe_addr).await;
        register_restate_deployment(&admin_url, &endpoint_url).await;

        let chat = state
            .with_db(|db| db.create_chat("Restate E2E", "mock-model", None))
            .await
            .expect("create chat");
        state
            .with_db({
                let chat_id = chat.id.clone();
                move |db| {
                    db.upsert_chat_board(
                        &chat_id,
                        &BoardState {
                            cells: vec![None; 9],
                            turn: "O".to_string(),
                        },
                    )
                }
            })
            .await
            .expect("seed board");
        let turn_id = format!("agent-service-e2e-{}", uuid::Uuid::new_v4());
        let request = AgentServiceTurnWorkflowRequest::new(
            turn_id.clone(),
            chat.id.clone(),
            "play the next move".to_string(),
            "mock-model".to_string(),
            None,
        );
        let submit = state
            .restate_http()
            .post(format!(
                "{}/AgentServiceTurnWorkflow/{}/run/send",
                ingress_url.trim_end_matches('/'),
                turn_id
            ))
            .json(&request)
            .send()
            .await
            .expect("submit workflow through Restate ingress");
        assert!(
            submit.status().is_success(),
            "Restate workflow submit failed: {}",
            submit.status()
        );

        wait_for_turn_done(&state, &turn_id).await;
        let outbox_events = state
            .with_db({
                let turn_id = turn_id.clone();
                move |db| db.list_turn_events_after(&turn_id, 0)
            })
            .await
            .expect("list turn outbox events");
        let messages = state
            .with_db({
                let chat_id = chat.id.clone();
                move |db| db.list_messages(&chat_id)
            })
            .await
            .expect("list messages");
        assert!(
            messages.iter().any(|message| message.role == "assistant"
                && message.text.contains("done via Restate E2E")),
            "assistant message was not persisted through Restate workflow; messages={messages:?}; outbox={outbox_events:?}"
        );

        let _ = shutdown_tx.send(());
        server.await.expect("endpoint server task");
    }

    struct LiveRestateTestHarness {
        state: AppStateData,
        process_worker: lash::durability::DurableProcessWorker,
        process_deployment: lash_restate::RestateProcessDeployment,
    }

    async fn live_restate_test_state(
        data_dir: &Path,
        ingress_url: String,
    ) -> LiveRestateTestHarness {
        let app_db = Arc::new(Mutex::new(
            AppDb::open(&data_dir.join("app.db")).expect("open app db"),
        ));
        let process_registry_store = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &data_dir.join("processes.db"),
                data_dir.join("lash-sessions"),
            )
            .await
            .expect("open process registry"),
        );
        let process_registry =
            Arc::clone(&process_registry_store) as Arc<dyn lash::process::ProcessRegistry>;
        let process_continuations =
            process_registry_store as Arc<dyn lash::process::ProcessContinuationStore>;
        let provider = lash::testing::TestProvider::builder()
            .kind("mock-provider")
            .complete(|_request| async {
                let text = r#"<lashlang>
process play_center_once(board_tool: Board) {
  state = await board_tool.read({})?
  if state.turn == "O" and contains(state.legal_moves, 4) {
    move = await board_tool.play({ cell: 4 })?
    finish { before: state, move: move, played: true }
  } else {
    finish { before: state, played: false }
  }
}
handle = start play_center_once(board_tool: board)
result = (await handle)?
finish "done via Restate E2E"
</lashlang>"#;
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
        let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let artifact_store = Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                .await
                .expect("open artifact store"),
        ) as Arc<dyn lash::persistence::LashlangArtifactStore>;
        let process_env_store = Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
                .await
                .expect("open process env store"),
        );
        let trigger_store = Arc::new(
            lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                .await
                .expect("open trigger store"),
        );
        let process_deployment = lash_restate::RestateProcessDeployment::new(
            ingress_url.clone(),
            Arc::clone(&process_registry),
            process_continuations,
        );
        let turn_deployment = lash_restate::RestateTurnDeployment::new(ingress_url);
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            artifact_store,
        );
        let core = LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
            .with_native_queued_work()
            .provider(provider)
            .model(
                lash::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("valid mock model spec"),
            )
            .store_factory(store_factory)
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(process_env_store)
            .trigger_store(trigger_store)
            .effect_host(turn_deployment.effect_host())
            .process_work(process_deployment.process_work())
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
            turn_deployment.turn_work_driver(),
            app_db,
            "mock-model".to_string(),
            None,
            AgentServiceDurability::Restate,
            // The scripted provider answers with a `<lashlang>` cell, so this
            // fixture is Lashlang by construction rather than by omission.
            lash::rlm::RlmDialect::Lashlang,
            std::env::var("RESTATE_INGRESS_URL").ok(),
        );
        LiveRestateTestHarness {
            state,
            process_worker,
            process_deployment,
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

    async fn wait_for_turn_done(state: &AppStateData, turn_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let events = state
                .with_db({
                    let turn_id = turn_id.to_string();
                    move |db| db.list_turn_events_after(&turn_id, 0)
                })
                .await
                .expect("list turn events");
            if events.iter().any(|event| event.is_done) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let mut tail = events
                    .iter()
                    .rev()
                    .take(8)
                    .map(|event| (event.id, event.is_done, event.item_json.as_str()))
                    .collect::<Vec<_>>();
                tail.reverse();
                panic!(
                    "timed out waiting for Restate workflow turn outbox to finish; event_count={}; tail={tail:#?}",
                    events.len(),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}
