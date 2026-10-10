use super::*;
use lash::SessionId;

pub(crate) fn turn_input_routes() -> Router<AppState> {
    Router::new()
        .route("/api/turn/input", post(enqueue_turn_input))
        .route("/api/turn/input/{input_id}", delete(cancel_pending_input))
        .route("/api/turn/input/{input_id}/edit", post(edit_pending_input))
}

// The workbench's turn-input ingress admission.
//
// Two routes admit an input into the session's durable ingress lane —
// `/api/turn/input`, where the client asked for it, and `/api/turn`, where a
// send arrived while a turn was running — so the admission body they share
// lives here rather than inside one of them.

pub(crate) async fn enqueue_turn_input(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TurnInputRequest>,
) -> Result<Json<TurnInputReceipt>, AppError> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let session_id = state.admit_session(&query, "api.turn.input").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurnInput {
            session_id: session_id.clone(),
        })?;
    // A next-turn input is a run the engine starts on its own.
    turns::watch_session_runs(&state, &session_id).await;
    let ingress = match request.ingress {
        TurnInputIngressRequest::ActiveTurn => {
            let Some(active) = state.active_turns.for_session(&session_id) else {
                return Err(AppError::conflict(
                    "inject now requires exactly one running turn",
                ));
            };
            lash::persistence::TurnInputIngress::active_turn(
                active.address.turn_id,
                lash::persistence::TurnInputCheckpointBoundary::AfterWork,
            )
        }
        TurnInputIngressRequest::NextTurn => lash::persistence::TurnInputIngress::next_turn(),
    };
    let receipt = admit_turn_input(
        &state,
        &session_id,
        text.clone(),
        lash::TurnInput::text(text),
        ingress,
        "api.turn.input",
    )
    .await?;
    Ok(Json(receipt))
}

pub(crate) async fn admit_queued_send(
    state: &AppState,
    session_id: &SessionId,
    text: String,
    attachment: Option<lash::attachments::AttachmentRef>,
) -> Result<Json<TurnAccepted>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurnInput {
            session_id: session_id.clone(),
        })?;
    turns::watch_session_runs(state, session_id).await;
    let mut input = lash::TurnInput::text(text.clone());
    if let Some(reference) = attachment {
        input = input.with_attachment(reference);
    }
    let receipt = admit_turn_input(
        state,
        session_id,
        text,
        input,
        lash::persistence::TurnInputIngress::next_turn(),
        "api.turn",
    )
    .await?;
    Ok(Json(TurnAccepted::queued(receipt)))
}

/// Admit `input` into the session's durable ingress lane and publish its receipt
/// to every viewer.
///
/// Both ingress surfaces share this: `/api/turn/input`, where the client asked
/// for a queued or injected input, and `/api/turn`, where a send arrived while a
/// turn was running and is admitted as the next turn's input instead of starting
/// a second one (FIG-1000). One body means the two cannot drift in what a viewer
/// is told about an accepted input.
pub(crate) async fn admit_turn_input(
    state: &AppState,
    session_id: &SessionId,
    text: String,
    input: lash::TurnInput,
    ingress: lash::persistence::TurnInputIngress,
    surface: &str,
) -> Result<TurnInputReceipt, AppError> {
    let source_id = lash::TurnId::prefixed("workbench-turn-input-", uuid::Uuid::new_v4());
    // The Durable Session never creates (ADR 0119), and the workbench admits
    // input for a session whose first turn may not have run yet. It creates the
    // session explicitly first — create-or-use, since the id may exist — which
    // builds no runtime, so the admission is not a side effect of the send.
    state
        .ensure_session(session_id)
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?;
    let acceptance = state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?
        .send(input)
        .ingress(ingress)
        .id(source_id)
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?
        .receipt()
        .clone();
    reject_if_active_turn_settled(state, &acceptance).await?;
    let accepted_state = lash::persistence::TurnInputState::open(acceptance.ingress.clone());
    let receipt = TurnInputReceipt {
        accepted: true,
        input_id: acceptance.input_id.to_string(),
        ingress: acceptance.ingress.clone(),
        state: accepted_state,
        text,
    };
    state.trace_for_session(
        session_id,
        "turn_input.enqueued",
        json!({
            "surface": surface,
            "receipt": serde_json::to_value(&receipt).unwrap_or(Value::Null),
        }),
    );
    state.publish_for_session_identified(
        session_id,
        format!("turn-input:{}", receipt.input_id),
        StreamItem::TurnInput {
            receipt: receipt.clone(),
        },
    );
    Ok(receipt)
}

pub(crate) async fn reject_if_active_turn_settled(
    state: &AppState,
    acceptance: &lash::TurnInputAcceptanceReceipt,
) -> Result<(), AppError> {
    let Some(turn_id) = acceptance.ingress.active_turn_id() else {
        return Ok(());
    };
    if state.active_turns.contains(&acceptance.session_id, turn_id) {
        return Ok(());
    }

    let session = state
        .open_session(&acceptance.session_id, "api.turn.input.cancel")
        .await
        .map_err(AppError::runtime)?;
    let outcome = session
        .durable()
        .cancel_pending_turn_inputs([lash::PendingTurnInputCancelTarget::input_id(
            acceptance.input_id.to_string(),
        )])
        .await
        .map_err(AppError::runtime)?;
    let outcome = outcome
        .into_iter()
        .next()
        .ok_or_else(|| AppError::internal("input withdrawal returned no receipt"))?
        .outcome;
    match outcome {
        lash::PendingTurnInputCancelOutcome::Cancelled(_)
        | lash::PendingTurnInputCancelOutcome::AlreadyCancelled(_) => Err(AppError::conflict(
            "the running turn settled before the input could be injected",
        )),
        lash::PendingTurnInputCancelOutcome::AlreadyAdmitted { .. }
        | lash::PendingTurnInputCancelOutcome::AlreadyCompleted(_) => Ok(()),
        // Audited: this is a locally synthesized reconciliation-invariant failure, not a propagated store error.
        lash::PendingTurnInputCancelOutcome::NotFound => Err(AppError::internal(format!(
            "active-turn input `{}` disappeared during settle reconciliation",
            acceptance.input_id
        ))),
    }
}

/// Session admission and authorization precede every queue mutation. The store
/// owns withdrawal, including the race with engine admission; the host returns
/// its typed answer instead of inferring success from an earlier read.
async fn pending_input_session(
    state: &AppState,
    query: &SessionQuery,
) -> Result<lash::DurableSession, AppError> {
    let session_id = state.admit_session(query, "api.turn.input.manage").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageTurnInputs {
            session_id: session_id.clone(),
        })?;
    state
        .core
        .session(session_id)
        .durable()
        .await
        .map_err(AppError::runtime)
}

pub(crate) async fn cancel_pending_input(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    AxumPath(input_id): AxumPath<String>,
) -> Result<Json<lash::PendingTurnInputCancelReceipt>, AppError> {
    let session = pending_input_session(&state, &query).await?;
    let receipt = session
        .cancel_pending_turn_inputs([lash::PendingTurnInputCancelTarget::input_id(input_id)])
        .await
        .map_err(AppError::runtime)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::internal("input withdrawal returned no receipt"))?;
    publish_input_withdrawals(&state, std::slice::from_ref(&receipt.outcome));
    Ok(Json(receipt))
}

pub(crate) async fn edit_pending_input(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    AxumPath(input_id): AxumPath<String>,
) -> Result<Json<lash::PendingTurnInputSuffixCancelOutcome>, AppError> {
    let session = pending_input_session(&state, &query).await?;
    let outcome = session
        .cancel_pending_turn_input_suffix(lash::PendingTurnInputCancelTarget::input_id(input_id))
        .await
        .map_err(AppError::runtime)?;
    if let lash::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = &outcome {
        publish_input_withdrawals(&state, outcomes);
    }
    Ok(Json(outcome))
}

fn publish_input_withdrawals(state: &AppState, outcomes: &[lash::PendingTurnInputCancelOutcome]) {
    for outcome in outcomes {
        let lash::PendingTurnInputCancelOutcome::Cancelled(input) = outcome else {
            continue;
        };
        let text = input
            .input
            .items
            .iter()
            .filter_map(|item| match item {
                lash::InputItem::Text { text } => Some(text.as_str()),
                lash::InputItem::Attachment { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let receipt = TurnInputReceipt {
            accepted: false,
            input_id: input.input_id.to_string(),
            ingress: input.state.ingress(),
            state: input.state.clone(),
            text,
        };
        state.trace_for_session(
            &input.session_id,
            "turn_input.cancelled",
            json!({ "outcome": outcome }),
        );
        // The existing input lane carries the durable cancelled state to all
        // viewers. No new persisted event shape or process-local queue exists.
        state.publish_for_session_identified(
            &input.session_id,
            format!("turn-input-cancelled:{}", input.input_id),
            StreamItem::TurnInput { receipt },
        );
    }
}
