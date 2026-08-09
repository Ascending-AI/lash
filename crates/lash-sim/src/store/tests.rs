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
    assert_eq!(summary.sessions[0].provider_outputs, vec!["one"]);
    assert_eq!(summary.sessions[1].provider_outputs, vec!["two"]);
    assert_ne!(
        summary.sessions[0].provider_outputs,
        summary.sessions[1].provider_outputs
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
        "durable.sleep.complete",
        json!({"durable_key": "sleep/session-001", "result": {"done": true}}),
    ));
    store.apply_boundary(&BoundaryEvent::new(
        "effect-1-replay",
        "session-001",
        BoundaryKind::DurableEffect,
        4,
        "durable.sleep.replay",
        json!({"durable_key": "sleep/session-001", "result": {"done": false}}),
    ));
    // Worker fencing is NOT abstractly projected (the abstract arm reports
    // identity only); the model reads the REAL reclaim/fence facts produced by
    // the live lease store, threaded in via `apply_observed_boundary`.
    store.apply_observed_boundary(
        &BoundaryEvent::new(
            "worker-1",
            "worker-001",
            BoundaryKind::Worker,
            5,
            "worker.stale-completion-rejected",
            json!({"session": "session-001"}),
        ),
        &json!({
            "worker_alias": "worker-001",
            "session": "session-001",
            "active_owner": { "incarnation_id": "worker-001:incarnation-002" },
            "active_fencing_token": 2,
            "lease_owner_changed": true,
            "stale_completion_rejected": true,
        }),
    );

    let summary = store.summary();
    assert_eq!(summary.sessions[0].observer_turn_indices, vec![1]);
    assert_eq!(summary.durable_effects[0].execution_count, 1);
    assert_eq!(summary.durable_effects[0].replay_count, 1);
    assert_eq!(summary.workers[0].stale_completion_rejections, 1);
    assert_eq!(summary.workers[0].lease_owner_changes, 1);
    assert_eq!(summary.workers[0].active_fencing_token, 2);
}

#[test]
fn abstract_worker_projection_fabricates_no_fencing() {
    // The abstract worker projection must NOT fabricate fencing: if the real
    // lease facts are never threaded in, the worker summary shows no fence
    // change and the worker oracle cannot pass.
    let mut store = ModelStore::default();
    let observed = store.project_boundary_observation(&BoundaryEvent::new(
        "worker-1",
        "worker-001",
        BoundaryKind::Worker,
        0,
        "worker.stale-completion-rejected",
        json!({"session": "session-001"}),
    ));
    assert!(observed.get("stale_completion_rejected").is_none());
    assert!(observed.get("lease_owner_changed").is_none());
    assert!(observed.get("active_fencing_token").is_none());
}
