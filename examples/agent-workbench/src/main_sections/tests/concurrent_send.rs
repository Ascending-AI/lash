use super::*;
use lash::SessionId;
use lash::TurnId;

#[test]
fn active_turn_idle_claim_is_atomic_per_session() {
    let active_turns = ActiveTurns::default();
    let start = Arc::new(std::sync::Barrier::new(3));
    let claims = std::thread::scope(|scope| {
        let left = scope.spawn({
            let active_turns = active_turns.clone();
            let start = Arc::clone(&start);
            move || {
                start.wait();
                active_turns
                    .try_insert_for_idle_session(
                        &SessionId::from("race-session"),
                        &TurnId::from("left"),
                        WorkbenchTurnKind::User,
                    )
                    .is_claimed()
            }
        });
        let right = scope.spawn({
            let active_turns = active_turns.clone();
            let start = Arc::clone(&start);
            move || {
                start.wait();
                active_turns
                    .try_insert_for_idle_session(
                        &SessionId::from("race-session"),
                        &TurnId::from("right"),
                        WorkbenchTurnKind::User,
                    )
                    .is_claimed()
            }
        });
        start.wait();
        [
            left.join().expect("left claim"),
            right.join().expect("right claim"),
        ]
    });
    assert_eq!(claims.into_iter().filter(|claimed| *claimed).count(), 1);
    // One winner, and the slot is a lookup rather than a scan: the ledger is
    // keyed by session, so a second claim cannot land beside the first.
    assert!(
        active_turns
            .for_session(&SessionId::from("race-session"))
            .is_some()
    );
}
