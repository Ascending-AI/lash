use super::*;

fn event(kind: BoundaryKind, id: &str, payload: Value) -> BoundaryEvent {
    BoundaryEvent::new(id, "session-001", kind, 1, "test", payload)
}

fn harness() -> RuntimeBoundaryHarness {
    let clock = crate::clock::SimClock::new();
    let factory: Arc<dyn SessionStoreFactory> =
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::with_clock(clock.clone()));
    RuntimeBoundaryHarness::new(factory, RuntimeEffectReplayStore::Memory, clock)
}

#[tokio::test]
async fn worker_failover_continuation_oracle_catches_a_store_that_fails_to_fence() {
    // END-TO-END NEGATIVE: drive the REAL worker-stale-completion boundary
    // against a store whose session-execution lease is already held, so the
    // worker's stale-owner claim is Busy and the real lease store can neither
    // fence nor continue the work. The failover-continuation oracle MUST catch
    // this — proving it bites on a real un-fencing path, not just synthetic
    // facts.
    let mut harness = harness();
    let session = "worker-unfenced-session";
    let store = harness.store_for_session(session).await.expect("store");
    let blocker = LeaseOwnerIdentity::opaque("blocker-owner", "blocker-owner:001");
    let blocking = match store
        .try_claim_session_execution_lease(session, &blocker, LEASE_TTL_MS)
        .await
        .expect("blocker claim")
    {
        SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
        other => panic!("expected to acquire the blocking lease: {other:?}"),
    };

    let worker_event = BoundaryEvent::new(
        "worker:unfenced:001",
        session,
        BoundaryKind::Worker,
        5,
        "worker.stale-completion-rejected",
        json!({ "session": session }),
    );
    let observed = harness
        .run_worker_stale_completion(&worker_event)
        .await
        .expect("worker boundary observed");

    // The real store could NOT fence: no rejection, no work continuation.
    assert_eq!(
        observed
            .get("stale_completion_rejected")
            .and_then(Value::as_bool),
        Some(false),
        "a busy store must not report a fenced stale completion: {observed}"
    );
    assert!(
        observed
            .get("runtime_worker_store")
            .and_then(|store| store.get("worker_owned_work"))
            .is_none(),
        "a store that failed to fence must not record worker-owned-work continuation: {observed}"
    );

    let delivered = crate::scheduler::DeliveredBoundary {
        schema: "test".to_string(),
        sequence: 0,
        scheduler: crate::scheduler::SchedulerDeliveryEvidence::default(),
        boundary_id: "worker:unfenced:001".to_string(),
        actor_alias: session.to_string(),
        kind: BoundaryKind::Worker,
        at: 5,
        label: "worker.stale-completion-rejected".to_string(),
        payload: json!({ "session": session }),
        observed,
    };
    let verdict = crate::oracles::worker_failover_continues_work(std::slice::from_ref(&delivered));
    assert!(
        !verdict.is_passed(),
        "the failover-continuation oracle must catch a store that failed to fence: {}",
        verdict.message
    );

    let _ = store
        .release_session_execution_lease(&blocking.completion())
        .await;
}

#[tokio::test]
async fn durable_effect_replays_through_runtime_effect_controller() {
    let mut harness = harness();
    let payload = json!({
        "durable_key": "sleep/session-001/001",
        "result": {"completed": true},
        "runtime_effect": {"effect_id": "effect/sleep/001"},
    });
    let first = harness
        .complete_durable_effect(&event(
            BoundaryKind::DurableEffect,
            "durable:first",
            payload.clone(),
        ))
        .await
        .expect("first durable effect");
    let replay = harness
        .complete_durable_effect(&event(
            BoundaryKind::DurableEffect,
            "durable:replay",
            json!({
                "durable_key": "sleep/session-001/001",
                "result": {"completed": false},
                "runtime_effect": {"effect_id": "effect/sleep/001"},
            }),
        ))
        .await
        .expect("replayed durable effect");

    assert_eq!(first["execution_count"], 1);
    assert_eq!(replay["execution_count"], 1);
    assert_eq!(replay["replay_count"], 1);
    assert_eq!(replay["replayed"], true);
    assert_eq!(first["result_digest"], replay["result_digest"]);
    assert_eq!(first["runtime_effect"]["local_executor_called"], true);
    assert_eq!(replay["runtime_effect"]["local_executor_called"], false);
    assert_eq!(
        replay["runtime_effect"]["controller"],
        "sqlite_runtime_effect_controller"
    );
}

#[tokio::test]
async fn durable_effect_observation_counts_a_second_real_local_execution() {
    let mut harness = harness();
    let boundary = event(
        BoundaryKind::DurableEffect,
        "durable:duplicate-local-execution",
        json!({
            "durable_key": "sleep/session-001/duplicate",
            "result": {"completed": true},
            "runtime_effect": {"effect_id": "effect/sleep/duplicate"},
        }),
    );

    let first = harness
        .complete_durable_effect(&boundary)
        .await
        .expect("first durable effect");
    harness.effect_controller = None;
    let duplicate = harness
        .complete_durable_effect(&boundary)
        .await
        .expect("duplicate local execution");

    assert_eq!(first["execution_count"], 1);
    assert_eq!(duplicate["execution_count"], 2);
    assert_eq!(duplicate["runtime_effect"]["local_executor_called"], true);
}

#[tokio::test]
async fn process_wake_uses_structural_source_identity_for_queued_work_claims() {
    let mut harness = harness();
    let payload = json!({
        "session": "session-001",
        "process_id": "sim-process-session-001-001",
        "sequence": 1,
    });
    let first = harness
        .deliver_process_wake(&event(
            BoundaryKind::ProcessWake,
            "wake:first",
            payload.clone(),
        ))
        .await
        .expect("first wake");
    let duplicate = harness
        .deliver_process_wake(&event(BoundaryKind::ProcessWake, "wake:dupe", payload))
        .await
        .expect("duplicate wake");

    let delivered = [
        crate::scheduler::DeliveredBoundary {
            schema: "test".to_string(),
            sequence: 1,
            scheduler: crate::scheduler::SchedulerDeliveryEvidence::default(),
            boundary_id: "wake:first".to_string(),
            actor_alias: "session-001".to_string(),
            kind: BoundaryKind::ProcessWake,
            at: 1,
            label: "test".to_string(),
            payload: json!({
                "process_id": "sim-process-session-001-001",
                "sequence": 1,
            }),
            observed: first.clone(),
        },
        crate::scheduler::DeliveredBoundary {
            schema: "test".to_string(),
            sequence: 2,
            scheduler: crate::scheduler::SchedulerDeliveryEvidence::default(),
            boundary_id: "wake:dupe".to_string(),
            actor_alias: "session-001".to_string(),
            kind: BoundaryKind::ProcessWake,
            at: 2,
            label: "test".to_string(),
            payload: json!({
                "process_id": "sim-process-session-001-001",
                "sequence": 1,
            }),
            observed: duplicate.clone(),
        },
    ];
    let verdict = crate::oracles::process_wake_at_most_once(&delivered);
    assert!(
        verdict.is_passed(),
        "one logical wake must materialize into at most one runtime turn: {}",
        verdict.message
    );

    assert_eq!(first["claimed_once"], true);
    assert_eq!(duplicate["claimed_once"], false);
    assert_eq!(first["runtime_queued_work"]["claim_id_present"], true);
    assert_eq!(duplicate["runtime_queued_work"]["claim_id_present"], false);
}

#[tokio::test]
async fn process_lifecycle_boundary_drives_real_disposition_recovery() {
    // END TO END through the REAL DurableProcessWorker sweep: spawn / crash /
    // sweep / abandon-request produce the ADR 0019 verdicts, and both
    // lifecycle oracles pass on the real observation.
    let mut harness = harness();
    let observed = harness
        .run_process_lifecycle(&event(
            BoundaryKind::ProcessLifecycle,
            "session-001:process-lifecycle:001",
            json!({ "session": "session-001" }),
        ))
        .await
        .expect("process lifecycle boundary");

    let processes = observed
        .pointer("/runtime_process_lifecycle/processes")
        .and_then(Value::as_array)
        .expect("recorded lifecycle processes");
    let by_id = |id: &str| {
        processes
            .iter()
            .find(|process| process["process_id"] == id)
            .unwrap_or_else(|| panic!("missing process `{id}`: {observed}"))
            .clone()
    };
    // Started OwnerBound + unexpired stale holder remains non-terminal and is
    // never re-run.
    let ob = by_id("ob-crashed");
    assert_eq!(ob["terminal_status"], "running");
    assert_eq!(ob["lease_lapsed"], false);
    assert_eq!(ob["reran"], false);
    // Rerunnable IS re-run to a run terminal.
    let rerun = by_id("rerun-crashed");
    assert_eq!(rerun["reran"], true);
    assert_ne!(rerun["terminal_status"], "abandoned");
    // OwnerBound + operator-authorized abandonment + lapsed lease -> reconciled.
    let reconciled = by_id("ob-abandon-req");
    assert_eq!(reconciled["terminal_status"], "abandoned");
    assert_eq!(reconciled["abandon_writer"], "reconciled_request");
    assert_eq!(reconciled["abandon_requested"], true);
    assert_eq!(reconciled["reran"], false);

    let delivered = crate::scheduler::DeliveredBoundary {
        schema: "test".to_string(),
        sequence: 0,
        scheduler: crate::scheduler::SchedulerDeliveryEvidence::default(),
        boundary_id: "session-001:process-lifecycle:001".to_string(),
        actor_alias: "session-001".to_string(),
        kind: BoundaryKind::ProcessLifecycle,
        at: 1,
        label: "process.lifecycle.recovery".to_string(),
        payload: json!({ "session": "session-001" }),
        observed,
    };
    let events = std::slice::from_ref(&delivered);
    assert!(
        crate::oracles::process_never_double_started(events).is_passed(),
        "the real recovery must satisfy the double-start oracle"
    );
    assert!(
        crate::oracles::abandoned_requires_evidence(events).is_passed(),
        "the real recovery must satisfy the evidence oracle"
    );
}

#[tokio::test]
async fn tool_boundary_uses_runtime_effect_controller_and_records_output() {
    let mut harness = harness();
    let observed = harness
        .complete_tool(&event(
            BoundaryKind::Tool,
            "tool:001",
            json!({
                "tool": "lookup",
                "output": {"answer": "tool data"},
            }),
        ))
        .await
        .expect("tool boundary");

    assert_eq!(observed["execution_count"], 1);
    assert_eq!(
        observed["runtime_effect"]["controller"],
        "sqlite_runtime_effect_controller"
    );
    assert_eq!(observed["runtime_tool_record"]["tool"], "lookup");
    assert!(
        observed["runtime_tool_output"]
            .to_string()
            .contains("tool data")
    );
}

#[tokio::test]
async fn exec_boundary_uses_runtime_effect_controller_and_preserves_exit_data() {
    let mut harness = harness();
    let observed = harness
        .execute_code(&event(
            BoundaryKind::ExecCode,
            "exec:001",
            json!({
                "output": "exec data",
                "exit_code": 7,
            }),
        ))
        .await
        .expect("exec boundary");

    assert_eq!(observed["execution_count"], 1);
    assert_eq!(
        observed["runtime_effect"]["controller"],
        "sqlite_runtime_effect_controller"
    );
    assert_eq!(observed["exit_code"], 7);
    assert!(
        observed["runtime_effect_outcome"]
            .to_string()
            .contains("exec data")
    );
}

#[tokio::test]
async fn journaled_exec_boundary_matches_model_replay_projection() {
    let boundary = event(
        BoundaryKind::ExecCode,
        "session-001:exec-code:001",
        json!({
            "output": "exec result 1 for session-001",
            "exit_code": 0,
        }),
    );
    let mut live_harness = harness();
    let live = live_harness
        .execute_code(&boundary)
        .await
        .expect("live journaled exec boundary");
    let replay = crate::store::ModelStore::default().project_boundary_observation(&boundary);

    assert_eq!(
        live, replay,
        "journaled exec-code runtime truth and model replay projection diverged"
    );
    assert!(
        live.get("runtime_effect_outcome").is_some(),
        "the exact boundary payload must retain the settled journaled outcome"
    );
}

#[tokio::test]
async fn worker_stale_completion_uses_runtime_session_lease_store() {
    let mut harness = harness();
    let observed = harness
        .run_worker_stale_completion(&event(
            BoundaryKind::Worker,
            "worker-001",
            json!({"session": "session-001"}),
        ))
        .await
        .expect("worker boundary");

    assert_eq!(observed["stale_completion_rejected"], true);
    assert_eq!(observed["process_stale_completion_rejected"], true);
    assert_eq!(observed["process_stale_output_absent"], true);
    assert_eq!(observed["process_terminal_writer"], "successor");
    assert_eq!(observed["process_terminal_event_count"], 1);
    assert_eq!(
        observed["runtime_worker_store"]["session_execution_lease_acquired_after_ttl"],
        true
    );
    assert!(observed["runtime_active_lease"].is_object());
    let work = &observed["runtime_worker_store"]["worker_owned_work"];
    assert_eq!(work["first_owner_claimed_work"], true);
    assert_eq!(work["second_owner_resumed_work"], true);
    assert_eq!(work["second_owner_outranks_first"], true);
    assert_eq!(work["stale_work_completion_rejected"], true);
    assert!(
        work["second_owner_claim_fencing_token"].as_u64().unwrap()
            > work["first_owner_claim_fencing_token"].as_u64().unwrap()
    );

    let repeated = harness
        .run_worker_stale_completion(&event(
            BoundaryKind::Worker,
            "worker-002",
            json!({"session": "session-001"}),
        ))
        .await
        .expect("second worker boundary for the same memory-backed session");
    assert_eq!(repeated["process_stale_completion_rejected"], true);
    assert_eq!(repeated["process_stale_output_absent"], true);
    assert_eq!(repeated["process_terminal_writer"], "successor");
    assert_eq!(repeated["process_terminal_event_count"], 1);
    assert_ne!(
        observed["runtime_worker_store"]["process_completion"]["process_id"],
        repeated["runtime_worker_store"]["process_completion"]["process_id"],
        "each generated worker boundary must own an independent process proof"
    );
}

#[tokio::test]
async fn worker_stale_process_completion_is_fenced_by_sqlite_registry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let clock = crate::clock::SimClock::new();
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(temp.path().join("sessions"))
            .with_clock(clock.clone()),
    );
    let mut harness = RuntimeBoundaryHarness::new(
        factory,
        RuntimeEffectReplayStore::sqlite_file(temp.path().join("effects.sqlite")),
        clock,
    );
    let observed = harness
        .run_worker_stale_completion(&event(
            BoundaryKind::Worker,
            "worker-sqlite-001",
            json!({"session": "session-sqlite-001"}),
        ))
        .await
        .expect("SQLite worker boundary");

    let process = &observed["runtime_worker_store"]["process_completion"];
    assert_eq!(process["stale_completion_rejected"], true);
    assert_eq!(process["stale_output_absent"], true);
    assert_eq!(process["terminal_writer"], "successor");
    assert_eq!(process["terminal_event_count"], 1);
    assert_eq!(process["fencing_token_advanced"], true);

    let repeated = harness
        .run_worker_stale_completion(&event(
            BoundaryKind::Worker,
            "worker-sqlite-002",
            json!({"session": "session-sqlite-001"}),
        ))
        .await
        .expect("second worker boundary for the same SQLite-backed session");
    let repeated_process = &repeated["runtime_worker_store"]["process_completion"];
    assert_eq!(repeated_process["stale_completion_rejected"], true);
    assert_eq!(repeated_process["stale_output_absent"], true);
    assert_eq!(repeated_process["terminal_writer"], "successor");
    assert_eq!(repeated_process["terminal_event_count"], 1);
    assert_ne!(
        process["process_id"], repeated_process["process_id"],
        "each generated worker boundary must own an independent SQLite process proof"
    );
}

#[derive(Clone, Copy, Debug)]
enum SegmentCrashPoint {
    BeforeHandoverPersist,
    AfterPersistBeforeSuccessor,
    DuringSuccessorResume,
}

fn sqlite_segment_harness(root: &std::path::Path) -> RuntimeBoundaryHarness {
    std::fs::create_dir_all(root).expect("create SQLite segment harness directory");
    let clock = crate::clock::SimClock::new();
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(root.join("sessions"))
            .with_clock(clock.clone()),
    );
    RuntimeBoundaryHarness::new(
        factory,
        RuntimeEffectReplayStore::sqlite_file(root.join("effects.sqlite")),
        clock,
    )
}

async fn run_seeded_segment_effect(
    harness: &mut RuntimeBoundaryHarness,
    process_id: &str,
    effect_ordinal: u64,
) -> Value {
    harness
        .complete_durable_effect(&event(
            BoundaryKind::DurableEffect,
            &format!("{process_id}:effect:{effect_ordinal}"),
            json!({
                "durable_key": format!("{process_id}:effect:{effect_ordinal}"),
                "result": {"ordinal": effect_ordinal, "value": effect_ordinal * 11},
            }),
        ))
        .await
        .expect("seeded durable effect")
}

#[tokio::test]
async fn sqlite_seeded_segment_crash_matrix_preserves_results_and_effect_identity() {
    let seed = 0x5eed_u64;
    let budget = 2 + seed % 3;
    let effect_count = budget * 2 + 1;

    for crash_point in [
        SegmentCrashPoint::BeforeHandoverPersist,
        SegmentCrashPoint::AfterPersistBeforeSuccessor,
        SegmentCrashPoint::DuringSuccessorResume,
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut baseline = sqlite_segment_harness(&temp.path().join("baseline"));
        let mut baseline_results = Vec::new();
        for effect in 0..effect_count {
            baseline_results.push(
                run_seeded_segment_effect(&mut baseline, "unsegmented", effect).await
                    ["result_digest"]
                    .clone(),
            );
        }

        let root = temp.path().join("segmented");
        let process_id = format!("segmented-{crash_point:?}").to_ascii_lowercase();
        let mut harness = sqlite_segment_harness(&root);
        let registry = harness
            .ensure_worker_process_registry()
            .await
            .expect("SQLite process registry");
        registry
            .register_process(ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: json!({"seed": seed, "budget": budget}),
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            ))
            .await
            .expect("register seeded process");

        let mut segmented_results = Vec::new();
        for effect in 0..budget {
            segmented_results.push(
                run_seeded_segment_effect(&mut harness, &process_id, effect).await["result_digest"]
                    .clone(),
            );
        }
        let handover = lash_core::PersistedSegmentHandover {
            segment_ordinal: 1,
            program_hash: "sim-seeded-program-v1".to_string(),
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("sim-seeded-program-v1".to_string()),
                engine_state: budget.to_le_bytes().to_vec(),
            },
        };

        match crash_point {
            SegmentCrashPoint::BeforeHandoverPersist => {
                harness = sqlite_segment_harness(&root);
                harness
                    .ensure_worker_process_continuations()
                    .await
                    .expect("reopen registry")
                    .put_segment_handover(&process_id, handover.clone())
                    .await
                    .expect("persist recovered handover");
            }
            SegmentCrashPoint::AfterPersistBeforeSuccessor => {
                harness
                    .ensure_worker_process_continuations()
                    .await
                    .expect("continuation store")
                    .put_segment_handover(&process_id, handover.clone())
                    .await
                    .expect("persist handover");
                harness = sqlite_segment_harness(&root);
                harness
                    .ensure_worker_process_continuations()
                    .await
                    .expect("reopen registry")
                    .put_segment_handover(&process_id, handover.clone())
                    .await
                    .expect("idempotent handover replay");
            }
            SegmentCrashPoint::DuringSuccessorResume => {
                harness
                    .ensure_worker_process_continuations()
                    .await
                    .expect("continuation store")
                    .put_segment_handover(&process_id, handover.clone())
                    .await
                    .expect("persist handover");
                harness = sqlite_segment_harness(&root);
                segmented_results.push(
                    run_seeded_segment_effect(&mut harness, &process_id, budget).await
                        ["result_digest"]
                        .clone(),
                );
                harness = sqlite_segment_harness(&root);
                let replay = run_seeded_segment_effect(&mut harness, &process_id, budget).await;
                assert_eq!(
                    replay["replayed"], true,
                    "resume replay must not re-execute"
                );
            }
        }

        let start = if matches!(crash_point, SegmentCrashPoint::DuringSuccessorResume) {
            budget + 1
        } else {
            budget
        };
        for effect in start..effect_count {
            segmented_results.push(
                run_seeded_segment_effect(&mut harness, &process_id, effect).await["result_digest"]
                    .clone(),
            );
        }
        assert_eq!(segmented_results, baseline_results, "result identity");

        let continuations = harness
            .ensure_worker_process_continuations()
            .await
            .expect("final continuation store");
        assert_eq!(
            continuations
                .latest_segment_handover(&process_id)
                .await
                .expect("latest handover")
                .expect("live successor")
                .segment_ordinal,
            1,
            "restart must leave an addressable successor"
        );
        let registry = harness
            .ensure_worker_process_registry()
            .await
            .expect("final registry");
        let terminal = ProcessAwaitOutput::Success {
            value: json!({"effects": effect_count, "seed": seed}),
            control: None,
        };
        registry
            .complete_process(
                &process_id,
                terminal.clone(),
                lash_core::ProcessCompletionAuthority::workflow_key(&process_id),
            )
            .await
            .expect("complete real terminal");
        assert_eq!(
            lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry))
                .await_terminal(&process_id)
                .await
                .expect("terminal after crash matrix"),
            terminal
        );
        let terminal_events = registry
            .events_after(&process_id, 0)
            .await
            .expect("terminal events")
            .into_iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count();
        assert_eq!(terminal_events, 1, "terminal is written once");
    }
}
