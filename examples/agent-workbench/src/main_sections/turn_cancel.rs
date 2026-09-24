use super::*;

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
    PolicyConflict {
        address: lash::TurnAddress,
        requested: lash::TurnCancelDisposition,
        accepted: lash::TurnCancellationEvidence,
    },
}

impl TurnCancelReceipt {
    pub(crate) fn terminal_is_pending(&self) -> bool {
        matches!(self, Self::CancellationRecordedTerminalPending { .. })
    }

    /// The turn this receipt answers for, whatever the outcome.
    pub(crate) fn address(&self) -> &lash::TurnAddress {
        match self {
            Self::TerminalAttached { address, .. }
            | Self::CancellationRecordedTerminalPending { address, .. }
            | Self::CompletionWonRace { address }
            | Self::UnknownOrRevoked { address }
            | Self::PolicyConflict { address, .. } => address,
        }
    }

    /// Whether a cancellation is in force for this turn.
    ///
    /// A completion that won the race and an address the driver does not know
    /// both leave nothing cancelled, so neither may cancel the turn's children.
    pub(crate) fn cancellation_is_in_force(&self) -> bool {
        match self {
            Self::TerminalAttached { .. }
            | Self::CancellationRecordedTerminalPending { .. }
            | Self::PolicyConflict { .. } => true,
            Self::CompletionWonRace { .. } | Self::UnknownOrRevoked { .. } => false,
        }
    }
}

/// Cancel the processes a cancelled turn is the durable parent of.
///
/// A turn that is foreground-awaiting a process it started holds the only
/// live reference to that work: cancelling the turn commits its terminal and
/// leaves the process running with nobody to receive its outcome (FIG-3155).
/// Before this, the only thing that ever cancelled the awaited subject was the
/// browser dropping the in-flight `/api/turn` request, which departs the
/// caller as a side effect — so an API-driven `abort` orphaned the process and
/// `stop` never reached it at all.
///
/// The parent-end sweep does not cover this: a `processes.start` from code
/// mode declares `OnParentEnd::Abandon`, so the turn's own parent-end ledger
/// row deliberately leaves the child alone. The turn control is the operator
/// saying "stop this work", which is a different act from the child-lifetime
/// policy the program declared, so the request is issued here.
///
/// Both modes issue the same request. A process cancellation is durable and
/// lands at the subject's next wake, never mid-step, so `stop`'s step-boundary
/// contract is honoured by the process's own cancel-at-wake rule rather than
/// by delaying the request.
pub(crate) async fn cancel_processes_parented_by_turn(
    state: &AppState,
    address: &lash::TurnAddress,
) -> Vec<String> {
    let filter = lash::process::ProcessListFilter {
        status: lash::process::ProcessStatusFilter::Any,
        parent_scope: Some(lash::process::ParentScope::turn(
            address.session_id.clone(),
            address.turn_id.clone(),
        )),
        ..lash::process::ProcessListFilter::default()
    };
    let observed = match state.process_observer.snapshot_all(&filter).await {
        Ok(observed) => observed,
        // Audited: process observation reads the global registry, which has no session tombstone contract.
        Err(error) => {
            state.trace_for_session(
                &address.session_id,
                "api.turn.cancel.process_scan_failed",
                json!({ "turn_id": address.turn_id, "error": error.to_string() }),
            );
            return Vec::new();
        }
    };
    let mut submitted = Vec::new();
    for item in observed {
        // A terminal row is settled and a row already carrying a request is
        // converging on its own; re-asking for either is pure noise.
        if item.process.terminal() || item.process.cancel_request.is_some() {
            continue;
        }
        let process_id = item.process.process_id.clone();
        let operation_id = format!("workbench-turn-cancel-process-{}", uuid::Uuid::new_v4());
        match restate::submit_process_cancel(
            state,
            restate::WorkbenchProcessCancelWorkflowRequest {
                operation_id,
                session_id: address.session_id.clone(),
                process_id: process_id.clone(),
            },
        )
        .await
        {
            Ok(_) => submitted.push(process_id.to_string()),
            // A turn cancellation that was accepted stays accepted: an
            // unreachable process workflow is reported, never folded back into
            // the turn control's own outcome.
            Err(error) => state.trace_for_session(
                &address.session_id,
                "api.turn.cancel.process_cancel_failed",
                json!({
                    "turn_id": address.turn_id,
                    "process_id": process_id,
                    "error": error.to_string(),
                }),
            ),
        }
    }
    submitted
}

#[cfg(test)]
pub(crate) async fn await_durable_turn_cancel_request(
    state: &AppState,
    address: &lash::TurnAddress,
) -> lash::TurnCancelRequestRecord {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(store) = state
                .session_store_factory
                .open_existing_store_by_id(&address.session_id)
                .await
                .expect("open durable cancellation store")
                && let Some(record) = store
                    .turn_cancel_request(address)
                    .await
                    .expect("read durable cancellation request")
            {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for durable cancellation request")
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
            session_id: session_id.clone(),
        })?;
    let cancellations = state
        .cancel_turns_for_session_with_driver(&session_id, driver, query.mode)
        .await?;
    let mut cancelled_processes = Vec::new();
    for receipt in cancellations
        .iter()
        .filter(|receipt| receipt.cancellation_is_in_force())
    {
        cancelled_processes
            .extend(cancel_processes_parented_by_turn(&state, receipt.address()).await);
    }
    state.trace_for_session(
        &session_id,
        "api.turn.cancel",
        json!({
            "session_id": session_id,
            "mode": query.mode.lash_mode(),
            "cancellations": cancellations,
            "cancelled_processes": cancelled_processes,
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
