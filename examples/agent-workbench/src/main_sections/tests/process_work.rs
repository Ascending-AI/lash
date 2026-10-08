use super::*;
use lash::ProcessId;
use lash::SessionId;

use super::tests::{RecordingTrace, Workbench, silent_provider};

/// Register a held process `provenance` originated in `workbench`'s registry,
/// labelled `label` for the work rail.
async fn register_held(
    workbench: &Workbench,
    provenance: lash::process::ProcessProvenance,
    label: &str,
) -> ProcessId {
    workbench
        .stores
        .process_registry()
        .register_process(
            lash::testing::held_engine_registration(
                json!({ "label": label }),
                provenance,
                lash::process::Lifetime::Detached,
            )
            .with_host_facing_label(Some(label.to_string())),
        )
        .await
        .expect("register process")
        .id
}

async fn complete(
    workbench: &Workbench,
    process_id: &ProcessId,
    output: lash::tools::ToolCallOutput,
) {
    workbench
        .stores
        .process_registry()
        .complete_process(
            process_id,
            lash::process::ProcessAwaitOutput::from_tool_output(output),
            lash::process::ProcessCompletionAuthority::workflow_key(process_id),
        )
        .await
        .expect("complete process");
}

/// The labels of the rows on the runtime-wide work rail, sorted.
async fn work_rail_labels(state: &AppState) -> Vec<String> {
    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    let mut labels = work
        .into_iter()
        .map(|item| item.process.label)
        .collect::<Vec<_>>();
    labels.sort();
    labels
}

/// The await route answers a process's terminal outcome through the
/// process-work port (ADR 0016) with its event log reconciled from the
/// durable store (ADR 0017); a failed process shows failed on the rail; an
/// unknown id errors instead of hanging.
#[tokio::test]
async fn await_work_route_returns_terminal_outcome_and_reconciled_events() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let awaited = register_held(
        &workbench,
        lash::process::ProcessProvenance::host(),
        "awaited",
    )
    .await;
    complete(
        &workbench,
        &awaited,
        lash::tools::ToolCallOutput::success(json!("done")),
    )
    .await;

    let Json(result) = await_work(AxumPath(awaited.to_string()), State(state.clone()))
        .await
        .expect("await work route");
    assert_eq!(
        result.outcome.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed)
    );
    assert!(matches!(
        &result.outcome,
        lash::process::ProcessAwaitOutput::Settled { output }
            if output.is_success() && output.value_for_projection() == json!("done")
    ));
    assert!(
        !result.events.is_empty(),
        "the reconciled event log is missing: {:?}",
        result.events
    );

    let failed = register_held(
        &workbench,
        lash::process::ProcessProvenance::host(),
        "failed",
    )
    .await;
    complete(
        &workbench,
        &failed,
        lash::tools::ToolCallOutput::failure(lash::tools::ToolFailure::runtime(
            lash::tools::ToolFailureClass::External,
            "deterministic_failure",
            "deterministic durable process failure",
        )),
    )
    .await;
    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list failed work");
    let failed = work
        .iter()
        .find(|item| item.process.process_id == failed)
        .expect("failed process in work API");
    assert_eq!(failed.process.status_label, "failed");
    assert!(failed.process.terminal);
    assert_eq!(
        failed.process.error.as_deref(),
        Some("deterministic durable process failure")
    );

    let missing = await_work(
        AxumPath("no-such-process".to_string()),
        State(state.clone()),
    )
    .await;
    assert!(missing.is_err(), "unknown process id must error");
    workbench.shutdown().await;
}

/// A process whose session's process state was deleted stays on the
/// runtime-wide rail, and its cancel is routed by the process, not by the
/// selected session: the request lands on the process and names the session
/// that originated it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let registry = workbench.stores.process_registry();
    let process_id = register_held(
        &workbench,
        lash::process::ProcessProvenance::session(lash::process::SessionScope::new(&session_id)),
        "orphaned",
    )
    .await;
    registry
        .add_observer(
            &session_id,
            &process_id,
            lash::process::ProcessObserverBy::host("workbench-session-delete"),
        )
        .await
        .expect("observe process");
    let deletion = registry
        .delete_session_process_state(&session_id)
        .await
        .expect("delete session process edges");
    assert_eq!(deletion.removed_observer_count, 1);
    let (_, current_session_id) = state.sessions.rotate();
    assert_ne!(current_session_id, session_id);

    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].process.process_id, process_id);

    let Json(receipt) = cancel_work(AxumPath(process_id.to_string()), State(state.clone()))
        .await
        .expect("submit process cancellation");
    assert!(receipt.accepted);
    assert_eq!(receipt.process_id, process_id);
    let requested = trace.custom("process.cancel_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(
        requested[0].0.as_ref(),
        Some(&session_id),
        "process cancellation must retain its originating trace session"
    );
    assert_eq!(
        requested[0].1["process_id"].as_str(),
        Some(process_id.as_str())
    );
    let cancelled = state
        .process_observer
        .process(&process_id)
        .await
        .expect("read the cancelled process")
        .expect("the process is still observable");
    assert!(
        cancelled.cancel_request.is_some() || cancelled.terminal(),
        "the cancel landed on the process: {cancelled:?}"
    );
    workbench.shutdown().await;
}

/// The work rail reads the runtime-wide process registry, and deleting a
/// session deliberately detaches rather than deletes the globally-owned rows
/// it originated. Without the retention half of the delete, every reset left
/// the dead session's finished work on the
/// rail forever (FIG-989).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_delete_reclaims_the_deleted_sessions_terminal_work() {
    use lash::process::{ProcessProvenance, SessionScope};
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let deleted_session_id = state.current_session_id();
    let surviving_session_id = SessionId::fixture(format!("{deleted_session_id}-survivor"));
    let mut ids = BTreeMap::new();
    for (label, originator) in [
        ("work-of-deleted-session", Some(deleted_session_id.clone())),
        (
            "live-work-of-deleted-session",
            Some(deleted_session_id.clone()),
        ),
        (
            "work-of-surviving-session",
            Some(surviving_session_id.clone()),
        ),
        ("host-owned-work", None),
    ] {
        let provenance = match &originator {
            Some(session_id) => ProcessProvenance::session(SessionScope::new(session_id)),
            None => ProcessProvenance::host(),
        };
        ids.insert(label, register_held(&workbench, provenance, label).await);
    }
    for label in [
        "work-of-deleted-session",
        "work-of-surviving-session",
        "host-owned-work",
    ] {
        complete(
            &workbench,
            &ids[label],
            lash::tools::ToolCallOutput::success(json!({ "delivered": true })),
        )
        .await;
    }
    assert_eq!(
        work_rail_labels(state).await,
        vec![
            "host-owned-work".to_string(),
            "live-work-of-deleted-session".to_string(),
            "work-of-deleted-session".to_string(),
            "work-of-surviving-session".to_string(),
        ],
        "every registered row is on the runtime-wide rail before the delete"
    );

    let Json(replacement) = Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(deleted_session_id.clone()),
        }),
    ))
    .await
    .expect("delete the session and reclaim its finished work");
    assert_ne!(replacement.settings.session_id, deleted_session_id);

    let deleted = trace.custom("api.session.delete.deleted");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(
        deleted[0].1["process_retention"]["pruned_processes"],
        json!(1),
        "one finished row reclaimed: {deleted:?}"
    );
    let rail = work_rail_labels(state).await;
    assert_eq!(
        rail,
        vec![
            "host-owned-work".to_string(),
            "live-work-of-deleted-session".to_string(),
            "work-of-surviving-session".to_string(),
        ],
        "live work and other owners' finished work stay on the rail"
    );
    assert!(
        matches!(
            workbench
                .stores
                .process_registry()
                .get_process(&ids["work-of-deleted-session"])
                .await,
            Err(lash::plugins::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the reclaimed row must read as a payload-free tombstone"
    );
    workbench.shutdown().await;
}

/// FIG-3155: the runtime-wide rail bounds retired rows by a recent-update
/// window. A row leaves the rail when its outcome is recorded, never because
/// time passed, so a running process older than the window stays on it.
#[tokio::test]
async fn work_rail_keeps_a_nonterminal_process_past_the_retirement_window() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    // Every write below is stamped a full minute before the rail's window, so
    // the window alone decides visibility.
    let stale_ms = lash::runtime::ClockWallTime::timestamp_ms(&lash::runtime::SystemClock)
        .saturating_sub(60_000);
    let stale_registry = workbench
        .stores
        .process_registry()
        .with_runtime_clock(Arc::new(lash::testing::TestClock::new(stale_ms)))
        .expect("the sqlite registry rebinds its clock");
    let mut ids = BTreeMap::new();
    for label in ["running-process", "settled-process"] {
        let process_id = stale_registry
            .register_process(lash::testing::held_engine_registration(
                json!({ "test": true }),
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register process")
            .id;
        ids.insert(label, process_id);
    }
    stale_registry
        .complete_process(
            &ids["settled-process"],
            lash::process::ProcessAwaitOutput::from_tool_output(
                lash::tools::ToolCallOutput::success(json!("done")),
            ),
            lash::process::ProcessCompletionAuthority::workflow_key(&ids["settled-process"]),
        )
        .await
        .expect("record the terminal outcome");

    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    let listed = work
        .iter()
        .map(|item| item.process.process_id.clone())
        .collect::<Vec<_>>();
    assert!(
        listed.contains(&ids["running-process"]),
        "a non-terminal process must stay on the rail until its terminal lands: {listed:?}"
    );
    assert!(
        !listed.contains(&ids["settled-process"]),
        "a terminal older than the retirement window still leaves the rail: {listed:?}"
    );
    workbench.shutdown().await;
}
