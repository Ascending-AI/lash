use super::*;
use lash::SessionId;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", content = "cancellation", rename_all = "snake_case")]
pub(crate) enum RecordedTurnCancellation {
    Requested(lash::TurnCancellationEvidence),
    AlreadyRequested(lash::TurnCancellationEvidence),
    /// An Abort landed on a turn already holding a Stop (after-step) request
    /// and upgraded it.
    Escalated(lash::TurnCancellationEvidence),
}

impl RecordedTurnCancellation {
    pub(crate) fn evidence(&self) -> &lash::TurnCancellationEvidence {
        match self {
            Self::Requested(evidence)
            | Self::AlreadyRequested(evidence)
            | Self::Escalated(evidence) => evidence,
        }
    }
}

/// How the workbench asks a turn to stop. `stop` (after-step) lets the current
/// protocol iteration finish and stops at its step boundary; `abort`
/// (immediate) fires the cooperative token and backtracks to the last
/// checkpoint. An `abort` on a turn that already holds a `stop` escalates it.
/// The "escalate after N seconds" policy lives in the UI, not in Lash.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkbenchTurnCancelMode {
    #[default]
    Abort,
    Stop,
}

impl WorkbenchTurnCancelMode {
    pub(crate) fn lash_mode(self) -> lash::TurnCancelMode {
        match self {
            Self::Abort => lash::TurnCancelMode::Immediate,
            Self::Stop => lash::TurnCancelMode::AfterStep,
        }
    }

    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Abort => "workbench Abort control",
            Self::Stop => "workbench Stop control",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TurnCancelQuery {
    #[serde(flatten)]
    pub(crate) session: SessionQuery,
    #[serde(default)]
    pub(crate) mode: WorkbenchTurnCancelMode,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum TurnCancelReceipt {
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
    pub(crate) fn terminal_is_pending(&self) -> bool {
        matches!(self, Self::CancellationRecordedTerminalPending { .. })
    }
}

pub(crate) async fn cancel_turn(
    State(state): State<AppState>,
    Query(query): Query<TurnCancelQuery>,
) -> Result<(StatusCode, Json<TurnCancelResponse>), AppError> {
    let driver = state.core.turn_work_driver();
    cancel_turn_with_driver(state, query, &driver).await
}

pub(crate) async fn cancel_turn_with_driver(
    state: AppState,
    query: TurnCancelQuery,
    driver: &lash::TurnWorkDriver,
) -> Result<(StatusCode, Json<TurnCancelResponse>), AppError> {
    let session_id = state
        .admit_session(&query.session, "api.turn.cancel")
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::CancelTurn {
            session_id: SessionId::from(session_id.clone()),
        })?;
    let cancellations = state
        .cancel_turns_for_session_with_driver(
            &SessionId::from(session_id.clone()),
            driver,
            query.mode,
        )
        .await?;
    state.trace_for_session(
        &SessionId::from(session_id.clone()),
        "api.turn.cancel",
        json!({
            "session_id": session_id,
            "mode": query.mode.lash_mode(),
            "cancellations": cancellations,
        }),
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
pub(crate) async fn attach_recorded_cancel_terminal(
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
