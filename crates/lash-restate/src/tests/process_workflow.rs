use super::*;
use lash_core::ProcessEventLogTestSupport as _;

use lashlang::testing::ast_builders as b;

#[tokio::test]
pub(super) async fn persisted_handover_is_change_feed_and_event_invariant() {
    let (registry, continuations) = process_stores();
    let _record = registry
        .register_process(rerunnable_registration())
        .await
        .expect("register process");
    let (_, cursor) = registry
        .processes_changed_since(lash_core::ProcessChangeCursor::default(), 10)
        .await
        .expect("initial change feed");
    continuations
        .put_segment_handover(
            &_record.id,
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 1,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
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
            .full_event_window(&_record.id, 0)
            .await
            .expect("events")
            .is_empty()
    );
}

/// A successor segment whose process was cancelled between segments is still
/// driven, so its command emission matches its journal: its stop delivery
/// fires the stop its runner was lent, and the runner's own recorded outcome
/// is the cancellation (FIG-3673).
#[tokio::test]
pub(super) async fn cancel_redrives_successor_engine() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let signal_transport = Arc::new(BlockingCancelSignalTransport::default());
    signal_transport.release.notify_one();
    let workflow = LashProcessWorkflowImpl::new(
        runner.clone(),
        registry.clone(),
        continuation_store(),
        RestateIngressClient::new(RestateConnection::with_transport(
            "https://restate.invalid",
            signal_transport.clone(),
        )),
        test_restate_authority_id(),
    );
    let registration = rerunnable_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register process")
        .id;
    registry
        .append_event(
            &process_id,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &process_id,
                &lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:cancel_redrives_successor_engine",
                    11,
                ),
            ),
        )
        .await
        .expect("cancel between segments");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        workflow.run_registration_for_test(
            process_id.clone(),
            registration,
            ProcessExecutionContext::default(),
            process_scope(&process_id),
            1,
            Some(lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "program-v1".to_string(),
                engine_state: vec![1],
            }),
        ),
    )
    .await
    .expect("the cancelled successor settles")
    .expect("cancelled successor");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_cancellation(output.as_ref())
    ));
    assert_eq!(
        signal_transport.requests.lock_recover()[0].url,
        format!("https://restate.invalid/LashProcessWorkflow/{process_id}%231/await_cancel"),
        "the successor watches its own segment's cancel promise"
    );
}

fn process_1() -> ProcessId {
    ProcessId::fixture("process-1")
}

#[test]
pub(super) fn cancel_terminal_from_successor_routes_to_root_await_workflow_key() {
    assert_eq!(terminal_completion_workflow_key(&process_1(), 0), None);
    assert_eq!(
        terminal_completion_workflow_key(&process_1(), 1),
        Some(process_1().to_string())
    );
    assert_eq!(
        process_segment_workflow_key(&process_1(), 1),
        format!("{}#1", process_1()),
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
    let registration = rerunnable_registration();
    let record = registry
        .register_process(registration.clone())
        .await
        .expect("register terminal-failure child");
    let process_id = record.id.clone();
    let parent_context = Arc::new(RecordingContext::default());
    let parent = RestateRuntimeEffectController::new_for_test(Arc::clone(&parent_context));
    let parent_wait = parent.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "await-terminal-child-failure"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: record.id.clone(),
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
            process_id: process_id.clone(),
            registration,
            execution_context: ProcessExecutionContext::default(),
            segment_ordinal: 0,
            journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
        },
        true,
    )
    .await
    .expect("terminal child workflow must complete through the Restate endpoint");
    let promise_key = restate_process_terminal_await_key(&test_restate_authority_id(), &process_id)
        .expect("terminal promise key")
        .promise_key();
    let resolution = restate_completed_promise(&endpoint_output, &promise_key)
        .expect("terminal child workflow must publish its process-terminal promise");
    let serde_json::Value::String(resolution) = resolution else {
        panic!("terminal promise completion must carry its serialized typed resolution")
    };
    parent_context.resolve_process_terminal_resolution(
        &process_id,
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
    assert_eq!(failure.code, "engine_service_unregistered");
    assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read terminal child")
            .and_then(|record| record.outcome),
        Some(ProcessAwaitOutput::Settled {
            output: output.clone(),
        })
    );
}

#[tokio::test]
pub(super) async fn replay_divergence_mid_child_aborts_parent_without_terminalizing_rerunnable_child()
 {
    let registry = process_registry();
    let registration = rerunnable_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register divergence-aborted child")
        .id;
    let runner = Arc::new(DivergenceThenSuccessRunner {
        runs: AtomicUsize::new(0),
    });
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    );

    let error = workflow
        .run_registration_for_test(
            process_id.clone(),
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::testing::UnavailableEffectController),
                durable_admission(&ExecutionScope::process(process_id.clone())),
            )
            .expect("divergence-aborted child scope"),
            0,
            None,
        )
        .await
        .expect_err("a diverged child must abort the parent invocation");
    let rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
            .to_string();
    assert!(
        rendered.contains("effect_replay_divergence"),
        "parent abort lost the typed divergence code: {rendered}"
    );
    let interrupted = registry
        .get_process(&process_id)
        .await
        .expect("read interrupted child")
        .expect("interrupted child remains registered");
    assert!(
        !interrupted.is_terminal() && interrupted.outcome.is_none(),
        "a divergence abort must leave the rerunnable child non-terminal: {interrupted:?}"
    );

    let rerun = workflow
        .run_registration_for_test(
            process_id.clone(),
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::testing::UnavailableEffectController),
                durable_admission(&ExecutionScope::process(process_id.clone())),
            )
            .expect("rerun child scope"),
            0,
            None,
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
    let registry = process_registry();
    let registration = rerunnable_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register rerunnable child")
        .id;
    let runner = Arc::new(OpaqueFailureThenSuccessRunner {
        runs: AtomicUsize::new(0),
    });
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    );

    workflow
        .run_registration_for_test(
            process_id.clone(),
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::testing::UnavailableEffectController),
                durable_admission(&ExecutionScope::process(process_id.clone())),
            )
            .expect("first child scope"),
            0,
            None,
        )
        .await
        .expect_err("opaque infrastructure failure must abort the invocation");
    let interrupted = registry
        .get_process(&process_id)
        .await
        .expect("read interrupted child")
        .expect("child remains registered");
    assert!(!interrupted.is_terminal());
    assert!(interrupted.outcome.is_none());

    let rerun = workflow
        .run_registration_for_test(
            process_id.clone(),
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::testing::UnavailableEffectController),
                durable_admission(&ExecutionScope::process(process_id.clone())),
            )
            .expect("rerun child scope"),
            0,
            None,
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
            lash_core::RuntimeErrorCode::EngineProcessIngressSubmit,
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
        crate::process::process_ingress_submit_error(&ProcessId::fixture("proc-1"), unregistered)
    else {
        panic!("ingress submit failures must stay typed runtime errors");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EngineServiceUnregistered
    );
    assert!(!error.code.is_retryable());

    let transient = crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/run".to_string(),
        status: 503,
        body: "unavailable".to_string(),
    };
    let lash_core::PluginError::Runtime(error) =
        crate::process::process_ingress_submit_error(&ProcessId::fixture("proc-1"), transient)
    else {
        panic!("ingress submit failures must stay typed runtime errors");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EngineProcessIngressSubmit
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
            lash_core::RuntimeErrorCode::EngineServiceUnregistered,
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
        rerunnable_registration(),
        ProcessId::fixture("wait"),
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
    // The run is recorded and the process stays live for the cancel below.
    runner.stay_live.store(true, Ordering::SeqCst);
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
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registration = rerunnable_registration()
        .with_wake_session_id(Some(SessionId::from("wake-smoke")))
        .with_start_key(Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "background-smoke-start",
        )));
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-smoke").into_runtime_invocation(),
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
    let process_id = record.id.clone();

    let external_ref = record.external_ref.as_ref().expect("external ref");
    assert_eq!(external_ref.backend, "restate");
    assert_eq!(external_ref.id, format!("LashProcessWorkflow/{process_id}"));
    assert_eq!(
        external_ref
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!(format!("invocation-{process_id}")))
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
    assert_eq!(observed[0].id, process_id);
    let observed_external_ref = observed[0].external_ref.as_ref().expect("external ref");
    assert_eq!(observed_external_ref.backend, "restate");
    assert_eq!(
        observed_external_ref.id,
        format!("LashProcessWorkflow/{process_id}")
    );

    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| registration.start_key.clone())
            .collect::<Vec<_>>(),
        vec![Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "background-smoke-start"
        ))]
    );
    assert_eq!(
        runner.ran.lock_recover().as_slice(),
        &[RecordedProcessRun {
            process_id: process_id.clone(),
            wake_target_session_id: Some(SessionId::from("wake-smoke")),
            tool_effect_id: Some("tool-smoke".to_string()),
            execution_scope_id: process_id.to_string(),
        }]
    );

    let live = registry
        .get_process(&process_id)
        .await
        .expect("read cancel target")
        .expect("retained target");
    assert!(!live.is_terminal());
    assert!(live.cancel_request.is_none());

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: process_id.clone(),
                    origin: lash_core::CancelOrigin::OperatorRequested,
                    requester: "actor:smoke".to_string(),
                    attribution: None,
                }),
            ),
            registry_local_executor(registry),
        )
        .await
        .expect("cancel through endpoint smoke");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Cancel { record },
    } = outcome
    else {
        panic!("expected cancellation outcome");
    };
    assert_eq!(record.id, process_id);
    let request = record
        .cancel_request
        .as_deref()
        .expect("folded cancellation");
    assert_eq!(request.origin, lash_core::CancelOrigin::OperatorRequested);
    assert_eq!(request.requester, "actor:smoke");
    let expected = RestateProcessCancelRequest {
        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
        process_id,
        request: request.clone(),
    };
    assert_eq!(
        context.cancelled.lock_recover().as_slice(),
        std::slice::from_ref(&expected)
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(process_id) = call.context.enclosing_process() else {
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
            process_id: process_id.clone(),
            event_type: "process.wake".to_string(),
            payload: serde_json::json!({ "message": line, "wake_input": line }),
        });
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "echo": line })),
            lash_core::ToolIntents::v3(vec![intent]),
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let line = call
                .args
                .get("line")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            lash_core::ToolOutcome::ok(serde_json::json!({ "echo": format!("snapshot:{line}") }))
        })
        .await
        .into()
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

pub(super) async fn recovery_worker(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> DurableProcessWorker {
    recovery_worker_with_plugins(registry, store_factory, Vec::new()).await
}

pub(super) async fn recovery_worker_with_plugins(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> DurableProcessWorker {
    recovery_worker_with_plugins_and_trace(registry, store_factory, extra_plugins, None).await
}

pub(super) async fn recovery_worker_with_plugins_and_trace(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
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
    // The worker reaches sessions through the catalog the test hands it, layered
    // onto a memory backend for every other port.
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(
        Arc::new(
            lash_sqlite_store::SqliteBackend::memory()
                .await
                .expect("open a SQLite memory backend"),
        )
        .into(),
    )
    .map_session_store_factory(|_| store_factory)
    .into_backend();
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(process_env_store)
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                recovery_artifact_store(),
                lash_lashlang_runtime::LashlangSurface::default(),
            )
            .with_execution_trace(trace_sink, lash_trace::TraceContext::default()),
        ),
    );
    DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(recovery_session_policy()),
    )
    .expect("valid test native substrate config")
}

/// The `processes` catalogue a fixture that starts a child links against.
///
/// Starting a child is a catalogue tool, not a special form (ADR 0095), so the
/// operation binds to the shipped `processes.start` tool id and carries that
/// tool's own contract rather than a fixture-local shape.
fn process_control_resources() -> lashlang::LashlangHostCatalog {
    let contract = lash_plugin_process_controls::process_start_tool_definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:start_process",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("link process start operation");
    resources
}

pub(super) async fn segmented_child_await_registration(
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    // process child() {
    //   finish { from: "child" }
    // }
    //
    // process main() {
    //   handle = await processes.start({ definition: child })?
    //   result = (await handle)?
    //   finish result.from
    // }
    let module = b::module(
        vec![
            b::process(
                "child",
                Vec::new(),
                b::finish(b::record(vec![("from", b::string("child"))])),
            ),
            b::process(
                "main",
                Vec::new(),
                b::block(vec![
                    b::assign("handle", b::start("child", Vec::new())),
                    b::assign("result", b::unwrap(b::await_expr(b::var("handle")))),
                    b::finish(b::field(b::var("result"), "from")),
                ]),
            ),
        ],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            process_control_resources(),
            lashlang::LashlangAbilities::default(),
        ),
    )
    .expect("link segmented child-await law");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ArtifactOwner::host("restate-workflow-test"),
        &linked.artifact,
    )
    .await
    .expect("store segmented child-await artifact");
    ProcessRegistration::new(
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref("main")
                .expect("main process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(
            "segmented-child-await-root",
        )),
        lash_core::Lifetime::Detached,
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
pub(super) async fn lashlang_process_retains_child_possession_across_restate_segments() {
    let (registry, continuations) = process_stores();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    // The cell starts its child through `processes.start`, which is a plugin
    // tool now, so the worker that runs the parent has to serve it.
    let worker = recovery_worker_with_plugins_and_trace(
        Arc::clone(&registry),
        memory_session_store_factory().await,
        vec![Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        )],
        Some(graphs.clone()),
    )
    .await;
    let workflow = Arc::new(
        LashProcessWorkflowImpl::new_for_test(
            Arc::new(RestateCoreProcessRunner::new(worker.clone())),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        )
        .with_segment_effect_budget_selector(|_| 1),
    );
    let registration = segmented_child_await_registration(persist_recovery_env_ref().await).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register segmented child-await parent")
        .id;

    let mut ordinal = 0_u64;
    let mut input_handover = None;
    let mut boundary_count = 0_usize;
    let restate_events = Arc::new(RecordingContext::default());
    loop {
        let context = Arc::new(ReplayableRecordingContext {
            events: Arc::clone(&restate_events),
            ..ReplayableRecordingContext::default()
        });
        context.install_process_worker(worker.clone());
        let controller = RestateRuntimeEffectController::with_options_for_test(
            Arc::clone(&context),
            RestateEffectControllerOptions::default().segment_effect_budget(1),
        );
        // Every segment continues the root segment's execution.
        let execution_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
            &process_id,
            "segmented-child-await-root-execution",
        );
        let outcome = workflow
            .run_registration_for_test(
                process_id.clone(),
                registration.clone(),
                ProcessExecutionContext::default()
                    .with_execution_write_authority(execution_authority.clone()),
                controller
                    .process_scope_for_test(durable_admission(&ExecutionScope::process(
                        process_id.clone(),
                    )))
                    .expect("segmented child-await scope"),
                ordinal,
                input_handover.take(),
            )
            .await
            .expect("run segmented child-await process");
        match outcome {
            lash_core::ProcessRunOutcome::SegmentBoundary(boundary) => {
                boundary_count += 1;
                let next = ordinal + 1;
                if next == 1 {
                    let before = graphs
                        .graphs()
                        .into_iter()
                        .find(|graph| {
                            matches!(&graph.subject, lash_trace::TraceRuntimeSubject::Process { process_id: subject_id }
                                if *subject_id == process_id)
                        })
                        .expect("parent attempt graph before replay");
                    context.start_replay();
                    let replay = workflow
                        .run_registration_for_test(
                            process_id.clone(),
                            registration.clone(),
                            ProcessExecutionContext::default()
                                .with_execution_write_authority(execution_authority),
                            controller
                                .process_scope_for_test(durable_admission(
                                    &ExecutionScope::process(process_id.clone()),
                                ))
                                .expect("replayed segment scope"),
                            ordinal,
                            None,
                        )
                        .await
                        .expect("replay the same Restate invocation");
                    assert!(matches!(
                        replay,
                        lash_core::ProcessRunOutcome::SegmentBoundary(_)
                    ));
                    let after = graphs
                        .graphs()
                        .into_iter()
                        .find(|graph| {
                            matches!(&graph.subject, lash_trace::TraceRuntimeSubject::Process { process_id: subject_id }
                                if *subject_id == process_id)
                        })
                        .expect("parent attempt graph after replay");
                    assert_eq!(
                        after.history, before.history,
                        "replay must not duplicate parent node events"
                    );
                }
                continuations
                    .put_segment_handover(
                        &process_id,
                        lash_core::PersistedSegmentHandover {
                            writer: String::new(),
                            segment_ordinal: next,
                            handover: boundary,
                        },
                    )
                    .await
                    .expect("store segmented child-await handover");
                input_handover = Some(
                    continuations
                        .get_segment_handover(&process_id, next)
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
    let parent_graphs = graphs
        .graphs()
        .into_iter()
        .filter(|graph| {
            matches!(&graph.subject, lash_trace::TraceRuntimeSubject::Process { process_id: subject_id }
            if *subject_id == process_id)
        })
        .collect::<Vec<_>>();
    let mut attempts = parent_graphs
        .iter()
        .map(|graph| graph.history[0].event.identity.attempt().expect("attempt"))
        .collect::<Vec<_>>();
    attempts.sort_unstable();
    assert_eq!(attempts, vec![1], "continued segments stay one attempt");
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
    lash_core::runtime::publish_process_execution_env(
        RECOVERY_PROCESS_ENV_STORE.as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-env"),
        &spec,
    )
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
    lash_core::runtime::publish_process_execution_env(
        RECOVERY_PROCESS_ENV_STORE.as_ref(),
        &lash_core::ArtifactOwner::host("restate-snapshot-recovery-env"),
        &spec,
    )
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
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    // process main() {
    //   called = await tools.snapshot_echo({ line: "restored" })?
    //   finish called.echo
    // }
    let module = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::block(vec![
                b::assign(
                    "called",
                    b::module_call(
                        &["tools"],
                        "snapshot_echo",
                        vec![b::record(vec![("line", b::string("restored"))])],
                    ),
                ),
                b::finish(b::field(b::var("called"), "echo")),
            ]),
        )],
        Vec::new(),
    );
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "snapshot_echo",
            "tool:snapshot_echo",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("host catalog operation must not conflict");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default().with_sleep(),
        ),
    )
    .expect("link snapshot lashlang module");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ArtifactOwner::host("restate-workflow-test"),
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
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.artifact.module_ref().clone(),
            process_ref,
            host_requirements_ref: linked_module.artifact.host_requirements_ref().clone(),
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
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
    let worker_a = recovery_worker(Arc::clone(&registry_a), Arc::clone(&store_factory)).await;
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
    let host_a = RestateRuntimeEffectController::new_for_test(context_a);
    let creator_scope = lash_core::SessionScope::new("root");
    let env_ref = persist_recovery_env_ref().await;
    let registration = ProcessRegistration::new(
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
        lash_core::Lifetime::Detached,
    )
    .with_extra_event_types([process_wake_event_type()])
    .with_execution_env_ref(Some(env_ref))
    .with_wake_session_id(Some(creator_scope.session_id.clone()))
    .with_start_key(Some(lash_core::StartKey::for_host(
        lash_core::StartKeyOwner::HOST,
        "recovery-start",
    )));

    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start {
            record: recover_tool,
        },
    } = host_a
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
            registry_local_executor(Arc::clone(&registry_a))
                .with_process_env_store(RECOVERY_PROCESS_ENV_STORE.clone()
                    as Arc<dyn lash_core::ProcessExecutionEnvStore>),
        )
        .await
        .expect("schedule and run process through Restate endpoint")
    else {
        panic!("the recovery start must report the started process");
    };
    let recover_tool_id = recover_tool.id;
    let recover_cancel_id = registry_a
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register nonterminal cancellation target before reopen")
        .id;
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
    assert_eq!(observed[0].id, recover_tool_id);
    assert!(
        registry_b
            .get_process(&recover_cancel_id)
            .await
            .expect("read reopened cancellation target")
            .is_some(),
        "the reopened cancellation target remains registered"
    );
    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&recover_tool_id)
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

    let worker_b = recovery_worker(Arc::clone(&registry_b), store_factory).await;
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
    let host_b = RestateRuntimeEffectController::new_for_test(context_b);
    host_b
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: recover_cancel_id.clone(),
                    origin: lash_core::CancelOrigin::OperatorRequested,
                    requester: "actor:post-rebuild".to_string(),
                    attribution: None,
                }),
            ),
            registry_local_executor(Arc::clone(&registry_b)),
        )
        .await
        .expect("cancel through reopened process workflow");
    assert!(
        registry_b
            .full_event_window(&recover_cancel_id, 0)
            .await
            .expect("events after cancel")
            .iter()
            .any(|event| event.event_type == "process.cancel_requested")
    );
}
