use lash::SessionId;
use lash::TurnId;
use lash::sync::MutexExt;
use std::future::Future;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, Response};
use futures_util::StreamExt;
use lash::observe::{RemoteSessionObservationStreamItem, SessionCursor};
use lash::remote::observations::{
    RemoteLiveReplayGap, RemoteSessionCursor, RemoteSessionObservation,
    RemoteSessionObservationEvent, RemoteSessionObservationEventPayload,
};
use lash::remote::turn_result::RemoteSendOutcome;
use lash::remote::usage::RemoteTurnActivity;
use lash::remote::{Envelope, Negotiated};
#[cfg(test)]
use lash::remote::{Negotiation, REMOTE_PROTOCOL};
use lash::rlm::RlmSendBuilderExt as _;
use lash::{LashSession, TurnActivity, TurnActivitySink, TurnCancelOutcome, TurnInput, TurnOutput};
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
    #[serde(flatten)]
    outcome: CancelTurnOutcome,
}

/// The facade cancel's answer, projected onto the wire: which
/// [`lash::CancelReceipt`] variant the durable session returned.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum CancelTurnOutcome {
    /// The input was still queued: it was withdrawn and no run applied it.
    Withdrawn,
    /// A running run now holds the request; `cancellation` is the
    /// cancellation gate's typed answer.
    Requested { cancellation: TurnCancelOutcome },
    /// The run had already settled.
    AlreadySettled,
    /// No accepted input or run answers to that id.
    NotFound,
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
    Activity {
        activity: Box<Envelope<RemoteTurnActivity>>,
    },
    Outcome {
        outcome: Box<Envelope<RemoteSendOutcome>>,
    },
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
/// failed, or the input's run did not answer (FIG-3837: a host maps all four
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

/// An answered run's output, or why the turn has none: a failed run, a
/// cancelled or withdrawn input, or a run parked until an operator resolves
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
    state.chat_summaries().await.map(Json)
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
    let chat = state
        .with_db(move |db| {
            db.create_chat(&title, &selection.model, selection.model_variant.as_deref())
        })
        .await?;
    let profile = llm_profile_choice_for_chat_selection(&ChatModelSelection {
        model: chat.model.clone(),
        model_variant: chat.model_variant.clone(),
    });
    state.open_session(&chat.id, profile).await?;
    Ok(Json(chat))
}

pub(crate) async fn attach_turn(
    State(state): State<AppStateData>,
    AxumPath((chat_id, turn_id)): AxumPath<(String, TurnId)>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let durable = chat_durable_session(&state, chat_id).await?;
    let handle = durable.attach_id(turn_id.clone());
    follow_accepted_input(durable.session_id().clone(), handle, headers, Some(turn_id))
}

pub(crate) async fn attach_input(
    State(state): State<AppStateData>,
    AxumPath((chat_id, input_id)): AxumPath<(String, lash::InputId)>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let durable = chat_durable_session(&state, chat_id).await?;
    let handle = durable.attach(input_id);
    follow_accepted_input(durable.session_id().clone(), handle, headers, None)
}

async fn chat_durable_session(
    state: &AppStateData,
    chat_id: String,
) -> AppResult<lash::DurableSession> {
    let session_id = SessionId::parse(&chat_id)?;
    state.with_db(move |db| db.require_chat(&chat_id)).await?;
    let durable = state.core().session(session_id).durable().await?;
    if !durable.exists().await? {
        return Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: "the chat has no durable session".to_string(),
        });
    }
    Ok(durable)
}

fn follow_accepted_input(
    session_id: SessionId,
    handle: lash::SendHandle,
    headers: HeaderMap,
    turn_id: Option<TurnId>,
) -> AppResult<Response> {
    let (negotiated, accept_json) = negotiate_remote(&headers)?;
    let input_id = handle.input_id().clone();
    let (tx, rx) = mpsc::channel::<StreamItem>(64);
    let sink = FollowTurnEvents {
        tx: tx.clone(),
        negotiated,
        sequence: Mutex::new(0),
    };
    let task_input_id = input_id.clone();
    tokio::spawn(async move {
        // Disconnecting drops only this observer; the engine keeps executing.
        let outcome = tokio::select! {
            _ = tx.closed() => return,
            outcome = handle.outcome_into(&sink) => outcome,
        };
        let item = match outcome {
            Ok(outcome) => StreamItem::Outcome {
                outcome: Box::new(Envelope::at(
                    &negotiated,
                    outcome.to_remote(&session_id, &task_input_id),
                )),
            },
            Err(error) => TurnRefusal::from(error).into_stream_item(),
        };
        let _ = tx.send(item).await;
        let _ = tx.send(StreamItem::Done).await;
    });
    let mut response = crate::ndjson::ndjson_response(ReceiverStream::new(rx));
    response.headers_mut().insert(
        "x-lash-protocol-accept",
        accept_json
            .parse()
            .map_err(|err| AppError::internal(format!("protocol Accept header: {err}")))?,
    );
    response.headers_mut().insert(
        "x-lash-input-id",
        input_id
            .as_str()
            .parse()
            .map_err(|err| AppError::internal(format!("input id header: {err}")))?,
    );
    if let Some(turn_id) = turn_id {
        response.headers_mut().insert(
            "x-lash-turn-id",
            turn_id
                .as_str()
                .parse()
                .map_err(|err| AppError::internal(format!("turn id header: {err}")))?,
        );
    }
    Ok(response)
}

struct FollowTurnEvents {
    tx: mpsc::Sender<StreamItem>,
    negotiated: Negotiated,
    sequence: Mutex<u64>,
}

#[async_trait]
impl TurnActivitySink for FollowTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        let sequence = {
            let mut sequence = self.sequence.lock_recover();
            let current = *sequence;
            *sequence = sequence.saturating_add(1);
            current
        };
        let item = match RemoteTurnActivity::from_core(sequence, activity) {
            Ok(activity) => StreamItem::Activity {
                activity: Box::new(Envelope::at(&self.negotiated, activity)),
            },
            Err(error) => StreamItem::Error {
                message: error.to_string(),
                retryable: false,
            },
        };
        let _ = self.tx.send(item).await;
    }
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
    let durable = state
        .core()
        .session(SessionId::parse(&chat_id)?)
        .durable()
        .await?;
    let rows = if durable.exists().await? {
        durable
            .transcript()
            .await?
            .visible()
            .filter(|row| row.kind != lash::transcript::TranscriptRowKind::User)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    state
        .with_db(move |db| {
            db.require_chat(&chat_id)?;
            for row in &rows {
                db.insert_transcript_row(&chat_id, row)?;
            }
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
    state
        .with_db({
            let chat_id = chat_id.clone();
            move |db| db.require_chat(&chat_id).map(|_| ())
        })
        .await?;
    let session = state
        .core()
        .session(SessionId::parse(chat_id.as_str())?)
        .durable()
        .await
        .map_err(branch_error)?;
    // The chat names a branch point by the node its last turn ended at: the
    // committed read view's leaf. A chat that has never messaged has no
    // committed view and nothing to pin.
    let node_id = session
        .read()
        .await
        .map_err(branch_error)?
        .and_then(|view| {
            view.session_graph()
                .leaf_node_id
                .as_ref()
                .map(ToString::to_string)
        })
        .ok_or_else(|| AppError::bad_request("the chat has no completed turn to pin"))?;
    // The revision that published that leaf is what lash pins; pinning it
    // keeps the state through every collection so a fork can name the node.
    let head = session
        .revisions()
        .await
        .map_err(branch_error)?
        .into_iter()
        .find(|revision| revision.head)
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
    let source_session_id = SessionId::parse(source_chat_id.clone())?;
    let observed_processes = state
        .core()
        .processes()
        .list_observed_by(
            &lash::process::SessionScope::new(source_session_id.clone()),
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .map(|record| record.process_id)
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

    // The response names an accepted input. The session's engine executes it.
    let turn_profile = llm_profile_choice_for_chat_selection(&llm_profile_selection);
    let session = state.open_session(&chat_id, turn_profile).await?;
    state.record_board_context(&session).await?;
    let replay_cursor = session.observe().current_observation().cursor;
    let turn_id = TurnId::prefixed("agent-service-turn:", uuid::Uuid::new_v4());
    let accepted = session
        .send(TurnInput::text(text.clone()))
        .id(turn_id.clone())
        .require_finish()?
        .await?;
    let input_id = accepted.input_id().clone();
    let (tx, rx) = mpsc::channel::<StreamItem>(64);
    let mut replay =
        spawn_live_replay_forwarder(session.clone(), replay_cursor, tx.clone(), negotiated);
    let run_state = state.clone();
    let task_turn_id = turn_id.clone();
    tokio::spawn(async move {
        let mut accepted = Some(accepted);
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
                let accepted = accepted.take();
                let session = session.clone();
                let run_state = run_state.clone();
                let chat_id = chat_id.clone();
                let tx = tx.clone();
                async move {
                    let turn = match accepted {
                        Some(turn) => turn.outcome().await,
                        None => match session
                            .send(TurnInput::text(turn_input))
                            .id(turn_id.clone())
                            .require_finish()
                        {
                            Ok(turn) => match turn.await {
                                Ok(turn) => turn.outcome().await,
                                Err(error) => Err(error),
                            },
                            Err(err) => Err(err),
                        },
                    };
                    let _output = match turn.map_err(TurnRefusal::from).and_then(answered_output) {
                        Ok(output) => output,
                        Err(refusal) => {
                            let _ = tx.send(refusal.into_stream_item()).await;
                            return Ok(TurnAttempt::Failed);
                        }
                    };
                    let messages =
                        mirror_committed_turn(&run_state, &chat_id, &session, &turn_id).await?;
                    for message in messages {
                        let _ = tx.send(StreamItem::Message { message }).await;
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

    let mut response = crate::ndjson::ndjson_response(ReceiverStream::new(rx));
    let headers = response.headers_mut();
    headers.insert(
        "x-lash-turn-id",
        HeaderValue::from_str(turn_id.as_str())
            .map_err(|err| AppError::internal(format!("invalid turn-id header: {err}")))?,
    );
    headers.insert(
        "x-lash-input-id",
        HeaderValue::from_str(input_id.as_str())
            .map_err(|err| AppError::internal(format!("invalid input-id header: {err}")))?,
    );
    headers.insert(
        "x-lash-protocol-accept",
        HeaderValue::from_str(&accept_json)
            .map_err(|err| AppError::internal(format!("invalid protocol-accept header: {err}")))?,
    );
    Ok(response)
}

/// Request cooperative cancellation of the turn the host id names.
///
/// The session's durable handle re-derives the accepted input from
/// `turn_id`, so a still-queued input is withdrawn outright and a running
/// run gets a durable cancel request. The ids route the request;
/// deployments exposing this endpoint beyond the local demo must
/// authenticate the caller and authorize access to the chat.
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
    let session_id = SessionId::parse(chat_id.as_str())?;
    let mut cancel = state
        .core()
        .session(session_id.clone())
        .durable()
        .await?
        .attach_id(turn_id.clone())
        .cancel()
        .origin("user");
    if let Some(request_id) = request
        .request_id
        .filter(|request_id| !request_id.trim().is_empty())
    {
        cancel = cancel.request_id(request_id);
    }
    if let Some(reason) = request.reason {
        cancel = cancel.reason(reason);
    }
    let outcome = match cancel.await? {
        lash::CancelReceipt::Withdrawn(_) => CancelTurnOutcome::Withdrawn,
        lash::CancelReceipt::Requested { receipt, .. } => CancelTurnOutcome::Requested {
            cancellation: receipt.outcome,
        },
        lash::CancelReceipt::AlreadySettled { .. } => CancelTurnOutcome::AlreadySettled,
        lash::CancelReceipt::NotFound => CancelTurnOutcome::NotFound,
        // `CancelReceipt` is non-exhaustive.
        _ => {
            return Err(AppError::internal(
                "the session answered cancel with an unrecognized receipt",
            ));
        }
    };
    Ok(Json(CancelTurnResponse {
        session_id,
        turn_id,
        outcome,
    }))
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
                    RemoteSessionObservationEventPayload::Committed { .. }
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
/// then forfeit the move and hand the board back -- and it belongs to the
/// host, not to lash: the session's engine runs each turn (`run_turn`) and
/// the route streams each item (`emit`).
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

pub(crate) async fn mirror_committed_turn(
    state: &AppStateData,
    chat_id: &str,
    session: &LashSession,
    turn_id: &TurnId,
) -> AppResult<Vec<ChatMessage>> {
    let transcript = session.durable().transcript().await?;
    let rows = transcript.visible()
        .filter(|row| row.provenance.turn_id.as_ref() == Some(turn_id))
        // The product owns the submitted board-click row, including its board.
        .filter(|row| row.kind != lash::transcript::TranscriptRowKind::User)
        .cloned().collect::<Vec<_>>();
    let chat_id = chat_id.to_owned();
    state
        .with_db(move |db| {
            rows.iter()
                .map(|row| db.insert_transcript_row(&chat_id, row))
                .collect()
        })
        .await
}

#[cfg(test)]
#[path = "route_tests/zero_move_turn.rs"]
mod zero_move_turn_tests;

#[cfg(test)]
#[path = "route_tests/reconnect.rs"]
mod reconnect_tests;

#[cfg(test)]
#[path = "route_tests/streaming.rs"]
mod tests;
