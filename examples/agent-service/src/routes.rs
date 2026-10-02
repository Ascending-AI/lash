use lash::SessionId;
use lash::TurnId;
use lash::sync::MutexExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use lash::observe::{RemoteSessionObservationStreamItem, SessionCursor};
use lash::rlm::RlmSendBuilderExt as _;
use lash::{
    LashSession, TurnActivity, TurnActivitySink, TurnCancelOutcome, TurnCancelRequest, TurnEvent,
    TurnInput, TurnOutput,
};
use lash_remote_protocol::{
    Envelope, Negotiated, RemoteLiveReplayGap, RemoteSessionCursor, RemoteSessionObservation,
    RemoteSessionObservationEvent, RemoteSessionObservationEventPayload,
};
#[cfg(test)]
use lash_remote_protocol::{Negotiation, REMOTE_PROTOCOL};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use crate::board::{BoardState, agent_owes_move};
use crate::db::{ChatBranchPoint, ChatMessage, ChatModelSelection, ChatSummary};
use crate::remote_protocol::negotiate_remote;
#[cfg(test)]
use crate::remote_protocol::test_remote_headers;
use crate::state::{AppError, AppResult, AppStateData};
use crate::ui::INDEX_HTML;

/// How many extra turns the host will spend re-prompting an agent that
/// finished a turn owing an O move (FIG-3181). One: a nudge, then a forfeit.
pub(crate) const ZERO_MOVE_RETRIES: usize = 1;

/// The nudge. It is turn input, never a persisted `user` row: the transcript
/// still holds exactly one user row per board click.
const ZERO_MOVE_NUDGE: &str = "You finished your turn without playing. It is still O's turn and the game is not over. Call `board.play(...)` exactly once now with one of the legal move indexes, then finish with one short sentence.";

/// What the user is told when the nudge did not land either.
const ZERO_MOVE_FORFEIT: &str = "The agent finished twice without playing. Its move for this round is forfeited and the board is yours again.";

#[derive(Debug, Deserialize)]
pub(crate) struct CreateChatRequest {
    title: Option<String>,
    model: Option<String>,
    model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateChatLlmProfileRequest {
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
    session_id: SessionId,
    turn_id: TurnId,
    outcome: TurnCancelOutcome,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AppSettings {
    default_profile: String,
    default_profile_variant: Option<String>,
    model_variants: Vec<&'static str>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
        /// The same request may succeed if sent again: the session was
        /// briefly busy, not the turn broken.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retryable: bool,
    },
    Done,
}

/// Why a sent turn produced no answer to persist: the send or its follow
/// failed, or the input's root did not answer (FIG-3837: a host maps all four
/// outcome statuses, never only `output()?`).
#[derive(Debug)]
pub(crate) struct TurnRefusal {
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

impl From<lash::EmbedError> for TurnRefusal {
    fn from(error: lash::EmbedError) -> Self {
        Self {
            retryable: error.is_retryable(),
            message: error.to_string(),
        }
    }
}

impl TurnRefusal {
    pub(crate) fn into_stream_item(self) -> StreamItem {
        StreamItem::Error {
            message: self.message,
            retryable: self.retryable,
        }
    }
}

/// An answered root's output, or why the turn has none: a failed root, a
/// cancelled or withdrawn input, or a root parked until an operator resolves
/// its park.
pub(crate) fn answered_output(outcome: lash::SendOutcome) -> Result<TurnOutput, TurnRefusal> {
    let refusal = |message: String| TurnRefusal {
        message,
        retryable: false,
    };
    match outcome {
        lash::SendOutcome::Settled { output, .. } => match output.status() {
            lash::TurnStatus::Answered => Ok(*output),
            status => Err(refusal(format!(
                "the turn ended as {status:?}: {:?}",
                output.result.outcome
            ))),
        },
        lash::SendOutcome::Withdrawn { .. } => Err(refusal("the input was withdrawn".to_string())),
        lash::SendOutcome::Parked { parked, .. } => Err(refusal(format!(
            "the turn is parked ({:?}); it resumes once an operator resolves the park",
            parked.reason
        ))),
        lash::SendOutcome::Stalled { stalled, .. } => Err(refusal(format!(
            "the input delivery stalled: {:?}",
            stalled.reason
        ))),
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
        default_profile: state.default_profile().to_string(),
        default_profile_variant: state.default_profile_variant().map(str::to_string),
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
    let selection = normalize_llm_profile_selection(
        request.model.as_deref(),
        request.model_variant.as_deref(),
        state.default_profile(),
        state.default_profile_variant(),
    )?;
    state
        .with_db(move |db| {
            db.create_chat(&title, &selection.model, selection.model_variant.as_deref())
        })
        .await
        .map(Json)
}

pub(crate) async fn update_chat_llm_profile(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    Json(request): Json<UpdateChatLlmProfileRequest>,
) -> AppResult<Json<ChatSummary>> {
    let selection = normalize_optional_llm_profile_selection(
        Some(&request.model),
        request.model_variant.as_deref(),
    )?
    .ok_or_else(|| AppError::bad_request("model is required"))?;
    state
        .with_db(move |db| {
            db.update_chat_llm_profile(
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
            move |db| db.chat_llm_profile_selection(&chat_id)
        })
        .await?;
    let session = state
        .open_session(&chat_id, llm_profile_choice_for_chat_selection(&selection))
        .await?;
    // The chat names a branch point by the node its last turn ended at; lash
    // names the same state by the head revision that published it. Pinning
    // the revision keeps it through every collection.
    let head = session
        .revisions()
        .await
        .map_err(branch_error)?
        .into_iter()
        .find(|revision| revision.head)
        .ok_or_else(|| AppError::internal("the chat session records no head revision"))?;
    let node_id = head
        .leaf_node_id
        .as_ref()
        .map(ToString::to_string)
        .ok_or_else(|| AppError::bad_request("the chat has no completed turn to pin"))?;
    session
        .pin(lash::Target::Revision(head.head_revision))
        .await
        .map_err(branch_error)?;
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
    let observed_processes = state
        .core()
        .process_registry()
        .list_observed_by(
            &source_chat_id.clone().try_into()?,
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| AppError::internal(error.to_string()))?
        .into_iter()
        .map(|record| record.id)
        .collect();
    let target_chat_id = uuid::Uuid::new_v4().to_string();
    state
        .with_db({
            let source_chat_id = source_chat_id.clone();
            let node_id = node_id.clone();
            let target_chat_id = target_chat_id.clone();
            move |db| db.prepare_chat_fork(&source_chat_id, &node_id, &target_chat_id)
        })
        .await?;
    let source_session_id = SessionId::parse(source_chat_id)?;
    let forked = async {
        let target = branch_point_target(&state, &source_session_id, &node_id).await?;
        state
            .core()
            .fork_at(
                &source_session_id,
                target,
                lash::ForkRequest {
                    session_id: target_chat_id.clone().try_into()?,
                    relation: lash::persistence::SessionRelation::Fork {
                        source_session_id: source_session_id.clone(),
                        source_node_id: Some(node_id.clone().try_into()?),
                    },
                    observed_processes,
                },
            )
            .await
            .map_err(branch_error)
    }
    .await;
    if let Err(error) = forked {
        // Both abort paths run the same compensator: `fork_at` can fail after
        // the fork's session store exists, and only `discard_pending_chat_fork`
        // reclaims it. A compensator failure must not mask the fork error the
        // caller is answered with.
        let _ = state.discard_pending_chat_fork(&target_chat_id).await;
        return Err(error);
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
    state.record_board_context_for_chat(&target_chat_id).await?;
    Ok(Json(chat))
}

pub(crate) async fn send_message(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<SendMessageRequest>,
) -> AppResult<Response> {
    let (negotiated, accept_json) = negotiate_remote(&headers)?;
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let request_model = normalize_optional_llm_profile_selection(
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;

    // The board is app-owned state. The user message keeps a snapshot for UI
    // replay, while tools read and mutate the canonical copy in the database.
    let user_board = request.board.clone();
    let (user_message, llm_profile_selection) = state
        .with_db({
            let chat_id = chat_id.clone();
            let text = text.clone();
            let user_board = user_board.clone();
            move |db| {
                db.require_chat(&chat_id)?;
                if let Some(selection) = request_model {
                    db.update_chat_llm_profile(
                        &chat_id,
                        &selection.model,
                        selection.model_variant.as_deref(),
                    )?;
                }
                let llm_profile_selection = db.chat_llm_profile_selection(&chat_id)?;
                db.maybe_title_from_first_message(&chat_id, &text)?;
                db.upsert_chat_board(&chat_id, &user_board)?;
                let message = db.insert_message_with_payload(
                    &chat_id,
                    "user",
                    &text,
                    Some(json!({ "board": user_board })),
                )?;
                Ok((message, llm_profile_selection))
            }
        })
        .await?;

    // One path in every durability mode: the chat's session takes the input
    // through `send()`, and the session's engine drives the turn -- in process
    // for the local store, in a Restate handler for the Restate deployment.
    let turn_profile = llm_profile_choice_for_chat_selection(&llm_profile_selection);
    let session = state.open_session(&chat_id, turn_profile).await?;
    state.record_board_context(&session).await?;
    let replay_cursor = session.observe().current_observation().cursor;
    let turn_id = TurnId::prefixed("agent-service-turn:", uuid::Uuid::new_v4());
    let (tx, rx) = mpsc::channel::<StreamItem>(64);
    let mut replay =
        spawn_live_replay_forwarder(session.clone(), replay_cursor, tx.clone(), negotiated);
    let run_state = state.clone();
    let task_turn_id = turn_id.clone();
    tokio::spawn(async move {
        let _ = tx
            .send(StreamItem::Message {
                message: user_message,
            })
            .await;
        // A turn can finish without ever calling `board.play`, which wedges the
        // round for good (FIG-3181); the recovery policy is its own helper.
        let emit_tx = tx.clone();
        let attempt = run_turn_with_zero_move_recovery(
            &run_state,
            &chat_id,
            text,
            task_turn_id,
            || TurnId::prefixed("agent-service-turn:", uuid::Uuid::new_v4()),
            |turn_input, turn_id| {
                let session = session.clone();
                let run_state = run_state.clone();
                let chat_id = chat_id.clone();
                let tx = tx.clone();
                async move {
                    let turn_state = Arc::new(Mutex::new(TurnPersistenceState::default()));
                    let ui_events = ChannelTurnEvents::persistence(
                        run_state.clone(),
                        chat_id.clone(),
                        Arc::clone(&turn_state),
                        Some(tx.clone()),
                    );
                    let turn = match session
                        .send(TurnInput::text(turn_input))
                        .id(turn_id)
                        .require_finish()
                    {
                        Ok(turn) => turn.outcome_into(&ui_events).await,
                        Err(err) => Err(err),
                    };
                    let output = match turn.map_err(TurnRefusal::from).and_then(answered_output) {
                        Ok(output) => output,
                        Err(refusal) => {
                            let _ = tx.send(refusal.into_stream_item()).await;
                            return Ok(TurnAttempt::Failed);
                        }
                    };
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
                                    retryable: false,
                                })
                                .await;
                        }
                    }
                    Ok(TurnAttempt::Completed)
                }
            },
            move |item| {
                let tx = emit_tx.clone();
                async move {
                    let _ = tx.send(item).await;
                }
            },
        )
        .await;
        match attempt {
            Ok(TurnAttempt::Completed) => wait_for_live_replay_flush(&mut replay).await,
            // The turn's own error is already on the stream. This path reports
            // in band and never hands the helper an error to propagate.
            Ok(TurnAttempt::Failed) | Err(_) => replay.abort(),
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

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-lash-turn-id", turn_id.as_str())
        .header("x-lash-protocol-accept", accept_json)
        .body(Body::from_stream(stream))
        .map_err(|err| AppError::internal(format!("build streaming response: {err}")))
}

/// Request cooperative cancellation of one exact foreground turn.
///
/// The ids route the request; deployments exposing this endpoint beyond the
/// local demo must authenticate the caller and authorize access to the chat.
pub(crate) async fn cancel_turn(
    State(state): State<AppStateData>,
    AxumPath((chat_id, turn_id)): AxumPath<(String, TurnId)>,
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
        lash::TurnAddress::new(
            SessionId::parse(chat_id.as_str())?,
            TurnId::parse(turn_id.as_str())?,
        ),
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
        session_id: SessionId::parse(chat_id)?,
        turn_id,
        outcome: receipt.outcome,
    }))
}

pub(crate) struct ChannelTurnEvents {
    state: AppStateData,
    chat_id: String,
    /// Where a failed persistence write is reported, when a client streams
    /// the turn.
    errors: Option<mpsc::Sender<StreamItem>>,
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
        errors: Option<mpsc::Sender<StreamItem>>,
    ) -> Self {
        Self {
            state,
            chat_id,
            errors,
            turn_state,
        }
    }

    async fn emit_error(&self, message: String) {
        if let Some(errors) = &self.errors {
            let _ = errors
                .send(StreamItem::Error {
                    message,
                    retryable: false,
                })
                .await;
        }
    }

    async fn handle(&self, activity: TurnActivity) {
        let event = &activity.event;
        if let TurnEvent::AssistantProseDelta { text, .. } = &event {
            self.turn_state
                .lock_recover()
                .assistant_prose
                .push_str(text);
            return;
        }
        // Keep persisted message order tied to event start order. The browser
        // only renders completed code/tool rows, but reload should still
        // reconstruct "thinking -> cell -> tools -> assistant".
        if let TurnEvent::ReasoningDelta { text, .. } = &event {
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
    negotiated: Negotiated,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        forward_live_replay_until_commit(session, cursor, tx, negotiated).await;
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
    negotiated: Negotiated,
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
                    retryable: false,
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
                        retryable: false,
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
                        event: Box::new(Envelope::at(&negotiated, event)),
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
                        observation: Box::new(Envelope::at(&negotiated, observation)),
                        gap: Box::new(Envelope::at(&negotiated, gap)),
                    })
                    .await;
            }
        }
    }
}

fn normalize_llm_profile_selection(
    model: Option<&str>,
    model_variant: Option<&str>,
    default_profile: &str,
    default_profile_variant: Option<&str>,
) -> AppResult<ChatModelSelection> {
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(default_profile)
        .to_string();
    let model_variant = normalize_model_variant(model_variant).or_else(|| {
        default_profile_variant
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

fn normalize_optional_llm_profile_selection(
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

/// The model key and reasoning a chat's selection runs with: the service's
/// catalog keys every OpenRouter model by its id.
pub(crate) fn llm_profile_choice_for_chat_selection(
    selection: &ChatModelSelection,
) -> LlmProfileChoice {
    LlmProfileChoice {
        key: lash::LlmProfileKey::new(selection.model.clone()),
        reasoning: selection
            .model_variant
            .clone()
            .map(lash::provider::ReasoningSelection::Effort)
            .unwrap_or_default(),
    }
}

/// A chat's model selection as the session records it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LlmProfileChoice {
    pub(crate) key: lash::LlmProfileKey,
    pub(crate) reasoning: lash::provider::ReasoningSelection,
}

fn normalize_model_variant(model_variant: Option<&str>) -> Option<String> {
    model_variant
        .map(str::trim)
        .filter(|variant| !variant.is_empty())
        .map(str::to_string)
}

/// The retained revision of `session_id` whose turn ended at `node_id`: the
/// name lash forks the chat's branch point by. A session the catalog does
/// not hold, or one that no longer retains that turn, has nothing to fork.
async fn branch_point_target(
    state: &AppStateData,
    session_id: &SessionId,
    node_id: &str,
) -> AppResult<lash::Target> {
    let not_retained = || AppError {
        status: StatusCode::CONFLICT,
        message: format!("branch point `{node_id}` is no longer retained"),
    };
    let session = state
        .core()
        .session(session_id.clone())
        .durable()
        .await
        .map_err(branch_error)?;
    let revisions = match session.revisions().await {
        Ok(revisions) => revisions,
        Err(
            lash::EmbedError::UnknownSession { .. }
            | lash::EmbedError::Store(
                lash::persistence::StoreError::SessionNotFound { .. }
                | lash::persistence::StoreError::SessionDeleted { .. },
            ),
        ) => return Err(not_retained()),
        Err(error) => return Err(branch_error(error)),
    };
    revisions
        .into_iter()
        .rev()
        .find(|revision| {
            revision
                .leaf_node_id
                .as_ref()
                .is_some_and(|leaf| leaf.as_str() == node_id)
        })
        .map(|revision| lash::Target::Revision(revision.head_revision))
        .ok_or_else(not_retained)
}

fn branch_error(error: lash::EmbedError) -> AppError {
    if matches!(
        &error,
        lash::EmbedError::Store(
            lash::persistence::StoreError::ForkTargetPending { .. }
                | lash::persistence::StoreError::ForkTargetUnavailable { .. }
                | lash::persistence::StoreError::ForkTargetPruned { .. }
        )
    ) {
        return AppError {
            status: StatusCode::CONFLICT,
            message: error.to_string(),
        };
    }
    AppError::internal(error.to_string())
}

/// What one turn through the zero-move recovery loop did.
pub(crate) enum TurnAttempt {
    /// The turn ran to completion and its assistant row is persisted.
    Completed,
    /// The turn itself failed. The runner has already reported the error, so
    /// the loop stops rather than spending a re-prompt on a broken turn.
    Failed,
}

/// An agent can finish a turn without ever calling `board.play`. `play()` is
/// the only place the board's `turn` flips back to `X`, and the UI disables
/// every cell while `turn != "X"`, so an unguarded zero-move turn wedges the
/// round for good (FIG-3181). This is the entire recovery policy -- one nudge,
/// then forfeit the move and hand the board back -- and it belongs to the host,
/// not to a durability mode: the session's engine runs each turn (`run_turn`)
/// and the route streams each item (`emit`).
pub(crate) async fn run_turn_with_zero_move_recovery<N, R, RF, E, EF>(
    state: &AppStateData,
    chat_id: &str,
    text: String,
    first_turn_id: TurnId,
    mut next_turn_id: N,
    mut run_turn: R,
    mut emit: E,
) -> AppResult<TurnAttempt>
where
    N: FnMut() -> TurnId,
    R: FnMut(String, TurnId) -> RF,
    RF: Future<Output = AppResult<TurnAttempt>>,
    E: FnMut(StreamItem) -> EF,
    EF: Future<Output = ()>,
{
    let mut turn_input = text;
    let mut turn_id = first_turn_id;
    for attempt in 0..=ZERO_MOVE_RETRIES {
        match run_turn(turn_input, turn_id).await? {
            TurnAttempt::Completed => {}
            TurnAttempt::Failed => return Ok(TurnAttempt::Failed),
        }
        if !agent_still_owes_move(state, chat_id).await {
            break;
        }
        if attempt == ZERO_MOVE_RETRIES {
            forfeit_agent_move(state, chat_id, &mut emit).await;
            break;
        }
        turn_input = ZERO_MOVE_NUDGE.to_string();
        turn_id = next_turn_id();
    }
    Ok(TurnAttempt::Completed)
}

/// Whether the canonical board still owes an O move after a completed turn.
///
/// A read failure answers "no": a broken database is reported by the next
/// request that touches it, and must not spend a re-prompt or forfeit a move.
async fn agent_still_owes_move(state: &AppStateData, chat_id: &str) -> bool {
    state
        .with_db({
            let chat_id = chat_id.to_string();
            move |db| db.chat_board(&chat_id).map(|board| agent_owes_move(&board))
        })
        .await
        .unwrap_or(false)
}

/// Forfeit the O move the agent never played: hand the board back to the human
/// and say so in the transcript, carrying the yielded board so the UI applies
/// it through the same path a tool result would (FIG-3181).
async fn forfeit_agent_move<E, EF>(state: &AppStateData, chat_id: &str, emit: &mut E)
where
    E: FnMut(StreamItem) -> EF,
    EF: Future<Output = ()>,
{
    let yielded = state
        .with_db({
            let chat_id = chat_id.to_string();
            move |db| db.yield_agent_turn(&chat_id)
        })
        .await;
    let board = match yielded {
        // Nothing owed after all — the board moved under us; say nothing.
        Ok(None) => return,
        Ok(Some(board)) => board,
        Err(err) => {
            emit(StreamItem::Error {
                message: err.message,
                retryable: false,
            })
            .await;
            return;
        }
    };
    if let Err(err) = state.record_board_context_for_chat(chat_id).await {
        emit(StreamItem::Error {
            message: err.message,
            retryable: false,
        })
        .await;
        return;
    }
    let inserted = state
        .with_db({
            let chat_id = chat_id.to_string();
            let payload = json!({ "board": board });
            move |db| {
                db.insert_message_with_payload(&chat_id, "system", ZERO_MOVE_FORFEIT, Some(payload))
            }
        })
        .await;
    match inserted {
        Ok(message) => emit(StreamItem::Message { message }).await,
        Err(err) => {
            emit(StreamItem::Error {
                message: err.message,
                retryable: false,
            })
            .await;
        }
    }
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

#[cfg(test)]
#[path = "route_tests/zero_move_turn.rs"]
mod zero_move_turn_tests;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::to_bytes;
    use lash::LashCore;
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;

    use super::*;
    use crate::db::AppDb;

    #[tokio::test]
    async fn message_route_streams_session_observations_with_mock_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-route-mock")
            .complete(|_request| async {
                let text = r#"<typescript>
finish("done through route");
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
        let double = crate::state::test_support::test_double().await;
        let backend = double.lash_backend();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
            &backend,
        );
        let core = LashCore::rlm_builder(backend, factory)
            .serve_test_llm_profile(
                provider,
                lash::LlmProfileMetadata::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-test",
                "test",
            ))
            .expect("core");
        let db = Arc::new(Mutex::new(
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
        ));
        let state = AppStateData::new(
            core,
            Arc::clone(&db),
            "mock-model".to_string(),
            None,
            double.connection(),
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
            test_remote_headers(),
            Json(SendMessageRequest {
                text: "exercise live replay".to_string(),
                board: crate::board::default_board(),
                model: None,
                model_variant: Default::default(),
            }),
        ))
        .await
        .expect("send message");
        let accept: Negotiation = serde_json::from_str(
            response
                .headers()
                .get("x-lash-protocol-accept")
                .expect("protocol Accept response header")
                .to_str()
                .expect("protocol Accept header text"),
        )
        .expect("protocol Accept JSON");
        assert_eq!(
            Negotiated::from_accept(REMOTE_PROTOCOL, &accept)
                .expect("valid protocol Accept")
                .selected(),
            lash_remote_protocol::REMOTE_PROTOCOL_VERSION
        );
        let turn_id = TurnId::fixture(
            response
                .headers()
                .get("x-lash-turn-id")
                .expect("turn id response header")
                .to_str()
                .expect("turn id header text"),
        );
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
        let negotiated = negotiate_remote(&test_remote_headers()).unwrap().0;
        let item = StreamItem::ReplayGap {
            observation: Box::new(Envelope::at(
                &negotiated,
                RemoteSessionObservation {
                    // Standalone stream payloads carry one shared protocol envelope.
                    session_id: SessionId::from("session-1"),
                    cursor: "cursor-after".to_string(),
                    turn_index: 3,
                    usage: lash_remote_protocol::RemoteUsage::default(),
                },
            )),
            gap: Box::new(Envelope::at(
                &negotiated,
                RemoteLiveReplayGap {
                    // Nested DTOs remain bare inside that envelope body.
                    session_id: SessionId::from("session-1"),
                    requested_cursor: "cursor-before".to_string(),
                    latest_cursor: "cursor-after".to_string(),
                    latest_revision: 7,
                    reason: lash_remote_protocol::RemoteLiveReplayGapReason::Trimmed,
                },
            )),
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
