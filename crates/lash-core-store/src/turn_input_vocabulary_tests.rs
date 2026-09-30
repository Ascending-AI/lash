use super::*;

#[test]
fn runtime_correlation_is_shared_by_clones_and_cleared_only_by_its_owner() {
    struct Correlation(String);
    struct OtherCorrelation;

    let mut context = TurnContext::new();
    context.set_runtime_correlation(Correlation("process-a".to_string()));
    let cloned = context.clone();
    let original = context.runtime_correlation::<Correlation>().unwrap();
    let shared = cloned.runtime_correlation::<Correlation>().unwrap();
    assert!(std::ptr::eq(original, shared));
    assert_eq!(shared.0, "process-a");
    assert!(context.runtime_correlation::<OtherCorrelation>().is_none());

    context.clear_runtime_correlation::<OtherCorrelation>();
    assert!(context.runtime_correlation::<Correlation>().is_some());
    context.clear_runtime_correlation::<Correlation>();
    assert!(context.runtime_correlation::<Correlation>().is_none());
    assert_eq!(
        cloned.runtime_correlation::<Correlation>().unwrap().0,
        "process-a"
    );
}

#[test]
fn durable_turn_input_drops_runtime_correlation_and_attempt_identity() {
    struct Correlation;

    let mut input = TurnInput::text("accepted words");
    input.trace_turn_id = Some(crate::TurnId::from("attempt-a"));
    input.turn_context.set_runtime_correlation(Correlation);
    let durable = input.durable_projection();
    assert!(durable.trace_turn_id.is_none());
    assert!(
        durable
            .turn_context
            .runtime_correlation::<Correlation>()
            .is_none()
    );
    assert!(matches!(&durable.items[..], [InputItem::Text { text }] if text == "accepted words"));
    assert!(
        input
            .turn_context
            .runtime_correlation::<Correlation>()
            .is_some()
    );

    let decoded: TurnInput = serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
    assert!(
        decoded
            .turn_context
            .runtime_correlation::<Correlation>()
            .is_none()
    );
    assert!(matches!(&decoded.items[..], [InputItem::Text { text }] if text == "accepted words"));
}

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

/// The provisioned acceptance id is a durable identity: a change to its
/// derivation (the domain, `EffectAddress::graph_key`, or the acceptance replay
/// key shape) renames the rows in-flight acceptances will look for on redrive,
/// and a redrive would then admit the same words a second time (FIG-3513).
/// A deliberate change must mint a new domain version and update this vector.
#[test]
fn provisioned_turn_input_id_is_pinned() {
    let address = crate::EffectAddress::new(
        crate::ExecutionScope::turn("session", "turn"),
        "session:turn:accept_turn_input",
    )
    .expect("valid acceptance address");
    assert_eq!(
        super::provisioned_turn_input_id(&address),
        "ti:6e69f3397990690f846f9b04b46cb7b3c7bdaf659b1a028ab0bc3fdd41730832"
    );
}
