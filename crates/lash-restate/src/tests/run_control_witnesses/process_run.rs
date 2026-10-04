/// Which run of a `SessionTurn` process's session a process-run law admits.
#[derive(Clone, Copy, Debug)]
enum ProcessRun {
    /// The process's own child run, named by the process (FIG-4378).
    Child,
    /// A run the process's shift admits ahead of its own row, named by its
    /// input's source key (FIG-4403).
    AdmittedAhead,
}

/// A run a `SessionTurn` process executes runs inline in the process's own
/// run, so the engine never holds a `LashTurn` run of its key on any lane:
/// the process's child run (FIG-4378), and every run its shift admits
/// ahead of it in a reused session (FIG-4403), which its name does not tie
/// to the process. The run's admission records the process's run as its
/// executor. While the process is live, the recovery pass leaves the started
/// run and its admitted input to it. Once the process is terminal nothing
/// executes the run, and the pass ends it `SubstrateLost` with its input.
async fn process_run(server: HarnessServer, which: ProcessRun) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let stores = harness.law_stores();
    let registry = stores.process_registry();
    let factory = stores.session_store_factory();
    let session = SessionId::fixture(format!("process-child-run-{}", harness.run_nonce()));
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::SessionTurn {
                    definition_key: "process-child-run:v1".to_string(),
                    create_request: Box::new(
                        lash_core::SessionCreateRequest::child_session(
                            "process-child-run-parent",
                            lash_core::SessionStartPoint::Empty,
                            lash_core::PluginOptions::default(),
                        )
                        .with_session_id(&session),
                    ),
                    turn_input: Box::new(lash_core::TurnInput::text("child turn")),
                    result: lash_core::SessionTurnOutcome::Turn,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(
                super::persist_session_turn_env_ref(stores.process_env_store().as_ref()).await,
            )),
        )
        .await
        .expect("register the SessionTurn process")
        .id;
    let target = RunRef {
        session: session.clone(),
        run: match which {
            ProcessRun::Child => lash_core::runtime::process_session_turn_id(&process_id),
            ProcessRun::AdmittedAhead => {
                lash_core::TurnId::fixture(format!("ahead-{}", harness.run_nonce()))
            }
        },
    };
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: vec![],
            config: lash_core::testing::mock_session_policy().into(),
            head: lash_core::SessionCreationHead::Config,
            owning_process_id: Some(process_id.clone()),
        },
    )
    .await
    .expect("store");
    let input = store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::next_turn(),
                lash_core::TurnInput::text("child turn"),
            )
            .with_source_key(target.run.as_str()),
        )
        .await
        .expect("enqueue")
        .input_id;
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        store.store(),
        &session,
        "process-child-run",
    )
    .await;
    let mut admission = lash_core::testing::store_fixtures::admit_run_request_for_test(
        &fence,
        &target.run,
        AdmittedHead::Input(input.clone()),
    );
    admission.executor = lash_core::store::RunExecutor::Acceptor {
        scope: lash_core::ExecutionScope::process(process_id.clone()),
    };
    store
        .store()
        .admit_run(&admission)
        .await
        .expect("admit the process's run")
        .expect("the run's admission reaches its head");
    let key = crate::session_shifts::turn_invocation_key(
        &lash_core::engine::ShiftRequest {
            session: session.clone(),
            request: ShiftRequestId::new("no-own-invocation"),
            intended_lane: None,
        },
        0,
    );
    assert!(
        harness
            .admin_client()
            .run_executions(
                &crate::RestateNamespace::default(),
                std::slice::from_ref(&key)
            )
            .await
            .expect("run executes")
            .is_empty(),
        "the engine holds no LashTurn run of the process's run on any lane"
    );

    let work = harness.session_work();
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::shift::StoreParkRecovery::new(factory.as_ref(), &clock);
    let live = read_recovery_pass(&work, &writer).await;
    assert!(
        live.iter().all(|pass| pass
            .as_ref()
            .is_ok_and(|report| !report.ended_runs.contains(&target))),
        "no pass ends the run a live process runs: {live:?}"
    );
    assert!(
        factory
            .run_terminal(&session, &target.run)
            .await
            .expect("terminal read")
            .is_none(),
        "the live process's run stays open"
    );
    assert!(
        matches!(
            factory
                .list_pending_turn_inputs(&session)
                .await
                .expect("pending")
                .as_slice(),
            [row] if row.input.input_id == input
                && row.status == lash_core::PendingTurnInputReadStatus::Admitted {
                    run: target.run.clone(),
                }
        ),
        "the live process's run still holds its input"
    );

    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("child done"),
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("complete the process");
    let ended = read_recovery_pass(&work, &writer).await;
    assert!(
        ended.iter().any(|pass| pass
            .as_ref()
            .is_ok_and(|report| report.ended_runs.contains(&target))),
        "a pass ends the run once its process is terminal: {ended:?}"
    );
    let terminal = factory
        .run_terminal(&session, &target.run)
        .await
        .expect("terminal read")
        .expect("the run has its terminal");
    assert_eq!(
        terminal.cause,
        RunTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        factory
            .list_pending_turn_inputs(&session)
            .await
            .expect("pending")
            .is_empty(),
        "the run's input is settled with it"
    );
    harness.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_its_child_run_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::in_process(), ProcessRun::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_its_child_run_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::Live, ProcessRun::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_a_run_admitted_ahead_of_its_own_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::in_process(), ProcessRun::AdmittedAhead).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_a_run_admitted_ahead_of_its_own_and_a_terminal_one_releases_it()
{
    process_run(HarnessServer::Live, ProcessRun::AdmittedAhead).await;
}
