use super::*;
use lash::SessionId;

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

#[expect(
    clippy::expect_used,
    reason = "the literal `image/png` is a valid MediaType by the attachments grammar"
)]
pub(crate) async fn admit_queued_send(
    state: &AppState,
    session_id: &SessionId,
    text: String,
    attachment_bytes: Option<Vec<u8>>,
) -> Result<Json<TurnAccepted>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurnInput {
            session_id: session_id.clone(),
        })?;
    let mut input = lash::TurnInput::text(text.clone());
    if let Some(attachment_bytes) = attachment_bytes {
        input = input.with_attachment(lash::direct::AttachmentSource::inline(
            lash::attachments::MediaType::parse("image/png").expect("workbench uploads only PNG"),
            attachment_bytes,
        ));
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
    let source_id = format!("workbench-turn-input-{}", uuid::Uuid::new_v4());
    // The Durable Session never creates (ADR 0097), and the workbench admits
    // input for a session whose first turn may not have run yet, so the
    // catalog entry is created here — explicitly, through the workbench's own
    // store factory — instead of being materialised as a side effect of the
    // enqueue. `create_store` is idempotent for an id that already exists.
    state
        .session_store_factory
        .create_store(&state_store_request(state, session_id))
        .await
        .map_err(|error| {
            state.session_admission_error(session_id, surface, lash::EmbedError::Store(error))
        })?;
    let acceptance = state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?
        .enqueue(input)
        .ingress(ingress)
        .id(source_id)
        .send()
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?;
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
        .cancel_pending_turn_input(&acceptance.input_id)
        .await
        .map_err(AppError::runtime)?;
    match outcome {
        lash::PendingTurnInputCancelOutcome::Cancelled(_)
        | lash::PendingTurnInputCancelOutcome::AlreadyCancelled(_) => Err(AppError::conflict(
            "the running turn settled before the input could be injected",
        )),
        lash::PendingTurnInputCancelOutcome::AlreadyClaimed { .. }
        | lash::PendingTurnInputCancelOutcome::AlreadyCompleted(_) => Ok(()),
        // Audited: this is a locally synthesized reconciliation-invariant failure, not a propagated store error.
        lash::PendingTurnInputCancelOutcome::NotFound => Err(AppError::internal(format!(
            "active-turn input `{}` disappeared during settle reconciliation",
            acceptance.input_id
        ))),
    }
}
