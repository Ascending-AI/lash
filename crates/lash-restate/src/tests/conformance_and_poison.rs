use super::*;
use lash_core::TurnFailureCode;

// No store-family macros: Restate certifies an engine adapter over borrowed memory/SQLite stores.
// No SQL-journal retirement/fencing macros: replay lives in workflow history, not SQL rows.

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
        lash_core::EffectAddress::new(
            ExecutionScope::turn(
                lash_core::SessionId::fixture(session_id),
                TurnId::fixture(turn_id.to_string()),
            ),
            replay_key,
        )
        .expect("valid test turn effect address"),
        lash_core::RuntimeAttribution::for_turn(
            lash_core::SessionId::fixture(session_id),
            lash_core::TurnId::fixture(turn_id),
            turn_index,
            protocol_iteration,
        ),
        effect_id,
    )
}

lash_conformance::turn_work_driver_tests!({
    let context = Arc::new(RecordingContext::default());
    let registration_context = Arc::clone(&context);
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(context));
    // The driver's session catalog: a SQLite memory store set.
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the turn-work-driver session catalog"),
    );
    ((), host, stores, move |_host, session_id, key| async move {
        registration_context
            .wait_for_await_event_registration(&session_id, &key)
            .await;
    })
});

pub(super) fn replayable_conformance_invocation(
    context: Arc<ReplayableRecordingContext>,
) -> lash_conformance::ConformanceInvocation {
    let controller: Arc<dyn RuntimeEffectController> = Arc::new(
        RestateRuntimeEffectController::new_for_test(Arc::clone(&context)),
    );
    lash_conformance::ConformanceInvocation::new(
        controller,
        ExecutionScope::runtime_operation("restate-replay-conformance"),
        || {},
        move || {
            context.start_replay();
            Arc::new(RestateRuntimeEffectController::new_for_test(Arc::clone(
                &context,
            ))) as Arc<dyn RuntimeEffectController>
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

    /// The law's waiter reattached once, both times to the one process it
    /// awaits: the law registers that process, so its id is the one the
    /// registrar minted and the transport reads it off the requests.
    fn assert_reattached_once(&self) {
        let requests = self.request_urls.lock_recover();
        assert_eq!(requests.len(), 2, "Restate process wait must reattach once");
        assert!(
            requests[0].contains("/LashProcessWorkflow/")
                && requests[0].ends_with("/await_terminal"),
            "the wait attaches to a process workflow: {requests:?}"
        );
        assert!(
            requests.iter().all(|url| url == &requests[0]),
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
                    .with_lash_code(TurnFailureCode::Timeout)
                    .with_retry_verdict(
                        lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                    ),
            ),
            1 => Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: HttpResponseBody::buffered(crate::wire::reply_json(&self.terminal)),
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
        lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
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
            "effect-controller-journaled-replay" => {}
            "effect-controller-code-cell-reexecution" => {
                assert!(
                    runs.iter()
                        .all(|name| !name.contains("code-cell-reexecution:cell")),
                    "a code cell is a direct local call, never a recorded run: {runs:?}"
                );
            }
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
        "effect_replay_divergence",
    )
});

// Derivation-authority terminals are engine-neutral: the executor's error
// wins the journal no matter which controller serves it, so FIG-3668 ports
// the leg onto the in-process recording context. The retry leg went with
// the deleted SQL engine: it asserted the SQL controller surfaced a
// retryable derivation error to the caller, where a Restate controller
// retries the journaled step under its own redelivery.
lash_conformance::effect_controller_response_derivation_tests!(@catalogue {
    let context = Arc::new(ReplayableRecordingContext::default());
    let make_context = Arc::clone(&context);
    (context, move || {
        replayable_conformance_invocation(Arc::clone(&make_context))
    })
}; [
    (
        effect_controller_response_derivation_terminals,
        "effect-controller-response-derivation-terminals"
    ),
    (attempt_history_terminal_variants_survive_result_replay, "attempt-terminal-replay"),
]);

/// The RLM protocol factory the FIG-3587 drift laws redrive cells with, its
/// Lashlang artifacts in [`RECOVERY_ARTIFACT_BACKEND`]. The laws' turns start
/// no process: there is no process substrate.
pub(super) fn drift_law_rlm_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
            &RECOVERY_ARTIFACT_BACKEND,
        )
        .with_process_lifecycle(false),
    )
}

// FIG-4454's committed-final recovery on a live server: the second endpoint
// the recipe provides serves the newer build the deployment change registers.

// FIG-4376's execution-control laws on a live server: a redrive replays the
// run's recorded config, turn budget included, from the server's journal.
mod recorded_execution_controls_live {
    use super::conformance_harness;

    async fn live_harness(
        law: &str,
    ) -> (
        conformance_harness::LiveConformanceHarness,
        &'static str,
        std::sync::Arc<dyn lash_core::EffectHost>,
        std::sync::Arc<dyn lash_core::StoreSet>,
        std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner>,
    ) {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-{law}-live-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }

    lash_conformance::turn_config_tests!(@law [
        #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    ] {
        live_harness("recorded-controls-redrive").await
    }; (a_redrive_runs_under_the_execution_controls_its_run_recorded, "turn-config-recorded-controls-redrive"));

    // FIG-4389's recorded termination law beside it: the redrive assembles
    // the terminal the run's recorded policy decides.
    lash_conformance::turn_config_tests!(@law [
        #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    ] {
        live_harness("recorded-termination-redrive").await
    }; (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

    lash_conformance::turn_config_tests!(@law [
        #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    ] {
        live_harness("recorded-request-defaults-redrive").await
    }; (a_redrive_calls_the_model_with_the_request_defaults_its_run_recorded, "turn-config-recorded-request-defaults-redrive"));
}

// The turn runs inside a live handler: its tool call opens a real Restate
// effect group whose child runs in the endpoint's dispatch invocation, which
// the recording contexts cannot serve (FIG-3397).
lash_conformance::turn_runner_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness =
            Arc::new(conformance_harness::LiveConformanceHarness::start_for_tools().await);
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let registry = stores.process_registry();
        let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!({"signal": "observed"}),
        ));
        let (process_work, wait_transport) =
            conformance_restate_process_work(Arc::clone(&registry), terminal);
        let verify_transport = Arc::clone(&wait_transport);
        // Restate state outlives a run: a fixed prefix would reopen the last
        // run's retired group and replay its settlement instead of running
        // the tool, so each run names its own session, turn and target.
        let prefix: &'static str = Box::leak(
            format!("restate-public-signal-intent-{}", harness.run_nonce()).into_boxed_str(),
        );
        (
            (harness, wait_transport),
            prefix,
            effect_host,
            stores,
            process_work,
            turn_runner,
            // Only the signal law waits on a process terminal through the
            // attach transport; the turn-cancel laws never touch it.
            move |law: &'static str| async move {
                if law == "public_signal_intent_wakes_parked_process" {
                    verify_transport.assert_reattached_once();
                }
            },
        )
    }
);

// FIG-1293's migrated tools on the live endpoint: the turn runs in a probe
// handler, `spawn_agent`'s child session runs in the endpoint's
// LashProcessWorkflow on the law's worker, and the crash is a failed handler
// attempt that Restate redelivers.
lash_conformance::migrated_tools_redrive_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: a fixed prefix would reopen the last
        // run's workflows and groups, so each run names its own.
        let prefix: &'static str =
            Box::leak(format!("restate-migrated-tools-{}", harness.run_nonce()).into_boxed_str());
        let plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
            Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
            Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ),
            Arc::new(lash_subagents::SubagentsPluginFactory::new(
                Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
                    lash_subagents::StaticCapability::new(
                        "default",
                        lash_core::facade_support::SessionSpec::inherit(),
                    ),
                ))),
                lash_core::lifetime::starter,
            )),
        ];
        (harness, prefix, effect_host, stores, turn_runner, plugins)
    }
);

// FIG-4110's frame-open laws on a live endpoint: each turn runs in a probe
// handler, its summarizer completion is journaled by the real server, and
// each crash is a failed handler attempt Restate redelivers.
lash_conformance::frame_open_redrive_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own session.
        let prefix: &'static str =
            Box::leak(format!("restate-frame-open-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

// FIG-4457's queued input runs law on a live endpoint: the first shift's
// death is a failed handler attempt Restate redelivers, and the second input
// is accepted in between.
lash_conformance::queued_input_runs_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own session.
        let prefix: &'static str = Box::leak(
            format!("restate-queued-input-runs-{}", harness.run_nonce()).into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

// FIG-4297's bound-trigger duplicate law on a live endpoint: each emission
// runs in a probe handler, the delivery's process in the endpoint's
// `LashProcessWorkflow`, and the first emission's crash is a failed handler
// attempt Restate redelivers.
lash_conformance::bound_trigger_duplicate_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own session.
        let prefix: &'static str =
            Box::leak(format!("restate-bound-trigger-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

/// ADR 0116 §7.3's declared-start tier on `harness`'s endpoint: the turn runs
/// in a probe handler, each `spawn_agent` child session runs in the
/// endpoint's `LashProcessWorkflow` on the law's worker, a crash is a failed
/// handler attempt Restate redelivers, and a scope close delivers its
/// children's cancels through the engine's process port.
fn declared_start_tier(
    harness: &conformance_harness::LiveConformanceHarness,
) -> lash_conformance::DeclaredStartTier {
    let backend = harness.law_backend();
    lash_conformance::DeclaredStartTier {
        // Restate state outlives a run, so each run names its own sessions.
        prefix: format!("restate-declared-start-{}", harness.run_nonce()),
        effect_host: harness.endpoint_host(),
        stores: harness.law_stores(),
        runner: harness.turn_runner(),
        rlm: vec![Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
                &backend,
            )
            .with_process_lifecycle(true),
        )],
        subagents: Arc::new(|| {
            let factory = lash_subagents::SubagentsPluginFactory::new(
                Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
                    lash_subagents::StaticCapability::new(
                        "default",
                        lash_core::facade_support::SessionSpec::inherit(),
                    ),
                ))),
                lash_core::lifetime::starter,
            );
            Arc::new(factory)
        }),
        delivery: Arc::clone(backend.process_work().port()),
    }
}

lash_conformance::declared_start_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let tier = declared_start_tier(&harness);
        (harness, tier)
    }
);

// The barrier laws (FIG-3400, ADR 0116 §7.1) on live Restate. Each
// scenario's turn runs in a live handler, and every member of its step's tool
// group — native calls, `batch` members and `Promise.all` leaves alike — is a
// child invocation of one durable effect group. The process-bridge producer
// executes its worker in the test process and stays on the in-process tiers.
lash_conformance::tool_batch_parallelism_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own sessions.
        let prefix: &'static str =
            Box::leak(format!("restate-tool-group-{}", harness.run_nonce()).into_boxed_str());
        let standard = || -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
            vec![Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            )]
        };
        (
            harness,
            prefix,
            effect_host,
            stores,
            vec![
                lash_conformance::batch_sugar_producer(standard()),
                lash_conformance::batch_wrappers_beside_native_calls_producer(standard()),
                lash_conformance::parallel_model_tool_calls_producer(standard()),
                lash_conformance::rlm_promise_all_producer(vec![drift_law_rlm_factory()], false),
                lash_conformance::rlm_promise_all_settled_producer(
                    vec![drift_law_rlm_factory()],
                    false,
                ),
            ],
            turn_runner,
        )
    }
);

// The `batch` sugar laws (ADR 0116 §7.2) on live Restate.
lash_conformance::batch_sugar_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own sessions.
        let prefix: &'static str =
            Box::leak(format!("restate-batch-sugar-{}", harness.run_nonce()).into_boxed_str());
        (
            harness,
            prefix,
            effect_host,
            stores,
            turn_runner,
            lash_conformance::BatchSugarFactories {
                enabled: vec![Arc::new(
                    lash_protocol_standard::StandardProtocolPluginFactory::new(),
                )],
                disabled: super::tool_batch_parallelism_on_the_double::withheld_factories(),
            },
        )
    }
);

// FIG-4079's tool-call identity laws on the live endpoint: each turn runs in
// a probe handler, and a crash is a failed handler attempt Restate
// redelivers. The process-admission law executes its worker in the test
// process and stays on the double's tiers.
lash_conformance::tool_call_identity_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        // Restate state outlives a run, so each run names its own sessions.
        let tier = lash_conformance::ToolCallIdentityTier {
            prefix: format!("restate-tool-call-identity-{}", harness.run_nonce()),
            effect_host: harness.endpoint_host(),
            stores: harness.law_stores(),
            runner: harness.tool_call_identity_runner(),
            rlm: vec![drift_law_rlm_factory()],
        };
        (harness, tier)
    }
);

// FIG-4159's worker-broker laws on the live endpoint: each turn runs in a
// probe handler, a lost worker fails the attempt retryably, and Restate
// redelivers the invocation, replaying its journal into the redrive.
lash_conformance::vm_broker_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        // Restate state outlives a run: each run names its own sessions.
        let prefix = format!("restate-vm-broker-{}", harness.run_nonce());
        let runner = harness.turn_runner();
        (harness, prefix, runner)
    }
);

// FIG-3547's segment redrive law on the live endpoint: the segments run in
// the endpoint's `LashProcessWorkflow`, a crash is a failed attempt Restate
// delivers again, and a lost substrate is the invocation killed and purged
// through the admin API, then submitted afresh.
lash_conformance::segment_redrive_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        // Restate state outlives a run: each run names its own processes.
        let prefix: &'static str =
            Box::leak(format!("restate-segment-redrive-{}", harness.run_nonce()).into_boxed_str());
        let stores = harness.law_stores();
        let runner = harness.turn_runner();
        (harness, prefix, stores, runner)
    }
);

// FIG-3682's admitted-head law on the live endpoint: the turn runs in a probe
// handler, crashes after its commit, and Restate redelivers it; the redrive
// replays the invocation's journal against a head that already holds the
// commit. The turn calls no tool, so it runs on the replay leg too.
lash_conformance::admitted_head_redrive_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        // Restate state outlives a run: each run names its own session.
        let prefix: &'static str =
            Box::leak(format!("restate-admitted-head-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

lash_conformance::wake_delivery_ordering_tests!({
    let backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set");
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
        serde_json::json!({"terminal_wait": "observed"}),
    ));
    let (process_work, wait_transport) =
        conformance_restate_process_work(Arc::clone(&registry), terminal);
    let verify_transport = Arc::clone(&wait_transport);
    let barrier_transport = Arc::clone(&wait_transport);
    (
        wait_transport,
        registry,
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Reattach,
        move || async move {
            barrier_transport.wait_for_bounded_reattachment().await;
        },
        move || async move {
            verify_transport.assert_reattached_once();
        },
    )
});

lash_conformance::wake_delivery_crash_tests!({
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let backend = lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
        lash_sqlite_store::SqliteStoreSetOptions {
            wake_delivery: lash_core::WakeDeliveryConfig::new(10_000)
                .expect("valid Restate conformance wake expiry")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid Restate conformance stale-claim age"),
            ..lash_sqlite_store::SqliteStoreSetOptions::memory()
        },
        Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
    )
    .await
    .expect("open a SQLite memory store set");
    let registry = backend.process_registry();
    let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
        serde_json::json!({"terminal_wait": "observed"}),
    ));
    let (process_work, wait_transport) = conformance_restate_process_work(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        terminal,
    );
    let factory: Arc<dyn lash_core::DeploymentStore> = backend.session_store_factory();
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
            verify_transport.assert_reattached_once();
        },
    )
});

/// Reads a receiver's wake redelivery floor straight from the server
/// double's SQLite memory catalog.
struct DoubleWakeRedeliveryFloors(Arc<lash_sqlite_store::SqliteStoreSet>);

#[async_trait::async_trait]
impl lash_conformance::WakeRedeliveryFloorProbe for DoubleWakeRedeliveryFloors {
    async fn receiver_floor(
        &self,
        session_id: &lash_core::SessionId,
        process_id: &lash_core::ProcessId,
    ) -> Option<u64> {
        use rusqlite::OptionalExtension as _;
        rusqlite::Connection::open(
            self.0
                .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
        )
        .expect("open the memory catalog")
        .query_row(
            "SELECT allocation_floor FROM wake_redelivery_fences
             WHERE session_id = ?1 AND process_id = ?2",
            rusqlite::params![session_id.as_str(), process_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .expect("read the receiver floor")
        .map(|floor| u64::try_from(floor).expect("non-negative receiver floor"))
    }
}

// The wake content-conflict law (FIG-4487) on the engine-driven path: the
// wake driver asks the Restate engine for the later wake's shift, over the
// server double's own store set and virtual clock.
lash_conformance::wake_delivery_conflict_tests!({
    let backend = lash_restate_test::backend(0x4487, lash_restate_test::ServerConfig::default())
        .await
        .expect("start the wake-conflict server double");
    let stores = Arc::clone(backend.engine_stores());
    let floors = Arc::new(DoubleWakeRedeliveryFloors(Arc::clone(backend.stores())))
        as Arc<dyn lash_conformance::WakeRedeliveryFloorProbe>;
    (
        backend.clone(),
        stores.session_store_factory(),
        stores.process_registry(),
        backend.test_clock(),
        backend.explicit_reconcile_session_work(),
        floors,
    )
});

// A Restate host resolves its group children at the endpoint, so it is never
// an unregistered host: `effect_group_unwired_host_tests!` does not apply.

// A close racing its own children's settlements seats one terminal per child.

// The session-config settlement laws on the Restate backend: its engine host
// over one SQLite memory store set per law.
lash_conformance::session_config_settlement_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = conformance_harness::LiveConformanceHarness::start().await;
        let make = harness.backend_factory();
        (harness, make)
    }
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
async fn live_restate_executing_effect_quiescence_witness() {
    let harness = conformance_harness::LiveConformanceHarness::start().await;
    tokio::time::timeout(
        Duration::from_secs(240),
        harness.run_executing_effect_quiescence_witness(),
    )
    .await
    .expect("Restate quiescence witness exceeded 240 seconds");
    harness.finish().await;
}

lash_conformance::effect_host_await_event_witness_tests!(
    #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
    {
        let harness = Arc::new(conformance_harness::LiveConformanceHarness::start().await);
        let make = harness.effect_host_factory();
        let make_catalog = harness.session_catalog_factory();
        let witness_harness = Arc::clone(&harness);
        let teardown_harness = Arc::clone(&harness);
        (
            harness,
            Duration::from_secs(240),
            make,
            make_catalog,
            move |host, assert_retirement| async move {
                witness_harness
                    .run_active_wait_registration_witnesses(host, assert_retirement)
                    .await;
            },
            async move { teardown_harness.finish().await },
        )
    }
);

/// The live effect-group suites above, on the in-process `lash-restate-test`
/// server double: the same endpoint and the same laws, with the Restate
/// server simulated in process — no sockets, no Docker, virtual time.
mod on_the_server_double {
    use super::conformance_harness::{HarnessServer, LiveConformanceHarness};
    use super::*;

    // The session-config settlement laws on the Restate backend: its engine
    // host over one SQLite memory store set per law.
    lash_conformance::session_config_settlement_tests!({
        let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
        let make = harness.backend_factory();
        (harness, make)
    });

    lash_conformance::turn_runner_tests!({
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let registry = stores.process_registry();
        let terminal = ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!({"signal": "observed"}),
        ));
        let (process_work, wait_transport) =
            conformance_restate_process_work(Arc::clone(&registry), terminal);
        let verify_transport = Arc::clone(&wait_transport);
        let prefix: &'static str = Box::leak(
            format!("restate-public-signal-intent-{}", harness.run_nonce()).into_boxed_str(),
        );
        (
            (harness, wait_transport),
            prefix,
            effect_host,
            stores,
            process_work,
            turn_runner,
            move |law: &'static str| async move {
                if law == "public_signal_intent_wakes_parked_process" {
                    verify_transport.assert_reattached_once();
                }
            },
        )
    });

    // FIG-3587's model-call drift law: the drifted redrive parks the turn and
    // fails its attempt retryably, so the invocation keeps its journal; a
    // retry under the restored surface replays it and finishes the turn once,
    // and the commit clears the park.
    lash_conformance::model_call_drift_park_tests!({
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let host = harness.endpoint_host();
        let runner = harness.turn_runner();
        let prefix: &'static str =
            Box::leak(format!("restate-model-drift-{}", harness.run_nonce()).into_boxed_str());
        let stores = harness.law_stores();
        (
            harness,
            prefix,
            host,
            stores,
            runner,
            vec![super::drift_law_rlm_factory()],
        )
    });

    lash_conformance::admitted_head_redrive_tests!({
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-admitted-head-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    });

    // A cancelled turn drops its tool child even when the tool ignores the
    // cancellation: on Restate the child's dispatch invocation is cancelled
    // and its handler future dropped.

    lash_conformance::effect_host_await_event_witness_tests!({
        let harness = Arc::new(LiveConformanceHarness::start_on(HarnessServer::in_process()).await);
        let make = harness.effect_host_factory();
        let make_catalog = harness.session_catalog_factory();
        let witness_harness = Arc::clone(&harness);
        let teardown_harness = Arc::clone(&harness);
        (
            harness,
            Duration::from_secs(240),
            make,
            make_catalog,
            move |host, assert_retirement| async move {
                witness_harness
                    .run_active_wait_registration_witnesses(host, assert_retirement)
                    .await;
            },
            async move { teardown_harness.finish().await },
        )
    });

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn executing_effect_quiescence_witness() {
        let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
        tokio::time::timeout(
            Duration::from_secs(240),
            harness.run_executing_effect_quiescence_witness(),
        )
        .await
        .expect("quiescence witness on the server double exceeded 240 seconds");
        harness.finish().await;
    }
}

#[tokio::test]
pub(super) async fn durable_trace_is_observed_once_across_a_redrive_and_adds_no_journal_command() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let controller = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().segment_effect_budget(1),
    )
    .with_tracing(
        lash_core::facade_support::TraceRuntime::default()
            .with_trace_sink(sink_dyn)
            .with_base_context(lash_trace::TraceContext {
                run_id: Some("restate-host-run".to_string()),
                ..lash_trace::TraceContext::default()
            }),
    );
    // Each attempt of the handler executes through a controller of its own.
    let attempt = || {
        lash_core::ScopedEffectController::borrowed(
            &controller,
            lash_core::AdmittedScope::runtime_operation("trace-replay-session"),
        )
        .expect("admitted operation scope")
    };
    let envelope = RuntimeEffectEnvelope::new(
        operation_effect_invocation(
            "trace-replay-session",
            lash_core::RuntimeAttribution::for_session("trace-replay-session"),
            "trace-replay-tool",
            "trace-replay-tool",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: Box::new(prepared_tool_call_with(
                "trace-replay-call",
                "trace_replay_tool",
            )),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_calls = Arc::new(AtomicUsize::new(0));

    let boundary = || {
        RuntimeEffectController::wants_segment_boundary(
            &controller,
            &lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: Some(128),
            },
        )
    };
    let first_calls = Arc::clone(&local_calls);
    attempt()
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                first_calls.fetch_add(1, Ordering::SeqCst);
                Ok(restate_segment_tool_attempt_outcome(0))
            }),
        )
        .await
        .expect("live journaled effect");
    assert_eq!(boundary(), Some(lash_core::BoundaryReason::JournalBudget));
    context.start_replay();
    let replay_calls = Arc::clone(&local_calls);
    attempt()
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
        boundary(),
        Some(lash_core::BoundaryReason::JournalBudget),
        "the redrive decides the same boundary"
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
            "durable_segment_boundary",
        ],
        "the attempt that ran the effect observed it; the redrive that reads it back observes nothing"
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

pub(super) fn fig1464_poison_list_envelope(session: &str, effect: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        operation_effect_invocation(
            session,
            lash_core::RuntimeAttribution::for_session(SessionId::fixture(session.to_string())),
            effect,
            effect,
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session(SessionId::fixture(
                    session.to_string(),
                )),
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
    let store = memory_trigger_store().await;
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
                .with_payload_schema(lash_core::JsonSchema::any()),
            },
        )
        .await
        .expect("seed a subscription so the listed outcome outgrows the budget")
        .expect("trigger registration outcome");

    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options_for_test(
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
        lash_core::RuntimeErrorCode::EngineJournaledEffectPoisoned
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

/// FIG-1464 deciding risk: the give-up verdict reads a process-configured
/// budget, so a budget change between attempts must not flip the *shape* of the
/// journal. The give-up occupies its slot with a fixed-size poison entry, so a
/// larger budget on redrive consumes the same slot and observes the same typed
/// failure instead of diverging by proposing a record where the first attempt
/// proposed nothing.
#[tokio::test]
pub(super) async fn fig1464_over_budget_give_up_replays_identically_under_a_larger_budget() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let store = memory_trigger_store().await;
    let envelope =
        || fig1464_poison_list_envelope("restate-budget-flip-session", "restate-budget-flip");

    let recorded = RestateRuntimeEffectController::with_options_for_test(
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
    let replayed = RestateRuntimeEffectController::with_options_for_test(
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

/// FIG-1767 / FIG-4909: the eager effect arm keeps canonical journal bytes
/// stable across recording and replay. FIG-4850's payload codec sorts object
/// fields while preserving the canonical envelope's JSON string verbatim.
/// The row the FIG-1767 sample command signals: the first id a
/// sequential-mint registry hands out, so the golden bytes stay fixed.
fn fig1767_target() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

#[tokio::test]
pub(super) async fn fig1767_journal_entry_byte_sequence_equality() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options_for_test(
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
            command: Box::new(ProcessCommand::Signal {
                signal: lash_core::ProcessSignal::new(
                    lash_core::ProcessSignalIdentity::new(
                        fig1767_target(),
                        "resume".to_string(),
                        "fig1767-signal".to_string(),
                    )
                    .expect("signal identity"),
                    serde_json::json!({"source": "fig1767"}),
                ),
            }),
        },
    );
    // The sample command signals a real row: the command names an exact process
    // lifetime, so the registry must hold it for the effect to reach the journal.
    let process_registry = sequential_process_registry();
    let fig1767_proc_id = process_registry
        .register_process(external_registration().with_extra_event_types([
            lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::JsonSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            },
        ]))
        .await
        .expect("register the process the sample command signals")
        .id;
    assert_eq!(fig1767_proc_id, fig1767_target());
    let recorded = controller
        .execute_effect(
            process_envelope.clone(),
            registry_local_executor(Arc::clone(&process_registry)),
        )
        .await
        .expect("process command effect execution");

    // Pin the payload codec's canonical bytes, including the journal generation.

    {
        let process_verdict_key = "lash:fig1767-process-cmd.journal-budget";
        let process_record_key = "lash:fig1767-process-cmd";

        let records = context.records.lock_recover();
        let process_verdict_bytes = records
            .get(process_verdict_key)
            .expect("process budget verdict journal entry");
        let process_record_bytes = records
            .get(process_record_key)
            .expect("process effect record journal entry");

        let process_record: serde_json::Value =
            serde_json::from_slice(process_record_bytes).expect("decode process effect record");
        let signal = &process_record["outcome"]["Ok"]["result"];
        assert_eq!(signal["op"], "signal");
        assert_eq!(signal["event"]["event_type"], "signal.resume");

        // Pin verdict entry byte sequence: JournaledBudgetVerdict::Proceed serializes as "Proceed"
        assert_eq!(
            process_verdict_bytes.as_slice(),
            b"\"Proceed\"",
            "process command budget verdict byte sequence mismatch"
        );
        // The signal append stamps a wall clock, so the one timestamp is
        // normalized and every other byte is pinned exactly.
        let process_record_text =
            String::from_utf8(process_record_bytes.clone()).expect("process record is UTF-8");
        let stamp = process_record_text
            .find("\"occurred_at\":")
            .expect("the appended signal event carries its append timestamp");
        let stamp_end = stamp
            + process_record_text[stamp..]
                .find([',', '}'])
                .expect("the numeric append timestamp ends before the next field or object close");
        let normalized_record = format!(
            "{}\"occurred_at\":0{}",
            &process_record_text[..stamp],
            &process_record_text[stamp_end..]
        );
        assert_eq!(
            normalized_record,
            r##"{"effect_journal_version":16,"envelope":{"hash":"5fdcfce131d0cc8e7adc35dcd93313066c80d52144bd147c15d430cb6b0391e7","json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\"},\"replay_key\":\"fig1767-process-cmd\"},\"effect_id\":\"fig1767-process-cmd\",\"attribution\":{\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\",\"turn_index\":1,\"protocol_iteration\":0}},\"command\":{\"type\":\"process\",\"command\":{\"op\":\"signal\",\"signal\":{\"identity\":{\"process_id\":\"p_00000000000070008000000000000001\",\"signal_name\":\"resume\",\"signal_id\":\"fig1767-signal\"},\"payload\":{\"source\":\"fig1767\"}}}}}"},"outcome":{"Ok":{"result":{"event":{"event_type":"signal.resume","invocation":{"attribution":{},"caused_by":{"process_id":"p_00000000000070008000000000000001","type":"process"},"replay":{"key":"process:p_00000000000070008000000000000001:signal.resume:fig1767-signal"},"subject":{"event_type":"signal.resume","process_id":"p_00000000000070008000000000000001","sequence":1,"type":"process_event"}},"occurred_at":0,"payload":{"source":"fig1767"},"process_id":"p_00000000000070008000000000000001","semantics":{"signal_wait":{"ordinal":1}},"sequence":1},"op":"signal"},"type":"process"}}}"##,
            "process command recorded effect golden bytes changed"
        );
    }

    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-process-cmd.journal-budget",
            "lash:fig1767-process-cmd.process-signal-append:v1",
            "lash:fig1767-process-cmd"
        ],
        "the eager effect must journal its decisions after its budget verdict and before its recorded effect"
    );

    context.start_replay();
    let replayed = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(process_envelope, registry_local_executor(process_registry))
    .await
    .expect("a fresh controller replays the canonical journal entry");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replayed process outcome"),
        serde_json::to_vec(&recorded).expect("serialize recorded process outcome"),
        "replay must decode the exact recorded outcome, including its original timestamp"
    );
}

/// FIG-1767: redriving the eager effect (the durable process command) whose
/// journaled budget verdict is a give-up executes nothing — the run future reaches
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
            command: Box::new(ProcessCommand::Signal {
                signal: lash_core::ProcessSignal::new(
                    lash_core::ProcessSignalIdentity::new(
                        fig1767_target(),
                        "resume".to_string(),
                        "fig1767-signal".to_string(),
                    )
                    .expect("signal identity"),
                    serde_json::json!({"source": "fig1767"}),
                ),
            }),
        },
    );

    // The sample command signals a real row: the command names an exact process
    // lifetime, so the registry must hold it for the effect to reach the journal.
    let recorded_registry = sequential_process_registry();
    let fig1767_proc_id = recorded_registry
        .register_process(external_registration().with_extra_event_types([
            lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::JsonSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            },
        ]))
        .await
        .expect("register the process the sample command signals")
        .id;
    assert_eq!(fig1767_proc_id, fig1767_target());
    let recorded_proc_err = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        process_envelope.clone(),
        registry_local_executor(recorded_registry),
    )
    .await
    .expect_err("process command over budget must give up");

    assert_eq!(
        recorded_proc_err.code,
        lash_core::RuntimeErrorCode::EngineJournaledEffectPoisoned
    );

    // Redrive process command under a larger budget — must read journaled verdict and execute nothing.
    // Use a real process executor: if the gate moved after the work, its outcome observer would
    // witness the ParentEnd operation before the missing effect record fails the redrive.
    context.replaying.store(true, Ordering::SeqCst);
    let replay_registry = sequential_process_registry();
    let fig1767_proc_id = replay_registry
        .register_process(external_registration().with_extra_event_types([
            lash_core::ProcessEventType {
                name: "fig1767.sample".to_string(),
                payload_schema: lash_core::JsonSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            },
        ]))
        .await
        .expect("register process for redrive witness")
        .id;
    assert_eq!(fig1767_proc_id, fig1767_target());
    let process_executed = Arc::new(AtomicBool::new(false));
    let ran_proc = Arc::clone(&process_executed);
    let process_observer: lash_core::ProcessOutcomeObserver = Arc::new(move |_, _| {
        ran_proc.store(true, Ordering::SeqCst);
    });

    let replayed_proc_err = RestateRuntimeEffectController::with_options_for_test(
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
        lash_core::RuntimeErrorCode::EngineJournaledEffectPoisoned
    );
    assert!(
        !process_executed.load(Ordering::SeqCst),
        "redriving a process command give-up verdict must execute nothing"
    );

    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-over-budget-proc.journal-budget",
            "lash:fig1767-over-budget-proc.journal-budget"
        ],
        "a give-up redrive must consume only the verdict slot"
    );
}

#[tokio::test]
pub(super) async fn journaled_cancel_peeks_replay_while_live_watcher_observes_later_cancel() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
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
        .await_await_event(&key, tokio_util::sync::CancellationToken::new())
        .await
        .expect("live watcher observes durable cancellation");
    assert_eq!(live, cancellation);
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
    let journal_wire =
        serde_json::to_vec(&JournaledEffectRecord::Recorded(RecordedRuntimeEffect {
            envelope: Arc::new(recorded_envelope),
            outcome: Ok(RuntimeEffectOutcome::Sleep),
        }))
        .expect("encode predecessor Restate journal entry");
    let JournaledEffectRecord::Recorded(recorded) = serde_json::from_slice(&journal_wire)
        .expect("replay predecessor through JournaledEffectRecord deserialization")
    else {
        panic!("predecessor effect must decode as a recorded journal entry");
    };

    let error = validate_recorded_effect_envelope(recorded, &reconstructed, None)
        .expect_err("Restate replay must refuse an envelope this build never writes");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EffectReplayDivergence
    );
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
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("model".to_string())
                    .context_window_tokens(128_000)
                    .capability(lash_core::LlmProfileCapability::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: Vec::new(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            SessionId::fixture("session".to_string()),
            "session:frame:test".to_string(),
            "session:request:test".to_string(),
        ),
        output_spec: None,
    }
}

pub(super) fn prepared_tool_call_with(
    call_id: &str,
    tool_name: &str,
) -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall {
        call_id: lash_core::ToolCallId::fixture(call_id),
        provider_call_id: None,
        tool_id: format!("tool:{tool_name}").into(),
        tool_name: tool_name.into(),
        args: serde_json::json!({ "call": call_id }),
        replay: None,
        prepared_payload: serde_json::Value::Null,
    }
}

pub(super) fn completed_tool_record(call_id: &str, tool_name: &str) -> lash_core::ToolCallRecord {
    lash_core::ToolCallRecord {
        call_id: lash_core::ToolCallId::fixture(call_id),
        provider_call_id: None,
        tool: tool_name.to_string(),
        args: serde_json::json!({ "call": call_id }),
        output: lash_core::ToolCallOutput::success(serde_json::json!({ "call": call_id })),
    }
}

pub(super) fn external_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
}

/// A process lash executes: an engine input with its captured execution env.
/// The engine kind is never resolved; the tests run it through their own
/// runner.
pub(super) fn executed_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "restate-test".to_string(),
            payload: serde_json::Value::Null,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
        "process-env:restate-test",
    )))
}

/// The environment a test's session-turn start captures, published to
/// `store`: the starter's recorded policy the child session is created
/// under, which records the model the test workers serve (FIG-4396).
pub(super) async fn persist_session_turn_env_ref(
    store: &dyn ProcessExecutionEnvStore,
) -> lash_core::ProcessExecutionEnvRef {
    lash_core::runtime::publish_process_execution_env(
        store,
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            super::process_workflow::recovery_session_policy(),
        ),
    )
    .await
    .expect("publish the session turn's captured environment")
}

/// A host session-turn start under `env_ref`, the environment it captured.
pub(super) fn session_turn_registration(
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::SessionTurn {
            definition_key: "test-session-turn:v1".to_string(),
            create_request: Box::new(lash_core::SessionCreateRequest::child_session(
                "test-parent",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )),
            turn_input: Box::new(lash_core::TurnInput::text("test child turn")),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(env_ref))
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
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("sqlite registry")
            .process_registry()
    })
}

/// A registry whose minted ids are the sequential test ids, for a law that
/// must name a process before the controller under test starts it.
pub(super) fn sequential_process_registry() -> Arc<dyn ProcessRegistry> {
    sequential_process_stores().0
}

pub(super) fn continuation_store() -> Arc<dyn lash_core::ProcessContinuationStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("sqlite continuation store")
            .process_registry()
    })
}

pub(super) fn process_stores() -> (
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core::ProcessContinuationStore>,
) {
    let storage = sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("sqlite process stores")
            .process_registry()
    });
    (
        Arc::clone(&storage) as Arc<dyn ProcessRegistry>,
        storage as Arc<dyn lash_core::ProcessContinuationStore>,
    )
}

/// Process stores whose registrar mints the sequential test ids, for a
/// redelivery law that re-registers its process on fresh stores: the
/// redelivered run must carry the id the recorded journal names.
pub(super) fn sequential_process_stores() -> (
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core::ProcessContinuationStore>,
) {
    let storage = sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
            lash_sqlite_store::SqliteStoreSetOptions {
                process_id_mint: lash_core::ProcessIdMint::sequential_for_testing(),
                ..lash_sqlite_store::SqliteStoreSetOptions::memory()
            },
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
        .expect("sqlite process stores")
        .process_registry()
    });
    (
        Arc::clone(&storage) as Arc<dyn ProcessRegistry>,
        storage as Arc<dyn lash_core::ProcessContinuationStore>,
    )
}

/// A lashlang registration as its start's registration step would hand it to
/// the registry (FIG-4527): it carries the configuration the creating engine
/// records with the row. The laws register on the registry directly, past
/// that step, and every worker engine they run is built over the default
/// surface and bounds, so that is what creation records here.
pub(super) fn lashlang_registration(
    input: lash_lashlang_runtime::LashlangProcessInput,
    provenance: lash_core::ProcessProvenance,
    lifetime: lash_core::Lifetime,
) -> ProcessRegistration {
    use lash_core::ProcessEngine as _;
    let mut registration = ProcessRegistration::new(
        input
            .into_process_input()
            .expect("serialize lashlang process input"),
        provenance,
        lifetime,
    );
    registration.engine_config = lash_lashlang_runtime::LashlangProcessEngine::new(
        recovery_artifact_store(),
        lash_lashlang_runtime::LashlangSurface::default(),
        RECOVERY_ARTIFACT_BACKEND.worker_recovery(),
    )
    .creation_config(&lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        super::process_workflow::recovery_session_policy(),
    ))
    .expect("record the creating engine's settings");
    registration
}

/// The backend whose Lashlang artifact store the recovery laws share — this
/// engine over one SQLite memory store set: their registration helpers
/// publish modules into it and their workers' engines read them back, as one
/// host's backend would.
pub(super) static RECOVERY_ARTIFACT_BACKEND: LazyLock<lash_core::Backend> = LazyLock::new(|| {
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the artifact backend runtime")
            .block_on(super::memory_engine_backend())
    })
    .join()
    .expect("open the recovery artifact backend on its own thread")
});

/// [`RECOVERY_ARTIFACT_BACKEND`]'s process-exec-env store: the environments
/// the recovery laws' registrations publish and their workers read back.
pub(super) static RECOVERY_PROCESS_ENV_STORE: LazyLock<Arc<dyn ProcessExecutionEnvStore>> =
    LazyLock::new(|| RECOVERY_ARTIFACT_BACKEND.process_env_store());

/// [`RECOVERY_ARTIFACT_BACKEND`]'s Lashlang artifact store.
pub(super) fn recovery_artifact_store() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&RECOVERY_ARTIFACT_BACKEND)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated native Restate and PostgreSQL services"]
async fn live_attachment_materialization_turn_witnesses() {
    let harness = conformance_harness::LiveConformanceHarness::start_for_tools().await;
    let directory = tempfile::tempdir().expect("file store directory");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachments");
    #[expect(
        clippy::disallowed_methods,
        reason = "service fixture reads its isolated PostgreSQL URL"
    )]
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL URL");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("PostgreSQL storage");
    let stores: Vec<(&str, Arc<dyn lash_core::StoreSet>)> = vec![
        (
            "memory",
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("SQLite memory"),
            ),
        ),
        (
            "file",
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(directory.path())
                    .await
                    .expect("SQLite file"),
            ),
        ),
        (
            "postgres",
            Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachments.path(),
                )),
            )),
        ),
    ];
    for (name, stores) in stores {
        let prefix = format!("native-attachment-budget-{}-{name}", harness.run_nonce());
        lash_conformance::attachment_materialization_turn_witnesses(
            &prefix,
            harness.endpoint_host(),
            stores,
            harness.turn_runner(),
        )
        .await;
    }
    harness.finish().await;
}
