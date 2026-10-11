use super::*;

/// Commit the record that `run`'s admission exported the admission of the
/// trace scope it retained, discharging the export the admission owes
/// (FIG-5457).
pub(super) async fn record_turn_traced(
    cx: &ActorContext,
    session: &SessionId,
    run: &TurnId,
) -> Result<(), TurnError> {
    let mut tx = cx.begin().await?;
    tx.write(DomainWrite::Turn(TurnWrite::TraceExported {
        session: session.clone(),
        run: run.clone(),
    }));
    cx.commit(tx, CommitLabel::TURN_TRACED).await?;
    Ok(())
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
