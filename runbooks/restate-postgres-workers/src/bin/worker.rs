use anyhow::{Context, Result};
use axum::{
    Json as AxumJson, Router, extract::State, http::StatusCode, routing::get, routing::post,
};
use lash::durability::DurableProcessWorker;
use lash::observe::SessionResume;
use lash::{TurnActivity, TurnActivitySink, TurnEvent, TurnInput};
use lash_core::AwaitEventResolver as _;
use lash_core::{
    ExecutionScope, ProcessEventAppendRequest, facade_support::TurnOutcome,
    facade_support::TurnStop,
};
use lash_postgres_store::PostgresStorage;
use lash_restate::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl, LashProcessWorkflow, RestateEffectHost, RestateProcessDeployment,
    RestateRuntimeEffectController,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::prelude::{Endpoint, WorkflowContext};
use restate_sdk::serde::Json;
use serde_json::json;
use std::fmt::Display;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_restate_postgres_workers_e2e::{
    DEFAULT_SESSION_ID, DirectDurableWaitAwaitRequest, DirectDurableWaitAwaitResponse,
    DirectDurableWaitResolveRequest, DirectDurableWaitResolveResponse, EXPECTED_ASYNC_TEXT,
    EXPECTED_DURABLE_INPUT_TEXT, EXPECTED_FINAL_TEXT, EXPECTED_FRAME_SWITCH_CANCEL_TEXT,
    EXPECTED_FRAME_SWITCH_TEXT, EXPECTED_PARENT_DURABLE_INPUT_TEXT, EXPECTED_SEGMENT_LOOP_TEXT,
    HealthResponse, TurnRequest, TurnResponse, TurnScenario, build_e2e_core,
    default_session_originator_id, e2e_tokio_thread_stack_bytes, ensure_e2e_schema, env,
    process_registry_from_storage, record_terminal_result, record_turn_activity,
    record_worker_event, required_env, s3_store_from_env, turn_session_id,
};

fn terminal_error(err: impl Display) -> TerminalError {
    TerminalError::new(err.to_string())
}

#[restate_sdk::workflow]
trait E2eTurnWorkflow {
    async fn run(request: Json<TurnRequest>) -> HandlerResult<Json<TurnResponse>>;

    #[shared]
    async fn health() -> HandlerResult<Json<HealthResponse>>;
}

#[derive(Clone)]
struct AppState {
    worker_id: String,
    storage: PostgresStorage,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    process_work_driver: lash::process::ProcessWorkDriver,
    restate_ingress_url: String,
    mock_provider_base_url: String,
    trace_dir: Option<PathBuf>,
    fail_once: bool,
}

impl AppState {
    async fn connect(process_work_driver: lash::process::ProcessWorkDriver) -> Result<Self> {
        let worker_id = env("WORKER_INSTANCE_ID", "worker-local");
        let database_url = required_env("DATABASE_URL")?;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .context("connect Postgres storage")?;
        ensure_e2e_schema(storage.pool()).await?;
        let attachment_store =
            Arc::new(s3_store_from_env()?) as Arc<dyn lash::persistence::AttachmentStore>;
        let restate_ingress_url = env("RESTATE_INGRESS_URL", "http://restate:8080");
        let mock_provider_base_url = env("MOCK_PROVIDER_BASE_URL", "http://mock-provider:18001");
        let trace_dir = std::env::var("LASH_E2E_TRACE_DIR").ok().map(PathBuf::from);
        if let Some(dir) = &trace_dir {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create trace dir `{}`", dir.display()))?;
        }
        let fail_once = env("LASH_E2E_FAIL_ONCE", "0") == "1";
        Ok(Self {
            worker_id,
            storage,
            attachment_store,
            process_work_driver,
            restate_ingress_url,
            mock_provider_base_url,
            trace_dir,
            fail_once,
        })
    }

    fn build_core(&self) -> Result<lash::LashCore> {
        build_e2e_core(lash_restate_postgres_workers_e2e::E2eCoreConfig {
            worker_id: self.worker_id.clone(),
            storage: self.storage.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            process_work_driver: self.process_work_driver.clone(),
            restate_ingress_url: self.restate_ingress_url.clone(),
            mock_provider_base_url: self.mock_provider_base_url.clone(),
            trace_dir: self.trace_dir.clone(),
            fail_once: self.fail_once,
        })
    }

    async fn run_turn_with_restate(
        &self,
        ctx: WorkflowContext<'_>,
        request: TurnRequest,
    ) -> HandlerResult<Json<TurnResponse>> {
        self.record(
            &request.workflow_id,
            "turn_started",
            json!({
                "scenario": request.scenario,
                "fail_once": request.fail_once,
            }),
        )
        .await?;

        let controller = RestateRuntimeEffectController::new(ctx);
        let core = self.build_core().map_err(terminal_error)?;
        if request.scenario == TurnScenario::SignalProcess {
            return Box::pin(self.signal_process(&controller, &core, &request))
                .await
                .map(Json);
        }

        if request.scenario == TurnScenario::DrainQueued {
            return Box::pin(self.drain_queued_turn_with_restate(&controller, &core, request))
                .await
                .map(Json);
        }

        if request.scenario == TurnScenario::FrameSwitchQueued {
            return Box::pin(self.frame_switch_queued(&controller, &core, request))
                .await
                .map(Json);
        }
        if request.scenario == TurnScenario::FrameSwitchCancel {
            return Box::pin(self.frame_switch_cancel(&controller, &core, request))
                .await
                .map(Json);
        }

        Box::pin(self.main_turn_with_restate(&controller, &core, request))
            .await
            .map(Json)
    }

    async fn drain_queued_turn_with_restate(
        &self,
        controller: &RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
        core: &lash::LashCore,
        request: TurnRequest,
    ) -> HandlerResult<TurnResponse> {
        let session = core
            .session(DEFAULT_SESSION_ID)
            .open()
            .await
            .map_err(terminal_error)?;

        let cursor = session.observe().current_observation().cursor;
        let cursor_text = cursor.as_str().to_string();
        let sink = RecordingTurnSink::new(
            self.storage.pool().clone(),
            request.workflow_id.clone(),
            self.worker_id.clone(),
            "queued",
            Some(cursor_text.clone()),
        );
        let turn = session
            .queued_turn()
            .drain_id(request.workflow_id.clone())
            .effects(controller)
            .stream_to(&sink)
            .await
            .map_err(terminal_error)?;
        let final_value = turn
            .as_ref()
            .and_then(|turn| turn.final_value().cloned())
            .unwrap_or(serde_json::Value::Null);
        self.finish_response(
            &request,
            final_value,
            sink.count().await,
            Some(cursor_text),
            turn.is_some(),
        )
        .await
    }

    async fn main_turn_with_restate(
        &self,
        controller: &RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
        core: &lash::LashCore,
        request: TurnRequest,
    ) -> HandlerResult<TurnResponse> {
        let session = core
            .session(turn_session_id(&request.workflow_id))
            .open()
            .await
            .map_err(terminal_error)?;

        let cursor = session.observe().current_observation().cursor;
        let cursor_text = cursor.as_str().to_string();

        let sink = RecordingTurnSink::new(
            self.storage.pool().clone(),
            request.workflow_id.clone(),
            self.worker_id.clone(),
            "main",
            Some(cursor_text.clone()),
        );
        let input = TurnInput::text(prompt_for_request(&request))
            .with_trace_turn_id(request.workflow_id.clone());
        let turn = session
            .turn(input)
            .turn_id(request.workflow_id.clone())
            .effects(controller)
            .stream_to(&sink)
            .await
            .map_err(terminal_error)?;
        let final_value = if matches!(
            request.scenario,
            TurnScenario::TurnControlHold
                | TurnScenario::TurnControlSleep
                | TurnScenario::TurnControlComplete
        ) {
            json!({
                "final": if matches!(
                    turn.outcome,
                    TurnOutcome::Stopped(TurnStop::Cancelled)
                ) {
                    "turn-control-cancelled"
                } else {
                    "turn-control-completed"
                },
                "outcome": turn.outcome,
                "cancellation": turn.cancellation,
                "turn_index": turn.state.turn_index,
            })
        } else {
            turn.final_value()
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        let submitted_events = sink.final_values().await;
        self.record(
            &request.workflow_id,
            "main_final_values",
            json!({
                "turn_result": final_value.clone(),
                "stream_final_values": submitted_events,
            }),
        )
        .await?;

        let replay_count = match session.observe().resume_from_cursor(&cursor) {
            Ok(SessionResume::Replayed { events }) => events.len(),
            Ok(SessionResume::Gap { .. }) => 0,
            Err(err) => {
                self.record(
                    &request.workflow_id,
                    "live_replay_failed",
                    json!({"error": err.to_string()}),
                )
                .await?;
                0
            }
        };
        self.record(
            &request.workflow_id,
            "live_replay_checked",
            json!({"events": replay_count, "cursor": cursor_text}),
        )
        .await?;

        self.finish_response(
            &request,
            final_value,
            sink.count().await,
            Some(cursor_text),
            false,
        )
        .await
    }

    async fn frame_switch_queued(
        &self,
        controller: &RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
        core: &lash::LashCore,
        request: TurnRequest,
    ) -> HandlerResult<TurnResponse> {
        let session = open_e2e_session(core).await?;
        let first = session
            .enqueue(TurnInput::text(format!(
                "Run queued frame switch. workflow_id={} frame_switch_queued_start=true",
                request.workflow_id
            )))
            .id(format!("{}:first", request.workflow_id))
            .send()
            .await
            .map_err(terminal_error)?;
        let enqueue_session = session.clone();
        let enqueue_pool = self.storage.pool().clone();
        let enqueue_workflow_id = request.workflow_id.clone();
        let enqueue_second = tokio::spawn(async move {
            wait_for_provider_scenario(
                &enqueue_pool,
                &enqueue_workflow_id,
                "frame_switch_queued_start",
            )
            .await?;
            enqueue_session
                .enqueue(TurnInput::text(format!(
                    "Run pending item. workflow_id={enqueue_workflow_id} frame_switch_pending=true"
                )))
                .id(format!("{enqueue_workflow_id}:second"))
                .send()
                .await
                .map_err(anyhow::Error::from)
        });
        let first_turn = session
            .queued_turn()
            .drain_id(format!("{}:first-drain", request.workflow_id))
            .effects(controller)
            .run()
            .await
            .map_err(terminal_error)?
            .ok_or_else(|| terminal_error("first queued frame-switch turn did not run"))?;
        let first_value = first_turn.final_value().cloned().ok_or_else(|| {
            terminal_error("queued frame-switch follow-on produced no final value")
        })?;
        let second = enqueue_second
            .await
            .map_err(terminal_error)?
            .map_err(terminal_error)?;
        let pending_after_follow = session
            .pending_turn_inputs()
            .await
            .map_err(terminal_error)?;
        let first_completed = pending_after_follow
            .iter()
            .all(|input| input.input_id != first.input_id);
        let second_pending_before_drain = pending_after_follow
            .iter()
            .any(|input| input.input_id == second.input_id);
        let second_turn = session
            .queued_turn()
            .drain_id(format!("{}:second-drain", request.workflow_id))
            .effects(controller)
            .run()
            .await
            .map_err(terminal_error)?
            .ok_or_else(|| terminal_error("second queued turn did not run"))?;
        let second_value = second_turn
            .final_value()
            .cloned()
            .ok_or_else(|| terminal_error("second queued turn produced no final value"))?;
        let queue_empty = session
            .queued_work()
            .await
            .map_err(terminal_error)?
            .is_empty();
        let inputs_empty = session
            .pending_turn_inputs()
            .await
            .map_err(terminal_error)?
            .is_empty();
        self.finish_response(
            &request,
            json!({
                "final": EXPECTED_FRAME_SWITCH_TEXT,
                "seed_visible": first_value.get("seed_visible").cloned().unwrap_or_default(),
                "follow_on": first_value.get("follow_on").cloned().unwrap_or_default(),
                "first_completed": first_completed,
                "second_pending_before_drain": second_pending_before_drain,
                "second_completed": second_value.get("pending_item").cloned().unwrap_or_default(),
                "queue_empty": queue_empty,
                "inputs_empty": inputs_empty,
            }),
            0,
            None,
            true,
        )
        .await
    }

    async fn frame_switch_cancel(
        &self,
        controller: &RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
        core: &lash::LashCore,
        request: TurnRequest,
    ) -> HandlerResult<TurnResponse> {
        let session = open_e2e_session(core).await?;
        session
            .enqueue(TurnInput::text(format!(
                "Run cancellable frame switch. workflow_id={} frame_switch_cancel_start=true",
                request.workflow_id
            )))
            .id(format!("{}:cancel-original", request.workflow_id))
            .send()
            .await
            .map_err(terminal_error)?;
        let cancel_session = session.clone();
        let cancel_pool = self.storage.pool().clone();
        let cancel_workflow_id = request.workflow_id.clone();
        let canceller = tokio::spawn(async move {
            wait_for_cancel_gate(&cancel_pool, &cancel_workflow_id).await?;
            Ok::<usize, anyhow::Error>(
                cancel_session
                    .cancel_running_turns_with_origin(Some("scripted-e2e-worker".to_string())),
            )
        });
        let cancelled = session
            .queued_turn()
            .drain_id(format!("{}:cancel-drain", request.workflow_id))
            .effects(controller)
            .run()
            .await
            .map_err(terminal_error)?
            .ok_or_else(|| terminal_error("cancellable queued turn did not run"))?;
        let cancel_count = canceller
            .await
            .map_err(terminal_error)?
            .map_err(terminal_error)?;
        let terminal_cancelled = matches!(
            cancelled.result.outcome,
            TurnOutcome::Stopped(TurnStop::Cancelled)
        );
        let claims_settled = session
            .queued_work()
            .await
            .map_err(terminal_error)?
            .is_empty()
            && session
                .pending_turn_inputs()
                .await
                .map_err(terminal_error)?
                .is_empty();
        let usable = session
            .turn(TurnInput::text(format!(
                "Run after cancellation. workflow_id={} frame_switch_post_cancel=true",
                request.workflow_id
            )))
            .turn_id(format!("{}:post-cancel", request.workflow_id))
            .effects(controller)
            .run()
            .await
            .map_err(terminal_error)?;
        let usable_value = usable
            .final_value()
            .cloned()
            .ok_or_else(|| terminal_error("post-cancel turn produced no final value"))?;
        self.finish_response(
            &request,
            json!({
                "final": EXPECTED_FRAME_SWITCH_CANCEL_TEXT,
                "terminal_cancelled": terminal_cancelled,
                "cancel_count": cancel_count,
                "claims_settled": claims_settled,
                "session_usable": usable_value.get("session_usable").cloned().unwrap_or_default(),
            }),
            0,
            None,
            true,
        )
        .await
    }

    async fn finish_response(
        &self,
        request: &TurnRequest,
        final_value: serde_json::Value,
        streamed_event_count: usize,
        replay_cursor: Option<String>,
        queued_turn_ran: bool,
    ) -> HandlerResult<TurnResponse> {
        let process_ids = self.load_session_process_ids().await?;
        let attachment_id = final_value
            .get("attachment_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let final_text = final_value
            .get("final")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(match request.scenario {
                TurnScenario::KitchenSink => EXPECTED_FINAL_TEXT,
                TurnScenario::TriggerSetup => "trigger-registered",
                TurnScenario::DrainQueued => "wake-consumed",
                TurnScenario::SignalSuspend => "signal-suspend-started",
                TurnScenario::SignalProcess => "signal-sent",
                TurnScenario::AsyncCompletion => EXPECTED_ASYNC_TEXT,
                TurnScenario::ProcessLlmQuery => "process-llm-query-complete",
                TurnScenario::DurableInputRequest => EXPECTED_DURABLE_INPUT_TEXT,
                TurnScenario::ParentDurableInputAfterChild => EXPECTED_PARENT_DURABLE_INPUT_TEXT,
                TurnScenario::ToolBatch => {
                    lash_restate_postgres_workers_e2e::EXPECTED_TOOL_BATCH_TEXT
                }
                TurnScenario::SegmentLoop => EXPECTED_SEGMENT_LOOP_TEXT,
                TurnScenario::FrameSwitchQueued | TurnScenario::FrameSwitchPrepared => {
                    EXPECTED_FRAME_SWITCH_TEXT
                }
                TurnScenario::FrameSwitchCancel => EXPECTED_FRAME_SWITCH_CANCEL_TEXT,
                TurnScenario::TurnControlHold | TurnScenario::TurnControlSleep => {
                    "turn-control-cancelled"
                }
                TurnScenario::TurnControlComplete => "turn-control-completed",
            })
            .to_string();
        let response = TurnResponse {
            workflow_id: request.workflow_id.clone(),
            worker_id: self.worker_id.clone(),
            process_id: process_ids.first().cloned().unwrap_or_default(),
            process_ids,
            attachment_id,
            final_text,
            final_value,
            streamed_event_count,
            replay_cursor,
            queued_turn_ran,
        };
        record_terminal_result(self.storage.pool(), &response)
            .await
            .map_err(terminal_error)?;
        self.record(
            &request.workflow_id,
            "turn_completed",
            json!({
                "attachment_id": response.attachment_id,
                "final_text": response.final_text,
                "queued_turn_ran": response.queued_turn_ran,
                "streamed_event_count": response.streamed_event_count,
            }),
        )
        .await?;
        Ok(response)
    }

    async fn signal_process(
        &self,
        controller: &RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
        core: &lash::LashCore,
        request: &TurnRequest,
    ) -> HandlerResult<TurnResponse> {
        let signal = request
            .signal
            .as_ref()
            .ok_or_else(|| terminal_error("signal_process scenario requires a signal payload"))?;
        let event_type = lash_core::facade_support::process_signal_event_type(&signal.signal_name)
            .map_err(terminal_error)?;
        let append = ProcessEventAppendRequest::new(event_type, signal.payload.clone())
            .with_replay_key(format!(
                "process:{}:signal.{}:{}",
                signal.process_id, signal.signal_name, signal.signal_id
            ));
        let scoped = controller
            .scoped_effect_controller(ExecutionScope::runtime_operation(format!(
                "e2e:{}:{}",
                request.workflow_id, signal.signal_id
            )))
            .map_err(terminal_error)?;
        let event = core
            .processes()
            .signal(
                &signal.process_id,
                signal.signal_name.clone(),
                signal.signal_id.clone(),
                append,
                scoped,
            )
            .await
            .map_err(terminal_error)?;
        self.finish_response(
            request,
            json!({
                "signalled": true,
                "process_id": signal.process_id,
                "signal": signal.signal_name,
                "sequence": event.sequence,
                "final": "signal-sent",
            }),
            0,
            None,
            false,
        )
        .await
    }

    async fn load_session_process_ids(&self) -> HandlerResult<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT process_id
             FROM lash_processes
             WHERE originator_id = $1
             ORDER BY created_at_ms, process_id",
        )
        .bind(default_session_originator_id())
        .fetch_all(self.storage.pool())
        .await
        .map_err(terminal_error)?)
    }

    async fn record(
        &self,
        workflow_id: &str,
        event_type: &str,
        detail: serde_json::Value,
    ) -> HandlerResult<()> {
        record_worker_event(
            self.storage.pool(),
            workflow_id,
            &self.worker_id,
            event_type,
            detail,
        )
        .await
        .map_err(terminal_error)?;
        Ok(())
    }
}

async fn direct_health(State(state): State<AppState>) -> AxumJson<HealthResponse> {
    AxumJson(HealthResponse {
        worker_id: state.worker_id,
        ok: true,
    })
}

async fn direct_resolve_durable_wait(
    State(state): State<AppState>,
    AxumJson(request): AxumJson<DirectDurableWaitResolveRequest>,
) -> Result<AxumJson<DirectDurableWaitResolveResponse>, (StatusCode, String)> {
    let outcome = RestateEffectHost::new(state.restate_ingress_url)
        .resolve_await_event(&request.key, request.resolution)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(AxumJson(DirectDurableWaitResolveResponse {
        worker_id: state.worker_id,
        outcome,
    }))
}

async fn direct_await_durable_wait(
    State(state): State<AppState>,
    AxumJson(request): AxumJson<DirectDurableWaitAwaitRequest>,
) -> Result<AxumJson<DirectDurableWaitAwaitResponse>, (StatusCode, String)> {
    let resolution = RestateEffectHost::new(state.restate_ingress_url)
        .await_await_event(
            &request.key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(AxumJson(DirectDurableWaitAwaitResponse {
        worker_id: state.worker_id,
        resolution,
    }))
}

fn prompt_for_request(request: &TurnRequest) -> String {
    match request.scenario {
        TurnScenario::KitchenSink => format!(
            "Run the canonical Lash Restate/Postgres/S3 kitchen sink workflow. workflow_id={} kitchen_sink=true fail_once={}",
            request.workflow_id, request.fail_once
        ),
        TurnScenario::TriggerSetup => format!(
            "Register the E2E trigger through Lashlang. workflow_id={} trigger_setup=true",
            request.workflow_id
        ),
        TurnScenario::DrainQueued => format!(
            "Drain the next queued E2E wake turn. workflow_id={} drain_queued=true",
            request.workflow_id
        ),
        TurnScenario::SignalSuspend => format!(
            "Start the E2E signal suspension process. workflow_id={} signal_suspend=true",
            request.workflow_id
        ),
        TurnScenario::SignalProcess => format!(
            "Signal the E2E process. workflow_id={} signal_process=true",
            request.workflow_id
        ),
        TurnScenario::AsyncCompletion => format!(
            "Run the E2E async host tool completion scenario. workflow_id={} async_completion=true",
            request.workflow_id
        ),
        TurnScenario::ProcessLlmQuery => format!(
            "Run the process llm_query scenario. workflow_id={} process_llm_query=true fail_once={}",
            request.workflow_id, request.fail_once
        ),
        TurnScenario::DurableInputRequest => format!(
            "Run the E2E durable input request scenario. workflow_id={} durable_input_request=true",
            request.workflow_id
        ),
        TurnScenario::ParentDurableInputAfterChild => format!(
            "Run the E2E parent durable input after child scenario. workflow_id={} parent_durable_input_after_child=true",
            request.workflow_id
        ),
        TurnScenario::ToolBatch => format!(
            "Run the E2E tool batch scenario. workflow_id={} tool_batch=true fail_once={}",
            request.workflow_id, request.fail_once
        ),
        TurnScenario::SegmentLoop => format!(
            "Run the E2E segmented authored loop control pair. workflow_id={} segment_loop=true",
            request.workflow_id
        ),
        TurnScenario::FrameSwitchQueued => format!(
            "Run the queued frame-switch scenario. workflow_id={} frame_switch_queued_start=true",
            request.workflow_id
        ),
        TurnScenario::FrameSwitchPrepared => format!(
            "Run the prepared frame-switch scenario. workflow_id={} frame_switch_prepared_start=true",
            request.workflow_id
        ),
        TurnScenario::FrameSwitchCancel => format!(
            "Run the cancellation frame-switch scenario. workflow_id={} frame_switch_cancel_start=true",
            request.workflow_id
        ),
        TurnScenario::TurnControlHold => format!(
            "Run the exact-turn cancellation hold. workflow_id={} turn_control_hold=true fail_once={}",
            request.workflow_id, request.fail_once
        ),
        TurnScenario::TurnControlSleep => format!(
            "Run the exact-turn suspended sleep cancellation. workflow_id={} turn_control_sleep=true",
            request.workflow_id
        ),
        TurnScenario::TurnControlComplete => format!(
            "Run the exact-turn completion race. workflow_id={} turn_control_complete=true",
            request.workflow_id
        ),
    }
}

async fn open_e2e_session(core: &lash::LashCore) -> HandlerResult<lash::LashSession> {
    Ok(core
        .session(DEFAULT_SESSION_ID)
        .open()
        .await
        .map_err(terminal_error)?)
}

async fn wait_for_cancel_gate(pool: &sqlx::PgPool, workflow_id: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let started: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM lash_e2e_tool_events
                WHERE workflow_id = $1 AND tool_name = 'cancel_gate'
            )",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .context("poll cancel_gate start")?;
        if started {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("timed out waiting for cancel_gate in `{workflow_id}`")
}

async fn wait_for_provider_scenario(
    pool: &sqlx::PgPool,
    workflow_id: &str,
    scenario: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let observed: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM lash_e2e_provider_calls
                WHERE workflow_id = $1 AND scenario = $2
            )",
        )
        .bind(workflow_id)
        .bind(scenario)
        .fetch_one(pool)
        .await
        .with_context(|| format!("poll provider scenario `{scenario}`"))?;
        if observed {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("timed out waiting for provider scenario `{scenario}` in `{workflow_id}`")
}

#[derive(Clone)]
struct RecordingTurnSink {
    pool: sqlx::PgPool,
    workflow_id: String,
    worker_id: String,
    stream_name: String,
    cursor: Option<String>,
    activities: Arc<tokio::sync::Mutex<Vec<TurnActivity>>>,
}

impl RecordingTurnSink {
    fn new(
        pool: sqlx::PgPool,
        workflow_id: String,
        worker_id: String,
        stream_name: impl Into<String>,
        cursor: Option<String>,
    ) -> Self {
        Self {
            pool,
            workflow_id,
            worker_id,
            stream_name: stream_name.into(),
            cursor,
            activities: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    async fn count(&self) -> usize {
        self.activities.lock().await.len()
    }

    async fn final_values(&self) -> Vec<serde_json::Value> {
        self.activities
            .lock()
            .await
            .iter()
            .filter_map(|activity| match &activity.event {
                TurnEvent::FinalValue { value } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl TurnActivitySink for RecordingTurnSink {
    async fn emit(&self, activity: TurnActivity) {
        if let Err(err) = record_turn_activity(
            &self.pool,
            &self.workflow_id,
            &self.worker_id,
            &self.stream_name,
            self.cursor.as_deref(),
            &activity,
        )
        .await
        {
            tracing::error!(
                workflow_id = %self.workflow_id,
                worker_id = %self.worker_id,
                error = %err,
                "failed to record streamed turn activity"
            );
        }
        self.activities.lock().await.push(activity);
    }
}

struct E2eTurnWorkflowImpl {
    state: AppState,
}

impl E2eTurnWorkflowImpl {
    fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl E2eTurnWorkflow for E2eTurnWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<TurnRequest>,
    ) -> HandlerResult<Json<TurnResponse>> {
        self.state.run_turn_with_restate(ctx, request).await
    }

    async fn health(
        &self,
        _ctx: restate_sdk::context::SharedWorkflowContext<'_>,
    ) -> HandlerResult<Json<HealthResponse>> {
        Ok(Json(HealthResponse {
            worker_id: self.state.worker_id.clone(),
            ok: true,
        }))
    }
}

fn main() -> Result<()> {
    lash_core::panic_containment::set_loud(true);
    let stack_bytes = e2e_tokio_thread_stack_bytes()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .context("build e2e worker Tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let database_url = required_env("DATABASE_URL")?;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .context("connect Postgres storage for process deployment")?;
    ensure_e2e_schema(storage.pool()).await?;
    let registry = process_registry_from_storage(&storage);
    let continuations =
        lash_restate_postgres_workers_e2e::process_continuations_from_storage(&storage);
    let deployment = RestateProcessDeployment::new(
        env("RESTATE_INGRESS_URL", "http://restate:8080"),
        registry,
        continuations,
    );
    let process_work_driver = deployment.process_work_driver();
    let state = AppState::connect(process_work_driver.clone()).await?;
    if state.fail_once {
        tracing::warn!(worker_id = %state.worker_id, "worker can exit once from crash_once tool");
        let recovering_failover_owner: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM lash_e2e_failover_markers WHERE worker_id = $1
            )",
        )
        .bind(&state.worker_id)
        .fetch_one(storage.pool())
        .await
        .context("check whether failover owner is restarting")?;
        if recovering_failover_owner {
            // Keep the crashed endpoint unavailable across Restate's first
            // retries so the proxy deterministically hands ownership to the
            // healthy peer. This also prevents a turn and its concurrent
            // cancellation-gate observer from locking round-robin routing to
            // the same restarted owner.
            tracing::warn!(
                worker_id = %state.worker_id,
                "delaying failover-owner endpoint restart for peer takeover"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    let core = state.build_core()?;
    let process_worker = DurableProcessWorker::new(core.durable_process_worker_config()?);
    let process_workflow = deployment
        .workflow(process_worker)
        .with_segment_effect_budget_selector(|registration| match &*registration.input {
            lash_core::ProcessInput::Engine { payload, .. }
                if payload
                    .pointer("/args/force_segmentation")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true) =>
            {
                3
            }
            _ => 10_000,
        });

    let port = env("WORKER_PORT", "18100");
    let addr: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .context("parse worker addr")?;
    let control_port = env("WORKER_CONTROL_PORT", "18101");
    let control_addr: SocketAddr = format!("0.0.0.0:{control_port}")
        .parse()
        .context("parse worker control addr")?;
    let control_listener = tokio::net::TcpListener::bind(control_addr)
        .await
        .context("bind worker control endpoint")?;
    let control_router = Router::new()
        .route("/health", get(direct_health))
        .route("/await-durable-wait", post(direct_await_durable_wait))
        .route("/resolve-durable-wait", post(direct_resolve_durable_wait))
        .with_state(state.clone());
    tokio::spawn(async move {
        if let Err(err) = axum::serve(control_listener, control_router).await {
            tracing::error!(error = %err, "worker control endpoint exited");
        }
    });
    let endpoint = Endpoint::builder()
        .bind(E2eTurnWorkflowImpl::new(state).serve())
        .bind(process_workflow.serve())
        .bind(LashDurableWaitWorkflowImpl.serve())
        .bind(LashDurableWaitIndexImpl.serve())
        .build();
    restate_sdk::http_server::HttpServer::new(endpoint)
        .listen_and_serve(addr)
        .await;
    Ok(())
}
