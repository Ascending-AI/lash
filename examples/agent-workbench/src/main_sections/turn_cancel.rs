#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", content = "cancellation", rename_all = "snake_case")]
enum RecordedTurnCancellation {
    Requested(lash::TurnCancellationEvidence),
    AlreadyRequested(lash::TurnCancellationEvidence),
}

#[cfg(test)]
impl RecordedTurnCancellation {
    fn evidence(&self) -> &lash::TurnCancellationEvidence {
        match self {
            Self::Requested(evidence) | Self::AlreadyRequested(evidence) => evidence,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum TurnCancelReceipt {
    TerminalAttached {
        address: lash::TurnAddress,
        cancellation: RecordedTurnCancellation,
        terminal: lash::TurnTerminal,
    },
    CancellationRecordedTerminalPending {
        address: lash::TurnAddress,
        cancellation: RecordedTurnCancellation,
    },
    CompletionWonRace {
        address: lash::TurnAddress,
    },
    UnknownOrRevoked {
        address: lash::TurnAddress,
    },
}

impl TurnCancelReceipt {
    fn terminal_is_pending(&self) -> bool {
        matches!(self, Self::CancellationRecordedTerminalPending { .. })
    }
}

async fn cancel_turn(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<(StatusCode, Json<TurnCancelResponse>), AppError> {
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::CancelTurn {
            session_id: session_id.clone(),
        })?;
    let cancellations = state.cancel_turns_for_session(&session_id).await?;
    state.trace_for_session(
        &session_id,
        "api.turn.cancel",
        json!({ "session_id": session_id, "cancellations": cancellations }),
    );
    let status = if cancellations
        .iter()
        .any(TurnCancelReceipt::terminal_is_pending)
    {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    let response = TurnCancelResponse {
        accepted: !cancellations.is_empty(),
        cancellations,
    };
    Ok((status, Json(response)))
}

/// Only a typed, durably recorded cancellation can turn attachment expiry
/// into a pending receipt. Every other terminal-attachment failure remains an
/// HTTP error at its call site.
async fn attach_recorded_cancel_terminal(
    driver: &lash::TurnWorkDriver,
    address: lash::TurnAddress,
    cancellation: RecordedTurnCancellation,
) -> Result<TurnCancelReceipt, AppError> {
    match driver
        .await_terminal_with_timeout(&address, TURN_TERMINAL_ATTACH_TIMEOUT)
        .await
    {
        Ok(terminal) => Ok(TurnCancelReceipt::TerminalAttached {
            address,
            cancellation,
            terminal,
        }),
        Err(err) if err.code == lash::runtime::RuntimeErrorCode::TurnTerminalAwaitTimeout => {
            Ok(TurnCancelReceipt::CancellationRecordedTerminalPending {
                address,
                cancellation,
            })
        }
        // Audited: terminal attachment lowers Restate transport and revocation failures to RuntimeError without a tombstone cause.
        Err(err) => Err(AppError::internal(err.to_string())),
    }
}
