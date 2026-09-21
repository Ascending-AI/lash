use super::*;
use lash::SessionId;
use lash::TurnId;

/// What the workbench tells the operator when a cancellation was recorded, the
/// turn's route was pruned, and no terminal ever attached.
///
/// Byte-identical to the sentence the page appended as a loose DOM node before
/// this became a projection row, so the break-glass runbook keeps gating on the
/// same words (FIG-3163).
pub(crate) const UNKNOWN_TURN_TERMINAL_NOTE: &str = "turn route cleared · terminal outcome unknown";

/// One turn whose terminal outcome this process does not know.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct UnknownTurnTerminal {
    pub(crate) turn_id: TurnId,
    /// The operator-facing sentence, carried with the record so every reader —
    /// the page, a later `/api/state` reader, a test — says the same thing.
    pub(crate) note: &'static str,
    /// The cancellation that was recorded before the route was pruned. It is
    /// the only thing anyone knows about how this turn ended.
    pub(crate) cancellation: lash::TurnCancellationEvidence,
    pub(crate) recorded_at_ms: i64,
}

/// The per-session ledger of unknown turn terminals.
///
/// In-process, like the rest of the workbench's host-owned view state: the
/// durable copy of this disclosure is the trace record emitted beside it, which
/// is what a reader who was not watching the page goes to.
#[derive(Clone, Debug, Default)]
pub(crate) struct UnknownTurnTerminals {
    inner: Arc<Mutex<BTreeMap<SessionId, Vec<UnknownTurnTerminal>>>>,
}

impl UnknownTurnTerminals {
    /// A repeated cancel request for the same turn discloses the same fact and must not stack
    /// rows in the timeline.
    pub(crate) fn record(&self, session_id: &SessionId, record: UnknownTurnTerminal) -> bool {
        let mut ledger = self.inner.lock_recover();
        let session = ledger.entry(session_id.clone()).or_default();
        if session
            .iter()
            .any(|existing| existing.turn_id == record.turn_id)
        {
            return false;
        }
        session.push(record);
        true
    }

    pub(crate) fn for_session(&self, session_id: &SessionId) -> Vec<UnknownTurnTerminal> {
        self.inner
            .lock_recover()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn remove(&self, session_id: &SessionId) {
        self.inner.lock_recover().remove(session_id);
    }
}

/// Disclose a route pruned without a terminal, on the projection and in the trace.
///
/// A pruned route with no terminal is the one outcome nobody can reconstruct
/// later: the turn is gone from the session's routing and nothing ever said how
/// it ended. Recording it here makes the disclosure outlive both the next
/// re-render of the page and the page itself (FIG-3163). A pending receipt
/// whose routing is retained is not this: that turn is still routable and may
/// still commit its own terminal.
pub(crate) fn record_unknown_turn_terminal(
    state: &AppState,
    address: &lash::TurnAddress,
    receipt: &TurnCancelReceipt,
    routing_retained: bool,
) {
    if routing_retained || !receipt.terminal_is_pending() {
        return;
    }
    let TurnCancelReceipt::CancellationRecordedTerminalPending { cancellation, .. } = receipt
    else {
        // `terminal_is_pending` is exactly that variant.
        return;
    };
    let unknown = UnknownTurnTerminal {
        turn_id: address.turn_id.clone(),
        note: UNKNOWN_TURN_TERMINAL_NOTE,
        cancellation: cancellation.evidence().clone(),
        recorded_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    if state
        .unknown_turn_terminals
        .record(&address.session_id, unknown.clone())
    {
        state.trace_for_session(
            &address.session_id,
            "turn.terminal_unknown",
            json!({
                "session_id": address.session_id,
                "turn_id": address.turn_id,
                "note": unknown.note,
                "cancellation": unknown.cancellation,
            }),
        );
    }
}
