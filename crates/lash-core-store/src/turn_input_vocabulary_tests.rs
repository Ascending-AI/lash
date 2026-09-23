use super::*;

fn active() -> TurnInputIngress {
    TurnInputIngress::active_turn("turn-a", TurnInputCheckpointBoundary::AfterWork)
}

fn next() -> TurnInputIngress {
    TurnInputIngress::next_turn()
}

#[test]
fn open_state_carries_the_admission_scope() {
    assert_eq!(
        TurnInputState::open(active()),
        TurnInputState::PendingActive(ActiveTurnIngress {
            turn_id: crate::TurnId::from("turn-a"),
            min_boundary: TurnInputCheckpointBoundary::AfterWork,
        })
    );
    assert_eq!(
        TurnInputState::open(next()),
        TurnInputState::DeferredNextTurn
    );
}

#[test]
fn from_persisted_accepts_every_legal_pair() {
    let legal = [
        (
            "pending_active",
            active(),
            TurnInputState::PendingActive(ActiveTurnIngress {
                turn_id: crate::TurnId::from("turn-a"),
                min_boundary: TurnInputCheckpointBoundary::AfterWork,
            }),
        ),
        (
            "accepted",
            active(),
            TurnInputState::Accepted(ActiveTurnIngress {
                turn_id: crate::TurnId::from("turn-a"),
                min_boundary: TurnInputCheckpointBoundary::AfterWork,
            }),
        ),
        (
            "deferred_next_turn",
            next(),
            TurnInputState::DeferredNextTurn,
        ),
        ("cancelled", active(), TurnInputState::Cancelled(active())),
        ("cancelled", next(), TurnInputState::Cancelled(next())),
        ("completed", active(), TurnInputState::Completed(active())),
        ("completed", next(), TurnInputState::Completed(next())),
    ];
    for (spelling, ingress, expected) in legal {
        assert_eq!(
            TurnInputState::from_persisted(spelling, ingress.clone()),
            Some(expected),
            "{spelling} under {ingress:?} must decode"
        );
    }
}

#[test]
fn from_persisted_rejects_every_check_illegal_pair() {
    let illegal = [
        ("pending_active", next()),
        ("deferred_next_turn", active()),
        ("accepted", next()),
    ];
    for (spelling, ingress) in illegal {
        assert_eq!(
            TurnInputState::from_persisted(spelling, ingress.clone()),
            None,
            "{spelling} under {ingress:?} must be refused"
        );
    }
    assert_eq!(TurnInputState::from_persisted("bogus", active()), None);
}

#[test]
fn state_kind_and_ingress_round_trip() {
    let states = [
        (
            TurnInputState::open(active()),
            TurnInputStateKind::PendingActive,
            active(),
        ),
        (
            TurnInputState::DeferredNextTurn,
            TurnInputStateKind::DeferredNextTurn,
            next(),
        ),
        (
            TurnInputState::open(active()).accepted().unwrap(),
            TurnInputStateKind::Accepted,
            active(),
        ),
        (
            TurnInputState::Cancelled(active()),
            TurnInputStateKind::Cancelled,
            active(),
        ),
        (
            TurnInputState::Completed(next()),
            TurnInputStateKind::Completed,
            next(),
        ),
    ];
    for (state, kind, ingress) in states {
        assert_eq!(state.kind(), kind);
        assert_eq!(state.as_str(), kind.as_str());
        assert_eq!(state.ingress(), ingress);
    }
}

#[test]
fn accepted_only_rebinds_active_turn_open_states() {
    let pending = TurnInputState::open(active());
    assert_eq!(
        pending.accepted(),
        Some(TurnInputState::Accepted(ActiveTurnIngress {
            turn_id: crate::TurnId::from("turn-a"),
            min_boundary: TurnInputCheckpointBoundary::AfterWork,
        }))
    );
    assert_eq!(TurnInputState::DeferredNextTurn.accepted(), None);
    assert_eq!(TurnInputState::Cancelled(next()).accepted(), None);
}

#[test]
fn state_spellings_stay_stable() {
    assert_eq!(
        TurnInputStateKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>(),
        [
            "pending_active",
            "deferred_next_turn",
            "accepted",
            "cancelled",
            "completed"
        ]
    );
    for kind in TurnInputStateKind::ALL {
        assert_eq!(
            TurnInputStateKind::from_wire_str(kind.as_str()),
            Some(*kind)
        );
    }
}

fn submission(ingress: TurnInputIngress, text: &str) -> PendingTurnInputDraft {
    PendingTurnInputDraft::new("session-a", ingress, TurnInput::text(text))
}

fn digest(draft: &PendingTurnInputDraft) -> String {
    draft.submission_digest().expect("digest a text submission")
}

#[test]
fn submission_digest_is_pinned() {
    assert_eq!(
        digest(&submission(active(), "hello")),
        "turn-input-submission:v1:blake3:9e21f41b5602f8fc559eb39be1aa37d809851e016e398c094ae90e1fa234a378"
    );
    assert_eq!(
        digest(&submission(next(), "hello")),
        "turn-input-submission:v1:blake3:23da023c1ede8d24906bc158f1149eecbdd8b742409841a3ccc7c4d7771e9981"
    );
}

#[test]
fn submission_digest_ignores_generated_and_lookup_identity() {
    let base = digest(&submission(active(), "hello"));
    assert_eq!(
        digest(
            &PendingTurnInputDraft::new("session-b", active(), TurnInput::text("hello"))
                .with_input_id("ti:generated")
                .with_source_key("host:retry")
        ),
        base,
        "session id, source key and input id locate the row; they are not the submission"
    );
}

#[test]
fn submission_digest_covers_every_submitted_field() {
    let base = digest(&submission(active(), "hello"));
    for (changed, field) in [
        (submission(active(), "hello!"), "input"),
        (
            submission(
                TurnInputIngress::active_turn("turn-b", TurnInputCheckpointBoundary::AfterWork),
                "hello",
            ),
            "target turn",
        ),
        (
            submission(
                TurnInputIngress::active_turn(
                    "turn-a",
                    TurnInputCheckpointBoundary::BeforeCompletion,
                ),
                "hello",
            ),
            "minimum boundary",
        ),
        (submission(next(), "hello"), "scope"),
    ] {
        assert_ne!(digest(&changed), base, "the {field} is submission identity");
    }
}
