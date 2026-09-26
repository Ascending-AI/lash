use super::*;
use lash::ProcessId;
use lash::SessionId;
use lash::TurnId;

#[cfg(test)]
#[path = "tests/support.rs"]
mod support;
use lash::rlm::RlmTurnBuilderExt;
#[path = "tests/restate_endpoint.rs"]
mod restate_endpoint;
use lash::tracing::{
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionMap,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLashlangGraphChildLink,
    TraceRuntimeScope, TraceRuntimeSubject,
};
pub(crate) use restate_endpoint::*;
use std::future::Future;
pub(crate) use support::*;
fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}
fn test_model() -> lash::ModelSpec {
    lash::ModelSpec::builder("test-model")
        .context_window_tokens(4096)
        .build()
        .expect("model spec")
}

#[cfg(test)]
#[path = "tests/commit_budget.rs"]
mod commit_budget_tests;
#[cfg(test)]
#[path = "tests/mail_payload.rs"]
mod mail_payload_tests;
#[cfg(test)]
#[path = "tests/provider_execution_evidence.rs"]
mod provider_execution_evidence_tests;
pub(crate) use provider_execution_evidence_tests::provider_execution_evidence_scenarios;
#[cfg(test)]
#[path = "tests/remote_execution_evidence.rs"]
mod remote_execution_evidence_tests;
pub(crate) use remote_execution_evidence_tests::browser_projection_trigger_identities;
#[cfg(test)]
#[path = "tests/product_event_persistence.rs"]
mod product_event_persistence_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat.rs"]
mod recoverable_chat_tests;
pub(crate) use recoverable_chat_tests::{
    recoverable_chat_test_state, recoverable_chat_test_state_with_dependencies,
    recoverable_chat_test_state_with_dependencies_and_context,
    recoverable_chat_test_state_with_provider, recoverable_chat_test_state_with_trigger_store,
    user_rows,
};
#[cfg(test)]
#[path = "tests/chat_projection_boundaries.rs"]
mod chat_projection_boundaries_tests;
#[cfg(test)]
#[path = "tests/lease_free_probes.rs"]
mod lease_free_probes_tests;
#[cfg(test)]
#[path = "tests/live_stream_user_rows.rs"]
mod live_stream_user_rows_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat_bare_prose.rs"]
mod recoverable_chat_bare_prose_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat_failures.rs"]
mod recoverable_chat_failures_tests;
#[cfg(test)]
#[path = "tests/reference_transport.rs"]
mod reference_transport_tests;
pub(crate) use reference_transport_tests::recoverable_chat_test_state_with_replay_store;
#[cfg(test)]
#[path = "tests/typescript_dialect.rs"]
mod typescript_dialect_tests;
pub(crate) use typescript_dialect_tests::{
    run_turn_through_the_workbench_open_path, scripted_cells_provider, transcript_code_languages,
};
#[cfg(test)]
#[path = "tests/continue_as_projection.rs"]
mod continue_as_projection_tests;
#[cfg(test)]
#[path = "tests/multi_session.rs"]
mod multi_session_tests;
#[cfg(test)]
#[path = "tests/tool_catalog.rs"]
mod tool_catalog_tests;
pub(crate) use tool_catalog_tests::{
    assert_live_tool_provider_execution_and_removal, assert_plugin_provider_execution,
    assert_tool_catalog_contract, catalog_lifecycle_provider,
};
#[cfg(test)]
#[path = "tests/approvals.rs"]
mod approvals_tests;
#[cfg(test)]
#[path = "tests/deferred_tools.rs"]
mod deferred_tools_tests;
#[cfg(test)]
#[path = "tests/done_stream_items.rs"]
mod done_stream_items_tests;
#[cfg(test)]
#[path = "tests/tool_loss.rs"]
mod tool_loss_tests;
/// The file `SqliteBackend` a durable workbench test core runs on, rooted
/// at the data directory's sessions root.
pub(super) fn test_file_backend(
    data_dir: &std::path::Path,
) -> Arc<lash_sqlite_store::SqliteBackend> {
    let root = data_dir.join("lash-sessions");
    Arc::new(sync_await(async move {
        lash_sqlite_store::SqliteBackend::open(&root)
            .await
            .expect("open the workbench test backend")
    }))
}

pub(super) fn explicit_durable_test_facets(data_dir: &std::path::Path) -> lash::LashCoreBuilder {
    explicit_durable_test_facets_over(test_file_backend(data_dir))
}

pub(super) fn explicit_durable_test_facets_over(
    backend: Arc<lash_sqlite_store::SqliteBackend>,
) -> lash::LashCoreBuilder {
    explicit_durable_test_facets_on(backend.into())
}

/// A durable test core over `backend`, whose RLM factory keeps its Lashlang
/// artifacts in that same backend.
pub(super) fn explicit_durable_test_facets_on(backend: lash::Backend) -> lash::LashCoreBuilder {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend,
    );
    lash::LashCore::rlm_builder(
        backend,
        lash::TurnBudget::Unbounded,
        factory,
    )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        // The `processes` module is catalogue presence, not an ability bit
        // (ADR 0095): the workbench's scripted sources author `processes.*`,
        // so the surface exists only where this factory is installed. Every
        // durable test core gets it here, as `bootstrap` gives the real app.
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash::process::lifetime::session_or_starter),
        ))
}

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

fn test_graph(
    graph_key: &str,
    session_id: &SessionId,
    subject: TraceRuntimeSubject,
    children: Vec<TraceLashlangGraphChildLink>,
) -> TraceLashlangGraph {
    TraceLashlangGraph {
        schema_version: lash::tracing::TRACE_SCHEMA_VERSION,
        graph_key: graph_key.to_string(),
        scope: TraceRuntimeScope::new(session_id),
        subject,
        source_identity: format!("{graph_key}:source"),
        module_ref: format!("{graph_key}:module"),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        status: TraceLanguageExecutionStatus::Running,
        completeness: lash::tracing::TraceLashlangGraphCompleteness::IncompleteMap,
        nodes: Vec::new(),
        edges: Vec::new(),
        children,
        history_limit: lash::tracing::DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT,
        node_retention: Vec::new(),
        conflicts: Vec::new(),
        history: Vec::new(),
        execution_map: None,
    }
}

fn append_started_graph(store: &TraceLashlangGraphStore, graph: &TraceLashlangGraph) {
    let identity = TraceLanguageExecutionIdentity {
        scope: graph.scope.clone(),
        subject: graph.subject.clone(),
        source_identity: graph.source_identity.clone(),
        module_ref: graph.module_ref.clone(),
        entry_kind: graph.entry_kind.clone(),
        entry_ref: graph.entry_ref.clone(),
        entry_name: graph.entry_name.clone(),
        engine_execution_id: None,
        generation: None,
    };
    let context = TraceContext {
        session_id: graph.scope.session_id.clone(),
        ..Default::default()
    };
    store
        .append(&TraceRecord::new(
            context,
            TraceEvent::LanguageExecution {
                language: "typescript".to_string(),
                event: TraceLanguageExecution {
                    event_key: format!("{}:start", graph.graph_key),
                    identity,
                    payload: TraceLanguageExecutionPayload::ExecutionStarted {
                        execution_map: TraceLanguageExecutionMap::default(),
                    },
                },
            },
        ))
        .expect("append test graph");
}

#[test]
fn turn_routing_state_survives_web_process_reconstruction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_path = temp.path().join("session-id");
    let turns_path = temp.path().join("active-turns.json");
    let sessions = WorkbenchSessions::persistent(session_path.clone()).expect("session ids");
    let session_id = sessions.current();
    let turns = ActiveTurns::persistent(turns_path.clone()).expect("active turns");
    turns.insert_with_prompt(
        &session_id,
        "durable-stop-turn",
        WorkbenchTurnKind::User,
        Some("actual restored prompt".into()),
        None,
    );
    drop(sessions);
    drop(turns);
    let recovered_ids = WorkbenchSessions::persistent(session_path).expect("recover ids");
    let recovered_turns = ActiveTurns::persistent(turns_path).expect("recover turns");
    assert_eq!(recovered_ids.current(), session_id);
    assert_eq!(
        recovered_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, "durable-stop-turn"))
    );
    let recovered_prompt = recovered_turns
        .prompt_for(&session_id, &TurnId::from("durable-stop-turn"))
        .expect("restored prompt");
    assert_eq!(recovered_prompt.text, "actual restored prompt");
    assert_eq!(recovered_prompt.attachment_id, None);
}

#[cfg(test)]
#[path = "tests/session_resume.rs"]
mod session_resume_tests;
#[test]
fn assistant_display_keeps_streamed_prose_with_terminal_value() {
    assert_eq!(
        combine_assistant_display_parts(
            Some("I started the background checks.".to_string()),
            Some("summary ready".to_string()),
        ),
        "I started the background checks.\n\nsummary ready"
    );
}

#[test]
fn assistant_display_does_not_duplicate_matching_terminal_value() {
    assert_eq!(
        combine_assistant_display_parts(
            Some("summary ready".to_string()),
            Some("summary ready".to_string())
        ),
        "summary ready"
    );
}

#[cfg(test)]
#[path = "tests/ui_contract.rs"]
mod ui_contract_tests;

#[test]
fn mail_received_account_contract_uses_slugs() {
    const ACCOUNT_SLUG_CONTRACT: &str = "`mail.Received.account` carries the account SLUG, not its display name: use the slug from the account enumeration (for example `work` or `personal`), not a display name such as `Work`, when filtering deliveries.";

    assert!(
        workbench_prompt().contains(ACCOUNT_SLUG_CONTRACT),
        "the workbench prompt must state the mail account slug contract"
    );
}

#[cfg(test)]
#[path = "tests/facade_homes.rs"]
mod facade_homes_tests;

#[test]
fn empty_model_variant_request_clears_selected_variant() {
    let selected_model = ModelSelection {
        model: "x-ai/grok-build-0.1".to_string(),
        model_variant: Some("medium".to_string()),
    };

    assert_eq!(
        model_variant_for_request(&selected_model, None),
        Some("medium".to_string())
    );
    assert_eq!(
        model_variant_for_request(&selected_model, Some(" high ")),
        Some("high".to_string())
    );
    assert_eq!(model_variant_for_request(&selected_model, Some("")), None);
    assert_eq!(
        model_variant_for_request(&selected_model, Some("   ")),
        None
    );
}

#[test]
fn event_stream_forwards_session_observation_live_replay() {
    run_async_test_on_stack_budget("workbench-observation-stream-test", || {
        event_stream_forwards_session_observation_live_replay_inner()
    });
}

async fn event_stream_forwards_session_observation_live_replay_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-observation-stream-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let model = test_model();
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-observation-stream-test")
        .complete(|_request| async {
            Ok(text_response(
                r#"<typescript>
finish("observed through live replay");
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model.clone())
        .without_queued_work()
        .build(crate::test_core_owner())
        .expect("build core");
    let session = core
        .session("workbench-observation-stream")
        .open()
        .await
        .expect("open session");
    let cursor = session.observe().current_observation().cursor;
    let (tx, mut rx) = mpsc::channel(64);
    let forwarder = tokio::spawn(forward_session_observations(session.clone(), cursor, tx));

    session
        .turn(lash::TurnInput::text("exercise observation stream"))
        .require_finish()
        .expect("require finish")
        .run()
        .await
        .expect("turn");

    let mut saw_cursor = false;
    let mut saw_final_value_observation = false;
    for _ in 0..64 {
        let item = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for stream item")
            .expect("stream item");
        match item {
            ObservationStreamItem::Cursor { cursor } => {
                assert!(!cursor.is_empty(), "cursor should be opaque but non-empty");
                saw_cursor = true;
            }
            ObservationStreamItem::Observation { event } => {
                let value = serde_json::to_value(&event).expect("remote event json");
                if value.pointer("/type").and_then(Value::as_str) == Some("turn_activity")
                    && value.pointer("/activity/type").and_then(Value::as_str)
                        == Some("final_value")
                {
                    saw_final_value_observation = true;
                }
            }
            ObservationStreamItem::ReplayGap { .. }
            | ObservationStreamItem::TerminalReplacement { .. }
            | ObservationStreamItem::ResidentReplacement { .. } => {}
        }
        if saw_cursor && saw_final_value_observation {
            break;
        }
    }
    assert!(saw_cursor, "stream should expose a replay cursor");
    assert!(
        saw_final_value_observation,
        "stream should expose turn activity through session observation"
    );
    assert_typed_turn_input_application(&session, &mut rx).await;
    forwarder.abort();
    drop(session);
    drop(core);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn event_stream_forwards_session_observation_replay_gap() {
    run_async_test_on_stack_budget("workbench-observation-gap-test", || {
        event_stream_forwards_session_observation_replay_gap_inner()
    });
}

async fn event_stream_forwards_session_observation_replay_gap_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-observation-gap-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let model = test_model();
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-observation-gap-test")
        .complete(|_request| async {
            Ok(text_response(
                r#"<typescript>
finish("gap source");
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model.clone())
        .live_replay_store(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash::observe::InMemoryLiveReplayStoreConfig::default()
            },
        )))
        .build(crate::test_core_owner())
        .expect("build core");
    let session = core
        .session("workbench-observation-gap")
        .open()
        .await
        .expect("open session");
    let cursor = session.observe().current_observation().cursor;
    let requested_cursor = cursor.to_string();

    session
        .turn(lash::TurnInput::text("trim cursor"))
        .require_finish()
        .expect("require finish")
        .run()
        .await
        .expect("turn");

    let (tx, mut rx) = mpsc::channel(64);
    let forwarder = tokio::spawn(forward_session_observations(session.clone(), cursor, tx));
    let mut saw_gap = false;
    for _ in 0..8 {
        let item = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for stream item")
            .expect("stream item");
        if let ObservationStreamItem::ReplayGap { observation, gap } = item {
            assert_eq!(gap.body.requested_cursor, requested_cursor);
            assert!(
                !gap.body.latest_cursor.is_empty(),
                "gap should include the latest recoverable cursor"
            );
            assert_eq!(observation.body.cursor, gap.body.latest_cursor);
            assert_eq!(observation.body.session_id, "workbench-observation-gap");
            assert_eq!(
                gap.body.reason,
                lash_remote_protocol::RemoteLiveReplayGapReason::Trimmed
            );
            saw_gap = true;
            break;
        }
    }
    forwarder.abort();

    assert!(saw_gap, "trimmed cursor should emit replay_gap");
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn state_snapshot_cursor_attaches_to_the_live_incarnation_without_a_gap() {
    run_async_test_on_stack_budget("workbench-snapshot-cursor-test", || {
        state_snapshot_cursor_attaches_to_the_live_incarnation_without_a_gap_inner()
    });
}

/// A healthy shell must produce zero `replay_gap` between reconnects.
///
/// `/api/state` hands the page a cursor to attach at. When that cursor named
/// no replay incarnation the attach was fenced into `replay_gap(unavailable)`,
/// the page recovered from state, and the fresh snapshot handed it another
/// unservable cursor — a re-snapshot loop per open tab with no outage anywhere
/// (FIG-3162). This pins both halves: the snapshot cursor attaches clean, and
/// a cursor from a genuinely different incarnation — what a replaced process
/// leaves behind — still gaps exactly once and converges on the recovery
/// cursor it hands back.
async fn state_snapshot_cursor_attaches_to_the_live_incarnation_without_a_gap_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-snapshot-cursor-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-snapshot-cursor-test")
        .complete(|_request| async {
            Ok(text_response(
                r#"<typescript>
finish("snapshot cursor");
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(test_model())
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(16),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let session_id = SessionId::from("workbench-snapshot-cursor");
    let session = state
        .core
        .session(session_id.to_string())
        .open()
        .await
        .expect("open session");
    session
        .turn(lash::TurnInput::text("fill the live replay buffer"))
        .require_finish()
        .expect("require finish")
        .run()
        .await
        .expect("turn");

    // The snapshot a healthy tab reads, and the attach it performs next.
    let snapshot = read_state_projection(&state, &session_id)
        .await
        .expect("read state projection");
    assert!(
        matches!(
            session
                .observe()
                .subscribe_from_cursor(&snapshot.cursor)
                .expect("attach at the snapshot cursor"),
            lash::observe::SessionObservationSubscription::Subscribed(_)
        ),
        "a snapshot cursor from a healthy shell must attach without a replay gap"
    );

    // Re-snapshotting and re-attaching stays clean: the loop this fixes needed
    // only one unservable cursor to run forever, so one clean round is the pin.
    let resnapshot = read_state_projection(&state, &session_id)
        .await
        .expect("re-read state projection");
    assert!(
        matches!(
            session
                .observe()
                .subscribe_from_cursor(&resnapshot.cursor)
                .expect("re-attach at the snapshot cursor"),
            lash::observe::SessionObservationSubscription::Subscribed(_)
        ),
        "a second snapshot must attach without a replay gap either"
    );

    // A real outage: the cursor the page holds was minted by a process that no
    // longer exists, so its replay incarnation is gone. That must still gap,
    // exactly once, and the recovery cursor it hands back must attach clean.
    let dead_incarnation = lash::observe::InMemoryLiveReplayStore::default();
    let stale_cursor = lash::observe::LiveReplayStore::current_cursor(
        &dead_incarnation,
        &session_id,
        lash::observe::SessionRevision(0),
    );
    let recovery_cursor = match session
        .observe()
        .subscribe_from_cursor(&stale_cursor)
        .expect("attach at the dead incarnation's cursor")
    {
        lash::observe::SessionObservationSubscription::Gap { observation, gap } => {
            assert_eq!(
                gap.reason,
                lash::observe::LiveReplayGapReason::Unavailable,
                "a cursor from a replaced process is unavailable, not trimmed"
            );
            observation.cursor
        }
        lash::observe::SessionObservationSubscription::Subscribed(_) => {
            panic!("a cursor from a replaced process must gap")
        }
    };
    assert!(
        matches!(
            session
                .observe()
                .subscribe_from_cursor(&recovery_cursor)
                .expect("attach at the recovery cursor"),
            lash::observe::SessionObservationSubscription::Subscribed(_)
        ),
        "the recovery cursor must converge, so one real outage costs exactly one gap"
    );

    drop(session);
    drop(state);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn turn_cancel_route_requests_first_party_turn_cancellation() {
    run_async_test_on_stack_budget("workbench-turn-cancel-test", || {
        turn_cancel_route_requests_first_party_turn_cancellation_inner()
    });
}

async fn turn_cancel_route_requests_first_party_turn_cancellation_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-turn-cancel-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = session_store_factory;
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete_error("cancel route test should not call the provider")
        .build()
        .into_handle();
    let model = test_model();
    let event_tx = SessionEventRegistry::new(16);
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx,
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);
    state.track_turn(&session_id, &TurnId::from("turn-cancel"));
    let session = state
        .core
        .session(&session_id)
        .open()
        .await
        .expect("open cancelled session");
    let address = session.turn_address("turn-cancel");
    let (cancelled, turn) = tokio::join!(
        cancel_turn(State(state.clone()), Query(TurnCancelQuery::default())),
        async {
            let recorded = await_durable_turn_cancel_request(&state, &address).await;
            assert_eq!(recorded.request.origin.as_deref(), Some("user"));
            assert_eq!(
                recorded.request.reason.as_deref(),
                Some("workbench Abort control")
            );
            assert_eq!(recorded.request.mode, lash::TurnCancelMode::Immediate);
            session
                .turn(lash::TurnInput::text("already cancelled"))
                .turn_id("turn-cancel")
                .run()
                .await
        },
    );
    let (status, Json(accepted)) = cancelled.expect("cancel turn");
    let turn = turn.expect("cancelled turn commits");
    assert_eq!(status, StatusCode::OK);
    assert!(accepted.accepted);
    assert!(matches!(
        turn.result.outcome,
        lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
    ));
    assert!(matches!(
        accepted.cancellations.as_slice(),
        [TurnCancelReceipt::TerminalAttached {
            cancellation: RecordedTurnCancellation::Requested(requested),
            terminal: lash::TurnTerminal::Committed {
                outcome: lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { evidence }),
                ..
            },
            ..
        }] if evidence == requested
            && evidence.origin.as_deref() == Some("user")
            && evidence.reason.as_deref() == Some("workbench Abort control")
    ));
    // The execution publisher owns the terminal event; the cancel route
    // publishes nothing for this core-run turn.
    assert!(events.try_recv().is_err(), "cancel route owns no terminal");
    state.publish_turn_done(&session_id, &TurnId::from("turn-cancel"));
    assert!(matches!(
        events.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { turn_id: Some(turn_id), .. },
            ..
        }) if turn_id == "turn-cancel"
    ));
    let duplicate = state
        .core
        .turn_work_driver()
        .request_cancel(lash::TurnCancelRequest::new(
            session.turn_address("turn-cancel"),
            "duplicate",
            Some("test-host".to_string()),
        ))
        .await
        .expect("read cancellation gate");
    assert!(matches!(
        duplicate.outcome,
        lash::TurnCancelOutcome::CompletionWonRace
    ));
    assert!(duplicate.record.is_none());
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn inbox_authority_resolves_for_any_account_name() {
    run_async_test_on_stack_budget("workbench-inbox-authority-test", || {
        inbox_authority_resolves_for_any_account_name_inner()
    });
}

async fn inbox_authority_resolves_for_any_account_name_inner() {
    let data_dir =
        std::env::temp_dir().join(format!("agent-workbench-inbox-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let mail_world = mail::MailWorld::new();
    mail_world.add_account("test").expect("add test");
    let provider = catalog_lifecycle_provider();
    let model = test_model();
    let session_id = WorkbenchSessions::fresh().current();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .plugin(Arc::new(
            WorkbenchPluginFactory::new().with_mail_world(mail_world.clone()),
        ))
        .build(crate::test_core_owner())
        .expect("build core");
    let session = core.session(session_id).open().await.expect("open session");

    let tool_names = session
        .admin()
        .tools()
        .active_manifests()
        .await
        .expect("active tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(
        tool_names.iter().any(|name| name == "inbox__test__send"),
        "inbox.test send tool should be active: {tool_names:?}"
    );
    assert_tool_catalog_contract(&core, &session).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        assert_plugin_provider_execution(&session, &mail_world),
    )
    .await
    .expect("plugin-provider turn should complete");
    tokio::time::timeout(
        Duration::from_secs(20),
        assert_live_tool_provider_execution_and_removal(&core, &session),
    )
    .await
    .expect("live-provider lifecycle should complete");
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn parallel_inbox_lists_complete_in_durable_workbench_turn() {
    run_async_test_on_stack_budget("workbench-parallel-inbox-list-test", || {
        parallel_inbox_lists_complete_in_durable_workbench_turn_inner()
    });
}

async fn parallel_inbox_lists_complete_in_durable_workbench_turn_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-parallel-inbox-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let mail_world = mail::MailWorld::new();
    mail_world.add_account("test").expect("add test");
    mail_world.add_account("test2").expect("add test2");
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete(|_| async {
            Ok(text_response(
                r#"<typescript>
const boxes = await Promise.all([inbox.test.list({}), inbox.test2.list({})]);
finish({ test: boxes[0], test2: boxes[1] });
</typescript>"#,
            ))
        })
        .build()
        .into_handle();
    let model = test_model();
    let session_id = WorkbenchSessions::fresh().current();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .plugin(Arc::new(
            WorkbenchPluginFactory::new().with_mail_world(mail_world.clone()),
        ))
        .build(crate::test_core_owner())
        .expect("build core");
    let session = core.session(session_id).open().await.expect("open session");

    let output = tokio::time::timeout(
        Duration::from_secs(5),
        session
            .turn(lash::TurnInput::text("list both inboxes"))
            .turn_id(format!("workbench-test-turn:{}", uuid::Uuid::new_v4()))
            .run(),
    )
    .await
    .expect("parallel inbox list turn must not hang")
    .expect("parallel inbox list turn");
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({
            "test": { "account": "test", "messages": [] },
            "test2": { "account": "test2", "messages": [] }
        }))
    );
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
#[ignore = "FIG-3600 S5a: a session command now settles through the session drive, asynchronously; the workbench refresh path that waited on a synchronous drain is rewritten onto send() in S5b"]
fn inbox_added_after_session_open_updates_persisted_tool_catalog() {
    run_async_test_on_stack_budget("workbench-dynamic-inbox-surface-test", || {
        inbox_added_after_session_open_updates_persisted_tool_catalog_inner()
    });
}

async fn inbox_added_after_session_open_updates_persisted_tool_catalog_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-dynamic-inbox-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = session_store_factory;
    let mail_world = mail::MailWorld::new();
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete_error("dynamic inbox surface test should not call the provider")
        .build()
        .into_handle();
    let model = test_model();
    let sessions = WorkbenchSessions::fresh();
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .plugin(Arc::new(
            WorkbenchPluginFactory::new().with_mail_world(mail_world.clone()),
        ))
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(1024),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail_world.clone(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };

    let receipt = Box::pin(enqueue_tool_catalog_refresh(&state, "initial_empty"))
        .await
        .expect("enqueue initial empty refresh");
    drain_refresh_batch(&state, &receipt).await;
    mail_world.add_account("Late Account").expect("add account");
    let receipt = Box::pin(enqueue_tool_catalog_refresh(&state, "account_added"))
        .await
        .expect("enqueue account refresh");
    drain_refresh_batch(&state, &receipt).await;

    let reopened = state
        .core
        .session(state.current_session_id())
        .open()
        .await
        .expect("reopen session");
    let tool_names = reopened
        .admin()
        .tools()
        .active_manifests()
        .await
        .expect("active tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(
        tool_names
            .iter()
            .any(|name| name == "inbox__late_account__send"),
        "late account send tool should be active after persisted refresh: {tool_names:?}"
    );
    reopened.close().await.expect("close session");

    // Removing the account must not poison the persisted session: the
    // provider stops resolving the account's tools, so restore orphans
    // them as non-members instead of
    // failing the reopen.
    mail_world
        .remove_account("late_account")
        .expect("remove account");
    let receipt = Box::pin(enqueue_tool_catalog_refresh(&state, "account_removed"))
        .await
        .expect("enqueue removal refresh");
    drain_refresh_batch(&state, &receipt).await;
    let reopened = state
        .core
        .session(state.current_session_id())
        .open()
        .await
        .expect("reopen session after account removal");
    let tool_state = reopened.admin().tools().state().await.expect("tool state");
    let send_tool_id = lash::tools::ToolId::from("tool:inbox__late_account__send");
    let send_entry = tool_state
        .iter()
        .find_map(|(id, entry)| (id == &send_tool_id).then_some(entry))
        .expect("removed account tool is kept as an orphan");
    assert!(
        send_entry.is_orphaned(),
        "removed account tool must be marked orphaned"
    );
    reopened.close().await.expect("close session");

    // Re-adding the account (same slug, same tool ids) rebinds the
    // orphans on the next refresh — the tools come back without ever
    // having made the session unopenable.
    mail_world
        .add_account("Late Account")
        .expect("re-add account");
    let receipt = Box::pin(enqueue_tool_catalog_refresh(&state, "account_readded"))
        .await
        .expect("enqueue re-add refresh");
    drain_refresh_batch(&state, &receipt).await;
    let reopened = state
        .core
        .session(state.current_session_id())
        .open()
        .await
        .expect("reopen session after account re-add");
    let tool_state = reopened.admin().tools().state().await.expect("tool state");
    let send_entry = tool_state
        .iter()
        .find_map(|(id, entry)| (id == &send_tool_id).then_some(entry))
        .expect("re-added account tool is present");
    assert!(
        !send_entry.is_orphaned(),
        "re-added account tool must be rebound to the live provider"
    );
    reopened.close().await.expect("close session");
    let _ = std::fs::remove_dir_all(data_dir);
}

#[cfg(test)]
#[path = "tests/trigger_lifecycle.rs"]
mod trigger_lifecycle_tests;
#[test]
fn button_trigger_occurrence_is_finishted_to_restate_workflow() {
    run_async_test_on_stack_budget("workbench-trigger-restate-test", || {
        button_trigger_occurrence_is_finishted_to_restate_workflow_inner()
    });
}

async fn button_trigger_occurrence_is_finishted_to_restate_workflow_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-queue-runner-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let backend = test_file_backend(&data_dir);
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
        backend.session_store_factory();
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete(|_| async { Ok(trigger_registration_response()) })
        .build()
        .into_handle();
    let model = test_model();
    let model = with_workbench_model_capability(model);
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    let event_tx = SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)
        .expect("open durable product events");
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend.clone().into(),
    );
    let core = LashCore::rlm_builder(backend.into(), lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .session_spec(lash::SessionSpec::new().turn_budget(lash::TurnBudget::Unbounded))
        .model(model)
        // The `processes` module is catalogue presence, not an ability bit (ADR
        // 0095): the workbench's scripted sources author `processes.*`, so the
        // surface only exists when this factory is installed, as bootstrap does.
        .plugin(Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash::process::lifetime::session_or_starter)))
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx,
        restate_ingress_url,
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let session = state
        .core
        .session(state.current_session_id())
        .open()
        .await
        .expect("open session");
    register_test_trigger(&session).await;
    drop(session);

    let _accepted = button_trigger(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(ButtonEventRequest {
            button: ButtonChoice::Blue,
            model: Some("button-model".to_string()),
            model_variant: Some("high".to_string()),
        }),
    )
    .await
    .expect("button command");
    let selected_model = state.selected_model();
    assert_eq!(selected_model.model, "button-model");
    assert_eq!(selected_model.model_variant.as_deref(), Some("high"));
    assert!(
        state.messages_snapshot().iter().any(|message| {
            message.role == "event" && message.text == "blue button trigger occurrence"
        }),
        "button click should publish the local accepted event"
    );

    let request = tokio::time::timeout(Duration::from_secs(2), restate_requests.recv())
        .await
        .expect("Restate request")
        .expect("Restate request payload");
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .expect("request path");
    assert!(
        path.starts_with("WorkbenchButtonTriggerWorkflow/workbench-button-"),
        "unexpected Restate path: {path}"
    );
    assert!(
        path.ends_with("/run/send"),
        "unexpected Restate path: {path}"
    );
    assert_eq!(
        request.pointer("/body/session_id").and_then(Value::as_str),
        Some(state.current_session_id().as_str())
    );
    assert_eq!(
        request.pointer("/body/button").and_then(Value::as_str),
        Some("Blue")
    );
    assert_eq!(
        request.pointer("/body/model/model").and_then(Value::as_str),
        Some("button-model")
    );
    assert_eq!(
        request
            .pointer("/body/model/model_variant")
            .and_then(Value::as_str),
        Some("high")
    );
    let _ = std::fs::remove_dir_all(data_dir);
}

pub(super) async fn spawn_restate_ingress_capture() -> (String, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Restate ingress");
    let addr = listener.local_addr().expect("mock Restate ingress addr");
    let app = Router::new()
        .route("/{*path}", post(capture_restate_send))
        .with_state(tx);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("mock Restate ingress stopped: {err}");
        }
    });
    (format!("http://{addr}"), rx)
}

/// A capture whose session-delete attach is held open until the test releases
/// it, the way a real delete of a session holding hundreds of processes and
/// live cron jobs holds the reset request open for tens of seconds.
pub(super) async fn spawn_restate_ingress_capture_with_delete_gate() -> (
    String,
    mpsc::UnboundedReceiver<Value>,
    Arc<tokio::sync::Notify>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Notify::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Restate ingress");
    let addr = listener.local_addr().expect("mock Restate ingress addr");
    let app = Router::new()
        .route("/{*path}", post(capture_restate_send_gated))
        .with_state((tx, Arc::clone(&gate)));
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("mock Restate ingress stopped: {err}");
        }
    });
    (format!("http://{addr}"), rx, gate)
}

async fn capture_restate_send_gated(
    AxumPath(path): AxumPath<String>,
    State((tx, gate)): State<(mpsc::UnboundedSender<Value>, Arc<tokio::sync::Notify>)>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    // Restate's own ingress accepts a bodiless call (cron cancel posts none),
    // so a capture that insisted on a JSON body would refuse requests the
    // workbench legitimately makes.
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let _ = tx.send(json!({
        "path": path,
        "body": body,
    }));
    if path.starts_with("WorkbenchSessionDeleteWorkflow/") && !path.ends_with("/send") {
        gate.notified().await;
        return (StatusCode::OK, Json(Value::Null));
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "invocationId": format!("inv_{}", uuid::Uuid::new_v4()),
            "status": "Accepted",
        })),
    )
}

async fn capture_restate_send(
    AxumPath(path): AxumPath<String>,
    State(tx): State<mpsc::UnboundedSender<Value>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let _ = tx.send(json!({
        "path": path,
        "body": body,
    }));
    if path.starts_with("WorkbenchSessionDeleteWorkflow/") && !path.ends_with("/send") {
        return (StatusCode::OK, Json(Value::Null));
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "invocationId": format!("inv_{}", uuid::Uuid::new_v4()),
            "status": "Accepted",
        })),
    )
}

#[test]
fn reset_chat_deletes_old_session_and_clears_trigger_started_work() {
    run_async_test_on_stack_budget("workbench-reset-chat-test", || {
        reset_chat_tests::reset_chat_deletes_old_session_and_clears_trigger_started_work_inner()
    });
}

#[cfg(test)]
#[path = "tests/restate_cron.rs"]
mod restate_cron_tests;
#[cfg(test)]
#[path = "tests/restate_recovery.rs"]
mod restate_recovery_tests;
pub(crate) use restate_cron_tests::{
    LIVE_RESTATE_CRON_ZOMBIE_EXPR, assert_live_non_current_cron_trace,
    assert_queued_turn_sync_cancelled, cron_trace_records_for_job,
    disable_cron_registration_for_sync_scenario, gated_live_restate_cron_provider,
    live_restate_cron_provider, live_restate_cron_tick_wait, retire_cron_session_and_assert_zombie,
    start_live_restate_cron_scenario, wait_for_cron_trace_record_count,
    wait_for_cron_workbench_message,
};

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_cron_zombie_cancel_path_end_to_end() {
    run_async_test_on_stack_budget_multi_thread("workbench-restate-cron-zombie-e2e", 4, || {
        live_restate_cron_zombie_cancel_path_end_to_end_inner()
    });
}

async fn live_restate_cron_zombie_cancel_path_end_to_end_inner() {
    let scenario = start_live_restate_cron_scenario(
        "agent-workbench-restate-cron-zombie-e2e",
        live_restate_cron_provider(LIVE_RESTATE_CRON_ZOMBIE_EXPR.to_string()),
    )
    .await;

    // FIG-1130 ruling: sync cancel is the live fast path and zombie cancel
    // is the crash-safety backstop. This future schedule leaves no queued
    // turn in flight; retirement commits first, then an explicit run drives
    // the backstop without relying on wall-clock ordering.
    retire_cron_session_and_assert_zombie(
        &scenario.state,
        &scenario.trace_path,
        &scenario.cron_session_id,
        &scenario.cron_job_key,
    )
    .await;
    assert_no_active_lash_restate_invocations(&scenario.state, Duration::from_secs(10)).await;
    scenario.shutdown().await;
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_cron_queued_turn_sync_cancel_path_end_to_end() {
    run_async_test_on_stack_budget_multi_thread(
        "workbench-restate-cron-sync-cancel-e2e",
        4,
        live_restate_cron_queued_turn_sync_cancel_path_end_to_end_inner,
    );
}

async fn live_restate_cron_queued_turn_sync_cancel_path_end_to_end_inner() {
    let (provider, mut queued_turn_entered, release_queued_turn) =
        gated_live_restate_cron_provider();
    let scenario =
        start_live_restate_cron_scenario("agent-workbench-restate-cron-sync-cancel-e2e", provider)
            .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), queued_turn_entered.recv())
            .await
            .expect("first cron queued-turn provider call timeout"),
        Some(1),
        "provider call zero registers the cron; call one must be its queued turn"
    );
    // Holding that queued turn pins the job in its scheduled state until a
    // second tick proves run() re-armed the chain. Releasing the provider
    // then deterministically lets the queued-turn sync become the canceler.
    wait_for_cron_trace_record_count(
        &scenario.trace_path,
        "agent_workbench.cron.restate.run",
        &scenario.cron_session_id,
        &scenario.cron_job_key,
        2,
        live_restate_cron_tick_wait(),
    )
    .await;
    assert_live_non_current_cron_trace(
        &scenario.trace_path,
        &scenario.cron_session_id,
        &scenario.cron_job_key,
    );
    disable_cron_registration_for_sync_scenario(&scenario).await;
    release_queued_turn.notify_waiters();
    wait_for_cron_workbench_message(
        &scenario.state,
        &scenario.trace_path,
        &scenario.cron_session_id,
        &scenario.cron_job_key,
        "cron tick observed",
        live_restate_cron_tick_wait(),
    )
    .await;
    assert_queued_turn_sync_cancelled(&scenario).await;
    // FIG-1130 ruling: both cancels are legitimate and idempotent, but the
    // zombie path is only the crash-safety backstop. This live-session path
    // requires sync_cancelled and terminal state, not zombie_cancelled.
    assert!(
        cron_trace_records_for_job(
            &scenario.trace_path,
            "agent_workbench.cron.restate.zombie_cancelled",
            &scenario.cron_session_id,
            &scenario.cron_job_key,
        )
        .is_empty(),
        "sync-cancel scenario must not need the zombie backstop"
    );
    assert_no_active_lash_restate_invocations(&scenario.state, Duration::from_secs(10)).await;
    scenario.shutdown().await;
}

/// A turn a live scenario started the way the chat route does: through the
/// session's `send()`, followed to its settlement on the page.
struct WorkbenchTurn {
    session_id: SessionId,
    turn_id: TurnId,
    follower: tokio::task::JoinHandle<restate::TurnSettlement>,
}

async fn run_workbench_turn_via_restate(state: &AppState, text: &str) -> WorkbenchTurn {
    state.push_message("user", text);
    let session_id = state.current_session_id();
    let turn_id = TurnId::from(format!("workbench-turn-{}", uuid::Uuid::new_v4()));
    // The route claims the session before it sends; so does this.
    state.track_turn_prompt(&session_id, &turn_id, text.to_string(), None);
    let follower = tokio::time::timeout(
        Duration::from_secs(60),
        restate::start_user_turn(
            state,
            restate::UserTurnRequest {
                turn_id: turn_id.clone(),
                session_id: session_id.clone(),
                text: text.to_string(),
                model: state.selected_model(),
                attachment_id: None,
            },
        ),
    )
    .await
    .expect("workbench turn acceptance timed out")
    .expect("accept workbench turn");
    WorkbenchTurn {
        session_id,
        turn_id,
        follower,
    }
}

/// Wait for the turn's follower to settle it on the page; a turn whose
/// terminalization recorded a failure fails the scenario.
async fn wait_for_workbench_turn_settled(turn: &mut WorkbenchTurn, timeout: Duration) {
    let settlement = tokio::time::timeout(timeout, &mut turn.follower)
        .await
        .unwrap_or_else(|_| panic!("turn {} did not settle within {timeout:?}", turn.turn_id))
        .expect("join the turn follower");
    if let Err(error) = settlement {
        panic!("turn {} settled as a failure: {error}", turn.turn_id);
    }
}

/// Wait for the turn's follower to settle it as a failure, and answer the
/// failure its terminalization recorded.
async fn wait_for_workbench_turn_failed(turn: &mut WorkbenchTurn, timeout: Duration) -> String {
    let settlement = tokio::time::timeout(timeout, &mut turn.follower)
        .await
        .unwrap_or_else(|_| panic!("turn {} did not settle within {timeout:?}", turn.turn_id))
        .expect("join the turn follower");
    settlement.expect_err("the turn must settle as a failure")
}

/// The turn a started `/api/turn` send runs as.
pub(super) fn started_turn_id(accepted: &TurnAccepted) -> TurnId {
    assert!(
        !accepted.queued,
        "the send was queued, not started: {accepted:?}"
    );
    accepted
        .turn_id
        .clone()
        .expect("a started send names its turn")
}

/// Wait until the send's follower settles `turn_id` and releases the
/// session's claim on it.
pub(super) async fn wait_for_turn_released(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while state
        .active_turns
        .for_session(session_id)
        .is_some_and(|active| active.address.turn_id == *turn_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "turn {turn_id} was not settled within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The Restate invocation of the turn's `LashTurn`, once the session's engine
/// admitted its root.
async fn lash_turn_invocation(
    state: &AppState,
    turn: &WorkbenchTurn,
    timeout: Duration,
) -> lash_restate::RestateInvocationId {
    lash_turn_invocation_at(
        &state.restate_admin_url,
        &lash::TurnAddress::new(&turn.session_id, &turn.turn_id),
        timeout,
    )
    .await
}

/// [`lash_turn_invocation`] for a caller holding only the admin URL.
pub(super) async fn lash_turn_invocation_at(
    admin_url: &str,
    address: &lash::TurnAddress,
    timeout: Duration,
) -> lash_restate::RestateInvocationId {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(admin_url));
    let key = lash_restate::turn_workflow_key(&address.session_id, &address.turn_id);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = admin
            .workflow_invocation_status("LashTurn", &key, "run")
            .await
            .expect("query the LashTurn invocation")
        {
            return lash_restate::RestateInvocationId::new(status.id);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the engine admitted no LashTurn for {key} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the engine runs nothing on `session_id`: no `LashSession`
/// drive and no `LashTurn` of the session is still in flight. A drive's last
/// admission pass opens the session after its root committed, and a drive
/// the host's own open contended retries, so a test that counts every claim
/// on the session waits for this before it counts.
pub(super) async fn wait_for_session_engine_idle(
    admin_url: &str,
    session_id: &SessionId,
    timeout: Duration,
) {
    #[derive(serde::Deserialize)]
    struct InFlight {
        target: String,
        status: String,
    }
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(admin_url));
    let session = session_id.as_str().replace('\'', "''");
    let query = format!(
        "SELECT target, status FROM sys_invocation WHERE status <> 'completed' AND \
         ((target_service_name = 'LashSession' AND target_service_key = '{session}') OR \
         (target_service_name = 'LashTurn' AND target_service_key LIKE '%:{session}%'))"
    );
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let in_flight = admin
            .query_json::<InFlight>(&query)
            .await
            .expect("query the session's in-flight engine invocations");
        if in_flight.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the engine still runs on {session_id} after {timeout:?}: {:?}",
            in_flight
                .iter()
                .map(|row| format!("{} {}", row.target, row.status))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_restate_invocation_success(
    state: &AppState,
    invocation_id: &lash_restate::RestateInvocationId,
    timeout: Duration,
) {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
            state.restate_http.clone(),
        ));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match admin
            .invocation_status(invocation_id)
            .await
            .expect("query Restate invocation status")
        {
            Some(status) if status.completed_successfully() => return,
            Some(status)
                if status.status == lash_restate::RestateInvocationLifecycle::Completed =>
            {
                panic!("Restate invocation {invocation_id} completed unsuccessfully: {status:#?}")
            }
            Some(status) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for Restate invocation {invocation_id} to complete; last status={status:#?}"
                );
            }
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for Restate invocation {invocation_id} to appear"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn assert_no_active_lash_restate_invocations(state: &AppState, timeout: Duration) {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
            state.restate_http.clone(),
        ));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let active = admin
            .unfinished_invocations_for_service_prefixes(&["Workbench", "LashProcessWorkflow"])
            .await
            .expect("query active Lash Restate invocations");
        if active.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Restate still has active Lash invocations: {active:#?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
struct LiveWorkbenchRestateHarness {
    state: AppState,
    process_worker: lash::durability::DurableProcessWorker,
    backend: Arc<WorkbenchRestateBackend>,
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
    trace_path: PathBuf,
}

/// The durable trust domain the live Restate legs run under.
///
/// The workbench host and the durable turn-control controller each derive their binding id
/// from it independently, so a hardcoded value here disagrees with the controller the moment
/// the script names a real authority — which is exactly what `turn-control host authority ...
/// does not match controller authority` reports.
/// The literal stays as the fallback so the suites that run without the script keep their
/// stable, self-consistent domain.
fn live_restate_authority_id() -> lash_restate::RestateAuthorityId {
    let value = std::env::var("RESTATE_AUTHORITY_ID")
        .unwrap_or_else(|_| "agent-workbench-tests".to_string());
    lash_restate::RestateAuthorityId::new(value).expect("valid Restate authority id")
}

async fn live_workbench_restate_state_with_provider(
    data_dir: &std::path::Path,
    restate_ingress_url: String,
    provider: ProviderHandle,
    sessions: WorkbenchSessions,
    active_turns: ActiveTurns,
) -> LiveWorkbenchRestateHarness {
    live_workbench_restate_state_with_provider_and_database(
        data_dir,
        restate_ingress_url,
        provider,
        sessions,
        active_turns,
        None,
        lash::durability::LeaseTimings::default(),
    )
    .await
}

async fn live_workbench_restate_state_with_provider_and_database(
    data_dir: &std::path::Path,
    restate_ingress_url: String,
    provider: ProviderHandle,
    sessions: WorkbenchSessions,
    active_turns: ActiveTurns,
    database_url: Option<&str>,
    lease_timings: lash::durability::LeaseTimings,
) -> LiveWorkbenchRestateHarness {
    // An isolated live-test runner may need to retain this exact store when a
    // fixture aborts. Record ownership before opening any replayable handle;
    // normal fixture teardown still removes its own directory directly.
    record_fixture_owned_data_dir(data_dir);
    let stores = WorkbenchStores::open(data_dir, database_url)
        .await
        .expect("open live workbench stores");
    let mut core_store_factory = stores.stores.session_store_factory();
    let session_id = sessions.current();
    let admission_gate = registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&SessionId::from(&session_id))
        .cloned();
    let mut store_set = Arc::clone(&stores.stores);
    if let Some(gate) = admission_gate {
        core_store_factory = Arc::new(GatedSessionStoreFactory {
            inner: core_store_factory,
            gate,
        });
        store_set = Arc::new(GatedStoreSet {
            inner: store_set,
            catalog: Arc::clone(&core_store_factory),
        });
    }
    let trigger_store = store_set.trigger_store();
    let process_env_store = store_set.process_env_store();
    let trace_path = data_dir.join("trace.jsonl");
    let lashlang_execution_path = data_dir.join("lashlang-execution.jsonl");
    let trace_sink = Arc::new(JsonlTraceSink::new(trace_path.clone())) as Arc<dyn TraceSink>;
    let lashlang_execution = Arc::new(TraceLashlangGraphStore::default());
    let lashlang_execution_sink = Arc::new(TeeTraceSink::new([
        Arc::clone(&lashlang_execution) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new(lashlang_execution_path)) as Arc<dyn TraceSink>,
    ])) as Arc<dyn TraceSink>;
    let model = lash::ModelSpec::builder("mock-model")
        .variant(lash::provider::ReasoningSelection::Effort(
            "high".to_string(),
        ))
        .context_window_tokens(4096)
        .build()
        .expect("model spec");
    let model = with_workbench_model_capability(model);
    let restate_http = reqwest::Client::new();
    let restate_admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let backend = Arc::new(lash_restate::RestateEngine::new(
        store_set,
        lash::restate::config(
            lash_restate::RestateConnection::with_client(
                restate_ingress_url.clone(),
                restate_http.clone(),
            ),
            lash_restate::RestateConnection::with_client(
                restate_admin_url.clone(),
                restate_http.clone(),
            ),
            live_restate_authority_id(),
        ),
    ));
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend.clone().into(),
    )
    .with_lashlang_execution_sink(lashlang_execution_sink);
    let core = LashCore::rlm_builder(
        lash::Backend::new(backend.clone()),
        lash::TurnBudget::Unbounded,
        factory,
    )
        .provider(provider)
        .model(model)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .trace_sink(Arc::clone(&trace_sink))
        .trace_level(TraceLevel::Extended)
        // The `processes` module is catalogue presence, not an ability bit (ADR
        // 0095): the workbench's scripted sources author `processes.*`, so the
        // surface only exists when this factory is installed, as bootstrap does.
        .plugin(Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash::process::lifetime::session_or_starter)))
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()))
        .lease_timings(lease_timings)
        .build(crate::test_core_owner())
        .expect("build core");
    let process_worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("build process worker config"),
    )
    .expect("valid test native substrate config");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let event_tx = SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)
        .expect("open durable product events");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store,
        process_observer,
        // Process work is resolved through the core.
        sessions,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "mock-model".to_string(),
            model_variant: Some("high".to_string()),
        })),
        trace_sink: Some(trace_sink),
        lashlang_execution,
        event_tx,
        restate_ingress_url,
        restate_admin_url,
        restate_http,
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns,
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    // As the host's bootstrap does: follow the turns a previous incarnation
    // over this data directory was following.
    Box::pin(restate::resume_turn_followers(&state)).await;
    LiveWorkbenchRestateHarness {
        state,
        process_worker,
        backend,
        process_env_store,
        trace_path,
    }
}

async fn wait_for_workbench_message(state: &AppState, needle: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let messages = state.messages_snapshot();
        if messages
            .iter()
            .any(|message| message.role == "assistant" && message.text.contains(needle))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for workbench message containing `{needle}`; messages={messages:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_trace_event_count(
    path: &std::path::Path,
    needle: &str,
    count: usize,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = std::fs::read_to_string(path)
            .map(|text| text.matches(needle).count())
            .unwrap_or(0);
        if seen >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count}+ `{needle}` trace events within {timeout:?}, saw {seen}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_restate_cron_sync(
    state: &AppState,
    trace_path: &std::path::Path,
    timeout: Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let known_jobs = state.restate_cron_job_keys.lock_recover().clone();
        let trace_text = std::fs::read_to_string(trace_path).unwrap_or_default();
        if !known_jobs.is_empty()
            && trace_text.contains("agent_workbench.cron.restate.sync_upserted")
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for Restate cron sync; known_jobs={known_jobs:#?}; messages={:#?}; trace_tail={}",
            state.messages_snapshot(),
            trace_tail(trace_path),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn trace_tail(path: &std::path::Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("<unreadable {}>", path.display());
    };
    let mut lines = text.lines().rev().take(20).collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

async fn wait_for_endpoint_socket(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Restate endpoint did not open a TCP listener at {addr}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn persisted_trigger_route_fires_after_reopening_sqlite_artifact_store() {
    run_async_test_on_stack_budget("workbench-persisted-trigger-test", || {
        persisted_trigger_route_fires_after_reopening_sqlite_artifact_store_inner()
    });
}

async fn persisted_trigger_route_fires_after_reopening_sqlite_artifact_store_inner() {
    let data_dir =
        std::env::temp_dir().join(format!("agent-workbench-trigger-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let session_id = WorkbenchSessions::fresh().current();

    {
        let core = test_workbench_core(test_file_backend(&data_dir));
        let session = core
            .session(session_id.clone())
            .open()
            .await
            .expect("open session");
        register_test_trigger(&session).await;
        drop(session);
        drop(core);
    }

    // A fresh backend over the same root: the registered trigger, its
    // compiled artifacts and the process registry are read back from disk.
    let backend = test_file_backend(&data_dir);
    let process_registry = backend.process_registry() as Arc<dyn lash::process::ProcessRegistry>;
    let core = test_workbench_core(backend);
    let _reopened = core
        .session(session_id)
        .open()
        .await
        .expect("reopen session");
    let report = emit_test_button_trigger(&core, ButtonChoice::Blue).await;
    assert_eq!(report.started_process_ids().len(), 1);
    lash::process::NativeProcessWork::for_registry(Arc::clone(&process_registry))
        .await_terminal(&report.started_process_ids()[0])
        .await
        .expect("trigger process should finish");

    let _ = std::fs::remove_dir_all(data_dir);
}

/// Drains an enqueued command batch in-process. The test cores' effect host
/// permits foreground drains; in production the session's engine drives the
/// same batch inside its `LashSession` handler.
async fn drain_refresh_batch(state: &AppState, receipt: &lash::SessionCommandReceipt) {
    let session = state
        .core
        .session(receipt.session_id.clone())
        .open()
        .await
        .expect("open session to drain refresh batch");
    let _ = session
        .queued_turn()
        .batch_ids([receipt.batch_id.clone()])
        .drain_id(receipt.batch_id.clone())
        .run()
        .await
        .expect("drain refresh batch");
    session.close().await.expect("close drain session");
}

#[cfg(test)]
#[path = "tests/queued_work.rs"]
mod queued_work_tests;
#[cfg(test)]
#[path = "tests/session_isolation.rs"]
mod session_isolation_tests;
fn test_workbench_core(backend: Arc<lash_sqlite_store::SqliteBackend>) -> LashCore {
    let provider = trigger_registration_provider();
    let model = test_model();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend.clone().into(),
    );
    LashCore::rlm_builder(backend.into(), lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .session_spec(lash::SessionSpec::new().turn_budget(lash::TurnBudget::Unbounded))
        .model(model)
        // The `processes` module is catalogue presence, not an ability bit (ADR
        // 0095): the workbench's scripted sources author `processes.*`, so the
        // surface only exists when this factory is installed, as bootstrap does.
        .plugin(Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash::process::lifetime::session_or_starter)))
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .build(crate::test_core_owner())
        .expect("build core")
}

pub(super) fn text_response(text: &str) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts: vec![lash::direct::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..lash::provider::LlmResponse::default()
    }
}

fn trigger_registration_response() -> lash::provider::LlmResponse {
    text_response(&format!(
        "<typescript>\n{}\n</typescript>",
        test_button_trigger_source().trim()
    ))
}

async fn register_test_trigger(session: &lash::LashSession) {
    let output = session
        .turn(lash::TurnInput::text("register trigger"))
        .turn_id(format!("workbench-test-register:{}", uuid::Uuid::new_v4()))
        .run()
        .await
        .expect("register trigger route");
    assert_eq!(output.final_value(), Some(&serde_json::json!("registered")));
}

#[cfg(test)]
#[path = "tests/remote_trigger_assertions.rs"]
mod remote_trigger_assertions_tests;
pub(crate) use remote_trigger_assertions_tests::{
    assert_remote_started_process_surface, assert_remote_trigger_emit_report_round_trip,
    assert_remote_trigger_subscription_records_round_trip,
};

async fn emit_test_button_trigger(
    core: &LashCore,
    button: ButtonChoice,
) -> lash::triggers::TriggerEmitReport {
    emit_test_button_trigger_with_scope(core, button, None).await
}

async fn emit_test_button_trigger_for_session(
    core: &LashCore,
    button: ButtonChoice,
    session_id: &SessionId,
) -> lash::triggers::TriggerEmitReport {
    emit_test_button_trigger_with_scope(core, button, Some(session_id)).await
}

async fn emit_test_button_trigger_with_scope(
    core: &LashCore,
    button: ButtonChoice,
    session_id: Option<&SessionId>,
) -> lash::triggers::TriggerEmitReport {
    let source_key =
        lash::triggers::empty_trigger_source_key(BUTTON_TRIGGER_SOURCE_TYPE).expect("source key");
    let idempotency_key = format!(
        "workbench-test-button-trigger:{}:{}",
        button.as_str(),
        uuid::Uuid::new_v4()
    );
    let scoped_effect_controller = lash::durability::EffectHost::scoped_static(
        core.effect_host().as_ref(),
        lash::runtime::AdmittedScope::runtime_operation(format!("trigger:{idempotency_key}")),
    )
    .expect("trigger occurrence execution scope")
    .expect("the backend host lends an owned controller");
    let mut request = lash::triggers::TriggerOccurrenceRequest::new(
        BUTTON_TRIGGER_SOURCE_TYPE,
        source_key,
        json!({
            "button": button.as_str(),
            "message": format!("user pressed the {} button", button.lower()),
            "pressed_at": "2026-06-02T12:00:00Z"
        }),
        idempotency_key,
    )
    .with_source(json!({}));
    if let Some(session_id) = session_id {
        request = request.for_session(session_id);
    }
    core.triggers()
        .emit(request, scoped_effect_controller)
        .await
        .expect("emit button trigger occurrence")
}

fn trigger_registration_provider() -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete(|_| async { Ok(trigger_registration_response()) })
        .build()
        .into_handle()
}

fn test_button_trigger_source() -> &'static str {
    r#"
        const remember = async (event: unknown) => {
          await processes.emit({ value: { kind: "button_pressed", button: event.button, message: event.message } });
          return { button: event.button, ok: true };
        };

        const handle = await triggers.register({
          source: ui.button.pressed({}),
          target: remember,
          inputs: (event) => ({ event: event }),
          name: "remembered"
        });
        finish("registered");
        "#
}

#[cfg(test)]
#[path = "tests/attachments_usage.rs"]
mod attachments_usage_tests;
#[cfg(test)]
#[path = "tests/store_maintenance.rs"]
mod store_maintenance_tests;
#[cfg(test)]
#[path = "tests/turn_input_application.rs"]
mod turn_input_application_tests;
pub(crate) use turn_input_application_tests::assert_typed_turn_input_application;
#[cfg(test)]
#[path = "tests/concurrent_send.rs"]
mod concurrent_send_tests;
#[cfg(test)]
#[path = "tests/tool_control.rs"]
mod tool_control_tests;
pub(crate) use concurrent_send_tests::{product_user_rows, queued_send_test_state};
#[cfg(test)]
#[path = "tests/no_progress_budget.rs"]
mod no_progress_budget_tests;
#[cfg(test)]
#[path = "tests/session_fence.rs"]
mod session_fence_tests;
#[cfg(test)]
#[path = "tests/session_open_admission.rs"]
mod session_open_admission_tests;
pub(crate) use session_open_admission_tests::{
    GatedSessionStoreFactory, GatedStoreSet, SessionOpenAdmissionGate,
    register_session_open_admission_gate, registered_session_open_admission_gates,
    unregister_session_open_admission_gate,
};

pub(crate) use recoverable_chat_tests::recoverable_chat_test_state_with_store_factory_and_trigger_store;

#[path = "tests/observation_config.rs"]
mod observation_config_tests;

#[path = "tests/reset_chat.rs"]
mod reset_chat_tests;
