// The workbench's turn-input ingress admission.
//
// Two routes admit an input into the session's durable ingress lane —
// `/api/turn/input`, where the client asked for it, and `/api/turn`, where a
// send arrived while a turn was running — so the admission body they share
// lives here rather than inside one of them.

async fn enqueue_turn_input(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TurnInputRequest>,
) -> Result<Json<TurnInputReceipt>, AppError> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurnInput {
            session_id: session_id.clone(),
        })?;
    let ingress = match request.ingress {
        TurnInputIngressRequest::ActiveTurn => {
            let active = state.active_turns.for_session(&session_id);
            let [address] = active.as_slice() else {
                return Err(AppError::conflict(
                    "inject now requires exactly one running turn",
                ));
            };
            lash::persistence::TurnInputIngress::active_turn(
                address.turn_id.clone(),
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

/// Admit `input` into the session's durable ingress lane and publish its receipt
/// to every viewer.
///
/// Both ingress surfaces share this: `/api/turn/input`, where the client asked
/// for a queued or injected input, and `/api/turn`, where a send arrived while a
/// turn was running and is admitted as the next turn's input instead of starting
/// a second one (FIG-1000). One body means the two cannot drift in what a viewer
/// is told about an accepted input.
async fn admit_turn_input(
    state: &AppState,
    session_id: &str,
    text: String,
    input: lash::TurnInput,
    ingress: lash::persistence::TurnInputIngress,
    surface: &str,
) -> Result<TurnInputReceipt, AppError> {
    let source_id = format!("workbench-turn-input-{}", uuid::Uuid::new_v4());
    let acceptance = state
        .core
        .enqueue_turn_input(session_id.to_string(), input, ingress, Some(source_id))
        .await
        .map_err(|error| state.session_admission_error(session_id, surface, error))?;
    reject_if_active_turn_settled(state, &acceptance).await?;
    let accepted_state = match acceptance.ingress {
        lash::persistence::TurnInputIngress::ActiveTurn { .. } => {
            lash::persistence::TurnInputState::PendingActive
        }
        lash::persistence::TurnInputIngress::NextTurn => {
            lash::persistence::TurnInputState::DeferredNextTurn
        }
    };
    let receipt = TurnInputReceipt {
        accepted: true,
        input_id: acceptance.input_id.clone(),
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

async fn reject_if_active_turn_settled(
    state: &AppState,
    acceptance: &lash::TurnInputAcceptanceReceipt,
) -> Result<(), AppError> {
    let Some(turn_id) = acceptance.ingress.active_turn_id() else {
        return Ok(());
    };
    if state
        .active_turns
        .contains(&acceptance.session_id, turn_id)
    {
        return Ok(());
    }

    let session = state
        .open_session(&acceptance.session_id)
        .await
        .map_err(AppError::runtime)?;
    let outcome = session
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
