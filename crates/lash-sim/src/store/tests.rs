use super::*;
use crate::scheduler::{BoundaryEvent, BoundaryKind};

#[test]
fn model_store_keeps_cross_session_outputs_isolated() {
    let mut store = ModelStore::default();
    store.apply_boundary(&BoundaryEvent::new(
        "open-1",
        "session-001",
        BoundaryKind::Ingress,
        0,
        "session.open",
        json!({}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "open-2",
        "session-002",
        BoundaryKind::Ingress,
        0,
        "session.open",
        json!({}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "p1",
        "session-001",
        BoundaryKind::Provider,
        1,
        "provider",
        json!({"text": "one"}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "p2",
        "session-002",
        BoundaryKind::Provider,
        1,
        "provider",
        json!({"text": "two"}),
    ));

    let summary = store.summary();
    assert_eq!(summary.session_count, 2);
    assert_eq!(summary.sessions[0].provider_turns[0].output, "one");
    assert_eq!(summary.sessions[1].provider_turns[0].output, "two");
    assert_ne!(
        summary.sessions[0].provider_turns,
        summary.sessions[1].provider_turns
    );
}

#[test]
fn model_store_projects_semantic_boundary_summaries() {
    let mut store = ModelStore::default();
    store.apply_boundary(&BoundaryEvent::new(
        "open-1",
        "session-001",
        BoundaryKind::Ingress,
        0,
        "session.open",
        json!({}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "provider-1",
        "session-001",
        BoundaryKind::Provider,
        1,
        "provider.chat.stream",
        json!({"text": "answer for session-001"}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "observer-1",
        "session-001",
        BoundaryKind::Observer,
        2,
        "observer.snapshot",
        json!({}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "effect-1",
        "session-001",
        BoundaryKind::DurableEffect,
        3,
        "durable.sleep.crash-redrive",
        json!({"durable_key": "sleep/session-001", "result": {"done": true}}),
    ));

    let summary = store.summary();
    assert_eq!(summary.sessions[0].observer_turn_indices, vec![1]);
    assert_eq!(summary.durable_effects[0].execution_count, 1);
    assert_eq!(summary.durable_effects[0].replay_count, 1);
}

#[test]
fn pending_input_cancellation_respects_admission_order_and_terminal_states() {
    for claim_first in [false, true] {
        let mut store = ModelStore::default();
        store.apply_boundary(&BoundaryEvent::new(
            "input",
            "session",
            BoundaryKind::QueuedIngress,
            0,
            "queued",
            json!({"ingress_mode": "next_turn"}),
        ));
        let admissions = [json!({"session": "session", "provider_boundary": "provider"})];
        let cancel = BoundaryEvent::new(
            "cancel",
            "session",
            BoundaryKind::Cancellation,
            1,
            "cancel",
            json!({"target": "input"}),
        );
        if claim_first {
            store.apply_provider_admissions(&admissions);
        }
        let first = store.apply_boundary(&cancel);
        assert_eq!(first["cancelled"], !claim_first);
        assert_eq!(
            first["cancel_outcome"],
            if claim_first {
                "already_claimed"
            } else {
                "cancelled"
            }
        );
        store.apply_provider_admissions(&admissions);
        store.apply_boundary(&BoundaryEvent::new(
            "provider",
            "session",
            BoundaryKind::Provider,
            2,
            "provider",
            json!({}),
        ));
        let repeated = store.apply_boundary(&cancel);
        assert_eq!(repeated["cancelled"], false);
        assert_eq!(
            repeated["cancel_outcome"],
            if claim_first {
                "already_completed"
            } else {
                "already_cancelled"
            }
        );
    }
}

#[test]
#[should_panic(
    expected = "queued-ingress boundary `queued-no-mode`: queued-ingress payload has no `ingress_mode` key"
)]
fn queued_ingress_without_a_mode_key_is_refused_rather_than_read_two_ways() {
    // On main this event was read two ways by one store: the projection
    // defaulted the absent key to next-turn (`input_state:
    // "deferred_next_turn"`) while `ModelPendingInput` defaulted it to
    // active-turn, so `queued_next_turn_boundaries` returned `[]` for the row
    // the same store had just described as deferred to the next turn. One
    // decoder now refuses the malformed boundary instead of inventing an
    // answer for it.
    let mut store = ModelStore::default();
    store.apply_boundary(&BoundaryEvent::new(
        "queued-no-mode",
        "session",
        BoundaryKind::QueuedIngress,
        0,
        "queued",
        json!({"source_key": "k"}),
    ));
}

#[test]
fn queued_ingress_mode_survives_the_round_trip_the_model_and_the_live_world_share() {
    for mode in [QueuedIngressMode::ActiveTurn, QueuedIngressMode::NextTurn] {
        let mut store = ModelStore::default();
        let observed = store.apply_boundary(&BoundaryEvent::new(
            "queued",
            "session",
            BoundaryKind::QueuedIngress,
            0,
            "queued",
            json!({"source_key": "k", "ingress_mode": mode.as_str()}),
        ));

        assert_eq!(
            QueuedIngressMode::from_payload(&observed),
            Ok(mode),
            "the projected observation must re-decode to the mode it was given"
        );
        assert_eq!(
            observed["input_state"],
            match mode {
                QueuedIngressMode::ActiveTurn => ACTIVE_TURN_INPUT_STATE,
                QueuedIngressMode::NextTurn => NEXT_TURN_INPUT_STATE,
            }
        );
        let held_for_next_turn = store.queued_next_turn_boundaries("session");
        assert_eq!(
            held_for_next_turn == vec!["queued".to_string()],
            mode == QueuedIngressMode::NextTurn,
            "the row the store holds for the next turn must agree with the mode it projected"
        );
    }
}
