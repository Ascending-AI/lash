#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn in_memory_trigger_store() -> Arc<dyn lash::triggers::TriggerStore> {
        Arc::new(lash::triggers::InMemoryTriggerStore::new())
    }
    use lash::rlm::RlmTurnBuilderExt;
    use lash::tracing::{
        TraceBranchSelection, TraceLashlangChildExecution, TraceLashlangEdgeSelection,
        TraceLashlangExecutionEvent, TraceLashlangExecutionIdentity, TraceLashlangGraphChildLink,
        TraceLashlangMap, TraceLashlangMapEdge, TraceLashlangMapNode, TraceLashlangStatus,
        TraceRuntimeScope, TraceRuntimeSubject,
    };
    use std::future::Future;

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

    include!("tests/recoverable_chat.rs");
    include!("tests/tool_catalog.rs");

    pub(super) fn explicit_durable_test_facets(
        data_dir: &std::path::Path,
    ) -> lash::LashCoreBuilder {
        let artifact_store = Arc::new(sync_await({
            let path = data_dir.join("artifacts.db");
            async move {
                lash_sqlite_store::Store::open(&path)
                    .await
                    .expect("open artifact store")
            }
        })) as Arc<dyn lash::persistence::LashlangArtifactStore>;
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::default()
                .with_lashlang_abilities(workbench_lashlang_abilities()),
            artifact_store,
        );
        lash::LashCore::rlm_builder(factory)
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .process_env_store(Arc::new(sync_await({
                let path = data_dir.join("process-env.db");
                async move {
                    lash_sqlite_store::Store::open(&path)
                        .await
                        .expect("open process env store")
                }
            })))
            .trigger_store(Arc::new(sync_await({
                let path = data_dir.join("triggers.db");
                async move {
                    lash_sqlite_store::SqliteTriggerStore::open(&path)
                        .await
                        .expect("open trigger store")
                }
            })))
    }

    const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

    pub(super) fn run_async_test_on_stack_budget<F, Fut>(name: &str, test: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(|| {
                let test = Box::pin(test());
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime")
                    .block_on(test)
            })
            .expect("spawn stack-budget test thread")
            .join()
            .expect("stack-budget test thread");
    }

    fn run_async_test_on_stack_budget_multi_thread<F, Fut>(
        name: &str,
        worker_threads: usize,
        test: F,
    ) where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(move || {
                let test = Box::pin(test());
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads)
                    .thread_stack_size(STACK_BUDGET_BYTES)
                    .enable_all()
                    .build()
                    .expect("tokio runtime")
                    .block_on(test)
            })
            .expect("spawn stack-budget multi-thread test thread")
            .join()
            .expect("stack-budget multi-thread test thread");
    }

    fn test_graph(
        graph_key: &str,
        session_id: &str,
        subject: TraceRuntimeSubject,
        children: Vec<TraceLashlangGraphChildLink>,
    ) -> TraceLashlangGraph {
        TraceLashlangGraph {
            graph_key: graph_key.to_string(),
            scope: TraceRuntimeScope::new(session_id),
            subject,
            module_ref: format!("{graph_key}:module"),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLashlangStatus::Running,
            nodes: Vec::new(),
            edges: Vec::new(),
            children,
        }
    }

    fn append_started_graph(store: &TraceLashlangGraphStore, graph: &TraceLashlangGraph) {
        let identity = TraceLashlangExecutionIdentity {
            scope: graph.scope.clone(),
            subject: graph.subject.clone(),
            module_ref: graph.module_ref.clone(),
            entry_kind: graph.entry_kind.clone(),
            entry_ref: graph.entry_ref.clone(),
            entry_name: graph.entry_name.clone(),
        };
        store
            .append(&TraceRecord::new(
                TraceContext::default().for_session(graph.scope.session_id.clone()),
                TraceEvent::LashlangExecution {
                    event: TraceLashlangExecutionEvent::ExecutionStarted {
                        event_key: format!("{}:start", graph.graph_key),
                        identity,
                        execution_map: TraceLashlangMap {
                            module_ref: graph.module_ref.clone(),
                            entry_kind: graph.entry_kind.clone(),
                            entry_ref: graph.entry_ref.clone(),
                            entry_name: graph.entry_name.clone(),
                            nodes: Vec::new(),
                            edges: Vec::new(),
                        },
                    },
                },
            ))
            .expect("append test graph");
    }

    #[test]
    fn reset_session_rotation_replaces_workbench_session_id() {
        let ids = WorkbenchSessionIds::fresh();
        let original = ids.current();
        let (old, new) = ids.rotate();
        assert_eq!(old, original);
        assert_eq!(ids.current(), new);
        assert_ne!(old, new);
        assert!(old.starts_with(SESSION_ID_PREFIX));
        assert!(new.starts_with(SESSION_ID_PREFIX));
    }

    #[test]
    fn turn_routing_state_survives_web_process_reconstruction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_path = temp.path().join("session-id");
        let turns_path = temp.path().join("active-turns.json");
        let session_ids =
            WorkbenchSessionIds::persistent(session_path.clone()).expect("session ids");
        let session_id = session_ids.current();
        let turns = ActiveTurns::persistent(turns_path.clone()).expect("active turns");
        turns.insert_with_prompt(
            &session_id,
            "durable-stop-turn",
            Some("actual restored prompt".into()),
        );
        drop(session_ids);
        drop(turns);
        let recovered_ids = WorkbenchSessionIds::persistent(session_path).expect("recover ids");
        let recovered_turns = ActiveTurns::persistent(turns_path).expect("recover turns");
        assert_eq!(recovered_ids.current(), session_id);
        assert_eq!(
            recovered_turns.for_session(&session_id),
            vec![lash::TurnAddress::new(&session_id, "durable-stop-turn")]
        );
        assert_eq!(
            recovered_turns
                .prompt_for(&session_id, "durable-stop-turn")
                .as_deref(),
            Some("actual restored prompt")
        );
    }

    include!("tests/session_resume.rs");
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

    include!("tests/ui_contract.rs");

    #[test]
    fn mail_received_event_type_matches_source_type() {
        let resources = workbench_lashlang_resources();
        let binding = resources
            .resolve_trigger_source(MAIL_RECEIVED_SOURCE_TYPE)
            .expect("mail.received source registered");
        assert_eq!(binding.event_type_name(), "mail.Received");
    }

    include!("tests/facade_homes.rs");

    #[test]
    fn lashlang_graph_store_builds_graph_state() {
        let store = TraceLashlangGraphStore::default();
        let context = TraceContext::default().for_session("s1");
        let identity = TraceLashlangExecutionIdentity {
            scope: TraceRuntimeScope::new("s1"),
            subject: TraceRuntimeSubject::Process {
                process_id: "p1".to_string(),
            },
            module_ref: "m1".to_string(),
            entry_kind: "process".to_string(),
            entry_ref: Some("r1:0".to_string()),
            entry_name: "main".to_string(),
        };
        let append = |event: TraceLashlangExecutionEvent| {
            store
                .append(&TraceRecord::new(
                    context.clone(),
                    TraceEvent::LashlangExecution { event },
                ))
                .expect("append tracking event");
        };

        append(TraceLashlangExecutionEvent::ExecutionStarted {
            event_key: "p1:start".to_string(),
            identity: identity.clone(),
            execution_map: TraceLashlangMap {
                module_ref: "m1".to_string(),
                entry_kind: "process".to_string(),
                entry_ref: Some("r1:0".to_string()),
                entry_name: "main".to_string(),
                nodes: vec![TraceLashlangMapNode {
                    id: "branch".to_string(),
                    kind: "branch".to_string(),
                    label: "if".to_string(),
                    label_metadata: None,
                }],
                edges: vec![
                    TraceLashlangMapEdge {
                        id: "then-edge".to_string(),
                        from: "branch".to_string(),
                        to: "then".to_string(),
                        label: "then".to_string(),
                    },
                    TraceLashlangMapEdge {
                        id: "else-edge".to_string(),
                        from: "branch".to_string(),
                        to: "else".to_string(),
                        label: "else".to_string(),
                    },
                ],
            },
        });
        append(TraceLashlangExecutionEvent::BranchSelected {
            event_key: "p1:branch".to_string(),
            identity: identity.clone(),
            node_id: "branch".to_string(),
            occurrence: 1,
            edge_id: "then-edge".to_string(),
            selected: TraceBranchSelection::Then,
        });
        append(TraceLashlangExecutionEvent::ChildStarted {
            event_key: "p1:child".to_string(),
            identity,
            parent_node_id: "branch".to_string(),
            occurrence: 1,
            child: TraceLashlangChildExecution {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p2".to_string(),
                },
                module_ref: Some("m1".to_string()),
                entry_ref: Some("r2:1".to_string()),
                entry_name: Some("child".to_string()),
            },
        });

        let graph = store.graph("process:p1").expect("graph");
        assert_eq!(graph.status, TraceLashlangStatus::Running);
        assert_eq!(graph.children.len(), 1);
        assert_eq!(graph.children[0].child_graph_key, "process:p2");
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "then-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Selected)
        );
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "else-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Rejected)
        );
    }

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
    fn done_stream_items_are_transient_and_not_snapshotted() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-transient-done-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(sync_await({
            let path = data_dir.join("processes.db");
            async move {
                lash_sqlite_store::SqliteProcessRegistry::open(&path, path.with_extension("sessions"))
                    .await
                    .expect("open registry")
            }
        })) as Arc<dyn lash::process::ProcessRegistry>;
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("transient done test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let session_id = state.current_session_id();
        let mut events = state.event_tx.subscribe(&session_id);

        state.publish_turn_done(&session_id, "transient-turn");

        assert!(matches!(
            events.try_recv(),
            Ok(ProductEvent {
                item: StreamItem::Done {
                    turn_id: Some(turn_id),
                    outcome: TurnDoneOutcome::Completed,
                },
                ..
            }) if turn_id == "transient-turn"
        ));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn trigger_dispatch_done_does_not_clear_an_active_turn() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-trigger-dispatch-done-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(sync_await({
            let path = data_dir.join("processes.db");
            async move {
                lash_sqlite_store::SqliteProcessRegistry::open(&path, path.with_extension("sessions"))
                    .await
                    .expect("open registry")
            }
        })) as Arc<dyn lash::process::ProcessRegistry>;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-trigger-dispatch-done-test")
            .complete_error("trigger dispatch done test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None)
                .expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(store_factory)
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let session_id = state.current_session_id();
        let mut events = state.event_tx.subscribe(&session_id);

        state.track_turn(&session_id, "foreground-turn");
        state.publish_trigger_dispatch_done(&session_id, "trigger-running");
        assert!(
            matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "trigger dispatch must not publish Done while a foreground turn is active"
        );

        state.active_turns.remove(&session_id, "foreground-turn");
        state.publish_trigger_dispatch_done(&session_id, "trigger-settled");
        assert!(matches!(
            events.try_recv(),
            Ok(ProductEvent {
                item: StreamItem::Done {
                    turn_id: None,
                    outcome: TurnDoneOutcome::Completed,
                },
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(data_dir);
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
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-observation-stream-test")
            .complete(|_request| async {
                Ok(text_response(
                    r#"<lashlang>
finish "observed through live replay"
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model.clone())
            .store_factory(Arc::clone(&store_factory))
            .disable_queued_work_driver()
            .build()
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
                | ObservationStreamItem::TerminalReplacement { .. } => {}
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
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-observation-gap-test")
            .complete(|_request| async {
                Ok(text_response(
                    r#"<lashlang>
finish "gap source"
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model.clone())
            .store_factory(Arc::clone(&store_factory))
            .live_replay_store(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
                lash::observe::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash::observe::InMemoryLiveReplayStoreConfig::default()
                },
            )))
            .build()
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
                assert_eq!(gap.requested_cursor, requested_cursor);
                assert!(
                    !gap.latest_cursor.is_empty(),
                    "gap should include the latest recoverable cursor"
                );
                assert_eq!(observation.cursor, gap.latest_cursor);
                assert_eq!(observation.session_id, "workbench-observation-gap");
                assert_eq!(
                    gap.reason,
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
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("cancel route test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let session_id = state.current_session_id();
        let mut events = state.event_tx.subscribe(&session_id);
        state.track_turn(&session_id, "turn-cancel");
        let session = state
            .core
            .session(&session_id)
            .open()
            .await
            .expect("open cancelled session");
        state
            .core
            .turn_work_driver()
            .request_cancel(lash::TurnCancelRequest::new(
                session.turn_address("turn-cancel"),
                "original-stop",
                Some("user".to_string()),
            ))
            .await
            .expect("seed cancellation request");
        let (cancelled, turn) = tokio::join!(
            cancel_turn(State(state.clone()), Query(SessionQuery::default())),
            session
                .turn(lash::TurnInput::text("already cancelled"))
                .turn_id("turn-cancel")
                .run(),
        );
        let Json(accepted) = cancelled.expect("cancel turn");
        let turn = turn.expect("cancelled turn commits");
        assert!(accepted.accepted);
        assert!(matches!(
            turn.result.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled)
        ));
        assert!(matches!(
            accepted.cancellations.as_slice(),
            [TurnCancelReceipt {
                outcome: lash::TurnCancelOutcome::AlreadyRequested(_),
                terminal: Some(lash::TurnTerminal::Committed {
                    cancellation: Some(evidence),
                    ..
                }),
                ..
            }] if evidence.request_id == "original-stop"
                && evidence.origin.as_deref() == Some("user")
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(ProductEvent {
                item: StreamItem::Done { .. },
                ..
            })
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
            lash::TurnCancelOutcome::AlreadyRequested(_)
        ));
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
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let mail_world = mail::MailWorld::new();
        mail_world.add_account("test").expect("add test");
        let provider = catalog_lifecycle_provider();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let session_id = WorkbenchSessionIds::fresh().current();
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(
                WorkbenchPluginFactory::new("").with_mail_world(mail_world.clone()),
            ))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let session = core.session(session_id).open().await.expect("open session");

        let tool_names = session
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
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let mail_world = mail::MailWorld::new();
        mail_world.add_account("test").expect("add test");
        mail_world.add_account("test2").expect("add test2");
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete(|_| async {
                Ok(text_response(
                    r#"<lashlang>
initial = await {
  test: inbox.test.list({})?,
  test2: inbox.test2.list({})?
}
finish initial
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let session_id = WorkbenchSessionIds::fresh().current();
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(
                WorkbenchPluginFactory::new("").with_mail_world(mail_world.clone()),
            ))
            .process_registry(Arc::clone(&process_registry))
            .build()
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
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let mail_world = mail::MailWorld::new();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("dynamic inbox surface test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let session_ids = WorkbenchSessionIds::fresh();
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(
                WorkbenchPluginFactory::new("").with_mail_world(mail_world.clone()),
            ))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids,
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(1024),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail_world.clone(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };

        let receipt = enqueue_tool_catalog_refresh(&state, "initial_empty")
            .await
            .expect("enqueue initial empty refresh");
        drain_refresh_batch(&state, &receipt).await;
        mail_world.add_account("Late Account").expect("add account");
        let receipt = enqueue_tool_catalog_refresh(&state, "account_added")
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
        let receipt = enqueue_tool_catalog_refresh(&state, "account_removed")
            .await
            .expect("enqueue removal refresh");
        drain_refresh_batch(&state, &receipt).await;
        let reopened = state
            .core
            .session(state.current_session_id())
            .open()
            .await
            .expect("reopen session after account removal");
        let tool_state = reopened.tools().state().await.expect("tool state");
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
        let receipt = enqueue_tool_catalog_refresh(&state, "account_readded")
            .await
            .expect("enqueue re-add refresh");
        drain_refresh_batch(&state, &receipt).await;
        let reopened = state
            .core
            .session(state.current_session_id())
            .open()
            .await
            .expect("reopen session after account re-add");
        let tool_state = reopened.tools().state().await.expect("tool state");
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

    include!("tests/trigger_lifecycle.rs");
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
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let trigger_store = Arc::new(
            lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                .await
                .expect("open trigger store"),
        );
        let artifact_store = Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                .await
                .expect("open artifact store"),
        );
        let artifact_store_for_core: Arc<dyn lashlang::LashlangArtifactStore> =
            artifact_store.clone();
        let process_env_store = Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
                .await
                .expect("open process env store"),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete(|_| async { Ok(trigger_registration_response()) })
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let model = with_workbench_model_capability(model);
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let event_tx =
            SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)
                .expect("open durable product events");
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::default()
                .with_lashlang_abilities(workbench_lashlang_abilities()),
            artifact_store_for_core,
        );
        let core = LashCore::rlm_builder(factory)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .process_registry(Arc::clone(&process_registry))
            .trigger_store(trigger_store)
            .advanced()
            .runtime_host_config(lash::durability::RuntimeHostConfig::new(
                Arc::new(lash::durability::InlineEffectHost::default()),
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    data_dir.join("attachments"),
                )),
                process_env_store,
            ))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url,
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
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

    async fn capture_restate_send(
        AxumPath(path): AxumPath<String>,
        State(tx): State<mpsc::UnboundedSender<Value>>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let _ = tx.send(json!({
            "path": path,
            "body": body,
        }));
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
            reset_chat_deletes_old_session_and_clears_trigger_started_work_inner()
        });
    }

    async fn reset_chat_deletes_old_session_and_clears_trigger_started_work_inner() {
        let data_dir =
            std::env::temp_dir().join(format!("agent-workbench-reset-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let provider = trigger_registration_provider();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(vec![ChatMessage {
                id: "message".to_string(),
                role: "user".to_string(),
                text: "before reset".to_string(),
                at: "2026-05-27T00:00:00Z".to_string(),
            }])),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(1024),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url,
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let old_session_id = state.current_session_id();
        let _deleted_session_events = state.event_tx.subscribe(&old_session_id);
        assert!(state.event_tx.contains(&old_session_id));
        let session = state
            .core
            .session(old_session_id.clone())
            .open()
            .await
            .expect("open old session");
        register_test_trigger(&session).await;
        let started = emit_test_button_trigger(&state.core, ButtonChoice::Red).await;
        assert_remote_trigger_emit_report_round_trip(&started);
        let trigger_records =
            assert_remote_trigger_subscription_records_round_trip(&data_dir, &old_session_id).await;
        assert_eq!(trigger_records.len(), 1);
        assert_eq!(started.started_process_ids().len(), 1);
        let old_work_before_reset = state
            .process_observer
            .snapshot_for_session(&old_session_id)
            .await
            .expect("old work before reset");
        assert_eq!(
            old_work_before_reset.visible_process_ids,
            started.started_process_ids()
        );
        append_started_graph(
            &state.lashlang_execution,
            &test_graph(
                "process:old-reset-process",
                &old_session_id,
                TraceRuntimeSubject::Process {
                    process_id: "old-reset-process".to_string(),
                },
                Vec::new(),
            ),
        );
        assert_eq!(state.lashlang_execution.graphs().len(), 1);
        assert_remote_started_process_surface(
            &state.core,
            process_registry.as_ref(),
            &old_session_id,
            &started.started_process_ids(),
        )
        .await;
        state
            .mail_world
            .add_account("Reset Probe")
            .expect("add account before reset");
        drop(session);
        let query = Query(SessionQuery { session_id: Some(old_session_id.clone()) });
        let Json(snapshot) = Box::pin(reset_chat(State(state.clone()), query))
            .await
            .expect("reset");

        assert_ne!(snapshot.settings.session_id, old_session_id);
        assert!(!state.event_tx.contains(&old_session_id));
        assert!(snapshot.messages.is_empty());
        assert!(state.messages_snapshot().is_empty());
        assert!(
            state.mail_world.account_summaries().is_empty(),
            "reset must clear mail accounts along with the chat session"
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
            path.starts_with("WorkbenchSessionDeleteWorkflow/workbench-delete-"),
            "unexpected Restate path: {path}"
        );
        assert!(
            path.ends_with("/run/send"),
            "unexpected Restate path: {path}"
        );
        assert_eq!(
            request.pointer("/body/session_id").and_then(Value::as_str),
            Some(old_session_id.as_str())
        );
        let old_work_after_reset = state
            .process_observer
            .snapshot_for_session(&old_session_id)
            .await
            .expect("old work after reset submission");
        assert_eq!(
            old_work_after_reset.visible_process_ids,
            started.started_process_ids(),
            "mock Restate ingress must not consume deletion work inline"
        );
        assert!(
            state
                .core
                .session(snapshot.settings.session_id)
                .open()
                .await
                .expect("open new session")
                .processes()
                .list()
                .await
                .expect("new work")
                .is_empty()
        );
        let Json(graph_index) =
            list_lashlang_graphs(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list graphs after reset");
        assert!(
            graph_index.graphs.is_empty(),
            "new session graph index should be empty after reset: {graph_index:#?}"
        );
        core_store_factory
            .delete_session(&old_session_id)
            .await
            .expect("retire old session for route check");
        let retired_error = Box::pin(app_state(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(old_session_id.clone()),
            }),
        ))
        .await
        .expect_err("retired session state must be refused");
        assert_eq!(retired_error.status, StatusCode::CONFLICT);
        assert!(retired_error.message.contains(&old_session_id));
        assert!(retired_error.message.contains("was used and deleted"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    include!("tests/restate_recovery.rs");

    #[test]
    #[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
    fn live_restate_cron_runs_trigger_and_queued_turn_end_to_end() {
        run_async_test_on_stack_budget_multi_thread("workbench-restate-cron-e2e", 4, || {
            live_restate_cron_runs_trigger_and_queued_turn_end_to_end_inner()
        });
    }

    async fn live_restate_cron_runs_trigger_and_queued_turn_end_to_end_inner() {
        let ingress_url = match std::env::var("RESTATE_INGRESS_URL") {
            Ok(value) => value,
            Err(_) => {
                eprintln!("skipping live Restate E2E: RESTATE_INGRESS_URL is not set");
                return;
            }
        };
        let admin_url = std::env::var("RESTATE_ADMIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
        let endpoint_bind: SocketAddr = std::env::var("AGENT_WORKBENCH_E2E_ENDPOINT_BIND")
            .unwrap_or_else(|_| "127.0.0.1:19081".to_string())
            .parse()
            .expect("valid workbench E2E endpoint bind");
        let endpoint_url = std::env::var("AGENT_WORKBENCH_E2E_ENDPOINT_URL")
            .unwrap_or_else(|_| format!("http://{endpoint_bind}"));
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-restate-e2e-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let harness = live_workbench_restate_state(&data_dir, ingress_url).await;
        restate::spawn_restate_endpoint(
            endpoint_bind,
            harness.state.clone(),
            harness.process_deployment,
            harness.process_worker,
        );
        wait_for_endpoint_socket(endpoint_bind).await;
        register_restate_deployment(&admin_url, &endpoint_url).await;
        let turn_invocation_id = run_workbench_turn_via_restate(
            &harness.state,
            "Register a cron trigger that runs every two seconds and reports the tick.",
        )
        .await;
        wait_for_workbench_message(&harness.state, "cron registered", Duration::from_secs(60))
            .await;
        wait_for_restate_invocation_success(
            &harness.state,
            &turn_invocation_id,
            Duration::from_secs(30),
        )
        .await;
        wait_for_restate_cron_sync(&harness.state, &harness.trace_path, Duration::from_secs(30))
            .await;
        wait_for_workbench_message(
            &harness.state,
            "cron tick observed",
            Duration::from_secs(60),
        )
        .await;
        wait_for_trace_event_count(
            &harness.trace_path,
            "agent_workbench.cron.restate.run",
            1,
            Duration::from_secs(30),
        )
        .await;
        let trace_text =
            std::fs::read_to_string(&harness.trace_path).expect("read workbench trace jsonl");
        assert!(
            trace_text.contains("agent_workbench.cron.restate.sync_upserted"),
            "trace should include cron sync; trace at {}",
            harness.trace_path.display()
        );
        assert!(
            trace_text.contains("agent_workbench.cron.restate.run"),
            "trace should include cron run; trace at {}",
            harness.trace_path.display()
        );
        // Regression for the single-tick chain kill: the occurrence
        // idempotency key must be unique per tick and run() must re-arm, so a
        // SECOND tick has to fire. The original bug passed any single-tick
        // assertion and died silently on tick two.
        wait_for_trace_event_count(
            &harness.trace_path,
            "agent_workbench.cron.restate.run",
            2,
            Duration::from_secs(30),
        )
        .await;
        let _ = restate::cancel_cron_jobs_for_session(
            &harness.state,
            &harness.state.current_session_id(),
            "live_e2e_cleanup",
        )
        .await;
        assert_no_active_lash_restate_invocations(&harness.state, Duration::from_secs(10)).await;
        let _ = std::fs::remove_dir_all(data_dir);
    }

    async fn run_workbench_turn_via_restate(
        state: &AppState,
        text: &str,
    ) -> lash_restate::RestateInvocationId {
        state.push_message("user", text);
        let turn_id = format!("workbench-turn-{}", uuid::Uuid::new_v4());
        let request = restate::WorkbenchTurnWorkflowRequest {
            turn_id: turn_id.clone(),
            session_id: state.current_session_id(),
            text: text.to_string(),
            model: state.selected_model(),
            attachment_id: None,
        };
        let invocation_id = tokio::time::timeout(
            Duration::from_secs(60),
            restate::submit_user_turn(state, request),
        )
        .await
        .expect("Restate-backed workbench turn timed out")
        .expect("finish Restate-backed workbench turn");
        state.track_turn_prompt(&state.current_session_id(), &turn_id, text.to_string());
        invocation_id
    }

    async fn wait_for_restate_invocation_success(
        state: &AppState,
        invocation_id: &lash_restate::RestateInvocationId,
        timeout: Duration,
    ) {
        let admin = lash_restate::RestateAdminClient::new(
            lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
                state.restate_http.clone(),
            ),
        );
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match admin
                .invocation_status(invocation_id)
                .await
                .expect("query Restate invocation status")
            {
                Some(status) if status.completed_successfully() => return,
                Some(status) if status.status == "completed" => {
                    panic!(
                        "Restate invocation {invocation_id} completed unsuccessfully: {status:#?}"
                    )
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
        let admin = lash_restate::RestateAdminClient::new(
            lash_restate::RestateConnection::with_client(
            state.restate_admin_url.clone(),
                state.restate_http.clone(),
            ),
        );
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
        process_deployment: lash_restate::RestateProcessDeployment,
        trace_path: PathBuf,
    }
    async fn live_workbench_restate_state_with_provider(
        data_dir: &std::path::Path,
        restate_ingress_url: String,
        provider: ProviderHandle,
        session_ids: WorkbenchSessionIds,
        active_turns: ActiveTurns,
    ) -> LiveWorkbenchRestateHarness {
        live_workbench_restate_state_with_provider_and_database(
            data_dir,
            restate_ingress_url,
            provider,
            session_ids,
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
        session_ids: WorkbenchSessionIds,
        active_turns: ActiveTurns,
        database_url: Option<&str>,
        lease_timings: lash::durability::LeaseTimings,
    ) -> LiveWorkbenchRestateHarness {
        let stores = WorkbenchStores::open(data_dir, database_url)
            .await
            .expect("open live workbench stores");
        let core_store_factory = Arc::clone(&stores.session_store_factory);
        let process_registry = Arc::clone(&stores.process_registry);
        let process_continuations = Arc::clone(&stores.process_continuations);
        let trigger_store = Arc::clone(&stores.trigger_store);
        let process_env_store = Arc::clone(&stores.process_env_store);
        let artifact_store = Arc::clone(&stores.artifact_store);
        let trace_path = data_dir.join("trace.jsonl");
        let lashlang_execution_path = data_dir.join("lashlang-execution.jsonl");
        let trace_sink = Arc::new(JsonlTraceSink::new(trace_path.clone())) as Arc<dyn TraceSink>;
        let lashlang_execution = Arc::new(TraceLashlangGraphStore::default());
        let lashlang_execution_sink = Arc::new(TeeTraceSink::new([
            Arc::clone(&lashlang_execution) as Arc<dyn TraceSink>,
            Arc::new(JsonlTraceSink::new(lashlang_execution_path)) as Arc<dyn TraceSink>,
        ])) as Arc<dyn TraceSink>;
        let model =
            lash::ModelSpec::from_token_limits(
                "mock-model",
                lash::provider::ReasoningSelection::Effort("high".to_string()),
                4096,
                None,
            )
                .expect("model spec");
        let model = with_workbench_model_capability(model);
        let process_deployment = lash_restate::RestateProcessDeployment::new(
            restate_ingress_url.clone(),
            Arc::clone(&process_registry),
            process_continuations,
        );
        let restate_http = reqwest::Client::new();
        let turn_deployment = lash_restate::RestateTurnDeployment::new(
            lash_restate::RestateConnection::with_client(
                restate_ingress_url.clone(),
                restate_http.clone(),
            ),
        );
        let queued_work_driver =
            lash::runtime::QueuedWorkDriver::new(Arc::new(WorkbenchQueuedWorkSubmitter {
                session_ids: session_ids.clone(),
                store_factory: Arc::clone(&core_store_factory),
                restate_ingress_url: restate_ingress_url.clone(),
                restate_http: restate_http.clone(),
                active_turns: active_turns.clone(),
            }));
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::default()
                .with_lashlang_abilities(workbench_lashlang_abilities()),
            artifact_store,
        )
        .with_lashlang_execution_sink(lashlang_execution_sink);
        let core = LashCore::rlm_builder(factory)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .process_env_store(process_env_store)
            .trigger_store(trigger_store)
            .trace_sink(Arc::clone(&trace_sink))
            .trace_level(TraceLevel::Extended)
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .plugin(Arc::new(
                lash_llm_tools::LlmToolsPluginFactory::default(),
            ))
            .effect_host(turn_deployment.effect_host())
            .process_work_driver(process_deployment.process_work_driver())
            .queued_work_driver(queued_work_driver.clone())
            .lease_timings(lease_timings)
            .build()
            .expect("build core");
        let process_worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("build process worker config"),
        );
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let event_tx =
            SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)
                .expect("open durable product events");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: process_deployment.process_work_driver(),
            session_ids,
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "mock-model".to_string(),
                model_variant: Some("high".to_string()),
            })),
            web_configured: false,
            trace_sink: Some(trace_sink),
            lashlang_execution,
            event_tx,
            queued_work_driver: queued_work_driver.clone(),
            restate_ingress_url,
            restate_admin_url: std::env::var("RESTATE_ADMIN_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:19071".to_string()),
            restate_http,
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns,
            authorization: WorkbenchAuthorization::allow_all(),
        };
        LiveWorkbenchRestateHarness {
            state,
            process_worker,
            process_deployment,
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
            let known_jobs = state
                .restate_cron_job_keys
                .lock()
                .expect("cron job key lock")
                .clone();
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

    async fn register_restate_deployment(admin_url: &str, endpoint_url: &str) {
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .expect("build Restate admin client");
        let response = client
            .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
            .json(&json!({
                "uri": endpoint_url,
                "force": true,
                "breaking": true,
            }))
            .send()
            .await
            .expect("register deployment with Restate admin API");
        assert!(
            response.status().is_success(),
            "Restate deployment registration failed: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
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
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let process_registry_path = data_dir.join("processes.db");
        let trigger_store_path = data_dir.join("triggers.db");
        let artifact_store_path = data_dir.join("artifacts.db");
        let process_env_store_path = data_dir.join("process-env.db");
        let session_id = WorkbenchSessionIds::fresh().current();

        {
            let artifact_store = Arc::new(
                lash_sqlite_store::Store::open(&artifact_store_path)
                    .await
                    .expect("open artifacts"),
            );
            let artifact_store_for_core: Arc<dyn lashlang::LashlangArtifactStore> =
                artifact_store.clone();
            let process_registry = Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::open(&process_registry_path, process_registry_path.with_extension("sessions"))
                    .await
                    .expect("open registry"),
            ) as Arc<dyn lash::process::ProcessRegistry>;
            let trigger_store = Arc::new(
                lash_sqlite_store::SqliteTriggerStore::open(&trigger_store_path)
                    .await
                    .expect("open trigger store"),
            );
            let process_env_store = Arc::new(
                lash_sqlite_store::Store::open(&process_env_store_path)
                    .await
                    .expect("open process env store"),
            );
            let core = test_workbench_core(
                session_store_factory.clone(),
                process_registry,
                trigger_store,
                artifact_store_for_core,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    data_dir.join("attachments"),
                )),
                process_env_store,
            );
            let session = core
                .session(session_id.clone())
                .open()
                .await
                .expect("open session");
            register_test_trigger(&session).await;
            drop(session);
            drop(core);
        }

        let artifact_store = Arc::new(
            lash_sqlite_store::Store::open(&artifact_store_path)
                .await
                .expect("reopen artifacts"),
        );
        let artifact_store_for_core: Arc<dyn lashlang::LashlangArtifactStore> =
            artifact_store.clone();
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&process_registry_path, process_registry_path.with_extension("sessions"))
                .await
                .expect("reopen registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let trigger_store = Arc::new(
            lash_sqlite_store::SqliteTriggerStore::open(&trigger_store_path)
                .await
                .expect("reopen trigger store"),
        );
        let process_env_store = Arc::new(
            lash_sqlite_store::Store::open(&process_env_store_path)
                .await
                .expect("reopen process env store"),
        );
        let core = test_workbench_core(
            session_store_factory,
            Arc::clone(&process_registry),
            trigger_store,
            artifact_store_for_core,
            Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )),
            process_env_store,
        );
        let _reopened = core
            .session(session_id)
            .open()
            .await
            .expect("reopen session");
        let report = emit_test_button_trigger(&core, ButtonChoice::Blue).await;
        assert_eq!(report.started_process_ids().len(), 1);
        lash::process::ProcessAwaiter::polling(Arc::clone(&process_registry))
            .await_terminal(&report.started_process_ids()[0])
            .await
            .expect("trigger process should finish");

        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// Stand-in for the Restate queued-turn workflow: drains an enqueued
    /// command batch in-process. The test cores' effect host permits
    /// foreground drains; in production the same drain runs inside a Restate
    /// handler context (`restate::run_queued_turn`).
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

    include!("tests/session_isolation.rs");
    include!("tests/queued_work.rs");
    fn test_workbench_core(
        session_store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
        process_registry: Arc<dyn lash::process::ProcessRegistry>,
        trigger_store: Arc<lash_sqlite_store::SqliteTriggerStore>,
        artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
        attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
        process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
    ) -> LashCore {
        let provider = trigger_registration_provider();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::default()
                .with_lashlang_abilities(workbench_lashlang_abilities()),
            artifact_store,
        );
        LashCore::rlm_builder(factory)
            .provider(provider)
            .model(model)
            .store_factory(session_store_factory)
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .process_registry(process_registry)
            .trigger_store(trigger_store)
            .advanced()
            .runtime_host_config(lash::durability::RuntimeHostConfig::new(
                Arc::new(lash::durability::InlineEffectHost::default()),
                attachment_store,
                process_env_store,
            ))
            .build()
            .expect("build core")
    }

    pub(super) fn text_response(text: &str) -> lash::provider::LlmResponse {
        lash::provider::LlmResponse {
            full_text: text.to_string(),
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
            "<lashlang>\n{}\n</lashlang>",
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

    async fn assert_remote_trigger_subscription_records_round_trip(
        data_dir: &std::path::Path,
        session_id: &str,
    ) -> Vec<lash::triggers::TriggerSubscriptionRecord> {
        let store = lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
            .await
            .expect("open trigger store for remote DTO round trip");
        let filter = lash::triggers::TriggerSubscriptionFilter::for_session(session_id);
        let remote_filter =
            lash_remote_protocol::RemoteTriggerSubscriptionFilter::from(filter.clone());
        remote_filter
            .validate()
            .expect("remote trigger subscription filter should validate");
        let round_trip_filter: lash::triggers::TriggerSubscriptionFilter = remote_filter
            .try_into()
            .expect("remote trigger subscription filter should convert back");
        assert_eq!(round_trip_filter, filter);

        let records = lash::triggers::TriggerStore::list_subscriptions(&store, filter)
            .await
            .expect("list persisted trigger subscriptions for remote DTO round trip");
        let remote_list =
            lash_remote_protocol::RemoteTriggerListSubscriptionsResponse::try_from(records.clone())
                .expect("remote trigger subscription list");
        remote_list
            .validate()
            .expect("remote trigger subscription list should validate");
        let round_trip_records: Vec<lash::triggers::TriggerSubscriptionRecord> = remote_list
            .try_into()
            .expect("remote trigger subscription list should convert back");
        assert_eq!(round_trip_records, records);

        for record in &records {
            let remote_record =
                lash_remote_protocol::RemoteTriggerSubscriptionRecord::try_from(record.clone())
                    .expect("remote trigger subscription record");
            remote_record
                .validate("WorkbenchTriggerSubscription")
                .expect("remote trigger subscription record should validate");
            let round_trip_record: lash::triggers::TriggerSubscriptionRecord = remote_record
                .try_into()
                .expect("remote trigger subscription record should convert back");
            assert_eq!(&round_trip_record, record);

            let remote_result =
                lash_remote_protocol::RemoteTriggerRegisterSubscriptionResult::try_from(
                    record.clone(),
                )
                .expect("remote trigger register result");
            remote_result
                .validate()
                .expect("remote trigger register result should validate");
            let round_trip_result: lash::triggers::TriggerSubscriptionRecord = remote_result
                .try_into()
                .expect("remote trigger register result should convert back");
            assert_eq!(&round_trip_result, record);
        }

        records
    }

    fn assert_remote_trigger_emit_report_round_trip(report: &lash::triggers::TriggerEmitReport) {
        let remote = lash_remote_protocol::RemoteTriggerEmitReport::from(report.clone());
        remote
            .validate()
            .expect("remote trigger emit report should validate");
        let round_trip: lash::triggers::TriggerEmitReport = remote
            .try_into()
            .expect("remote trigger emit report should convert back");
        assert_eq!(&round_trip, report);
    }

    async fn assert_remote_started_process_surface(
        core: &LashCore,
        registry: &dyn lash::process::ProcessRegistry,
        session_id: &str,
        process_ids: &[String],
    ) {
        let filter = lash::process::ProcessListFilter {
            definition: None,
            status: lash::process::ProcessStatusFilter::Any,
            waiting: None,
            ..Default::default()
        };
        let observed = core
            .processes()
            .list(&filter)
            .await
            .expect("list observed processes for remote DTO round trip");
        let remote_list =
            lash_remote_protocol::RemoteProcessListResponse::try_from(observed.clone())
                .expect("observed process list should convert to remote DTO");
        remote_list
            .validate()
            .expect("remote process list should validate");
        let round_trip_observed: Vec<lash::process::ObservedProcess> = remote_list
            .try_into()
            .expect("remote process list should convert back");
        for process_id in process_ids {
            assert!(
                round_trip_observed
                    .iter()
                    .any(|process| process.process_id == *process_id),
                "remote process list did not include started process {process_id}"
            );
        }

        let snapshot = core
            .processes()
            .session_snapshot(session_id)
            .await
            .expect("capture process work snapshot for remote DTO round trip");
        let remote_snapshot = lash_remote_protocol::RemoteProcessWorkSnapshot::try_from(snapshot)
            .expect("process work snapshot should convert to remote DTO");
        remote_snapshot
            .validate()
            .expect("remote process work snapshot should validate");
        let round_trip_snapshot: lash::process::ProcessWorkSnapshot = remote_snapshot
            .try_into()
            .expect("remote process work snapshot should convert back");
        assert_eq!(round_trip_snapshot.session_id, session_id);

        for process_id in process_ids {
            let record = registry
                .get_process(process_id)
                .await
                .expect("process read should succeed")
                .expect("started process record should exist");
            let remote_record = lash_remote_protocol::RemoteProcessRecord::try_from(record)
                .expect("started process record should convert to remote DTO");
            remote_record
                .validate("WorkbenchStartedProcessRecord")
                .expect("remote started process record should validate");
            let round_trip_record: lash::process::ProcessRecord = remote_record
                .try_into()
                .expect("remote started process record should convert back");
            assert_eq!(&round_trip_record.id, process_id);

            let events = registry
                .recent_events(process_id, 32)
                .await
                .expect("load started process event tail for remote DTO round trip");
            let expected_tail = events
                .iter()
                .map(|event| (event.sequence, event.event_type.clone()))
                .collect::<Vec<_>>();
            let remote_events = lash_remote_protocol::RemoteProcessEventsResponse::from((
                process_id.clone(),
                events,
            ));
            remote_events
                .validate()
                .expect("remote started process event tail should validate");
            let (round_trip_process_id, round_trip_events): (
                String,
                Vec<lash::process::ProcessEvent>,
            ) = remote_events
                .try_into()
                .expect("remote started process event tail should convert back");
            let round_trip_tail = round_trip_events
                .iter()
                .map(|event| (event.sequence, event.event_type.clone()))
                .collect::<Vec<_>>();
            assert_eq!(round_trip_process_id, *process_id);
            assert_eq!(round_trip_tail, expected_tail);
        }
    }

    async fn emit_test_button_trigger(
        core: &LashCore,
        button: ButtonChoice,
    ) -> lash::triggers::TriggerEmitReport {
        emit_test_button_trigger_with_scope(core, button, None).await
    }

    async fn emit_test_button_trigger_for_session(
        core: &LashCore,
        button: ButtonChoice,
        session_id: &str,
    ) -> lash::triggers::TriggerEmitReport {
        emit_test_button_trigger_with_scope(core, button, Some(session_id)).await
    }

    async fn emit_test_button_trigger_with_scope(
        core: &LashCore,
        button: ButtonChoice,
        session_id: Option<&str>,
    ) -> lash::triggers::TriggerEmitReport {
        let source_key = lash::triggers::empty_trigger_source_key(BUTTON_TRIGGER_SOURCE_TYPE)
            .expect("source key");
        let idempotency_key = format!(
            "workbench-test-button-trigger:{}:{}",
            button.as_str(),
            uuid::Uuid::new_v4()
        );
        let scoped_effect_controller = lash::runtime::ScopedEffectController::shared(
            Arc::new(lash::runtime::InlineRuntimeEffectController::default()),
            lash::runtime::ExecutionScope::runtime_operation(format!("trigger:{idempotency_key}")),
        )
        .expect("inline trigger occurrence execution scope");
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
            .emit(
                request,
                scoped_effect_controller,
            )
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
        process remember(event: ui.button.Pressed) {
          wake { kind: "button_pressed", button: event.button, message: event.message }
          finish { button: event.button, ok: true }
        }

        handle = await triggers.register({
          source: ui.button.pressed({}),
          target: remember,
          inputs: { event: trigger.event },
          name: "remembered"
        })?
        finish "registered"
        "#
    }

    fn test_cron_trigger_source() -> &'static str {
        r#"
        process remember_tick(tick: cron.Tick) {
          wake { kind: "cron_tick", fired_at: tick.fired_at }
          finish { fired_at: tick.fired_at }
        }

        handle = await triggers.register({
          source: cron.Schedule({ expr: "*/2 * * * * *", tz: "UTC" }),
          target: remember_tick,
          inputs: { tick: trigger.event },
          name: "cron smoke"
        })?
        finish "cron registered"
        "#
    }

    include!("tests/attachments_usage.rs");
    include!("tests/turn_input_application.rs");
    include!("tests/tool_control.rs");
    include!("tests/concurrent_send.rs");
}
