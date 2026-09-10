use super::*;

#[test]
pub(super) fn missing_segment_handover_distinguishes_superseded_orphan_from_current_input() {
    let latest = lash_core::PersistedSegmentHandover {
        segment_ordinal: 4,
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: "program-v1".to_string(),
            engine_state: vec![4],
        },
    };
    assert!(missing_segment_is_superseded(2, Some(&latest)));
    assert!(!missing_segment_is_superseded(4, Some(&latest)));
    assert!(!missing_segment_is_superseded(5, Some(&latest)));
    assert!(!missing_segment_is_superseded(1, None));
}

#[tokio::test]
pub(super) async fn persisted_handover_is_change_feed_and_event_invariant() {
    let (registry, continuations) = process_stores();
    let _record = registry
        .register_process(rerunnable_registration("segment-invariant"))
        .await
        .expect("register process");
    let (_, cursor) = registry
        .processes_changed_since(lash_core::ProcessChangeCursor::default(), 10)
        .await
        .expect("initial change feed");
    continuations
        .put_segment_handover(
            &ProcessId::from("segment-invariant"),
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::DurationCap,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![9],
                },
            },
        )
        .await
        .expect("persist handover");
    let (changes, next_cursor) = registry
        .processes_changed_since(cursor, 10)
        .await
        .expect("change feed after handover");
    assert!(changes.is_empty());
    assert_eq!(next_cursor, cursor);
    assert!(
        registry
            .events_after(&ProcessId::from("segment-invariant"), 0)
            .await
            .expect("events")
            .is_empty()
    );
}

#[tokio::test]
pub(super) async fn cancel_redrives_successor_engine() {
    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([
            process_success(serde_json::Value::Null).into()
        ])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = rerunnable_registration("cancel-between-segments");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    registry
        .append_event(
            &ProcessId::from("cancel-between-segments"),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from("cancel-between-segments"),
                Some("stop".to_string()),
            ),
        )
        .await
        .expect("cancel between segments");
    let outcome = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            native_process_scope(&ProcessId::from("cancel-between-segments")),
            1,
            Some(lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "program-v1".to_string(),
                engine_state: vec![1],
            }),
            async { Ok(()) },
        )
        .await
        .expect("cancelled successor");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_cancellation(output.as_ref())
    ));
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        1,
        "the cancelled successor must still be driven for replay-consistent command emission"
    );
}

#[test]
pub(super) fn cancel_terminal_from_successor_routes_to_root_await_workflow_key() {
    assert_eq!(
        terminal_completion_workflow_key(&ProcessId::from("process-1"), 0),
        None
    );
    assert_eq!(
        terminal_completion_workflow_key(&ProcessId::from("process-1"), 1),
        Some("process-1".to_string())
    );
    assert_eq!(
        process_segment_workflow_key(&ProcessId::from("process-1"), 1),
        "process-1#1",
        "the running successor key must differ from the terminal await key"
    );
}

#[test]
pub(super) fn runtime_handler_error_classification_keeps_lane_busy_retryable() {
    let error = handler_error_from_plugin(lash_core::PluginError::Runtime(
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
            "child session execution lane is busy",
        ),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Retryable"),
        "lane contention must ask Restate to retry: {debug}"
    );
}

#[test]
pub(super) fn controller_handler_error_classification_keeps_lane_busy_retryable() {
    let error = handler_error_from_plugin(lash_core::PluginError::RuntimeEffectController(
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
            "child session execution lane is busy",
        ),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Retryable"),
        "controller-owned lane contention must ask Restate to retry: {debug}"
    );
}

#[tokio::test]
pub(super) async fn terminal_child_failure_becomes_typed_process_output_for_the_awaiting_parent() {
    let registry = process_registry();
    let registration = rerunnable_registration("terminal-child-failure");
    let record = registry
        .register_process(registration.clone())
        .await
        .expect("register terminal-failure child");
    let parent_context = Arc::new(RecordingContext::default());
    let parent = RestateRuntimeEffectController::new(Arc::clone(&parent_context));
    let parent_wait = parent.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "await-terminal-child-failure"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_ref: lash_core::ProcessRef::from_record(&record),
            }),
        ),
        registry_local_executor(Arc::clone(&registry)),
    );
    tokio::pin!(parent_wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut parent_wait)
            .await
            .is_err(),
        "the parent must be parked before the child publishes its terminal promise"
    );

    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(TerminalFailureRunner),
                Arc::clone(&registry),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let endpoint_output = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        "terminal-child-failure",
        &RestateProcessWorkflowInput {
            registration,
            execution_context: ProcessExecutionContext::default(),
            segment_ordinal: 0,
            execution_id: None,
        },
        true,
    )
    .await
    .expect("terminal child workflow must complete through the Restate endpoint");
    let promise_key =
        restate_process_terminal_await_key(&ProcessId::from("terminal-child-failure"))
            .expect("terminal promise key")
            .promise_key();
    let resolution = restate_completed_promise(&endpoint_output, &promise_key)
        .expect("terminal child workflow must publish its process-terminal promise");
    let serde_json::Value::String(resolution) = resolution else {
        panic!("terminal promise completion must carry its serialized typed resolution")
    };
    parent_context.resolve_process_terminal_resolution(
        &ProcessId::from("terminal-child-failure"),
        serde_json::from_str(&resolution).expect("decode published terminal resolution"),
    );
    let awaited = tokio::time::timeout(Duration::from_secs(1), &mut parent_wait)
        .await
        .expect("the published terminal promise must wake the awaiting parent")
        .expect("the awaiting parent must settle instead of hanging");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Await { output },
    } = awaited
    else {
        panic!("awaiting parent received the wrong process outcome")
    };
    let ProcessAwaitOutput::Settled { output } = output.as_ref() else {
        panic!("awaiting parent must receive the child's settled output")
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("awaiting parent must receive the child's typed failure")
    };
    assert_eq!(failure.code, "restate_service_unregistered");
    assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    assert_eq!(
        registry
            .get_process(&ProcessId::from("terminal-child-failure"))
            .await
            .expect("read terminal child")
            .and_then(|record| record.outcome),
        Some(ProcessAwaitOutput::Settled {
            output: output.clone(),
        })
    );
}

#[tokio::test]
pub(super) async fn worker_replacement_mid_child_aborts_parent_without_terminalizing_rerunnable_child()
 {
    let process_id = "replacement-aborted-child";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register replacement-aborted child");
    let runner = Arc::new(ReplacementThenSuccessRunner {
        runs: AtomicUsize::new(0),
    });
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    );

    let error = workflow
        .run_registration(
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                lash_core::ExecutionScope::process(process_id),
            )
            .expect("replacement-aborted child scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect_err("worker replacement must abort the parent invocation");
    let rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
            .to_string();
    assert!(
        rendered.contains("worker_replacement_abort"),
        "parent abort lost the typed replacement code: {rendered}"
    );
    let interrupted = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read interrupted child")
        .expect("interrupted child remains registered");
    assert!(
        !interrupted.is_terminal() && interrupted.outcome.is_none(),
        "replacement abort must leave the rerunnable child non-terminal: {interrupted:?}"
    );

    let rerun = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                lash_core::ExecutionScope::process(process_id),
            )
            .expect("rerun child scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("the rerunnable child must succeed on a fresh invocation");
    assert!(matches!(
        rerun,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_success(output.as_ref())
    ));
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
pub(super) async fn opaque_process_infrastructure_failure_does_not_become_terminal_process_truth() {
    let process_id = "opaque-infrastructure-failure";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register rerunnable child");
    let runner = Arc::new(OpaqueFailureThenSuccessRunner {
        runs: AtomicUsize::new(0),
    });
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    );

    workflow
        .run_registration(
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                lash_core::ExecutionScope::process(process_id),
            )
            .expect("first child scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect_err("opaque infrastructure failure must abort the invocation");
    let interrupted = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read interrupted child")
        .expect("child remains registered");
    assert!(!interrupted.is_terminal());
    assert!(interrupted.outcome.is_none());

    let rerun = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                lash_core::ExecutionScope::process(process_id),
            )
            .expect("rerun child scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("fresh invocation must be able to rerun the child");
    assert!(matches!(
        rerun,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_success(output.as_ref())
    ));
}

#[test]
pub(super) fn runtime_handler_error_classification_keeps_restate_ingress_retryable() {
    let error = handler_error_from_plugin(lash_core::PluginError::Runtime(
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::RestateProcessIngressSubmit,
            "process workflow ingress is temporarily unavailable",
        ),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Retryable"),
        "typed Restate ingress failures must ask Restate to retry: {debug}"
    );
}

#[test]
pub(super) fn ingress_submit_maps_an_unregistered_service_to_the_terminal_code() {
    let unregistered = crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/run".to_string(),
        status: 404,
        body: "not found".to_string(),
    };
    let lash_core::PluginError::Runtime(error) =
        crate::process::process_ingress_submit_error(&ProcessId::from("proc-1"), unregistered)
    else {
        panic!("ingress submit failures must stay typed runtime errors");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateServiceUnregistered
    );
    assert!(!error.code.is_retryable());

    let transient = crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/run".to_string(),
        status: 503,
        body: "unavailable".to_string(),
    };
    let lash_core::PluginError::Runtime(error) =
        crate::process::process_ingress_submit_error(&ProcessId::from("proc-1"), transient)
    else {
        panic!("ingress submit failures must stay typed runtime errors");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateProcessIngressSubmit
    );
    assert!(error.code.is_retryable());
}

#[test]
pub(super) fn runtime_handler_error_classification_makes_unregistered_service_terminal() {
    // An unregistered service is a deployment fact: retrying cannot make an
    // unbound service appear, so the ingress-submit 404 must not join the
    // retryable ingress class above.
    let error = handler_error_from_plugin(lash_core::PluginError::Runtime(
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::RestateServiceUnregistered,
            "no deployment binds LashProcessWorkflow/run",
        ),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Terminal"),
        "an unregistered-service ingress failure must stop Restate redelivery: {debug}"
    );
}

#[test]
pub(super) fn runtime_handler_error_classification_makes_terminal_runtime_error_terminal() {
    let error = handler_error_from_plugin(lash_core::PluginError::Runtime(
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            "turn exceeded its store node budget",
        ),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Terminal"),
        "terminal runtime errors must stop Restate redelivery: {debug}"
    );
}

#[test]
pub(super) fn boundary_with_armed_wait_is_declined_instead_of_terminalized() {
    let mut record = lash_core::ProcessRecord::from_registration(
        rerunnable_registration("wait"),
        lash_core::ProcessIncarnation::from_registration_sequence(1),
    );
    record.wait = Some(lash_core::WaitState {
        since_ms: 1,
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:wait:signal.ready:1".to_string(),
            ordinal: 1,
        },
    });
    assert!(boundary_must_be_declined(Some(&record)));
    record.wait = None;
    assert!(!boundary_must_be_declined(Some(&record)));
}

#[tokio::test]
pub(super) async fn process_workflow_endpoint_smoke_schedules_runs_and_cancels_process() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                runner.clone(),
                registry.clone(),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context = Arc::new(RecordingContext::with_endpoint(endpoint));
    let host = RestateRuntimeEffectController::new(context.clone());
    let registration = external_registration("task-smoke")
        .with_wake_session_id(Some(SessionId::from("wake-smoke")));
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-smoke"),
    ));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: vec![SessionId::from("session")],
                    env_spec: None,
                    execution_context: Box::new(execution_context),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("start through endpoint smoke");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong start outcome");
    };

    let external_ref = record.external_ref.as_ref().expect("external ref");
    assert_eq!(external_ref.backend, "restate");
    assert_eq!(external_ref.id, "LashProcessWorkflow/task-smoke");
    assert_eq!(
        external_ref
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("invocation-task-smoke"))
    );

    let observed = registry
        .list_observed_by(
            &SessionId::from("session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("session observed");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].id, "task-smoke");
    let observed_external_ref = observed[0].external_ref.as_ref().expect("external ref");
    assert_eq!(observed_external_ref.backend, "restate");
    assert_eq!(observed_external_ref.id, "LashProcessWorkflow/task-smoke");

    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| registration.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-smoke"]
    );
    assert_eq!(
        runner.ran.lock_recover().as_slice(),
        &[RecordedProcessRun {
            process_id: ProcessId::from("task-smoke"),
            wake_target_session_id: Some(SessionId::from("wake-smoke")),
            tool_effect_id: Some("tool-smoke".to_string()),
            execution_scope_id: "task-smoke".to_string(),
            turn_control_participation: lash_core::TurnControlParticipation::DurableJournaled,
        }]
    );

    let process_ref = lash_core::ProcessRef::from_record(&record);

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_ref,
                    reason: Some("stop-smoke".to_string()),
                    replay: None,
                }),
            ),
            registry_local_executor(registry),
        )
        .await
        .expect("cancel through endpoint smoke");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Cancel { .. }
        }
    ));
    assert_eq!(
        context.cancelled.lock_recover().as_slice(),
        &[("task-smoke".to_string(), Some("stop-smoke".to_string()))]
    );
    assert_eq!(
        runner.cancelled.lock_recover().as_slice(),
        &[RestateProcessCancelRequest {
            process_id: ProcessId::from("task-smoke"),
            reason: Some("stop-smoke".to_string()),
        }]
    );
}

pub(super) struct RecoveryProcessTool;

impl RecoveryProcessTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_echo",
            "recovery_echo",
            "Echo a line and emit a durable process wake.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "recovery_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RecoveryProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let _ = call;
        lash_core::ToolOutcome::err_fmt(
            "recovery_echo owes a process.wake emission and runs only on the leaf attempt route",
        )
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(process_id) = call.context.runtime_process_id() else {
            return lash_core::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::from_output(lash_core::ToolCallOutput::failure(
                    lash_core::ToolFailure::runtime(
                        lash_core::ToolFailureClass::Internal,
                        "recovery_echo_outside_process",
                        "recovery_echo runs only inside a durable process",
                    ),
                )),
            );
        };
        // The wake append is journal-capable work: the attempt declares it and
        // the intent executor emits it once the attempt commits.
        let intent = lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
            session_id: SessionId::from(call.context.session_id()),
            process_id: ProcessId::from(process_id.to_string()),
            event_type: "process.wake".to_string(),
            payload: serde_json::json!({ "message": line, "wake_input": line }),
        });
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "echo": line })),
            lash_core::ToolIntents::v1(vec![intent]),
        )
    }
}

pub(super) struct SnapshotRecoveryTool;

impl SnapshotRecoveryTool {
    pub(super) fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:snapshot_echo",
            "snapshot_echo",
            "Echo a line from a snapshot-backed process tool.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "snapshot_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SnapshotRecoveryTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "snapshot_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolOutcome::ok(serde_json::json!({ "echo": format!("snapshot:{line}") }))
    }
}

#[derive(serde::Deserialize)]
pub(super) struct SnapshotRecoveryToolOptions {
    snapshot_ref: String,
}

pub(super) fn snapshot_recovery_tool_options(snapshot_ref: &str) -> lash_core::PluginOptions {
    lash_core::PluginOptions::typed(
        "snapshot-recovery-tool",
        serde_json::json!({ "snapshot_ref": snapshot_ref }),
    )
    .expect("snapshot recovery plugin options")
}

pub(super) fn snapshot_recovery_tool_factory() -> Arc<dyn lash_core::facade_support::PluginFactory>
{
    Arc::new(lash_core::plugin::PluginSpecFactory::new(
        "snapshot-recovery-tool",
        Arc::new(|ctx| {
            let snapshot_available = ctx
                .plugin_options
                .decode::<SnapshotRecoveryToolOptions>("snapshot-recovery-tool")
                .map_err(|err| {
                    lash_core::PluginError::Registration(format!(
                        "invalid snapshot recovery tool options: {err}"
                    ))
                })?
                .is_some_and(|options| options.snapshot_ref == "tool-authority:sha256:ok");
            let spec = if snapshot_available {
                lash_core::facade_support::PluginSpec::new()
                    .with_tool_provider(Arc::new(SnapshotRecoveryTool))
            } else {
                lash_core::facade_support::PluginSpec::new()
            };
            Ok(spec)
        }),
    ))
}

pub(super) fn recovery_worker(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> DurableProcessWorker {
    recovery_worker_with_plugins(registry, store_factory, Vec::new())
}

pub(super) fn recovery_worker_with_plugins(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecoveryProcessTool);
    let mut plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::facade_support::PluginFactory>,
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "recovery-tool",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        )),
    ];
    plugins.extend(extra_plugins);
    let plugin_host = lash_core::facade_support::PluginHost::new(plugins);
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        RECOVERY_PROCESS_ENV_STORE.clone();
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(process_env_store)
    .with_process_engine(Arc::new(
        lash_lashlang_runtime::LashlangProcessEngine::in_memory(
            lash_lashlang_runtime::LashlangSurface::default(),
        ),
    ));
    DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            store_factory,
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(recovery_session_policy()),
    )
    .expect("valid test native substrate config")
}

pub(super) struct ProcessParentIntentTool {
    calls: Arc<AtomicUsize>,
}

impl ProcessParentIntentTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:process_parent_intent",
            "process_parent_intent",
            "Optionally start a child carrying a Cancel-at-parent-end policy.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "emit": { "type": "boolean" },
                    "child": { "type": "string" }
                },
                "required": ["emit", "child"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "process_parent_intent"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProcessParentIntentTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "process_parent_intent").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the process-parent law must use AttemptContext")
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let child = call
            .args
            .get("child")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("missing-child");
        let intents = if call
            .args
            .get("emit")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: SessionId::from(call.context.session_id()),
                    request: lash_core::ProcessStartRequest::external(
                        format!("ignored-{child}"),
                        lash_core::ProcessOriginator::host_scoped("process-parent-law"),
                        serde_json::json!({"process_parent_child": child}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Cancel,
                },
            ))])
        } else {
            lash_core::ToolIntents::default()
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"child": child})),
            intents,
        )
    }
}

pub(super) fn process_parent_intent_plugin(
    calls: Arc<AtomicUsize>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "process-parent-intent",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(ProcessParentIntentTool { calls })),
    ))
}

pub(super) struct PanicOnceAfterDurableProcessTerminal {
    crashes: AtomicUsize,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicOnceAfterDurableProcessTerminal {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "process.parent_end.after_terminal"
            && self.crashes.fetch_add(1, Ordering::SeqCst) == 0
        {
            panic!("injected crash after durable process terminal and before parent teardown");
        }
    }
}

pub(super) fn process_parent_worker(
    registry: Arc<dyn ProcessRegistry>,
    plugin: Arc<dyn lash_core::facade_support::PluginFactory>,
    probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
) -> DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        RECOVERY_PROCESS_ENV_STORE.clone();
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(process_env_store)
    .with_process_engine(Arc::new(
        lash_lashlang_runtime::LashlangProcessEngine::in_memory(
            lash_lashlang_runtime::LashlangSurface::default(),
        ),
    ));
    let plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::facade_support::PluginFactory>,
        plugin,
    ];
    DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(plugins)),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(recovery_session_policy())
        .with_turn_phase_probe_slot(probe_slot),
    )
    .expect("valid test native substrate config")
}

pub(super) async fn process_parent_lashlang_registration(
    process_id: &ProcessId,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          early = await tools.process_parent_intent({ emit: true, child: "segmented" })?
          later = await tools.process_parent_intent({ emit: false, child: "none" })?
          finish later.child
        }
        "#,
    )
    .expect("parse segmented process-parent law");
    let contract = ProcessParentIntentTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "process_parent_intent",
            "tool:process_parent_intent",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("link process-parent law tool");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default().with_processes(),
        ),
    )
    .expect("link segmented process-parent law");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store segmented process-parent artifact");
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("main")
                .expect("main process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("process-parent-law")),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn segmented_child_await_registration(
    process_id: &ProcessId,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process child() {
          finish { from: "child" }
        }

        process main() {
          handle = start child()
          result = (await handle)?
          finish result.from
        }
        "#,
    )
    .expect("parse segmented child-await law");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::default().with_processes(),
        ),
    )
    .expect("link segmented child-await law");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store segmented child-await artifact");
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("main")
                .expect("main process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(
            "segmented-child-await-root",
        )),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
pub(super) async fn lashlang_process_retains_child_possession_across_restate_segments() {
    let (registry, continuations) = process_stores();
    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let workflow = Arc::new(
        LashProcessWorkflowImpl::new_for_test(
            Arc::new(RestateCoreProcessRunner::new(worker.clone())),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        )
        .with_segment_effect_budget_selector(|_| 1),
    );
    let registration = segmented_child_await_registration(
        &ProcessId::from("segmented-child-await-parent"),
        persist_recovery_env_ref().await,
    )
    .await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register segmented child-await parent");

    let mut ordinal = 0_u64;
    let mut input_handover = None;
    let mut boundary_count = 0_usize;
    let mut execution_id = None::<String>;
    let restate_events = Arc::new(RecordingContext::default());
    loop {
        let context = Arc::new(ReplayableRecordingContext {
            events: Arc::clone(&restate_events),
            ..ReplayableRecordingContext::default()
        });
        context.install_process_worker(worker.clone());
        let controller = RestateRuntimeEffectController::with_options(
            Arc::clone(&context),
            RestateEffectControllerOptions::default().segment_effect_budget(1),
        );
        let retained = registry
            .get_process(&registration.id)
            .await
            .expect("read retained child-await parent")
            .expect("segmented child-await parent exists");
        let (current_execution_id, execution_authority) = segment_execution_authority(
            &registration.id,
            ordinal,
            execution_id.as_deref(),
            &format!("segmented-child-await-invocation-{ordinal}"),
            retained.first_started.as_deref(),
        )
        .expect("derive segmented child-await authority");
        execution_id = Some(current_execution_id);
        let outcome = workflow
            .run_registration(
                registration.clone(),
                ProcessExecutionContext::default()
                    .with_execution_write_authority(execution_authority),
                controller
                    .scoped_effect_controller(ExecutionScope::process(&registration.id))
                    .expect("segmented child-await scope"),
                ordinal,
                input_handover.take(),
                pending_process_cancel_signal(),
            )
            .await
            .expect("run segmented child-await process");
        match outcome {
            lash_core::ProcessRunOutcome::SegmentBoundary(boundary) => {
                boundary_count += 1;
                let next = ordinal + 1;
                continuations
                    .put_segment_handover(
                        &registration.id,
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next,
                            handover: boundary,
                        },
                    )
                    .await
                    .expect("store segmented child-await handover");
                input_handover = Some(
                    continuations
                        .get_segment_handover(&registration.id, next)
                        .await
                        .expect("reload segmented child-await handover")
                        .expect("stored segmented child-await handover")
                        .handover,
                );
                ordinal = next;
            }
            lash_core::ProcessRunOutcome::Terminal { output, .. } => {
                assert_eq!(
                    *output,
                    process_success(serde_json::json!("child")),
                    "a resumed Lashlang segment retains possession of children started by this run"
                );
                break;
            }
        }
    }
    assert_eq!(
        boundary_count, 2,
        "start and await each cross an effect-count segment boundary"
    );
}

#[tokio::test]
pub(super) async fn process_parents_teardown_after_durable_end_across_segments_and_tool_call_route()
{
    let (registry, continuations) = process_stores();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let plugin = process_parent_intent_plugin(Arc::clone(&provider_calls));
    let probe_slot = lash_core::runtime::RuntimeTurnPhaseProbeSlot::default();
    lash_core::runtime::RuntimeTurnPhaseProbeSlot::set_for_session(
        &probe_slot,
        "process-parent-law",
        Arc::new(PanicOnceAfterDurableProcessTerminal {
            crashes: AtomicUsize::new(0),
        }),
    );
    let worker = process_parent_worker(Arc::clone(&registry), Arc::clone(&plugin), probe_slot);
    let workflow = Arc::new(
        LashProcessWorkflowImpl::new_for_test(
            Arc::new(RestateCoreProcessRunner::new(worker.clone())),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        )
        .with_segment_effect_budget_selector(|_| 1),
    );
    let env_ref = persist_recovery_env_ref().await;
    let segmented = process_parent_lashlang_registration(
        &ProcessId::from("segmented-process-parent"),
        env_ref.clone(),
    )
    .await;
    registry
        .register_process(segmented.clone())
        .await
        .expect("register segmented process parent");

    let mut ordinal = 0_u64;
    let mut input_handover = None;
    let mut boundary_count = 0_usize;
    let mut execution_id = None::<String>;
    loop {
        let context = Arc::new(ReplayableRecordingContext::default());
        context.defer_process_workflows();
        let context_evidence = Arc::clone(&context);
        let controller = RestateRuntimeEffectController::with_options(
            context,
            RestateEffectControllerOptions::default().segment_effect_budget(1),
        );
        let workflow = Arc::clone(&workflow);
        let registration = segmented.clone();
        let handover = input_handover.take();
        let retained = registry
            .get_process(&ProcessId::from("segmented-process-parent"))
            .await
            .expect("read retained process execution")
            .expect("segmented process exists");
        let (current_execution_id, execution_authority) = segment_execution_authority(
            &ProcessId::from("segmented-process-parent"),
            ordinal,
            execution_id.as_deref(),
            &format!("process-parent-law-invocation-{ordinal}"),
            retained.first_started.as_deref(),
        )
        .expect("derive process-parent invocation authority");
        execution_id = Some(current_execution_id);
        let run = tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_authority),
                    controller
                        .scoped_effect_controller(ExecutionScope::process(
                            "segmented-process-parent",
                        ))
                        .expect("segmented process scope"),
                    ordinal,
                    handover,
                    pending_process_cancel_signal(),
                )
                .await
        })
        .await;
        match run {
            Ok(Ok(lash_core::ProcessRunOutcome::SegmentBoundary(boundary))) => {
                boundary_count += 1;
                let durable_state: serde_json::Value =
                    serde_json::from_slice(&boundary.engine_state)
                        .expect("decode versioned Lashlang handover state");
                let visible_processes = registry
                    .list_processes(&lash_core::ProcessListFilter {
                        status: lash_core::ProcessStatusFilter::Any,
                        ..lash_core::ProcessListFilter::default()
                    })
                    .await
                    .expect("inspect early intent children");
                assert_eq!(
                    durable_state["version"],
                    serde_json::json!(lash_lashlang_runtime::LASHLANG_SEGMENT_STATE_VERSION)
                );
                assert_eq!(
                    durable_state["parent_end_actions"]
                        .as_array()
                        .expect("handover carries parent-end action array")
                        .len(),
                    1,
                    "the early intent survives every segment boundary; provider_calls={}; records={:?}; processes={visible_processes:?}",
                    provider_calls.load(Ordering::SeqCst),
                    context_evidence
                        .records
                        .lock_recover()
                        .values()
                        .filter_map(
                            |bytes| serde_json::from_slice::<RecordedRuntimeEffect>(bytes).ok()
                        )
                        .map(|recorded| recorded.outcome)
                        .collect::<Vec<_>>(),
                );
                let next = ordinal + 1;
                continuations
                    .put_segment_handover(
                        &ProcessId::from("segmented-process-parent"),
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next,
                            handover: boundary,
                        },
                    )
                    .await
                    .expect("durably store process-parent handover");
                let loaded = continuations
                    .get_segment_handover(&ProcessId::from("segmented-process-parent"), next)
                    .await
                    .expect("reload process-parent handover")
                    .expect("stored process-parent handover");
                input_handover = Some(loaded.handover);
                ordinal = next;
            }
            Err(join_error) if join_error.is_panic() => break,
            other => panic!("unexpected segmented process-parent result: {other:?}"),
        }
    }
    assert_eq!(
        boundary_count, 2,
        "the real Lashlang process spans three segments"
    );
    let terminal_parent = registry
        .get_process(&ProcessId::from("segmented-process-parent"))
        .await
        .expect("read terminal segmented parent")
        .expect("segmented parent exists");
    assert_eq!(
        terminal_parent.outcome,
        Some(process_success(serde_json::json!("none"))),
        "the terminal is durable before the injected teardown crash"
    );
    let pending = registry
        .get_pending_parent_end_plan(&ProcessId::from("segmented-process-parent"))
        .await
        .expect("load segmented parent-end plan")
        .expect("the early-segment action survives in durable state");
    let [segmented_action] = pending.actions.as_slice() else {
        panic!("expected one literal early-segment action: {pending:?}");
    };
    assert_eq!(
        segmented_action.parent_end.policy,
        lash_core::ProcessParentEndPolicy::Cancel
    );
    assert_eq!(
        lash_core::rederive_tool_intent_identity(&segmented_action.identity)
            .expect("rederive retained segmented identity"),
        segmented_action.identity,
        "the retained action carries its full validated identity"
    );
    assert_eq!(
        registry
            .events_after(&segmented_action.parent_end.process_id, 0)
            .await
            .expect("events before segmented redrive")
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        0,
        "the crash is after terminal commit and before teardown"
    );

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("redrive durable segmented parent-end plan");
    assert!(
        registry
            .get_pending_parent_end_plan(&ProcessId::from("segmented-process-parent"))
            .await
            .expect("inspect completed segmented plan")
            .is_none()
    );
    assert_eq!(
        registry
            .events_after(&segmented_action.parent_end.process_id, 0)
            .await
            .expect("events after segmented redrive")
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        1,
        "redrive applies the early-segment Cancel exactly once"
    );

    let tool_parent = ProcessRegistration::new(
        "tool-call-process-parent",
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "tool-call-process-parent-call",
                "tool:process_parent_intent",
                "process_parent_intent",
                serde_json::json!({"emit": true, "child": "tool-call"}),
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("process-parent-law")),
    )
    .with_execution_env_ref(Some(env_ref));
    registry
        .register_process(tool_parent)
        .await
        .expect("register ToolCall process parent");
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive ToolCall process parent");
    let tool_terminal = lash_core::NativeProcessWork::for_registry(Arc::clone(&registry))
        .await_terminal(&ProcessId::from("tool-call-process-parent"))
        .await
        .expect("await ToolCall parent terminal");
    assert_eq!(
        tool_terminal,
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!({"child": "tool-call"}),
        ))
    );
    let children = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("list process-parent children");
    for child_name in ["segmented", "tool-call"] {
        let child = children
            .iter()
            .find(|record| {
                matches!(
                    record.input.as_ref(),
                    ProcessInput::External { metadata }
                        if metadata["process_parent_child"] == child_name
                )
            })
            .unwrap_or_else(|| panic!("missing {child_name} child in {children:?}"));
        let awaiter = lash_core::NativeProcessWork::for_registry(Arc::clone(&registry));
        tokio::time::timeout(
            Duration::from_secs(5),
            awaiter.await_event(&child.id, "process.cancel_requested", 0),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out awaiting {child_name} child Cancel delivery"))
        .unwrap_or_else(|error| panic!("failed awaiting {child_name} child Cancel: {error}"));
        assert_eq!(
            registry
                .events_after(&child.id, 0)
                .await
                .expect("read child cancellation")
                .iter()
                .filter(|event| event.event_type == "process.cancel_requested")
                .count(),
            1,
            "{child_name} child receives one literal Cancel"
        );
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
}

pub(super) fn recovery_session_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    }
}

pub(super) async fn persist_recovery_env_ref() -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::empty(),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist recovery process execution env")
}

pub(super) async fn persist_snapshot_recovery_env_ref(
    snapshot_ref: &str,
) -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        snapshot_recovery_tool_options(snapshot_ref),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist snapshot recovery process execution env")
}

pub(super) fn process_wake_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.wake".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            wake: Some(lash_core::ProcessWakeSpec {
                when: Some(lash_core::ProcessValueSelector::Present(
                    "/wake_input".to_string(),
                )),
                input: lash_core::ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..lash_core::ProcessEventSemanticsSpec::default()
        },
    }
}

pub(super) async fn snapshot_lashlang_registration(
    process_id: &ProcessId,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          called = await tools.snapshot_echo({ line: "restored" })?
          finish called.echo
        }
        "#,
    )
    .expect("snapshot lashlang module");
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "snapshot_echo",
            "tool:snapshot_echo",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("host catalog operation must not conflict");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_sleep()
                .with_process_signals(),
        ),
    )
    .expect("link snapshot lashlang module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked_module.artifact,
    )
    .await
    .expect("store snapshot lashlang module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("main")
        .expect("main process ref")
        .clone();
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
pub(super) async fn sqlite_process_recovery_reopens_registry_worker_observers_wakes_and_cancel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_a = recovery_worker(Arc::clone(&registry_a), Arc::clone(&store_factory));
    let _root_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::default(),
            policy: recovery_session_policy(),
        })
        .await
        .expect("create root session store before wake delivery");
    let endpoint_a = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_a)),
                Arc::clone(&registry_a),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context_a = Arc::new(RecordingContext::with_endpoint(endpoint_a));
    let host_a = RestateRuntimeEffectController::new(context_a);
    let creator_scope = lash_core::SessionScope::new("root");
    let env_ref = persist_recovery_env_ref().await;
    let registration = ProcessRegistration::new(
        "recover-tool",
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "recover-call",
                "tool:recovery_echo",
                "recovery_echo",
                serde_json::json!({ "line": "wake-after-rebuild" }),
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(creator_scope.clone()),
    )
    .with_extra_event_types([process_wake_event_type()])
    .with_execution_env_ref(Some(env_ref))
    .with_wake_session_id(Some(creator_scope.session_id.clone()));

    host_a
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: vec![creator_scope.session_id.clone()],
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            registry_local_executor(Arc::clone(&registry_a)),
        )
        .await
        .expect("schedule and run process through Restate endpoint");
    drop(host_a);
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let observed = registry_b
        .list_observed_by(
            &creator_scope.session_id,
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list reopened observations");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].id, "recover-tool");
    let recovered_process_ref = lash_core::ProcessRef::from_record(&observed[0]);
    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&ProcessId::from("recover-tool"))
            .await
            .expect("await recovered terminal process"),
        process_success(serde_json::json!({ "echo": "wake-after-rebuild" }))
    );
    let queue_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy {
                model: lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
                ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
            },
        })
        .await
        .expect("open root session store");
    let queued = queue_store
        .list_queued_work(&SessionId::from("root"))
        .await
        .expect("list queued wakes");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].items.len(), 1);
    let lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } = &queued[0].items[0].payload
    else {
        panic!("expected process wake queue payload");
    };
    assert_eq!(wake.input, "wake-after-rebuild");
    assert_eq!(wake.target_session_id, "root");

    let worker_b = recovery_worker(Arc::clone(&registry_b), store_factory);
    let endpoint_b = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_b)),
                Arc::clone(&registry_b),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context_b = Arc::new(RecordingContext::with_endpoint(endpoint_b));
    let host_b = RestateRuntimeEffectController::new(context_b);
    host_b
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_ref: recovered_process_ref,
                    reason: Some("post-rebuild cancel probe".to_string()),
                    replay: None,
                }),
            ),
            registry_local_executor(Arc::clone(&registry_b)),
        )
        .await
        .expect("cancel through reopened process workflow");
    assert!(
        registry_b
            .events_after(&ProcessId::from("recover-tool"), 0)
            .await
            .expect("events after cancel")
            .iter()
            .any(|event| event.event_type == "process.cancel_requested")
    );
}
