//! Served process starts on the Restate server double: FIG-3779's
//! served-process-start laws, and FIG-3719's served-only process start and
//! sleep outside a Run.

// FIG-3779's served-process-start laws on the server double: a cell's
// `agents.spawn` is cut at one point of its declared process start — past the
// start, or between the registry write and the workflow send — and redriven
// under a drifted binding; its child session runs in the endpoint's
// `LashProcessWorkflow` on the law's worker.
mod served_process_start_on_the_server_double {
    use std::sync::Arc;

    use super::super::conformance_harness::{HarnessServer, LiveConformanceHarness};

    /// The subagent plugin under a capability registry holding `names`.
    fn subagents(names: &[&str]) -> Arc<dyn lash_core::facade_support::PluginFactory> {
        let registry = names.iter().fold(
            lash_subagents::CapabilityRegistry::new(),
            |registry, name| {
                registry.with(Arc::new(lash_subagents::StaticCapability::new(
                    *name,
                    lash_core::facade_support::SessionSpec::inherit(),
                )))
            },
        );
        Arc::new(lash_subagents::SubagentsPluginFactory::new(
            Arc::new(registry),
            lash_core::lifetime::starter,
        ))
    }

    lash_conformance::served_process_start_tests!({
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let runner = harness.turn_runner();
        let host = harness.endpoint_host();
        let prefix: &'static str =
            Box::leak(format!("restate-spawn-{}", harness.run_nonce()).into_boxed_str());
        let stores = harness.law_stores();
        (
            harness,
            prefix,
            host,
            stores,
            runner,
            vec![super::super::conformance_and_poison::drift_law_rlm_factory()],
            lash_conformance::SubagentFactories {
                recorded: subagents(&["default"]),
                // Another capability changes `spawn_agent`'s input schema:
                // its capability enum, and `capability` becomes required.
                drifted: subagents(&["default", "reviewer"]),
            },
        )
    });
}

// FIG-3719 and FIG-3779 on the server double: a served-only process start or
// sleep answers at its frontier marker. One the replay reaches live refuses,
// so the drifted command parks with no process started and no sleep
// journaled; one whose start and sleep were recorded is served.
mod served_only_outside_a_run {
    use std::sync::Arc;

    use lash_conformance::{ConformanceTurnAttempt, ConformanceTurnEnd};
    use lash_core::{
        CommandJournalGuard, EffectAddress, ProcessCommand, ProcessId, RuntimeAttribution,
        RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
        RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeErrorCode, ServedOnlyRange,
    };
    use lash_sansio::{SessionId, TurnId};

    use super::super::conformance_harness::{HarnessServer, LiveConformanceHarness};

    struct NoopProcessWork;

    #[async_trait::async_trait]
    impl lash_core::ProcessWorkSubstrate for NoopProcessWork {
        async fn deliver_process_start(
            &self,
            record: &lash_core::ProcessRecord,
        ) -> Result<(), lash_core::PluginError> {
            panic!("unexpected process start for {}", record.id)
        }

        async fn await_process_terminal(
            &self,
            process_id: &ProcessId,
        ) -> Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
            panic!("unexpected terminal wait for {process_id}")
        }

        async fn deliver_cancel(
            &self,
            _process_id: &ProcessId,
            _request: &lash_core::CancelRequest,
            _key: &str,
        ) -> Result<(), lash_core::PluginError> {
            Ok(())
        }

        async fn publish_process_terminal(
            &self,
            process_id: &lash_core::ProcessId,
            output: &lash_core::ProcessAwaitOutput,
            key: &str,
        ) -> Result<(), lash_core::PluginError> {
            let _ = (process_id, output, key);
            Ok(())
        }
    }

    /// What the effects answered: the process start's result, the sleep's
    /// result, and the refusal the command's guard tripped on.
    type Answers = (
        Result<(), RuntimeEffectControllerError>,
        Result<(), RuntimeEffectControllerError>,
        Option<RuntimeEffectControllerError>,
    );

    /// One attempt of a command that starts a process and then sleeps: served
    /// only when `served_only`, and crashing once both are recorded when
    /// `crash_after`, so the next attempt replays them.
    fn job(
        registry: &Arc<dyn lash_core::ProcessRegistry>,
        start_key: &lash_core::StartKey,
        served_only: bool,
        crash_after: bool,
        answers: tokio::sync::mpsc::UnboundedSender<Answers>,
    ) -> ConformanceTurnAttempt {
        let registry = Arc::clone(registry);
        let start_key = start_key.clone();
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let start_key = start_key.clone();
            let answers = answers.clone();
            Box::pin(async move {
                let scope = scoped.execution_scope().clone();
                let namespace = format!("{}:exec:lk2", scope.id());
                let mut guard = CommandJournalGuard::open();
                if served_only {
                    guard = guard.served_only(ServedOnlyRange {
                        lower: format!("{namespace}:"),
                        upper: format!("{namespace}:~seal"),
                        refusal: RuntimeEffectControllerError::new(
                            RuntimeErrorCode::LashlangCellBindingDrift,
                            "code cell binding `agents.spawn` (tool `spawn_agent`) is \
                             changed in the live tool registry",
                        ),
                    });
                }
                let guard = Arc::new(guard);
                let guarded = scoped.with_journal_guard(Arc::clone(&guard));
                let invocation = |key: String| {
                    RuntimeEffectInvocation::new(
                        EffectAddress::new(scope.clone(), key.clone())
                            .unwrap_or_else(|error| panic!("address {key}: {error}")),
                        RuntimeAttribution::none(),
                        key,
                    )
                };
                let registration = lash_core::testing::held_engine_registration(
                    serde_json::json!({ "fixture": "served-only" }),
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_start_key(Some(start_key.clone()));
                let started = guarded
                    .execute_effect(
                        RuntimeEffectEnvelope::new(
                            invocation(format!("{namespace}:0000000000:process:start:{start_key}")),
                            RuntimeEffectCommand::Process {
                                command: Box::new(ProcessCommand::Start {
                                    registration: registration.into(),
                                    observers: Vec::new(),
                                    execution_context: Box::default(),
                                }),
                            },
                        ),
                        RuntimeEffectLocalExecutor::processes(
                            registry,
                            Arc::new(NoopProcessWork),
                            lash_core::testing::process_engine_fixture(),
                            lash_core::runtime::HostStartAdmission::default(),
                        )
                        .with_process_env_store(crate::tests::fixture_env_store()),
                    )
                    .await
                    .map(|_| ());
                let slept = guarded
                    .execute_effect(
                        RuntimeEffectEnvelope::new(
                            invocation(format!("{namespace}:0000000000:attempt:1:sleep")),
                            RuntimeEffectCommand::Sleep {
                                spec: lash_core::SleepSpec::For { duration_ms: 1 },
                            },
                        ),
                        RuntimeEffectLocalExecutor::sleep(
                            tokio_util::sync::CancellationToken::new(),
                        ),
                    )
                    .await
                    .map(|_| ());
                if crash_after {
                    panic!("the command crashes once its start and sleep are recorded");
                }
                let _ = answers.send((started, slept, guard.tripped()));
                ConformanceTurnEnd::Settled
            })
        })
    }

    /// The process workflows the double was asked to run.
    fn workflow_runs(server: &lash_restate_test::RestateTestServer) -> usize {
        server
            .invocations()
            .iter()
            .filter(|invocation| {
                invocation
                    .target
                    .starts_with(crate::LashService::ProcessWorkflow.base_name())
            })
            .count()
    }

    /// The processes the registry holds under `start_key`.
    async fn started_under(
        registry: &Arc<dyn lash_core::ProcessRegistry>,
        start_key: &lash_core::StartKey,
    ) -> usize {
        registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .unwrap_or_else(|error| panic!("read the registry: {error}"))
            .into_iter()
            .filter(|record| record.start_key.as_ref() == Some(start_key))
            .count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_drifted_process_start_or_sleep_at_the_live_frontier_parks_without_acting() {
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let server = harness
            .server_double()
            .unwrap_or_else(|| panic!("the in-process harness runs on the server double"));
        let registry = harness.law_stores().process_registry();
        let nonce = harness.run_nonce();
        let session_id = SessionId::fixture(format!("served-only-{nonce}"));
        let turn_id = TurnId::from("served-only-turn");
        let start_key = lash_core::StartKey::for_host(format!("served-only-probe-{nonce}"));
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel::<Answers>();
        harness
            .turn_runner()
            .run_turn(
                lash_core::AdmittedScope::turn(&session_id, &turn_id),
                job(&registry, &start_key, true, false, answers),
            )
            .await;
        let (started, slept, tripped) = answered
            .recv()
            .await
            .unwrap_or_else(|| panic!("the served-only job ran"));
        for (effect, answer) in [("process start", started), ("sleep", slept)] {
            let refusal = answer.expect_err("a served-only effect at the live frontier refuses");
            assert_eq!(
                refusal.code,
                RuntimeErrorCode::LashlangCellBindingDrift,
                "the {effect} refuses with the drift: {refusal:?}"
            );
        }
        assert_eq!(
            tripped.map(|refusal| refusal.code),
            Some(RuntimeErrorCode::LashlangCellBindingDrift),
            "the refusal trips the command's guard, so the turn parks"
        );
        assert_eq!(
            started_under(&registry, &start_key).await,
            0,
            "the drifted command started no process"
        );
        assert_eq!(
            workflow_runs(&server),
            0,
            "the drifted command submitted no process workflow"
        );
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_drifted_process_start_and_sleep_that_were_recorded_are_served() {
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let server = harness
            .server_double()
            .unwrap_or_else(|| panic!("the in-process harness runs on the server double"));
        let registry = harness.law_stores().process_registry();
        let nonce = harness.run_nonce();
        let session_id = SessionId::fixture(format!("served-recorded-{nonce}"));
        let turn_id = TurnId::from("served-recorded-turn");
        let start_key = lash_core::StartKey::for_host(format!("served-recorded-probe-{nonce}"));
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel::<Answers>();
        harness
            .turn_runner()
            .run_crashed_then_redriven_turn(
                lash_core::AdmittedScope::turn(&session_id, &turn_id),
                job(&registry, &start_key, false, true, answers.clone()),
                job(&registry, &start_key, true, false, answers),
            )
            .await;
        let (started, slept, tripped) = answered
            .recv()
            .await
            .unwrap_or_else(|| panic!("the served-only redrive ran"));
        started.unwrap_or_else(|refusal| panic!("the recorded start is served: {refusal:?}"));
        slept.unwrap_or_else(|refusal| panic!("the recorded sleep is served: {refusal:?}"));
        assert!(tripped.is_none(), "nothing refused: {tripped:?}");
        assert_eq!(
            started_under(&registry, &start_key).await,
            1,
            "the recorded start's row stands"
        );
        assert_eq!(
            workflow_runs(&server),
            1,
            "the start's workflow was submitted once, by the first attempt"
        );
        harness.finish().await;
    }
}
