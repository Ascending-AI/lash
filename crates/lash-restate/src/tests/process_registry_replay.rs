use super::*;
use lashlang::LashlangArtifactStore as _;

#[tokio::test]
pub(super) async fn restate_controller_replays_parent_shaped_start_await_suspend_flow() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-parent-flow-replay";
    let terminal = process_success(serde_json::json!({ "done": true }));
    let suspend_key = restate_await_event_key(
        &ExecutionScope::process(process_id),
        AwaitEventWaitIdentity::Custom {
            key: "parent-resume-input".to_string(),
        },
    )
    .expect("parent suspend key");
    context.resolve_process_terminal(&ProcessId::from(process_id), &terminal);
    context.resolve_durable_event(RestateDurableWaitResolveRequest {
        key: suspend_key.clone(),
        resolution: Resolution::Ok(serde_json::json!({ "answer": "resume" })),
    });

    run_parent_shaped_start_await_suspend_flow(
        &host,
        registry.clone(),
        &ProcessId::from(process_id),
        suspend_key.clone(),
    )
    .await;
    run_parent_shaped_start_await_suspend_flow(
        &host,
        registry,
        &ProcessId::from(process_id),
        suspend_key,
    )
    .await;

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[
            format!("send:{process_id}"),
            format!("call:{process_id}"),
            format!("send:{process_id}"),
            format!("call:{process_id}"),
        ],
        "a parent-shaped replay after suspension must preserve child start/await command order"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_schedules_lashlang_process_with_serializable_input() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let module = lashlang::parse("process scan(root: str) { finish root }")
        .expect("lashlang process module");
    let catalog = lashlang::LashlangHostCatalog::new();
    let linked_module = lashlang::LinkedModule::link(
        module.clone(),
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all()),
    )
    .expect("link lashlang module");
    let artifact_store = Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    artifact_store
        .publish_module_artifact(
            &lash_core::ArtifactOwner::host("restate-serializable-input"),
            &linked_module.artifact,
        )
        .await
        .expect("publish serializable-input artifact");
    let (process_env_store, process_env_ref) = lash_core::testing::process_execution_env_fixture();
    let process_ref = linked_module
        .artifact
        .process_ref("scan")
        .expect("scan process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("root".to_string(), serde_json::json!("."));
    let registration = ProcessRegistration::new(
        "process-1",
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref.clone(),
            process_ref: process_ref.clone(),
            host_requirements_ref: linked_module.host_requirements_ref.clone(),
            process_name: "scan".to_string(),
            args: args.clone(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(process_env_ref))
    .with_wake_session_id(Some(SessionId::from("session")));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "lashlang-process-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: Vec::new(),
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            registry_local_executor(registry.clone())
                .with_process_env_store(process_env_store)
                .with_process_engines(lash_core::ProcessEngineRegistry::new().with_registration(
                    lash_lashlang_runtime::lashlang_process_engine_registration(
                        lash_lashlang_runtime::LashlangProcessEngine::new(
                            artifact_store,
                            lash_lashlang_runtime::LashlangSurface::default(),
                        ),
                    ),
                )),
        )
        .await
        .expect("start");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert_eq!(
        record
            .external_ref
            .as_ref()
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from("process-1"))
            .await
            .expect("read process")
            .expect("registered process")
            .external_ref
            .as_ref()
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    let started = context.started.lock_recover().clone();
    assert_eq!(started.len(), 1);
    let ProcessInput::Engine { kind, payload } = started[0].input.as_ref() else {
        panic!("expected engine process input");
    };
    assert_eq!(kind, lash_lashlang_runtime::LASHLANG_ENGINE_KIND);
    let sent = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("typed lashlang payload");
    assert_eq!(sent.module_ref, linked_module.module_ref);
    assert_eq!(sent.process_ref, process_ref);
    assert_eq!(
        sent.host_requirements_ref,
        linked_module.host_requirements_ref
    );
    assert_eq!(sent.process_name, "scan");
    assert_eq!(sent.args, args);
    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| { registration.wake_session_id.as_deref() })
            .collect::<Vec<_>>(),
        vec![Some("session")]
    );
}

#[tokio::test]
pub(super) async fn restate_controller_lists_and_transfers_observers_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let s1 = lash_core::SessionScope::new("s1");
    let s2 = lash_core::SessionScope::new("s2");
    registry
        .register_process(external_registration("task-list"))
        .await
        .expect("register");
    registry
        .add_observer(
            &s1.session_id,
            &ProcessId::from("task-list"),
            lash_core::ProcessObserverBy::host("restate-list-test"),
        )
        .await
        .expect("observe");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-list-s1"),
                RuntimeEffectCommand::process(ProcessCommand::List {
                    session_scope: s1.clone(),
                    mode: lash_core::ProcessListMode::Live,
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("list");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::List { entries },
    } = outcome
    else {
        panic!("wrong list outcome");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "task-list");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-transfer"),
                RuntimeEffectCommand::process(ProcessCommand::Transfer {
                    from_scope: s1.clone(),
                    to_scope: s2.clone(),
                    process_ids: vec![ProcessId::from("task-list")],
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("transfer");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Transfer
        }
    ));

    let entries = registry
        .list_observed_by(
            &s2.session_id,
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("s2 observed");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "task-list");
    assert!(
        registry
            .list_observed_by(
                &s1.session_id,
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("s1")
            .is_empty()
    );
    assert!(context.started.lock_recover().is_empty());
}

#[tokio::test]
pub(super) async fn restate_controller_awaits_and_signals_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let host = RestateRuntimeEffectController::new(context.clone()).with_trace_sink(sink_dyn);
    let registry = process_registry();
    let await_record = registry
        .register_process(external_registration("task-await-signal"))
        .await
        .expect("register");
    let signal_record = registry
        .register_process(
            external_registration("task-signal")
                .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
                .with_extra_event_types([lash_core::ProcessEventType {
                    name: "signal.notify".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                }]),
        )
        .await
        .expect("register signal target");
    let awaited_output = process_success(serde_json::json!({ "done": true }));
    registry
        .complete_process(
            &ProcessId::from("task-await-signal"),
            awaited_output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    context.resolve_process_terminal(&ProcessId::from("task-await-signal"), &awaited_output);

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-await"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_ref: lash_core::ProcessRef::from_record(&await_record),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("await");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Await { output },
    } = outcome
    else {
        panic!("wrong await outcome");
    };
    assert_eq!(
        *output,
        process_success(serde_json::json!({ "done": true }))
    );
    assert_eq!(
        sink.records
            .lock_recover()
            .iter()
            .map(|record| record.event.kind())
            .collect::<Vec<_>>(),
        vec!["durable_wait_parked", "durable_wait_resolved"],
        "process awaits expose the same durable wait evidence as await-event"
    );

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_ref: lash_core::ProcessRef::from_record(&signal_record),
                    signal_name: "notify".to_string(),
                    signal_id: "notify".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify" }),
                    )
                    .with_replay_key("signal:notify"),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { event },
    } = outcome
    else {
        panic!("wrong signal outcome");
    };
    assert_eq!(event.event_type, "signal.notify");
    assert!(context.started.lock_recover().is_empty());

    // Append-before-resolve discipline: the durable event is the record, the
    // promise resolution is only the wake-up, keyed by the Nth occurrence of
    // this signal name so repeated signals map onto one-shot engine promises.
    {
        let resolved = context.resolved_events.lock_recover();
        assert_eq!(resolved.len(), 1);
        let expected_key = restate_await_event_key(
            &ExecutionScope::process("task-signal"),
            AwaitEventWaitIdentity::process_signal("task-signal", "notify", 1),
        )
        .expect("first signal wait key");
        assert_eq!(resolved[0].key, expected_key);
        assert_eq!(
            resolved[0].resolution,
            Resolution::Ok(serde_json::json!({ "signal": "notify" }))
        );
    }

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal-2"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_ref: lash_core::ProcessRef::from_record(&signal_record),
                    signal_name: "notify".to_string(),
                    signal_id: "notify-2".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify-2" }),
                    )
                    .with_replay_key("signal:notify-2"),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("second signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { .. },
    } = outcome
    else {
        panic!("wrong second signal outcome");
    };
    let resolved = context.resolved_events.lock_recover();
    assert_eq!(resolved.len(), 2);
    let expected_key = restate_await_event_key(
        &ExecutionScope::process("task-signal"),
        AwaitEventWaitIdentity::process_signal("task-signal", "notify", 2),
    )
    .expect("second signal wait key");
    assert_eq!(
        resolved[1].key, expected_key,
        "second signal must resolve the ordinal-2 wait key"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_cancel_requests_call_workflow_cancel() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let registration = external_registration("task-cancel");
    let record = registry
        .register_process(registration)
        .await
        .expect("register");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_ref: lash_core::ProcessRef::from_record(&record),
                    reason: Some("user requested".to_string()),
                    replay: None,
                }),
            ),
            registry_local_executor(registry),
        )
        .await
        .expect("cancel");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Cancel { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert!(!record.is_terminal());
    assert_eq!(
        context.cancelled.lock_recover().as_slice(),
        &[(
            "task-cancel".to_string(),
            Some("user requested".to_string())
        )]
    );
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RecordedProcessRun {
    pub(super) process_id: ProcessId,
    pub(super) wake_target_session_id: Option<SessionId>,
    pub(super) tool_effect_id: Option<String>,
    pub(super) execution_scope_id: String,
    pub(super) turn_control_participation: lash_core::TurnControlParticipation,
}

#[derive(Default)]
pub(super) struct RecordingRunner {
    pub(super) ran: Mutex<Vec<RecordedProcessRun>>,
    pub(super) cancelled: Mutex<Vec<RestateProcessCancelRequest>>,
}

#[async_trait::async_trait]
impl RestateProcessRunner for RecordingRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let turn_control_participation = scoped_effect_controller
            .controller()
            .turn_control_participation()
            .await
            .map_err(PluginError::Runtime)?;
        self.ran.lock_recover().push(RecordedProcessRun {
            process_id: registration.id.clone(),
            wake_target_session_id: registration.wake_session_id.clone(),
            tool_effect_id: execution_context
                .causal_invocation
                .and_then(|invocation| invocation.effect_id().map(str::to_string)),
            execution_scope_id: scoped_effect_controller.scope_id().to_string(),
            turn_control_participation,
        });
        Ok(process_success(serde_json::json!({"ok": true})).into())
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.cancelled.lock_recover().push(request);
        Ok(())
    }
}

pub(super) struct AlreadyStartedRunner {
    pub(super) calls: Mutex<usize>,
    pub(super) winner: lash_core::LeaseOwnerIdentity,
}

pub(super) struct TerminalFailureRunner;

pub(super) struct ReplacementThenSuccessRunner {
    pub(super) runs: AtomicUsize,
}

pub(super) struct OpaqueFailureThenSuccessRunner {
    pub(super) runs: AtomicUsize,
}

#[async_trait::async_trait]
impl RestateProcessRunner for ReplacementThenSuccessRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(PluginError::RuntimeEffectController(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::WorkerReplacementAbort,
                    "recorded runtime effect did not match the reconstructed envelope",
                )
                .with_summary(lash_core::RuntimeEffectReplayMismatchReport {
                    divergent_path_count: 1,
                    first_divergent_paths: vec!["command.request.model".to_string()],
                }),
            ));
        }
        Ok(process_success(serde_json::json!({"rerun": "succeeded"})).into())
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for OpaqueFailureThenSuccessRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(PluginError::Session(
                "process infrastructure became unavailable".to_string(),
            ));
        }
        Ok(process_success(serde_json::json!({"rerun": "succeeded"})).into())
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for TerminalFailureRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        Err(PluginError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::RestateServiceUnregistered,
            "no deployment binds the child worker",
        )))
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for AlreadyStartedRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        *self.calls.lock_recover() += 1;
        Err(PluginError::ProcessAlreadyStarted {
            process_id: registration.id,
            by: Box::new(self.winner.clone()),
        })
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

pub(super) struct SegmentedRecordingRunner {
    pub(super) outcomes: Mutex<VecDeque<lash_core::ProcessRunOutcome>>,
    pub(super) handovers: Mutex<Vec<Option<lash_core::SegmentHandover>>>,
    pub(super) runs: AtomicUsize,
}

#[derive(Default)]
pub(super) struct CancellationAwareRunner {
    started: tokio::sync::Notify,
    finish_successfully: tokio::sync::Notify,
    failure_after_cancel: Option<&'static str>,
}

#[derive(Debug, Default)]
pub(super) struct BlockingCancelSignalTransport {
    requests: Mutex<Vec<HttpRequest>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Debug)]
pub(super) struct CeilingCancelWatchTransport {
    requests: AtomicUsize,
    attachment_started: tokio::sync::Semaphore,
    expire_attachment: tokio::sync::Semaphore,
}

impl Default for CeilingCancelWatchTransport {
    fn default() -> Self {
        Self {
            requests: AtomicUsize::new(0),
            attachment_started: tokio::sync::Semaphore::new(0),
            expire_attachment: tokio::sync::Semaphore::new(0),
        }
    }
}

impl CeilingCancelWatchTransport {
    async fn await_attachment(&self) {
        self.attachment_started
            .acquire()
            .await
            .expect("cancel watch transport remains open")
            .forget();
    }

    fn expire_attachment(&self) {
        self.expire_attachment.add_permits(1);
    }
}

#[async_trait::async_trait]
impl HttpTransport for CeilingCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.attachment_started.add_permits(1);
        self.expire_attachment
            .acquire()
            .await
            .expect("cancel watch transport remains open")
            .forget();
        Err(
            LlmTransportError::new("cancel watch attach ceiling elapsed")
                .with_kind(lash_core::ProviderFailureKind::Timeout)
                .with_code("timeout")
                .with_retry_verdict(
                    lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                ),
        )
    }
}

/// Answers every cancel-watch attach with Restate's 404 for a service no
/// registered deployment binds.
#[derive(Debug, Default)]
pub(super) struct UnregisteredCancelWatchTransport;

#[async_trait::async_trait]
impl HttpTransport for UnregisteredCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        Ok(HttpResponse {
            status: 404,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(
                r#"{"message":"Service 'LashProcessWorkflow' not found"}"#,
            ),
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct BrokenCancelWatchTransport;

#[async_trait::async_trait]
impl HttpTransport for BrokenCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        Err(LlmTransportError::new("cancel watch transport failed"))
    }
}

#[async_trait::async_trait]
impl HttpTransport for BlockingCancelSignalTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(r#""cancel_requested""#),
        })
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for CancellationAwareRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.started.notify_one();
        tokio::select! {
            _ = cancellation.cancelled() => match self.failure_after_cancel {
                Some(message) => Err(PluginError::Session(message.to_string())),
                None => Ok(process_cancellation("cancel signal observed", None).into()),
            },
            _ = self.finish_successfully.notified() =>
                Ok(process_success(serde_json::json!("runner completed")).into()),
        }
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for SegmentedRecordingRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.handovers.lock_recover().push(handover);
        self.outcomes
            .lock_recover()
            .pop_front()
            .ok_or_else(|| PluginError::Session("unexpected duplicate segment run".to_string()))
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

pub(super) fn native_process_scope(
    process_id: &ProcessId,
) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
        lash_core::ExecutionScope::process(process_id.to_string()),
    )
    .expect("native process scope")
}

pub(super) async fn pending_process_cancel_signal() -> Result<(), HandlerError> {
    std::future::pending().await
}

#[tokio::test]
pub(super) async fn running_process_cancel_uses_native_signal_without_poll_delay() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let signal_transport = Arc::new(BlockingCancelSignalTransport::default());
    let cancel_ingress = RestateIngressClient::new(RestateConnection::with_transport(
        "https://restate.invalid",
        signal_transport.clone(),
    ));
    let workflow = Arc::new(LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
        cancel_ingress,
    ));
    let registration = rerunnable_registration("prompt-cancel");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            let cancellation_signal =
                workflow.cancellation_signal(&ProcessId::from("prompt-cancel"), 0);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    native_process_scope(&ProcessId::from("prompt-cancel")),
                    0,
                    None,
                    cancellation_signal,
                )
                .await
        })
    };
    runner.started.notified().await;
    signal_transport.started.notified().await;
    registry
        .append_event(
            &ProcessId::from("prompt-cancel"),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from("prompt-cancel"),
                Some("stop promptly".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    signal_transport.release.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("native cancellation signal must not hang")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_cancellation(output.as_ref())
    ));
    let requests = signal_transport.requests.lock_recover();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://restate.invalid/LashProcessWorkflow/prompt-cancel/await_cancel"
    );
}

#[tokio::test]
pub(super) async fn cancellation_cleanup_failure_does_not_write_a_false_terminal() {
    let runner = Arc::new(CancellationAwareRunner {
        failure_after_cancel: Some("simulated durable child cleanup failure"),
        ..CancellationAwareRunner::default()
    });
    let registry = process_registry();
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    ));
    let registration = rerunnable_session_turn_registration("cancel-cleanup-failure");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    let (signal, cancellation_signal) = tokio::sync::oneshot::channel();
    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    native_process_scope(&ProcessId::from("cancel-cleanup-failure")),
                    0,
                    None,
                    async move {
                        cancellation_signal.await.map_err(|_| {
                            HandlerError::from(TerminalError::new(
                                "test cancellation signal sender dropped",
                            ))
                        })
                    },
                )
                .await
        })
    };
    runner.started.notified().await;
    registry
        .append_event(
            &ProcessId::from("cancel-cleanup-failure"),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from("cancel-cleanup-failure"),
                Some("exercise cleanup failure".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    signal.send(()).expect("resolve cancellation signal");

    let error = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("cleanup failure settles")
        .expect("join running process")
        .expect_err("cleanup failure must stay retryable");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    assert!(
        source
            .to_string()
            .contains("simulated durable child cleanup failure"),
        "unexpected handler error: {error:?}"
    );
    let record = registry
        .get_process(&ProcessId::from("cancel-cleanup-failure"))
        .await
        .expect("read process after cleanup failure")
        .expect("process remains registered");
    assert!(
        !record.is_terminal(),
        "a cancelled SessionTurn cleanup failure must not write a false terminal"
    );
}

#[tokio::test]
pub(super) async fn non_session_cancel_preserves_the_prior_cancelled_terminal_on_runner_failure() {
    let runner = Arc::new(CancellationAwareRunner {
        failure_after_cancel: Some("simulated non-session runner failure"),
        ..CancellationAwareRunner::default()
    });
    let registry = process_registry();
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    ));
    let registration = rerunnable_registration("non-session-cancel-failure");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    let (signal, cancellation_signal) = tokio::sync::oneshot::channel();
    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    native_process_scope(&ProcessId::from("non-session-cancel-failure")),
                    0,
                    None,
                    async move {
                        cancellation_signal.await.map_err(|_| {
                            HandlerError::from(TerminalError::new(
                                "test cancellation signal sender dropped",
                            ))
                        })
                    },
                )
                .await
        })
    };
    runner.started.notified().await;
    registry
        .append_event(
            &ProcessId::from("non-session-cancel-failure"),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from("non-session-cancel-failure"),
                Some("preserve prior non-session behavior".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    signal.send(()).expect("resolve cancellation signal");

    let outcome = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("non-session cancellation settles")
        .expect("join running process")
        .expect("non-session runner failure remains masked by cancellation");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_cancellation(output.as_ref())
    ));
    let record = registry
        .get_process(&ProcessId::from("non-session-cancel-failure"))
        .await
        .expect("read process")
        .expect("process remains registered");
    assert_eq!(record.status, lash_core::ProcessStatus::Cancelled);
}

#[tokio::test]
pub(super) async fn cancel_watch_reissues_after_attach_ceiling_until_segment_completes() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let transport = Arc::new(CeilingCancelWatchTransport::default());
    let connection = RestateConnection::with_transport_and_config(
        "https://restate.invalid",
        transport.clone(),
        short_restate_timeouts(100, 10),
    );
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
        RestateIngressClient::new(connection),
    );
    let registration = rerunnable_registration("ceiling-reissues");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    let finish = Arc::clone(&runner);
    let attachments = Arc::clone(&transport);
    let finisher = tokio::spawn(async move {
        for _ in 0..2 {
            attachments.await_attachment().await;
            attachments.expire_attachment();
        }
        attachments.await_attachment().await;
        finish.finish_successfully.notify_one();
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        workflow.run_registration(
            registration,
            ProcessExecutionContext::default(),
            native_process_scope(&ProcessId::from("ceiling-reissues")),
            0,
            None,
            workflow.cancellation_signal(&ProcessId::from("ceiling-reissues"), 0),
        ),
    )
    .await
    .expect("cancel watch must re-attach without hanging")
    .expect("attach ceiling expiry must not fail the segment");
    finisher.await.expect("finish segment");

    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_success(output.as_ref())
    ));
    assert_eq!(
        transport.requests.load(Ordering::SeqCst),
        3,
        "the cancel watch must re-attach across several ceilings"
    );
}

#[tokio::test]
pub(super) async fn non_timeout_cancel_watch_error_fails_the_segment() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        process_registry(),
        continuation_store(),
        RestateIngressClient::new(RestateConnection::with_transport(
            "https://restate.invalid",
            Arc::new(BrokenCancelWatchTransport),
        )),
    );

    let error = workflow
        .run_registration(
            rerunnable_registration("broken-cancel-watch"),
            ProcessExecutionContext::default(),
            native_process_scope(&ProcessId::from("broken-cancel-watch")),
            0,
            None,
            workflow.cancellation_signal(&ProcessId::from("broken-cancel-watch"), 0),
        )
        .await
        .expect_err("a non-timeout cancel watch failure must fail the segment");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();

    assert!(
        source.to_string().contains("cancel watch transport failed"),
        "unexpected handler error: {error:?}"
    );
    assert!(
        source.to_string().starts_with("Retryable error"),
        "a transport fault is worth retrying, which is the case the \
         missing-registration terminal below has to be distinguishable from: \
         {error:?}"
    );
}

/// A cancel watch addressed to a service no deployment binds ends the segment
/// with the engine's own 404-class **terminal**, not a retryable error the
/// engine backs off forever (FIG-1579).
///
/// The cancel watch is a `loop`, and every error in it that is not a timeout
/// leaves through one arm. Before this, a 404 left through the same arm as a
/// broken socket and became `HandlerErrorInner::Retryable`, so a deployment that
/// forgot to `bind(LashProcessWorkflowImpl…)` produced an invocation retrying
/// with infinite exponential backoff and no operator ever told what was wrong —
/// the engine-tier twin of the warn-and-strand this contract rules out. A
/// missing registration is deterministic: retrying cannot make it appear.
#[tokio::test]
pub(super) async fn an_unregistered_cancel_watch_service_is_a_terminal_not_an_indefinite_retry() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        process_registry(),
        continuation_store(),
        RestateIngressClient::new(RestateConnection::with_transport(
            "https://restate.invalid",
            Arc::new(UnregisteredCancelWatchTransport),
        )),
    );

    let error = workflow
        .run_registration(
            rerunnable_registration("unregistered-cancel-watch"),
            ProcessExecutionContext::default(),
            native_process_scope(&ProcessId::from("unregistered-cancel-watch")),
            0,
            None,
            workflow.cancellation_signal(&ProcessId::from("unregistered-cancel-watch"), 0),
        )
        .await
        .expect_err("a cancel watch against an unbound service must fail the segment");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    let rendered = source.to_string();

    assert!(
        rendered.starts_with("Terminal error [404]"),
        "a missing registration must leave as the engine's own terminal, so the \
         invocation ends instead of backing off forever: {error:?}"
    );
    assert!(
        rendered.contains("LashProcessWorkflow/await_cancel"),
        "the terminal names the address nothing binds, because `404 from \
         Restate` is not something an operator can act on: {error:?}"
    );
}

/// The classifier the terminal above turns on: a `404` is a missing registration
/// only on a route that addresses a service by name, and only for that status.
///
/// The negative half is the one that matters. `404` is not self-describing — on
/// a **control** route it names a missing *resource*, and most often an
/// invocation that is already gone: completed, killed, or aged out of retention.
/// A predicate that read the status alone would let a caller wired onto
/// `PATCH /invocations/{id}/cancel` terminalize real work over a cancel that
/// arrived one moment late, which is the opposite of the mistake this exists to
/// prevent. Routes therefore opt in by name, and an unclassified one keeps the
/// retryable handling every 404 had before.
#[test]
pub(super) fn only_a_404_on_a_service_call_route_reads_as_a_missing_registration() {
    let service_call = |operation| crate::RestateHttpError::Status {
        operation,
        url: "https://restate.invalid/LashProcessWorkflow/k/await_cancel".to_string(),
        status: 404,
        body: "not found".to_string(),
    };
    for operation in [
        "Restate workflow call",
        "Restate object call",
        "Restate /send",
    ] {
        assert!(
            service_call(operation).is_service_unregistered(),
            "`{operation}` addresses a service by name, so its 404 is a missing \
             registration"
        );
        assert!(!service_call(operation).is_timeout());
    }

    for operation in [
        "Restate invocation cancel",
        "Restate invocation kill",
        "Restate SQL query",
    ] {
        let control_route = crate::RestateHttpError::Status {
            operation,
            url: "https://restate.invalid/invocations/inv_1/cancel".to_string(),
            status: 404,
            body: "invocation not found".to_string(),
        };
        assert!(
            !control_route.is_service_unregistered(),
            "`{operation}` is a control route: its 404 names a resource that is \
             gone, and terminalizing on it would end real work over a cancel \
             that arrived late"
        );
    }

    let unavailable = crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/await_cancel".to_string(),
        status: 503,
        body: "overloaded".to_string(),
    };
    assert!(
        !unavailable.is_service_unregistered(),
        "a busy engine is worth retrying; only `no such service or handler` is not"
    );
}

#[tokio::test]
pub(super) async fn transient_cancel_registry_read_error_cannot_fall_through_to_success() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    ));
    let registration = rerunnable_registration("transient-cancel-read");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let (signal, cancellation_signal) = tokio::sync::oneshot::channel();
    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    native_process_scope(&ProcessId::from("transient-cancel-read")),
                    0,
                    None,
                    async move {
                        cancellation_signal.await.map_err(|_| {
                            HandlerError::from(TerminalError::new(
                                "test cancellation signal sender dropped",
                            ))
                        })
                    },
                )
                .await
        })
    };
    runner.started.notified().await;
    registry
        .append_event(
            &ProcessId::from("transient-cancel-read"),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from("transient-cancel-read"),
                Some("retry the durable read".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    workflow.fail_next_cancel_reads(1);
    signal
        .send(())
        .expect("resolve process cancellation signal");
    runner.finish_successfully.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("transient registry error must be retried")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_cancellation(output.as_ref())
    ));
}

#[tokio::test]
pub(super) async fn exhausted_cancel_confirmation_is_a_retryable_handler_error() {
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(CancellationAwareRunner::default()),
        process_registry(),
        continuation_store(),
    );
    workflow.fail_next_cancel_reads(6);

    let error = workflow
        .confirm_process_cancel_requested_for_test(&ProcessId::from("cancel-confirmation"))
        .await
        .expect_err("exhausted confirmation must stay retryable");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    let rendered = source.to_string();

    assert!(
        format!("{error:?}").contains("Retryable"),
        "unexpected handler error: {error:?}"
    );
    assert!(
        rendered.contains("simulated transient cancel registry read failure"),
        "unexpected handler error: {error:?}"
    );
}

#[tokio::test]
pub(super) async fn absent_event_after_cancel_promise_is_a_terminal_handler_error() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("missing-cancel-event"))
        .await
        .expect("register process");
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(CancellationAwareRunner::default()),
        registry,
        continuation_store(),
    );

    let error = workflow
        .confirm_process_cancel_requested_for_test(&ProcessId::from("missing-cancel-event"))
        .await
        .expect_err("a resolved promise without its durable event must be terminal");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();

    assert!(
        source.to_string().starts_with("Terminal error [500]:"),
        "unexpected handler error: {error:?}"
    );
}

#[tokio::test]
pub(super) async fn durable_segment_handover_resumes_once_and_terminalizes_once() {
    let continuation = lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash: "program-v1".to_string(),
        engine_state: vec![1, 2, 3],
    };
    let terminal = process_success(serde_json::json!({"result": 42}));
    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([
            lash_core::ProcessRunOutcome::SegmentBoundary(continuation.clone()),
            terminal.clone().into(),
        ])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let (registry, continuations) = process_stores();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        Arc::clone(&continuations),
    );
    let registration = rerunnable_registration("segmented-durable");
    let _record = registry
        .register_process(registration.clone())
        .await
        .expect("register segmented process");
    let first_context = Arc::new(ReplayableRecordingContext::default());
    let first_controller = RestateRuntimeEffectController::new(first_context.clone());

    let first = workflow
        .run_registration(
            registration.clone(),
            ProcessExecutionContext::default(),
            first_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable first-segment scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run first segment");
    let lash_core::ProcessRunOutcome::SegmentBoundary(first_handover) = first else {
        panic!("first incarnation must end at a segment boundary");
    };
    let persisted = lash_core::PersistedSegmentHandover {
        segment_ordinal: 1,
        handover: first_handover,
    };
    continuations
        .put_segment_handover(&ProcessId::from("segmented-durable"), persisted.clone())
        .await
        .expect("persist before successor schedule");
    continuations
        .put_segment_handover(&ProcessId::from("segmented-durable"), persisted.clone())
        .await
        .expect("crash recovery repeats the persist idempotently");
    assert_eq!(
        process_segment_workflow_key(&ProcessId::from("segmented-durable"), 1),
        "segmented-durable#1"
    );

    let loaded = continuations
        .get_segment_handover(&ProcessId::from("segmented-durable"), 1)
        .await
        .expect("load successor handover")
        .expect("persisted successor handover");
    let resumed = loaded.handover;
    first_context.start_replay();
    let successor_context = Arc::new(ReplayableRecordingContext::default());
    let successor_controller = RestateRuntimeEffectController::new(successor_context);
    let second = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            successor_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable successor scope"),
            1,
            Some(resumed),
            pending_process_cancel_signal(),
        )
        .await
        .expect("run successor segment");
    assert!(matches!(
        second,
        lash_core::ProcessRunOutcome::Terminal { .. }
    ));
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        runner.handovers.lock_recover().as_slice(),
        &[None, Some(continuation)]
    );
    let events = registry
        .events_after(&ProcessId::from("segmented-durable"), 0)
        .await
        .expect("process events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1,
        "only the true terminal is process-visible"
    );
    let awaited = lash_core::NativeProcessWork::for_registry(registry)
        .await_terminal(&ProcessId::from("segmented-durable"))
        .await
        .expect("await true terminal");
    assert_eq!(awaited, terminal);
}

#[derive(Clone, Copy, Debug)]
pub(super) enum RestateSegmentReplayPoint {
    GetHandover,
    PutHandover,
    CancelCheck,
}

pub(super) fn restate_segment_tool_attempt_outcome(ordinal: u64) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(completed_tool_record(
                &format!("matrix-call-{ordinal}"),
                "matrix_tool",
            )),
            intents: lash_core::ToolIntents::default(),
        }),
        triggers: Vec::new(),
    }
}

#[tokio::test]
pub(super) async fn restate_segment_transition_replay_matrix_preserves_lineage_invariants() {
    for replay_point in [
        RestateSegmentReplayPoint::GetHandover,
        RestateSegmentReplayPoint::PutHandover,
        RestateSegmentReplayPoint::CancelCheck,
    ] {
        let process_id = format!("matrix-{replay_point:?}").to_ascii_lowercase();
        let (registry, continuations) = process_stores();
        let registration = rerunnable_registration(&process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register matrix process");
        let terminal = process_success(serde_json::json!({"result": 99, "effects": [0, 1, 2]}));
        let runner = Arc::new(SegmentedRecordingRunner {
            outcomes: Mutex::new(VecDeque::from([
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "matrix-program-v1".to_string(),
                    engine_state: vec![1],
                }),
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "matrix-program-v1".to_string(),
                    engine_state: vec![2],
                }),
                terminal.clone().into(),
            ])),
            handovers: Mutex::new(Vec::new()),
            runs: AtomicUsize::new(0),
        });
        let workflow = LashProcessWorkflowImpl::new_for_test(
            Arc::clone(&runner),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        );
        let mut input_handover = None;
        let mut successor_keys = HashSet::new();

        for ordinal in 0_u64..3 {
            let context = Arc::new(ReplayableRecordingContext::default());
            let controller = RestateRuntimeEffectController::with_options(
                Arc::clone(&context),
                RestateEffectControllerOptions::default().segment_effect_budget(1),
            );
            let local_calls = Arc::new(AtomicUsize::new(0));
            let envelope = RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(
                        ExecutionScope::process(process_id.clone()),
                        format!("matrix:{process_id}:{ordinal}"),
                    )
                    .expect("valid process matrix effect address"),
                    lash_core::RuntimeAttribution::none(),
                    format!("matrix-effect-{ordinal}"),
                ),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call_with(&format!("matrix-call-{ordinal}"), "matrix_tool"),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            );
            let first_calls = Arc::clone(&local_calls);
            let first_effect = controller
                .execute_effect(
                    envelope.clone(),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        first_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("first matrix effect");
            let progress = lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: None,
            };
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget)
            );
            context.start_replay();
            let replay_calls = Arc::clone(&local_calls);
            let replay_effect = controller
                .execute_effect(
                    envelope,
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        replay_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("replayed matrix effect");
            assert_eq!(
                serde_json::to_value(&replay_effect).expect("serialize replay effect"),
                serde_json::to_value(&first_effect).expect("serialize first effect"),
                "replay effect identity"
            );
            assert_eq!(
                local_calls.load(Ordering::SeqCst),
                1,
                "handler replay must not double-execute an effect"
            );
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget),
                "replay must cut at the identical completed-effect budget"
            );

            if ordinal > 0 {
                let loaded = continuations
                    .get_segment_handover(&ProcessId::from(process_id.clone()), ordinal)
                    .await
                    .expect("get input handover")
                    .expect("running segment input survives");
                if matches!(replay_point, RestateSegmentReplayPoint::GetHandover) {
                    assert_eq!(
                        continuations
                            .get_segment_handover(&ProcessId::from(process_id.clone()), ordinal)
                            .await
                            .expect("replayed get"),
                        Some(loaded.clone())
                    );
                }
                input_handover = Some(loaded.handover);
            }

            let run_once = workflow
                .run_registration(
                    registration.clone(),
                    ProcessExecutionContext::default(),
                    native_process_scope(&ProcessId::from(process_id.clone())),
                    ordinal,
                    input_handover.take(),
                    pending_process_cancel_signal(),
                )
                .await
                .expect("matrix segment run");
            if ordinal == 2 {
                assert_eq!(run_once, terminal.clone().into());
                break;
            }
            let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = run_once else {
                panic!("matrix segment {ordinal} must request a boundary");
            };
            let next = ordinal + 1;
            let persisted = lash_core::PersistedSegmentHandover {
                segment_ordinal: next,
                handover: boundary,
            };
            continuations
                .put_segment_handover(&ProcessId::from(process_id.clone()), persisted.clone())
                .await
                .expect("put matrix handover");
            if matches!(replay_point, RestateSegmentReplayPoint::PutHandover) {
                continuations
                    .put_segment_handover(&ProcessId::from(process_id.clone()), persisted)
                    .await
                    .expect("replayed put is idempotent");
            }
            let cancel_checks = if matches!(replay_point, RestateSegmentReplayPoint::CancelCheck) {
                2
            } else {
                1
            };
            for _ in 0..cancel_checks {
                assert!(
                    !workflow
                        .process_cancel_requested(&ProcessId::from(process_id.clone()))
                        .await
                        .expect("matrix cancel check")
                );
            }
            let key = process_segment_workflow_key(&ProcessId::from(process_id.clone()), next);
            assert!(
                successor_keys.insert(key.clone()),
                "one successor per ordinal"
            );
        }

        assert_eq!(
            successor_keys.len(),
            2,
            "exactly one successor for each boundary"
        );
        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            3,
            "no duplicate incarnation"
        );
        registry
            .complete_process(
                &ProcessId::from(process_id.clone()),
                terminal.clone(),
                workflow_key_authority(&ProcessId::from(process_id.clone())),
            )
            .await
            .expect("write root terminal");
        let attach_after_retention =
            lash_core::NativeProcessWork::for_registry(Arc::clone(&registry));
        assert_eq!(
            attach_after_retention
                .await_terminal(&ProcessId::from(process_id.clone()))
                .await
                .expect("post-retention durable attach"),
            terminal
        );
        assert_eq!(
            registry
                .events_after(&ProcessId::from(process_id), 0)
                .await
                .expect("matrix events")
                .iter()
                .filter(|event| event.semantics.terminal.is_some())
                .count(),
            1,
            "root terminal is durable and exactly once"
        );
    }
}
