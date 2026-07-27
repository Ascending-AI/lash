use super::*;

fn started_for(lease: &crate::ProcessLease, attempt: u32) -> ProcessStarted {
    ProcessStarted {
        owner: lease.owner.clone(),
        fencing_token: lease.fencing_token,
        attempt,
        started_at_ms: u64::from(attempt),
    }
}

async fn release_lease(registry: &dyn ProcessRegistry, lease: &crate::ProcessLease) {
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(lease))
        .await
        .expect("release process lease");
}

/// Wave-1 execution fencing contract shared by memory, SQLite, and Postgres.
pub(super) async fn respects_recovery_disposition(registry: Arc<dyn ProcessRegistry>) {
    registry
        .register_process(rerunnable_registration("fence-rerunnable"))
        .await
        .expect("register rerunnable");
    let first = registry
        .claim_process_lease(
            "fence-rerunnable",
            &process_lease_owner("rerun-first"),
            60_000,
        )
        .await
        .expect("claim first rerunnable attempt")
        .acquired()
        .expect("first rerunnable lease");
    let first_started = started_for(&first, 1);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "fence-rerunnable",
                first_started.clone(),
                &ProcessExecutionWriteAuthority::lease(first.clone()),
            )
            .await
            .expect("record first rerunnable attempt"),
        ProcessStartOutcome::Started(_)
    ));
    release_lease(registry.as_ref(), &first).await;
    let second = registry
        .claim_process_lease(
            "fence-rerunnable",
            &process_lease_owner("rerun-second"),
            60_000,
        )
        .await
        .expect("claim second rerunnable attempt")
        .acquired()
        .expect("second rerunnable lease");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "fence-rerunnable",
                started_for(&first, 2),
                &ProcessExecutionWriteAuthority::lease(first.clone()),
            )
            .await,
        Err(crate::PluginError::ProcessLeaseSuperseded { .. })
    ));
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "fence-rerunnable",
                started_for(&second, 2),
                &ProcessExecutionWriteAuthority::lease(second.clone()),
            )
            .await
            .expect("record second rerunnable attempt"),
        ProcessStartOutcome::Started(_)
    ));
    let fenced_wait = WaitState {
        since_ms: 2,
        kind: WaitKind::Signal {
            name: "continue".to_string(),
            event_type: "signal.continue".to_string(),
            key: "process:fence-rerunnable:signal.continue:1".to_string(),
            ordinal: 1,
        },
    };
    assert!(matches!(
        registry
            .set_process_wait_with_authority(
                "fence-rerunnable",
                fenced_wait.clone(),
                &ProcessExecutionWriteAuthority::lease(first.clone()),
            )
            .await,
        Err(crate::PluginError::ProcessLeaseSuperseded { .. })
    ));
    registry
        .set_process_wait_with_authority(
            "fence-rerunnable",
            fenced_wait,
            &ProcessExecutionWriteAuthority::lease(second.clone()),
        )
        .await
        .expect("current runner enters wait");
    assert!(matches!(
        registry
            .clear_process_wait_with_authority(
                "fence-rerunnable",
                &ProcessExecutionWriteAuthority::lease(first.clone()),
            )
            .await,
        Err(crate::PluginError::ProcessLeaseSuperseded { .. })
    ));
    registry
        .clear_process_wait_with_authority(
            "fence-rerunnable",
            &ProcessExecutionWriteAuthority::lease(second.clone()),
        )
        .await
        .expect("current runner clears wait");
    assert!(matches!(
        registry
            .append_event_with_authority(
                "fence-rerunnable",
                ProcessEventAppendRequest::new(
                    "process.progress",
                    serde_json::json!({"writer": "stale"}),
                ),
                &ProcessExecutionWriteAuthority::lease(first.clone()),
            )
            .await,
        Err(crate::PluginError::ProcessLeaseSuperseded { .. })
    ));
    let rerun_events = registry
        .events_after("fence-rerunnable", 0)
        .await
        .expect("read rerunnable attempts");
    assert_eq!(
        rerun_events
            .iter()
            .filter(|event| event.event_type == "process.first_started")
            .count(),
        2,
        "a recovered Rerunnable row records a new attempt generation"
    );
    release_lease(registry.as_ref(), &second).await;

    registry
        .register_process(owner_bound_registration("fence-owner-bound"))
        .await
        .expect("register owner-bound");
    let owner_bound_lease = registry
        .claim_process_lease(
            "fence-owner-bound",
            &process_lease_owner("owner-bound-first"),
            60_000,
        )
        .await
        .expect("claim owner-bound")
        .acquired()
        .expect("owner-bound lease");
    let owner_bound_started = started_for(&owner_bound_lease, 1);
    registry
        .record_first_started_with_authority(
            "fence-owner-bound",
            owner_bound_started.clone(),
            &ProcessExecutionWriteAuthority::lease(owner_bound_lease.clone()),
        )
        .await
        .expect("record owner-bound start");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "fence-owner-bound",
                owner_bound_started,
                &ProcessExecutionWriteAuthority::lease(owner_bound_lease.clone()),
            )
            .await
            .expect("replay owner-bound start"),
        ProcessStartOutcome::AlreadyApplied(_)
    ));
    let other = ProcessStarted {
        owner: process_lease_owner("owner-bound-other"),
        fencing_token: 0,
        attempt: 2,
        started_at_ms: 2,
    };
    match registry
        .record_first_started_with_authority(
            "fence-owner-bound",
            other,
            &ProcessExecutionWriteAuthority::testing("fence-owner-bound"),
        )
        .await
        .expect("typed owner-bound rejection")
    {
        ProcessStartOutcome::AlreadyStarted { by, .. } => {
            assert_eq!(by, owner_bound_lease.owner)
        }
        other => panic!("expected AlreadyStarted, got {other:?}"),
    }
    release_lease(registry.as_ref(), &owner_bound_lease).await;

    registry
        .register_process(rerunnable_registration("fence-transient"))
        .await
        .expect("register transient row");
    let transient = registry
        .claim_process_lease(
            "fence-transient",
            &process_lease_owner("transient-first"),
            60_000,
        )
        .await
        .expect("claim transient row")
        .acquired()
        .expect("transient lease");
    release_lease(registry.as_ref(), &transient).await;
    let reclaimed = registry
        .claim_process_lease(
            "fence-transient",
            &process_lease_owner("transient-second"),
            60_000,
        )
        .await
        .expect("reclaim transient row")
        .acquired()
        .expect("transient row remains claimable");
    registry
        .record_first_started_with_authority(
            "fence-transient",
            started_for(&reclaimed, 1),
            &ProcessExecutionWriteAuthority::lease(reclaimed.clone()),
        )
        .await
        .expect("subsequent execution starts after transient failure")
        .into_record()
        .expect("subsequent execution is accepted");
    let completed = registry
        .complete_process_with_lease(
            &reclaimed,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"ran": true}),
                control: None,
            },
        )
        .await
        .expect("subsequent execution completes");
    assert!(matches!(&completed, ProcessCompletionOutcome::Committed(_)));

    registry
        .register_process(rerunnable_registration("fence-terminal-replay"))
        .await
        .expect("register terminal replay");
    let terminal_lease = registry
        .claim_process_lease(
            "fence-terminal-replay",
            &process_lease_owner("terminal-writer"),
            60_000,
        )
        .await
        .expect("claim terminal writer")
        .acquired()
        .expect("terminal lease");
    let stored_output = ProcessAwaitOutput::Success {
        value: serde_json::json!({"stored": true}),
        control: None,
    };
    registry
        .complete_process_with_lease(&terminal_lease, stored_output.clone())
        .await
        .expect("commit terminal");
    let replayed = registry
        .complete_process_with_lease(
            &terminal_lease,
            ProcessAwaitOutput::Failure {
                class: crate::ToolFailureClass::Execution,
                code: "divergent".to_string(),
                message: "must not replace stored terminal".to_string(),
                raw: None,
                control: None,
            },
        )
        .await
        .expect("terminal replay adopts stored outcome");
    assert_eq!(replayed.status.await_output(), Some(&stored_output));

    registry
        .register_process(rerunnable_registration("fence-exhausted").with_max_attempts(Some(1)))
        .await
        .expect("register bounded rerunnable");
    let bounded_first = registry
        .claim_process_lease(
            "fence-exhausted",
            &process_lease_owner("bounded-first"),
            60_000,
        )
        .await
        .expect("claim bounded first")
        .acquired()
        .expect("bounded first lease");
    registry
        .record_first_started_with_authority(
            "fence-exhausted",
            started_for(&bounded_first, 1),
            &ProcessExecutionWriteAuthority::lease(bounded_first.clone()),
        )
        .await
        .expect("record bounded first");
    release_lease(registry.as_ref(), &bounded_first).await;
    let sweep = registry
        .claim_process_lease(
            "fence-exhausted",
            &process_lease_owner("bounded-sweep"),
            60_000,
        )
        .await
        .expect("claim bounded sweep")
        .acquired()
        .expect("bounded sweep lease");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "fence-exhausted",
                started_for(&sweep, 2),
                &ProcessExecutionWriteAuthority::lease(sweep.clone()),
            )
            .await
            .expect("bounded start decision"),
        ProcessStartOutcome::AttemptsExhausted {
            attempts: 1,
            max_attempts: 1,
            ..
        }
    ));
    let terminal = registry
        .complete_process_with_lease(
            &sweep,
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(AbandonEvidence {
                    writer: AbandonWriter::EngineGaveUp,
                    owner: Some(bounded_first.owner),
                    epoch_ms: 3,
                }),
                control: None,
            },
        )
        .await
        .expect("sweep terminalizes exhausted process");
    assert_eq!(
        terminal.status.terminal_state(),
        Some(ProcessTerminalState::Abandoned)
    );
}

pub(super) async fn leased_terminal_replay_returns_stored_record(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "proc-lease-terminal-replay";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register");
    let current = registry
        .claim_process_lease(process_id, &process_lease_owner("current-owner"), 60_000)
        .await
        .expect("current lease")
        .acquired()
        .expect("current lease acquired");

    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!({"writer": "current"}),
        control: None,
    };
    let completed = registry
        .complete_process_with_lease(&current, output.clone())
        .await
        .expect("current lease completes");
    assert!(matches!(completed, ProcessCompletionOutcome::Committed(_)));
    assert!(completed.is_terminal());
    assert!(
        registry
            .get_process_lease(process_id)
            .await
            .expect("read released lease")
            .is_none(),
        "terminal append and lease release must commit together"
    );
    let replayed = registry
        .complete_process_with_lease(&current, output)
        .await
        .expect("same leased terminal replay is idempotent");
    assert!(matches!(
        &replayed,
        ProcessCompletionOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(
        serde_json::to_value(replayed.stored()).expect("serialize replayed terminal record"),
        serde_json::to_value(completed.stored()).expect("serialize completed terminal record"),
        "replaying the same terminal event must return the existing terminal record"
    );
    let terminal_events = registry
        .events_after(process_id, 0)
        .await
        .expect("terminal events")
        .into_iter()
        .filter(|event| event.semantics.terminal.is_some())
        .count();
    assert_eq!(
        terminal_events, 1,
        "terminal output must append exactly once"
    );
}
