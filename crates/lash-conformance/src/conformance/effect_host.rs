//! [`EffectHost`] scope-factory and effect-controller replay conformance.

use super::*;
use crate::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use pretty_assertions::assert_eq;

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
        _session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        Ok(())
    }

    async fn cancel_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl EffectHost for RecordingEffectHost {
    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

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
    effect_host_await_events_with_active_wait_witness(
        make,
        effect_host_await_event_when_quiescent_waits_for_live_waits,
    )
    .await;
}

/// Run the generic AwaitEvent conformance suite with an implementation-owned
/// witness for the active-wait quiescence law.
///
/// Durable engine hosts use this form when starting an ingress task does not
/// itself prove that the remote wait registration committed. The witness must
/// establish that registration through the implementation's real await path
/// before asserting the shared retirement behavior.
pub async fn effect_host_await_events_with_active_wait_witness<F, W, WFut>(make: F, witness: W)
where
    F: Fn() -> Arc<dyn EffectHost>,
    W: FnOnce(Arc<dyn EffectHost>) -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "effect_host_await_events");
    drop((first, second));
    effect_host_local_turn_control_resolves_on_minting_host(make()).await;
    effect_host_await_event_key_is_stable(make()).await;
    effect_host_await_event_accepts_early_resolution(make()).await;
    effect_host_await_event_duplicate_resolution_is_terminal(make()).await;
    effect_host_await_event_cancel_and_timeout_are_terminal(make()).await;
    effect_host_await_event_revokes_session_scope(make()).await;
    effect_host_await_event_retires_non_session_scopes(make()).await;
    effect_host_await_event_reinstate_lifts_process_scope_fence(make()).await;
    witness(make()).await;
    effect_host_when_quiescent_waits_for_executing_effects(make()).await;
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

/// How a substrate treats completed effects when an invocation is redriven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConformanceEffectRedrive {
    /// The successor reads completed effects from the engine journal.
    ReplaysJournal,
    /// An uncommitted effect is executed again by the successor invocation.
    ReexecutesUncommitted,
}

/// One live engine invocation used by controller conformance contracts.
///
/// Redrive consumes the current invocation, runs its explicit end control, and
/// returns a controller bound to the successor invocation. There is no ended
/// state that can still expose a controller.
pub struct ConformanceInvocation {
    controller: Arc<dyn RuntimeEffectController>,
    effect_redrive: ConformanceEffectRedrive,
    end: Arc<dyn Fn() + Send + Sync>,
    redrive: Arc<dyn Fn() -> Arc<dyn RuntimeEffectController> + Send + Sync>,
}

impl ConformanceInvocation {
    /// Build an invocation from its scoped controller and lifecycle controls.
    pub fn new(
        controller: Arc<dyn RuntimeEffectController>,
        effect_redrive: ConformanceEffectRedrive,
        end: impl Fn() + Send + Sync + 'static,
        redrive: impl Fn() -> Arc<dyn RuntimeEffectController> + Send + Sync + 'static,
    ) -> Self {
        Self {
            controller,
            effect_redrive,
            end: Arc::new(end),
            redrive: Arc::new(redrive),
        }
    }

    /// Borrow the controller bound to the live invocation.
    pub fn controller(&self) -> &dyn RuntimeEffectController {
        self.controller.as_ref()
    }

    /// Clone the live controller handle for an in-flight task.
    pub fn controller_handle(&self) -> Arc<dyn RuntimeEffectController> {
        Arc::clone(&self.controller)
    }

    /// Describe what a successor does with a completed pre-crash effect.
    pub fn effect_redrive(&self) -> ConformanceEffectRedrive {
        self.effect_redrive
    }

    /// Construct an invocation for the receipt-less native controller.
    pub fn native() -> Self {
        Self::new(
            Arc::new(crate::NativeRuntimeEffectController::default()),
            ConformanceEffectRedrive::ReexecutesUncommitted,
            || {},
            || Arc::new(crate::NativeRuntimeEffectController::default()),
        )
    }

    #[must_use]
    /// End this invocation and construct its successor.
    pub fn redrive(self) -> Self {
        (self.end)();
        let controller = (self.redrive)();
        Self {
            controller,
            effect_redrive: self.effect_redrive,
            end: self.end,
            redrive: self.redrive,
        }
    }

    /// End the live invocation without constructing a successor.
    pub fn end(self) {
        (self.end)();
    }
}

/// Run journaled-effect replay checks across an explicitly scoped invocation.
pub async fn effect_controller_journaled_effect_replay<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let controller = invocation.controller();
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
        crate::RuntimeErrorCode::from_wire_code("journaled_effect_replay_error")
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

    let invocation = invocation.redrive();
    let controller = invocation.controller();
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
        crate::RuntimeErrorCode::from_wire_code("journaled_effect_replay_error")
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
    invocation.end();
}

/// Prove that retiring one session removes every journal scope it owns.
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

/// Prove that a runtime-operation scope's journal retires by exact identity
/// (FIG-2500): the retired operation's rows go, a sibling operation's rows and
/// their replay answers stay, and the retired scope is fenced against a
/// late admission.
pub async fn effect_host_retires_runtime_operation_journal(host: &dyn EffectHost) {
    let suffix = uuid::Uuid::new_v4().simple();
    let retired_id = format!("retired-journal-op-{suffix}");
    let in_flight_id = format!("in-flight-journal-op-{suffix}");
    for (operation_id, effect_id) in [
        (&retired_id, "retired-op-journal"),
        (&in_flight_id, "in-flight-op-journal"),
    ] {
        host.scoped(ExecutionScope::runtime_operation(operation_id.clone()))
            .expect("runtime-operation scope")
            .controller()
            .execute_effect(
                exec_code_conformance_envelope(effect_id, "op-envelope"),
                RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(replay_conformance_exec_outcome(
                        "recorded-before-retirement",
                    ))
                }),
            )
            .await
            .expect("record runtime-operation journal row before retirement");
    }

    let deleted = host
        .retire_effect_journal(crate::EffectJournalRetirement::runtime_operation(
            retired_id.clone(),
        ))
        .await
        .expect("retire runtime-operation effect journal");
    assert_eq!(
        deleted, 1,
        "runtime-operation retirement must delete the exact canonical operation scope"
    );

    let admission = host
        .scoped(ExecutionScope::runtime_operation(retired_id))
        .expect("retired scope still binds a controller")
        .controller()
        .execute_effect(
            exec_code_conformance_envelope("retired-op-journal", "op-envelope"),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(replay_conformance_exec_outcome("never-admitted"))
            }),
        )
        .await
        .expect_err("a retired runtime-operation scope admits nothing");
    assert_eq!(admission.code, crate::RuntimeErrorCode::EffectScopeRetired);

    let replayed = host
        .scoped(ExecutionScope::runtime_operation(in_flight_id))
        .expect("in-flight scope")
        .controller()
        .execute_effect(
            exec_code_conformance_envelope("in-flight-op-journal", "op-envelope"),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(replay_conformance_exec_outcome("executed-again"))
            }),
        )
        .await
        .expect("the in-flight operation still replays");
    let RuntimeEffectOutcome::ExecCode { result } = replayed else {
        panic!("the in-flight operation must return an exec-code outcome");
    };
    assert_eq!(
        result.expect("in-flight exec result").terminal_finish,
        Some(serde_json::json!("recorded-before-retirement")),
        "a sibling operation's journal answers from its recorded row"
    );
}

/// Assert that a durable effect controller surfaces the same structural replay
/// mismatch detail as the shared canonical-envelope validator.
pub async fn effect_controller_replay_mismatch_diagnostics<F>(make: F, mismatch_code: &str)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let controller = invocation.controller();
    let envelope = |tool_name| {
        replay_conformance_tool_attempt_envelope(
            "replay-mismatch-tool-attempt",
            "replay-mismatch-call",
            tool_name,
        )
    };
    controller
        .execute_effect(
            envelope("first_tool"),
            replay_conformance_tool_attempt_recording_executor(
                ReplayConformanceToolAttempt::new(
                    "replay-mismatch-tool-attempt",
                    "replay-mismatch-call",
                    "first_tool",
                ),
                None,
            ),
        )
        .await
        .expect("record mismatch-vector envelope");

    let invocation = invocation.redrive();
    let controller = invocation.controller();
    let error = controller
        .execute_effect(
            envelope("divergent_tool"),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect_err("reusing a replay key with a divergent envelope must fail");
    assert_eq!(error.code.as_str(), mismatch_code);
    assert!(
        error.code.is_replay_mismatch(),
        "{mismatch_code} must retain the shared typed replay-mismatch classification"
    );
    assert_eq!(
        error.summary,
        Some(crate::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 2,
            first_divergent_paths: vec![
                "command.call.tool_id".to_string(),
                "command.call.tool_name".to_string(),
            ],
        }),
        "replay mismatch must surface its divergent structural path"
    );
    assert!(
        error
            .message
            .contains("divergent_paths=[command.call.tool_id, command.call.tool_name]"),
        "replay mismatch message must surface its divergent structural path: {}",
        error.message
    );
    invocation.end();
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
    assert_eq!(scoped_turn.turn_id(), Some(&crate::TurnId::from("turn-1")));
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

pub(super) async fn effect_host_local_turn_control_resolves_on_minting_host(
    host: Arc<dyn EffectHost>,
) {
    let scope = ExecutionScope::turn(
        format!("local-control-{}", uuid::Uuid::new_v4()),
        "local-turn",
    );
    let local = crate::runtime::NativeRuntimeEffectController::default();
    let local_scoped =
        ScopedEffectController::borrowed(&local, scope.clone()).expect("local turn-control scope");
    let binding = host
        .turn_control_binding(&local_scoped)
        .await
        .expect("local turn-control binding");
    let crate::TurnControlBinding::HostOwned { resolver, .. } = binding else {
        panic!("local turn-control binding must be host-owned");
    };
    for identity in [
        AwaitEventWaitIdentity::TurnCancelGate,
        AwaitEventWaitIdentity::TurnTerminal,
    ] {
        let key = resolver
            .await_event_key(&scope, identity)
            .await
            .expect("mint local turn-control key through binding");
        let resolution = Resolution::Ok(serde_json::json!({"control": "ready"}));
        resolver
            .resolve_await_event(&key, resolution.clone())
            .await
            .expect("resolve local turn-control key through minting resolver");
        let result = host
            .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
            .await;
        assert_eq!(
            result.expect("minting host must accept its local turn-control key"),
            resolution
        );
    }
}

#[cfg(test)]
mod local_control_conformance_tests {
    use super::*;

    #[derive(Default)]
    struct ForeignResolverHost {
        host: crate::NativeEffectHost,
        foreign: crate::NativeEffectHost,
    }

    #[async_trait::async_trait]
    impl crate::AwaitEventResolver for ForeignResolverHost {
        async fn await_await_event(
            &self,
            key: &crate::AwaitEventKey,
            cancel: tokio_util::sync::CancellationToken,
            deadline: Option<std::time::Instant>,
        ) -> Result<Resolution, crate::RuntimeError> {
            self.host.await_await_event(key, cancel, deadline).await
        }
    }

    #[async_trait::async_trait]
    impl EffectHost for ForeignResolverHost {
        fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
            &self.foreign
        }

        fn scoped<'run>(
            &'run self,
            scope: ExecutionScope,
        ) -> Result<ScopedEffectController<'run>, crate::RuntimeError> {
            self.host.scoped(scope)
        }
    }

    #[tokio::test]
    #[should_panic(expected = "minting host must accept its local turn-control key")]
    async fn effect_host_conformance_rejects_foreign_turn_control_resolver() {
        effect_host_local_turn_control_resolves_on_minting_host(Arc::new(
            ForeignResolverHost::default(),
        ))
        .await;
    }
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

    host.revoke_await_events_for_session(&SessionId::from("await-event-session-revoke"))
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

/// Process and runtime-operation promises carry no session to revoke; they
/// retire with their scope through the journal lever, and the fence that
/// retirement leaves refuses every later mint, resolve, peek, and await while
/// a sibling scope's terminal keeps answering (FIG-2499). Session scopes are
/// A pruned process id the host registers again starts unfenced: reinstating
/// the process scope lifts the fence its retirement left, so the new
/// incarnation mints and resolves promises, while a session-bearing scope is
/// refused on this lever exactly as on retirement (ADR 0049, FIG-2499).
async fn effect_host_await_event_reinstate_lifts_process_scope_fence(host: Arc<dyn EffectHost>) {
    let suffix = uuid::Uuid::new_v4().simple();
    let process_id = ProcessId::from(format!("await-event-reinstated-process-{suffix}"));
    let scope = ExecutionScope::process(process_id.clone());
    host.await_event_key(
        &scope,
        AwaitEventWaitIdentity::tool_completion("call-before-prune"),
    )
    .await
    .expect("the first incarnation mints");
    host.retire_effect_journal(crate::EffectJournalRetirement::process(process_id.clone()))
        .await
        .expect("the prune retires the process scope");
    let fenced = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-while-fenced"),
        )
        .await
        .expect_err("a pruned process id mints nothing until it is registered again");
    assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");

    host.reinstate_effect_scope(&scope)
        .await
        .expect("registering the id again lifts the fence");
    let key = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-after-reregistration"),
        )
        .await
        .expect("the re-registered incarnation mints");
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("again")))
            .await
            .expect("the re-registered incarnation resolves"),
        ResolveOutcome::Accepted
    );
    assert_eq!(
        host.peek_await_event(&key)
            .await
            .expect("the re-registered incarnation's terminal reads"),
        Some(Resolution::Ok(serde_json::json!("again")))
    );
    host.reinstate_effect_scope(&scope)
        .await
        .expect("reinstating an unfenced scope is idempotent");

    let session_scope =
        durable_turn_scope(format!("await-event-reinstate-session-{suffix}"), "turn-1");
    let refused = host
        .reinstate_effect_scope(&session_scope)
        .await
        .expect_err("session scopes are fenced by revocation, not the scope lever");
    assert_eq!(refused.code.as_str(), "await_event_scope_not_retirable");
}

/// A scope with an actively awaited, unresolved promise is not quiescent:
/// `WhenQuiescent` refuses it with `effect_scope_not_quiescent` and leaves
/// it unfenced, and retires it once the wait has resolved (FIG-2499 fix
/// round 2, ruling 3).
///
/// The law holds the waiter alive on purpose. Memory waits are not durable:
/// a wait whose waiter was dropped before resolution stays a live entry in
/// every durable host's index or rows and refuses retirement there, while
/// the in-process host keeps no record of a dropped waiter and retires the
/// scope. That is the one memory-versus-durable differential in quiescence
/// (ADR 0049), and a law over the dropped case would assert two answers.
pub(crate) async fn effect_host_await_event_when_quiescent_waits_for_live_waits(
    host: Arc<dyn EffectHost>,
) {
    let suffix = uuid::Uuid::new_v4().simple();
    let scope = ExecutionScope::runtime_operation(format!("await-event-live-wait-{suffix}"));
    let key = host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("call-live"))
        .await
        .expect("the operation mints");
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
    // The spawned waiter parks asynchronously; give it the scheduler before
    // asking for quiescence.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!waiter.is_finished(), "the wait is still open");

    effect_host_registered_wait_rejects_quiescent_retirement(host, scope, key, waiter).await;
}

/// Assert the active-wait retirement law after the caller has witnessed the
/// implementation's genuine wait registration.
///
/// This split lets engine-backed conformance observe its durable registration
/// boundary without replacing the shared refusal, unfenced-state, settlement,
/// and eventual-retirement assertions.
pub async fn effect_host_registered_wait_rejects_quiescent_retirement(
    host: Arc<dyn EffectHost>,
    scope: ExecutionScope,
    key: crate::AwaitEventKey,
    waiter: tokio::task::JoinHandle<Result<Resolution, crate::RuntimeError>>,
) {
    let refused = host
        .retire_effect_journal(
            crate::EffectJournalRetirement::for_scope(&scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect_err("an actively awaited unresolved promise is not quiescent");
    assert_eq!(refused.code.as_str(), "effect_scope_not_quiescent");
    assert_eq!(
        host.peek_await_event(&key)
            .await
            .expect("the refused retirement left the scope unfenced"),
        None
    );

    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("settled")))
            .await
            .expect("resolve the live wait"),
        ResolveOutcome::Accepted
    );
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("the parked wait returns once resolved")
        .expect("waiter task joins")
        .expect("the wait resolves rather than erroring");
    assert_eq!(waited, Resolution::Ok(serde_json::json!("settled")));

    host.retire_effect_journal(
        crate::EffectJournalRetirement::for_scope(&scope)
            .expect("runtime operations are retirable")
            .when_quiescent(),
    )
    .await
    .expect("the scope is quiescent once its wait has resolved");
    let fenced = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("call-after"),
        )
        .await
        .expect_err("the retired scope mints nothing");
    assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");
}

/// A scope with an executing effect is not quiescent: `WhenQuiescent`
/// refuses it with `effect_scope_not_quiescent` while the effect's executor
/// is still running, leaves the scope unfenced, and retires it once the
/// effect has completed (FIG-2499 fix round 3, ruling 4).
///
/// A deployment-level host that runs no local executor of its own — the
/// Restate host outside a handler answers every non-AwaitEvent effect with
/// `restate_effect_host_requires_handler_scope` — cannot be held to this law
/// through this seam; its handler-side controller is held to it by the live
/// end-to-end harness instead.
pub(crate) async fn effect_host_when_quiescent_waits_for_executing_effects(
    host: Arc<dyn EffectHost>,
) {
    let suffix = uuid::Uuid::new_v4().simple();
    let scope = ExecutionScope::runtime_operation(format!("executing-effect-{suffix}"));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let executor_started = Arc::clone(&started);
    let executor_release = Arc::clone(&release);
    let executor = RuntimeEffectLocalExecutor::testing(move |_| {
        let started = Arc::clone(&executor_started);
        let release = Arc::clone(&executor_release);
        async move {
            started.notify_one();
            release.notified().await;
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!("completed"),
            })
        }
    });
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("executing-effect"),
            "work",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("executing-effect-{suffix}"),
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "executing-effect".to_string(),
        },
    );
    let executing_host = Arc::clone(&host);
    let executing_scope = scope.clone();
    let mut executing = crate::task::spawn(async move {
        executing_host
            .scoped(executing_scope)
            .expect("the operation scope binds")
            .controller()
            .execute_effect(envelope, executor)
            .await
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), started.notified());
    tokio::pin!(started);
    tokio::select! {
        _ = &mut started => {}
        outcome = &mut executing => {
            let outcome = outcome.expect("the effect task joins");
            match outcome {
                Err(error)
                    if error.code == lash_core::RuntimeErrorCode::RestateEffectHostRequiresHandlerScope =>
                {
                    // Not a local executor host: see the doc comment.
                    return;
                }
                other => panic!("the effect neither started nor was refused as handler-only: {other:?}"),
            }
        }
    }

    let refused = host
        .retire_effect_journal(
            crate::EffectJournalRetirement::for_scope(&scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect_err("an executing effect is not quiescent");
    assert_eq!(refused.code.as_str(), "effect_scope_not_quiescent");
    host.await_event_key(
        &scope,
        AwaitEventWaitIdentity::tool_completion("still-open"),
    )
    .await
    .expect("the refused retirement left the scope unfenced");

    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), executing)
        .await
        .expect("the released effect completes")
        .expect("the effect task joins")
        .expect("the effect completes");

    host.retire_effect_journal(
        crate::EffectJournalRetirement::for_scope(&scope)
            .expect("runtime operations are retirable")
            .when_quiescent(),
    )
    .await
    .expect("the scope is quiescent once its effect has completed");
    let fenced = host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("after-retirement"),
        )
        .await
        .expect_err("the retired scope mints nothing");
    assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");
}

/// refused by the scope lever: their promises die with their session.
async fn effect_host_await_event_retires_non_session_scopes(host: Arc<dyn EffectHost>) {
    let suffix = uuid::Uuid::new_v4().simple();
    let retired_op = format!("await-event-retired-op-{suffix}");
    let retired_scope = ExecutionScope::runtime_operation(retired_op.clone());
    let survivor_process = format!("await-event-surviving-process-{suffix}");
    let survivor_scope = ExecutionScope::process(survivor_process.clone());
    let retired_key = host
        .await_event_key(
            &retired_scope,
            AwaitEventWaitIdentity::tool_completion("call-retired"),
        )
        .await
        .expect("runtime-operation key");
    let survivor_key = host
        .await_event_key(
            &survivor_scope,
            AwaitEventWaitIdentity::tool_completion("call-survivor"),
        )
        .await
        .expect("process key");
    assert_eq!(
        host.resolve_await_event(&survivor_key, Resolution::Ok(serde_json::json!("kept")))
            .await
            .expect("resolve the surviving process key"),
        ResolveOutcome::Accepted
    );

    host.retire_effect_journal(crate::EffectJournalRetirement::runtime_operation(
        retired_op.clone(),
    ))
    .await
    .expect("retire the runtime-operation scope");

    assert_eq!(
        host.resolve_await_event(&retired_key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("resolve retired key"),
        ResolveOutcome::UnknownOrRevoked
    );
    let err = host
        .peek_await_event(&retired_key)
        .await
        .expect_err("retired key must not peek");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
    let err = host
        .await_await_event(
            &retired_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("retired key must not await");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
    let mint_err = host
        .await_event_key(
            &retired_scope,
            AwaitEventWaitIdentity::tool_completion("call-after-retirement"),
        )
        .await
        .expect_err("a retired scope mints nothing");
    assert_eq!(mint_err.code.as_str(), "await_event_unknown_or_revoked");

    assert_eq!(
        host.peek_await_event(&survivor_key)
            .await
            .expect("the surviving process terminal still reads"),
        Some(Resolution::Ok(serde_json::json!("kept"))),
        "retiring one scope must not touch a sibling scope's terminal"
    );

    host.retire_effect_journal(crate::EffectJournalRetirement::process(survivor_process))
        .await
        .expect("retire the process scope through the same lever");
    assert_eq!(
        host.resolve_await_event(&survivor_key, Resolution::Cancelled)
            .await
            .expect("resolve retired process key"),
        ResolveOutcome::UnknownOrRevoked
    );

    let session_scope = durable_turn_scope(
        format!("await-event-scope-lever-session-{suffix}"),
        "turn-scope-lever",
    );
    let err = host
        .retire_await_events_for_scope(&session_scope)
        .await
        .expect_err("session scopes retire through their session, never the scope lever");
    assert_eq!(err.code.as_str(), "await_event_scope_not_retirable");
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
            host.cancel_await_events_for_session(&SessionId::from(
                "await-event-session-cancel-waits",
            ))
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
pub async fn effect_controller_concurrent_replay_deterministic<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let controller = invocation.controller();
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

    let invocation = invocation.redrive();
    let controller = invocation.controller();
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
    invocation.end();
}

/// Run the tool-attempt replay conformance case for a handler-scoped durable
/// controller.
///
/// Store-backed controllers that support overlapping effect calls record two
/// child attempts concurrently and replay them in reverse order. Ordered
/// workflow-context controllers record them sequentially, then still replay in
/// reverse order. In both modes, outcomes must resolve by stable `replay.key`
/// rather than request position, completion order, or source order.
pub async fn effect_controller_tool_attempt_fanout_replay_deterministic<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let controller = invocation.controller();
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

    let invocation = invocation.redrive();
    let controller = invocation.controller();
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
    invocation.end();
}

pub(super) fn exec_code_conformance_envelope(effect_id: &str, code: &str) -> RuntimeEffectEnvelope {
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
pub struct LeaseFencingController {
    pub controller: Arc<dyn RuntimeEffectController>,
    pub start_replay: Box<dyn Fn() + Send + Sync>,
}

/// A raw mutation applied to the effect-replay row for a given `replay_key`.
/// Backends implement it with a direct row update (SQLite `rusqlite`, Postgres
/// `sqlx`); it is async so Postgres can issue a pooled query.
pub type EffectLeaseMutator = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// Factory that builds a fresh lease-fencing controller bound to one shared
/// durable store with the requested lease TTL. Async so Postgres backends can
/// issue pooled queries during setup.
pub type EffectLeaseControllerFactory = Box<
    dyn Fn(
            std::time::Duration,
            Arc<dyn crate::Clock>,
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
pub struct EffectLeaseFencingBackend {
    pub make_controller: EffectLeaseControllerFactory,
    pub steal_lease: EffectLeaseMutator,
    pub expire_lease: EffectLeaseMutator,
}

/// Clock whose host timestamp and sleep completions are advanced independently.
///
/// The renewal conformance case uses the separate controls to cross the first
/// claim's original expiry on shared-clock stores only after observing a
/// completed renewal cycle. Server-clock stores retain their authoritative
/// lease instant while using the same explicit renewal and backoff gates.
#[derive(Debug)]
struct LeaseFencingClock {
    timestamp_ms: std::sync::atomic::AtomicU64,
    sleep_started: tokio::sync::Semaphore,
    release_sleep: tokio::sync::Semaphore,
}

impl LeaseFencingClock {
    fn new(timestamp_ms: u64) -> Self {
        Self {
            timestamp_ms: std::sync::atomic::AtomicU64::new(timestamp_ms),
            sleep_started: tokio::sync::Semaphore::new(0),
            release_sleep: tokio::sync::Semaphore::new(0),
        }
    }

    fn advance(&self, duration: std::time::Duration) {
        self.timestamp_ms.fetch_add(
            duration.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    async fn await_sleep_started(&self) {
        self.sleep_started
            .acquire()
            .await
            .expect("lease-fencing clock remains open")
            .forget();
    }

    fn release_one_sleep(&self) {
        self.release_sleep.add_permits(1);
    }
}

#[async_trait::async_trait]
impl crate::Clock for LeaseFencingClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.timestamp_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, _duration: std::time::Duration) {
        self.sleep_started.add_permits(1);
        self.release_sleep
            .acquire()
            .await
            .expect("lease-fencing clock remains open")
            .forget();
    }

    async fn sleep_until(&self, _deadline: std::time::Instant) {
        self.sleep(std::time::Duration::ZERO).await;
    }
}

#[test]
fn lease_fencing_clock_wall_clock_faces_agree() {
    let clock = LeaseFencingClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

fn lease_fencing_system_clock() -> Arc<dyn crate::Clock> {
    Arc::new(crate::facade_support::SystemClock)
}

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
pub async fn effect_controller_lease_fencing(backend: EffectLeaseFencingBackend) {
    let run = uuid::Uuid::new_v4().to_string();
    lease_fencing_renews_long_running_lease(&backend, &run).await;
    lease_fencing_reports_lease_lost_when_stolen(&backend, &run).await;
    lease_fencing_rejects_finalize_after_expiry(&backend, &run).await;
    lease_fencing_reclaims_explicitly_expired_lease(&backend, &run).await;
}

async fn lease_fencing_renews_long_running_lease(backend: &EffectLeaseFencingBackend, run: &str) {
    let ttl = std::time::Duration::from_millis(300);
    let renew_interval = ttl / 3;
    let replay_key = format!("lease-renewal-{run}");
    let initial_timestamp = crate::ClockWallTime::timestamp_ms(&crate::facade_support::SystemClock);
    let clock = Arc::new(LeaseFencingClock::new(initial_timestamp));
    let first = (backend.make_controller)(ttl, clock.clone()).await;
    let second = (backend.make_controller)(ttl, clock.clone()).await;
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

    // Deliberately complete one renewal cycle, then move the injected timestamp
    // to the original claim's expiry. Shared-clock stores cross that expiry;
    // server-clock stores keep their authoritative lease time. The second
    // observed sleep is the renewal loop re-arming only after the backend
    // accepted and persisted the renewal.
    clock.await_sleep_started().await;
    clock.advance(renew_interval);
    clock.release_one_sleep();
    clock.await_sleep_started().await;
    clock.advance(ttl - renew_interval);

    // A busy observation enters the injected backoff gate. If the renewal did
    // not preserve the lease, the competing executor enters instead and trips
    // the same property assertion as the original wall-clock-raced case.
    let (competing_entered_tx, competing_entered_rx) = tokio::sync::oneshot::channel();
    let competing_controller = Arc::clone(&second.controller);
    let competing_envelope = envelope.clone();
    let competing_task = crate::task::spawn(async move {
        competing_controller
            .execute_effect(
                competing_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = competing_entered_tx.send(());
                    Ok(replay_conformance_exec_outcome("stolen-owner"))
                }),
            )
            .await
    });
    tokio::select! {
        () = clock.await_sleep_started() => {}
        entered = competing_entered_rx => {
            entered.expect("competing executor admission signal");
            panic!("renewed in-progress lease should keep a competing claimant busy");
        }
    }
    competing_task.abort();
    assert!(
        competing_task.await.is_err(),
        "busy competing claimant task aborts"
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

async fn lease_fencing_reports_lease_lost_when_stolen(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    let ttl = std::time::Duration::from_millis(300);
    let replay_key = format!("lease-stolen-{run}");
    let controller = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
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

async fn lease_fencing_rejects_finalize_after_expiry(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // A long TTL keeps the renewal task idle during the brief block so the
    // finalize path (not renewal) is the one that observes the expired lease.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-expiry-{run}");
    let controller = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
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
async fn lease_fencing_reclaims_explicitly_expired_lease(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // Keep the successor's lease independent of scheduler timing. The test
    // expires the predecessor through the backend affordance below; a short
    // real TTL would only race successor renewal/finalization under load.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-explicit-reclaim-{run}");
    let vanished = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let successor = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
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

const REPLAY_CONFORMANCE_DEADLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy)]
struct ReplayConformanceToolAttempt {
    effect_id: &'static str,
    call_id: &'static str,
    tool_name: &'static str,
}

impl ReplayConformanceToolAttempt {
    const fn new(effect_id: &'static str, call_id: &'static str, tool_name: &'static str) -> Self {
        Self {
            effect_id,
            call_id,
            tool_name,
        }
    }
}

struct ReplayConformanceProbe {
    entered: tokio::sync::mpsc::UnboundedSender<&'static str>,
    release: Arc<tokio::sync::Notify>,
    completion_order: Arc<Mutex<Vec<String>>>,
}

/// Run a slow request before a fast request, prove both local executors are
/// entered before either may finish, then observe the fast controller call
/// finish recording before allowing the slow executor to complete.
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
                    session_id: SessionId::from("replay-session"),
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

pub(super) fn replay_conformance_exec_outcome(effect_id: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(crate::ExecResponse {
            observations: Vec::new(),
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            degraded_bindings: Vec::new(),
            terminal_finish: Some(serde_json::json!(effect_id)),
        })),
    }
}

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
