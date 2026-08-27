use lash::sync::MutexExt;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds};
use futures_util::FutureExt as _;
use lash::TurnInput;
use lash::rlm::RlmTurnBuilderExt as _;
use lash::runtime::AwaitEventResolver as _;
use lash_restate::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl, LashProcessWorkflow,
};
use restate_sdk::context::{
    ContextClient, ContextReadState, ContextSideEffects, ContextWriteState, InvocationHandle,
    RunFuture,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::prelude::{Endpoint, ObjectContext, SharedObjectContext, WorkflowContext};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    AppError, AppErrorVerdict, AppState, ButtonChoice, CRON_SCHEDULE_SOURCE_TYPE,
    ChannelTurnEvents, ModelSelection, TurnStreamState, apply_model_selection_to_session,
    assistant_text_for_display, commit_assistant_transcript, enqueue_button_trigger_command,
    enqueue_mail_received_trigger_command, model_spec_from_selection,
    restate_ingress::{submit_restate_empty, submit_restate_workflow_json},
    workbench_owns_committed_agent_reply, workbench_turn_assistant_message_id,
};

const CRON_STATE_KEY: &str = "state";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchTurnWorkflowRequest {
    pub turn_id: String,
    pub session_id: String,
    pub text: String,
    pub model: ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchQueuedTurnWorkflowRequest {
    pub turn_id: String,
    pub session_id: String,
    pub reason: String,
    #[serde(default)]
    pub batch_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_id: Option<String>,
}

impl WorkbenchQueuedTurnWorkflowRequest {
    pub(crate) fn queued_turn(&self, session: &lash::LashSession) -> lash::QueuedTurnBuilder {
        debug_assert!(self.batch_ids.is_empty());
        session.queued_turn().drain_id(
            self.drain_id
                .clone()
                .unwrap_or_else(|| self.turn_id.clone()),
        )
    }

    pub(crate) fn selected_queued_turn(
        &self,
        session: &lash::LashSession,
    ) -> lash::SelectedQueuedTurnBuilder {
        debug_assert!(!self.batch_ids.is_empty());
        session
            .queued_turn()
            .batch_ids(self.batch_ids.iter().cloned())
            .drain_id(
                self.drain_id
                    .clone()
                    .unwrap_or_else(|| self.turn_id.clone()),
            )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchButtonTriggerWorkflowRequest {
    pub operation_id: String,
    pub session_id: String,
    pub button: ButtonChoice,
    pub model: ModelSelection,
    pub pressed_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchSessionDeleteWorkflowRequest {
    pub operation_id: String,
    pub session_id: String,
    pub execution_scope: lash::runtime::ExecutionScope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchProcessCancelWorkflowRequest {
    pub operation_id: String,
    pub session_id: String,
    pub process_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkbenchMailReceivedWorkflowRequest {
    pub operation_id: String,
    pub session_id: String,
    pub model: ModelSelection,
    pub delivery: crate::mail::MailDelivery,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct WorkbenchCronRequest {
    session_id: String,
    source_key: String,
    expr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CronScheduleSource {
    expr: String,
    #[serde(default)]
    tz: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct WorkbenchCronState {
    request: WorkbenchCronRequest,
    next_execution_time: String,
    next_execution_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_fired_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkbenchCronInfo {
    source_key: String,
    expr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tz: Option<String>,
    next_execution_time: String,
    next_execution_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_fired_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CronEmitReport {
    started_process_ids: Vec<String>,
}

impl From<&WorkbenchCronState> for WorkbenchCronInfo {
    fn from(state: &WorkbenchCronState) -> Self {
        Self {
            source_key: state.request.source_key.clone(),
            expr: state.request.expr.clone(),
            tz: state.request.tz.clone(),
            next_execution_time: state.next_execution_time.clone(),
            next_execution_id: state.next_execution_id.clone(),
            last_fired_at: state.last_fired_at.clone(),
        }
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchTurnWorkflow {
    async fn run(request: Json<WorkbenchTurnWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchTurnWorkflowImpl {
    state: AppState,
}

impl WorkbenchTurnWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchTurnWorkflow for WorkbenchTurnWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchTurnWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let session_id = request.session_id.clone();
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        Box::pin(run_user_turn_terminalized(
            self.state.clone(),
            request,
            &controller,
        ))
        .await?;
        sync_cron_jobs_with_context(&self.state, controller.context(), &session_id, "user_turn")
            .await?;
        self.state
            .queued_work_driver
            .drain_session(&session_id, "user_turn_completed")
            .await
            // Audited: typed queued-work store refusals use the shared terminal classifier; ambiguous failures remain retryable.
            .map_err(classified_plugin_handler_error)?;
        Ok(Json(()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchQueuedTurnWorkflow {
    async fn run(request: Json<WorkbenchQueuedTurnWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchQueuedTurnWorkflowImpl {
    state: AppState,
}

impl WorkbenchQueuedTurnWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchQueuedTurnWorkflow for WorkbenchQueuedTurnWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchQueuedTurnWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let session_id = request.session_id.clone();
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        Box::pin(run_queued_turn_terminalized(
            self.state.clone(),
            request,
            &controller,
        ))
        .await?;
        self.state
            .queued_work_driver
            .drain_session(&session_id, "queued_turn_completed")
            .await
            // Audited: typed queued-work store refusals use the shared terminal classifier; ambiguous failures remain retryable.
            .map_err(classified_plugin_handler_error)?;
        sync_cron_jobs_with_context(
            &self.state,
            controller.context(),
            &session_id,
            "queued_turn",
        )
        .await?;
        Ok(Json(()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchButtonTriggerWorkflow {
    async fn run(request: Json<WorkbenchButtonTriggerWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchButtonTriggerWorkflowImpl {
    state: AppState,
}

impl WorkbenchButtonTriggerWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchButtonTriggerWorkflow for WorkbenchButtonTriggerWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchButtonTriggerWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let session_id = request.session_id.clone();
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        run_button_trigger(self.state.clone(), request, &controller)
            .await
            .map_err(terminal_handler_error)?;
        self.state
            .queued_work_driver
            .drain_session(&session_id, "button_trigger")
            .await
            // Audited: typed queued-work store refusals use the shared terminal classifier; ambiguous failures remain retryable.
            .map_err(classified_plugin_handler_error)?;
        Ok(Json(()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchMailReceivedWorkflow {
    async fn run(request: Json<WorkbenchMailReceivedWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchMailReceivedWorkflowImpl {
    state: AppState,
}

impl WorkbenchMailReceivedWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchMailReceivedWorkflow for WorkbenchMailReceivedWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchMailReceivedWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let session_id = request.session_id.clone();
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        run_mail_received(self.state.clone(), request, &controller)
            .await
            .map_err(terminal_handler_error)?;
        self.state
            .queued_work_driver
            .drain_session(&session_id, "mail_received")
            .await
            // Audited: typed queued-work store refusals use the shared terminal classifier; ambiguous failures remain retryable.
            .map_err(classified_plugin_handler_error)?;
        Ok(Json(()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchSessionDeleteWorkflow {
    async fn run(request: Json<WorkbenchSessionDeleteWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchSessionDeleteWorkflowImpl {
    state: AppState,
}

impl WorkbenchSessionDeleteWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchSessionDeleteWorkflow for WorkbenchSessionDeleteWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchSessionDeleteWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        run_session_delete(self.state.clone(), request, &controller)
            .await
            .map_err(terminal_handler_error)?;
        Ok(Json(()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait WorkbenchProcessCancelWorkflow {
    async fn run(request: Json<WorkbenchProcessCancelWorkflowRequest>) -> HandlerResult<Json<()>>;
}

pub(crate) struct WorkbenchProcessCancelWorkflowImpl {
    state: AppState,
}

impl WorkbenchProcessCancelWorkflowImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchProcessCancelWorkflow for WorkbenchProcessCancelWorkflowImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<WorkbenchProcessCancelWorkflowRequest>,
    ) -> HandlerResult<Json<()>> {
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        run_process_cancel(self.state.clone(), request, &controller)
            .await
            .map_err(terminal_handler_error)?;
        Ok(Json(()))
    }
}

#[restate_sdk::object]
trait WorkbenchCronJob {
    async fn upsert(request: Json<WorkbenchCronRequest>) -> HandlerResult<Json<WorkbenchCronInfo>>;
    async fn run() -> HandlerResult<Json<()>>;
    async fn cancel() -> HandlerResult<Json<()>>;
    #[shared]
    async fn info() -> HandlerResult<Json<Option<WorkbenchCronInfo>>>;
}

pub(crate) struct WorkbenchCronJobImpl {
    state: AppState,
}

include!("restate_cron.rs");

impl WorkbenchCronJobImpl {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl WorkbenchCronJob for WorkbenchCronJobImpl {
    async fn upsert(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<WorkbenchCronRequest>,
    ) -> HandlerResult<Json<WorkbenchCronInfo>> {
        let now = journaled_now(&ctx, "workbench-cron:upsert-now").await?;
        if let Some(Json(existing)) = ctx.get::<Json<WorkbenchCronState>>(CRON_STATE_KEY).await?
            && existing.request == request
        {
            // Only short-circuit while the stored chain is still alive. If the
            // recorded next execution is in the past the chain died (e.g. a
            // crash between fire and re-arm) and an equal-request upsert is
            // exactly the sync pass that should revive it.
            let chain_alive = DateTime::parse_from_rfc3339(&existing.next_execution_time)
                .map(|next| next.with_timezone(&Utc) > now)
                .unwrap_or(false);
            if chain_alive {
                return Ok(Json(WorkbenchCronInfo::from(&existing)));
            }
        }
        cancel_stored_execution(&ctx).await?;
        let state = schedule_next(&ctx, request, now, None).await?;
        Ok(Json(WorkbenchCronInfo::from(&state)))
    }

    async fn run(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        let Some(Json(state)) = ctx.get::<Json<WorkbenchCronState>>(CRON_STATE_KEY).await? else {
            return Ok(Json(()));
        };
        // Orphan guard: cancel jobs whose session is permanently retired or
        // whose store metadata is absent, while allowing every live session
        // (current or non-current) to tick. This also catches jobs armed by a
        // previous process run that in-memory cancel bookkeeping cannot see.
        let disposition = {
            let app_state = self.state.clone();
            let session_id = state.request.session_id.clone();
            let journal_value = ctx
                .run(move || {
                    let app_state = app_state.clone();
                    let session_id = session_id.clone();
                    async move {
                        cron_session_disposition(&app_state.core, &session_id)
                            .await
                            .map(|disposition| disposition.journal_value().to_string())
                    }
                })
                .name("workbench-cron:session-disposition")
                .await?;
            CronSessionDisposition::from_journal_value(&journal_value)?
        };
        match cron_tick_decision(disposition, &state, ctx.key()) {
            CronTick::Cancel { trace } => {
                journaled_workbench_trace(
                    &ctx,
                    self.state.clone(),
                    state.request.session_id.clone(),
                    "cron.restate.zombie_cancelled",
                    trace,
                    "workbench-cron:trace-cancelled",
                )
                .await?;
                ctx.clear(CRON_STATE_KEY);
                return Ok(Json(()));
            }
            CronTick::Run => {}
        }
        let controller = lash_restate::RestateRuntimeEffectController::new(ctx);
        let fired_at = journaled_now(controller.context(), "workbench-cron:fired-at").await?;
        let request = state.request.clone();
        let fired_at_text = fired_at.to_rfc3339();
        // Re-arm before emitting: a tick whose emission fails terminally must
        // not take the whole schedule down with it.
        schedule_next(
            controller.context(),
            state.request.clone(),
            fired_at,
            Some(fired_at.to_rfc3339()),
        )
        .await?;
        journaled_workbench_trace(
            controller.context(),
            self.state.clone(),
            state.request.session_id.clone(),
            "cron.restate.run",
            json!({
                "job_key": controller.context().key(),
                "job_session_id": &state.request.session_id,
                "source_key": &state.request.source_key,
                "expr": &state.request.expr,
                "tz": &state.request.tz,
                "fired_at": fired_at.to_rfc3339(),
                "decision_basis": "session_store_meta_present",
                "session_state": "live",
            }),
            "workbench-cron:trace-run",
        )
        .await?;
        let Json(emit_report) =
            emit_cron_occurrence(self.state.clone(), request, fired_at_text, &controller).await?;
        journaled_workbench_trace(
            controller.context(),
            self.state.clone(),
            state.request.session_id.clone(),
            "cron.restate.emit_completed",
            json!({
                "job_key": controller.context().key(),
                "source_key": &state.request.source_key,
                "expr": &state.request.expr,
                "tz": &state.request.tz,
                "fired_at": fired_at.to_rfc3339(),
                "started_process_ids": emit_report.started_process_ids,
            }),
            "workbench-cron:trace-emit-completed",
        )
        .await?;
        self.state
            .queued_work_driver
            .drain_session(&state.request.session_id, "cron_tick")
            .await
            // Audited: typed queued-work store refusals use the shared terminal classifier; ambiguous failures remain retryable.
            .map_err(classified_plugin_handler_error)?;
        Ok(Json(()))
    }

    async fn cancel(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        cancel_stored_execution(&ctx).await?;
        ctx.clear(CRON_STATE_KEY);
        Ok(Json(()))
    }

    async fn info(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<Option<WorkbenchCronInfo>>> {
        Ok(Json(
            ctx.get::<Json<WorkbenchCronState>>(CRON_STATE_KEY)
                .await?
                .as_ref()
                .map(|Json(state)| WorkbenchCronInfo::from(state)),
        ))
    }
}

pub(crate) fn spawn_restate_endpoint(
    addr: SocketAddr,
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
) {
    let endpoint = Endpoint::builder()
        .bind(WorkbenchTurnWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchQueuedTurnWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchButtonTriggerWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchMailReceivedWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchSessionDeleteWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchProcessCancelWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchCronJobImpl::new(state).serve())
        .bind(process_deployment.workflow(process_worker).serve())
        .bind(LashDurableWaitWorkflowImpl.serve())
        .bind(LashDurableWaitIndexImpl.serve())
        .build();
    tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(endpoint)
            .listen_and_serve(addr)
            .await;
    });
}

pub(crate) async fn submit_user_turn(
    state: &AppState,
    request: WorkbenchTurnWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        &state.restate_http,
        &state.restate_ingress_url,
        "WorkbenchTurnWorkflow",
        &request.turn_id,
        &request,
    )
    .await
}

pub(crate) async fn submit_queued_turn_request(
    restate_http: &reqwest::Client,
    restate_ingress_url: &str,
    request: &WorkbenchQueuedTurnWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        restate_http,
        restate_ingress_url,
        "WorkbenchQueuedTurnWorkflow",
        &request.turn_id,
        request,
    )
    .await
}

pub(crate) async fn submit_button_trigger(
    state: &AppState,
    request: WorkbenchButtonTriggerWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        &state.restate_http,
        &state.restate_ingress_url,
        "WorkbenchButtonTriggerWorkflow",
        &request.operation_id,
        &request,
    )
    .await
}

pub(crate) async fn submit_mail_received(
    state: &AppState,
    request: WorkbenchMailReceivedWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_mail_received_with_client(&state.restate_http, &state.restate_ingress_url, request).await
}

pub(crate) async fn submit_mail_received_with_client(
    restate_http: &reqwest::Client,
    restate_ingress_url: &str,
    request: WorkbenchMailReceivedWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        restate_http,
        restate_ingress_url,
        "WorkbenchMailReceivedWorkflow",
        &request.operation_id,
        &request,
    )
    .await
}

pub(crate) async fn submit_session_delete(
    state: &AppState,
    request: WorkbenchSessionDeleteWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        &state.restate_http,
        &state.restate_ingress_url,
        "WorkbenchSessionDeleteWorkflow",
        &request.operation_id,
        &request,
    )
    .await
}

pub(crate) async fn submit_process_cancel(
    state: &AppState,
    request: WorkbenchProcessCancelWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    submit_restate_workflow_json(
        &state.restate_http,
        &state.restate_ingress_url,
        "WorkbenchProcessCancelWorkflow",
        &request.operation_id,
        &request,
    )
    .await
}

/// Cancel every cron job belonging to `session_id`, derived from the durable
/// trigger registrations (the same source `sync_cron_jobs_with_context`
/// schedules from), plus anything this process armed. The in-memory key set
/// alone is not enough: jobs armed by a previous process run are invisible to
/// it and would keep firing into a deleted session forever.
pub(crate) async fn cancel_cron_jobs_for_session(
    state: &AppState,
    session_id: &str,
    reason: &str,
) -> Result<(), AppError> {
    let mut policy = lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded);
    policy.session_id = Some(session_id.to_string());
    policy.model = model_spec_from_selection(state.selected_model());
    let store = state
        .session_store_factory
        .create_store(&lash::persistence::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.to_string(),
            relation: lash::persistence::SessionRelation::Root,
            policy,
        })
        .await
        .map_err(|error| {
            state.session_admission_error(
                session_id,
                "cron.restate.cancel",
                lash::EmbedError::Store(error),
            )
        })?;
    store.read_session_state_version().await.map_err(|error| {
        state.session_admission_error(
            session_id,
            "cron.restate.cancel",
            lash::EmbedError::Store(error),
        )
    })?;
    let mut filter = lash::triggers::TriggerSubscriptionFilter::for_session(session_id);
    filter.source_type = Some(CRON_SCHEDULE_SOURCE_TYPE.to_string());
    let registrations = state
        .trigger_store
        .list_subscriptions(filter)
        .await
        .map_err(AppError::internal)?;
    if state
        .session_store_factory
        .session_was_deleted(session_id)
        .await
        .map_err(AppError::internal)?
    {
        return Err(state.session_admission_error(
            session_id,
            "cron.restate.cancel",
            lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted {
                session_id: session_id.to_string(),
            }),
        ));
    }
    store.read_session_state_version().await.map_err(|error| {
        state.session_admission_error(
            session_id,
            "cron.restate.cancel",
            lash::EmbedError::Store(error),
        )
    })?;
    let mut job_keys: BTreeSet<String> = registrations
        .iter()
        .map(|registration| cron_job_key(session_id, &registration.source_key))
        .collect();
    job_keys.extend({
        let mut guard = state.restate_cron_job_keys.lock_recover();
        guard.remove(session_id).unwrap_or_default()
    });
    for job_key in job_keys {
        state.trace_for_session(
            session_id,
            "cron.restate.cancel",
            json!({
                "job_key": job_key,
                "reason": reason,
            }),
        );
        submit_restate_empty(state, "WorkbenchCronJob", &job_key, "cancel").await?;
    }
    Ok(())
}

async fn run_user_turn(
    state: AppState,
    request: WorkbenchTurnWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    let input = workbench_turn_input(&state, &request).await?;
    let turn_model_id = request.model.model.clone();
    let turn_model = model_spec_from_selection(request.model);
    let session = state
        .open_session(&request.session_id)
        .await
        .map_err(AppError::session_open)?;
    apply_model_selection_to_session(&state, &session, turn_model.clone(), "restate_user_turn")
        .await?;
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    let output = session
        .turn(input)
        .turn_id(request.turn_id.clone())
        .require_finish()
        // Audited: require_finish only validates local turn-builder configuration and performs no session-store I/O.
        .map_err(AppError::internal)?
        .effects(controller)
        .stream_to(&ui_events)
        .await
        .map_err(AppError::runtime)?;
    record_turn_output_for_model(
        &state,
        &session,
        &request.turn_id,
        output,
        turn_state,
        "restate_user_turn.completed",
        Some(&turn_model_id),
    )
    .await?;
    Ok(())
}

pub(crate) async fn workbench_turn_input(
    state: &AppState,
    request: &WorkbenchTurnWorkflowRequest,
) -> Result<TurnInput, AppError> {
    let mut input = TurnInput::text(request.text.clone());
    if let Some(attachment_id) = request.attachment_id.as_deref() {
        // Request-supplied id: untrusted, so a malformed one is a bad request.
        let attachment_id = lash::attachments::AttachmentId::parse(attachment_id)
            .map_err(|err| AppError::bad_request(err.to_string()))?;
        let stored = state
            .attachment_store
            .get(&attachment_id)
            .await
            // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
            .map_err(AppError::internal)?;
        input = input.with_attachment(lash::direct::AttachmentSource::inline(
            lash::attachments::MediaType::parse("image/png").expect("workbench uploads only PNG"),
            stored.bytes,
        ));
    }
    Ok(input)
}

async fn run_user_turn_terminalized(
    state: AppState,
    request: WorkbenchTurnWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> HandlerResult<()> {
    let session_id = request.session_id.clone();
    let turn_id = request.turn_id.clone();
    terminalize_turn_execution(
        &state,
        &session_id,
        &turn_id,
        "restate_user_turn.failed",
        // Boxed: this future is within a few bytes of the large-future budget.
        AssertUnwindSafe(Box::pin(run_user_turn(state.clone(), request, controller)))
            .catch_unwind()
            .await,
    )
    .await
}

async fn run_button_trigger(
    state: AppState,
    request: WorkbenchButtonTriggerWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    state.set_selected_model(request.model.clone());
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "button-trigger:{}",
            request.operation_id
        )))
        // Audited: constructing this scope only validates the local runtime-operation id.
        .map_err(AppError::internal)?;
    let receipt = enqueue_button_trigger_command(
        &state,
        &request.session_id,
        request.button,
        &request.pressed_at,
        &request.operation_id,
        scoped_effect_controller,
    )
    .await
    // Audited: trigger delivery consumes per-subscription failures into its report, so this helper cannot return a typed session tombstone.
    .map_err(AppError::internal)?;
    state.trace_for_session(
        &request.session_id,
        "button_trigger.restate.trigger_occurrence",
        json!({
            "button": request.button,
            "occurrence_id": receipt.occurrence_id,
            "started_process_ids": receipt.started_process_ids(),
        }),
    );
    state.push_message_with_id_for_session(
        &request.session_id,
        format!("button-trigger:{}:event", request.operation_id),
        "event",
        "button trigger occurrence emitted",
    );
    // Trigger occurrence dispatch is the end of this client-initiated request.
    // Clear the UI's busy state when this request owns it, but do not clear a
    // foreground turn's busy state during a mid-turn occurrence.
    state.publish_trigger_dispatch_done(&request.session_id, &request.operation_id);
    Ok(())
}

async fn run_mail_received(
    state: AppState,
    request: WorkbenchMailReceivedWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    state.set_selected_model(request.model.clone());
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "mail-received:{}",
            request.operation_id
        )))
        // Audited: constructing this scope only validates the local runtime-operation id.
        .map_err(AppError::internal)?;
    let receipt = enqueue_mail_received_trigger_command(
        &state,
        &request.session_id,
        &request.delivery,
        &request.operation_id,
        scoped_effect_controller,
    )
    .await
    // Audited: trigger delivery consumes per-subscription failures into its report, so this helper cannot return a typed session tombstone.
    .map_err(AppError::internal)?;
    state.trace_for_session(
        &request.session_id,
        "mail_received.restate.trigger_occurrence",
        json!({
            "account": request.delivery.account,
            "title": request.delivery.title,
            "occurrence_id": receipt.occurrence_id,
            "started_process_ids": receipt.started_process_ids(),
        }),
    );
    state.push_message_with_id_for_session(
        &request.session_id,
        format!("mail-received:{}:event", request.operation_id),
        "event",
        "mail received trigger occurrence queued",
    );
    // Trigger occurrence dispatch is the end of this client-initiated request.
    // Clear the UI's busy state when this request owns it, but do not clear a
    // foreground turn's busy state during a mid-turn occurrence.
    state.publish_trigger_dispatch_done(&request.session_id, &request.operation_id);
    Ok(())
}

async fn run_session_delete(
    state: AppState,
    request: WorkbenchSessionDeleteWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    let active_turns = state.active_turns.for_session(&request.session_id);
    controller
        .revoke_await_events_for_session(&request.session_id)
        .await
        // Audited: Restate wait-index transport failures are untyped RuntimeError values with no store cause.
        .map_err(AppError::internal)?;
    if !active_turns.is_empty() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let still_active = state.active_turns.for_session(&request.session_id);
            if active_turns
                .iter()
                .all(|address| !still_active.contains(address))
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // Audited: this locally generated timeout observes only the in-process active-turn registry.
                return Err(AppError::internal(format!(
                    "timed out waiting for revoked turns to settle before deleting session `{}`",
                    request.session_id
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let scoped_effect_controller = controller
        .scoped_effect_controller(request.execution_scope)
        // Audited: constructing this scope only validates the supplied execution-scope fields.
        .map_err(AppError::internal)?;
    state
        .delete_session_and_reclaim_processes(&request.session_id, scoped_effect_controller)
        .await?;
    Ok(())
}

async fn run_process_cancel(
    state: AppState,
    request: WorkbenchProcessCancelWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "workbench-process-cancel:{}",
            request.process_id
        )))
        // Audited: constructing this scope only validates the local runtime-operation id.
        .map_err(AppError::internal)?;
    let summary = state
        .core
        .processes()
        .cancel(&request.process_id, scoped_effect_controller)
        .await
        // Audited: process cancellation uses the global process registry and never consults a session tombstone.
        .map_err(AppError::internal)?;
    state.trace_for_session(
        &request.session_id,
        "process.restate.cancel_requested",
        json!({
            "process_id": request.process_id,
            "summary": summary,
        }),
    );
    Ok(())
}

async fn run_queued_turn(
    state: AppState,
    request: WorkbenchQueuedTurnWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> Result<(), AppError> {
    let session = state
        .open_session(&request.session_id)
        .await
        .map_err(AppError::session_open)?;
    let selected_model = model_spec_from_selection(state.selected_model());
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    state.trace_for_session(
        &request.session_id,
        "queued_work.restate.start",
        json!({
            "reason": request.reason,
            "session_id": request.session_id,
            "turn_id": request.turn_id,
            "model": serde_json::to_value(&selected_model).unwrap_or(Value::Null),
        }),
    );
    let output = if request.batch_ids.is_empty() {
        request
            .queued_turn(&session)
            .effects(controller)
            .stream_to(&ui_events)
            .await
            .map_err(AppError::runtime)?
            .ran()
    } else {
        request
            .selected_queued_turn(&session)
            .effects(controller)
            .stream_to(&ui_events)
            .await
            .map_err(AppError::runtime)?
            .turn
    };
    let Some(output) = output else {
        state.trace_for_session(
            &request.session_id,
            "queued_work.restate.empty",
            json!({
                "reason": request.reason,
                "session_id": request.session_id,
                "turn_id": request.turn_id,
            }),
        );
        state.publish_turn_done(&request.session_id, &request.turn_id);
        return Ok(());
    };
    record_turn_output_for_model(
        &state,
        &session,
        &request.turn_id,
        output,
        turn_state,
        "restate_queued_turn.completed",
        Some(&selected_model.id),
    )
    .await?;
    Ok(())
}

async fn run_queued_turn_terminalized(
    state: AppState,
    request: WorkbenchQueuedTurnWorkflowRequest,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
) -> HandlerResult<()> {
    let session_id = request.session_id.clone();
    let turn_id = request.turn_id.clone();
    terminalize_turn_execution(
        &state,
        &session_id,
        &turn_id,
        "restate_queued_turn.failed",
        // Boxed: this future is within a few bytes of the large-future budget.
        AssertUnwindSafe(Box::pin(run_queued_turn(
            state.clone(),
            request,
            controller,
        )))
        .catch_unwind()
        .await,
    )
    .await
}

pub(crate) async fn terminalize_turn_execution(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    trace_name: &str,
    result: Result<Result<(), AppError>, Box<dyn std::any::Any + Send>>,
) -> HandlerResult<()> {
    match result {
        Ok(Ok(())) => {
            settle_workbench_turn(state, session_id, turn_id)
                .await
                .map_err(settlement_handler_error)?;
            Ok(())
        }
        Ok(Err(err)) => match err.verdict {
            AppErrorVerdict::Retryable => {
                state.trace_for_session(
                    session_id,
                    "turn.restate.retrying",
                    json!({
                        "operation": trace_name,
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "error": err.message,
                    }),
                );
                Err(HandlerError::from(err))
            }
            AppErrorVerdict::Terminal => {
                let message = err.message.clone();
                // Settlement is the durable boundary for this attempt. If it
                // fails, that failure necessarily masks the original turn error
                // because publishing either outcome before settlement would lie.
                settle_workbench_turn(state, session_id, turn_id)
                    .await
                    .map_err(settlement_handler_error)?;
                record_turn_failure(state, session_id, turn_id, trace_name, &message);
                Err(terminal_handler_error(err))
            }
            AppErrorVerdict::Ambiguous => {
                // Ambiguous turn failures terminate after their settlement.
                let message = err.message.clone();
                settle_workbench_turn(state, session_id, turn_id)
                    .await
                    .map_err(settlement_handler_error)?;
                record_turn_failure(state, session_id, turn_id, trace_name, &message);
                Err(terminal_handler_error(err))
            }
        },
        Err(payload) => {
            let message = panic_payload_message(payload);
            let message = format!("Restate-backed turn panicked: {message}");
            settle_workbench_turn(state, session_id, turn_id)
                .await
                .map_err(settlement_handler_error)?;
            record_turn_failure(state, session_id, turn_id, trace_name, &message);
            Err(TerminalError::new(message).into())
        }
    }
}

pub(crate) async fn settle_workbench_turn(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
) -> Result<(), AppError> {
    let session = state
        .open_session(session_id)
        .await
        .map_err(AppError::runtime)?;
    let targets = session
        .pending_turn_inputs()
        .await
        .map_err(AppError::runtime)?
        .into_iter()
        .filter(|input| input.ingress.active_turn_id() == Some(turn_id))
        .map(|input| lash::PendingTurnInputCancelTarget::input_id(input.input_id))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        state.active_turns.remove(session_id, turn_id);
        return Ok(());
    }
    let cancellations = session
        .cancel_pending_turn_inputs(targets)
        .await
        .map_err(AppError::runtime)?;
    state.trace_for_session(
        session_id,
        "turn_input.settle_cancelled",
        json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "cancellations": cancellations,
        }),
    );
    state.active_turns.remove(session_id, turn_id);
    Ok(())
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
pub(crate) async fn record_turn_output(
    state: &AppState,
    session: &lash::LashSession,
    turn_id: &str,
    output: lash::TurnReport,
    turn_state: Arc<Mutex<TurnStreamState>>,
    trace_name: &str,
) -> Result<(), AppError> {
    let selected_model = state.selected_model();
    record_turn_output_for_model(
        state,
        session,
        turn_id,
        output,
        turn_state,
        trace_name,
        Some(&selected_model.model),
    )
    .await
}

async fn record_turn_output_for_model(
    state: &AppState,
    session: &lash::LashSession,
    turn_id: &str,
    output: lash::TurnReport,
    turn_state: Arc<Mutex<TurnStreamState>>,
    trace_name: &str,
    model: Option<&str>,
) -> Result<(), AppError> {
    let streamed_prose = {
        let mut turn_state = turn_state.lock_recover();
        let streamed_prose = turn_state.assistant_prose();
        turn_state.settle_terminal();
        streamed_prose
    };
    let assistant_text = assistant_text_for_display(&output, &streamed_prose);
    state.trace_for_session(
        &session.session_id(),
        trace_name,
        json!({
            "assistant_text": assistant_text.clone(),
            "streamed_prose": streamed_prose,
            "outcome": &output.outcome,
            "errors": &output.errors,
            "final_value": output.final_value().cloned(),
            "tool_value": output.tool_value().map(|(tool_name, value)| {
                json!({
                    "tool_name": tool_name,
                    "value": value,
                })
            }),
        }),
    );
    // Active-turn ingress is now an ordinary committed user message. Publish
    // the exact committed graph projection so the live page replaces the
    // ingress receipt with the same message that `/api/state` and resume read.
    // Re-publishing earlier ingress messages is harmless because the browser
    // deduplicates committed message ids.
    for message in session
        .read_view()
        .messages()
        .iter()
        .filter(|message| message.id.starts_with("m_ingress_"))
    {
        state.publish_for_session_identified(
            &session.session_id(),
            format!("message:{}", message.id),
            crate::StreamItem::Message {
                message: crate::chat_message_from_committed(message),
            },
        );
    }
    for record in output.llm_calls.iter().cloned() {
        let call_id = record.call_id.0.clone();
        let remote_record: lash::remote::llm::RemoteLlmCallRecord = record.into();
        state.publish_for_session_identified(
            &session.session_id(),
            format!("turn:{turn_id}:model-call:{call_id}"),
            crate::StreamItem::ModelCallRecorded {
                record: remote_record,
            },
        );
    }
    match &output.outcome {
        lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { evidence }) => {
            let message = format!("turn stopped · request {}", evidence.request_id);
            state.push_message_with_id_for_session(
                &session.session_id(),
                format!("turn:{turn_id}:cancelled"),
                "event",
                message,
            );
        }
        lash::TurnOutcome::Stopped(stop) => {
            let _ = stop;
            state.push_message_with_id_for_session(
                &session.session_id(),
                format!("turn:{turn_id}:failed"),
                "event",
                crate::PUBLIC_TURN_FAILURE_MESSAGE,
            );
        }
        _ => {
            if workbench_owns_committed_agent_reply(&output) {
                commit_assistant_transcript(session, turn_id, assistant_text.clone(), model)
                    .await?;
            }
            state.push_message_with_id_for_session(
                &session.session_id(),
                workbench_turn_assistant_message_id(turn_id),
                "assistant",
                assistant_text,
            );
        }
    }
    state.publish_turn_done(&session.session_id(), turn_id);
    Ok(())
}

fn record_turn_failure(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    trace_name: &str,
    message: &str,
) {
    state.trace_for_session(
        session_id,
        trace_name,
        json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "error": message,
        }),
    );
    state.publish_turn_failed(session_id, turn_id);
}

include!("restate_cron_sync.rs");

/// Idempotency key for one cron tick's trigger occurrence. Must be unique
/// per (job, tick): `fired_at` is the journaled fire time, so retries of the
/// same tick dedupe while the next tick gets a fresh occurrence. (A key
/// without the tick component kills the schedule: the second tick conflicts,
/// the handler fails before re-arming, and the chain stops.)
fn cron_occurrence_key(job_key: &str, fired_at: &str) -> String {
    format!("workbench-cron:{job_key}:{fired_at}")
}

async fn emit_cron_occurrence(
    state: AppState,
    request: WorkbenchCronRequest,
    fired_at: String,
    controller: &lash_restate::RestateRuntimeEffectController<'_, ObjectContext<'_>>,
) -> HandlerResult<Json<CronEmitReport>> {
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "cron:{}:{fired_at}",
            controller.context().key()
        )))
        .map_err(|err| HandlerError::from(TerminalError::new(err.to_string())))?;
    emit_cron_occurrence_with_effect_controller(
        state,
        request,
        fired_at,
        controller.context().key(),
        scoped_effect_controller,
    )
    .await
}

async fn emit_cron_occurrence_with_effect_controller(
    state: AppState,
    request: WorkbenchCronRequest,
    fired_at: String,
    job_key: &str,
    scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
) -> HandlerResult<Json<CronEmitReport>> {
    let report = state
        .core
        .triggers()
        .emit(
            lash::triggers::TriggerOccurrenceRequest::new(
                CRON_SCHEDULE_SOURCE_TYPE,
                request.source_key.clone(),
                json!({
                    "fired_at": fired_at,
                }),
                cron_occurrence_key(job_key, &fired_at),
            )
            .with_source(json!({
                "expr": request.expr,
                "tz": request.tz,
            })),
            scoped_effect_controller,
        )
        .await
        .map_err(classified_embed_handler_error)?;
    Ok(Json(CronEmitReport {
        started_process_ids: report.started_process_ids(),
    }))
}

async fn schedule_next(
    ctx: &ObjectContext<'_>,
    request: WorkbenchCronRequest,
    now: DateTime<Utc>,
    last_fired_at: Option<String>,
) -> HandlerResult<WorkbenchCronState> {
    let next = next_cron_time(&request.expr, request.tz.as_deref(), now)
        .map_err(|err| HandlerError::from(TerminalError::new(err)))?;
    let delay = next
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(0));
    let handle = ctx
        .object_client::<WorkbenchCronJobClient>(ctx.key())
        .run()
        .send_after(delay);
    let next_execution_id = handle.invocation_id().await?;
    let state = WorkbenchCronState {
        request,
        next_execution_time: next.to_rfc3339(),
        next_execution_id,
        last_fired_at,
    };
    ctx.set(CRON_STATE_KEY, Json(state.clone()));
    Ok(state)
}

async fn cancel_stored_execution(ctx: &ObjectContext<'_>) -> HandlerResult<()> {
    if let Some(Json(existing)) = ctx.get::<Json<WorkbenchCronState>>(CRON_STATE_KEY).await? {
        let _ = ctx
            .invocation_handle(existing.next_execution_id)
            .cancel()
            .await;
    }
    Ok(())
}

async fn journaled_now(
    ctx: &ObjectContext<'_>,
    name: &'static str,
) -> HandlerResult<DateTime<Utc>> {
    let now = ctx
        .run(|| async { Ok::<_, HandlerError>(Utc::now().to_rfc3339()) })
        .name(name)
        .await?;
    DateTime::parse_from_rfc3339(&now)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|err| TerminalError::new(err.to_string()).into())
}

async fn journaled_workbench_trace(
    ctx: &ObjectContext<'_>,
    state: AppState,
    session_id: String,
    name: &'static str,
    payload: Value,
    effect_name: &'static str,
) -> HandlerResult<()> {
    ctx.run(move || {
        let state = state.clone();
        let session_id = session_id.clone();
        let payload = payload.clone();
        async move {
            state.trace_for_session(&session_id, name, payload);
            Ok::<(), HandlerError>(())
        }
    })
    .name(effect_name)
    .await?;
    Ok(())
}

fn next_cron_time(
    expr: &str,
    tz: Option<&str>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let timezone: Tz = tz
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("UTC")
        .parse()
        .map_err(|err| format!("invalid timezone: {err}"))?;
    let cron = CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expr)
        .map_err(|err| format!("invalid cron expression `{expr}`: {err}"))?;
    let zoned_now = now.with_timezone(&timezone);
    cron.find_next_occurrence(&zoned_now, false)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|err| format!("cron expression `{expr}` has no next occurrence: {err}"))
}

fn cron_request_from_registration(
    session_id: &str,
    registration: &lash::triggers::TriggerRegistration,
) -> Result<(String, WorkbenchCronRequest), String> {
    let source_type = registration.source_type.as_str();
    if source_type != CRON_SCHEDULE_SOURCE_TYPE {
        return Err(format!("unexpected source type `{source_type}`"));
    }
    let source =
        lashlang::HostDescriptor::decode(&registration.source).map_err(|err| err.to_string())?;
    if source.source_type != source_type {
        return Err(format!(
            "registration source type `{source_type}` does not match host descriptor `{}`",
            source.source_type
        ));
    }
    let payload: CronScheduleSource = source
        .decode_as(&crate::workbench_lashlang_resources())
        .map_err(|err| err.to_string())?;
    let request = WorkbenchCronRequest {
        session_id: session_id.to_string(),
        source_key: registration.source_key.clone(),
        expr: payload.expr,
        tz: payload.tz,
        name: registration.name.clone(),
    };
    Ok((cron_job_key(session_id, &registration.source_key), request))
}

fn cron_job_key(session_id: &str, source_key: &str) -> String {
    format!("{session_id}:{source_key}")
}

fn terminal_handler_error(err: AppError) -> HandlerError {
    TerminalError::new(err.message).into()
}

fn settlement_handler_error(err: AppError) -> HandlerError {
    match err.verdict {
        AppErrorVerdict::Retryable => HandlerError::from(err),
        AppErrorVerdict::Terminal => terminal_handler_error(err),
        AppErrorVerdict::Ambiguous => {
            // Ambiguous settlement failures remain retryable.
            HandlerError::from(err)
        }
    }
}

fn classified_embed_handler_error(error: lash::EmbedError) -> HandlerError {
    settlement_handler_error(AppError::runtime(error))
}

fn classified_plugin_handler_error(error: lash::plugins::PluginError) -> HandlerError {
    classified_embed_handler_error(lash::EmbedError::Plugin(error))
}

#[cfg(test)]
#[path = "restate_tests.rs"]
mod tests;

#[async_trait::async_trait]
trait QueuedWorkExt {
    async fn drain_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError>;
}

#[async_trait::async_trait]
impl QueuedWorkExt for lash::runtime::NativeQueuedWork {
    async fn drain_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError> {
        use lash::runtime::QueuedWorkSubstrate as _;

        self.drain_session_work(
            lash::runtime::SessionWorkTarget::Session(session_id.to_string()),
            reason,
        )
        .await
        .map(|_| ())
    }
}
