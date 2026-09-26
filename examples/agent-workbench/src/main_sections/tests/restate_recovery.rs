use super::*;
use lash::ProcessId;
use lash::SessionId;
use lash::TurnId;

#[path = "restate_recovery/closure_lifecycle.rs"]
mod closure_lifecycle;

#[path = "restate_recovery/immutable_deployment.rs"]
mod immutable_deployment;

fn complete_full_process_event_page(
    outcome: lash::process::ObservedProcessEventReadOutcome,
) -> Vec<lash::process::ObservedProcessEvent> {
    match outcome {
        lash::process::ProcessEventReadOutcome::Retained(lash::process::ProcessEventPage {
            events: lash::process::ProcessEventPageEvents::Full(events),
            more: lash::process::ProcessEventPageMore::Complete,
        }) => events,
        _ => panic!("expected one complete full process-event page"),
    }
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_process_llm_query_with_typed_output_succeeds() {
    run_async_test_on_stack_budget_multi_thread("workbench-process-llm-query-e2e", 4, || {
        live_restate_process_llm_query_with_typed_output_succeeds_inner()
    });
}

async fn live_restate_process_llm_query_with_typed_output_succeeds_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-process-llm-query-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create process llm_query E2E data dir");
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider_calls_for_provider = Arc::clone(&provider_calls);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-process-llm-query-e2e")
        .complete(move |request| {
            let call = provider_calls_for_provider
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                match call {
                    0 => Ok(text_response(
                        r#"<typescript>
const enrich = async (event: unknown) => {
  return await llm.query({
    task: "Classify the supplied email",
    inputs: { event: event },
    output: { category: "str", confidence: "float" }
  });
};
const handle = await processes.start({ definition: enrich, args: { event: { email: "hello@example.com" } } });
finish(await handle);
</typescript>"#,
                    )),
                    1 => {
                        assert!(request.stream_events.is_none());
                        assert!(matches!(
                            request.output_spec,
                            Some(lash::provider::LlmOutputSpec::JsonSchema(_))
                        ));
                        Ok(text_response(
                            r#"{"kind":"value","value":{"category":"personal","confidence":0.98},"error":null}"#,
                        ))
                    }
                    other => panic!("unexpected process llm_query provider call {other}"),
                }
            }
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let mut turn =
        run_workbench_turn_via_restate(&harness.state, "Run the typed process llm_query repro.")
            .await;
    wait_for_workbench_turn_settled(&mut turn, Duration::from_secs(30)).await;
    wait_for_workbench_message(&harness.state, "personal", Duration::from_secs(30)).await;
    assert_eq!(
        provider_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "outer turn plus exactly one in-attempt llm_query provider call"
    );
    println!("workbench process-llm-query gate passed: typed-output; provider-calls=2");
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

/// A turn whose cell awaits two tool calls together opens a durable effect
/// group, whose index, payload and dispatcher services the workbench endpoint
/// serves because lash binds them: before the backend bound lash's services,
/// the workbench bound none of the effect-group family and this turn hung on
/// the group's first call.
#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_turn_tool_batch_runs_through_effect_group_services() {
    run_async_test_on_stack_budget_multi_thread("workbench-tool-batch-e2e", 4, || {
        live_restate_turn_tool_batch_runs_through_effect_group_services_inner()
    });
}

async fn live_restate_turn_tool_batch_runs_through_effect_group_services_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-tool-batch-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create tool-batch E2E data dir");
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider_calls_for_provider = Arc::clone(&provider_calls);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-tool-batch-e2e")
        .complete(move |_request| {
            let call =
                provider_calls_for_provider.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                match call {
                    0 => Ok(text_response(
                        r#"<typescript>
const found = await Promise.all([
  tools.search({ query: "text checksum", limit: 1 }),
  tools.search({ query: "mail", limit: 1 }),
]);
finish(`tool batch settled: ${found.length}`);
</typescript>"#,
                    )),
                    other => panic!("unexpected tool-batch provider call {other}"),
                }
            }
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let mut turn = run_workbench_turn_via_restate(&harness.state, "Search twice at once.").await;
    wait_for_workbench_turn_settled(&mut turn, Duration::from_secs(30)).await;
    wait_for_workbench_message(
        &harness.state,
        "tool batch settled: 2",
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        provider_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one model call issues the whole batch"
    );
    // A settled group child's cancellation watch ends with its settlement
    // (FIG-3709), so the deployment drains once the turn is done.
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_ingress_owner_restart_resumes_and_remains_cancellable() {
    if std::env::var_os("AGENT_WORKBENCH_RECOVERY_E2E_CHILD").is_some() {
        run_async_test_on_stack_budget_multi_thread("workbench-recovery-child", 4, || {
            live_restate_recovery_child()
        });
        return;
    }
    run_async_test_on_stack_budget_multi_thread("workbench-recovery-parent", 4, || {
        live_restate_ingress_owner_restart_resumes_and_remains_cancellable_inner()
    });
}

#[cfg(unix)]
#[test]
fn restate_recovery_failure_reaps_child_before_aborting_process() {
    use std::os::unix::process::ExitStatusExt as _;

    const CHILD_ENV: &str = "AGENT_WORKBENCH_RECOVERY_FAILURE_SCOPE_CHILD";
    const ROOT_ENV: &str = "AGENT_WORKBENCH_RECOVERY_FAILURE_SCOPE_ROOT";
    if std::env::var_os(CHILD_ENV).is_some() {
        let root = PathBuf::from(std::env::var(ROOT_ENV).expect("recovery failure probe root"));
        let storage = root.join("retained-recovery-store");
        std::fs::create_dir(&storage).expect("create recovery failure probe storage");
        let _failure_scope = AbortRestateFixtureOnPanic::armed("recovery-child-owner");
        let child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn recovery failure probe child");
        let _owned_child = OwnedFixtureChild::new(child);
        std::fs::write(root.join("child-pid"), _owned_child.id().to_string())
            .expect("record recovery failure probe child pid");
        panic!("intentional recovery fixture failure-scope probe");
    }

    let root = tempfile::tempdir().expect("create recovery failure-scope parent directory");
    let output = std::process::Command::new(
        std::env::current_exe().expect("resolve recovery failure-scope test executable"),
    )
    .arg(
        "tests::restate_recovery_tests::restate_recovery_failure_reaps_child_before_aborting_process",
    )
    .arg("--exact")
    .arg("--nocapture")
    .env(CHILD_ENV, "1")
    .env(ROOT_ENV, root.path())
    .output()
    .expect("run recovery failure-scope child");
    assert_eq!(
        output.status.signal(),
        Some(6),
        "recovery fixture failure must abort its libtest process: {output:#?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("intentional recovery fixture failure-scope probe")
            && stderr.contains("after child teardown and before replay storage cleanup"),
        "recovery failure-scope child missed its teardown/abort boundary: {stderr}"
    );
    let child_pid = std::fs::read_to_string(root.path().join("child-pid"))
        .expect("read recovery failure probe child pid");
    assert!(
        !std::path::Path::new("/proc")
            .join(child_pid.trim())
            .exists(),
        "recovery fixture failure left child {} alive",
        child_pid.trim()
    );
    let retained = root.path().join("retained-recovery-store");
    assert!(
        retained.exists(),
        "recovery fixture abort must retain replay storage for gate teardown"
    );
    std::fs::remove_dir(&retained).expect("remove recovery failure probe storage");
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_suspended_sleep_cancel_wakes_and_streams_evidence() {
    run_async_test_on_stack_budget_multi_thread("workbench-suspended-sleep-cancel", 4, || {
        live_restate_suspended_sleep_cancel_wakes_and_streams_evidence_inner()
    });
}

async fn live_restate_suspended_sleep_cancel_wakes_and_streams_evidence_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-suspended-sleep-cancel-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create suspended sleep E2E data dir");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-suspended-sleep-cancel-e2e")
        .complete(|_| async {
            Ok(text_response(
                "<typescript>\nawait sleep(300000);\nfinish(\"unreachable\");\n</typescript>",
            ))
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let turn =
        run_workbench_turn_via_restate(&harness.state, "cancel this suspended durable sleep").await;
    let invocation_id = lash_turn_invocation(&harness.state, &turn, Duration::from_secs(30)).await;
    wait_for_workbench_restate_invocation_suspended(
        &harness.state,
        &invocation_id,
        Duration::from_secs(90),
    )
    .await;
    let session_id = harness.state.current_session_id();
    let active_turns = harness.state.active_turns.for_session(&session_id);
    let [routed_address] = active_turns.as_slice() else {
        panic!("expected exactly one routed suspended turn")
    };
    let routed_address = routed_address.clone();
    let session = harness
        .state
        .core
        .session(&session_id)
        .open()
        .await
        .expect("open suspended turn session for durable address");
    let address = session.turn_address(&routed_address.address.turn_id);
    drop(session);
    let mut events = harness.state.event_tx.subscribe(&session_id);
    let started = tokio::time::Instant::now();
    let receipts = harness
        .state
        .cancel_turns_for_session(&session_id)
        .await
        .expect("cancel suspended workbench turn");
    let [receipt] = receipts.as_slice() else {
        panic!("expected one suspended-turn cancellation receipt")
    };
    let evidence = match receipt {
        TurnCancelReceipt::TerminalAttached { cancellation, .. }
        | TurnCancelReceipt::CancellationRecordedTerminalPending { cancellation, .. } => {
            cancellation.evidence().clone()
        }
        other => panic!("suspended-turn cancellation did not win: {other:?}"),
    };
    let terminal = tokio::time::timeout(
        Duration::from_secs(10),
        harness
            .state
            .core
            .turn_work_driver()
            .await_terminal(&address),
    )
    .await
    .expect("suspended sleep cancellation must not wait for the 300-second timer")
    .expect("attach suspended sleep terminal");
    assert!(matches!(
        terminal,
        lash::TurnTerminal::Committed {
            outcome:
                lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled {
                    evidence: ref terminal_evidence,
                }),
            ..
        } if terminal_evidence == &evidence
    ));
    assert!(started.elapsed() < Duration::from_secs(10));

    let expected_message = format!("turn stopped · request {}", evidence.request_id);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await,
                Ok(ProductEvent {
                    item: StreamItem::Message { ref message },
                    ..
                }) if message.text == expected_message
            ) {
                break;
            }
        }
    })
    .await
    .expect("late cancellation evidence must arrive on the SSE product stream");
    wait_for_active_turns_empty(&harness.state, &session_id, Duration::from_secs(10)).await;
    println!("workbench suspended-sleep gate passed: post-suspension-cancel; late-SSE-evidence");
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_stop_over_process_await_commits_cancelled_and_streams_evidence() {
    run_async_test_on_stack_budget_multi_thread("workbench-stop-over-process-await", 4, || {
        live_restate_stop_over_process_await_commits_cancelled_and_streams_evidence_inner()
    });
}

async fn live_restate_stop_over_process_await_commits_cancelled_and_streams_evidence_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-stop-over-process-await-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create Stop-over-process E2E data dir");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-stop-over-process-await-e2e")
        .complete(|_| async {
            Ok(text_response(
                r#"<typescript>
const hold_for_stop = async () => {
  let elapsed_seconds = 0;
  while (elapsed_seconds < 300) {
    await sleep(1000);
    elapsed_seconds = elapsed_seconds + 1;
  }
  return "unreachable";
};
const handle = await processes.start({ definition: hold_for_stop, label: "hold_for_stop" });
finish(await handle);
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let turn = run_workbench_turn_via_restate(
        &harness.state,
        "start and await the held process, then accept Stop",
    )
    .await;
    let invocation_id = lash_turn_invocation(&harness.state, &turn, Duration::from_secs(30)).await;
    wait_for_workbench_restate_invocation_suspended(
        &harness.state,
        &invocation_id,
        Duration::from_secs(90),
    )
    .await;
    let process_id =
        wait_for_running_process(&harness.state, "hold_for_stop", Duration::from_secs(20)).await;
    let session_id = harness.state.current_session_id();
    let active_turns = harness.state.active_turns.for_session(&session_id);
    let [routed_address] = active_turns.as_slice() else {
        panic!("expected exactly one routed suspended turn")
    };
    let routed_address = routed_address.clone();
    let session = harness
        .state
        .core
        .session(&session_id)
        .open()
        .await
        .expect("open suspended turn session for durable address");
    let address = session.turn_address(&routed_address.address.turn_id);
    drop(session);
    let mut events = harness.state.event_tx.subscribe(&session_id);
    let started = tokio::time::Instant::now();
    let receipts = harness
        .state
        .cancel_turns_for_session(&session_id)
        .await
        .expect("cancel suspended workbench turn");
    let [receipt] = receipts.as_slice() else {
        panic!("expected one suspended-turn cancellation receipt")
    };
    let evidence = match receipt {
        TurnCancelReceipt::TerminalAttached { cancellation, .. }
        | TurnCancelReceipt::CancellationRecordedTerminalPending { cancellation, .. } => {
            cancellation.evidence().clone()
        }
        other => panic!("suspended-turn cancellation did not win: {other:?}"),
    };
    let terminal = tokio::time::timeout(
        Duration::from_secs(10),
        harness
            .state
            .core
            .turn_work_driver()
            .await_terminal(&address),
    )
    .await
    .expect("Stop-over-process cancellation must not wait for the 300-second process budget")
    .expect("attach Stop-over-process turn terminal");
    assert!(matches!(
        terminal,
        lash::TurnTerminal::Committed {
            outcome:
                lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled {
                    evidence: ref terminal_evidence,
                }),
            ..
        } if terminal_evidence == &evidence
    ));
    assert!(started.elapsed() < Duration::from_secs(10));

    let expected_message = format!("turn stopped · request {}", evidence.request_id);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await,
                Ok(ProductEvent {
                    item: StreamItem::Message { ref message },
                    ..
                }) if message.text == expected_message
            ) {
                break;
            }
        }
    })
    .await
    .expect("late cancellation evidence must arrive on the SSE product stream");
    let process_terminal = tokio::time::timeout(
        Duration::from_secs(10),
        harness.state.core.processes().await_output(&process_id),
    )
    .await
    .expect("Stop-over-process must terminate the awaited process")
    .expect("attach awaited process terminal");
    assert!(
        matches!(
            process_terminal,
            lash::process::ProcessAwaitOutput::Settled { ref output }
                if !output.is_success()
                    && output.value_for_projection()["source"] == "cancellation"
        ),
        "Stop-over-process settled the process incorrectly: {process_terminal:#?}"
    );
    wait_for_active_turns_empty(&harness.state, &session_id, Duration::from_secs(10)).await;
    println!(
        "workbench Stop-over-process gate passed: committed-Cancelled; process-Cancelled; late-SSE-evidence"
    );
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_turn_input_ingress_delivers_once_and_queues_after_settle() {
    run_async_test_on_stack_budget_multi_thread("workbench-turn-ingress-e2e", 4, || {
        live_restate_turn_input_ingress_delivers_once_and_queues_after_settle_inner()
    });
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_processes_outlive_session_delete_and_cancel_globally() {
    run_async_test_on_stack_budget_multi_thread("workbench-process-lifecycle-e2e", 4, || {
        live_restate_processes_outlive_session_delete_and_cancel_globally_inner()
    });
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_session_delete_revokes_process_await_without_cancelling_process() {
    run_async_test_on_stack_budget_multi_thread("workbench-revoked-process-await", 4, || {
        live_restate_session_delete_revokes_process_await_without_cancelling_process_inner()
    });
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_terminal_session_delete_failure_keeps_the_session_live() {
    run_async_test_on_stack_budget_multi_thread("workbench-delete-failure-e2e", 4, || {
        live_restate_terminal_session_delete_failure_keeps_the_session_live_inner()
    });
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_provider_auth_failure_terminalizes_and_session_recovers() {
    run_async_test_on_stack_budget_multi_thread("workbench-auth-failure-e2e", 4, || {
        live_restate_provider_auth_failure_terminalizes_and_session_recovers_inner()
    });
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_rate_limit_retry_converges_observers_to_one_copy() {
    run_async_test_on_stack_budget_multi_thread("workbench-rate-limit-retry-e2e", 4, || {
        live_restate_rate_limit_retry_converges_observers_to_one_copy_inner()
    });
}

async fn live_restate_provider_auth_failure_terminalizes_and_session_recovers_inner() {
    let (harness, data_dir) = live_failure_path_harness(
        "auth-failure",
        failure_provider::DevProviderScenario::AuthFailureOnce,
    )
    .await;
    let session_id = harness.state.current_session_id();
    let mut product_events = harness.state.event_tx.subscribe(&session_id);
    let (mut failed_turn, failed_address) =
        submit_workbench_turn_via_restate(&harness.state, "trigger deterministic auth failure")
            .await;
    let terminal = harness
        .state
        .core
        .turn_work_driver()
        .await_terminal_with_timeout(&failed_address, Duration::from_secs(20))
        .await
        .expect("auth failure must publish a turn terminal");
    let lash::TurnTerminal::Committed { outcome, .. } = terminal else {
        panic!("provider auth failure did not settle through the turn contract: {terminal:#?}");
    };
    assert_eq!(
        outcome,
        lash::TurnOutcome::Stopped(lash::TurnStop::ProviderError)
    );
    assert_eq!(
        outcome.cancellation(),
        None,
        "provider failure is not cancellation"
    );
    // The honest ProviderError result is a settled turn: its follower records
    // it and releases the route.
    wait_for_workbench_turn_settled(&mut failed_turn, Duration::from_secs(20)).await;
    assert!(
        harness
            .state
            .active_turns
            .for_session(&failed_address.session_id)
            .is_none(),
        "failed turn left a dangling active route"
    );

    let mut rendered_failure = None;
    let mut saw_done = false;
    while rendered_failure.is_none() || !saw_done {
        let event = tokio::time::timeout(Duration::from_secs(5), product_events.recv())
            .await
            .expect("failure transcript event timeout")
            .expect("failure transcript event");
        match event.item {
            StreamItem::Message { message } if message.role == "event" => {
                rendered_failure = Some(message.text)
            }
            StreamItem::Done { .. } => saw_done = true,
            _ => {}
        }
    }
    assert_eq!(
        rendered_failure.as_deref(),
        Some("turn could not be completed"),
        "product transcript must expose safe failure copy"
    );
    assert!(harness.state.messages_snapshot().iter().any(|message| {
        message.role == "event"
            && message.text == "turn could not be completed"
            && !message.text.to_ascii_lowercase().contains("cancelled")
    }));

    let (mut recovery_turn, recovery_address) = submit_workbench_turn_via_restate(
        &harness.state,
        "prove the same session accepts the next turn",
    )
    .await;
    wait_for_workbench_turn_settled(&mut recovery_turn, Duration::from_secs(20)).await;
    let recovery_terminal = harness
        .state
        .core
        .turn_work_driver()
        .await_terminal_with_timeout(&recovery_address, Duration::from_secs(20))
        .await
        .expect("recovery turn terminal");
    assert!(
        matches!(recovery_terminal, lash::TurnTerminal::Committed { .. }),
        "next turn did not recover: {recovery_terminal:#?}"
    );
    wait_for_workbench_message(
        &harness.state,
        "session recovered after provider auth failure",
        Duration::from_secs(20),
    )
    .await;
    println!(
        "workbench auth-failure gate passed: failed terminal, visible error, next turn recovered"
    );
    harness.shutdown(data_dir).await;
}

async fn submit_workbench_turn_via_restate(
    state: &AppState,
    text: &str,
) -> (WorkbenchTurn, lash::TurnAddress) {
    let turn = run_workbench_turn_via_restate(state, text).await;
    let session = state
        .core
        .session(&turn.session_id)
        .open()
        .await
        .expect("open workbench session for durable turn address");
    let address = session.turn_address(&turn.turn_id);
    drop(session);
    (turn, address)
}

async fn wait_for_restate_invocation_completion(
    state: &AppState,
    invocation_id: &lash_restate::RestateInvocationId,
    timeout: Duration,
) -> lash_restate::RestateInvocationStatus {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
            state.restate_http.clone(),
        ));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = admin
            .invocation_status(invocation_id)
            .await
            .expect("query Restate invocation status")
            && status.status == lash_restate::RestateInvocationLifecycle::Completed
        {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for Restate invocation {invocation_id} to complete"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn live_restate_rate_limit_retry_converges_observers_to_one_copy_inner() {
    let (harness, data_dir) = live_failure_path_harness(
        "rate-limit-retry",
        failure_provider::DevProviderScenario::RateLimitOnce,
    )
    .await;
    let session = harness
        .state
        .core
        .session(harness.state.current_session_id())
        .open()
        .await
        .expect("open observer session");
    let cursor = session.observe().current_observation().cursor;
    let lash::observe::SessionObservationSubscription::Subscribed(mut subscription) = session
        .observe()
        .subscribe_from_cursor(&cursor)
        .expect("subscribe before retry turn")
    else {
        panic!("fresh observer cursor unexpectedly had a replay gap");
    };
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        let mut saw_turn_activity = false;
        loop {
            let event = tokio::time::timeout(
                Duration::from_secs(20),
                futures_util::StreamExt::next(&mut subscription),
            )
            .await
            .expect("retry observer event timeout")
            .expect("retry observer subscription closed")
            .expect("retry observer event");
            saw_turn_activity |= matches!(
                event.payload,
                lash::observe::SessionObservationEventPayload::TurnActivity(_)
            );
            let committed = matches!(
                event.payload,
                lash::observe::SessionObservationEventPayload::Committed { .. }
            );
            events.push(event);
            if committed && saw_turn_activity {
                return events;
            }
        }
    });

    let (mut turn, address) =
        submit_workbench_turn_via_restate(&harness.state, "trigger deterministic rate limit retry")
            .await;
    wait_for_workbench_turn_settled(&mut turn, Duration::from_secs(20)).await;
    let terminal = harness
        .state
        .core
        .turn_work_driver()
        .await_terminal_with_timeout(&address, Duration::from_secs(20))
        .await
        .expect("retry turn terminal");
    assert!(
        matches!(terminal, lash::TurnTerminal::Committed { .. }),
        "successful retry did not commit: {terminal:#?}"
    );
    let live_events = collector.await.expect("retry observer collector");
    let lash::observe::SessionResume::Replayed {
        events: replay_events,
    } = session
        .observe()
        .resume_from_cursor(&cursor)
        .expect("replay retry events")
    else {
        panic!("retry events should remain in bounded replay");
    };
    for (label, events) in [("live", &live_events), ("replay", &replay_events)] {
        let (prose, resets) = fold_retry_observer_prose(events);
        assert_eq!(
            resets, 1,
            "{label} observer missed ModelAttemptReset: {events:#?}"
        );
        assert_eq!(
            prose.matches("retry observer single-copy marker").count(),
            1,
            "{label} observer retained duplicate retry prose: {prose:?}"
        );
    }
    wait_for_workbench_message(
        &harness.state,
        "provider retry succeeded",
        Duration::from_secs(20),
    )
    .await;
    let assistant = harness
        .state
        .messages_snapshot()
        .into_iter()
        .find(|message| message.role == "assistant")
        .expect("retry assistant transcript");
    assert_eq!(
        assistant
            .text
            .matches("retry observer single-copy marker")
            .count(),
        1,
        "workbench transcript retained duplicate retry prose: {:?}",
        assistant.text
    );
    let session_id = session.session_id();
    // This handle predates the externally driven Restate turn. Dropping it
    // releases the observation snapshot without flushing that stale graph over
    // the workflow's committed transcript.
    drop(session);
    let committed = harness
        .state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open committed retry session");
    assert_single_retry_marker_message("committed", committed.read_view().messages());
    committed
        .close()
        .await
        .expect("close committed retry session");
    let reloaded = harness
        .state
        .core
        .session(session_id)
        .open()
        .await
        .expect("reload retry session");
    assert_single_retry_marker_message("reload", reloaded.read_view().messages());
    reloaded
        .close()
        .await
        .expect("close reloaded retry session");
    println!(
        "workbench rate-limit gate passed: retry succeeded and live/replay observers converged"
    );
    harness.shutdown(data_dir).await;
}

fn assert_single_retry_marker_message(projection: &str, messages: &[lash::messages::Message]) {
    // Settled: no turn of this session is running any more, so a protocol-owned
    // reply would be admitted here if one stood as this turn's answer. The
    // workbench's own committed copy follows it, so none does (FIG-1406).
    let rlm_reply_ids = durable_rlm_reply_message_ids(messages, &BTreeSet::new());
    let messages = messages
        .iter()
        .filter_map(|message| project_committed_chat_message(message, &rlm_reply_ids))
        .collect::<Vec<_>>();
    let marker_messages = messages
        .iter()
        .filter(|message| {
            message.role == "assistant"
                && message.text.contains("retry observer single-copy marker")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        marker_messages.len(),
        1,
        "{projection} workbench projection duplicated retry prose: {messages:#?}"
    );
    assert!(
        marker_messages[0].id.starts_with("workbench-assistant:"),
        "{projection} workbench projection retained protocol-owned prose: {messages:#?}"
    );
}

fn fold_retry_observer_prose(
    events: &[Arc<lash::observe::SessionObservationEvent>],
) -> (String, usize) {
    let projection = fold_turn_activities(events.iter().filter_map(|event| {
        let lash::observe::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            return None;
        };
        Some(activity)
    }));
    (
        projection.assistant_prose(),
        projection.model_attempt_reset_count(),
    )
}

async fn live_failure_path_harness(
    label: &str,
    scenario: failure_provider::DevProviderScenario,
) -> (LiveFailurePathHarness, PathBuf) {
    live_failure_path_harness_with_provider(label, scenario.provider()).await
}

async fn live_failure_path_harness_with_provider(
    label: &str,
    provider: ProviderHandle,
) -> (LiveFailurePathHarness, PathBuf) {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-{label}-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create failure-path E2E data dir");
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let state = harness.state.clone();
    let endpoint = LiveRestateEndpoint::start(
        &admin_url,
        state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;
    (LiveFailurePathHarness { state, endpoint }, data_dir)
}

struct LiveFailurePathHarness {
    pub(super) state: AppState,
    endpoint: LiveRestateEndpoint,
}

impl LiveFailurePathHarness {
    async fn shutdown(mut self, data_dir: PathBuf) {
        self.endpoint
            .stop_after_producers_closed_and_drained(&self.state, Duration::from_secs(30))
            .await;
        std::fs::remove_dir_all(&data_dir)
            .unwrap_or_else(|error| panic!("remove owned fixture {}: {error}", data_dir.display()));
        assert!(
            !data_dir.exists(),
            "owned fixture data directory remained at {}",
            data_dir.display()
        );
    }
}

async fn live_restate_terminal_session_delete_failure_keeps_the_session_live_inner() {
    // An orphan active-turn claim: a registry row with no workflow behind it,
    // the shape a turn leaves between ingress claiming the slot and its
    // workflow journaling any awaits. Await-gate revocation cannot settle it,
    // so the delete workflow exhausts its bounded settle window and fails
    // terminally with the session still live. (A turn genuinely held inside a
    // real workflow settles under revocation and lets the delete succeed --
    // the revokes E2E test covers that path; the route additionally sweeps
    // orphans through its cooperative cancel.)
    let (harness, data_dir) = live_failure_path_harness(
        "delete-failure",
        failure_provider::DevProviderScenario::RenderedSurface,
    )
    .await;
    let session_id = harness.state.current_session_id();
    harness
        .state
        .open_session(&session_id, "test")
        .await
        .expect("materialize the session before its failed delete");
    harness
        .state
        .active_turns
        .insert(&session_id, "held-delete-turn", WorkbenchTurnKind::User);

    // Submit the durable delete workflow directly -- the redrive path a
    // delete whose submitting process died leaves behind. The route would
    // first sweep the orphan claim through its cooperative cancel; the
    // workflow alone must reach its own bounded terminal failure.
    let execution_scope = lash::runtime::ExecutionScope::session_delete(&session_id);
    let delete_invocation_id = restate::submit_session_delete(
        &harness.state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: session_id.clone(),
            execution_scope,
        },
    )
    .await
    .expect("submit deletion against the orphan active-turn claim");
    let delete_status = wait_for_restate_invocation_completion(
        &harness.state,
        &delete_invocation_id,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        !delete_status.completed_successfully(),
        "the unsettleable active-turn claim must fail the real delete workflow: {delete_status:#?}"
    );
    assert!(
        delete_status
            .completion_failure
            .as_deref()
            .is_some_and(
                |failure| failure.contains(session_id.as_str()) && failure.contains("remains live")
            ),
        "the delete must fail with the session-remains-live refusal: {delete_status:#?}"
    );
    assert_eq!(harness.state.current_session_id(), session_id);
    assert!(
        !harness
            .state
            .core
            .session(session_id.clone())
            .durable()
            .await
            .expect("durable handle for the session")
            .was_deleted()
            .await
            .expect("read failed-delete tombstone fence")
    );
    assert_eq!(
        harness.state.active_turns.retirement(&session_id),
        None,
        "a terminal delete failure lifts the in-process fence"
    );
    let _ = app_state(
        State(harness.state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("the actual terminal failure leaves GET /api/state live");

    // Drop the orphan claim: the delete can now be retried through the same
    // route, and the fence admits the retry.
    harness
        .state
        .active_turns
        .remove(&session_id, &TurnId::from("held-delete-turn"));
    let post_tombstone_turn = "turn-admitted-after-delete-snapshot";
    fail_session_delete_retention_once(&session_id, &TurnId::from(post_tombstone_turn));
    let Json(replacement) = Box::pin(tokio::time::timeout(
        Duration::from_secs(30),
        reset_chat(
            State(harness.state.clone()),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
        ),
    ))
    .await
    .expect("post-tombstone retention redrive settles")
    .expect("retry succeeds after the orphan claim is released");
    assert_ne!(replacement.settings.session_id, session_id);
    harness
        .state
        .active_turns
        .remove(&session_id, &TurnId::from(post_tombstone_turn));
    assert!(
        harness
            .state
            .core
            .session(session_id.clone())
            .durable()
            .await
            .expect("durable handle for the session")
            .was_deleted()
            .await
            .expect("read successful retry tombstone fence")
    );
    harness.shutdown(data_dir).await;
}

/// How long the surviving process sleeps before it settles.
const REVOKED_PROCESS_AWAIT_SLEEP: Duration = Duration::from_secs(90);
/// Slack past the process's own sleep for it to wake, settle and publish.
const REVOKED_PROCESS_AWAIT_SETTLE_MARGIN: Duration = Duration::from_secs(45);

async fn live_restate_session_delete_revokes_process_await_without_cancelling_process_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-revoked-process-await-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create revoked process-await E2E data dir");

    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-revoked-process-await-e2e")
        .complete(|_| async {
            Ok(text_response(&format!(
                r#"<typescript>
const survive_revocation = async () => {{
  await sleep({sleep_ms});
  return "survived session deletion";
}};
const handle = await processes.start({{ definition: survive_revocation, label: "survive_revocation" }});
finish(await handle);
</typescript>"#,
                sleep_ms = REVOKED_PROCESS_AWAIT_SLEEP.as_millis(),
            )))
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let deleted_session_id = harness.state.current_session_id();
    let mut turn = run_workbench_turn_via_restate(
        &harness.state,
        "await a process while this session is deleted",
    )
    .await;
    let turn_invocation_id =
        lash_turn_invocation(&harness.state, &turn, Duration::from_secs(30)).await;
    wait_for_workbench_restate_invocation_suspended(
        &harness.state,
        &turn_invocation_id,
        Duration::from_secs(90),
    )
    .await;
    let process_id = wait_for_running_process(
        &harness.state,
        "survive_revocation",
        Duration::from_secs(20),
    )
    .await;
    // The process was already sleeping when it was seen running, so its own
    // sleep bounds when it settles. How soon the session delete lands is
    // Restate's business: on its default inactivity timeout the turn reads as
    // suspended after about a minute, and with every await suspending it does
    // at once, so the terminal deadline must come from the process, not from
    // the turn.
    let process_settles_by = tokio::time::Instant::now()
        + REVOKED_PROCESS_AWAIT_SLEEP
        + REVOKED_PROCESS_AWAIT_SETTLE_MARGIN;

    let execution_scope = lash::runtime::ExecutionScope::session_delete(&deleted_session_id);
    let delete_invocation_id = restate::submit_session_delete(
        &harness.state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: deleted_session_id.clone(),
            execution_scope,
        },
    )
    .await
    .expect("submit deletion while the foreground turn awaits a process");
    wait_for_restate_invocation_success(
        &harness.state,
        &delete_invocation_id,
        Duration::from_secs(20),
    )
    .await;
    let immediately_after_delete = harness
        .state
        .process_observer
        .clone()
        .process(&process_id.clone())
        .await
        .expect("read process immediately after session revocation")
        .expect("session revocation keeps its process record");
    assert!(
        !immediately_after_delete.terminal(),
        "session revocation must leave the independently sleeping process live"
    );
    let immediate_events = complete_full_process_event_page(
        harness
            .state
            .process_observer
            .first_event_page(
                &process_id.clone(),
                std::num::NonZeroUsize::new(4_096).expect("non-zero test page size"),
                lash::process::ProcessEventQueryMode::Full,
            )
            .await
            .expect("read process events immediately after session revocation"),
    );
    assert!(
        !immediate_events
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"),
        "session revocation immediately emitted a process cancel: {immediate_events:#?}"
    );

    let turn_failure = wait_for_workbench_turn_failed(&mut turn, Duration::from_secs(20)).await;
    // Re-baselined for FIG-2358: the revoked turn resumes while the delete
    // workflow is inside its bounded settle window, so the session fence
    // refuses it as retiring ("is being deleted"); a turn that resumes after
    // the tombstone commits gets the deleted refusal ("used and deleted").
    // Either way it is the shared typed retirement refusal, never success.
    assert!(
        turn_failure.contains("used and deleted") || turn_failure.contains("is being deleted"),
        "revoked turn must terminalize as the typed retirement refusal: {turn_failure}"
    );
    let process_terminal = tokio::time::timeout_at(
        process_settles_by,
        harness
            .state
            .core
            .processes()
            .await_output(&process_id.clone()),
    )
    .await
    .expect("revoked session must not stop the independent process")
    .expect("attach surviving process terminal");
    assert!(
        matches!(
            &process_terminal,
            lash::process::ProcessAwaitOutput::Settled { output }
                if output.is_success()
                    && output.value_for_projection() == json!("survived session deletion")
        ),
        "session revocation changed the process terminal: {process_terminal:#?}"
    );
    let events = complete_full_process_event_page(
        harness
            .state
            .process_observer
            .first_event_page(
                &process_id,
                std::num::NonZeroUsize::new(4_096).expect("non-zero test page size"),
                lash::process::ProcessEventQueryMode::Full,
            )
            .await
            .expect("read surviving process events"),
    );
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"),
        "session revocation emitted a process cancel: {events:#?}"
    );
    assert!(
        harness
            .state
            .active_turns
            .for_session(&deleted_session_id)
            .is_none(),
        "deleted-session settlement left a routed foreground turn"
    );
    println!(
        "workbench revoked-process-await gate passed: typed-SessionDeleted; no-process-cancel; process-survived"
    );
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

async fn live_restate_processes_outlive_session_delete_and_cancel_globally_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-process-lifecycle-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create process lifecycle E2E data dir");

    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-process-lifecycle-e2e")
        .complete(|_| async {
            Ok(text_response(
                r#"<typescript>
const survivor = async () => {
  await sleep(8000);
  return "survived session deletion";
};
const cancellable = async () => {
  await sleep(60000);
  return "cancellation failed";
};
const survivor_handle = await processes.start({ definition: survivor, label: "survivor" });
const cancellable_handle = await processes.start({ definition: cancellable, label: "cancellable" });
finish("started lifecycle gates");
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let deleted_session_id = harness.state.current_session_id();
    let mut turn =
        run_workbench_turn_via_restate(&harness.state, "start process lifecycle gates").await;
    wait_for_workbench_turn_settled(&mut turn, Duration::from_secs(30)).await;
    let (survivor_id, cancellable_id) = wait_for_named_running_processes(
        &harness.state,
        &["survivor", "cancellable"],
        Duration::from_secs(20),
    )
    .await;

    let execution_scope = lash::runtime::ExecutionScope::session_delete(&deleted_session_id);
    let delete_invocation_id = restate::submit_session_delete(
        &harness.state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: deleted_session_id.clone(),
            execution_scope,
        },
    )
    .await
    .expect("submit session deletion while processes run");
    wait_for_restate_invocation_success(
        &harness.state,
        &delete_invocation_id,
        Duration::from_secs(20),
    )
    .await;

    let Json(work_after_delete) =
        list_work(State(harness.state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list runtime work after session deletion");
    for process_id in [&survivor_id, &cancellable_id] {
        assert!(
            work_after_delete
                .iter()
                .any(|item| item.process.process_id == *process_id && !item.process.terminal),
            "work rail lost live process {process_id} after deleting {deleted_session_id}: {work_after_delete:#?}"
        );
    }

    let Json(cancel_receipt) = cancel_work(
        AxumPath(cancellable_id.to_string()),
        State(harness.state.clone()),
    )
    .await
    .expect("cancel orphaned process through work API");
    assert!(cancel_receipt.accepted);
    wait_for_process_event(
        &harness.state,
        &cancellable_id.clone(),
        "process.cancel_requested",
        Duration::from_secs(20),
    )
    .await;
    let cancelled = tokio::time::timeout(
        Duration::from_secs(20),
        harness
            .state
            .core
            .processes()
            .await_output(&cancellable_id.clone()),
    )
    .await
    .expect("cancelled process terminal timeout")
    .expect("await cancelled process");
    assert!(
        matches!(
            cancelled,
            lash::process::ProcessAwaitOutput::Settled { ref output }
                if !output.is_success()
                    && output.value_for_projection()["source"] == "cancellation"
        ),
        "process cancellation settled with the wrong outcome: {cancelled:#?}"
    );

    let survived = tokio::time::timeout(
        Duration::from_secs(20),
        harness
            .state
            .core
            .processes()
            .await_output(&survivor_id.clone()),
    )
    .await
    .expect("surviving process terminal timeout")
    .expect("await surviving process");
    assert!(
        matches!(
            &survived,
            lash::process::ProcessAwaitOutput::Settled { output }
                if output.is_success()
                    && output.value_for_projection() == json!("survived session deletion")
        ),
        "session-independent process did not complete successfully: {survived:#?}"
    );
    let Json(terminal_work) =
        list_work(State(harness.state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list terminal runtime work");
    assert!(terminal_work.iter().any(|item| {
        item.process.process_id == survivor_id
            && item.process.terminal
            && item.process.lifecycle == lash::process::ProcessStatus::Completed
    }));
    assert!(terminal_work.iter().any(|item| {
        item.process.process_id == cancellable_id
            && item.process.terminal
            && item.process.lifecycle == lash::process::ProcessStatus::Cancelled
            && item
                .events
                .iter()
                .any(|event| event.event_type == "process.cancel_requested")
    }));
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

async fn wait_for_running_process(state: &AppState, label: &str, timeout: Duration) -> ProcessId {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let processes = state
            .process_observer
            .list(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::any_of([
                    lash::process::ProcessStatus::Running,
                ]),
                ..lash::process::ProcessListFilter::default()
            })
            .await
            .expect("list running processes");
        if let Some(process) = processes
            .iter()
            .find(|process| process.identity.label.as_deref() == Some(label))
        {
            return process.process_id.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for running process {label:?}; observed={processes:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_named_running_processes(
    state: &AppState,
    labels: &[&str],
    timeout: Duration,
) -> (ProcessId, ProcessId) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let processes = state
            .process_observer
            .list(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::any_of([
                    lash::process::ProcessStatus::Running,
                ]),
                ..lash::process::ProcessListFilter::default()
            })
            .await
            .expect("list running processes");
        let named = labels
            .iter()
            .filter_map(|label| {
                processes
                    .iter()
                    .find(|process| process.identity.label.as_deref() == Some(*label))
                    .map(|process| process.process_id.clone())
            })
            .collect::<Vec<_>>();
        if named.len() == labels.len() {
            return (named[0].clone(), named[1].clone());
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for named running processes {labels:?}; observed={processes:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_process_event(
    state: &AppState,
    process_id: &ProcessId,
    event_type: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let events = complete_full_process_event_page(
            state
                .process_observer
                .first_event_page(
                    process_id,
                    std::num::NonZeroUsize::new(4_096).expect("non-zero test page size"),
                    lash::process::ProcessEventQueryMode::Full,
                )
                .await
                .expect("read process events"),
        );
        if events.iter().any(|event| event.event_type == event_type) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {event_type} on {process_id}; events={events:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn live_restate_turn_input_ingress_delivers_once_and_queues_after_settle_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-turn-ingress-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create turn ingress E2E data dir");
    let sessions = WorkbenchSessions::fresh();
    let session_id = sessions.current();
    let admission_gate = Arc::new(SessionOpenAdmissionGate::new(&session_id));
    register_session_open_admission_gate(Arc::clone(&admission_gate));

    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_provider = Arc::clone(&requests);
    let (provider_call_tx, mut provider_call_rx) = mpsc::unbounded_channel::<usize>();
    let release_first_provider_call = Arc::new(tokio::sync::Notify::new());
    let release_first_provider_call_for_provider = Arc::clone(&release_first_provider_call);
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_provider = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-turn-ingress-e2e")
        .complete(move |request| {
            let requests = Arc::clone(&requests_for_provider);
            let provider_call_tx = provider_call_tx.clone();
            let release_first_provider_call = Arc::clone(&release_first_provider_call_for_provider);
            let response_index = Arc::clone(&response_index_for_provider);
            async move {
                let serialized =
                    serde_json::to_string(&request).expect("serialize provider request");
                requests.lock_recover().push(serialized);
                let call_index = response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = provider_call_tx.send(call_index);
                if call_index == 0 {
                    release_first_provider_call.notified().await;
                }
                Ok(match call_index {
                    0 => text_response("<typescript>\nawait sleep(2000);\n</typescript>"),
                    1 => text_response(
                        "<typescript>\nfinish(\"current turn settled\");\n</typescript>",
                    ),
                    2 => text_response(
                        "<typescript>\nfinish(\"queued turn settled\");\n</typescript>",
                    ),
                    other => panic!("unexpected provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let harness = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        sessions,
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint = LiveRestateEndpoint::start(
        &admin_url,
        harness.state.clone(),
        harness.backend,
        harness.process_worker,
    )
    .await;

    let mut rendered_events = harness
        .state
        .event_tx
        .subscribe(&harness.state.current_session_id());

    let mut turn =
        run_workbench_turn_via_restate(&harness.state, "initial turn input_ingress_gate=true")
            .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), provider_call_rx.recv())
            .await
            .expect("first provider call timeout"),
        Some(0)
    );

    let Json(injected) = enqueue_turn_input(
        State(harness.state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "active injection marker".to_string(),
            ingress: TurnInputIngressRequest::ActiveTurn,
        }),
    )
    .await
    .expect("enqueue active-turn input through workbench API");
    let Json(queued) = enqueue_turn_input(
        State(harness.state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "queued next marker".to_string(),
            ingress: TurnInputIngressRequest::NextTurn,
        }),
    )
    .await
    .expect("enqueue next-turn input through workbench API");
    assert!(matches!(
        injected.ingress,
        lash::persistence::TurnInputIngress::ActiveTurn { .. }
    ));
    assert!(matches!(
        queued.ingress,
        lash::persistence::TurnInputIngress::NextTurn
    ));
    release_first_provider_call.notify_one();

    for expected in [1, 2] {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), provider_call_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("provider call {expected} timeout")),
            Some(expected)
        );
    }
    wait_for_workbench_turn_settled(&mut turn, Duration::from_secs(30)).await;
    // The next-turn input is a root the session's engine starts on its own:
    // no route follows it, and its reply reaches the page through the
    // committed transcript.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let snapshot = loop {
        let Json(snapshot) = Box::pin(app_state(
            State(harness.state.clone()),
            Query(SessionQuery::default()),
        ))
        .await
        .expect("read the workbench state");
        let queued_reply_committed = snapshot.transcript.iter().any(|row| {
            matches!(
                row,
                TranscriptRow::Message { message } if message.text.contains("queued turn settled")
            )
        });
        if snapshot.observation.turn_index >= 2 && queued_reply_committed {
            break snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the queued input's own turn did not commit; turn_index={}",
            snapshot.observation.turn_index
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // The engine-started root's follower settles it through its own open,
    // and the engine's drive opens the session once more for its last
    // admission pass: the gate counts every claim on the session, so it arms
    // once that follower is done and the engine runs nothing on the session.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while harness.state.active_turns.follows.follows_any(&session_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the queued root's follower did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for_session_engine_idle(&admin_url, &session_id, Duration::from_secs(30)).await;
    admission_gate.arm();
    let state_for_holder = harness.state.clone();
    let session_id_for_holder = session_id.clone();
    let held_open = tokio::spawn(async move {
        state_for_holder
            .open_session(&session_id_for_holder, "test")
            .await
    });
    admission_gate.wait_until_admitted().await;
    // The page's reads answer from the durable head and never claim the session
    // execution lease (FIG-3144, FIG-3151), so `/api/state` answers *through* a
    // held admitted open rather than queueing behind it.
    let Json(held_read) = Box::pin(app_state(
        State(harness.state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("a lease-free read must answer while an admitted open is held");
    drop(held_read);
    // The bounded-retry refusal belongs to the surfaces that still take the
    // lease. `DELETE /api/queued-work/<batch>` opens the session before it can
    // look at the batch, so it is the live surface that still answers 503 while
    // the lane is held; the page's reads no longer reach this path at all.
    let exhausted = Box::pin(cancel_queued_work_batch(
        AxumPath("no-such-batch".to_string()),
        State(harness.state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect_err("a held admitted open must exhaust the bounded retry policy");
    assert_eq!(exhausted.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(exhausted.verdict, AppErrorVerdict::Retryable);
    assert_eq!(
        exhausted.message,
        "session is temporarily busy; retry the request"
    );
    let (attempts, acquisitions, admissions, contentions) = admission_gate.counts();
    assert!(
        (1..=SESSION_OPEN_MAX_ATTEMPTS).contains(&contentions),
        "the logical read must stop at the host deadline or attempt cap; contentions={contentions}"
    );
    // Either finite bound may win; both must leave the observed attempts fenced.
    assert_eq!(attempts, contentions + acquisitions);
    assert_eq!(acquisitions, 1, "only the held open may acquire the lane");
    assert_eq!(
        acquisitions, admissions,
        "the held claim must pass admit_session_state and no retry may bypass admission"
    );
    admission_gate.release();
    held_open
        .await
        .expect("join held session open")
        .expect("held admitted open completes after release");
    admission_gate.finish();

    let settled_session = harness
        .state
        .open_session(&session_id, "test")
        .await
        .expect("open settled ingress session through the host retry boundary");
    let read_view = settled_session.read_view();
    assert_eq!(
        read_view.turn_index(),
        2,
        "queued input must commit its own turn"
    );
    drop(settled_session);

    let captured = requests.lock_recover().clone();
    assert_eq!(captured.len(), 3, "unexpected provider request sequence");
    assert!(!captured[0].contains("active injection marker"));
    assert_eq!(
        captured[1].matches("active injection marker").count(),
        1,
        "active-turn input must reach the next provider iteration exactly once"
    );
    let completed_in_running_turn = std::fs::read_to_string(&harness.trace_path)
        .expect("read turn ingress trace")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record.get("name").and_then(Value::as_str) == Some("turn_input.completed")
                && record.pointer("/context/turn_id").and_then(Value::as_str)
                    == injected.ingress.active_turn_id().map(TurnId::as_str)
                && record
                    .pointer("/payload/claims")
                    .and_then(Value::as_array)
                    .is_some_and(|claims| {
                        claims.iter().any(|claim| {
                            claim
                                .get("input_ids")
                                .and_then(Value::as_array)
                                .is_some_and(|ids| {
                                    ids.iter().any(|id| id.as_str() == Some(&injected.input_id))
                                })
                        })
                    })
        })
        .count();
    assert_eq!(
        completed_in_running_turn,
        1,
        "active-turn input must complete exactly once under the in-flight turn id; trace={} ",
        trace_tail(&harness.trace_path)
    );
    assert_eq!(
        captured[2].matches("active injection marker").count(),
        1,
        "later assembled history must contain the committed active input exactly once"
    );
    assert!(!captured[0].contains("queued next marker"));
    assert!(!captured[1].contains("queued next marker"));
    assert_eq!(captured[2].matches("queued next marker").count(), 1);

    assert_eq!(
        snapshot.observation.turn_index, 2,
        "queued input must commit its own turn"
    );
    let committed = snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::Message { message } => Some(message.text.clone()),
            TranscriptRow::Reasoning { .. }
            | TranscriptRow::CodeBlock { .. }
            | TranscriptRow::Note { .. } => None,
        })
        .collect::<Vec<_>>();
    assert!(
        committed
            .iter()
            .any(|text| text.contains("queued next marker")),
        "queued input missing from committed transcript: {committed:#?}"
    );
    assert_eq!(
        committed
            .iter()
            .filter(|text| text.contains("active injection marker"))
            .count(),
        1,
        "active injection must be one committed user message: {committed:#?}"
    );
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|message| {
                message.role == "user" && message.text == "active injection marker"
            })
            .count(),
        1,
        "HTTP transcript must expose the committed active input exactly once"
    );
    let mut rendered_active_input = false;
    loop {
        match rendered_events.try_recv() {
            Ok(ProductEvent {
                item: StreamItem::Message { message },
                ..
            }) if message.role == "user" && message.text == "active injection marker" => {
                rendered_active_input = true;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("rendered turn-ingress event stream lagged by {skipped} items")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    assert!(
        rendered_active_input,
        "rendered page stream must receive the committed active input as a normal user message"
    );
    assert!(
        snapshot.pending_turn_inputs.is_empty(),
        "both ingress claims must settle"
    );
    unregister_session_open_admission_gate(&session_id);
    endpoint
        .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
        .await;
    let _ = std::fs::remove_dir_all(data_dir);
}

async fn live_restate_ingress_owner_restart_resumes_and_remains_cancellable_inner() {
    for backend in ["sqlite", "postgres"] {
        live_restate_ingress_owner_restart_for_store(backend).await;
    }
}

/// Session-lease timings this deployment chooses through
/// [`lash::LashCoreBuilder::lease_timings`].
///
/// The whole point of the knob is failover latency: a dead ingress owner's
/// session-execution lease cannot be superseded until it expires, so the TTL
/// *is* the floor on how long a replacement waits before it can resume the
/// turn. The workbench trades a wider false-takeover window for a fast
/// restart, and the recovery assertion below is written against that trade —
/// it demands a takeover strictly faster than the stock 30s TTL, which only
/// the configured timings can deliver.
fn recovery_e2e_lease_timings() -> lash::durability::LeaseTimings {
    lash::durability::LeaseTimings::new(Duration::from_millis(300), Duration::from_millis(100))
        .expect("valid recovery E2E lease timings")
}

async fn live_restate_ingress_owner_restart_for_store(backend: &'static str) {
    // Declared before every child/store owner so unwinding kills and reaps those
    // resources before this guard stops libtest from entering another fixture.
    let mut failure_scope = AbortRestateFixtureOnPanic::armed("ingress-owner-restart");
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let endpoint_bind_variable = match backend {
        "sqlite" => "AGENT_WORKBENCH_E2E_ENDPOINT_BIND",
        "postgres" => "AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_BIND",
        other => panic!("unsupported recovery E2E backend `{other}`"),
    };
    let endpoint_bind: SocketAddr = std::env::var(endpoint_bind_variable)
        .unwrap_or_else(|_| {
            panic!("{endpoint_bind_variable} must assign a distinct immutable recovery endpoint")
        })
        .parse()
        .unwrap_or_else(|error| panic!("valid {endpoint_bind_variable}: {error}"));
    record_fixture_owned_endpoint(endpoint_bind);
    let endpoint_url = format!("http://{endpoint_bind}");
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-recovery-{backend}-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create recovery E2E data dir");
    let session_id = SessionId::from(format!("workbench-recovery-{backend}-e2e"));
    let turn_id = TurnId::from(format!("workbench-turn-recovery-{backend}-e2e"));
    std::fs::write(data_dir.join("session-id"), session_id.as_str())
        .expect("write recovery E2E session id");

    let mut first = spawn_recovery_e2e_child(&data_dir, endpoint_bind, &ingress_url, backend);
    let first_pid = first.id();
    wait_for_endpoint_socket(endpoint_bind).await;
    let deployment_id = register_restate_deployment(&admin_url, &endpoint_url).await;
    // The first owner sends the turn once its handlers are registered, as the
    // browser's send would reach it.
    std::fs::write(data_dir.join(RECOVERY_E2E_START_TURN), turn_id.as_str())
        .expect("ask the first owner to send the recovery E2E turn");
    let invocation_id = lash_turn_invocation_at(
        &admin_url,
        &lash::TurnAddress::new(&session_id, &turn_id),
        Duration::from_secs(20),
    )
    .await;
    wait_for_provider_owner(&data_dir, first_pid, Duration::from_secs(20)).await;
    let admitted = restate_invocation_status_with_deployment(&admin_url, &invocation_id)
        .await
        .expect("admitted recovery invocation status");
    assert_eq!(
        admitted.pinned_deployment_id.as_deref(),
        Some(deployment_id.as_str()),
        "recovery invocation must stay pinned to the original immutable deployment"
    );
    wait_for_trace_event_count(
        &data_dir.join("trace.jsonl"),
        "llm_call_completed",
        1,
        Duration::from_secs(20),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let first_generation = session_lease_generation(&data_dir, backend, &session_id).await;

    first.stop_and_reap();

    let restart_started = tokio::time::Instant::now();
    let mut replacement = spawn_recovery_e2e_child(&data_dir, endpoint_bind, &ingress_url, backend);
    let _replacement_pid = replacement.id();
    wait_for_endpoint_socket(endpoint_bind).await;
    // This is a process restart of the same configuration and storage at the
    // same immutable endpoint. Keep the original Restate deployment identity;
    // re-registering would turn the crash-recovery probe into a deployment update.
    wait_for_session_lease_generation(
        &data_dir,
        backend,
        &session_id,
        first_generation + 1,
        Duration::from_secs(10),
    )
    .await;
    // The dead owner's lease has to *expire* before anything may supersede it,
    // so this elapsed time is a direct reading of the configured TTL. Stock
    // timings would hold the turn hostage for the full 30s default; dropping
    // `lease_timings` from the workbench core reddens this line.
    let takeover_latency = restart_started.elapsed();
    let default_ttl = lash::durability::LeaseTimings::default().ttl();
    assert!(
        takeover_latency < default_ttl,
        "the configured {:?} session-lease TTL must recover the turn faster \
         than the stock {default_ttl:?} default; takeover took {takeover_latency:?}",
        recovery_e2e_lease_timings().ttl(),
    );
    assert!(
        session_lease_generation(&data_dir, backend, &session_id).await > first_generation,
        "replacement must resume under a superseding session-lease generation"
    );

    let recovered_active_turns = ActiveTurns::persistent(data_dir.join("active-turns.json"))
        .expect("reopen recovered active-turn routing");
    assert_eq!(
        recovered_active_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, &turn_id)),
        "retryable resume failures must not permanently clear the durable turn address"
    );

    let address = lash::TurnAddress::new(&session_id, &turn_id);
    let database_url = (backend == "postgres").then(|| {
        std::env::var("AGENT_WORKBENCH_E2E_DATABASE_URL")
            .expect("Postgres recovery E2E database URL")
    });
    let stores = WorkbenchStores::open(&data_dir, database_url.as_deref())
        .await
        .expect("reopen recovery session catalog");
    let driver = lash_restate::RestateEngine::new(
        Arc::clone(&stores.stores),
        lash::restate::config(
            ingress_url,
            lash_restate::RestateAuthorityId::new(
                std::env::var("RESTATE_AUTHORITY_ID").expect("Restate authority id"),
            )
            .expect("valid Restate authority id"),
        ),
    )
    .turn_work_driver();
    let receipt = driver
        .request_cancel(
            lash::TurnCancelRequest::new(
                address.clone(),
                format!("workbench-recovery-{backend}-e2e-cancel"),
                Some("user".to_string()),
            )
            .with_reason("deterministic ingress-owner restart gate"),
        )
        .await
        .expect("request cancellation after ingress-owner restart");
    assert!(
        matches!(
            receipt.outcome,
            lash::TurnCancelOutcome::Requested(_) | lash::TurnCancelOutcome::AlreadyRequested(_)
        ),
        "recovered turn cancellation did not reach the durable gate: {receipt:#?}"
    );
    let terminal = driver
        .await_terminal_with_timeout(&address, Duration::from_secs(20))
        .await
        .expect("recovered turn must commit a cancellation terminal");
    let lash::TurnTerminal::Committed { outcome, .. } = terminal else {
        panic!("recovered turn returned non-committed terminal: {terminal:#?}");
    };
    let lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { evidence }) = outcome else {
        panic!("recovered turn did not commit Cancelled: {outcome:#?}");
    };
    assert_eq!(
        evidence.request_id,
        format!("workbench-recovery-{backend}-e2e-cancel")
    );
    assert_eq!(evidence.origin.as_deref(), Some("user"));
    assert_eq!(
        evidence.reason.as_deref(),
        Some("deterministic ingress-owner restart gate")
    );
    let product_event_path = data_dir.join("product-events.json");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let product_events = SessionEventRegistry::persistent(product_event_path.clone(), 4)
            .expect("reopen product events after ingress-owner replacement")
            .snapshot(&session_id);
        let done_count = product_events
            .events
            .iter()
            .filter(|event| matches!(&event.item, StreamItem::Done { .. }))
            .count();
        if done_count > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner replacement did not settle the durable product projection"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let done_count = SessionEventRegistry::persistent(product_event_path, 4)
        .expect("reopen settled product events after duplicate-observation window")
        .snapshot(&session_id)
        .events
        .iter()
        .filter(|event| matches!(&event.item, StreamItem::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "owner replacement and Restate redelivery must settle the product projection once"
    );
    wait_for_restate_deployment_and_unpinned_invocations_drained(
        &admin_url,
        &deployment_id,
        Duration::from_secs(30),
    )
    .await;
    replacement.stop_and_reap();
    assert!(
        tokio::net::TcpStream::connect(endpoint_bind).await.is_err(),
        "recovery E2E endpoint {endpoint_bind} remained open after child teardown"
    );
    drop(driver);
    drop(stores);
    println!("workbench ingress-owner restart gate passed: backend={backend}");
    std::fs::remove_dir_all(&data_dir).expect("remove drained recovery E2E data directory");
    assert!(!data_dir.exists());
    failure_scope.disarm();
}

async fn wait_for_restate_deployment_and_unpinned_invocations_drained(
    admin_url: &str,
    deployment_id: &str,
    timeout: Duration,
) {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(admin_url));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let open = admin
            .open_invocations_by_deployment()
            .await
            .expect("query recovery E2E Restate deployment drain")
            .into_iter()
            .filter(|row| {
                row.pinned_deployment_id.is_none()
                    || row.pinned_deployment_id.as_deref() == Some(deployment_id)
            })
            .map(|row| row.open_count)
            .sum::<u64>();
        if open == 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recovery E2E deployment {deployment_id} retained {open} pinned/unpinned Restate invocations within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The file whose appearance asks a recovery child to send the turn it names.
/// Only the first owner finds it unconsumed: it removes the file once sent.
const RECOVERY_E2E_START_TURN: &str = "start-turn";

fn spawn_recovery_e2e_child(
    data_dir: &std::path::Path,
    endpoint_bind: SocketAddr,
    ingress_url: &str,
    backend: &str,
) -> OwnedFixtureChild {
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("resolve workbench test executable"),
    );
    command
        .arg("live_restate_ingress_owner_restart_resumes_and_remains_cancellable")
        .arg("--ignored")
        .arg("--nocapture")
        .env("AGENT_WORKBENCH_RECOVERY_E2E_CHILD", "1")
        .env("AGENT_WORKBENCH_RECOVERY_E2E_DATA_DIR", data_dir)
        .env("AGENT_WORKBENCH_RECOVERY_E2E_BACKEND", backend)
        .env(
            "AGENT_WORKBENCH_RECOVERY_E2E_ENDPOINT_BIND",
            endpoint_bind.to_string(),
        )
        .env("RESTATE_INGRESS_URL", ingress_url);
    let child = command.spawn().expect("spawn workbench recovery child");
    record_fixture_owned_child(child.id());
    OwnedFixtureChild::new(child)
}

async fn live_restate_recovery_child() {
    let data_dir = PathBuf::from(
        std::env::var("AGENT_WORKBENCH_RECOVERY_E2E_DATA_DIR").expect("recovery child data dir"),
    );
    let endpoint_bind: SocketAddr = std::env::var("AGENT_WORKBENCH_RECOVERY_E2E_ENDPOINT_BIND")
        .expect("recovery child endpoint bind")
        .parse()
        .expect("valid recovery child endpoint bind");
    let ingress_url =
        std::env::var("RESTATE_INGRESS_URL").expect("recovery child Restate ingress URL");
    let backend = std::env::var("AGENT_WORKBENCH_RECOVERY_E2E_BACKEND")
        .expect("recovery child store backend");
    let database_url = match backend.as_str() {
        "sqlite" => None,
        "postgres" => Some(
            std::env::var("AGENT_WORKBENCH_E2E_DATABASE_URL")
                .expect("Postgres recovery E2E database URL"),
        ),
        other => panic!("unsupported recovery E2E backend `{other}`"),
    };
    let provider_owner_path = data_dir.join("provider-owner");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-recovery-e2e")
        .complete(move |_| {
            let provider_owner_path = provider_owner_path.clone();
            async move {
                std::fs::write(provider_owner_path, std::process::id().to_string())
                    .expect("record provider owner pid");
                Ok(text_response(
                    "<typescript>\nawait sleep(60000);\nfinish(\"unreachable\");\n</typescript>",
                ))
            }
        })
        .build()
        .into_handle();
    let active_turns = ActiveTurns::persistent(data_dir.join("active-turns.json"))
        .expect("open child active-turn routing");
    let sessions =
        WorkbenchSessions::persistent(data_dir.join("session-id")).expect("open child session id");
    let lease_timings = recovery_e2e_lease_timings();
    let harness = live_workbench_restate_state_with_provider_and_database(
        &data_dir,
        ingress_url,
        provider,
        sessions,
        active_turns,
        database_url.as_deref(),
        lease_timings,
    )
    .await;
    let state = harness.state.clone();
    restate::spawn_restate_endpoint(
        endpoint_bind,
        harness.state,
        harness.backend,
        harness.process_worker,
    );
    let start_turn = data_dir.join(RECOVERY_E2E_START_TURN);
    let mut followers = Vec::new();
    loop {
        if let Ok(turn_id) = std::fs::read_to_string(&start_turn) {
            let session_id = state.current_session_id();
            let turn_id = TurnId::from(turn_id.trim().to_string());
            state.track_turn(&session_id, &turn_id);
            followers.push(
                restate::start_user_turn(
                    &state,
                    restate::UserTurnRequest {
                        turn_id,
                        session_id,
                        text: "hold until durable cancellation".to_string(),
                        model: ModelSelection {
                            model: "mock-model".to_string(),
                            model_variant: Some("high".to_string()),
                        },
                        attachment_id: None,
                    },
                )
                .await
                .expect("send the recovery E2E turn"),
            );
            std::fs::remove_file(&start_turn).expect("consume the recovery E2E start request");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_provider_owner(data_dir: &std::path::Path, expected_pid: u32, timeout: Duration) {
    let path = data_dir.join("provider-owner");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(expected_pid)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "replacement ingress owner {expected_pid} did not resume the durable turn within {timeout:?}; trace tail={}",
            trace_tail(&data_dir.join("trace.jsonl")),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_workbench_restate_invocation_suspended(
    state: &AppState,
    invocation_id: &lash_restate::RestateInvocationId,
    timeout: Duration,
) {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
            state.restate_http.clone(),
        ));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last_status = admin
            .invocation_status(invocation_id)
            .await
            .expect("query Restate invocation status");
        if last_status.as_ref().is_some_and(|status| {
            status.status == lash_restate::RestateInvocationLifecycle::Suspended
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Restate invocation {invocation_id} did not suspend within {timeout:?}; last status={last_status:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_active_turns_empty(state: &AppState, session_id: &SessionId, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if state.active_turns.for_session(session_id).is_none() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "active turn routing did not settle within {timeout:?}: {:?}",
            state.active_turns.for_session(session_id)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn session_lease_generation(
    data_dir: &std::path::Path,
    backend: &str,
    session_id: &SessionId,
) -> i64 {
    match backend {
        "sqlite" => {
            // The backend keeps session leases in its durable core, one of
            // several databases under the sessions root; name it rather than
            // take whichever `.db` the directory lists first.
            let database_path = data_dir
                .join("lash-sessions")
                .join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name());
            rusqlite::Connection::open_with_flags(
                &database_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("open recovery E2E SQLite durable core")
            .query_row(
                "SELECT lease_fencing_token FROM session_execution_leases WHERE session_id = ?1",
                [session_id.as_str()],
                |row| row.get(0),
            )
            .expect("read recovery E2E SQLite session lease generation")
        }
        "postgres" => {
            let database_url = std::env::var("AGENT_WORKBENCH_E2E_DATABASE_URL")
                .expect("Postgres recovery E2E database URL");
            let pool = sqlx::PgPool::connect(&database_url)
                .await
                .expect("connect to recovery E2E Postgres");
            sqlx::query_scalar(
                "SELECT lease_fencing_token FROM lash_session_execution_leases
                 WHERE session_id = $1",
            )
            .bind(session_id.as_str())
            .fetch_one(&pool)
            .await
            .expect("read recovery E2E Postgres session lease generation")
        }
        other => panic!("unsupported recovery E2E backend `{other}`"),
    }
}

async fn wait_for_session_lease_generation(
    data_dir: &std::path::Path,
    backend: &str,
    session_id: &SessionId,
    expected: i64,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Admission and execution each take a lease-fenced continuation read. Both
        // acquisitions may become visible before this poll observes the first one,
        // and fencing generations are monotonic rather than gap-free.
        if session_lease_generation(data_dir, backend, session_id).await >= expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "replacement did not supersede the dead session-lease generation within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
