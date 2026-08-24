use lash::sync::MutexExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use lash::observe::{RemoteSessionObservationStreamItem, SessionCursor};
use lash::rlm::RlmTurnBuilderExt as _;
use lash::{
    LashSession, TurnActivity, TurnActivitySink, TurnCancelOutcome, TurnCancelRequest, TurnEvent,
    TurnInput, TurnOutput,
};
use lash_remote_protocol::{
    Envelope, RemoteLiveReplayGap, RemoteSessionCursor, RemoteSessionObservation,
    RemoteSessionObservationEvent, RemoteSessionObservationEventPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use crate::board::BoardState;
use crate::db::{ChatBranchPoint, ChatMessage, ChatModelSelection, ChatSummary};
#[cfg(feature = "restate")]
use crate::restate::send_message_restate;
#[cfg(feature = "restate")]
use crate::state::AgentServiceDurability;
use crate::state::{AppError, AppResult, AppStateData};
use crate::ui::INDEX_HTML;

const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateChatRequest {
    title: Option<String>,
    model: Option<String>,
    model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateChatModelRequest {
    model: String,
    model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendMessageRequest {
    text: String,
    board: BoardState,
    model: Option<String>,
    model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CancelTurnRequest {
    request_id: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForkChatRequest {
    node_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CancelTurnResponse {
    session_id: String,
    turn_id: String,
    outcome: TurnCancelOutcome,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AppSettings {
    default_model: String,
    default_model_variant: Option<String>,
    model_variants: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamItem {
    Observation {
        event: Box<Envelope<RemoteSessionObservationEvent>>,
    },
    ReplayCursor {
        cursor: String,
    },
    ReplayGap {
        observation: Box<Envelope<RemoteSessionObservation>>,
        gap: Box<Envelope<RemoteLiveReplayGap>>,
    },
    Message {
        message: ChatMessage,
    },
    Error {
        message: String,
    },
    Done,
}

impl StreamItem {
    pub(crate) fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }
}

pub(crate) async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) async fn list_chats(
    State(state): State<AppStateData>,
) -> AppResult<Json<Vec<ChatSummary>>> {
    state.with_db(|db| db.list_chats()).await.map(Json)
}

pub(crate) async fn settings(State(state): State<AppStateData>) -> Json<AppSettings> {
    Json(AppSettings {
        default_model: state.default_model().to_string(),
        default_model_variant: state.default_model_variant().map(str::to_string),
        model_variants: vec!["low", "medium", "high"],
    })
}

pub(crate) async fn create_chat(
    State(state): State<AppStateData>,
    Json(request): Json<CreateChatRequest>,
) -> AppResult<Json<ChatSummary>> {
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("New chat");
    let title = title.to_string();
    let selection = normalize_model_selection(
        request.model.as_deref(),
        request.model_variant.as_deref(),
        state.default_model(),
        state.default_model_variant(),
    )?;
    state
        .with_db(move |db| {
            db.create_chat(&title, &selection.model, selection.model_variant.as_deref())
        })
        .await
        .map(Json)
}

pub(crate) async fn update_chat_model(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    Json(request): Json<UpdateChatModelRequest>,
) -> AppResult<Json<ChatSummary>> {
    let selection =
        normalize_optional_model_selection(Some(&request.model), request.model_variant.as_deref())?
            .ok_or_else(|| AppError::bad_request("model is required"))?;
    state
        .with_db(move |db| {
            db.update_chat_model(
                &chat_id,
                &selection.model,
                selection.model_variant.as_deref(),
            )
        })
        .await
        .map(Json)
}

pub(crate) async fn list_messages(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
) -> AppResult<Json<Vec<ChatMessage>>> {
    state
        .with_db(move |db| {
            db.require_chat(&chat_id)?;
            db.list_messages(&chat_id)
        })
        .await
        .map(Json)
}

/// Debug read of the app-owned board: the same authoritative state the board
/// tools operate on, so an external harness can verify the UI against the
/// backend instead of trusting either alone.
pub(crate) async fn chat_board(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
) -> AppResult<Json<serde_json::Value>> {
    state
        .with_db(move |db| {
            db.require_chat(&chat_id)?;
            let board = db.chat_board(&chat_id)?;
            Ok(crate::board::board_snapshot(&board))
        })
        .await
        .map(Json)
}

pub(crate) async fn list_chat_branch_points(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
) -> AppResult<Json<Vec<ChatBranchPoint>>> {
    state
        .with_db(move |db| db.list_branch_points(&chat_id))
        .await
        .map(Json)
}

pub(crate) async fn pin_chat_branch_point(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
) -> AppResult<Json<ChatBranchPoint>> {
    let selection = state
        .with_db({
            let chat_id = chat_id.clone();
            move |db| db.chat_model_selection(&chat_id)
        })
        .await?;
    let session = state
        .open_session(&chat_id, model_spec_for_chat_selection(&selection)?)
        .await?;
    let snapshot = session.admin().state().export().await;
    let node_id = snapshot
        .session_graph
        .leaf_node_id
        .clone()
        .ok_or_else(|| AppError::bad_request("the chat has no completed turn to pin"))?;
    state.core().pin(&node_id).await.map_err(branch_error)?;
    state
        .with_db(move |db| db.save_branch_point(&chat_id, &node_id))
        .await
        .map(Json)
}

pub(crate) async fn fork_chat(
    State(state): State<AppStateData>,
    AxumPath(source_chat_id): AxumPath<String>,
    Json(request): Json<ForkChatRequest>,
) -> AppResult<Json<ChatSummary>> {
    let node_id = request.node_id.trim().to_string();
    if node_id.is_empty() {
        return Err(AppError::bad_request("branch point is required"));
    }
    let target_chat_id = uuid::Uuid::new_v4().to_string();
    state
        .with_db({
            let source_chat_id = source_chat_id.clone();
            let node_id = node_id.clone();
            let target_chat_id = target_chat_id.clone();
            move |db| db.prepare_chat_fork(&source_chat_id, &node_id, &target_chat_id)
        })
        .await?;
    if let Err(error) = state.core().fork_at(node_id, target_chat_id.clone()).await {
        let _ = state
            .with_db(move |db| db.delete_chat(&target_chat_id))
            .await;
        return Err(branch_error(error));
    }
    let chat = match state
        .with_db({
            let target_chat_id = target_chat_id.clone();
            move |db| db.finish_chat_fork(&target_chat_id)
        })
        .await
    {
        Ok(chat) => chat,
        Err(error) => {
            state.discard_pending_chat_fork(&target_chat_id).await?;
            return Err(error);
        }
    };
    Ok(Json(chat))
}

pub(crate) async fn send_message(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    Json(request): Json<SendMessageRequest>,
) -> AppResult<Response> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let request_model = normalize_optional_model_selection(
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;

    // The board is app-owned state. The user message keeps a snapshot for UI
    // replay, while tools read and mutate the canonical copy in the database.
    let user_board = request.board.clone();
    let (user_message, model_selection) = state
        .with_db({
            let chat_id = chat_id.clone();
            let text = text.clone();
            let user_board = user_board.clone();
            move |db| {
                db.require_chat(&chat_id)?;
                if let Some(selection) = request_model {
                    db.update_chat_model(
                        &chat_id,
                        &selection.model,
                        selection.model_variant.as_deref(),
                    )?;
                }
                let model_selection = db.chat_model_selection(&chat_id)?;
                db.maybe_title_from_first_message(&chat_id, &text)?;
                db.upsert_chat_board(&chat_id, &user_board)?;
                let message = db.insert_message_with_payload(
                    &chat_id,
                    "user",
                    &text,
                    Some(json!({ "board": user_board })),
                )?;
                Ok((message, model_selection))
            }
        })
        .await?;

    #[cfg(feature = "restate")]
    if state.durability() == AgentServiceDurability::Restate {
        return send_message_restate(state, chat_id, text, user_message, model_selection).await;
    }

    let turn_model = model_spec_for_chat_selection(&model_selection)?;
    let session = state.open_session(&chat_id, turn_model).await?;
    let replay_cursor = session.observe().current_observation().cursor;
    let turn_id = format!("agent-service-local-turn:{}", uuid::Uuid::new_v4());
    let (tx, rx) = mpsc::channel::<StreamItem>(64);
    let mut replay = spawn_live_replay_forwarder(session.clone(), replay_cursor, tx.clone());
    let run_state = state.clone();
    let task_turn_id = turn_id.clone();
    tokio::spawn(async move {
        let _ = tx
            .send(StreamItem::Message {
                message: user_message,
            })
            .await;
        let turn_state = Arc::new(Mutex::new(TurnPersistenceState::default()));
        let ui_events = ChannelTurnEvents::persistence(
            run_state.clone(),
            chat_id.clone(),
            Arc::clone(&turn_state),
        );
        let turn = session
            .turn(TurnInput::text(text))
            .turn_id(task_turn_id)
            .require_finish();
        let turn = match turn {
            Ok(turn) => turn.stream_to(&ui_events).await.map(|result| TurnOutput {
                result,
                activities: Vec::new(),
            }),
            Err(err) => Err(err),
        };
        match turn {
            Ok(output) => {
                let assistant_text = assistant_text_for_persistence(
                    &output,
                    turn_state.lock_recover().assistant_prose(),
                );
                let inserted = run_state
                    .with_db({
                        let chat_id = chat_id.clone();
                        move |db| db.insert_message(&chat_id, "assistant", &assistant_text)
                    })
                    .await;
                match inserted {
                    Ok(message) => {
                        let _ = tx.send(StreamItem::Message { message }).await;
                    }
                    Err(err) => {
                        let _ = tx
                            .send(StreamItem::Error {
                                message: err.message,
                            })
                            .await;
                    }
                }
                wait_for_live_replay_flush(&mut replay).await;
            }
            Err(err) => {
                replay.abort();
                let _ = tx
                    .send(StreamItem::Error {
                        message: err.to_string(),
                    })
                    .await;
            }
        }
        let _ = tx.send(StreamItem::Done).await;
    });

    let stream = ReceiverStream::new(rx).map(|item| {
        let mut line = serde_json::to_string(&item).unwrap_or_else(|err| {
            json!({
                "type": "error",
                "message": err.to_string(),
            })
            .to_string()
        });
        line.push('\n');
        Ok::<Bytes, Infallible>(Bytes::from(line))
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-lash-turn-id", turn_id)
        .body(Body::from_stream(stream))
        .expect("valid streaming response"))
}

/// Request cooperative cancellation of one exact foreground turn.
///
/// The ids route the request; deployments exposing this endpoint beyond the
/// local demo must authenticate the caller and authorize access to the chat.
pub(crate) async fn cancel_turn(
    State(state): State<AppStateData>,
    AxumPath((chat_id, turn_id)): AxumPath<(String, String)>,
    Json(request): Json<CancelTurnRequest>,
) -> AppResult<Json<CancelTurnResponse>> {
    state
        .with_db({
            let chat_id = chat_id.clone();
            move |db| db.require_chat(&chat_id).map(|_| ())
        })
        .await?;
    let request_id = request
        .request_id
        .filter(|request_id| !request_id.trim().is_empty())
        .unwrap_or_else(|| format!("agent-service-cancel:{}", uuid::Uuid::new_v4()));
    let mut cancel = TurnCancelRequest::new(
        lash::TurnAddress::new(&chat_id, &turn_id),
        request_id,
        Some("user".to_string()),
    );
    cancel.reason = request.reason;
    let receipt = state
        .turn_work_driver()
        .request_cancel(cancel)
        .await
        .map_err(|err| AppError::internal(err.to_string()))?;
    Ok(Json(CancelTurnResponse {
        session_id: chat_id,
        turn_id,
        outcome: receipt.outcome,
    }))
}

pub(crate) struct ChannelTurnEvents {
    state: AppStateData,
    chat_id: String,
    turn_id: Option<String>,
    turn_state: Arc<Mutex<TurnPersistenceState>>,
}

#[derive(Default)]
pub(crate) struct TurnPersistenceState {
    reasoning: Option<(i64, String)>,
    assistant_prose: String,
    code: Option<String>,
    code_message: Option<i64>,
    tools: HashMap<String, i64>,
}

impl TurnPersistenceState {
    pub(crate) fn assistant_prose(&self) -> &str {
        &self.assistant_prose
    }
}

impl ChannelTurnEvents {
    pub(crate) fn persistence(
        state: AppStateData,
        chat_id: String,
        turn_state: Arc<Mutex<TurnPersistenceState>>,
    ) -> Self {
        Self {
            state,
            chat_id,
            turn_id: None,
            turn_state,
        }
    }

    #[cfg(feature = "restate")]
    pub(crate) fn outbox(
        state: AppStateData,
        chat_id: String,
        turn_id: String,
        turn_state: Arc<Mutex<TurnPersistenceState>>,
    ) -> Self {
        Self {
            state,
            chat_id,
            turn_id: Some(turn_id),
            turn_state,
        }
    }

    async fn emit_error(&self, message: String) {
        if let Some(turn_id) = self.turn_id.clone() {
            let item = StreamItem::Error { message };
            let _ = self
                .state
                .with_db(move |db| {
                    let is_done = item.is_done();
                    db.insert_turn_event(&turn_id, &item, is_done)
                })
                .await;
        }
    }

    async fn handle(&self, activity: TurnActivity) {
        let event = &activity.event;
        if let TurnEvent::AssistantProseDelta { text } = &event {
            self.turn_state
                .lock_recover()
                .assistant_prose
                .push_str(text);
            return;
        }
        // Keep persisted message order tied to event start order. The browser
        // only renders completed code/tool rows, but reload should still
        // reconstruct "thinking -> lashlang -> tools -> assistant".
        if let TurnEvent::ReasoningDelta { text } = &event {
            let update = {
                let mut state = self.turn_state.lock_recover();
                match state.reasoning.as_mut() {
                    Some((id, existing)) => {
                        existing.push_str(text);
                        Some((*id, existing.clone(), false))
                    }
                    None => Some((0, text.to_string(), true)),
                }
            };
            if let Some((id, reasoning, insert)) = update {
                let result = if insert {
                    self.state
                        .with_db({
                            let chat_id = self.chat_id.clone();
                            let reasoning = reasoning.clone();
                            move |db| db.insert_reasoning(&chat_id, &reasoning)
                        })
                        .await
                        .map(|message| {
                            self.turn_state.lock_recover().reasoning =
                                Some((message.id, reasoning));
                        })
                } else {
                    self.state
                        .with_db({
                            let reasoning = reasoning.clone();
                            move |db| db.update_reasoning(id, &reasoning)
                        })
                        .await
                        .map(|_| ())
                };
                if let Err(err) = result {
                    self.emit_error(err.message).await;
                }
            }
            return;
        }
        if let TurnEvent::CodeBlockStarted { code, .. } = &event {
            self.turn_state.lock_recover().code = Some(code.clone());
            match self
                .state
                .with_db({
                    let chat_id = self.chat_id.clone();
                    let event = event.clone();
                    let code = code.clone();
                    move |db| db.insert_code_block(&chat_id, event, Some(code))
                })
                .await
            {
                Ok(message) => {
                    self.turn_state.lock_recover().code_message = Some(message.id);
                }
                Err(err) => {
                    self.emit_error(err.message).await;
                }
            }
            return;
        }
        if matches!(&event, TurnEvent::ToolCallStarted { .. }) {
            match self
                .state
                .with_db({
                    let chat_id = self.chat_id.clone();
                    let event = event.clone();
                    move |db| db.insert_tool_call(&chat_id, event)
                })
                .await
            {
                Ok(message) => {
                    self.turn_state
                        .lock_recover()
                        .tools
                        .insert(activity.correlation_id.0.to_string(), message.id);
                }
                Err(err) => {
                    self.emit_error(err.message).await;
                }
            }
            return;
        }
        if matches!(&event, TurnEvent::ToolCallCompleted { .. }) {
            let existing = self
                .turn_state
                .lock_recover()
                .tools
                .remove(activity.correlation_id.0.as_ref());
            let result = self
                .state
                .with_db({
                    let chat_id = self.chat_id.clone();
                    let event = event.clone();
                    move |db| {
                        if let Some(id) = existing {
                            db.update_tool_call(id, event)
                        } else {
                            db.insert_tool_call(&chat_id, event)
                        }
                    }
                })
                .await;
            if let Err(err) = result {
                self.emit_error(err.message).await;
            }
            return;
        }
        if matches!(&event, TurnEvent::CodeBlockCompleted { .. }) {
            let (code, existing) = {
                let mut state = self.turn_state.lock_recover();
                (state.code.take(), state.code_message.take())
            };
            let result = self
                .state
                .with_db({
                    let chat_id = self.chat_id.clone();
                    let event = event.clone();
                    move |db| {
                        if let Some(id) = existing {
                            db.update_code_block(id, event, code)
                        } else {
                            db.insert_code_block(&chat_id, event, code)
                        }
                    }
                })
                .await;
            if let Err(err) = result {
                self.emit_error(err.message).await;
            }
        }
    }
}

#[async_trait]
impl TurnActivitySink for ChannelTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.handle(activity).await;
    }
}

pub(crate) fn spawn_live_replay_forwarder(
    session: LashSession,
    cursor: SessionCursor,
    tx: mpsc::Sender<StreamItem>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        forward_live_replay_until_commit(session, cursor, tx).await;
    })
}

pub(crate) async fn wait_for_live_replay_flush(replay: &mut JoinHandle<()>) {
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut *replay)
        .await
        .is_err()
    {
        replay.abort();
    }
}

async fn forward_live_replay_until_commit(
    session: LashSession,
    cursor: SessionCursor,
    tx: mpsc::Sender<StreamItem>,
) {
    if tx
        .send(StreamItem::ReplayCursor {
            cursor: cursor.to_string(),
        })
        .await
        .is_err()
    {
        return;
    }
    let observable = session.observe();
    let mut subscription = match observable
        .subscribe_and_recover_remote(RemoteSessionCursor::new(cursor.to_string()))
    {
        Ok(subscription) => subscription,
        Err(err) => {
            let _ = tx
                .send(StreamItem::Error {
                    message: err.to_string(),
                })
                .await;
            return;
        }
    };
    loop {
        let item = match subscription.next().await {
            Some(Ok(item)) => item,
            Some(Err(err)) => {
                let _ = tx
                    .send(StreamItem::Error {
                        message: err.to_string(),
                    })
                    .await;
                break;
            }
            None => break,
        };
        match item {
            RemoteSessionObservationStreamItem::Event(event) => {
                let committed = matches!(
                    &event.event,
                    RemoteSessionObservationEventPayload::Committed
                );
                if tx
                    .send(StreamItem::Observation {
                        event: Box::new(Envelope::new(event)),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if committed {
                    break;
                }
            }
            RemoteSessionObservationStreamItem::Gap { observation, gap } => {
                let _ = tx
                    .send(StreamItem::ReplayGap {
                        observation: Box::new(Envelope::new(observation)),
                        gap: Box::new(Envelope::new(gap)),
                    })
                    .await;
            }
        }
    }
}

fn normalize_model_selection(
    model: Option<&str>,
    model_variant: Option<&str>,
    default_model: &str,
    default_model_variant: Option<&str>,
) -> AppResult<ChatModelSelection> {
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(default_model)
        .to_string();
    let model_variant = normalize_model_variant(model_variant).or_else(|| {
        default_model_variant
            .map(str::trim)
            .filter(|variant| !variant.is_empty())
            .map(str::to_string)
    });
    if model.trim().is_empty() {
        return Err(AppError::bad_request("model is required"));
    }
    Ok(ChatModelSelection {
        model,
        model_variant,
    })
}

fn normalize_optional_model_selection(
    model: Option<&str>,
    model_variant: Option<&str>,
) -> AppResult<Option<ChatModelSelection>> {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(ChatModelSelection {
        model: model.to_string(),
        model_variant: normalize_model_variant(model_variant),
    }))
}

pub(crate) fn model_spec_for_chat_selection(
    selection: &ChatModelSelection,
) -> AppResult<lash::ModelSpec> {
    lash::ModelSpec::builder(selection.model.clone())
        .variant(
            selection
                .model_variant
                .clone()
                .map(lash::provider::ReasoningSelection::Effort)
                .unwrap_or_default(),
        )
        .context_window_tokens(DEFAULT_CONTEXT_WINDOW_TOKENS)
        .build()
        .map(crate::default_openrouter_model_capability_for)
        .map_err(|error| AppError::bad_request(error.to_string()))
}

fn normalize_model_variant(model_variant: Option<&str>) -> Option<String> {
    model_variant
        .map(str::trim)
        .filter(|variant| !variant.is_empty())
        .map(str::to_string)
}

fn branch_error(error: lash::EmbedError) -> AppError {
    if matches!(
        &error,
        lash::EmbedError::Store(lash::persistence::StoreError::ForkPointNotRetained { .. })
    ) {
        return AppError {
            status: StatusCode::CONFLICT,
            message: error.to_string(),
        };
    }
    AppError::internal(error.to_string())
}

pub(crate) fn assistant_text_for_persistence(output: &TurnOutput, streamed_prose: &str) -> String {
    if let Some(value) = output.final_value() {
        return terminal_value_text(value);
    }
    if let Some((_tool_name, value)) = output.tool_value() {
        return terminal_value_text(value);
    }
    output
        .assistant_message()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(streamed_prose)
        .to_string()
}

fn terminal_value_text(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

#[cfg(all(test, feature = "restate"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::to_bytes;
    use lash::LashCore;
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;

    use super::*;
    use crate::db::AppDb;
    use crate::state::AgentServiceDurability;

    #[tokio::test]
    async fn message_route_streams_session_observations_with_mock_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-route-mock")
            .complete(|_request| async {
                let text = r#"<lashlang>
finish "done through route"
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
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                    .await
                    .expect("artifact store"),
            ),
        );
        let core = LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
            .provider(provider)
            .model(
                lash::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.join("lash-sessions"),
            )))
            .effect_host(Arc::new(
                lash::durability::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
                    .await
                    .expect("process env store"),
            ))
            .trigger_store(Arc::new(
                lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                    .await
                    .expect("trigger store"),
            ))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-test",
                "test",
            ))
            .expect("core");
        let turn_work_driver = core.turn_work_driver();
        let db = Arc::new(Mutex::new(
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
        ));
        let state = AppStateData::from_shared_db(
            core,
            turn_work_driver,
            Arc::clone(&db),
            "mock-model".to_string(),
            None,
            AgentServiceDurability::Local,
            // The scripted provider answers with a `<lashlang>` cell, so this
            // fixture is Lashlang by construction rather than by omission.
            lash::rlm::RlmDialect::Lashlang,
            None,
        );
        let chat = state
            .with_db(|db| db.create_chat("route replay", "mock-model", None))
            .await
            .expect("create chat");
        // Boxed: the handler's future is large enough that holding it inline
        // in a test frame trips `clippy::large_futures` under the `restate`
        // feature, where this target is only ever built.
        let response = Box::pin(send_message(
            State(state.clone()),
            AxumPath(chat.id.clone()),
            Json(SendMessageRequest {
                text: "exercise live replay".to_string(),
                board: crate::board::default_board(),
                model: None,
                model_variant: Default::default(),
            }),
        ))
        .await
        .expect("send message");
        let turn_id = response
            .headers()
            .get("x-lash-turn-id")
            .expect("turn id response header")
            .to_str()
            .expect("turn id header text")
            .to_string();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let lines = std::str::from_utf8(&body)
            .expect("utf8")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .collect::<Vec<_>>();

        assert!(
            lines
                .iter()
                .any(|line| line.get("type").and_then(serde_json::Value::as_str)
                    == Some("replay_cursor")),
            "stream should expose an opaque live replay cursor: {lines:#?}"
        );
        assert!(
            lines.iter().all(|line| {
                line.get("type").and_then(serde_json::Value::as_str) != Some("event")
            }),
            "stream should not expose legacy direct turn events: {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| {
                line.get("type").and_then(serde_json::Value::as_str) == Some("observation")
                    && line
                        .pointer("/event/type")
                        .and_then(serde_json::Value::as_str)
                        == Some("turn_activity")
                    && line
                        .pointer("/event/activity/type")
                        .and_then(serde_json::Value::as_str)
                        == Some("final_value")
            }),
            "stream should contain remote observation turn activity: {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| {
                line.get("type").and_then(serde_json::Value::as_str) == Some("message")
                    && line
                        .pointer("/message/role")
                        .and_then(serde_json::Value::as_str)
                        == Some("assistant")
                    && line
                        .pointer("/message/text")
                        .and_then(serde_json::Value::as_str)
                        == Some("done through route")
            }),
            "stream should include the persisted assistant message: {lines:#?}"
        );
        let cancelled = cancel_turn(
            State(state),
            AxumPath((chat.id, turn_id)),
            Json(CancelTurnRequest {
                request_id: Some("route-test-stop".to_string()),
                reason: Some("test completed turn".to_string()),
            }),
        )
        .await
        .expect("cancel endpoint");
        assert!(matches!(
            cancelled.0.outcome,
            TurnCancelOutcome::CompletionWonRace
        ));
    }

    #[test]
    fn replay_gap_stream_item_uses_remote_gap_payload() {
        let item = StreamItem::ReplayGap {
            observation: Box::new(Envelope::new(RemoteSessionObservation {
                // Standalone stream payloads carry one shared protocol envelope.
                session_id: "session-1".to_string(),
                cursor: "cursor-after".to_string(),
                turn_index: 3,
                usage: lash_remote_protocol::RemoteUsage::default(),
            })),
            gap: Box::new(Envelope::new(RemoteLiveReplayGap {
                // Nested DTOs remain bare inside that envelope body.
                session_id: "session-1".to_string(),
                requested_cursor: "cursor-before".to_string(),
                latest_cursor: "cursor-after".to_string(),
                latest_revision: 7,
                reason: lash_remote_protocol::RemoteLiveReplayGapReason::Trimmed,
            })),
        };
        let value = serde_json::to_value(item).expect("json");

        assert_eq!(value.pointer("/type"), Some(&json!("replay_gap")));
        assert_eq!(
            value.pointer("/gap/requested_cursor"),
            Some(&json!("cursor-before"))
        );
        assert_eq!(
            value.pointer("/gap/latest_cursor"),
            Some(&json!("cursor-after"))
        );
        assert_eq!(
            value.pointer("/observation/cursor"),
            Some(&json!("cursor-after"))
        );
        assert_eq!(
            value.pointer("/observation/session_id"),
            Some(&json!("session-1"))
        );
        assert_eq!(value.pointer("/gap/latest_revision"), Some(&json!(7)));
        assert_eq!(value.pointer("/gap/reason"), Some(&json!("trimmed")));
    }
}
