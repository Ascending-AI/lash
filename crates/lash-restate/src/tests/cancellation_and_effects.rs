use super::*;

#[tokio::test]
pub(super) async fn execute_await_event_forwards_the_invocation_replay_key() {
    assert_eq!(
        crate::controller::context::LASH_REPLAY_KEY_HEADER,
        "x-lash-replay-key"
    );

    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let invocation = runtime_invocation(RuntimeEffectKind::AwaitEvent, "correlated-await");
    let expected_replay_key = invocation.replay_key().to_owned();
    let key = test_restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::Custom {
            key: "correlated-await".to_owned(),
        },
    )
    .expect("correlated await-event key");
    let expected_resolution = Resolution::Ok(serde_json::json!({ "ready": true }));
    context.resolve_durable_event(RestateDurableWaitResolveRequest {
        key: key.clone(),
        resolution: expected_resolution.clone(),
    });

    let outcome = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(invocation, RuntimeEffectCommand::AwaitEvent { key }),
            RuntimeEffectLocalExecutor::await_event(
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
        )
        .await
        .expect("pre-resolved await-event effect");

    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent { resolution }
            if resolution == expected_resolution
    ));
    assert_eq!(
        *context.awaited_replay_keys.lock_recover(),
        vec![expected_replay_key]
    );
}

#[tokio::test]
pub(super) async fn execute_await_event_rejects_foreign_authority_before_context_work() {
    let context = Arc::new(RecordingContext::default());
    let owner = RestateAuthorityId::new("execute-await-owner").expect("valid owner authority");
    let foreign =
        RestateAuthorityId::new("execute-await-foreign").expect("valid foreign authority");
    let controller = RestateRuntimeEffectController::new(context.clone(), owner);
    let key = restate_await_event_key_for_authority(
        &foreign,
        &durable_turn_scope("foreign-await-session", "turn"),
        AwaitEventWaitIdentity::Custom {
            key: "foreign-await".to_string(),
        },
    )
    .expect("foreign authority key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let error = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::AwaitEvent, "foreign-authority-await"),
                RuntimeEffectCommand::AwaitEvent { key },
            ),
            RuntimeEffectLocalExecutor::await_event(cancellation, None),
        )
        .await
        .expect_err("foreign authority must refuse before handler context work");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::AwaitEventUnknownOrRevoked
    );
    assert_eq!(
        context.session_revocation_checks.load(Ordering::SeqCst),
        0,
        "authority refusal must precede the session-tombstone lookup"
    );
}

#[tokio::test]
pub(super) async fn recording_context_propagates_revoked_session_from_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    RestateControllerContext::update_session_waits(
        &context,
        SessionId::from("recording-revoked"),
        true,
    )
    .await
    .expect("revoke recording-context session");

    let outcome = RestateControllerContext::sleep_or_turn_cancel(
        &context,
        Duration::from_secs(60),
        Some(test_turn_cancel_wait_request(
            &SessionId::from("recording-revoked"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("recording-context revoked verdict");

    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
            if session_id == "recording-revoked"
    ));
}

#[tokio::test]
pub(super) async fn positional_replay_context_propagates_revoked_session_from_turn_cancel_gate() {
    let context = Arc::new(PositionalReplayContext::default());
    RestateControllerContext::update_session_waits(
        &context,
        SessionId::from("positional-revoked"),
        true,
    )
    .await
    .expect("revoke positional-context session");

    let outcome = RestateControllerContext::sleep_or_turn_cancel(
        &context,
        Duration::from_secs(60),
        Some(test_turn_cancel_wait_request(
            &SessionId::from("positional-revoked"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("positional-context revoked verdict");

    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
            if session_id == "positional-revoked"
    ));
}

#[tokio::test]
pub(super) async fn replayable_recording_context_propagates_revoked_session_from_turn_cancel_gate()
{
    let context = Arc::new(ReplayableRecordingContext::default());
    RestateControllerContext::update_session_waits(
        &context,
        SessionId::from("replayable-revoked"),
        true,
    )
    .await
    .expect("revoke replayable-context session");

    let outcome = RestateControllerContext::sleep_or_turn_cancel(
        &context,
        Duration::from_secs(60),
        Some(test_turn_cancel_wait_request(
            &SessionId::from("replayable-revoked"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("replayable-context revoked verdict");

    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
            if session_id == "replayable-revoked"
    ));
}

#[tokio::test]
pub(super) async fn recording_context_process_await_reports_turn_cancelled() {
    let context = Arc::new(RecordingContext::default());
    let turn_cancel =
        test_turn_cancel_wait_request(&SessionId::from("recording-process"), &TurnId::from("turn"));
    let task_context = Arc::clone(&context);
    let task = tokio::spawn(async move {
        RestateControllerContext::await_process_terminal_or_turn_cancel(
            &task_context,
            ProcessId::from("recording-process-child"),
            Some(turn_cancel),
            crate::controller::context::ProcessCancelRace::NotRaced,
        )
        .await
    });
    wait_for_test_turn_cancel_registration(&context.turn_cancel_gate).await;
    let turn_cancel =
        test_turn_cancel_wait_request(&SessionId::from("recording-process"), &TurnId::from("turn"));
    RestateControllerContext::resolve_event(
        &context,
        RestateDurableWaitResolveRequest {
            key: turn_cancel.key,
            resolution: Resolution::Cancelled,
        },
    )
    .await
    .expect("resolve recording-context gate");

    let outcome = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("recording-context process await must wake")
        .expect("join recording-context process await")
        .expect("recording-context process await outcome");
    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::TurnCancelled
    ));
}

#[tokio::test]
pub(super) async fn positional_replay_context_process_await_reports_turn_cancelled() {
    let context = Arc::new(PositionalReplayContext::default());
    let turn_cancel = test_turn_cancel_wait_request(
        &SessionId::from("positional-process"),
        &TurnId::from("turn"),
    );
    let task_context = Arc::clone(&context);
    let task = tokio::spawn(async move {
        RestateControllerContext::await_process_terminal_or_turn_cancel(
            &task_context,
            ProcessId::from("positional-process-child"),
            Some(turn_cancel),
            crate::controller::context::ProcessCancelRace::NotRaced,
        )
        .await
    });
    wait_for_test_turn_cancel_registration(&context.turn_cancel_gate).await;
    let turn_cancel = test_turn_cancel_wait_request(
        &SessionId::from("positional-process"),
        &TurnId::from("turn"),
    );
    RestateControllerContext::resolve_event(
        &context,
        RestateDurableWaitResolveRequest {
            key: turn_cancel.key,
            resolution: Resolution::Cancelled,
        },
    )
    .await
    .expect("resolve positional-context gate");

    let outcome = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("positional-context process await must wake")
        .expect("join positional-context process await")
        .expect("positional-context process await outcome");
    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::TurnCancelled
    ));
}

#[tokio::test]
pub(super) async fn replayable_recording_context_process_await_reports_turn_cancelled() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let turn_cancel = test_turn_cancel_wait_request(
        &SessionId::from("replayable-process"),
        &TurnId::from("turn"),
    );
    let task_context = Arc::clone(&context);
    let task = tokio::spawn(async move {
        RestateControllerContext::await_process_terminal_or_turn_cancel(
            &task_context,
            ProcessId::from("replayable-process-child"),
            Some(turn_cancel),
            crate::controller::context::ProcessCancelRace::NotRaced,
        )
        .await
    });
    wait_for_test_turn_cancel_registration(&context.events.turn_cancel_gate).await;
    let turn_cancel = test_turn_cancel_wait_request(
        &SessionId::from("replayable-process"),
        &TurnId::from("turn"),
    );
    RestateControllerContext::resolve_event(
        &context,
        RestateDurableWaitResolveRequest {
            key: turn_cancel.key,
            resolution: Resolution::Cancelled,
        },
    )
    .await
    .expect("resolve replayable-context gate");

    let outcome = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("replayable-context process await must wake")
        .expect("join replayable-context process await")
        .expect("replayable-context process await outcome");
    assert!(matches!(
        outcome,
        RestateTurnCancelRaceOutcome::TurnCancelled
    ));
}

#[tokio::test]
pub(super) async fn completed_waits_unregister_the_shared_test_turn_cancel_gate() {
    let recording = Arc::new(RecordingContext::default());
    RestateControllerContext::sleep_or_turn_cancel(
        &recording,
        Duration::ZERO,
        Some(test_turn_cancel_wait_request(
            &SessionId::from("recording-complete"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("complete recording-context wait");
    assert_eq!(recording.turn_cancel_gate.registration_count(), 0);

    let positional = Arc::new(PositionalReplayContext::default());
    RestateControllerContext::sleep_or_turn_cancel(
        &positional,
        Duration::ZERO,
        Some(test_turn_cancel_wait_request(
            &SessionId::from("positional-complete"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("complete positional-context wait");
    assert_eq!(positional.turn_cancel_gate.registration_count(), 0);

    let replayable = Arc::new(ReplayableRecordingContext::default());
    RestateControllerContext::sleep_or_turn_cancel(
        &replayable,
        Duration::ZERO,
        Some(test_turn_cancel_wait_request(
            &SessionId::from("replayable-complete"),
            &TurnId::from("turn"),
        )),
        crate::controller::context::ProcessCancelRace::NotRaced,
    )
    .await
    .expect("complete replayable-context wait");
    assert_eq!(replayable.events.turn_cancel_gate.registration_count(), 0);
}

#[test]
pub(super) fn restate_turn_cancel_race_excludes_process_owned_waits() {
    let turn_scope = durable_turn_scope("session", "turn");
    let process_scoped_sleep = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::process("worker"),
            "session:turn:1:0:process:worker:sleep:1",
        )
        .expect("valid process sleep address"),
        lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 0),
        "parent:process:worker:sleep:1",
    );
    assert!(
        restate_timer_turn_cancel_wait_request(
            &test_restate_authority_id(),
            &process_scoped_sleep,
            false,
            None
        )
        .expect("process sleep classification")
        .is_none(),
        "background process sleep must outlive its originating turn"
    );
    assert!(
        restate_timer_turn_cancel_wait_request(
            &test_restate_authority_id(),
            &process_scoped_sleep,
            true,
            Some(&ExecutionScope::process("worker")),
        )
        .expect("explicit process scope")
        .is_none(),
        "an explicitly process-owned wait must not observe its causal turn's cancel gate"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &test_restate_authority_id(),
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "process-wait"),
            false,
            None,
        )
        .expect("process await-event classification")
        .is_none(),
        "background process await-event must outlive its originating turn"
    );

    assert!(
        restate_timer_turn_cancel_wait_request(
            &test_restate_authority_id(),
            &runtime_invocation(RuntimeEffectKind::Sleep, "turn-sleep"),
            true,
            Some(&turn_scope),
        )
        .expect("turn sleep classification")
        .is_some(),
        "foreground turn sleep must observe the durable cancellation gate"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &test_restate_authority_id(),
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "turn-process-wait"),
            true,
            Some(&turn_scope),
        )
        .expect("foreground process await-event classification")
        .is_some(),
        "a Lashlang wait inside a foreground turn must observe the turn gate"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_executes_atomic_effect_inside_run() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let err = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::ToolAttempt, "step"),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call(),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect_err("unavailable local executor should be returned from ctx.run");

    assert_eq!(
        err.code,
        lash_core::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        &["lash:session:turn:1:0:tool_attempt:step".to_string()]
    );
    assert!(context.sleeps.lock_recover().is_empty());
}

#[tokio::test]
pub(super) async fn restate_positional_replay_records_tool_attempt_as_one_command() {
    let context = Arc::new(PositionalReplayContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let call = prepared_tool_call_with("call-fast", "fast_tool");
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-attempt"),
        RuntimeEffectCommand::ToolAttempt {
            call,
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_runs = Arc::new(AtomicUsize::new(0));

    let first = host
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing({
                let local_runs = Arc::clone(&local_runs);
                |_envelope| async move {
                    local_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record("call-fast", "fast_tool")),
                            intents: lash_core::ToolIntents::v3(vec![
                                lash_core::ToolIntent::StartProcess(Box::new(
                                    lash_core::StartProcessIntent {
                                        session_id: SessionId::from("session"),
                                        declaration: lash_core::ProcessStartDeclaration::external(
                                            lash_core::ProcessOriginator::host_scoped(
                                                "restate-positional-law",
                                            ),
                                            serde_json::json!({"captured": true}),
                                            lash_core::ProcessLifecyclePolicy::new(
                                                lash_core::ParentScope::Host,
                                                lash_core::OnParentEnd::Abandon,
                                            ),
                                        ),
                                    },
                                )),
                            ]),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    })
                }
            }),
        )
        .await
        .expect("first attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = first else {
        panic!("expected tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record, .. } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 1);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);

    context.start_replay();
    let replayed = host
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(|_| async {
                panic!("positional replay should not rerun the ToolAttempt executor")
            }),
        )
        .await
        .expect("replayed attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = replayed else {
        panic!("expected replayed tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record, .. } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 2);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
pub(super) async fn restate_controller_routes_sleep_only_through_timer() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "sleep"),
                RuntimeEffectCommand::Sleep {
                    spec: lash_core::SleepSpec::For { duration_ms: 42 },
                },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("sleep");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    assert_eq!(context.sleeps.lock_recover().as_slice(), &[42]);
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        &["lash:session:turn:1:0:sleep:sleep:frontier".to_string()],
        "the only run a sleep journals is its frontier marker (FIG-3779)"
    );
}

#[tokio::test]
pub(super) async fn restate_turn_wait_rejects_missing_cancel_scope() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new_for_test(context);
    let error = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "missing-cancel-scope"),
                RuntimeEffectCommand::Sleep {
                    spec: lash_core::SleepSpec::For { duration_ms: 1 },
                },
            ),
            RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect_err("turn sleep must not silently disable durable cancellation");
    assert_eq!(error.code.as_str(), "restate_turn_cancel_scope_missing");
}

/// FIG-3672 P9: a Restate timer races only the turn's durable gate. The
/// waiting execution's own token is not a race arm: a timer it could end
/// would end differently on a replay that never fired the token.
#[tokio::test]
pub(super) async fn restate_timer_is_not_ended_by_its_executions_token() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let sleep = host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Sleep, "cancelled-sleep"),
            RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For {
                    duration_ms: 60_000,
                },
            },
        ),
        RuntimeEffectLocalExecutor::sleep(cancellation)
            .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), sleep)
            .await
            .is_err(),
        "a fired token must not end a durable timer"
    );
    assert_eq!(context.sleeps.lock_recover().as_slice(), &[60_000]);
}

#[tokio::test]
pub(super) async fn restate_suspended_timer_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let sleep = tokio::spawn(async move {
        RestateRuntimeEffectController::new_for_test(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "suspended-sleep"),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: 300_000,
                        },
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(task_cancellation)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    wait_for_test_turn_cancel_registration(&context.turn_cancel_gate).await;
    assert!(!sleep.is_finished(), "timer must genuinely remain pending");

    let cancel_key = test_restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            key: cancel_key,
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-sleep",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let error = tokio::time::timeout(Duration::from_secs(1), sleep)
        .await
        .expect("durable cancel gate must wake a suspended timer promptly")
        .expect("join suspended timer")
        .expect_err("durable turn cancellation must abort the timer effect");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled
    );
    // The verdict rides the recorded outcome; no controller writes it back
    // into the waiting execution's token (FIG-3672 P9).
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
pub(super) async fn restate_suspended_await_event_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    let authority = RestateAuthorityId::new("suspended-await-owner").expect("valid test authority");
    let awaited_key = restate_await_event_key_for_authority(
        &authority,
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::Custom {
            key: "wait-for-signal".to_string(),
        },
    )
    .expect("await-event key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let task_authority = authority.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context, task_authority)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "suspended-await-event"),
                    RuntimeEffectCommand::AwaitEvent { key: awaited_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    wait_for_test_turn_cancel_registration(&context.turn_cancel_gate).await;
    assert!(
        !wait.is_finished(),
        "await-event must genuinely remain pending"
    );

    let cancel_key = restate_await_event_key_for_authority(
        &authority,
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            key: cancel_key,
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-await-event",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let outcome = tokio::time::timeout(Duration::from_secs(1), wait)
        .await
        .expect("durable cancel gate must wake a suspended await-event promptly")
        .expect("join suspended await-event")
        .expect("turn cancellation should terminalize the await-event");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));
    // The verdict rides the recorded outcome; no controller writes it back
    // into the waiting execution's token (FIG-3672 P9).
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
pub(super) async fn restate_routes_every_execution_scope_to_an_exact_durable_wait_address() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let scopes = [
        durable_turn_scope("session", "turn"),
        ExecutionScope::process("process"),
        ExecutionScope::queue_drain("session", "drain"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::runtime_operation("operation"),
    ];
    let mut addresses = HashSet::new();

    for (index, scope) in scopes.into_iter().enumerate() {
        let key = test_restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::Custom {
                key: format!("scope-{index}"),
            },
        )
        .expect("scope wait key");
        let address = RestateDurableWaitAddress::for_key(&key);
        assert!(addresses.insert(address.workflow_key.clone()));
        assert!(
            !address.index_key().contains('/'),
            "wait-index object keys must remain one ingress path segment"
        );
        let resolution = Resolution::Ok(serde_json::json!({ "scope": index }));
        assert_eq!(
            host.resolve_await_event(&key, resolution.clone())
                .await
                .expect("resolve scope wait"),
            ResolveOutcome::Accepted
        );
        assert_eq!(
            host.await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .expect("await scope wait"),
            resolution
        );
    }
}

#[tokio::test]
pub(super) async fn restate_execute_effect_honors_cancellation_and_terminalizes_late_resolution() {
    let context = Arc::new(RecordingContext::default());
    let authority = RestateAuthorityId::new("execute-cancel-owner").expect("valid test authority");
    let key = restate_await_event_key_for_authority(
        &authority,
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::tool_completion("cancel-tool"),
    )
    .expect("cancel wait key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = context.clone();
    let task_key = key.clone();
    let task_cancellation = cancellation.clone();
    let task_authority = authority.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context, task_authority)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "cancel-wait"),
                    RuntimeEffectCommand::AwaitEvent { key: task_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    wait_for_test_turn_cancel_registration(&context.turn_cancel_gate).await;
    assert!(
        !wait.is_finished(),
        "mock wait must genuinely remain pending"
    );
    // The turn's durable gate, not the execution's token, ends the wait
    // (FIG-3672 P9).
    cancellation.cancel();
    assert!(!wait.is_finished(), "the token alone ends nothing");
    let cancel_key = restate_await_event_key_for_authority(
        &authority,
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            key: cancel_key,
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": { "request_id": "cancel-wait", "origin": "test" },
            })),
        }),
        ResolveOutcome::Accepted
    );
    let outcome = wait
        .await
        .expect("join cancellation wait")
        .expect("cancel wait");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));

    let host = RestateRuntimeEffectController::new(context, authority);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Cancelled,
        }
    );
}

#[tokio::test]
pub(super) async fn restate_deadline_durably_terminalizes_timeout() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let key = test_restate_await_event_key(
        &ExecutionScope::runtime_operation("deadline-operation"),
        AwaitEventWaitIdentity::Custom {
            key: "deadline".to_string(),
        },
    )
    .expect("deadline key");
    let resolution = host
        .await_await_event(
            &key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now() + Duration::from_millis(10)),
        )
        .await
        .expect("deadline wait");
    assert_eq!(resolution, Resolution::Timeout);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late deadline resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Timeout,
        }
    );
}

#[tokio::test]
pub(super) async fn restate_session_cancel_cancels_current_waits_but_allows_new_waits() {
    let context = Arc::new(RecordingContext::default());
    let first_key = test_restate_await_event_key(
        &ExecutionScope::queue_drain("cancel-session", "drain-one"),
        AwaitEventWaitIdentity::Custom {
            key: "first".to_string(),
        },
    )
    .expect("first session wait");
    let task_context = context.clone();
    let task_key = first_key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new_for_test(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    host.cancel_await_events_for_session(&SessionId::from("cancel-session"))
        .await
        .expect("cancel session waits");
    assert_eq!(
        wait.await
            .expect("join cancelled session wait")
            .expect("cancelled session wait"),
        Resolution::Cancelled
    );

    let next_key = test_restate_await_event_key(
        &durable_turn_scope("cancel-session", "turn-two"),
        AwaitEventWaitIdentity::Custom {
            key: "next".to_string(),
        },
    )
    .expect("next session wait");
    let expected = Resolution::Ok(serde_json::json!("resumed"));
    host.resolve_await_event(&next_key, expected.clone())
        .await
        .expect("resolve new session wait");
    assert_eq!(
        host.await_await_event(&next_key, tokio_util::sync::CancellationToken::new(), None,)
            .await
            .expect("new wait after cancel"),
        expected
    );
}

#[tokio::test]
pub(super) async fn restate_session_delete_revokes_current_and_future_waits() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let key = test_restate_await_event_key(
        &ExecutionScope::session_delete("deleted-session"),
        AwaitEventWaitIdentity::Custom {
            key: "delete".to_string(),
        },
    )
    .expect("delete wait");
    let task_context = context.clone();
    let task_key = key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new_for_test(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    host.revoke_await_events_for_session(&SessionId::from("deleted-session"))
        .await
        .expect("revoke deleted session waits");
    assert_eq!(
        wait.await
            .expect("join deleted wait")
            .expect("deleted wait"),
        Resolution::Cancelled
    );

    let future_key = test_restate_await_event_key(
        &durable_turn_scope("deleted-session", "future-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "future".to_string(),
        },
    )
    .expect("future revoked wait");
    let future_error = host
        .await_await_event(
            &future_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("future revoked wait is not observable");
    assert_eq!(future_error.code.as_str(), "await_event_unknown_or_revoked");
    assert_eq!(
        host.resolve_await_event(&future_key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve after deletion"),
        ResolveOutcome::UnknownOrRevoked
    );
}

#[tokio::test]
pub(super) async fn restate_effect_host_checks_revocation_then_awaits_resolution() {
    let expected = Resolution::Ok(serde_json::json!({ "answer": "approved" }));
    let scripted = Arc::new(ScriptedHttpTransport::new([
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered("false"),
        },
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(
                serde_json::to_string(&expected).expect("encode resolution"),
            ),
        },
    ]));
    let host = RestateEffectHost::new_for_test(RestateConnection::with_transport(
        "https://restate.example",
        scripted.clone(),
    ));
    let key = test_restate_await_event_key(
        &durable_turn_scope("single-call-session", "single-call-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "single-call-wait".to_string(),
        },
    )
    .expect("single-call wait key");

    let resolution = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("await resolution through ingress");

    assert_eq!(resolution, expected);
    let requests = scripted.requests();
    assert_eq!(requests.len(), 2, "durable wait must check its tombstone");
    assert!(
        requests[0]
            .url
            .contains("/LashDurableWaitIndex/single-call-session/")
            && requests[0].url.ends_with("/is_revoked"),
        "durable wait must check the session tombstone first: {}",
        requests[0].url
    );
    assert!(
        requests[1].url.contains("/LashDurableWaitWorkflow/")
            && requests[1].url.ends_with("/await_resolution"),
        "durable wait must call await_resolution directly: {}",
        requests[1].url
    );
}

#[derive(Debug)]
pub(super) struct AwaitEventCancellationTransport {
    requests: Mutex<Vec<HttpRequest>>,
    resolve_outcome: ResolveOutcome,
}

#[async_trait::async_trait]
impl HttpTransport for AwaitEventCancellationTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let url = request.url.clone();
        self.requests.lock_recover().push(request);
        let body = if url.ends_with("/is_revoked") {
            "false".to_string()
        } else if url.ends_with("/await_resolution") {
            return std::future::pending().await;
        } else if url.ends_with("/resolve") {
            serde_json::to_string(&self.resolve_outcome).expect("encode resolve outcome")
        } else {
            return Err(LlmTransportError::new(format!(
                "unexpected await-event cancellation request: {url}"
            )));
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(body),
        })
    }
}

#[tokio::test]
pub(super) async fn restate_effect_host_cancellation_records_and_returns_the_durable_winner() {
    let earlier = Resolution::Ok(serde_json::json!({ "winner": "earlier" }));
    for (resolve_outcome, expected) in [
        (ResolveOutcome::Accepted, Resolution::Cancelled),
        (
            ResolveOutcome::AlreadyResolved {
                terminal: earlier.clone(),
            },
            earlier,
        ),
    ] {
        let transport = Arc::new(AwaitEventCancellationTransport {
            requests: Mutex::new(Vec::new()),
            resolve_outcome,
        });
        let host = RestateEffectHost::new_for_test(RestateConnection::with_transport(
            "https://restate.example",
            transport.clone(),
        ));
        let key = test_restate_await_event_key(
            &durable_turn_scope("cancel-session", "cancel-turn"),
            AwaitEventWaitIdentity::Custom {
                key: "cancel-wait".to_string(),
            },
        )
        .expect("cancel wait key");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let resolution = host
            .await_await_event(&key, cancel, None)
            .await
            .expect("settle cancelled wait through ingress");

        assert_eq!(resolution, expected);
        let requests = transport.requests.lock_recover();
        let resolve = requests
            .iter()
            .find(|request| request.url.ends_with("/resolve"))
            .expect("cancellation must resolve through the durable index");
        let request: RestateDurableWaitResolveRequest =
            serde_json::from_slice(&resolve.body).expect("decode cancellation resolve request");
        assert_eq!(request.key, key);
        assert_eq!(request.resolution, Resolution::Cancelled);
    }
}

pub(super) struct PostCommitFailingQueuedWorkRunHandle {
    attempts: AtomicUsize,
    recovered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash_core::facade_support::QueuedWorkRunHandle for PostCommitFailingQueuedWorkRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash_core::facade_support::QueuedWorkRunRequest,
    ) -> Result<(), lash_core::facade_support::QueuedWorkRunError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_core::facade_support::QueuedWorkRunError::transient(
                PluginError::Session(
                    "FIG-430 deterministic post-commit dispatch failure".to_string(),
                ),
            ));
        }
        self.recovered.notify_one();
        Ok(())
    }
}

/// FIG-430: durable acceptance is final once the pending-input row commits.
/// Dispatch failure is operational telemetry, and the wake retries itself
/// without waiting for another enqueue or unrelated host event.
#[tokio::test]
pub(super) async fn restate_enqueue_never_errors_after_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-enqueue-post-commit-error";
    let provider = lash_core::testing::TestProvider::builder()
        .kind("fig-430-stub")
        .complete(|_| async { Ok(lash_core::LlmResponse::default()) })
        .build()
        .into_handle();
    let queued_work = Arc::new(PostCommitFailingQueuedWorkRunHandle {
        attempts: AtomicUsize::new(0),
        recovered: tokio::sync::Notify::new(),
    });
    let recovered = queued_work.recovered.notified();
    // A Restate backend whose engine-backed queued driver is the failing
    // run handle: the enqueue commits, then the driver's wake fails.
    let backend = Arc::new(crate::RestateBackend::new(
        "http://127.0.0.1:8080",
        crate::RestateAuthorityId::new("lash-restate-fig430").expect("valid authority"),
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(dir.path().join("sessions"))
                .await
                .expect("open FIG-430 store set"),
        ),
        crate::RestateQueuedWork::Engine(Arc::new(lash_core::NativeQueuedWork::new(
            queued_work.clone(),
        ))),
    ));
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(lash_core::ModelSpec::new(
            "fig-430-model",
            std::num::NonZeroUsize::new(1024).expect("non-zero context window"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-restate-fig430-test",
            "lash-restate-fig430-test-boot",
        ))
        .expect("build FIG-430 core");
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open FIG-430 session");

    let outcome = session
        .durable()
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), recovered)
        .await
        .expect("the failed post-commit wake must retry on its own");
    let persisted = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("inspect committed pending input");

    match (&outcome, persisted.as_slice()) {
        (Err(_), []) => {}
        (Ok(receipt), [stored]) => {
            assert_eq!(stored.input.input_id, receipt.input_id);
            assert_eq!(stored.input.session_id, receipt.session_id);
            assert_eq!(stored.input.source_key, receipt.source_key);
            assert_eq!(stored.input.ingress(), receipt.ingress);
            assert_eq!(receipt.source_key.as_deref(), Some("host:fig-430-retry"));
        }
        (Err(error), stored) => panic!(
            "enqueue returned an undifferentiated error after durable commit: \
             caller_outcome={error:?}, persisted_row_count={}",
            stored.len()
        ),
        (Ok(receipt), stored) => panic!(
            "successful enqueue must identify exactly one durable row: \
             caller_outcome={receipt:?}, persisted_rows={stored:?}"
        ),
    }
    assert_eq!(
        queued_work.attempts.load(Ordering::SeqCst),
        2,
        "the wake path retries exactly once after the injected failure"
    );

    let retry_receipt = session
        .durable()
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await
        .expect("retry the same durable source identity");
    assert_eq!(
        Some(&retry_receipt.input_id),
        outcome.as_ref().ok().map(|receipt| &receipt.input_id),
        "the source key is the idempotent retry identity"
    );
    assert_eq!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("inspect idempotent retry")
            .len(),
        1,
        "an exact source retry must not create another durable input"
    );
}

pub(super) fn replay_test_policy(session_id: &SessionId) -> lash_core::SessionPolicy {
    let mut policy = lash_core::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from(session_id.to_string()));
    policy
}

pub(super) fn replay_test_state(
    session_id: &SessionId,
    policy: &lash_core::SessionPolicy,
) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

pub(super) fn replay_test_input(turn_id: &TurnId) -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("finish once");
    input.trace_turn_id = Some(TurnId::from(turn_id.to_string()));
    input
}

pub(super) async fn replay_test_runtime(
    session_id: &SessionId,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
) -> lash_core::facade_support::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins(
        session_id,
        policy,
        initial_state,
        host,
        store,
        lash_core::testing::test_standard_protocol_factories(),
    ))
    .await
}

pub(super) fn bind_restate_test_effect_host(
    host: &mut lash_core::facade_support::RuntimeHostConfig,
    context: &Arc<ReplayableRecordingContext>,
) {
    host.control.effect_host = Arc::new(RestateRuntimeEffectController::new_for_test(Arc::clone(
        context,
    )));
}

pub(super) async fn replay_test_runtime_with_plugins(
    session_id: &SessionId,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> lash_core::facade_support::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins_and_registry(
        session_id,
        policy,
        initial_state,
        host,
        store,
        plugin_factories,
        None,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn replay_test_runtime_with_plugins_and_registry(
    session_id: &SessionId,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
) -> lash_core::facade_support::LashRuntime {
    let mut builder = lash_core::facade_support::LashRuntime::builder(
        host,
        lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-replay-test",
            "lash-restate-replay-test-boot",
        ),
    )
    .with_session_id(session_id)
    .with_policy(policy)
    .with_initial_state(initial_state)
    .with_plugin_factories(plugin_factories)
    .with_store(store);
    if let Some(process_registry) = process_registry {
        let watched = lash_core::facade_support::watch_process_registry(process_registry);
        let process_registry = Arc::clone(watched.registry());
        builder = builder
            .with_process_work(lash_core::ProcessWorkWiring::new(
                watched,
                Arc::new(lash_core::NativeProcessWork::for_registry(process_registry)),
            ))
            .with_queued_work(Arc::new(lash_core::NoQueuedWork::new()));
    }
    Box::pin(builder.build())
        .await
        .expect("build replay test runtime")
}

pub(super) async fn run_restate_replay_turn(
    runtime: &mut lash_core::facade_support::LashRuntime,
    context: Arc<ReplayableRecordingContext>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> lash_core::facade_support::AssembledTurn {
    let controller = RestateRuntimeEffectController::new_for_test(context);
    let scoped_effect_controller = controller
        .scoped_effect_controller(durable_admission(&durable_turn_scope(session_id, turn_id)))
        .expect("scoped restate controller");
    runtime
        .stream_turn(
            replay_test_input(turn_id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await
        .expect("run replay test turn")
}

/// A process body parked on `waitSignal` observes no turn: its wait races the
/// segment's durable cancel promise, and the recorded race is the only thing
/// that ends it (FIG-3673). The execution's lent stop is not a drive input, so
/// firing it leaves the wait parked; committing the cancel ends it cancelled.
#[tokio::test]
pub(super) async fn a_process_parked_on_a_signal_is_cancelled_by_its_durable_race() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let authority = RestateAuthorityId::new("process-signal-owner").expect("valid test authority");
    let key = restate_await_event_key_for_authority(
        &authority,
        &ExecutionScope::process("worker"),
        AwaitEventWaitIdentity::process_signal(lash_core::ProcessId::from("worker"), "go", 1),
    )
    .expect("process signal wait key");
    let lent_stop = tokio_util::sync::CancellationToken::new();
    let task_stop = lent_stop.clone();
    let task_context = context.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::with_options(
            task_context,
            authority,
            RestateEffectControllerOptions::default().process_segment_drive(),
        )
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::AwaitEvent, "process-signal-wait"),
                RuntimeEffectCommand::AwaitEvent { key },
            ),
            RuntimeEffectLocalExecutor::await_event_under(
                &lash_core::TurnCancelWait::unobserved(task_stop),
                None,
                Arc::new(lash_core::facade_support::SystemClock),
            ),
        )
        .await
    });
    lent_stop.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    if wait.is_finished() {
        panic!(
            "the lent stop must not end a recorded wait: {:?}",
            wait.await.expect("join the signal wait")
        );
    }
    context.commit_process_cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the cancel promise ends the parked signal wait promptly")
        .expect("join the signal wait")
        .expect("signal wait outcome");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));
    assert_eq!(context.process_cancel_race_verdicts(), vec![true]);
}
