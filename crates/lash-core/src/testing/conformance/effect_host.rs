//! [`EffectHost`] scope-factory and effect-controller replay conformance.

use super::*;
use crate::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::sync::MutexExt;

/// One scope selected by an [`EffectHost`] and one effect envelope executed
/// through the scoped controller.
#[derive(Clone, Debug)]
pub struct RecordingEffectHostRecord {
    pub runtime_scope: RuntimeScope,
    pub execution_scope: ExecutionScope,
    pub effect_id: String,
    pub effect_kind: RuntimeEffectKind,
    pub replay_key: Option<String>,
    pub envelope_hash: String,
}

#[derive(Clone)]
struct RecordingEffectHostController {
    execution_scope: ExecutionScope,
    records: Arc<Mutex<Vec<RecordingEffectHostRecord>>>,
}

impl crate::AwaitEventResolver for RecordingEffectHostController {}

#[async_trait::async_trait]
impl RuntimeEffectController for RecordingEffectHostController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let envelope_hash = envelope.stable_hash()?;
        self.records.lock_recover().push(RecordingEffectHostRecord {
            runtime_scope: envelope.invocation.scope.clone(),
            execution_scope: self.execution_scope.clone(),
            effect_id: envelope
                .invocation
                .effect_id()
                .expect("effect invocation")
                .to_string(),
            effect_kind: envelope.command.kind(),
            replay_key: envelope.invocation.replay_key().map(ToOwned::to_owned),
            envelope_hash,
        });
        match envelope.command {
            RuntimeEffectCommand::Sleep { .. } => Ok(RuntimeEffectOutcome::Sleep),
            command => Err(RuntimeEffectControllerError::foreign(
                "recording_effect_host_unsupported_command",
                format!(
                    "recording effect host cannot synthesize {} outcomes",
                    command.kind().as_str()
                ),
            )),
        }
    }
}

/// Test fixture that records every selected [`ExecutionScope`] and every effect
/// envelope executed through the returned scoped controller.
#[derive(Clone, Default)]
pub struct RecordingEffectHost {
    selected_scopes: Arc<Mutex<Vec<ExecutionScope>>>,
    records: Arc<Mutex<Vec<RecordingEffectHostRecord>>>,
    retirements: Arc<Mutex<Vec<crate::EffectJournalRetirement>>>,
}

impl RecordingEffectHost {
    pub fn selected_scopes(&self) -> Vec<ExecutionScope> {
        self.selected_scopes.lock_recover().clone()
    }

    pub fn records(&self) -> Vec<RecordingEffectHostRecord> {
        self.records.lock_recover().clone()
    }

    pub fn retirements(&self) -> Vec<crate::EffectJournalRetirement> {
        self.retirements.lock_recover().clone()
    }

    fn scoped_for<'run>(
        &self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, crate::RuntimeError> {
        self.selected_scopes.lock_recover().push(scope.clone());
        ScopedEffectController::shared(
            Arc::new(RecordingEffectHostController {
                execution_scope: scope.clone(),
                records: Arc::clone(&self.records),
            }),
            scope,
        )
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for RecordingEffectHost {
    async fn revoke_await_events_for_session(
        &self,
        _session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        Ok(())
    }

    async fn cancel_await_events_for_session(
        &self,
        _session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl EffectHost for RecordingEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, crate::RuntimeError> {
        self.scoped_for(scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, crate::RuntimeError> {
        Ok(Some(self.scoped_for(scope)?))
    }

    async fn retire_effect_journal(
        &self,
        retirement: crate::EffectJournalRetirement,
    ) -> Result<usize, crate::RuntimeError> {
        self.retirements.lock_recover().push(retirement);
        Ok(0)
    }
}

/// Run the generic [`EffectHost`] scope-factory conformance suite.
///
/// This suite checks the deployment-level contract: execution scopes must carry
/// stable semantic identity, empty ids must fail loudly, and hosts that expose
/// a static scoped controller must preserve the same scope metadata. It does
/// not assert durability; that remains a property of each implementation.
/// Substrate-native hosts such as Restate complete the in-flight contract at
/// this effect-host/controller boundary; [`RuntimePersistence`] remains the
/// committed-state store contract, not a workflow-history contract.
pub async fn effect_host<F>(make: F)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "effect_host");
    drop((first, second));
    effect_host_preserves_scope_metadata(make()).await;
    effect_host_rejects_missing_scope_ids(make()).await;
    effect_host_static_scope_preserves_metadata_when_available(make()).await;
}

/// Run the generic AwaitEvent conformance suite for hosts that implement the
/// external completion primitive.
///
/// This is intentionally separate from [`effect_host`]: deployment-level hosts
/// may be valid scope factories while requiring an external workflow/object
/// context before an AwaitEvent can be awaited.
pub async fn effect_host_await_events<F>(make: F)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "effect_host_await_events");
    drop((first, second));
    effect_host_await_event_key_is_stable(make()).await;
    effect_host_await_event_accepts_early_resolution(make()).await;
    effect_host_await_event_duplicate_resolution_is_terminal(make()).await;
    effect_host_await_event_cancel_and_timeout_are_terminal(make()).await;
    effect_host_await_event_revokes_session_scope(make()).await;
    effect_host_await_event_session_cancel_resolves_outstanding_waits(make()).await;
    effect_host_await_event_rejects_tampered_keys(make()).await;
}

/// Exercise the controller seam with a deterministic segmentation cadence.
///
/// Effect controllers do not own process handover persistence, so the shared
/// conformance harness can only prove the part of the segmentation contract at
/// this seam: cadence-independent result/effect identity, deterministic cuts,
/// and one logical successor and terminal in the scripted lineage. Store and
/// engine suites extend this vector with crash/restart and durable-await
/// assertions.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_segmentation_vector(controller: &dyn RuntimeEffectController) {
    static VECTOR_RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct FixedCadenceController<'a> {
        inner: &'a dyn RuntimeEffectController,
        cadence: u64,
    }

    impl crate::AwaitEventResolver for FixedCadenceController<'_> {}

    #[async_trait::async_trait]
    impl RuntimeEffectController for FixedCadenceController<'_> {
        fn wants_segment_boundary(
            &self,
            progress: &crate::SegmentProgress,
        ) -> Option<crate::BoundaryReason> {
            (progress.effects_executed >= self.cadence)
                .then_some(crate::BoundaryReason::JournalBudget)
        }

        fn supports_concurrent_effects(&self) -> bool {
            self.inner.supports_concurrent_effects()
        }

        async fn execute_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'_>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            self.inner.execute_effect(envelope, local_executor).await
        }
    }

    async fn run_script(
        controller: &dyn RuntimeEffectController,
        id_prefix: &str,
        honor_boundaries: bool,
    ) -> (Vec<serde_json::Value>, Vec<u64>, usize) {
        let local_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut effects = Vec::new();
        let mut successors = Vec::new();
        let mut progress = crate::SegmentProgress::default();
        for ordinal in 0_u64..7 {
            let calls = Arc::clone(&local_calls);
            let input = serde_json::json!({ "iteration": ordinal, "accumulator": ordinal * 3 });
            let outcome = controller
                .execute_effect(
                    exec_code_conformance_envelope(
                        &format!("{id_prefix}-{ordinal}"),
                        &input.to_string(),
                    ),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(replay_conformance_exec_outcome(&input.to_string()))
                    }),
                )
                .await
                .expect("segmentation vector journaled effect");
            let RuntimeEffectOutcome::ExecCode { result } = outcome else {
                panic!("segmentation vector must return exec-code outcome");
            };
            effects.push(
                result
                    .expect("segmentation exec result")
                    .terminal_finish
                    .expect("segmentation exec marker"),
            );
            progress.effects_executed += 1;
            if honor_boundaries
                && ordinal + 1 < 7
                && controller.wants_segment_boundary(&progress).is_some()
            {
                successors.push(successors.len() as u64 + 1);
                progress = crate::SegmentProgress::default();
            }
        }
        (
            effects,
            successors,
            local_calls.load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    let cadence = FixedCadenceController {
        inner: controller,
        cadence: 2,
    };
    let run = VECTOR_RUN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (baseline_effects, baseline_successors, baseline_calls) =
        run_script(controller, &format!("segment-vector-baseline-{run}"), false).await;
    let (segmented_effects, segmented_successors, segmented_calls) =
        run_script(&cadence, &format!("segment-vector-segmented-{run}"), true).await;

    assert_eq!(segmented_effects, baseline_effects, "segment invariance");
    assert!(baseline_successors.is_empty());
    assert_eq!(segmented_successors, vec![1, 2, 3]);
    assert_eq!(baseline_calls, 7, "each baseline effect executes once");
    assert_eq!(segmented_calls, 7, "each segmented effect executes once");
    assert_eq!(
        segmented_successors
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        segmented_successors.len(),
        "each segment ordinal has exactly one successor"
    );
}

/// Run journaled-effect replay checks for controllers with an explicit replay mode.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_journaled_effect_replay(
    controller: &dyn RuntimeEffectController,
    start_replay: impl FnOnce(),
) {
    effect_controller_segmentation_vector(controller).await;
    let success = replay_conformance_tool_attempt_envelope(
        "replay-success",
        "call-replay-success",
        "replay_success_tool",
    );
    let error = replay_conformance_tool_attempt_envelope(
        "replay-error",
        "call-replay-error",
        "replay_error_tool",
    );
    let trigger = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("replay-session"),
            "replay-trigger-list",
            RuntimeEffectKind::Trigger,
            "replay-trigger-list",
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(crate::TriggerCommand::List {
                owner_scope: crate::TriggerOwnerScope::session("replay-session"),
                filter: crate::TriggerSubscriptionFilter::default(),
            }),
        },
    );
    let owner_scope = crate::TriggerOwnerScope::session("replay-session");
    let actor = crate::ProcessOriginator::session(crate::SessionScope::new("replay-session"));
    let draft = crate::TriggerSubscriptionDraft::for_process(
        "replay-key",
        crate::ProcessExecutionEnvRef::new("process-env:replay"),
        "test.source",
        "source-key",
        crate::ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({}),
        },
        crate::ProcessIdentity::new("test"),
    );
    let trigger_mutations = [
        (
            "register",
            crate::TriggerCommand::Register {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                draft: draft.clone(),
            },
        ),
        (
            "update",
            crate::TriggerCommand::Update {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                subscription_key: "replay-key".to_string(),
                draft,
                expected_revision: 1,
            },
        ),
        (
            "enable",
            crate::TriggerCommand::Enable {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                subscription_key: "replay-key".to_string(),
                expected_revision: 1,
            },
        ),
        (
            "disable",
            crate::TriggerCommand::Disable {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                subscription_key: "replay-key".to_string(),
                expected_revision: 1,
            },
        ),
        (
            "delete",
            crate::TriggerCommand::Delete {
                owner_scope,
                actor,
                subscription_key: "replay-key".to_string(),
                expected_revision: 1,
            },
        ),
    ]
    .map(|(operation, command)| {
        let effect_id = format!("replay-trigger-{operation}");
        (
            operation,
            RuntimeEffectEnvelope::new(
                RuntimeInvocation::effect(
                    RuntimeScope::new("replay-session"),
                    effect_id.clone(),
                    RuntimeEffectKind::Trigger,
                    effect_id,
                ),
                RuntimeEffectCommand::Trigger {
                    command: Box::new(command),
                },
            ),
        )
    });

    let first_success = controller
        .execute_effect(
            success.clone(),
            replay_conformance_tool_attempt_recording_executor(
                ReplayConformanceToolAttempt::new(
                    "replay-success",
                    "call-replay-success",
                    "replay_success_tool",
                ),
                None,
            ),
        )
        .await
        .expect("first journaled-effect success");
    assert_replay_conformance_tool_attempt_marker(
        first_success,
        "call-replay-success",
        "replay_success_tool",
    );
    let first_error = controller
        .execute_effect(
            error.clone(),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Err(RuntimeEffectControllerError::foreign(
                    "journaled_effect_replay_error",
                    "recorded journaled-effect error",
                ))
            }),
        )
        .await
        .expect_err("first journaled-effect error");
    assert_eq!(
        first_error.code,
        crate::RuntimeErrorCode::ForeignCode("journaled_effect_replay_error".to_string())
    );
    let first_trigger = controller
        .execute_effect(
            trigger.clone(),
            RuntimeEffectLocalExecutor::testing(|envelope| async move {
                assert!(matches!(
                    envelope.command,
                    RuntimeEffectCommand::Trigger { .. }
                ));
                Ok(RuntimeEffectOutcome::Trigger {
                    result: Box::new(Ok(crate::TriggerCommandOutcome::List {
                        records: Vec::new(),
                    })),
                })
            }),
        )
        .await
        .expect("first typed trigger effect");
    let mut first_trigger_mutations = Vec::new();
    for (operation, envelope) in &trigger_mutations {
        let operation = (*operation).to_string();
        first_trigger_mutations.push(
            controller
                .execute_effect(
                    envelope.clone(),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        Ok(RuntimeEffectOutcome::Trigger {
                            result: Box::new(Err(crate::TriggerOperationError::Invalid {
                                message: format!("recorded {operation} outcome"),
                            })),
                        })
                    }),
                )
                .await
                .expect("first typed trigger mutation effect"),
        );
    }

    start_replay();
    let local_calls = Arc::new(Mutex::new(Vec::new()));
    let replay_success = controller
        .execute_effect(
            success,
            replay_conformance_failing_executor(Arc::clone(&local_calls)),
        )
        .await
        .expect("replayed journaled-effect success");
    assert_replay_conformance_tool_attempt_marker(
        replay_success,
        "call-replay-success",
        "replay_success_tool",
    );
    let replay_error = controller
        .execute_effect(
            error,
            replay_conformance_failing_executor(Arc::clone(&local_calls)),
        )
        .await
        .expect_err("replayed journaled-effect error");
    assert_eq!(
        replay_error.code,
        crate::RuntimeErrorCode::ForeignCode("journaled_effect_replay_error".to_string())
    );
    let replay_trigger = controller
        .execute_effect(
            trigger,
            replay_conformance_failing_executor(Arc::clone(&local_calls)),
        )
        .await
        .expect("replayed typed trigger effect");
    assert_eq!(
        serde_json::to_value(replay_trigger).expect("serialize replayed trigger outcome"),
        serde_json::to_value(first_trigger).expect("serialize original trigger outcome")
    );
    for ((_, envelope), first_outcome) in trigger_mutations.into_iter().zip(first_trigger_mutations)
    {
        let replayed = controller
            .execute_effect(
                envelope,
                replay_conformance_failing_executor(Arc::clone(&local_calls)),
            )
            .await
            .expect("replayed typed trigger mutation effect");
        assert_eq!(
            serde_json::to_value(replayed).expect("serialize replayed trigger mutation outcome"),
            serde_json::to_value(first_outcome)
                .expect("serialize original trigger mutation outcome")
        );
    }
    assert!(
        local_calls.lock_recover().is_empty(),
        "journaled-effect replay must not invoke local closures"
    );
}

/// Prove that retiring one session removes every journal scope it owns.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_host_retires_session_journal(host: &dyn EffectHost) {
    let session_id = "retired-journal-session";
    let scopes = [
        ExecutionScope::turn(session_id, "retired-turn"),
        ExecutionScope::queue_drain(session_id, "retired-drain"),
        ExecutionScope::session_delete(session_id),
    ];

    for (ordinal, scope) in scopes.into_iter().enumerate() {
        let controller = host.scoped(scope).expect("retired journal scope");
        let envelope = exec_code_conformance_envelope(
            &format!("retired-journal-{ordinal}"),
            "retired-journal-envelope",
        );
        controller
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(replay_conformance_exec_outcome(
                        "recorded-before-retirement",
                    ))
                }),
            )
            .await
            .expect("record journal row before retirement");
    }

    let deleted = host
        .retire_effect_journal(crate::EffectJournalRetirement::session(session_id))
        .await
        .expect("retire session effect journal");
    assert_eq!(
        deleted, 3,
        "retirement must delete every effect row for the session"
    );
}

/// Prove that terminal-process retention can retire the exact process journal
/// without parsing or prefix-matching its canonical key.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_host_retires_process_journal(host: &dyn EffectHost) {
    let process_id = "retired-journal-process";
    let controller = host
        .scoped(ExecutionScope::process(process_id))
        .expect("retired process scope");
    controller
        .controller()
        .execute_effect(
            exec_code_conformance_envelope("retired-process-journal", "retired-process-envelope"),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(replay_conformance_exec_outcome(
                    "recorded-before-retirement",
                ))
            }),
        )
        .await
        .expect("record process journal row before retirement");

    let deleted = host
        .retire_effect_journal(crate::EffectJournalRetirement::process(process_id))
        .await
        .expect("retire process effect journal");
    assert_eq!(
        deleted, 1,
        "process retirement must delete the exact canonical process scope"
    );
}

/// Assert that a durable effect controller surfaces the same structural replay
/// mismatch detail as the shared canonical-envelope validator.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_replay_mismatch_diagnostics(
    controller: &dyn RuntimeEffectController,
    mismatch_code: &str,
) {
    let envelope = |duration_ms| {
        RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::for_turn("replay-mismatch-session", "replay-mismatch-turn", 0, 0),
                "replay-mismatch-sleep",
                RuntimeEffectKind::Sleep,
                "replay-mismatch-sleep",
            ),
            RuntimeEffectCommand::Sleep { duration_ms },
        )
    };
    controller
        .execute_effect(
            envelope(0),
            RuntimeEffectLocalExecutor::testing(|_| async { Ok(RuntimeEffectOutcome::Sleep) }),
        )
        .await
        .expect("record mismatch-vector envelope");

    let error = controller
        .execute_effect(envelope(1), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect_err("reusing a replay key with a divergent envelope must fail");
    assert_eq!(error.code.as_str(), mismatch_code);
    assert!(
        error.code.is_replay_mismatch(),
        "{mismatch_code} must retain the shared typed replay-mismatch classification"
    );
    assert_eq!(
        error.summary,
        Some(crate::RuntimeEffectReplayMismatchSummary {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.duration_ms".to_string()],
        }),
        "replay mismatch must surface its divergent structural path"
    );
    assert!(
        error
            .message
            .contains("divergent_paths=[command.duration_ms]"),
        "replay mismatch message must surface its divergent structural path: {}",
        error.message
    );
}

async fn effect_host_preserves_scope_metadata(host: Arc<dyn EffectHost>) {
    let scope = ExecutionScope::queue_drain("session-1", "drain-1");
    let scoped = host.scoped(scope.clone()).expect("queue drain scope");
    assert_eq!(
        scoped.execution_scope(),
        &scope,
        "scoped controller must retain the selected semantic scope"
    );
    assert_eq!(scoped.scope_id(), "drain-1");
    assert_eq!(scoped.turn_id(), None);

    let turn_scope = durable_turn_scope("session-1", "turn-1");
    let scoped_turn = host.scoped(turn_scope.clone()).expect("turn scope");
    assert_eq!(scoped_turn.execution_scope(), &turn_scope);
    assert_eq!(scoped_turn.scope_id(), "turn-1");
    assert_eq!(scoped_turn.turn_id(), Some("turn-1"));
}

async fn effect_host_rejects_missing_scope_ids(host: Arc<dyn EffectHost>) {
    let invalid_scopes = [
        ExecutionScope::turn("", "turn"),
        ExecutionScope::turn("session", ""),
        ExecutionScope::process(""),
        ExecutionScope::queue_drain("session", ""),
        ExecutionScope::session_delete(""),
        ExecutionScope::runtime_operation(""),
    ];

    for scope in invalid_scopes {
        let err = match host.scoped(scope) {
            Ok(_) => panic!("invalid execution scope must be rejected"),
            Err(err) => err,
        };
        assert_eq!(
            err.code,
            crate::RuntimeErrorCode::MissingExecutionScopeId,
            "invalid scope ids must fail with the stable missing-scope code"
        );
    }
}

async fn effect_host_static_scope_preserves_metadata_when_available(host: Arc<dyn EffectHost>) {
    let scope = ExecutionScope::runtime_operation("static-runtime-op");
    let Some(scoped) = host
        .scoped_static(scope.clone())
        .expect("static scope factory")
    else {
        return;
    };
    assert_eq!(scoped.execution_scope(), &scope);
    assert_eq!(scoped.scope_id(), "static-runtime-op");
}

async fn effect_host_await_event_key_is_stable(host: Arc<dyn EffectHost>) {
    let scope = durable_turn_scope("await-event-session-stable", "turn-stable");
    let wait = AwaitEventWaitIdentity::tool_completion("call-stable");

    let first = host
        .await_event_key(&scope, wait.clone())
        .await
        .expect("first await-event key");
    let second = host
        .await_event_key(&scope, wait)
        .await
        .expect("second await-event key");

    assert_eq!(first, second);
}

async fn effect_host_await_event_accepts_early_resolution(host: Arc<dyn EffectHost>) {
    let scope = durable_turn_scope("await-event-session-early", "turn-early");
    let key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-early"),
        )
        .await
        .expect("await-event key");
    let resolution = Resolution::Ok(serde_json::json!({ "ready": true }));

    assert_eq!(
        host.resolve_await_event(&key, resolution.clone())
            .await
            .expect("early resolve"),
        ResolveOutcome::Accepted
    );
    let awaited = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("await early-resolved event");
    assert_eq!(awaited, resolution);
}

async fn effect_host_await_event_duplicate_resolution_is_terminal(host: Arc<dyn EffectHost>) {
    let scope = durable_turn_scope("await-event-session-dupe", "turn-dupe");
    let key = host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("call-dupe"))
        .await
        .expect("await-event key");
    let resolution = Resolution::Ok(serde_json::json!("first"));

    let first = host
        .resolve_await_event(&key, resolution.clone())
        .await
        .expect("first resolve");
    let second = host
        .resolve_await_event(&key, Resolution::Ok(serde_json::json!("second")))
        .await
        .expect("duplicate resolve");

    assert_eq!(first, ResolveOutcome::Accepted);
    assert_eq!(
        second,
        ResolveOutcome::AlreadyResolved {
            terminal: resolution
        }
    );
}

async fn effect_host_await_event_cancel_and_timeout_are_terminal(host: Arc<dyn EffectHost>) {
    let cancel_scope = durable_turn_scope("await-event-session-cancel", "turn-cancel");
    let cancel_key = host
        .await_event_key(
            &cancel_scope,
            AwaitEventWaitIdentity::tool_completion("call-cancel"),
        )
        .await
        .expect("cancel await-event key");
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let cancelled = host
        .await_await_event(&cancel_key, cancel, None)
        .await
        .expect("cancelled await-event");
    assert_eq!(cancelled, Resolution::Cancelled);
    assert_eq!(
        host.resolve_await_event(&cancel_key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late cancel resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Cancelled
        }
    );

    let timeout_scope = durable_turn_scope("await-event-session-timeout", "turn-timeout");
    let timeout_key = host
        .await_event_key(
            &timeout_scope,
            AwaitEventWaitIdentity::tool_completion("call-timeout"),
        )
        .await
        .expect("timeout await-event key");
    let timed_out = host
        .await_await_event(
            &timeout_key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now()),
        )
        .await
        .expect("timed-out await-event");
    assert_eq!(timed_out, Resolution::Timeout);
}

async fn effect_host_await_event_revokes_session_scope(host: Arc<dyn EffectHost>) {
    let scope = durable_turn_scope("await-event-session-revoke", "turn-revoke");
    let key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-revoke"),
        )
        .await
        .expect("await-event key");

    host.revoke_await_events_for_session("await-event-session-revoke")
        .await
        .expect("revoke session");

    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("resolve revoked key"),
        ResolveOutcome::UnknownOrRevoked
    );
    let err = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect_err("revoked key must not await");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
}

/// The standalone wait-revocation lever: cancelling a session's durable waits
/// resolves every *outstanding* wait with [`Resolution::Cancelled`] (waiters
/// never hang; late resolves observe the terminal) while leaving the session
/// usable — new waits registered afterwards resolve normally, unlike the
/// tombstoning session revocation exercised above.
async fn effect_host_await_event_session_cancel_resolves_outstanding_waits(
    host: Arc<dyn EffectHost>,
) {
    let scope = durable_turn_scope("await-event-session-cancel-waits", "turn-cancel-waits");
    let key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-cancel-waits"),
        )
        .await
        .expect("await-event key");

    let waiter_host = Arc::clone(&host);
    let waiter_key = key.clone();
    let waiter = crate::task::spawn(async move {
        waiter_host
            .await_await_event(
                &waiter_key,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });
    // The spawned waiter registers its wait asynchronously; cancel repeatedly
    // (the lever is idempotent) until the waiter observes a terminal.
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            host.cancel_await_events_for_session("await-event-session-cancel-waits")
                .await
                .expect("cancel session waits");
            if waiter.is_finished() {
                return waiter.await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("outstanding wait must terminate after session cancel")
    .expect("waiter task joins")
    .expect("cancelled wait resolves rather than erroring");
    assert_eq!(
        waited,
        Resolution::Cancelled,
        "outstanding waits must resolve with the Cancelled terminal"
    );
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve after cancel"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Cancelled
        }
    );

    // The session is NOT tombstoned: a wait registered after the cancel still
    // resolves normally.
    let later_key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-after-cancel"),
        )
        .await
        .expect("post-cancel await-event key");
    assert_eq!(
        host.resolve_await_event(&later_key, Resolution::Ok(serde_json::json!("still-works")))
            .await
            .expect("post-cancel resolve"),
        ResolveOutcome::Accepted
    );
    assert_eq!(
        host.await_await_event(&later_key, tokio_util::sync::CancellationToken::new(), None)
            .await
            .expect("post-cancel wait resolves"),
        Resolution::Ok(serde_json::json!("still-works"))
    );
}

async fn effect_host_await_event_rejects_tampered_keys(host: Arc<dyn EffectHost>) {
    let scope = durable_turn_scope("await-event-session-tamper", "turn-tamper");
    let mut key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-tamper"),
        )
        .await
        .expect("await-event key");
    key.signature.push_str("-tampered");

    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("bad")))
            .await
            .expect("resolve tampered key"),
        ResolveOutcome::UnknownOrRevoked
    );
    let err = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect_err("tampered key must not await");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
}

/// Run the concurrent recorded-effect replay conformance case for a
/// handler-scoped durable controller.
///
/// The first pass starts two recorded effects concurrently and intentionally
/// lets the second finish before the first. After `start_replay`, the same
/// effects are requested in the opposite order with local executors that fail
/// if called. A compliant controller returns the recorded outcomes by
/// `replay.key`, independent of local completion/request ordering.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_concurrent_replay_deterministic(
    controller: &dyn RuntimeEffectController,
    start_replay: impl FnOnce(),
) {
    let slow = replay_conformance_tool_attempt_envelope("effect-slow", "call-slow", "slow_tool");
    let fast = replay_conformance_tool_attempt_envelope("effect-fast", "call-fast", "fast_tool");
    let first_pass = replay_conformance_concurrent_first_pass(
        controller,
        slow.clone(),
        ReplayConformanceToolAttempt::new("effect-slow", "call-slow", "slow_tool"),
        fast.clone(),
        ReplayConformanceToolAttempt::new("effect-fast", "call-fast", "fast_tool"),
    )
    .await;
    let slow_first = first_pass.0.expect("slow first pass");
    let fast_first = first_pass.1.expect("fast first pass");
    assert_replay_conformance_tool_attempt_marker(slow_first, "call-slow", "slow_tool");
    assert_replay_conformance_tool_attempt_marker(fast_first, "call-fast", "fast_tool");

    start_replay();
    let replay_local_calls = Arc::new(Mutex::new(Vec::new()));
    let replay_pass = tokio::time::timeout(REPLAY_CONFORMANCE_DEADLOCK_TIMEOUT, async {
        tokio::join!(
            controller.execute_effect(
                fast,
                replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
            ),
            controller.execute_effect(
                slow,
                replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
            ),
        )
    })
    .await
    .expect("concurrent replay effects must resolve from host history");
    let fast_replay = replay_pass.0.expect("fast replay");
    let slow_replay = replay_pass.1.expect("slow replay");
    assert_replay_conformance_tool_attempt_marker(fast_replay, "call-fast", "fast_tool");
    assert_replay_conformance_tool_attempt_marker(slow_replay, "call-slow", "slow_tool");
    assert!(
        replay_local_calls.lock_recover().is_empty(),
        "replay must return recorded outcomes without invoking local executors"
    );
}

/// Run the tool-attempt replay conformance case for a handler-scoped durable
/// controller.
///
/// Store-backed controllers that support overlapping effect calls record two
/// child attempts concurrently and replay them in reverse order. Ordered
/// workflow-context controllers record them sequentially, then still replay in
/// reverse order. In both modes, outcomes must resolve by stable `replay.key`
/// rather than request position, completion order, or source order.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_tool_attempt_fanout_replay_deterministic(
    controller: &dyn RuntimeEffectController,
    start_replay: impl FnOnce(),
) {
    let slow =
        replay_conformance_tool_attempt_envelope("tool-attempt-slow", "call-slow", "slow_tool");
    let fast =
        replay_conformance_tool_attempt_envelope("tool-attempt-fast", "call-fast", "fast_tool");

    let first_pass = if controller.supports_concurrent_effects() {
        replay_conformance_concurrent_first_pass(
            controller,
            slow.clone(),
            ReplayConformanceToolAttempt::new("tool-attempt-slow", "call-slow", "slow_tool"),
            fast.clone(),
            ReplayConformanceToolAttempt::new("tool-attempt-fast", "call-fast", "fast_tool"),
        )
        .await
    } else {
        let slow_first = controller
            .execute_effect(
                slow.clone(),
                replay_conformance_tool_attempt_recording_executor(
                    ReplayConformanceToolAttempt::new(
                        "tool-attempt-slow",
                        "call-slow",
                        "slow_tool",
                    ),
                    None,
                ),
            )
            .await;
        let fast_first = controller
            .execute_effect(
                fast.clone(),
                replay_conformance_tool_attempt_recording_executor(
                    ReplayConformanceToolAttempt::new(
                        "tool-attempt-fast",
                        "call-fast",
                        "fast_tool",
                    ),
                    None,
                ),
            )
            .await;
        (slow_first, fast_first)
    };

    let slow_first = first_pass.0.expect("slow tool-attempt first pass");
    let fast_first = first_pass.1.expect("fast tool-attempt first pass");
    assert_replay_conformance_tool_attempt_marker(slow_first, "call-slow", "slow_tool");
    assert_replay_conformance_tool_attempt_marker(fast_first, "call-fast", "fast_tool");

    start_replay();
    let replay_local_calls = Arc::new(Mutex::new(Vec::new()));
    let replay_pass = if controller.supports_concurrent_effects() {
        tokio::time::timeout(REPLAY_CONFORMANCE_DEADLOCK_TIMEOUT, async {
            tokio::join!(
                controller.execute_effect(
                    fast,
                    replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
                ),
                controller.execute_effect(
                    slow,
                    replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
                ),
            )
        })
        .await
        .expect("concurrent tool-attempt replay must resolve from host history")
    } else {
        let fast_replay = controller
            .execute_effect(
                fast,
                replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
            )
            .await;
        let slow_replay = controller
            .execute_effect(
                slow,
                replay_conformance_failing_executor(Arc::clone(&replay_local_calls)),
            )
            .await;
        (fast_replay, slow_replay)
    };
    let fast_replay = replay_pass.0.expect("fast tool-attempt replay");
    let slow_replay = replay_pass.1.expect("slow tool-attempt replay");
    assert_replay_conformance_tool_attempt_marker(fast_replay, "call-fast", "fast_tool");
    assert_replay_conformance_tool_attempt_marker(slow_replay, "call-slow", "slow_tool");
    assert!(
        replay_local_calls.lock_recover().is_empty(),
        "tool-attempt replay must return recorded outcomes without invoking local executors"
    );
}

#[cfg(any(test, feature = "testing"))]
fn exec_code_conformance_envelope(effect_id: &str, code: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("journaled-session", "journaled-turn", 7, 0),
            format!("exec-code:{effect_id}"),
            RuntimeEffectKind::ExecCode,
            format!("exec-code-replay:{effect_id}"),
        ),
        RuntimeEffectCommand::ExecCode {
            language: "conformance".to_string(),
            code: code.to_string(),
        },
    )
}

/// One controller bound to a shared durable effect-replay store, paired with a
/// `start_replay` toggle. The toggle exists because `start_replay` is a concrete
/// controller affordance, not a [`RuntimeEffectController`] trait method.
#[cfg(any(test, feature = "testing"))]
pub struct LeaseFencingController {
    pub controller: Arc<dyn RuntimeEffectController>,
    pub start_replay: Box<dyn Fn() + Send + Sync>,
}

/// A raw mutation applied to the effect-replay row for a given `replay_key`.
/// Backends implement it with a direct row update (SQLite `rusqlite`, Postgres
/// `sqlx`); it is async so Postgres can issue a pooled query.
#[cfg(any(test, feature = "testing"))]
pub type EffectLeaseMutator = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// Factory that builds a fresh lease-fencing controller bound to one shared
/// durable store with the requested lease TTL. Async so Postgres backends can
/// issue pooled queries during setup.
#[cfg(any(test, feature = "testing"))]
pub type EffectLeaseControllerFactory = Box<
    dyn Fn(
            std::time::Duration,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = LeaseFencingController> + Send>>
        + Send
        + Sync,
>;

/// Backend adapter for the effect-replay lease-fencing conformance suite.
///
/// `make_controller` returns a fresh controller bound to one shared durable
/// store with the requested lease TTL. `steal_lease` overwrites the lease
/// owner/token for a `replay_key` (another worker reclaimed the row);
/// `expire_lease` forces the lease already-expired. Both mutate the same store
/// the controllers share.
#[cfg(any(test, feature = "testing"))]
pub struct EffectLeaseFencingBackend {
    pub make_controller: EffectLeaseControllerFactory,
    pub steal_lease: EffectLeaseMutator,
    pub expire_lease: EffectLeaseMutator,
}

#[cfg(any(test, feature = "testing"))]
fn lease_fencing_envelope(replay_key: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("effect-lease-session", "effect-lease-turn", 1, 0),
            replay_key,
            RuntimeEffectKind::ExecCode,
            replay_key,
        ),
        RuntimeEffectCommand::ExecCode {
            language: "code".to_string(),
            code: "emit".to_string(),
        },
    )
}

/// Run the durable effect-replay lease-fencing conformance suite. Every durable
/// effect-replay controller (SQLite, Postgres, ...) must satisfy the same
/// fencing contract, so the row-level renewal/steal/expiry behavior lives here
/// once instead of in store-specific raw-row tests:
///
/// - a renewed in-progress lease keeps a competing claimant out, then replays;
/// - a stolen lease aborts the original owner with a lease-lost error;
/// - a lease that expires before finalize is rejected with a lease-lost error;
/// - a successor reclaims and executes an effect after its predecessor's lease
///   is explicitly expired.
#[cfg(any(test, feature = "testing"))]
pub async fn effect_controller_lease_fencing(backend: EffectLeaseFencingBackend) {
    let run = uuid::Uuid::new_v4().to_string();
    lease_fencing_renews_long_running_lease(&backend, &run).await;
    lease_fencing_reports_lease_lost_when_stolen(&backend, &run).await;
    lease_fencing_rejects_finalize_after_expiry(&backend, &run).await;
    lease_fencing_reclaims_explicitly_expired_lease(&backend, &run).await;
}

#[cfg(any(test, feature = "testing"))]
async fn lease_fencing_renews_long_running_lease(backend: &EffectLeaseFencingBackend, run: &str) {
    // Generous TTL: the renewal task must outlive scheduler starvation when
    // the whole workspace's test binaries run in parallel; a tight TTL makes
    // this case flake under load without exercising anything extra.
    let ttl = std::time::Duration::from_millis(300);
    let replay_key = format!("lease-renewal-{run}");
    let first = (backend.make_controller)(ttl).await;
    let second = (backend.make_controller)(ttl).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let first_controller = Arc::clone(&first.controller);
    let first_envelope = envelope.clone();
    let first_release = Arc::clone(&release);
    let first_task = crate::task::spawn(async move {
        first_controller
            .execute_effect(
                first_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    first_release.notified().await;
                    Ok(replay_conformance_exec_outcome("renewed-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("first executor entered");

    // Let several lease TTLs lapse; the renewal task must keep the lease alive.
    tokio::time::sleep(ttl * 3).await;
    let competing = tokio::time::timeout(
        ttl * 2,
        second.controller.execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                Ok(replay_conformance_exec_outcome("stolen-owner"))
            }),
        ),
    )
    .await;
    assert!(
        competing.is_err(),
        "renewed in-progress lease should keep a competing claimant busy",
    );

    release.notify_waiters();
    let first_outcome = first_task
        .await
        .expect("first task joins")
        .expect("renewed owner finalizes");
    assert_replay_conformance_exec_marker(first_outcome, "renewed-owner");

    (second.start_replay)();
    let replayed = second
        .controller
        .execute_effect(
            envelope,
            replay_conformance_failing_executor(Arc::new(Mutex::new(Vec::new()))),
        )
        .await
        .expect("replayed renewed outcome");
    assert_replay_conformance_exec_marker(replayed, "renewed-owner");
}

#[cfg(any(test, feature = "testing"))]
async fn lease_fencing_reports_lease_lost_when_stolen(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    let ttl = std::time::Duration::from_millis(300);
    let replay_key = format!("lease-stolen-{run}");
    let controller = (backend.make_controller)(ttl).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let never_release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&controller.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&never_release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("should-not-finalize"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    (backend.steal_lease)(replay_key.clone()).await;

    let err = tokio::time::timeout(std::time::Duration::from_secs(2), owner_task)
        .await
        .expect("renewal should notice the stolen lease")
        .expect("owner task joins")
        .expect_err("stolen lease must fail the original owner");
    assert!(
        err.code.as_str().ends_with("_effect_replay_lease_lost"),
        "expected an effect-replay lease-lost error, got code `{}`: {}",
        err.code,
        err.message,
    );
    let _keep_notify_alive = never_release;
}

#[cfg(any(test, feature = "testing"))]
async fn lease_fencing_rejects_finalize_after_expiry(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // A long TTL keeps the renewal task idle during the brief block so the
    // finalize path (not renewal) is the one that observes the expired lease.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-expiry-{run}");
    let controller = (backend.make_controller)(ttl).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&controller.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("expired-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    (backend.expire_lease)(replay_key.clone()).await;
    release.notify_waiters();

    let err = owner_task
        .await
        .expect("owner task joins")
        .expect_err("expired lease must not finalize");
    assert!(
        err.code.as_str().ends_with("_effect_replay_lease_lost"),
        "expected an effect-replay lease-lost error, got code `{}`: {}",
        err.code,
        err.message,
    );
}

/// A successor must reclaim and execute an effect after its predecessor's
/// lease is explicitly expired.
#[cfg(any(test, feature = "testing"))]
async fn lease_fencing_reclaims_explicitly_expired_lease(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // Keep the successor's lease independent of scheduler timing. The test
    // expires the predecessor through the backend affordance below; a short
    // real TTL would only race successor renewal/finalization under load.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-explicit-reclaim-{run}");
    let vanished = (backend.make_controller)(ttl).await;
    let successor = (backend.make_controller)(ttl).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let never_release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&vanished.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&never_release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("vanished-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("vanished executor entered");
    // Kill the first owner mid-claim so it cannot renew or finalize. The
    // backend affordance then drives expiry as an explicit test event.
    owner_task.abort();
    assert!(owner_task.await.is_err(), "vanished owner task aborts");

    (backend.expire_lease)(replay_key.clone()).await;

    let reclaimed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        successor.controller.execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                Ok(replay_conformance_exec_outcome("successor-owner"))
            }),
        ),
    )
    .await
    .expect("a successor must reclaim the abandoned row after its lease is explicitly expired")
    .expect("successor executes the reclaimed effect");
    assert_replay_conformance_exec_marker(reclaimed, "successor-owner");
    let _keep_notify_alive = never_release;
}

#[cfg(any(test, feature = "testing"))]
fn replay_conformance_tool_attempt_envelope(
    effect_id: &'static str,
    call_id: &'static str,
    tool_name: &'static str,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn(
                "tool-attempt-conformance-session",
                "tool-attempt-conformance-turn",
                7,
                0,
            ),
            effect_id,
            RuntimeEffectKind::ToolAttempt,
            format!("tool-attempt-conformance:tool-attempt-conformance-turn:{effect_id}"),
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: crate::PreparedToolCall::from_parts(
                call_id,
                crate::ToolId::from(format!("tool:{tool_name}")),
                tool_name,
                serde_json::json!({ "call": call_id }),
                None,
                serde_json::json!({ "prepared": effect_id }),
            ),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

#[cfg(any(test, feature = "testing"))]
const REPLAY_CONFORMANCE_DEADLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy)]
struct ReplayConformanceToolAttempt {
    effect_id: &'static str,
    call_id: &'static str,
    tool_name: &'static str,
}

#[cfg(any(test, feature = "testing"))]
impl ReplayConformanceToolAttempt {
    const fn new(effect_id: &'static str, call_id: &'static str, tool_name: &'static str) -> Self {
        Self {
            effect_id,
            call_id,
            tool_name,
        }
    }
}

#[cfg(any(test, feature = "testing"))]
struct ReplayConformanceProbe {
    entered: tokio::sync::mpsc::UnboundedSender<&'static str>,
    release: Arc<tokio::sync::Notify>,
    completion_order: Arc<Mutex<Vec<String>>>,
}

/// Run a slow request before a fast request, prove both local executors are
/// entered before either may finish, then observe the fast controller call
/// finish recording before allowing the slow executor to complete.
#[cfg(any(test, feature = "testing"))]
async fn replay_conformance_concurrent_first_pass(
    controller: &dyn RuntimeEffectController,
    slow_envelope: RuntimeEffectEnvelope,
    slow: ReplayConformanceToolAttempt,
    fast_envelope: RuntimeEffectEnvelope,
    fast: ReplayConformanceToolAttempt,
) -> (
    Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) {
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let start_fast = Arc::new(tokio::sync::Notify::new());
    let release_slow = Arc::new(tokio::sync::Notify::new());
    let release_fast = Arc::new(tokio::sync::Notify::new());
    let fast_recorded = Arc::new(tokio::sync::Notify::new());
    let completion_order = Arc::new(Mutex::new(Vec::new()));

    let slow_call = controller.execute_effect(
        slow_envelope,
        replay_conformance_tool_attempt_recording_executor(
            slow,
            Some(ReplayConformanceProbe {
                entered: entered_tx.clone(),
                release: Arc::clone(&release_slow),
                completion_order: Arc::clone(&completion_order),
            }),
        ),
    );
    let fast_call = {
        let start_fast = Arc::clone(&start_fast);
        let release_fast = Arc::clone(&release_fast);
        let fast_recorded = Arc::clone(&fast_recorded);
        let completion_order = Arc::clone(&completion_order);
        async move {
            start_fast.notified().await;
            let outcome = controller
                .execute_effect(
                    fast_envelope,
                    replay_conformance_tool_attempt_recording_executor(
                        fast,
                        Some(ReplayConformanceProbe {
                            entered: entered_tx,
                            release: Arc::clone(&release_fast),
                            completion_order: Arc::clone(&completion_order),
                        }),
                    ),
                )
                .await;
            fast_recorded.notify_one();
            outcome
        }
    };
    let orchestrate = async {
        assert_eq!(
            entered_rx.recv().await,
            Some(slow.effect_id),
            "the first requested effect must enter its local executor"
        );
        start_fast.notify_one();
        assert_eq!(
            entered_rx.recv().await,
            Some(fast.effect_id),
            "the second effect must enter while the first local executor is still gated"
        );
        release_fast.notify_one();
        fast_recorded.notified().await;
        release_slow.notify_one();
    };

    let (slow_outcome, fast_outcome, ()) =
        tokio::time::timeout(REPLAY_CONFORMANCE_DEADLOCK_TIMEOUT, async {
            tokio::join!(slow_call, fast_call, orchestrate)
        })
        .await
        .expect(
            "concurrent first-pass effects must enter both local executors and record fast first",
        );
    assert_eq!(
        completion_order.lock_recover().as_slice(),
        &[fast.effect_id.to_string(), slow.effect_id.to_string()],
        "first pass must prove local completion order can differ from effect request order"
    );
    (slow_outcome, fast_outcome)
}

#[cfg(any(test, feature = "testing"))]
fn replay_conformance_tool_attempt_recording_executor(
    attempt: ReplayConformanceToolAttempt,
    concurrent_probe: Option<ReplayConformanceProbe>,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |envelope| async move {
        assert_eq!(envelope.invocation.effect_id(), Some(attempt.effect_id));
        if let Some(probe) = concurrent_probe {
            probe
                .entered
                .send(attempt.effect_id)
                .expect("conformance orchestrator must observe executor entry");
            probe.release.notified().await;
            probe
                .completion_order
                .lock_recover()
                .push(attempt.effect_id.to_string());
        }
        Ok(replay_conformance_tool_attempt_outcome(
            attempt.call_id,
            attempt.tool_name,
        ))
    })
}

#[cfg(any(test, feature = "testing"))]
fn replay_conformance_failing_executor(
    replay_local_calls: Arc<Mutex<Vec<String>>>,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |envelope| async move {
        replay_local_calls
            .lock_recover()
            .push(envelope.invocation.effect_id().unwrap_or("").to_string());
        Err(RuntimeEffectControllerError::foreign(
            "conformance_replay_local_executor_called",
            "recorded replay must not invoke local effect execution",
        ))
    })
}

#[cfg(any(test, feature = "testing"))]
fn replay_conformance_tool_attempt_outcome(
    call_id: &'static str,
    tool_name: &'static str,
) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(crate::ToolAttemptLaunch::Done {
            record: Box::new(crate::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: tool_name.to_string(),
                args: serde_json::json!({ "call": call_id }),
                output: crate::ToolCallOutput::success(serde_json::json!({
                    "call": call_id,
                    "tool": tool_name,
                })),
                duration_ms: 0,
            }),
            intents: crate::ToolIntents::v1(vec![crate::ToolIntent::StartProcess(Box::new(
                crate::StartProcessIntent {
                    session_id: "replay-session".to_string(),
                    request: crate::ProcessStartRequest::external(
                        format!("{call_id}:intent-child"),
                        crate::ProcessOriginator::host_scoped("effect-host-conformance"),
                        serde_json::json!({"tool": tool_name}),
                    ),
                    on_parent_end: crate::ProcessParentEndPolicy::Abandon,
                },
            ))]),
        }),
        triggers: Vec::new(),
    }
}

#[cfg(any(test, feature = "testing"))]
fn replay_conformance_exec_outcome(effect_id: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(crate::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            terminal_finish: Some(serde_json::json!(effect_id)),
        })),
    }
}

#[cfg(any(test, feature = "testing"))]
fn assert_replay_conformance_exec_marker(outcome: RuntimeEffectOutcome, expected: &str) {
    let RuntimeEffectOutcome::ExecCode { result } = outcome else {
        panic!("expected exec-code effect outcome");
    };
    let response = result.expect("exec-code response");
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!(expected)),
        "replayed outcome must come from the matching replay key"
    );
}

#[cfg(any(test, feature = "testing"))]
fn assert_replay_conformance_tool_attempt_marker(
    outcome: RuntimeEffectOutcome,
    expected_call_id: &str,
    expected_tool_name: &str,
) {
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected tool-attempt effect outcome");
    };
    let crate::ToolAttemptLaunch::Done { record, .. } = *launch else {
        panic!("expected completed tool-attempt launch");
    };
    assert_eq!(record.call_id.as_deref(), Some(expected_call_id));
    assert_eq!(record.tool, expected_tool_name);
    assert_eq!(
        record.output.value_for_projection(),
        serde_json::json!({
            "call": expected_call_id,
            "tool": expected_tool_name,
        }),
        "replayed tool-attempt outcome must come from the matching replay key"
    );
}
