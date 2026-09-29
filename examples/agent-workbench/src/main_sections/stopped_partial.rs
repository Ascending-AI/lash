//! The stopped-partial panel's two routes (ADR 0114 §5.4).
//!
//! A stopped turn's partial output is durable data lash returns to the host;
//! lash never feeds it back. The panel reads it through
//! `GET /api/turns/{turn_id}/stopped-partial`, live after the turn settles and
//! again after a reconnect. Discard is the page closing the panel: it makes no
//! request, and the next message is an ordinary send, which is lash's
//! backtrack default. Continue in context posts the user's choices and a
//! follow-up to `POST /api/turns/{turn_id}/stopped-partial/continue`, which
//! renders them with `lash::build_resubmission` and sends the result through
//! the same `send()` a typed message takes.

use super::*;
use lash::{SessionId, TurnId, TurnInput};

/// The panel's read of one turn's partial.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum StoppedPartialResponse {
    Available(Box<AvailableStoppedPartial>),
    /// The turn is still running, or it is sealed and not yet committed.
    Pending,
    /// The turn settled without a stop.
    NotStopped,
    /// The session owns no such turn.
    Unknown,
}

#[derive(Debug, Serialize)]
pub(crate) struct AvailableStoppedPartial {
    pub(crate) partial: lash::StoppedPartial,
    pub(crate) summary: lash::StoppedPartialSummary,
    /// The choice the default selection makes for each item. Reasoning and
    /// fragments have none: the user must choose them.
    pub(crate) default_choices: BTreeMap<String, lash::ItemChoice>,
    /// What the default selection would send.
    pub(crate) preview: ResubmissionPreview,
}

/// What a selection renders to, or the helper's typed refusal of it.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ResubmissionPreview {
    Ready {
        input: Vec<lash::InputItem>,
        omissions: lash::OmissionReport,
    },
    Refused {
        error: ResubmissionRefusal,
    },
}

/// A [`lash::ResubmissionError`] in the page's terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResubmissionRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    /// The items the refusal names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) items: Vec<String>,
}

impl From<&lash::ResubmissionError> for ResubmissionRefusal {
    fn from(error: &lash::ResubmissionError) -> Self {
        let (code, items) = match error {
            lash::ResubmissionError::SelectionIncomplete { items } => (
                "selection_incomplete",
                items.iter().map(|item| item.as_str().to_string()).collect(),
            ),
            lash::ResubmissionError::ChoiceNotAllowed { item, .. } => {
                ("choice_not_allowed", vec![item.as_str().to_string()])
            }
            lash::ResubmissionError::UnknownItem { item } => {
                ("unknown_item", vec![item.as_str().to_string()])
            }
            lash::ResubmissionError::Empty => ("empty", Vec::new()),
            lash::ResubmissionError::Unencodable { .. } => ("unencodable", Vec::new()),
        };
        Self {
            code,
            message: error.to_string(),
            items,
        }
    }
}

/// The user's choices and follow-up for one partial.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContinueInContextRequest {
    /// Choices by item id, over the default selection.
    #[serde(default)]
    pub(crate) choices: BTreeMap<String, lash::ItemChoice>,
    /// The user's next message, after the quoted excerpt.
    #[serde(default)]
    pub(crate) text: String,
    /// Render the selection without sending it.
    #[serde(default)]
    pub(crate) preview: bool,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) model_variant: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ContinueInContextResponse {
    Preview {
        preview: ResubmissionPreview,
    },
    /// The resubmission was accepted as the input of a new turn.
    Sent {
        turn_id: TurnId,
        omissions: lash::OmissionReport,
    },
}

/// The default selection's choice for each item.
pub(crate) fn default_choices(
    partial: &lash::StoppedPartial,
) -> BTreeMap<String, lash::ItemChoice> {
    lash::ResubmissionSelection::defaults(partial)
        .choices()
        .iter()
        .map(|(item, choice)| (item.as_str().to_string(), *choice))
        .collect()
}

/// Render `choices` over the default selection, with `text` as the
/// follow-up. Pure: nothing is sent.
pub(crate) fn resubmit(
    partial: &lash::StoppedPartial,
    choices: &BTreeMap<String, lash::ItemChoice>,
    text: &str,
) -> Result<lash::Resubmission, lash::ResubmissionError> {
    let mut selection = lash::ResubmissionSelection::defaults(partial);
    for (item, choice) in choices {
        selection.choose(&lash::PartialItemId(item.clone()), *choice);
    }
    let follow_up = match text.trim() {
        "" => TurnInput::empty(),
        text => TurnInput::text(text),
    };
    lash::build_resubmission(&selection, follow_up)
}

pub(crate) fn preview(
    partial: &lash::StoppedPartial,
    choices: &BTreeMap<String, lash::ItemChoice>,
    text: &str,
) -> ResubmissionPreview {
    match resubmit(partial, choices, text) {
        Ok(resubmission) => ResubmissionPreview::Ready {
            input: resubmission.input.items,
            omissions: resubmission.omissions,
        },
        Err(error) => ResubmissionPreview::Refused {
            error: ResubmissionRefusal::from(&error),
        },
    }
}

/// The durable read, through the session's own authorization: a turn of
/// another session, or a fork's ancestor, is `Unknown`.
async fn read_partial(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash::StoppedPartialRead, AppError> {
    state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(AppError::session_open)?
        .stopped_partial(turn_id)
        .await
        .map_err(AppError::runtime)
}

pub(crate) async fn read_stopped_partial(
    AxumPath(turn_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StoppedPartialResponse>, AppError> {
    let session_id = state
        .admit_session(&query, "api.turns.stopped_partial")
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let turn_id = TurnId::from(turn_id);
    Ok(Json(
        match read_partial(&state, &session_id, &turn_id).await? {
            lash::StoppedPartialRead::Available(partial) => {
                StoppedPartialResponse::Available(Box::new(AvailableStoppedPartial {
                    summary: partial.summary(),
                    default_choices: default_choices(&partial),
                    preview: preview(&partial, &BTreeMap::new(), ""),
                    partial,
                }))
            }
            lash::StoppedPartialRead::Pending => StoppedPartialResponse::Pending,
            lash::StoppedPartialRead::NotStopped => StoppedPartialResponse::NotStopped,
            lash::StoppedPartialRead::Unknown => StoppedPartialResponse::Unknown,
        },
    ))
}

pub(crate) async fn continue_stopped_partial(
    AxumPath(turn_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<ContinueInContextRequest>,
) -> Result<Response, AppError> {
    let session_id = state
        .admit_session(&query, "api.turns.stopped_partial.continue")
        .await?;
    let stopped_turn = TurnId::from(turn_id);
    let partial = match read_partial(&state, &session_id, &stopped_turn).await? {
        lash::StoppedPartialRead::Available(partial) => partial,
        read => {
            return Err(AppError::conflict(format!(
                "turn `{stopped_turn}` has no stopped partial to continue from ({read:?})"
            )));
        }
    };
    if request.preview {
        state
            .authorization
            .authorize(WorkbenchAuthorizationAction::Observe {
                session_id: session_id.clone(),
            })?;
        return Ok(Json(ContinueInContextResponse::Preview {
            preview: preview(&partial, &request.choices, &request.text),
        })
        .into_response());
    }
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: session_id.clone(),
        })?;
    let resubmission = match resubmit(&partial, &request.choices, &request.text) {
        Ok(resubmission) => resubmission,
        Err(error) => {
            let refusal = ResubmissionRefusal::from(&error);
            state.trace_for_session(
                &session_id,
                "api.turns.stopped_partial.refused",
                json!({ "turn_id": stopped_turn, "refusal": &refusal }),
            );
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": refusal.message, "resubmission": refusal })),
            )
                .into_response());
        }
    };
    let turn_model = model_spec_for_request(
        &state.selected_model(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    state.set_selected_model(ModelSelection::from_spec(&turn_model));
    state.sessions.touch(&session_id);
    let turn_id = TurnId::from(format!("workbench-turn-{}", uuid::Uuid::new_v4()));
    state.trace_for_session(
        &session_id,
        "api.turns.stopped_partial.continue",
        json!({
            "stopped_turn": stopped_turn,
            "partial_digest": partial.digest.to_hex(),
            "turn_id": turn_id,
            "omissions": &resubmission.omissions,
        }),
    );
    // A running turn cannot take this input, and queuing it behind one would
    // quote a partial the running turn has already moved past.
    let follow_up = request.text.trim().to_string();
    let cleanup = ActiveTurnSubmissionGuard::user_turn(&state, &session_id, &turn_id);
    match state.active_turns.try_insert_with_prompt_for_idle_session(
        &session_id,
        &turn_id,
        WorkbenchTurnKind::User,
        Some(follow_up.clone()),
        None,
    ) {
        ActiveTurnClaim::Claimed => {}
        ActiveTurnClaim::Busy => {
            cleanup.complete();
            return Err(AppError::conflict(
                "a turn is running; continue from the stopped turn once it settles",
            ));
        }
        ActiveTurnClaim::Refused(retirement) => {
            cleanup.complete();
            return Err(state.retirement_fence_refusal(
                &session_id,
                "api.turns.stopped_partial.continue",
                retirement,
            ));
        }
    }
    let lash::Resubmission { input, omissions } = resubmission;
    let request = restate::UserTurnRequest {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        text: follow_up,
        model: ModelSelection::from_spec(&turn_model),
        attachment_id: None,
    };
    drop(
        tokio::spawn(commit_and_start_resubmission(
            state, cleanup, request, input,
        ))
        .await
        .map_err(|error| AppError::internal(format!("turn admission task failed: {error}")))??,
    );
    Ok(Json(ContinueInContextResponse::Sent { turn_id, omissions }).into_response())
}

/// The user row shows exactly what the model is sent: the labeled excerpt and
/// the follow-up.
fn resubmitted_text(input: &TurnInput) -> String {
    input
        .items
        .iter()
        .filter_map(|item| match item {
            lash::InputItem::Text { text } => Some(text.as_str()),
            lash::InputItem::Attachment { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn commit_and_start_resubmission(
    state: AppState,
    cleanup: ActiveTurnSubmissionGuard,
    request: restate::UserTurnRequest,
    input: TurnInput,
) -> Result<tokio::task::JoinHandle<restate::TurnSettlement>, AppError> {
    state.push_message_with_id_for_session(
        &request.session_id,
        workbench_turn_user_message_id(&request.turn_id),
        "user",
        resubmitted_text(&input),
    );
    state.trace_for_session(
        &request.session_id,
        "api.turn.admission_committed",
        json!({ "turn_id": request.turn_id }),
    );
    let follower = restate::start_user_turn_with_input(&state, request, input).await?;
    cleanup.complete();
    Ok(follower)
}

#[cfg(test)]
#[path = "tests/stopped_partial.rs"]
mod tests;
