use super::*;

// No store-family macros: Restate certifies an engine adapter over borrowed memory/SQLite stores.
// No SQL-journal retirement/fencing macros: replay lives in workflow history, not SQL rows.
// No store effect-group drain macro: queued durable drain is a storage-side protocol.

fn operation_effect_invocation(
    operation_id: impl Into<String>,
    attribution: lash_core::RuntimeAttribution,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::runtime_operation(operation_id.into()),
            replay_key,
        )
        .expect("valid test runtime-operation effect address"),
        attribution,
        effect_id,
    )
}

fn turn_effect_invocation(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(ExecutionScope::turn(session_id, turn_id), replay_key)
            .expect("valid test turn effect address"),
        lash_core::RuntimeAttribution::for_turn(
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        ),
        effect_id,
    )
}

lash_conformance::turn_work_driver_tests!({
    let context = Arc::new(RecordingContext::default());
    let registration_context = Arc::clone(&context);
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new(context));
    ((), host, move |_host, session_id, key| async move {
        registration_context
            .wait_for_await_event_registration(&session_id, &key)
            .await;
    })
});

pub(super) fn replayable_conformance_invocation(
    context: Arc<ReplayableRecordingContext>,
) -> lash_conformance::ConformanceInvocation {
    let controller: Arc<dyn RuntimeEffectController> =
        Arc::new(RestateRuntimeEffectController::new(Arc::clone(&context)));
    lash_conformance::ConformanceInvocation::new(
        controller,
        ExecutionScope::runtime_operation("restate-replay-conformance"),
        lash_conformance::ConformanceEffectRedrive::ReplaysJournal,
        || {},
        move || {
            context.start_replay();
            Arc::new(RestateRuntimeEffectController::new(Arc::clone(&context)))
                as Arc<dyn RuntimeEffectController>
        },
    )
}

pub(super) fn crash_redrive_conformance_invocation(
    _scenario: &str,
) -> lash_conformance::ConformanceInvocation {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller: Arc<dyn RuntimeEffectController> =
        Arc::new(RestateRuntimeEffectController::new(Arc::clone(&context)));
    lash_conformance::ConformanceInvocation::new(
        controller,
        ExecutionScope::runtime_operation("restate-crash-redrive-conformance"),
        lash_conformance::ConformanceEffectRedrive::ReplaysJournal,
        || {},
        move || {
            context.start_replay_allowing_journal_extension();
            Arc::new(RestateRuntimeEffectController::new(Arc::clone(&context)))
                as Arc<dyn RuntimeEffectController>
        },
    )
}

#[derive(Debug)]
pub(super) struct ConformanceProcessWaitTransport {
    terminal: ProcessAwaitOutput,
    request_urls: Mutex<Vec<String>>,
}

impl ConformanceProcessWaitTransport {
    fn new(terminal: ProcessAwaitOutput) -> Self {
        Self {
            terminal,
            request_urls: Mutex::new(Vec::new()),
        }
    }

    fn assert_reattached_to(&self, process_id: &ProcessId) {
        let requests = self.request_urls.lock_recover();
        assert_eq!(requests.len(), 2, "Restate process wait must reattach once");
        let expected = format!("/LashProcessWorkflow/{process_id}/await_terminal");
        assert!(
            requests.iter().all(|url| url.ends_with(&expected)),
            "every Restate attachment must retain the same process id: {requests:?}"
        );
    }

    async fn wait_for_bounded_reattachment(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.request_urls.lock_recover().len() >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Restate must begin its reattachment before terminal completion");
    }
}

#[async_trait::async_trait]
impl HttpTransport for ConformanceProcessWaitTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let request_index = {
            let mut requests = self.request_urls.lock_recover();
            let index = requests.len();
            requests.push(request.url);
            index
        };
        match request_index {
            0 => Err(
                LlmTransportError::new("conformance attachment ceiling elapsed")
                    .with_kind(lash_core::ProviderFailureKind::Timeout)
                    .with_code("timeout")
                    .with_retry_verdict(
                        lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                    ),
            ),
            1 => Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: HttpResponseBody::buffered(
                    serde_json::to_string(&self.terminal)
                        .expect("serialize conformance process terminal"),
                ),
            }),
            _ => Err(LlmTransportError::new(
                "conformance process wait exceeded one reattachment",
            )),
        }
    }
}

pub(super) fn conformance_restate_process_work(
    registry: Arc<dyn ProcessRegistry>,
    terminal: ProcessAwaitOutput,
) -> (
    Arc<dyn lash_core::ProcessWorkSubstrate>,
    Arc<ConformanceProcessWaitTransport>,
) {
    let transport = Arc::new(ConformanceProcessWaitTransport::new(terminal));
    let connection = RestateConnection::with_transport(
        "https://conformance.restate.invalid",
        Arc::clone(&transport) as Arc<dyn HttpTransport>,
    );
    let process_work = Arc::new(RestateProcessIngressRunner::new(
        connection,
        registry,
        continuation_store(),
    )) as Arc<dyn lash_core::ProcessWorkSubstrate>;
    (process_work, transport)
}

lash_conformance::effect_controller_replay_tests!(
    {
        let context = Arc::new(ReplayableRecordingContext::default());
        let make_context = Arc::clone(&context);
        (context, move || {
            replayable_conformance_invocation(Arc::clone(&make_context))
        })
    },
    |law: &str, context: &Arc<ReplayableRecordingContext>| {
        let runs = context.runs();
        match law {
            "effect-controller-concurrent-replay" => {
                assert_eq!(runs.len(), 4);
                assert!(runs.iter().any(|name| name.ends_with(":effect-slow")));
                assert!(runs.iter().any(|name| name.ends_with(":effect-fast")));
            }
            "effect-controller-tool-attempt-fanout" => {
                assert_eq!(runs.len(), 4);
                assert!(runs.iter().any(|name| name.ends_with(":tool-attempt-slow")));
                assert!(runs.iter().any(|name| name.ends_with(":tool-attempt-fast")));
            }
            "effect-controller-journaled-replay" => {}
            unknown => panic!("unexpected Restate replay conformance law: {unknown}"),
        }
    }
);

lash_conformance::effect_controller_replay_mismatch_tests!({
    let context = Arc::new(ReplayableRecordingContext::default());
    let make_context = Arc::clone(&context);
    (
        context,
        move || replayable_conformance_invocation(Arc::clone(&make_context)),
        "worker_replacement_abort",
    )
});

lash_conformance::durable_queued_drain_wait_resolver_tests!({
    (
        (),
        || {
            Arc::new(RestateRuntimeEffectController::new(Arc::new(
                RecordingContext::default(),
            ))) as Arc<dyn lash_core::AwaitEventResolver>
        },
        || {
            Arc::new(RestateEffectHost::new("http://127.0.0.1:8080"))
                as Arc<dyn lash_core::AwaitEventResolver>
        },
    )
});

lash_conformance::signal_intent_tests!({
    let context = Arc::new(RecordingContext::default());
    let effect_host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new(context));
    let registry =
        Arc::new(lash_core::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
        serde_json::json!({"signal": "observed"}),
    ));
    let (process_work, wait_transport) =
        conformance_restate_process_work(Arc::clone(&registry), terminal);
    let verify_transport = Arc::clone(&wait_transport);
    (
        wait_transport,
        "restate-public-signal-intent",
        effect_host,
        registry,
        process_work,
        move || async move {
            verify_transport
                .assert_reattached_to(&ProcessId::from("restate-public-signal-intent-target"));
        },
    )
});

lash_conformance::wake_delivery_ordering_tests!({
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
        serde_json::json!({"terminal_wait": "observed"}),
    ));
    let (process_work, wait_transport) = conformance_restate_process_work(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        terminal,
    );
    let verify_transport = Arc::clone(&wait_transport);
    let barrier_transport = Arc::clone(&wait_transport);
    (
        wait_transport,
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        registry as Arc<dyn lash_conformance::WakeDeliveryOrderingGroupFaultInjector>,
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Reattach,
        move || async move {
            barrier_transport.wait_for_bounded_reattachment().await;
        },
        move || async move {
            verify_transport.assert_reattached_to(&ProcessId::from("wake-ordering-terminal"));
        },
    )
});

lash_conformance::wake_delivery_crash_tests!({
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let registry = Arc::new(
        lash_core::TestLocalProcessRegistry::default()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .with_wake_delivery_config(
                lash_core::WakeDeliveryConfig::new(10_000)
                    .expect("valid Restate conformance wake expiry")
                    .with_enqueuing_stale_after_ms(25)
                    .expect("valid Restate conformance stale-claim age"),
            ),
    );
    let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
        serde_json::json!({"terminal_wait": "observed"}),
    ));
    let (process_work, wait_transport) = conformance_restate_process_work(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        terminal,
    );
    let factory = Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::with_clock(
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>
        ),
    );
    let verify_transport = Arc::clone(&wait_transport);
    let barrier_transport = Arc::clone(&wait_transport);
    (
        wait_transport,
        factory,
        registry as Arc<dyn lash_core::ConformanceProcessRegistry>,
        clock,
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Reattach,
        move || async move {
            barrier_transport.wait_for_bounded_reattachment().await;
        },
        move || async move {
            verify_transport.assert_reattached_to(&ProcessId::from("wake-crash-terminal"));
        },
    )
});

lash_conformance::turn_crash_matrix_tests!({
    let dir = tempfile::tempdir().expect("Restate turn-crash conformance tempdir");
    let root = dir.path().to_path_buf();
    (
        dir,
        move |scenario: &str| {
            let path = root.join(format!("restate-turn-crash-{scenario}.db"));
            sync_await(async move {
                Arc::new(
                    lash_sqlite_store::Store::open(&path)
                        .await
                        .expect("open Restate turn-crash SQLite state carrier"),
                ) as Arc<dyn lash_core::RuntimePersistence>
            })
        },
        crash_redrive_conformance_invocation,
    )
});

lash_conformance::effect_group_host_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = effect_group_conformance::LiveConformanceHarness::start().await;
        let factory = harness.group_host_factory();
        (harness, factory)
    }
);

lash_conformance::effect_group_cancelled_child_terminal_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = effect_group_conformance::LiveConformanceHarness::start().await;
        let factory = harness.group_host_factory();
        (harness, factory)
    }
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
async fn live_restate_effect_group_design_witnesses() {
    let harness = effect_group_conformance::LiveConformanceHarness::start().await;
    tokio::time::timeout(Duration::from_secs(240), harness.run_design_witnesses())
        .await
        .expect("Restate design witnesses exceeded 240 seconds");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
async fn live_restate_executing_effect_quiescence_witness() {
    let harness = effect_group_conformance::LiveConformanceHarness::start().await;
    tokio::time::timeout(
        Duration::from_secs(240),
        harness.run_executing_effect_quiescence_witness(),
    )
    .await
    .expect("Restate quiescence witness exceeded 240 seconds");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
async fn live_restate_cold_reopen_witnesses() {
    let harness = effect_group_conformance::LiveConformanceHarness::start().await;
    tokio::time::timeout(
        Duration::from_secs(240),
        harness.run_cold_reopen_witnesses(),
    )
    .await
    .expect("Restate cold-reopen witnesses exceeded 240 seconds");
    harness.finish().await;
}

lash_conformance::effect_host_await_event_witness_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = Arc::new(effect_group_conformance::LiveConformanceHarness::start().await);
        let make = harness.effect_host_factory();
        let witness_harness = Arc::clone(&harness);
        let teardown_harness = Arc::clone(&harness);
        (
            harness,
            Duration::from_secs(240),
            make,
            move |host, assert_retirement| async move {
                witness_harness
                    .run_active_wait_registration_witnesses(host, assert_retirement)
                    .await;
            },
            async move { teardown_harness.finish().await },
        )
    }
);

#[tokio::test]
pub(super) async fn durable_trace_reemits_on_redrive_without_adding_a_journal_command() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().segment_effect_budget(1),
    )
    .with_trace_sink_and_context(
        sink_dyn,
        lash_trace::TraceContext {
            run_id: Some("restate-host-run".to_string()),
            ..lash_trace::TraceContext::default()
        },
    );
    let envelope = RuntimeEffectEnvelope::new(
        operation_effect_invocation(
            "trace-replay-session",
            lash_core::RuntimeAttribution::for_session("trace-replay-session"),
            "trace-replay-tool",
            "trace-replay-tool",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call_with("trace-replay-call", "trace_replay_tool"),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_calls = Arc::new(AtomicUsize::new(0));

    let first_calls = Arc::clone(&local_calls);
    controller
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                first_calls.fetch_add(1, Ordering::SeqCst);
                Ok(restate_segment_tool_attempt_outcome(0))
            }),
        )
        .await
        .expect("live journaled effect");
    context.start_replay();
    let replay_calls = Arc::clone(&local_calls);
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                replay_calls.fetch_add(1, Ordering::SeqCst);
                Ok(restate_segment_tool_attempt_outcome(0))
            }),
        )
        .await
        .expect("replayed journaled effect");
    assert_eq!(
        RuntimeEffectController::wants_segment_boundary(
            &controller,
            &lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: Some(128),
            },
        ),
        Some(lash_core::BoundaryReason::JournalBudget)
    );

    assert_eq!(
        local_calls.load(Ordering::SeqCst),
        1,
        "redrive reuses the journaled outcome"
    );
    assert_eq!(
        context.runs().len(),
        2,
        "each handler pass issues only the effect's ctx.run; trace append adds no command"
    );
    let events = sink
        .records
        .lock_recover()
        .iter()
        .map(|record| record.event.kind())
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        vec![
            "journaled_effect_started",
            "journaled_effect_settled",
            "journaled_effect_started",
            "journaled_effect_settled",
            "durable_segment_boundary",
        ],
        "redrive repetition is the benign live-observation class"
    );
    let records = sink.records.lock_recover();
    assert!(records.iter().all(|record| {
        record.context.run_id.as_deref() == Some("restate-host-run")
            && record.context.session_id.as_deref() == Some("trace-replay-session")
    }));
    assert_eq!(
        records
            .last()
            .and_then(|record| record.context.effect_id.as_deref()),
        Some("trace-replay-tool"),
        "segment boundaries retain the scope of the effect that crossed the budget"
    );
}

#[tokio::test]
pub(super) async fn restate_handler_controller_journals_typed_trigger_execution() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let envelope = RuntimeEffectEnvelope::new(
        operation_effect_invocation(
            "restate-trigger-session",
            lash_core::RuntimeAttribution::for_session("restate-trigger-session"),
            "restate-trigger-list",
            "restate-trigger-list",
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session("restate-trigger-session"),
                filter: lash_core::TriggerSubscriptionFilter::default(),
            }),
        },
    );
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;

    let outcome = controller
        .execute_effect(envelope, RuntimeEffectLocalExecutor::triggers(store))
        .await
        .expect("handler-scoped Restate controller must execute typed trigger effects")
        .into_trigger()
        .expect("typed trigger outcome");

    assert!(matches!(
        outcome,
        Ok(lash_core::TriggerCommandOutcome::List { records }) if records.is_empty()
    ));
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-trigger-list"]
    );
}

pub(super) fn fig1464_poison_list_envelope(session: &str, effect: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        operation_effect_invocation(
            session,
            lash_core::RuntimeAttribution::for_session(session),
            effect,
            effect,
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session(session),
                filter: lash_core::TriggerSubscriptionFilter::default(),
            }),
        },
    )
}

/// FIG-1464 poison path: an effect outcome the durable journal will refuse
/// would fail every redrive of the enclosing turn identically, leaving the turn
/// uncommitted forever. The seam decides that verdict itself and gives up with a
/// typed terminal failure the host can observe, while still journaling the
/// effect exactly once so replay reproduces the same give-up.
#[tokio::test]
pub(super) async fn fig1464_unjournalable_effect_outcome_gives_up_with_a_typed_terminal_failure() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    store
        .execute_command(
            "fig1464-poison-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::session("restate-poison-session"),
                actor: lash_core::ProcessOriginator::host_scoped("fig1464"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "fig1464/poison-subscription",
                    lash_core::ProcessExecutionEnvRef::new("process-env:fig1464"),
                    "ui.button.pressed",
                    source_key,
                    ProcessInput::Engine {
                        kind: "fig1464-engine".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("fig1464-engine"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("seed a subscription so the listed outcome outgrows the budget")
        .expect("trigger registration outcome");

    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // Sits above the poison substitute (envelope plus a fixed-length typed
        // message) and below the listed subscription record.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(700),
    );

    let error = controller
        .execute_effect(
            fig1464_poison_list_envelope("restate-poison-session", "restate-poison-list"),
            RuntimeEffectLocalExecutor::triggers(store as Arc<dyn lash_core::TriggerStore>),
        )
        .await
        .expect_err("an unjournalable effect outcome must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        error.code.is_terminal(),
        "the give-up must not be re-attempted"
    );
    assert!(
        error.message.contains("durable journal budget"),
        "the typed failure must name why the effect gave up: {}",
        error.message
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-poison-list"],
        "the give-up is journaled once, so replay reproduces it"
    );
}

/// FIG-1464: the poison substitute still carries the envelope replay validation
/// matches on, so an envelope that is itself over budget leaves no journalable
/// record at all. Journaling the substitute anyway would propose an entry the
/// engine rejects, reviving the redrive loop with the give-up now silent. The
/// seam decides before it pays for the effect, and occupies the journal slot with
/// the fixed-size poison entry so the journal shape does not depend on the
/// configured budget.
#[tokio::test]
pub(super) async fn fig1464_over_budget_envelope_gives_up_with_a_fixed_size_poison_entry() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    );
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;

    let error = controller
        .execute_effect(
            fig1464_poison_list_envelope("restate-wide-envelope-session", "restate-wide-envelope"),
            RuntimeEffectLocalExecutor::triggers(store),
        )
        .await
        .expect_err("an unjournalable envelope must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        error.code.is_terminal(),
        "the give-up must not be re-attempted"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-wide-envelope"],
        "the give-up must occupy its journal slot exactly once"
    );
}

/// FIG-1464: the tool-batch and durable-process-command sites run their effect
/// outside the run closure, so the budget give-up has to be their pre-flight
/// gate. A give-up decided after the batch ran would discard a settled batch
/// with no journal entry, and the next redrive would execute every child again.
#[tokio::test]
pub(super) async fn fig1464_over_budget_tool_batch_gives_up_before_running_the_batch() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    );
    let executed = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&executed);

    let error = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::ToolBatch, "fig1464-over-budget-batch"),
                RuntimeEffectCommand::ToolBatch {
                    batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
                },
            ),
            RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                ran.store(true, Ordering::SeqCst);
                Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
                    "an over-budget tool batch must never run",
                ))
            }),
        )
        .await
        .expect_err("an unjournalable envelope must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "the give-up must be decided before the batch runs"
    );
    assert_eq!(
        context.runs.lock_recover().len(),
        1,
        "the give-up must occupy its journal slot exactly once"
    );
}

/// FIG-1464 deciding risk: the give-up verdict reads a process-configured
/// budget, so a budget change between attempts must not flip the *shape* of the
/// journal. The give-up occupies its slot with a fixed-size poison entry, so a
/// larger budget on redrive consumes the same slot and observes the same typed
/// failure instead of diverging by proposing a record where the first attempt
/// proposed nothing.
#[tokio::test]
pub(super) async fn fig1464_over_budget_give_up_replays_identically_under_a_larger_budget() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;
    let envelope =
        || fig1464_poison_list_envelope("restate-budget-flip-session", "restate-budget-flip");

    let recorded = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        envelope(),
        RuntimeEffectLocalExecutor::triggers(Arc::clone(&store)),
    )
    .await
    .expect_err("the over-budget envelope must give up");
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-budget-flip"],
        "the give-up must occupy its journal slot"
    );

    context.replaying.store(true, Ordering::SeqCst);
    let replayed = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // The budget the redrive was configured with now clears the envelope, so
        // an un-journaled give-up would have journaled a record here.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(envelope(), RuntimeEffectLocalExecutor::triggers(store))
    .await
    .expect_err("replaying the poison entry must reproduce the give-up");

    assert_eq!(replayed.code, recorded.code);
    assert_eq!(
        replayed.message, recorded.message,
        "the replayed give-up must render the journaled verdict, not the new budget"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-budget-flip", "lash:restate-budget-flip"],
        "the redrive must consume the same journal slot, not add one"
    );
}

/// FIG-1464 round 3, residual B: at the tool-batch and process-command sites the
/// effect runs outside the run closure, so nothing but this seam's own journal
/// slot can stop a replay from running it again. Re-deciding the give-up from
/// live config was not enough: a redrive configured with a larger budget cleared
/// the envelope, ran the batch, and only then replayed the poison entry and threw
/// the settled batch away - an at-least-once execution of every child. The
/// verdict is journaled ahead of the batch, so the journaled verdict is what
/// decides on replay and the batch never runs.
#[tokio::test]
pub(super) async fn fig1464_over_budget_tool_batch_replay_under_a_larger_budget_never_runs_the_batch()
 {
    let context = Arc::new(ReplayableRecordingContext::default());
    let batch_envelope = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::ToolBatch, "fig1464-budget-flip-batch"),
            RuntimeEffectCommand::ToolBatch {
                batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
            },
        )
    };
    let never_runs = || {
        RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            panic!("a batch whose give-up is already journaled must never run");
        })
    };

    let recorded = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(batch_envelope(), never_runs())
    .await
    .expect_err("the over-budget batch must give up before running");

    context.replaying.store(true, Ordering::SeqCst);
    let replayed = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // Big enough that a give-up re-decided from live config would proceed,
        // run the batch, and only then meet the journaled give-up.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(batch_envelope(), never_runs())
    .await
    .expect_err("the journaled verdict must still give up");

    assert_eq!(
        replayed.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert_eq!(
        replayed.message, recorded.message,
        "the replayed give-up must render the journaled verdict, not the new budget"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:session:turn:1:0:tool_batch:fig1464-budget-flip-batch.journal-budget",
            "lash:session:turn:1:0:tool_batch:fig1464-budget-flip-batch.journal-budget"
        ],
        "the redrive must consume the same verdict slot and add none"
    );
}

/// FIG-1767: both eager effect arms (durable process command and durable tool batch)
/// emit byte-identical journal records before and after collapsing into the shared helper.
#[tokio::test]
pub(super) async fn fig1767_journal_entry_byte_sequence_equality() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    );

    // Arm 1: Durable Process Command
    let process_invocation = turn_effect_invocation(
        "fig1767-session",
        "fig1767-turn",
        1,
        0,
        "fig1767-process-cmd",
        "fig1767-process-cmd",
    );
    let process_envelope = RuntimeEffectEnvelope::new(
        process_invocation,
        RuntimeEffectCommand::Process {
            command: Box::new(ProcessCommand::ParentEnd {
                identity: lash_core::ToolIntentIdentity {
                    session_id: SessionId::from("fig1767"),
                    execution_scope_id: "scope".to_string(),
                    tool_call_id: "call".to_string(),
                    intent_index: 0,
                    replay_key: "key".to_string(),
                    minting_emission_replay_key: None,
                },
                process_id: ProcessId::from("fig1767-proc"),
                policy: lash_core::ProcessParentEndPolicy::Cancel,
            }),
        },
    );
    controller
        .execute_effect(
            process_envelope.clone(),
            registry_local_executor(process_registry()),
        )
        .await
        .expect("process command effect execution");

    // Retrieve records produced for DurableProcessCommand. These bytes are the
    // golden serialization captured from main before the helper extraction.

    {
        let process_verdict_key = "lash:fig1767-process-cmd.journal-budget";
        let parent_end_decision_key = "lash:fig1767-process-cmd.parent-end-cancel-decision:v1";
        let process_record_key = "lash:fig1767-process-cmd";

        let records = context.records.lock_recover();
        let process_verdict_bytes = records
            .get(process_verdict_key)
            .expect("process budget verdict journal entry");
        let parent_end_decision_bytes = records
            .get(parent_end_decision_key)
            .expect("parent-end cancellation decision journal entry");
        let process_record_bytes = records
            .get(process_record_key)
            .expect("process effect record journal entry");

        let process_record: serde_json::Value =
            serde_json::from_slice(process_record_bytes).expect("decode process effect record");
        let refusal = &process_record["outcome"]["Ok"]["result"]["outcome"];
        assert_eq!(refusal["process_id"], "fig1767-proc");
        assert_eq!(
            refusal["message"],
            lash_core::PluginError::ProcessUnknown {
                process_id: ProcessId::from("fig1767-proc"),
            }
            .to_string()
        );

        // Pin verdict entry byte sequence: JournaledBudgetVerdict::Proceed serializes as "Proceed"
        assert_eq!(
            process_verdict_bytes.as_slice(),
            b"\"Proceed\"",
            "process command budget verdict byte sequence mismatch"
        );
        assert_eq!(
            parent_end_decision_bytes.as_slice(),
            br##"{"Ok":{"identity":{"policy":"cancel","process_id":"fig1767-proc","tool_intent_identity":{"execution_scope_id":"scope","intent_index":0,"replay_key":"key","session_id":"fig1767","tool_call_id":"call"}},"result":{"Err":{"message":{"process_id":"fig1767-proc"},"type":"process_unknown"}},"version":1}}"##,
            "parent-end cancellation decision byte sequence mismatch"
        );
        assert_eq!(
            process_record_bytes,
            br##"{"envelope":{"json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\"},\"replay_key\":\"fig1767-process-cmd\"},\"effect_id\":\"fig1767-process-cmd\",\"attribution\":{\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\",\"turn_index\":1,\"protocol_iteration\":0}},\"command\":{\"type\":\"process\",\"command\":{\"op\":\"parent_end\",\"identity\":{\"session_id\":\"fig1767\",\"execution_scope_id\":\"scope\",\"tool_call_id\":\"call\",\"intent_index\":0,\"replay_key\":\"key\"},\"process_id\":\"fig1767-proc\",\"policy\":\"cancel\"}}}","hash":"a5dc0aa07d15348d4931a296d8ea95daa3aad6d2c0cc8d5f886e75fcf40d2232"},"outcome":{"Ok":{"type":"process","result":{"op":"parent_end","outcome":{"status":"refused","identity":{"session_id":"fig1767","execution_scope_id":"scope","tool_call_id":"call","intent_index":0,"replay_key":"key"},"process_id":"fig1767-proc","code":"plugin","message":"unknown process `fig1767-proc`"}}}}}"##,
            "process command recorded effect golden bytes changed"
        );
    }

    // Arm 2: Durable Tool Batch
    let batch_invocation = turn_effect_invocation(
        "fig1767-session",
        "fig1767-turn",
        1,
        0,
        "fig1767-tool-batch",
        "fig1767-tool-batch",
    );
    let batch_envelope = RuntimeEffectEnvelope::new(
        batch_invocation,
        RuntimeEffectCommand::ToolBatch {
            batch: lash_core::PreparedToolBatch::new("fig1767-batch", vec![prepared_tool_call()]),
        },
    );
    controller
        .execute_effect(
            batch_envelope.clone(),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(RuntimeEffectOutcome::ToolBatch {
                    launches: vec![],
                    triggers: vec![],
                    settlement_order: vec![],
                })
            }),
        )
        .await
        .expect("tool batch effect execution");

    // Retrieve records produced for DurableToolBatch
    let batch_verdict_key = "lash:fig1767-tool-batch.journal-budget";
    let batch_record_key = "lash:fig1767-tool-batch";

    let records = context.records.lock_recover();
    let batch_verdict_bytes = records
        .get(batch_verdict_key)
        .expect("batch budget verdict journal entry");
    let batch_record_bytes = records
        .get(batch_record_key)
        .expect("batch effect record journal entry");

    assert_eq!(
        batch_verdict_bytes.as_slice(),
        b"\"Proceed\"",
        "tool batch budget verdict byte sequence mismatch"
    );
    assert_eq!(
        batch_record_bytes,
        br##"{"envelope":{"json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\"},\"replay_key\":\"fig1767-tool-batch\"},\"effect_id\":\"fig1767-tool-batch\",\"attribution\":{\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\",\"turn_index\":1,\"protocol_iteration\":0}},\"command\":{\"type\":\"tool_batch\",\"batch\":{\"batch_id\":\"fig1767-batch\",\"calls\":[{\"call\":{\"call_id\":\"call-1\",\"tool_id\":\"tool:tool\",\"tool_name\":\"tool\",\"args\":{}},\"replay_suffix\":\"child:0:call-1\"}]}}}","hash":"7303b5b54d2d530f219457ea07eeaea45e798d3d71c38df1356b428b0a4ba623"},"outcome":{"Ok":{"type":"tool_batch","launches":[],"settlement_order":[]}}}"##,
        "tool batch recorded effect golden bytes changed"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-process-cmd.journal-budget",
            "lash:fig1767-process-cmd.parent-end-cancel-decision:v1",
            "lash:fig1767-process-cmd",
            "lash:fig1767-tool-batch.journal-budget",
            "lash:fig1767-tool-batch"
        ],
        "each eager effect must journal its decisions after its budget verdict and before its recorded effect"
    );
}

/// FIG-1767: redriving an eager effect (both durable process command and durable tool batch)
/// whose journaled budget verdict is a give-up executes nothing — the run future reaches
/// the helper unpolled and is never executed.
#[tokio::test]
pub(super) async fn fig1767_give_up_verdict_redrive_executes_nothing() {
    let context = Arc::new(ReplayableRecordingContext::default());

    // 1. Durable Process Command over budget
    let process_invocation = turn_effect_invocation(
        "fig1767-session",
        "fig1767-turn",
        1,
        0,
        "fig1767-over-budget-proc",
        "fig1767-over-budget-proc",
    );
    let process_envelope = RuntimeEffectEnvelope::new(
        process_invocation,
        RuntimeEffectCommand::Process {
            command: Box::new(ProcessCommand::ParentEnd {
                identity: lash_core::ToolIntentIdentity {
                    session_id: SessionId::from("fig1767"),
                    execution_scope_id: "scope".to_string(),
                    tool_call_id: "call".to_string(),
                    intent_index: 0,
                    replay_key: "key".to_string(),
                    minting_emission_replay_key: None,
                },
                process_id: ProcessId::from("fig1767-proc"),
                policy: lash_core::ProcessParentEndPolicy::Cancel,
            }),
        },
    );

    let recorded_proc_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        process_envelope.clone(),
        registry_local_executor(process_registry()),
    )
    .await
    .expect_err("process command over budget must give up");

    assert_eq!(
        recorded_proc_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );

    // Redrive process command under a larger budget — must read journaled verdict and execute nothing.
    // Use a real process executor: if the gate moved after the work, its outcome observer would
    // witness the ParentEnd operation before the missing effect record fails the redrive.
    context.replaying.store(true, Ordering::SeqCst);
    let replay_registry = process_registry();
    replay_registry
        .register_process(external_registration("fig1767-proc"))
        .await
        .expect("register process for redrive witness");
    let process_executed = Arc::new(AtomicBool::new(false));
    let ran_proc = Arc::clone(&process_executed);
    let process_observer: lash_core::ProcessOutcomeObserver = Arc::new(move |_| {
        ran_proc.store(true, Ordering::SeqCst);
    });

    let replayed_proc_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(
        process_envelope,
        registry_local_executor(replay_registry).with_process_outcome_observer(process_observer),
    )
    .await
    .expect_err("replayed give-up verdict must return poisoned error");

    assert_eq!(
        replayed_proc_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !process_executed.load(Ordering::SeqCst),
        "redriving a process command give-up verdict must execute nothing"
    );

    // 2. Durable Tool Batch over budget
    context.replaying.store(false, Ordering::SeqCst);
    let batch_invocation = turn_effect_invocation(
        "fig1767-session",
        "fig1767-turn",
        1,
        0,
        "fig1767-over-budget-batch",
        "fig1767-over-budget-batch",
    );
    let batch_envelope = RuntimeEffectEnvelope::new(
        batch_invocation,
        RuntimeEffectCommand::ToolBatch {
            batch: lash_core::PreparedToolBatch::new("fig1767-batch", vec![prepared_tool_call()]),
        },
    );

    let recorded_batch_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        batch_envelope.clone(),
        RuntimeEffectLocalExecutor::testing(|_| async {
            panic!("tool batch initial attempt must give up before running work");
        }),
    )
    .await
    .expect_err("tool batch over budget must give up");

    assert_eq!(
        recorded_batch_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );

    // Redrive tool batch under a larger budget — must read journaled verdict and execute nothing
    context.replaying.store(true, Ordering::SeqCst);
    let batch_executed = Arc::new(AtomicBool::new(false));
    let ran_batch = Arc::clone(&batch_executed);

    let replayed_batch_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(
        batch_envelope,
        RuntimeEffectLocalExecutor::testing(move |_| async move {
            ran_batch.store(true, Ordering::SeqCst);
            panic!("tool batch work closure must never be executed on give-up redrive");
        }),
    )
    .await
    .expect_err("replayed give-up verdict must return poisoned error");

    assert_eq!(
        replayed_batch_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !batch_executed.load(Ordering::SeqCst),
        "redriving a tool batch give-up verdict must execute nothing"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-over-budget-proc.journal-budget",
            "lash:fig1767-over-budget-proc.journal-budget",
            "lash:fig1767-over-budget-batch.journal-budget",
            "lash:fig1767-over-budget-batch.journal-budget"
        ],
        "a give-up redrive must consume only the verdict slot before the next effect"
    );
}

#[tokio::test]
pub(super) async fn journaled_cancel_peeks_replay_while_live_watcher_observes_later_cancel() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scope = durable_turn_scope("journaled-peek-session", "journaled-peek-turn");
    let key = controller
        .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .await
        .expect("cancel gate key");
    let envelope = |identity: &str| {
        RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(scope.clone(), identity)
                    .expect("valid journaled peek address"),
                lash_core::RuntimeAttribution {
                    session_id: Some(SessionId::from("journaled-peek-session")),
                    turn_id: Some(TurnId::from("journaled-peek-turn")),
                    turn_index: None,
                    protocol_iteration: None,
                },
                identity,
            ),
            RuntimeEffectCommand::PeekAwaitEvent { key: key.clone() },
        )
    };
    let start = envelope("turn_cancel.start_gate");
    let post_abort = envelope("turn_cancel.post_abort_gate");
    assert_ne!(
        start.stable_hash().expect("start hash"),
        post_abort.stable_hash().expect("post-abort hash"),
        "later owner reads require a distinct causal identity"
    );

    let first = controller
        .execute_effect(start.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("fresh start-gate peek")
        .into_peek_await_event()
        .expect("start-gate outcome");
    assert_eq!(first, None);

    let cancellation = Resolution::Ok(serde_json::json!({
        "status": "cancel_requested",
        "request_id": "after-start"
    }));
    assert_eq!(
        controller
            .resolve_await_event(&key, cancellation.clone())
            .await
            .expect("resolve cancel gate"),
        ResolveOutcome::Accepted
    );
    let later = controller
        .execute_effect(
            post_abort.clone(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("fresh post-abort peek")
        .into_peek_await_event()
        .expect("post-abort outcome");
    assert_eq!(later, Some(cancellation.clone()));

    context.start_replay();
    let replayed_start = controller
        .execute_effect(start, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed start-gate peek")
        .into_peek_await_event()
        .expect("replayed start-gate outcome");
    let replayed_later = controller
        .execute_effect(post_abort, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed post-abort peek")
        .into_peek_await_event()
        .expect("replayed post-abort outcome");
    assert_eq!(replayed_start, None);
    assert_eq!(replayed_later, Some(cancellation.clone()));

    let live = controller
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("live watcher observes durable cancellation");
    assert_eq!(live, cancellation);
}

#[test]
pub(super) fn restate_handler_controller_disallows_concurrent_effect_calls() {
    let controller = RestateRuntimeEffectController::new(Arc::new(RecordingContext::default()));

    assert!(
        !controller.supports_concurrent_effects(),
        "Restate handler context calls such as ctx.run must be awaited before the next effect call"
    );
}

#[test]
pub(super) fn restate_replay_refuses_pre_effect_19_session_list_envelope() {
    const PREDECESSOR_SESSION_LIST_ENVELOPE: &str = r#"{"json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"session-blue\",\"turn_id\":\"turn-blue\"},\"replay_key\":\"trigger:list\"},\"effect_id\":\"trigger:list\",\"attribution\":{\"session_id\":\"session-blue\"}},\"command\":{\"type\":\"trigger\",\"command\":{\"op\":\"list\",\"owner_scope\":{\"type\":\"session\",\"session_id\":\"session-blue\"},\"filter\":{\"session_id\":\"session-blue\"}}}}","hash":"51ba8b5ff2d3fe2ff5f5009d4f8fc42946b11e86575901a64393b3e01c912db1"}"#;

    let recorded_envelope: lash_core::facade_support::CanonicalRuntimeEffectEnvelope =
        serde_json::from_str(PREDECESSOR_SESSION_LIST_ENVELOPE)
            .expect("deserialize the Restate journal's predecessor envelope directly");
    let recorded_json: serde_json::Value =
        serde_json::from_str(recorded_envelope.json()).expect("inspect predecessor envelope");
    assert_eq!(
        recorded_json.pointer("/command/command/filter/session_id"),
        Some(&serde_json::json!("session-blue")),
        "the Restate fixture must carry the retired shape"
    );

    let reconstructed = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::turn("session-blue", "turn-blue"),
                "trigger:list",
            )
            .expect("valid trigger-list address"),
            RuntimeAttribution::for_session("session-blue"),
            "trigger:list",
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session("session-blue"),
                filter: lash_core::TriggerSubscriptionFilter::for_session("session-blue"),
            }),
        },
    )
    .canonical_form()
    .expect("canonical current trigger-list envelope");
    let journal_wire = serde_json::to_vec(&RecordedRuntimeEffect {
        envelope: Arc::new(recorded_envelope),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    })
    .expect("encode predecessor Restate journal entry");
    let JournaledEffectRecord::Recorded(recorded) = serde_json::from_slice(&journal_wire)
        .expect("replay predecessor through JournaledEffectRecord deserialization")
    else {
        panic!("predecessor effect must decode as a recorded journal entry");
    };

    let error = validate_recorded_effect_envelope(recorded, &reconstructed, None)
        .expect_err("Restate replay must refuse the pre-effect-19 envelope before comparison");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectEnvelopeVersion
    );
    assert_ne!(
        error.code,
        lash_core::RuntimeErrorCode::WorkerReplacementAbort
    );
}

#[test]
pub(super) fn recorded_runtime_effect_hash_mismatch_fails_explicitly() {
    let recorded_envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("recorded envelope");
    let reconstructed = test_sleep_envelope(2)
        .canonical_form()
        .expect("reconstructed envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(recorded_envelope),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let err = validate_recorded_effect_envelope(recorded, &reconstructed, None)
        .expect_err("hash mismatch");

    assert_eq!(
        err.code,
        lash_core::RuntimeErrorCode::WorkerReplacementAbort
    );
    assert!(
        err.code.is_replay_mismatch(),
        "Restate replay divergence must retain the shared typed classification"
    );
    assert_eq!(
        err.summary.expect("mismatch summary"),
        lash_core::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.spec.duration_ms".to_string()],
        }
    );
}

#[test]
pub(super) fn recorded_runtime_effect_hash_match_returns_replayed_outcome() {
    let envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("canonical envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(envelope.clone()),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let outcome = validate_recorded_effect_envelope(recorded, &envelope, None)
        .expect("hash match")
        .expect("replayed outcome");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
}

pub(super) fn test_sleep_envelope(duration_ms: u64) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        turn_effect_invocation("session", "turn", 0, 0, "sleep:test", "sleep:test"),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms },
        },
    )
}

pub(super) fn llm_spec() -> lash_core::LlmRequestSpec {
    lash_core::LlmRequestSpec {
        instructions: None,
        model: "model".to_string(),
        messages: Vec::new(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session".to_string(),
            "session:frame:test".to_string(),
            "session:request:test".to_string(),
        ),
        output_spec: None,
    }
}

pub(super) fn prepared_tool_call() -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        "call-1",
        "tool:tool",
        "tool",
        serde_json::json!({}),
        None,
        serde_json::Value::Null,
    )
}

pub(super) fn prepared_tool_call_with(
    call_id: &str,
    tool_name: &str,
) -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        call_id,
        format!("tool:{tool_name}"),
        tool_name,
        serde_json::json!({ "call": call_id }),
        None,
        serde_json::Value::Null,
    )
}

pub(super) fn completed_tool_record(call_id: &str, tool_name: &str) -> lash_core::ToolCallRecord {
    lash_core::ToolCallRecord {
        call_id: Some(call_id.to_string()),
        tool: tool_name.to_string(),
        args: serde_json::json!({ "call": call_id }),
        output: lash_core::ToolCallOutput::success(serde_json::json!({ "call": call_id })),
        duration_ms: 1,
    }
}

pub(super) fn external_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

pub(super) fn rerunnable_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

pub(super) fn rerunnable_session_turn_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::SessionTurn {
            definition_key: "test-session-turn:v1".to_string(),
            create_request: Box::new(lash_core::SessionCreateRequest::child_session(
                "test-parent",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )),
            turn_input: Box::new(lash_core::TurnInput::text("test child turn")),
            output_contract: lash_core::ToolOutputContract::Static,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

pub(super) fn owner_bound_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::OwnerBound,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

pub(super) fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

pub(super) fn process_registry() -> Arc<dyn ProcessRegistry> {
    Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite registry")
    }))
}

pub(super) fn continuation_store() -> Arc<dyn lash_core::ProcessContinuationStore> {
    Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite continuation store")
    }))
}

pub(super) fn process_stores() -> (
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core::ProcessContinuationStore>,
) {
    let storage = Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite process stores")
    }));
    (
        Arc::clone(&storage) as Arc<dyn ProcessRegistry>,
        storage as Arc<dyn lash_core::ProcessContinuationStore>,
    )
}

pub(super) fn lashlang_process_input(
    input: lash_lashlang_runtime::LashlangProcessInput,
) -> ProcessInput {
    input
        .into_process_input()
        .expect("serialize lashlang process input")
}

#[derive(Default)]
pub(super) struct DurableMemoryAttachmentStore {
    inner: lash_core::facade_support::InMemoryAttachmentStore,
}

#[async_trait::async_trait]
impl lash_core::AttachmentStore for DurableMemoryAttachmentStore {
    fn persistence(&self) -> lash_core::AttachmentStorePersistence {
        lash_core::AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: lash_core::AttachmentCreateMeta,
    ) -> Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<Option<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.head(id).await
    }
}

#[derive(Default)]
pub(super) struct DurableMemoryProcessEnvStore {
    inner: lash_core::facade_support::InMemoryProcessExecutionEnvStore,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableMemoryProcessEnvStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .publish_process_execution_env(owner, env_ref, bytes)
            .await
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .transfer_process_execution_env(from, to, env_ref)
            .await
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .release_process_execution_env(owner, env_ref)
            .await
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lash_core::PluginError> {
        self.inner.retire_process_execution_env_owner(owner).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

pub(super) static RECOVERY_PROCESS_ENV_STORE: LazyLock<Arc<DurableMemoryProcessEnvStore>> =
    LazyLock::new(|| Arc::new(DurableMemoryProcessEnvStore::default()));

pub(super) struct CommitRetryStore {
    pub(super) inner: Arc<dyn lash_core::RuntimePersistence>,
    pub(super) lease_claim_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: Arc<dyn lash_core::RuntimePersistence>) -> Self {
        Self {
            inner,
            lease_claim_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

lash_core::impl_noop_attachment_manifest!(CommitRetryStore);

// Pass-through wrapper over the shared in-memory recovery store; every
// segment delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::SessionCommitStore for CommitRetryStore {
    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, lash_core::StoreError> {
        self.inner.admit_and_bind_session(binding).await
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, lash_core::StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        self.inner.commit_runtime_state(commit).await
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<lash_core::SessionMeta>, lash_core::StoreError> {
        self.inner.load_session_meta().await
    }
}

#[async_trait::async_trait]
impl lash_core::SessionExecutionLeaseStore for CommitRetryStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, lash_core::StoreError> {
        self.lease_claim_count.fetch_add(1, Ordering::SeqCst);
        self.inner
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                executor_id,
                claim_nonce,
                lease_ttl_ms,
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLease, lash_core::StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::SessionExecutionLeaseObservation, lash_core::StoreError> {
        self.inner.get_session_execution_lease(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkStore for CommitRetryStore {
    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> Result<lash_core::runtime::QueuedWorkEnqueueOutcome, lash_core::StoreError> {
        self.inner.enqueue_queued_work_with_outcome(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::QueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::StoreError,
    > {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::SelectedQueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
                policy,
            )
            .await
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, lash_core::StoreError> {
        self.inner
            .queued_work_batch_completed(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, lash_core::StoreError> {
        self.inner.pending_session_work_ordering(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::TurnInputStore for CommitRetryStore {
    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, lash_core::StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::PendingTurnInput>, lash_core::StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::TurnInputApplication>, lash_core::StoreError> {
        self.inner.list_turn_input_applications(session_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_active_turn_inputs(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::runtime::TurnInputClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<lash_core::TurnCancelInputOutcome, lash_core::StoreError> {
        self.inner
            .defer_orphaned_active_turn_inputs(session_id, session_execution_lease, scope)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::StoreMaintenance for CommitRetryStore {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<lash_core::VacuumReport> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<lash_core::GcReport> {
        self.inner.gc_unreachable().await
    }
}
