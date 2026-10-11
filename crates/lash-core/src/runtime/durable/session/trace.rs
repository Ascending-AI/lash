use super::*;

/// Settle `candidate`, the trace admission `turn.admit` retains, on that
/// commit's result: handed to its adapter once it landed, or when its
/// acknowledgement was lost, and refused when nothing landed. A turn's
/// admission is exported with its first phase commit
/// ([`export_owed_admission`]), never at its admission: an owner lost
/// between the two exported nothing, so the owner that takes the turn over
/// exports it once (FIG-5998).
pub(super) fn defer_turn_admission<T>(
    candidate: Box<dyn lash_trace::TraceAdmissionCandidate>,
    committed: &Result<T, DurableError>,
) {
    match committed {
        Ok(_) | Err(DurableError::AckLost { .. }) => candidate.defer(),
        Err(_) => candidate.settle(lash_trace::TraceCandidateOutcome::Refused),
    }
}

/// Export the admission of the trace scope `row`'s admission retained, if
/// the turn still owes it: no commit recorded it exported. The caller is
/// about to submit the turn's next commit, the one that advances or ends
/// it, and calls this immediately before: a commit that lands has its
/// export made, whatever becomes of its owner. `true` when the export was
/// owed, so a commit that advances the turn records it
/// ([`TurnWrite::TraceExported`]); one that ends the turn drops the row
/// that would hold the record. A commit that fails leaves the export owed,
/// and the adapter's memory dedupes the retry (FIG-5998).
pub(in crate::runtime::durable) fn export_owed_admission(
    tracing: Option<&crate::trace::TraceRuntime>,
    row: &TurnRow,
) -> bool {
    if row.trace_exported {
        return false;
    }
    let Some(scope) = row
        .admission
        .trace()
        .filter(|scope| scope.anchor.context().is_some())
    else {
        return false;
    };
    if let Some(tracing) = tracing {
        tracing.scopes().export_admitted(scope);
    }
    true
}

/// Settle `candidate`, the trace admission a commit retains, on that
/// commit's result: selected once it landed, deferred to its adapter when
/// its acknowledgement was lost (a reader of the retained admission then
/// reconciles it), and refused when nothing landed (FIG-5395).
pub(in crate::runtime::durable) fn settle_trace_admission<T>(
    candidate: Box<dyn lash_trace::TraceAdmissionCandidate>,
    committed: &Result<T, DurableError>,
) {
    match committed {
        Ok(_) => candidate.settle(lash_trace::TraceCandidateOutcome::Selected),
        Err(DurableError::AckLost { .. }) => candidate.defer(),
        Err(_) => candidate.settle(lash_trace::TraceCandidateOutcome::Refused),
    }
}
