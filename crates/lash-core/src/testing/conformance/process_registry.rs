//! [`ProcessRegistry`] conformance: registration, events, wakes, grants,
//! and lease fencing.

use super::process_change_feed::process_change_feed_never_misses_concurrent_terminal_writers;
use super::process_coordination::{
    awaiter_await_event_never_returns_events_at_or_before_cursor,
    awaiter_cross_task_completion_resolves_promptly, prune_never_touches_non_terminal_rows,
    prune_removes_terminal_processes_older_than_cutoff, prune_respects_filter,
    prune_respects_leases_grants_and_wake_acks, prune_respects_watermark,
    watched_decorator_preserves_registry_semantics,
};
use super::process_filters::list_processes_filters_by_enriched_fields;
use super::process_references::live_reference_summary_tracks_non_terminal_reference_counts;
use super::*;
use crate::ProcessCompletionOutcome;
use crate::TestProcessRegistryWriteExt;

mod completion_authority;
mod execution_fencing;
mod fold;
mod lease_reclaim;
/// Run the full [`ProcessRegistry`] conformance suite against the backend
/// produced by `make`. `make` must return a fresh, empty registry on each call.
pub async fn process_registry<F>(make: F)
where
    F: Fn() -> Arc<dyn ProcessRegistry>,
{
    process_registry_with_expected_durability(make, crate::DurabilityTier::Inline).await;
}
/// Run the full [`ProcessRegistry`] suite plus durable reopen checks.
pub async fn process_registry_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableProcessRegistry,
{
    process_registry_with_expected_durability(|| make().open, crate::DurabilityTier::Durable).await;
    process_registry_survives_reopen(make()).await;
}
/// Run the full [`ProcessRegistry`] conformance suite against a backend with an
/// explicit expected durability tier.
pub async fn process_registry_with_expected_durability<F>(
    make: F,
    expected_tier: crate::DurabilityTier,
) where
    F: Fn() -> Arc<dyn ProcessRegistry>,
{
    process_registry_reports_declared_durability(make(), expected_tier).await;
    fold::process_record_is_the_fold_of_its_event_log(make()).await;
    fold::owner_bound_handover_resume_is_folded_and_replayable(make()).await;
    registration_is_idempotent_and_hash_conflicts_fail(make()).await;
    external_refs_and_handle_grant_membership_round_trip(make()).await;
    validates_custom_events_and_materializes_wakes(make()).await;
    custom_wake_events_preserve_typed_provenance_and_replay(make()).await;
    event_streams_filter_order_and_wait_without_leaking_old_events(make()).await;
    wake_semantics_matrix_materializes_declared_wakes(make()).await;
    keyed_events_materialize_idempotent_wakes(make()).await;
    wake_semantic_events_without_target_record_without_delivery(make()).await;
    terminal_and_cancel_events_require_keys(make()).await;
    await_reads_terminal_materialized_output(make()).await;
    disposition_first_started_and_abandon_request_persist(make()).await;
    execution_fencing::respects_recovery_disposition(make()).await;
    abandoned_terminal_round_trips_and_pins_writer_rules(make()).await;
    completion_authority::completion_authority_validated_against_disposition(make()).await;
    completion_authority::completion_authority_reads_live_disposition_not_stale(make()).await;
    wait_state_round_trips_filters_and_clears_on_terminal(make()).await;
    list_processes_filters_by_status_and_waiting(make()).await;
    list_processes_filters_by_enriched_fields(make()).await;
    live_reference_summary_tracks_non_terminal_reference_counts(make()).await;
    process_change_feed_orders_resumes_and_includes_terminal_transitions(make()).await;
    process_change_feed_never_misses_concurrent_terminal_writers(make()).await;
    count_and_recent_events_match_the_log(make()).await;
    transfer_handle_grants_moves_addressability(make()).await;
    multiple_sessions_can_hold_grants(make()).await;
    processes_can_exist_with_zero_grants(make()).await;
    delete_session_revokes_handles_by_session(make()).await;
    list_non_terminal_excludes_terminal_processes(make()).await;
    list_live_handle_grants_excludes_terminal_history(make()).await;
    active_process_lease_fences_competing_owner(make()).await;
    superseded_process_lease_cannot_renew(make()).await;
    renewed_process_lease_survives_original_expiry(make()).await;
    completed_lease_releases_and_reclaim_bumps_fencing(make()).await;
    stale_lease_completion_cannot_release_live_lease(make()).await;
    stale_process_lease_cannot_complete_or_disturb_current_owner(make()).await;
    expired_process_lease_cannot_complete(make()).await;
    execution_fencing::leased_terminal_replay_returns_stored_record(make()).await;
    lease_reclaim::process_lease_reclaim_contract(make()).await;
    prune_removes_terminal_processes_older_than_cutoff(make()).await;
    awaiter_cross_task_completion_resolves_promptly(make()).await;
    awaiter_await_event_never_returns_events_at_or_before_cursor(make()).await;
    watched_decorator_preserves_registry_semantics(make()).await;
    prune_respects_leases_grants_and_wake_acks(make()).await;
    prune_respects_filter(make()).await;
    prune_respects_watermark(make()).await;
    prune_never_touches_non_terminal_rows(make()).await;
}

pub(super) fn process_lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

const EXPIRING_PROCESS_LEASE_TTL_MS: u64 = 20;

async fn claim_process_lease_after_expiry(
    registry: &dyn ProcessRegistry,
    process_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> crate::ProcessLease {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match registry
            .claim_process_lease(process_id, owner, lease_ttl_ms)
            .await
            .expect("claim after lease expiry resolves")
        {
            crate::ProcessLeaseClaimOutcome::Acquired(lease) => return lease,
            crate::ProcessLeaseClaimOutcome::Busy { .. }
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            crate::ProcessLeaseClaimOutcome::Busy { holder } => {
                panic!("lease was not claimable before the expiry deadline; holder: {holder:?}");
            }
        }
    }
}

fn local_process_lease_owner(
    owner_id: &str,
    host_id: &str,
    boot_id: &str,
    pid: u32,
    process_start: &str,
) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity {
        owner_id: owner_id.to_string(),
        incarnation_id: format!("{owner_id}:incarnation"),
        liveness: crate::LeaseOwnerLiveness::local_process_for_test(
            host_id,
            boot_id,
            pid,
            process_start,
        ),
    }
}

pub(super) fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryDisposition::ExternallyOwned,
        ProcessProvenance::host(),
    )
}

fn owner_bound_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryDisposition::OwnerBound,
        ProcessProvenance::host(),
    )
}

pub(super) fn rerunnable_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryDisposition::Rerunnable,
        ProcessProvenance::host(),
    )
}

pub(super) fn wake_event_type(name: &str) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                dedupe_key: ProcessWakeDedupeKey::EventIdentity,
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }
}

fn wake_event_type_with(name: &str, wake: ProcessWakeSpec) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(wake),
            ..ProcessEventSemanticsSpec::default()
        },
    }
}

pub(super) fn plain_event_type(name: &str) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: LashSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }
}

async fn registration_is_idempotent_and_hash_conflicts_fail(registry: Arc<dyn ProcessRegistry>) {
    let first = registry
        .register_process(registration("proc-idempotent"))
        .await
        .expect("first register");
    let second = registry
        .register_process(registration("proc-idempotent"))
        .await
        .expect("replay register");
    assert_eq!(
        first.registration_hash, second.registration_hash,
        "identical registration must be idempotent"
    );
    assert!(
        registry
            .register_process(
                registration("proc-idempotent")
                    .with_extra_event_types([wake_event_type("producer.wake")]),
            )
            .await
            .is_err(),
        "a different registration under the same id must fail with a hash conflict"
    );
}

async fn process_registry_reports_declared_durability(
    registry: Arc<dyn ProcessRegistry>,
    expected_tier: crate::DurabilityTier,
) {
    assert_eq!(
        registry.durability_tier(),
        expected_tier,
        "process registry conformance must pin the backend's declared durability tier"
    );
}

async fn external_refs_and_handle_grant_membership_round_trip(registry: Arc<dyn ProcessRegistry>) {
    assert!(
        registry
            .set_external_ref(
                "missing-process",
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "missing".to_string(),
                    metadata: None,
                },
            )
            .await
            .is_err(),
        "setting an external ref for an unknown process must fail"
    );

    registry
        .register_process(registration("proc-external-ref"))
        .await
        .expect("register process");
    let external_ref = ProcessExternalRef {
        backend: "worker".to_string(),
        id: "job-123".to_string(),
        metadata: Some(serde_json::json!({ "queue": "critical" })),
    };
    let updated = registry
        .set_external_ref("proc-external-ref", external_ref.clone())
        .await
        .expect("set external ref");
    assert_eq!(updated.external_ref, Some(external_ref.clone()));
    let repeated = registry
        .set_external_ref("proc-external-ref", external_ref.clone())
        .await
        .expect("repeat identical external ref");
    assert_eq!(
        serde_json::to_value(&repeated).expect("serialize repeated process record"),
        serde_json::to_value(&updated).expect("serialize updated process record"),
        "repeating the same external ref must return the existing record unchanged"
    );
    let conflicting_external_ref = ProcessExternalRef {
        backend: "worker".to_string(),
        id: "job-456".to_string(),
        metadata: Some(serde_json::json!({ "queue": "critical" })),
    };
    assert!(
        registry
            .set_external_ref("proc-external-ref", conflicting_external_ref)
            .await
            .is_err(),
        "changing an already-set external ref must fail"
    );
    let metadata_conflicting_external_ref = ProcessExternalRef {
        backend: "worker".to_string(),
        id: "job-123".to_string(),
        metadata: Some(serde_json::json!({ "queue": "standard" })),
    };
    assert!(
        registry
            .set_external_ref("proc-external-ref", metadata_conflicting_external_ref)
            .await
            .is_err(),
        "changing only external ref metadata must fail"
    );
    assert_eq!(
        registry
            .get_process("proc-external-ref")
            .await
            .expect("process after external ref")
            .external_ref,
        Some(external_ref),
        "external ref must persist on the process record"
    );

    let owner = SessionScope::new("grant-owner");
    assert!(
        !registry
            .has_handle_grant(&owner, "proc-external-ref")
            .await
            .expect("missing grant check"),
        "has_handle_grant must be false before grant_handle"
    );
    registry
        .grant_handle(
            &owner,
            "proc-external-ref",
            ProcessHandleDescriptor::new(Some("test"), Some("external ref")),
        )
        .await
        .expect("grant handle");
    assert!(
        registry
            .has_handle_grant(&owner, "proc-external-ref")
            .await
            .expect("present grant check"),
        "has_handle_grant must be true after grant_handle"
    );
    registry
        .revoke_handle(&owner, "proc-external-ref")
        .await
        .expect("revoke handle");
    assert!(
        !registry
            .has_handle_grant(&owner, "proc-external-ref")
            .await
            .expect("revoked grant check"),
        "has_handle_grant must be false after revoke_handle"
    );
    assert!(
        registry
            .list_handle_grants(&owner)
            .await
            .expect("list grants after revoke")
            .is_empty(),
        "revoked handles must disappear from list_handle_grants"
    );
}

async fn validates_custom_events_and_materializes_wakes(registry: Arc<dyn ProcessRegistry>) {
    let target_scope = SessionScope::new("s1");
    let mut properties = serde_json::Map::new();
    properties.insert("line".to_string(), serde_json::json!({ "type": "string" }));
    properties.insert(
        "wake_input".to_string(),
        serde_json::json!({ "type": "string" }),
    );
    let event_type = ProcessEventType {
        name: "producer.line".to_string(),
        payload_schema: LashSchema::object(properties, vec!["line".to_string()]),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                dedupe_key: ProcessWakeDedupeKey::EventIdentity,
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    };
    registry
        .register_process(registration("proc-1").with_extra_event_types([event_type]))
        .await
        .expect("register");

    let event = registry
        .append_event(
            "proc-1",
            ProcessEventAppendRequest::new(
                "producer.line",
                serde_json::json!({
                    "line": "deploy failed",
                    "wake_input": "Process event: deploy failed"
                }),
            )
            .with_wake_target_scope(target_scope),
        )
        .await
        .expect("append");

    assert_eq!(event.event.sequence, 1, "first event is sequence 1");
    assert_eq!(
        event
            .event
            .semantics
            .wake
            .as_ref()
            .map(|wake| wake.input.as_str()),
        Some("Process event: deploy failed"),
        "wake input materialized from the declared selector"
    );
    assert_eq!(
        registry
            .wake_events_after("proc-1", 0)
            .await
            .expect("wake events")
            .len(),
        1
    );
    registry
        .ack_wake("proc-1", event.event.sequence)
        .await
        .expect("ack wake");
    assert!(
        registry
            .wake_events_after("proc-1", 0)
            .await
            .expect("wake events")
            .is_empty(),
        "ack_wake must suppress the acked wake from wake_events_after"
    );
    assert!(
        registry
            .append_event(
                "proc-1",
                ProcessEventAppendRequest::new(
                    "producer.line",
                    serde_json::json!({ "wake_input": "missing required line" }),
                ),
            )
            .await
            .is_err(),
        "payload missing a required field must be rejected"
    );
}

async fn custom_wake_events_preserve_typed_provenance_and_replay(
    registry: Arc<dyn ProcessRegistry>,
) {
    let target_scope = SessionScope::for_agent_frame("target-session", "target-frame");
    let target_scope_id = target_scope.id();
    let process_caused_by = CausalRef::SessionNode {
        session_id: "target-session".to_string(),
        node_id: "trigger:button".to_string(),
    };
    let event_type = wake_event_type_with(
        "producer.custom_wake",
        ProcessWakeSpec {
            when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
            input: ProcessValueSelector::Pointer("/wake_input".to_string()),
            dedupe_key: ProcessWakeDedupeKey::EventIdentity,
        },
    );
    registry
        .register_process(
            registration("proc-provenance")
                .with_extra_event_types([event_type])
                .with_process_provenance(
                    ProcessProvenance::session(SessionScope::new("owner-session"))
                        .with_caused_by(Some(process_caused_by.clone())),
                ),
        )
        .await
        .expect("register");

    let request = ProcessEventAppendRequest::new(
        "producer.custom_wake",
        serde_json::json!({
            "line": "build failed",
            "wake_input": "custom wake: build failed",
        }),
    )
    .with_replay_key("custom-wake:build-failed")
    .with_wake_target_scope(target_scope);
    let first = registry
        .append_event("proc-provenance", request.clone())
        .await
        .expect("append");
    let replay = registry
        .append_event("proc-provenance", request)
        .await
        .expect("replay append");

    assert_eq!(first.event.sequence, 1);
    assert_eq!(replay.event.sequence, first.event.sequence);
    assert_eq!(
        registry
            .events_after("proc-provenance", 0)
            .await
            .expect("events")
            .len(),
        1,
        "a replayed custom wake event must not append a second event row"
    );
    assert_eq!(first.event.invocation.scope, RuntimeScope::new("runtime"));
    assert!(matches!(
        &first.event.invocation.subject,
        RuntimeSubject::ProcessEvent {
            process_id,
            sequence: 1,
            event_type,
        } if process_id == "proc-provenance" && event_type == "producer.custom_wake"
    ));
    assert_eq!(
        first.event.invocation.caused_by,
        Some(CausalRef::Process {
            process_id: "proc-provenance".to_string()
        })
    );
    assert_eq!(
        first
            .event
            .invocation
            .replay
            .as_ref()
            .map(|replay| replay.key.as_str()),
        Some("custom-wake:build-failed")
    );

    let wake = first.wake_delivery.expect("wake delivery");
    assert_eq!(wake.event_type, "producer.custom_wake");
    assert_eq!(wake.event_invocation, first.event.invocation);
    assert_eq!(wake.process_caused_by, Some(process_caused_by));
    assert_eq!(wake.target_session_id, "target-session");
    assert_eq!(wake.target_scope_id, target_scope_id);
    assert_eq!(wake.process_id, "proc-provenance");
    assert_eq!(wake.sequence, first.event.sequence);
    assert_eq!(wake.dedupe_key, "proc-provenance:1");
    assert_eq!(wake.input, "custom wake: build failed");
    assert_eq!(
        replay
            .wake_delivery
            .expect("replayed wake delivery")
            .wake_id,
        wake.wake_id,
        "replaying a wake event must re-materialize the same wake identity"
    );
}

async fn event_streams_filter_order_and_wait_without_leaking_old_events(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (registry, hub) = crate::watch_process_registry(registry);
    let awaiter = crate::ProcessAwaiter::new(Arc::clone(&registry), hub);
    registry
        .register_process(registration("proc-stream").with_extra_event_types([
            plain_event_type("producer.line"),
            wake_event_type("producer.wake"),
            plain_event_type("producer.future"),
        ]))
        .await
        .expect("register");
    registry
        .append_event(
            "proc-stream",
            ProcessEventAppendRequest::new("producer.line", serde_json::json!({"line": "one"})),
        )
        .await
        .expect("append line one");
    registry
        .append_event(
            "proc-stream",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "wake two"}),
            )
            .with_wake_target_scope(SessionScope::new("root")),
        )
        .await
        .expect("append wake");
    registry
        .append_event(
            "proc-stream",
            ProcessEventAppendRequest::new("producer.line", serde_json::json!({"line": "three"})),
        )
        .await
        .expect("append line three");

    let after_one = registry
        .events_after("proc-stream", 1)
        .await
        .expect("events after one");
    assert_eq!(
        after_one
            .iter()
            .map(|event| (event.sequence, event.event_type.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "producer.wake"), (3, "producer.line")],
        "events_after must preserve sequence order and exclude older events"
    );
    assert!(
        registry
            .events_after("proc-stream", 3)
            .await
            .expect("events after three")
            .is_empty(),
        "events_after must not leak events at or before the cursor"
    );
    let wake_after_one = registry
        .wake_events_after("proc-stream", 1)
        .await
        .expect("wake events after one");
    assert_eq!(
        wake_after_one
            .iter()
            .map(|event| (event.sequence, event.event_type.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "producer.wake")],
        "wake_events_after must filter to unacked wake events after the cursor"
    );
    assert!(
        registry
            .wake_events_after("proc-stream", 2)
            .await
            .expect("wake events after wake")
            .is_empty(),
        "wake_events_after must not return the cursor event itself"
    );
    let immediate = awaiter
        .await_event("proc-stream", "producer.line", 1)
        .await
        .expect("immediate wait");
    assert_eq!(
        immediate.sequence, 3,
        "ProcessAwaiter::await_event must return an existing matching event immediately"
    );

    let waiter_awaiter = awaiter.clone();
    let waiter = crate::task::spawn(async move {
        waiter_awaiter
            .await_event("proc-stream", "producer.future", 3)
            .await
            .expect("future wait")
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    registry
        .append_event(
            "proc-stream",
            ProcessEventAppendRequest::new("producer.future", serde_json::json!({"line": "four"})),
        )
        .await
        .expect("append future event");
    let future = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("future wait timeout")
        .expect("future waiter task");
    assert_eq!(future.sequence, 4);
}

async fn wake_semantics_matrix_materializes_declared_wakes(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(
            registration("proc-wake-matrix").with_extra_event_types([
                wake_event_type_with(
                    "matrix.when_false",
                    ProcessWakeSpec {
                        when: Some(ProcessValueSelector::Const(serde_json::json!(false))),
                        input: ProcessValueSelector::Const(serde_json::json!("must not wake")),
                        dedupe_key: ProcessWakeDedupeKey::EventIdentity,
                    },
                ),
                wake_event_type_with(
                    "matrix.payload",
                    ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Payload,
                        dedupe_key: ProcessWakeDedupeKey::EventIdentity,
                    },
                ),
                wake_event_type_with(
                    "matrix.const_input",
                    ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Const(serde_json::json!(
                            "constant wake input"
                        )),
                        dedupe_key: ProcessWakeDedupeKey::EventIdentity,
                    },
                ),
                wake_event_type_with(
                    "matrix.template",
                    ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Template {
                            template: "line {line} #{n}".to_string(),
                            fields: [
                                (
                                    "line".to_string(),
                                    ProcessValueSelector::Pointer("/line".to_string()),
                                ),
                                (
                                    "n".to_string(),
                                    ProcessValueSelector::Pointer("/n".to_string()),
                                ),
                            ]
                            .into_iter()
                            .collect(),
                        },
                        dedupe_key: ProcessWakeDedupeKey::EventIdentity,
                    },
                ),
                wake_event_type_with(
                    "matrix.selector_dedupe",
                    ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                        dedupe_key: ProcessWakeDedupeKey::Selector(ProcessValueSelector::Pointer(
                            "/dedupe".to_string(),
                        )),
                    },
                ),
                wake_event_type_with(
                    "matrix.const_dedupe",
                    ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                        dedupe_key: ProcessWakeDedupeKey::Const("constant-dedupe".to_string()),
                    },
                ),
            ]),
        )
        .await
        .expect("register");
    let target = SessionScope::new("root");

    let no_wake = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new("matrix.when_false", serde_json::json!({}))
                .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append when false");
    assert!(
        no_wake.wake_delivery.is_none(),
        "a false wake.when selector must suppress wake materialization"
    );
    let payload = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new("matrix.payload", serde_json::json!("payload wake"))
                .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append payload wake")
        .wake_delivery
        .expect("payload wake");
    assert_eq!(payload.input, "payload wake");
    assert_eq!(payload.dedupe_key, "proc-wake-matrix:2");
    let const_input = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new("matrix.const_input", serde_json::json!({}))
                .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append const wake")
        .wake_delivery
        .expect("const wake");
    assert_eq!(const_input.input, "constant wake input");
    let template = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new(
                "matrix.template",
                serde_json::json!({"line": "done", "n": 7}),
            )
            .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append template wake")
        .wake_delivery
        .expect("template wake");
    assert_eq!(template.input, "line done #7");
    let selector_first = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new(
                "matrix.selector_dedupe",
                serde_json::json!({"wake_input": "selector one", "dedupe": "group-a"}),
            )
            .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append selector wake one")
        .wake_delivery
        .expect("selector wake one");
    let selector_second = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new(
                "matrix.selector_dedupe",
                serde_json::json!({"wake_input": "selector two", "dedupe": "group-a"}),
            )
            .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append selector wake two")
        .wake_delivery
        .expect("selector wake two");
    assert_eq!(selector_first.dedupe_key, "group-a");
    assert_eq!(
        selector_first.wake_id, selector_second.wake_id,
        "selector dedupe must produce a stable wake id for the same target and selector value"
    );
    let const_dedupe_first = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new(
                "matrix.const_dedupe",
                serde_json::json!({"wake_input": "const one"}),
            )
            .with_wake_target_scope(target.clone()),
        )
        .await
        .expect("append const dedupe one")
        .wake_delivery
        .expect("const dedupe one");
    let const_dedupe_second = registry
        .append_event(
            "proc-wake-matrix",
            ProcessEventAppendRequest::new(
                "matrix.const_dedupe",
                serde_json::json!({"wake_input": "const two"}),
            )
            .with_wake_target_scope(target),
        )
        .await
        .expect("append const dedupe two")
        .wake_delivery
        .expect("const dedupe two");
    assert_eq!(const_dedupe_first.dedupe_key, "constant-dedupe");
    assert_eq!(
        const_dedupe_first.wake_id, const_dedupe_second.wake_id,
        "const dedupe must produce a stable wake id for the same target"
    );
    let wake_sequences = registry
        .wake_events_after("proc-wake-matrix", 0)
        .await
        .expect("wake events")
        .into_iter()
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        wake_sequences,
        vec![2, 3, 4, 5, 6, 7, 8],
        "wake_events_after must include only events whose wake semantics materialized"
    );
}

async fn process_registry_survives_reopen(factory: ReopenableProcessRegistry) {
    let scope = SessionScope::new("reopen-session");
    factory
        .open
        .register_process(
            registration("proc-reopen")
                .with_extra_event_types([wake_event_type("producer.reopen_wake")]),
        )
        .await
        .expect("register");
    let external_ref = ProcessExternalRef {
        backend: "worker".to_string(),
        id: "reopen-job-123".to_string(),
        metadata: Some(serde_json::json!({ "queue": "reopen" })),
    };
    let updated = factory
        .open
        .set_external_ref("proc-reopen", external_ref.clone())
        .await
        .expect("set external ref before reopen");
    factory
        .open
        .grant_handle(
            &scope,
            "proc-reopen",
            ProcessHandleDescriptor::new(Some("test"), Some("reopen")),
        )
        .await
        .expect("grant");
    let appended = factory
        .open
        .append_event(
            "proc-reopen",
            ProcessEventAppendRequest::new(
                "producer.reopen_wake",
                serde_json::json!({"wake_input": "survived reopen"}),
            )
            .with_replay_key("producer:reopen")
            .with_wake_target_scope(scope.clone()),
        )
        .await
        .expect("append");

    let reopened_record = factory
        .reopen
        .get_process("proc-reopen")
        .await
        .expect("process exists after reopen");
    assert_eq!(reopened_record.id, "proc-reopen");
    assert_eq!(
        reopened_record.external_ref,
        Some(external_ref.clone()),
        "external ref must survive durable reopen"
    );
    let repeated = factory
        .reopen
        .set_external_ref("proc-reopen", external_ref.clone())
        .await
        .expect("repeat external ref after reopen");
    assert_eq!(
        serde_json::to_value(&repeated).expect("serialize repeated reopened process record"),
        serde_json::to_value(&reopened_record).expect("serialize reopened process record"),
        "repeating the same external ref after reopen must not mutate the process record"
    );
    assert_eq!(
        serde_json::to_value(&updated.external_ref).expect("serialize original external ref"),
        serde_json::to_value(&repeated.external_ref).expect("serialize repeated external ref"),
        "the reopened repeat must preserve the original external ref"
    );
    assert!(
        factory
            .reopen
            .set_external_ref(
                "proc-reopen",
                ProcessExternalRef {
                    backend: "worker".to_string(),
                    id: "reopen-job-456".to_string(),
                    metadata: Some(serde_json::json!({ "queue": "reopen" })),
                },
            )
            .await
            .is_err(),
        "conflicting external ref assignment after reopen must fail"
    );
    let reopened_events = factory
        .reopen
        .events_after("proc-reopen", 0)
        .await
        .expect("events after reopen");
    assert_eq!(reopened_events.len(), 2);
    assert!(
        reopened_events
            .iter()
            .any(|event| event.event_type == "process.external_ref_set")
    );
    assert_eq!(
        reopened_events
            .iter()
            .find(|event| event.event_type == "producer.reopen_wake")
            .expect("producer event survives reopen")
            .sequence,
        appended.event.sequence
    );
    assert_eq!(
        factory
            .reopen
            .list_handle_grants(&scope)
            .await
            .expect("grants after reopen")
            .len(),
        1
    );
    let replayed = factory
        .reopen
        .append_event(
            "proc-reopen",
            ProcessEventAppendRequest::new(
                "producer.reopen_wake",
                serde_json::json!({"wake_input": "survived reopen"}),
            )
            .with_replay_key("producer:reopen")
            .with_wake_target_scope(scope),
        )
        .await
        .expect("replay after reopen");
    assert_eq!(replayed.event.sequence, appended.event.sequence);
}

async fn keyed_events_materialize_idempotent_wakes(registry: Arc<dyn ProcessRegistry>) {
    let target_scope = SessionScope::new("session");
    let target_scope_id = target_scope.id();
    registry
        .register_process(
            registration("proc-wake").with_extra_event_types([wake_event_type("process.wake")]),
        )
        .await
        .expect("register");
    let request = ProcessEventAppendRequest::new(
        "process.wake",
        serde_json::json!({
            "message": "deploy failed",
            "wake_input": "Process wake: deploy failed",
        }),
    )
    .with_replay_key("wake:deploy failed")
    .with_wake_target_scope(target_scope);

    let first = registry
        .append_event("proc-wake", request.clone())
        .await
        .expect("append");
    let second = registry
        .append_event("proc-wake", request)
        .await
        .expect("replay append");

    assert_eq!(
        first.event.sequence, second.event.sequence,
        "replaying the same key must return the same sequence, not a new event"
    );
    assert_eq!(first.wake_delivery, second.wake_delivery);
    let wake = first.wake_delivery.expect("wake delivery");
    assert_eq!(wake.input, "Process wake: deploy failed");
    assert_eq!(wake.target_scope_id, target_scope_id);
    assert_eq!(wake.process_id, "proc-wake");
    assert_eq!(wake.sequence, first.event.sequence);
    assert!(
        registry
            .append_event(
                "proc-wake",
                ProcessEventAppendRequest::new(
                    "process.wake",
                    serde_json::json!({
                        "message": "other",
                        "wake_input": "Process wake: other",
                    }),
                )
                .with_replay_key("wake:deploy failed"),
            )
            .await
            .is_err(),
        "a different payload under an existing replay key must be rejected"
    );
}

async fn wake_semantic_events_without_target_record_without_delivery(
    registry: Arc<dyn ProcessRegistry>,
) {
    registry
        .register_process(
            registration("proc-missing-wake-target")
                .with_extra_event_types([wake_event_type("process.wake")]),
        )
        .await
        .expect("register");

    let appended = registry
        .append_event(
            "proc-missing-wake-target",
            ProcessEventAppendRequest::new(
                "process.wake",
                serde_json::json!({
                    "message": "target missing",
                    "wake_input": "Process wake: target missing",
                }),
            )
            .with_replay_key("wake:missing-target"),
        )
        .await
        .expect("wake-semantic event without target scope records");
    assert_eq!(appended.event.sequence, 1);
    assert!(
        appended.wake_delivery.is_none(),
        "dangling wake target should not materialize a delivery"
    );
    assert_eq!(
        registry
            .events_after("proc-missing-wake-target", 0)
            .await
            .expect("events after append")
            .len(),
        1,
        "dangling wake append should persist the process event"
    );
}

async fn terminal_and_cancel_events_require_keys(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-terminal"))
        .await
        .expect("register");

    assert!(
        registry
            .append_event(
                "proc-terminal",
                ProcessEventAppendRequest::new(
                    "process.cancel_requested",
                    serde_json::json!({"reason": "stop"}),
                ),
            )
            .await
            .is_err(),
        "cancel_requested without a replay key must be rejected"
    );
    registry
        .append_event(
            "proc-terminal",
            ProcessEventAppendRequest::cancel_requested("proc-terminal", Some("stop".to_string())),
        )
        .await
        .expect("cancel intent");
    registry
        .complete_process(
            "proc-terminal",
            ProcessAwaitOutput::Cancelled {
                message: "stopped".to_string(),
                raw: None,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete cancelled");
    assert_eq!(
        registry
            .get_process("proc-terminal")
            .await
            .and_then(|record| record.status.terminal_state()),
        Some(ProcessTerminalState::Cancelled)
    );
}

async fn await_reads_terminal_materialized_output(registry: Arc<dyn ProcessRegistry>) {
    let (registry, hub) = crate::watch_process_registry(registry);
    let awaiter = crate::ProcessAwaiter::new(Arc::clone(&registry), hub);
    registry
        .register_process(registration("proc-2"))
        .await
        .expect("register");
    registry
        .complete_process(
            "proc-2",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "ok": true }),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    assert_eq!(
        awaiter.await_terminal("proc-2").await.expect("await"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "ok": true }),
            control: None,
        }
    );
    assert!(
        registry
            .get_process("proc-2")
            .await
            .expect("record")
            .is_terminal()
    );
}

/// Recovery-disposition, first-started, and abandon-request facts persist and
/// round-trip, with the first-writer-wins immutability the sweep relies on
/// (ADR 0019).
async fn disposition_first_started_and_abandon_request_persist(registry: Arc<dyn ProcessRegistry>) {
    let record = registry
        .register_process(owner_bound_registration("proc-disposition"))
        .await
        .expect("register");
    assert_eq!(record.disposition, RecoveryDisposition::OwnerBound);
    assert!(record.first_started.is_none());
    assert!(record.abandon_request.is_none());
    assert!(
        registry
            .get_process_lease("proc-disposition")
            .await
            .expect("lease read")
            .is_none(),
        "an unclaimed row reports no lease holder"
    );

    // first_started persists and is immutable once written (first-writer-wins).
    let started = ProcessStarted {
        owner: process_lease_owner("starter"),
        fencing_token: 0,
        attempt: 1,
        started_at_ms: 111,
    };
    let recorded = registry
        .record_first_started("proc-disposition", started.clone())
        .await
        .expect("record started");
    assert_eq!(recorded.first_started.as_deref(), Some(&started));
    let rerecorded = registry
        .record_first_started(
            "proc-disposition",
            ProcessStarted {
                owner: process_lease_owner("other-starter"),
                fencing_token: 0,
                attempt: 2,
                started_at_ms: 222,
            },
        )
        .await
        .expect_err("a different OwnerBound execution is rejected");
    assert!(matches!(
        rerecorded,
        crate::PluginError::ProcessAlreadyStarted { by, .. }
            if *by == process_lease_owner("starter")
    ));

    // A live lease is exposed read-side by holder and expiry.
    let holder = registry
        .claim_process_lease("proc-disposition", &process_lease_owner("holder"), 60_000)
        .await
        .expect("claim")
        .acquired()
        .expect("acquired");
    let read = registry
        .get_process_lease("proc-disposition")
        .await
        .expect("lease read")
        .expect("a claimed row reports its holder");
    assert_eq!(read.lease_token, holder.lease_token);
    assert_eq!(read.expires_at_epoch_ms, holder.expires_at_epoch_ms);

    // abandon_request persists and preserves the first recorded authorization.
    let request = AbandonRequest {
        requested_by: "operator".to_string(),
        requested_at_ms: 333,
        reason: Some("host retired".to_string()),
    };
    let marked = registry
        .request_process_abandon("proc-disposition", request.clone())
        .await
        .expect("request abandon");
    assert_eq!(marked.abandon_request.as_deref(), Some(&request));
    let remarked = registry
        .request_process_abandon(
            "proc-disposition",
            AbandonRequest {
                requested_by: "second-operator".to_string(),
                requested_at_ms: 444,
                reason: None,
            },
        )
        .await
        .expect("re-request abandon");
    assert_eq!(
        remarked.abandon_request.as_deref(),
        Some(&request),
        "the first recorded abandon request is preserved"
    );
    let reloaded = registry
        .get_process("proc-disposition")
        .await
        .expect("reload");
    assert_eq!(reloaded.first_started.as_deref(), Some(&started));
    assert_eq!(reloaded.abandon_request.as_deref(), Some(&request));

    assert!(
        registry
            .record_first_started(
                "missing-process",
                ProcessStarted {
                    owner: process_lease_owner("x"),
                    fencing_token: 0,
                    attempt: 1,
                    started_at_ms: 1,
                },
            )
            .await
            .is_err(),
        "recording started for an unknown process must fail"
    );
    assert!(
        registry
            .request_process_abandon(
                "missing-process",
                AbandonRequest {
                    requested_by: "x".to_string(),
                    requested_at_ms: 1,
                    reason: None,
                },
            )
            .await
            .is_err(),
        "requesting abandon for an unknown process must fail"
    );
}

/// The Abandoned terminal round-trips its evidence payload, is prunable, rejects
/// a revenant's later write, and rejects an abandon request once terminal
/// (ADR 0019).
async fn abandoned_terminal_round_trips_and_pins_writer_rules(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(owner_bound_registration("proc-abandoned"))
        .await
        .expect("register");
    let evidence = AbandonEvidence {
        writer: AbandonWriter::Sweep,
        owner: Some(process_lease_owner("dead-owner")),
        epoch_ms: 999,
    };
    let terminal = registry
        .complete_process(
            "proc-abandoned",
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(evidence.clone()),
                control: None,
            },
            // OwnerBound row: the unleased sweep-standin path is workflow-key
            // authority (mirrors the Restate run handler abandoning a started row).
            ProcessCompletionAuthority::workflow_key("proc-abandoned"),
        )
        .await
        .expect("abandon");
    assert_eq!(
        terminal.status.terminal_state(),
        Some(ProcessTerminalState::Abandoned)
    );
    match terminal.into_record().status {
        ProcessStatus::Abandoned {
            await_output: ProcessAwaitOutput::Abandoned { evidence: got, .. },
        } => assert_eq!(
            *got, evidence,
            "the Abandoned terminal round-trips its evidence"
        ),
        other => panic!("expected an Abandoned terminal, got {other:?}"),
    }

    let abandoned = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Abandoned,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list abandoned");
    assert!(
        abandoned.iter().any(|record| record.id == "proc-abandoned"),
        "the abandoned status filter includes abandoned rows"
    );

    // A revenant cannot overwrite the terminal: retries adopt the stored
    // terminal outcome rather than surfacing a retryable conflict.
    let replayed = registry
        .complete_process(
            "proc-abandoned",
            ProcessAwaitOutput::Success {
                value: serde_json::json!("revenant"),
                control: None,
            },
            ProcessCompletionAuthority::workflow_key("proc-abandoned"),
        )
        .await
        .expect("terminal retry returns stored outcome");
    assert!(matches!(
        &replayed,
        ProcessCompletionOutcome::Superseded { .. }
    ));
    assert_eq!(
        replayed.status.terminal_state(),
        Some(ProcessTerminalState::Abandoned)
    );
    assert!(
        registry
            .request_process_abandon(
                "proc-abandoned",
                AbandonRequest {
                    requested_by: "operator".to_string(),
                    requested_at_ms: 1,
                    reason: None,
                },
            )
            .await
            .is_err(),
        "a terminal row cannot accept an abandon request"
    );

    // A far-future cutoff (kept within i64 range for SQL backends that bind the
    // cutoff as a signed integer) prunes every terminal row.
    let report = registry
        .prune_terminal_processes(i64::MAX as u64, None, None)
        .await
        .expect("prune");
    assert!(
        report.pruned_processes >= 1,
        "abandoned rows are terminal and therefore prunable"
    );
    assert!(
        registry.get_process("proc-abandoned").await.is_none(),
        "a pruned abandoned row is gone"
    );
}

async fn count_and_recent_events_match_the_log(registry: Arc<dyn ProcessRegistry>) {
    assert!(
        registry
            .count_events_through("missing-process", "signal.ready", 10)
            .await
            .is_err(),
        "counting events for an unknown process must fail"
    );
    assert!(
        registry.recent_events("missing-process", 4).await.is_err(),
        "recent events for an unknown process must fail"
    );

    registry
        .register_process(
            registration("proc-event-queries")
                .with_event_types(vec![plain_event_type("signal.a"), plain_event_type("b")]),
        )
        .await
        .expect("register");
    let mut sequences = Vec::new();
    for (index, event_type) in ["signal.a", "b", "signal.a", "signal.a", "b"]
        .iter()
        .enumerate()
    {
        let appended = registry
            .append_event(
                "proc-event-queries",
                ProcessEventAppendRequest::new(*event_type, serde_json::json!({ "n": index }))
                    .with_replay_key(format!("proc-event-queries:{index}")),
            )
            .await
            .expect("append");
        sequences.push(appended.event.sequence);
    }

    // Counts honor both the type filter and the sequence bound.
    let count_all = registry
        .count_events_through("proc-event-queries", "signal.a", *sequences.last().unwrap())
        .await
        .expect("count all");
    assert_eq!(count_all, 3);
    let count_bounded = registry
        .count_events_through("proc-event-queries", "signal.a", sequences[2])
        .await
        .expect("count bounded");
    assert_eq!(count_bounded, 2, "the bound is inclusive and type-filtered");
    let count_none = registry
        .count_events_through("proc-event-queries", "signal.missing", u64::MAX)
        .await
        .expect("count missing type");
    assert_eq!(count_none, 0);

    // Recent events are the LAST `limit`, returned in ascending order —
    // identical to the tail of the full log.
    let full = registry
        .events_after("proc-event-queries", 0)
        .await
        .expect("full log");
    let recent = registry
        .recent_events("proc-event-queries", 2)
        .await
        .expect("recent");
    assert_eq!(
        recent
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        full[full.len() - 2..]
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
    );
    let generous = registry
        .recent_events("proc-event-queries", 100)
        .await
        .expect("generous limit");
    assert_eq!(
        generous.len(),
        full.len(),
        "limit above log length is the whole log"
    );
}

async fn list_processes_filters_by_status_and_waiting(registry: Arc<dyn ProcessRegistry>) {
    for id in ["proc-list-running", "proc-list-waiting", "proc-list-done"] {
        registry
            .register_process(registration(id))
            .await
            .expect("register list process");
    }
    registry
        .set_process_wait(
            "proc-list-waiting",
            WaitState {
                since_ms: 1,
                kind: WaitKind::Signal {
                    name: "ready".to_string(),
                    event_type: "signal.ready".to_string(),
                    key: "process:proc-list-waiting:signal.ready:1".to_string(),
                    ordinal: 1,
                },
            },
        )
        .await
        .expect("set wait state");
    registry
        .complete_process(
            "proc-list-done",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");

    let ids = |records: Vec<ProcessRecord>| {
        records
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>()
    };

    let any = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list any");
    assert_eq!(
        ids(any),
        vec![
            "proc-list-done".to_string(),
            "proc-list-running".to_string(),
            "proc-list-waiting".to_string(),
        ],
        "list_processes(Any) returns every record in stable id order"
    );

    let running = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Running,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list running");
    assert_eq!(
        ids(running),
        vec![
            "proc-list-running".to_string(),
            "proc-list-waiting".to_string(),
        ],
        "waiting processes are lifecycle-running"
    );

    let completed = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Completed,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list completed");
    assert_eq!(ids(completed), vec!["proc-list-done".to_string()]);

    let waiting = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            waiting: Some(true),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list waiting");
    assert_eq!(ids(waiting), vec!["proc-list-waiting".to_string()]);

    let not_waiting = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Running,
            waiting: Some(false),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list not waiting");
    assert_eq!(ids(not_waiting), vec!["proc-list-running".to_string()]);
}

async fn process_change_feed_orders_resumes_and_includes_terminal_transitions(
    registry: Arc<dyn ProcessRegistry>,
) {
    let ids = |records: Vec<ProcessRecord>| {
        records
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>()
    };
    let initial = ProcessChangeCursor::initial();

    let (empty, empty_cursor) = registry
        .processes_changed_since(initial, 10)
        .await
        .expect("empty feed");
    assert!(empty.is_empty(), "a fresh registry has no changed records");
    assert_eq!(
        empty_cursor, initial,
        "an empty page keeps the caller's cursor unchanged"
    );
    let (zero_limit, zero_cursor) = registry
        .processes_changed_since(initial, 0)
        .await
        .expect("zero-limit feed");
    assert!(zero_limit.is_empty(), "limit zero returns no records");
    assert_eq!(
        zero_cursor, initial,
        "limit zero keeps the caller's cursor unchanged"
    );

    registry
        .register_process(
            registration("proc-change-a")
                .with_extra_event_types([plain_event_type("producer.tick")]),
        )
        .await
        .expect("register a");
    registry
        .register_process(registration("proc-change-b"))
        .await
        .expect("register b");

    let (first_page, cursor_after_a) = registry
        .processes_changed_since(initial, 1)
        .await
        .expect("first feed page");
    assert_eq!(
        ids(first_page),
        vec!["proc-change-a".to_string()],
        "the first page follows registration change order"
    );
    let (reread_first_page, reread_cursor_after_a) = registry
        .processes_changed_since(initial, 1)
        .await
        .expect("reread first feed page");
    assert_eq!(
        ids(reread_first_page),
        vec!["proc-change-a".to_string()],
        "re-reading from the same cursor is idempotent"
    );
    assert_eq!(
        reread_cursor_after_a, cursor_after_a,
        "re-reading from the same cursor returns the same next cursor"
    );
    let (second_page, cursor_after_b) = registry
        .processes_changed_since(cursor_after_a, 1)
        .await
        .expect("second feed page");
    assert_eq!(
        ids(second_page),
        vec!["proc-change-b".to_string()],
        "resuming from the first page reaches the next changed record"
    );
    let (no_new_records, same_cursor) = registry
        .processes_changed_since(cursor_after_b, 10)
        .await
        .expect("empty resumed feed");
    assert!(no_new_records.is_empty());
    assert_eq!(
        same_cursor, cursor_after_b,
        "an empty resumed page keeps the cursor unchanged"
    );

    registry
        .append_event(
            "proc-change-a",
            ProcessEventAppendRequest::new("producer.tick", serde_json::json!({"n": 1})),
        )
        .await
        .expect("append a event");
    registry
        .complete_process(
            "proc-change-b",
            ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete b");

    let (changed_after_b, cursor_after_a_update) = registry
        .processes_changed_since(cursor_after_b, 1)
        .await
        .expect("changed after registration cursor");
    assert_eq!(
        ids(changed_after_b),
        vec!["proc-change-a".to_string()],
        "later row mutations appear after the saved cursor"
    );
    let (terminal_page, cursor_after_terminal) = registry
        .processes_changed_since(cursor_after_a_update, 1)
        .await
        .expect("terminal change page");
    assert_eq!(terminal_page.len(), 1);
    assert_eq!(terminal_page[0].id, "proc-change-b");
    assert!(
        terminal_page[0].is_terminal(),
        "terminal transitions appear in the process change feed"
    );
    let (empty_after_terminal, stable_terminal_cursor) = registry
        .processes_changed_since(cursor_after_terminal, 10)
        .await
        .expect("empty after terminal page");
    assert!(empty_after_terminal.is_empty());
    assert_eq!(stable_terminal_cursor, cursor_after_terminal);
}

async fn wait_state_round_trips_filters_and_clears_on_terminal(registry: Arc<dyn ProcessRegistry>) {
    assert!(
        registry
            .set_process_wait(
                "missing-process",
                WaitState {
                    since_ms: 1,
                    kind: WaitKind::Signal {
                        name: "ready".to_string(),
                        event_type: "signal.ready".to_string(),
                        key: "process:missing-process:signal.ready:1".to_string(),
                        ordinal: 1,
                    },
                },
            )
            .await
            .is_err(),
        "setting wait state for an unknown process must fail"
    );

    registry
        .register_process(registration("proc-wait-roundtrip"))
        .await
        .expect("register wait process");
    let wait = WaitState {
        since_ms: 1234,
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:proc-wait-roundtrip:signal.ready:1".to_string(),
            ordinal: 1,
        },
    };

    let waiting = registry
        .set_process_wait("proc-wait-roundtrip", wait.clone())
        .await
        .expect("set wait state");
    assert_eq!(waiting.wait, Some(wait.clone()));
    assert_eq!(
        registry
            .get_process("proc-wait-roundtrip")
            .await
            .expect("wait process")
            .wait,
        Some(wait.clone()),
        "wait state must persist on the process record"
    );
    assert!(
        registry
            .list_processes(&ProcessListFilter {
                waiting: Some(true),
                ..ProcessListFilter::default()
            })
            .await
            .expect("list waiting processes")
            .iter()
            .any(|record| record.id == "proc-wait-roundtrip"),
        "waiting=true must include processes with a current wait state"
    );
    assert!(
        registry
            .list_processes(&ProcessListFilter {
                waiting: Some(false),
                ..ProcessListFilter::default()
            })
            .await
            .expect("list idle processes")
            .iter()
            .all(|record| record.id != "proc-wait-roundtrip"),
        "waiting=false must exclude processes with a current wait state"
    );

    let resumed = registry
        .clear_process_wait("proc-wait-roundtrip")
        .await
        .expect("clear wait state");
    assert_eq!(resumed.wait, None);

    registry
        .set_process_wait("proc-wait-roundtrip", wait)
        .await
        .expect("set wait state before terminal completion");
    let completed = registry
        .complete_process(
            "proc-wait-roundtrip",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "done": true }),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete waiting process");
    assert!(
        completed.wait.is_none(),
        "terminal completion must clear current wait state"
    );
    assert!(
        registry
            .set_process_wait(
                "proc-wait-roundtrip",
                WaitState {
                    since_ms: 5678,
                    kind: WaitKind::Signal {
                        name: "again".to_string(),
                        event_type: "signal.again".to_string(),
                        key: "process:proc-wait-roundtrip:signal.again:1".to_string(),
                        ordinal: 1,
                    },
                },
            )
            .await
            .is_err(),
        "terminal processes cannot enter a wait state"
    );
}

async fn transfer_handle_grants_moves_addressability(registry: Arc<dyn ProcessRegistry>) {
    let s1 = SessionScope::new("s1");
    let s2 = SessionScope::new("s2");
    registry
        .register_process(registration("proc-3"))
        .await
        .expect("register");
    registry
        .grant_handle(
            &s1,
            "proc-3",
            ProcessHandleDescriptor::new(Some("tool"), Some("demo")),
        )
        .await
        .expect("grant");
    registry
        .transfer_handle_grants(&s1, &s2, &["proc-3".to_string()])
        .await
        .expect("transfer");

    assert_eq!(
        registry
            .list_handle_grants(&s1)
            .await
            .expect("grants")
            .len(),
        0
    );
    assert_eq!(
        registry
            .list_handle_grants(&s2)
            .await
            .expect("grants")
            .len(),
        1
    );
    assert!(
        registry
            .events_after("proc-3", 0)
            .await
            .expect("events")
            .is_empty(),
        "addressability transfer must not append process events"
    );
}

async fn multiple_sessions_can_hold_grants(registry: Arc<dyn ProcessRegistry>) {
    let s1 = SessionScope::new("s1");
    let s2 = SessionScope::new("s2");
    let s3 = SessionScope::new("s3");
    registry
        .register_process(registration("proc-5"))
        .await
        .expect("register");
    registry
        .grant_handle(
            &s1,
            "proc-5",
            ProcessHandleDescriptor::new(Some("tool"), Some("demo")),
        )
        .await
        .expect("grant s1");
    registry
        .grant_handle(
            &s2,
            "proc-5",
            ProcessHandleDescriptor::new(Some("worker"), Some("demo")),
        )
        .await
        .expect("grant s2");

    let grant_sessions = registry
        .handle_grants_for_process("proc-5")
        .await
        .expect("process grants")
        .into_iter()
        .map(|grant| grant.session_id)
        .collect::<Vec<_>>();
    assert_eq!(grant_sessions, vec!["s1".to_string(), "s2".to_string()]);

    registry
        .transfer_handle_grants(&s1, &s3, &["proc-5".to_string()])
        .await
        .expect("transfer s1");
    let grant_sessions = registry
        .handle_grants_for_process("proc-5")
        .await
        .expect("process grants")
        .into_iter()
        .map(|grant| grant.session_id)
        .collect::<Vec<_>>();
    assert_eq!(grant_sessions, vec!["s2".to_string(), "s3".to_string()]);
    assert!(
        registry
            .events_after("proc-5", 0)
            .await
            .expect("events")
            .is_empty()
    );
}

async fn processes_can_exist_with_zero_grants(registry: Arc<dyn ProcessRegistry>) {
    let s1 = SessionScope::new("s1");
    registry
        .register_process(registration("proc-4"))
        .await
        .expect("register");
    assert!(
        registry
            .list_handle_grants(&s1)
            .await
            .expect("grants")
            .is_empty()
    );
}

async fn delete_session_revokes_handles_by_session(registry: Arc<dyn ProcessRegistry>) {
    let deleted_scope = SessionScope::new("deleted");
    let remaining_scope = SessionScope::new("remaining");
    for process_id in ["sole", "shared", "terminal"] {
        registry
            .register_process(
                registration(process_id)
                    .with_extra_event_types([wake_event_type("producer.wake")])
                    .with_wake_target(Some(deleted_scope.clone())),
            )
            .await
            .expect("register");
        registry
            .grant_handle(
                &deleted_scope,
                process_id,
                ProcessHandleDescriptor::new(Some("test"), Some(process_id)),
            )
            .await
            .expect("grant deleted");
    }
    registry
        .grant_handle(
            &remaining_scope,
            "shared",
            ProcessHandleDescriptor::new(Some("test"), Some("shared")),
        )
        .await
        .expect("grant remaining");
    registry
        .complete_process(
            "terminal",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete terminal");
    registry
        .append_event(
            "sole",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({ "wake_input": "wake deleted" }),
            )
            .with_wake_target_scope(deleted_scope.clone()),
        )
        .await
        .expect("append wake");
    let shared_wake = registry
        .append_event(
            "shared",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({ "wake_input": "wake shared" }),
            )
            .with_wake_target_scope(remaining_scope.clone()),
        )
        .await
        .expect("append shared wake");
    registry
        .ack_wake("shared", shared_wake.event.sequence)
        .await
        .expect("ack shared wake");
    assert!(
        registry
            .wake_events_after("shared", 0)
            .await
            .expect("shared wake events before delete")
            .is_empty(),
        "acked shared wake should be suppressed before session deletion"
    );

    let report = registry
        .delete_session_process_state("deleted")
        .await
        .expect("delete session process state");

    assert_eq!(report.revoked_handle_count, 3);
    assert_eq!(report.deleted_wake_count, 0);
    assert_eq!(report.orphaned_process_ids, vec!["sole".to_string()]);
    assert_eq!(report.preserved_process_ids, vec!["shared".to_string()]);
    for process_id in ["sole", "shared", "terminal"] {
        assert!(
            registry
                .get_process(process_id)
                .await
                .expect("process survives session delete")
                .wake_target
                .is_none(),
            "session deletion should detach process wake target for {process_id}"
        );
    }
    assert!(
        registry
            .list_handle_grants(&deleted_scope)
            .await
            .expect("deleted grants")
            .is_empty()
    );
    assert_eq!(
        registry
            .list_handle_grants(&remaining_scope)
            .await
            .expect("remaining grants")
            .len(),
        1
    );
    assert!(
        registry
            .wake_events_after("shared", 0)
            .await
            .expect("shared wake events after delete")
            .is_empty(),
        "session deletion must preserve process-scoped wake acknowledgements"
    );
}

async fn list_non_terminal_excludes_terminal_processes(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-live"))
        .await
        .expect("register live");
    registry
        .register_process(registration("proc-done"))
        .await
        .expect("register done");
    registry
        .complete_process(
            "proc-done",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete done");

    let ids = registry
        .list_non_terminal()
        .await
        .expect("list non-terminal")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["proc-live".to_string()],
        "list_non_terminal must exclude terminal processes and be process_id ordered"
    );
}

async fn list_live_handle_grants_excludes_terminal_history(registry: Arc<dyn ProcessRegistry>) {
    let scope = SessionScope::new("history-owner");
    for process_id in ["proc-live-grant", "proc-done-grant"] {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register");
        registry
            .grant_handle(
                &scope,
                process_id,
                ProcessHandleDescriptor::new(Some("test"), Some(process_id)),
            )
            .await
            .expect("grant");
    }
    registry
        .complete_process(
            "proc-done-grant",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete done");

    let live_ids = registry
        .list_live_handle_grants(&scope)
        .await
        .expect("list live grants")
        .into_iter()
        .map(|(grant, _)| grant.process_id)
        .collect::<Vec<_>>();
    assert_eq!(
        live_ids,
        vec!["proc-live-grant".to_string()],
        "list_live_handle_grants must exclude completed historical handles"
    );

    let all_ids = registry
        .list_handle_grants(&scope)
        .await
        .expect("list all grants")
        .into_iter()
        .map(|(grant, _)| grant.process_id)
        .collect::<Vec<_>>();
    assert_eq!(
        all_ids,
        vec!["proc-done-grant".to_string(), "proc-live-grant".to_string()],
        "list_handle_grants remains the explicit all-history path"
    );
}

async fn active_process_lease_fences_competing_owner(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-lease-active"))
        .await
        .expect("register");
    let first = registry
        .claim_process_lease("proc-lease-active", &process_lease_owner("owner-a"), 60_000)
        .await
        .expect("first claim")
        .acquired()
        .expect("first claim acquired");
    let conflict = registry
        .claim_process_lease("proc-lease-active", &process_lease_owner("owner-b"), 60_000)
        .await
        .expect("competing claim resolves");
    match conflict {
        crate::ProcessLeaseClaimOutcome::Busy { holder } => {
            assert_eq!(
                holder.lease_token, first.lease_token,
                "the busy outcome must carry the observed live holder"
            );
        }
        crate::ProcessLeaseClaimOutcome::Acquired(_) => {
            panic!("an active lease must fence a competing owner")
        }
    }
    // The original incarnation may re-enter its own live lease: the expiry
    // extends while token and fencing token stay stable.
    let reentered = registry
        .claim_process_lease(
            "proc-lease-active",
            &process_lease_owner("owner-a"),
            120_000,
        )
        .await
        .expect("owner re-claims its own live lease")
        .acquired()
        .expect("same incarnation re-enters");
    assert_eq!(reentered.lease_token, first.lease_token);
    assert_eq!(reentered.fencing_token, first.fencing_token);
    assert!(reentered.expires_at_epoch_ms >= first.expires_at_epoch_ms);
}

async fn superseded_process_lease_cannot_renew(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-lease-superseded"))
        .await
        .expect("register");
    let old = registry
        .claim_process_lease(
            "proc-lease-superseded",
            &process_lease_owner("owner-a"),
            EXPIRING_PROCESS_LEASE_TTL_MS,
        )
        .await
        .expect("old lease")
        .acquired()
        .expect("old lease acquired");
    claim_process_lease_after_expiry(
        registry.as_ref(),
        "proc-lease-superseded",
        &process_lease_owner("owner-b"),
        60_000,
    )
    .await;
    let stale = registry.renew_process_lease(&old, 60_000).await;
    assert!(
        stale
            .as_ref()
            .is_err_and(|err| err.to_string().contains("missing or expired")),
        "a superseded lease must not renew, got {stale:?}"
    );
}

async fn renewed_process_lease_survives_original_expiry(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-lease-renew"))
        .await
        .expect("register");
    let lease = registry
        .claim_process_lease("proc-lease-renew", &process_lease_owner("owner-a"), 20)
        .await
        .expect("lease")
        .acquired()
        .expect("lease acquired");
    let renewed = registry
        .renew_process_lease(&lease, 60_000)
        .await
        .expect("renew");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    registry
        .renew_process_lease(&renewed, 60_000)
        .await
        .expect("a renewed lease survives the original TTL");
}

async fn completed_lease_releases_and_reclaim_bumps_fencing(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-lease-complete"))
        .await
        .expect("register");
    let first = registry
        .claim_process_lease(
            "proc-lease-complete",
            &process_lease_owner("owner-a"),
            60_000,
        )
        .await
        .expect("first claim")
        .acquired()
        .expect("first claim acquired");
    registry
        .complete_process_lease(&ProcessLeaseCompletion::from_lease(&first))
        .await
        .expect("complete lease");
    let second = registry
        .claim_process_lease(
            "proc-lease-complete",
            &process_lease_owner("owner-b"),
            60_000,
        )
        .await
        .expect("a new owner can claim a released lease")
        .acquired()
        .expect("released lease is claimable");
    assert!(
        second.fencing_token > first.fencing_token,
        "a re-claim must bump the fencing token (was {}, now {})",
        first.fencing_token,
        second.fencing_token
    );
}

async fn stale_lease_completion_cannot_release_live_lease(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(registration("proc-lease-stale-complete"))
        .await
        .expect("register");
    let old = registry
        .claim_process_lease(
            "proc-lease-stale-complete",
            &process_lease_owner("owner-a"),
            0,
        )
        .await
        .expect("old lease")
        .acquired()
        .expect("old lease acquired");
    let current = registry
        .claim_process_lease(
            "proc-lease-stale-complete",
            &process_lease_owner("owner-b"),
            60_000,
        )
        .await
        .expect("new live lease")
        .acquired()
        .expect("new live lease acquired");
    // A stale completion (old token) must not release the live lease.
    registry
        .complete_process_lease(&ProcessLeaseCompletion::from_lease(&old))
        .await
        .expect("stale completion is ignored");
    let conflict = registry
        .claim_process_lease(
            "proc-lease-stale-complete",
            &process_lease_owner("owner-c"),
            60_000,
        )
        .await
        .expect("competing claim resolves");
    assert!(
        matches!(conflict, crate::ProcessLeaseClaimOutcome::Busy { .. }),
        "a stale completion must not release the live lease, got {conflict:?}"
    );
    // The live owner can still renew.
    registry
        .renew_process_lease(&current, 60_000)
        .await
        .expect("the live owner can still renew");
}

async fn stale_process_lease_cannot_complete_or_disturb_current_owner(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "proc-lease-terminal-fence";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register");
    let stale = registry
        .claim_process_lease(
            process_id,
            &process_lease_owner("stale-owner"),
            EXPIRING_PROCESS_LEASE_TTL_MS,
        )
        .await
        .expect("stale lease")
        .acquired()
        .expect("stale lease acquired");
    let current = claim_process_lease_after_expiry(
        registry.as_ref(),
        process_id,
        &process_lease_owner("current-owner"),
        60_000,
    )
    .await;
    let stale_result = registry
        .complete_process_with_lease(
            &stale,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"writer": "stale"}),
                control: None,
            },
        )
        .await;
    assert!(
        stale_result.is_err(),
        "a superseded lease must not append a terminal event"
    );
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("process exists")
            .is_terminal(),
        "stale completion must leave the process non-terminal"
    );
    let terminal_events = registry
        .events_after(process_id, 0)
        .await
        .expect("events after stale completion")
        .into_iter()
        .filter(|event| event.semantics.terminal.is_some())
        .count();
    assert_eq!(
        terminal_events, 0,
        "a stale completion must not append a terminal event"
    );
    let lease_after_stale_completion = registry
        .get_process_lease(process_id)
        .await
        .expect("read current lease after stale completion")
        .expect("current owner must retain its lease");
    assert_eq!(
        serde_json::to_value(&lease_after_stale_completion)
            .expect("serialize lease after stale completion"),
        serde_json::to_value(&current).expect("serialize current lease"),
        "a stale completion must not alter the current owner's lease"
    );
}

async fn expired_process_lease_cannot_complete(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "proc-lease-terminal-expired";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register");
    let expired = registry
        .claim_process_lease(
            process_id,
            &process_lease_owner("expired-owner"),
            EXPIRING_PROCESS_LEASE_TTL_MS,
        )
        .await
        .expect("expired lease")
        .acquired()
        .expect("short lease acquired");
    tokio::time::sleep(std::time::Duration::from_millis(
        EXPIRING_PROCESS_LEASE_TTL_MS + 100,
    ))
    .await;
    let expired_result = registry
        .complete_process_with_lease(
            &expired,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"writer": "expired"}),
                control: None,
            },
        )
        .await;
    assert!(
        expired_result.is_err(),
        "an expired lease must not complete a process"
    );
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("process exists")
            .is_terminal(),
        "expired completion must leave the process non-terminal"
    );
    let terminal_events = registry
        .events_after(process_id, 0)
        .await
        .expect("events after expired completion")
        .into_iter()
        .filter(|event| event.semantics.terminal.is_some())
        .count();
    assert_eq!(
        terminal_events, 0,
        "an expired completion must not append a terminal event"
    );
}
