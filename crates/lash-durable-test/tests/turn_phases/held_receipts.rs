//! Held-receipt laws (FIG-6969) for the scripted turn: its final answer
//! owes no callback, so its receipt is held for the turn's commit
//! (FIG-6955), and the turn leaves its phases without that commit.

use super::*;

/// [`Mode::CheckpointFault`]: the `BeforeCompletion` checkpoint the final
/// answer finishes through fails, once.
pub(super) fn checkpoint_fault(
    services: &L3Services,
    checkpoint: lash_core::CheckpointKind,
) -> Result<(), TurnError> {
    if services.mode != Mode::CheckpointFault
        || checkpoint != lash_core::CheckpointKind::BeforeCompletion
        || std::mem::replace(&mut services.seen.lock_recover().held_fault, true)
    {
        return Ok(());
    }
    Err(TurnError::Exec("the scripted checkpoint failed".to_owned()))
}

/// [`Mode::CancelWhileDeriving`]: the host cancels the turn, `Immediate`,
/// once, while its final answer derives: the store accepts the cancel
/// before the turn's commit opens.
pub(super) async fn cancel_while_deriving(services: &L3Services) {
    if services.mode != Mode::CancelWhileDeriving {
        return;
    }
    let deriving_the_final_answer = {
        let mut seen = services.seen.lock_recover();
        let last = seen.calls.last().is_some_and(|call| call.second);
        last && !std::mem::replace(&mut seen.held_fault, true)
    };
    if !deriving_the_final_answer {
        return;
    }
    let answer = request_turn_cancel(
        &services.backend(),
        TurnCancelRequest {
            session: session(),
            run: run(),
            request_id: "l3-cancel-while-deriving".to_owned(),
            origin: None,
            reason: Some("the host cancelled".to_owned()),
            undelivered: TurnCancelUndeliveredInputPolicy::Defer,
            mode: TurnCancelMode::Immediate,
        },
    )
    .await;
    assert!(answer.is_ok(), "the cancel is accepted: {answer:?}");
}

/// The held receipt commits on its own, as `model.answer`, before the turn
/// leaves its phases, and the final call is sent once:
///
/// - [`Mode::CheckpointFault`]: the next pass derives the receipt and the
///   turn commits once.
/// - [`Mode::CancelWhileDeriving`]: the turn's commit reads the cancel at
///   its open, and the turn ends in one `turn.cancel` with the paid
///   receipt, whose commit carries the call's sealed fact.
pub(super) fn laws(
    mode: Mode,
    seen: &Seen,
    committed: &dyn Fn(CommitLabel) -> usize,
) -> Vec<String> {
    let mut violations = Vec::new();
    if !seen.held_fault {
        violations.push(format!("{mode:?}: the fault never came"));
    }
    let sends = seen.calls.iter().filter(|call| call.second).count();
    if sends != 1 {
        violations.push(format!(
            "{mode:?}: the final call was sent {sends} times: {:?}",
            seen.calls
        ));
    }
    // The first call's receipt, then the held one's.
    let receipts = committed(CommitLabel::MODEL_ANSWER);
    if receipts != 2 {
        violations.push(format!("{mode:?}: {receipts} receipts committed"));
    }
    let (commits, cancels) = match mode {
        Mode::CancelWhileDeriving => (0, 1),
        _ => (1, 0),
    };
    let ended = (
        committed(CommitLabel::TURN_COMMIT),
        committed(CommitLabel::TURN_CANCEL),
    );
    if ended != (commits, cancels) {
        violations.push(format!(
            "{mode:?}: {} head commits and {} cancel terminals",
            ended.0, ended.1
        ));
    }
    violations
}
