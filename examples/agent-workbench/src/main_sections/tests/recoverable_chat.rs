async fn recoverable_chat_test_state(
    data_dir: &std::path::Path,
    channel_capacity: usize,
) -> AppState {
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-workbench-test")
        .complete(|_| async {
            Ok(text_response(
                "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
            ))
        })
        .build()
        .into_handle();
    recoverable_chat_test_state_with_provider(data_dir, channel_capacity, provider).await
}

async fn recoverable_chat_test_state_with_provider(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
) -> AppState {
    recoverable_chat_test_state_with_provider_and_trigger_store(
        data_dir,
        channel_capacity,
        provider,
        in_memory_trigger_store(),
    )
    .await
}

pub(crate) async fn recoverable_chat_test_state_with_trigger_store(
    data_dir: &std::path::Path,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
) -> AppState {
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-trigger-store-test")
        .complete(|_| async {
            Ok(text_response(
                "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
            ))
        })
        .build()
        .into_handle();
    recoverable_chat_test_state_with_provider_and_trigger_store(
        data_dir,
        16,
        provider,
        trigger_store,
    )
    .await
}

async fn recoverable_chat_test_state_with_provider_and_trigger_store(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
) -> AppState {
    let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    recoverable_chat_test_state_with_dependencies(
        data_dir,
        channel_capacity,
        provider,
        trigger_store,
        store_factory,
        None,
    )
    .await
}

async fn recoverable_chat_test_state_with_dependencies(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    queued_work_driver: Option<lash::runtime::QueuedWorkDriver>,
) -> AppState {
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &data_dir.join("processes.db"),
            data_dir.join("lash-sessions"),
        )
        .await
        .expect("open process registry"),
    ) as Arc<dyn lash::process::ProcessRegistry>;
    let model = with_workbench_model_capability(
        lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None)
            .expect("model spec"),
    );
    let mut core_builder = explicit_durable_test_facets(data_dir)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .process_registry(Arc::clone(&process_registry))
        .trigger_store(Arc::clone(&trigger_store));
    if let Some(queued_work_driver) = queued_work_driver {
        core_builder = core_builder.queued_work_driver(queued_work_driver);
    }
    let core = core_builder
        .build()
        .expect("build test core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    AppState {
        core,
        attachment_store: test_attachment_store(),
        trigger_store,
        process_observer,
        process_work_driver: inert_process_work_driver(process_registry),
        session_ids: WorkbenchSessionIds::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        web_configured: false,
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(channel_capacity),
        queued_work_driver: inert_queued_work_driver(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
    }
}

struct RetiringSubscriptionListTriggerStore {
    inner: lash::triggers::InMemoryTriggerStore,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    session_to_retire: Mutex<Option<String>>,
}

impl RetiringSubscriptionListTriggerStore {
    fn new(store_factory: Arc<dyn lash::persistence::SessionStoreFactory>) -> Self {
        Self {
            inner: lash::triggers::InMemoryTriggerStore::new(),
            store_factory,
            session_to_retire: Mutex::new(None),
        }
    }

    fn retire_on_next_list(&self, session_id: &str) {
        *self
            .session_to_retire
            .lock()
            .expect("retiring trigger store session lock") = Some(session_id.to_string());
    }
}

#[async_trait::async_trait]
impl lash::triggers::TriggerStore for RetiringSubscriptionListTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash::triggers::TriggerCommand,
    ) -> std::result::Result<
        lash::triggers::TriggerEffectResult,
        lash::plugins::PluginError,
    > {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: lash::triggers::TriggerSubscriptionFilter,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerSubscriptionRecord>,
        lash::plugins::PluginError,
    > {
        let session_id = self
            .session_to_retire
            .lock()
            .expect("retiring trigger store session lock")
            .take();
        if let Some(session_id) = session_id {
            self.store_factory
                .delete_session(&session_id)
                .await
                .map_err(lash::plugins::PluginError::Session)?;
        }
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &str,
    ) -> std::result::Result<usize, lash::plugins::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: lash::triggers::TriggerOccurrenceRequest,
    ) -> std::result::Result<lash::triggers::TriggerIngressResult, lash::plugins::PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: lash::triggers::TriggerOccurrenceFilter,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerOccurrenceRecord>,
        lash::plugins::PluginError,
    > {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerDeliveryReservation>,
        lash::plugins::PluginError,
    > {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerDeliveryReservation>,
        lash::plugins::PluginError,
    > {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerDeliveryReservation>,
        lash::plugins::PluginError,
    > {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerDeliveryReservation>,
        lash::plugins::PluginError,
    > {
        self.inner.list_deliveries().await
    }

    async fn list_delivery_process_ids(
        &self,
    ) -> std::result::Result<Vec<String>, lash::plugins::PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> std::result::Result<
        Vec<lash::triggers::TriggerDeliveryRetentionCandidate>,
        lash::plugins::PluginError,
    > {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash::triggers::TriggerDeliveryRetentionCandidate],
    ) -> std::result::Result<usize, lash::plugins::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash::plugins::PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }
}

struct RetiringQueuedWorkRunHandle {
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    session_to_retire: Mutex<Option<String>>,
}

impl RetiringQueuedWorkRunHandle {
    fn new(store_factory: Arc<dyn lash::persistence::SessionStoreFactory>) -> Self {
        Self {
            store_factory,
            session_to_retire: Mutex::new(None),
        }
    }

    fn retire_on_next_run(&self, session_id: &str) {
        *self
            .session_to_retire
            .lock()
            .expect("retiring queued-work session lock") = Some(session_id.to_string());
    }
}

#[async_trait::async_trait]
impl lash::runtime::QueuedWorkRunHandle for RetiringQueuedWorkRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash::runtime::QueuedWorkRunRequest,
    ) -> std::result::Result<(), lash::runtime::QueuedWorkRunError> {
        let session_id = self
            .session_to_retire
            .lock()
            .expect("retiring queued-work session lock")
            .take();
        if let Some(session_id) = session_id {
            self.store_factory
                .delete_session(&session_id)
                .await
                .map_err(|error| {
                    lash::runtime::QueuedWorkRunError::terminal(
                        lash::plugins::PluginError::Session(error),
                    )
                })?;
        }
        Ok(())
    }
}

async fn retire_workbench_session(state: &AppState, session_id: &str) {
    drop(
        state
            .core
            .session(session_id)
            .open()
            .await
            .expect("open session before retirement"),
    );
    let scope = state
        .core
        .session_delete_scope(session_id)
        .await
        .expect("build session delete scope");
    let effect_host = state.core.effect_host();
    let scoped = effect_host
        .scoped(scope)
        .expect("scope inline session deletion");
    state
        .core
        .delete_session(session_id, scoped)
        .await
        .expect("retire session");
}

fn assert_deleted_session_conflict(error: &AppError, session_id: &str) {
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.message, deleted_session_message(session_id));
    assert!(error.terminal);
    assert!(!error.retryable);
}

#[test]
fn reset_cron_cancellation_preserves_a_retired_session_refusal() {
    run_async_test_on_stack_budget("retired-session-reset-cron-cancel-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let error = crate::restate::cancel_cron_jobs_for_session(
            &state,
            &session_id,
            "reset",
        )
        .await
        .expect_err("reset cron cancellation must refuse a retired session");

        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn reset_cron_close_preserves_a_concurrent_retirement_refusal() {
    run_async_test_on_stack_budget("retired-session-reset-cron-close-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.path().join("lash-sessions")),
        );
        let trigger_store = Arc::new(RetiringSubscriptionListTriggerStore::new(Arc::clone(
            &store_factory,
        )));
        let provider = lash::testing::TestProvider::builder()
            .kind("retired-session-reset-cron-close-test")
            .complete(|_| async {
                Ok(text_response(
                    "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
                ))
            })
            .build()
            .into_handle();
        let state = recoverable_chat_test_state_with_dependencies(
            data_dir.path(),
            16,
            provider,
            trigger_store.clone(),
            store_factory,
            None,
        )
        .await;
        let session_id = state.current_session_id();
        trigger_store.retire_on_next_list(&session_id);

        let error = crate::restate::cancel_cron_jobs_for_session(
            &state,
            &session_id,
            "reset-close-race",
        )
        .await
        .expect_err("cron cancellation close must preserve a concurrent retirement");

        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn tool_catalog_refresh_close_preserves_a_concurrent_retirement_refusal() {
    run_async_test_on_stack_budget("retired-session-tool-refresh-close-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.path().join("lash-sessions")),
        );
        let retiring_run_handle =
            Arc::new(RetiringQueuedWorkRunHandle::new(Arc::clone(&store_factory)));
        let queued_work_driver =
            lash::runtime::QueuedWorkDriver::new(retiring_run_handle.clone());
        let provider = lash::testing::TestProvider::builder()
            .kind("retired-session-tool-refresh-close-test")
            .complete(|_| async {
                Ok(text_response(
                    "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
                ))
            })
            .build()
            .into_handle();
        let state = recoverable_chat_test_state_with_dependencies(
            data_dir.path(),
            16,
            provider,
            in_memory_trigger_store(),
            store_factory,
            Some(queued_work_driver),
        )
        .await;
        let session_id = state.current_session_id();
        retiring_run_handle.retire_on_next_run(&session_id);

        let error = enqueue_tool_catalog_refresh(&state, "close_retirement_race")
            .await
            .expect_err("tool-catalog refresh close must preserve a concurrent retirement");

        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn retired_session_admission_precedes_attachment_reads_and_submission() {
    run_async_test_on_stack_budget("retired-session-send-turn-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        state.restate_ingress_url = restate_ingress_url;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let error = send_turn(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
            Json(TurnRequest {
                text: "must not be accepted".to_string(),
                model: Some("test-model".to_string()),
                model_variant: None,
                attachment_id: Some("missing-retired-attachment".to_string()),
            }),
        )
        .await
        .expect_err("retired session turn must be refused");

        assert_deleted_session_conflict(&error, &session_id);
        assert!(
            matches!(
                restate_requests.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "retired session refusal must not submit a Restate workflow"
        );
        assert!(state.messages_snapshot().is_empty());
        assert!(state.active_turns.for_session(&session_id).is_empty());
    });
}

#[test]
fn observing_a_retired_session_returns_the_typed_conflict() {
    run_async_test_on_stack_budget("retired-session-observations-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let error = session_observations(
            State(state),
            Query(EventsQuery {
                cursor: None,
                session_id: Some(session_id.clone()),
            }),
        )
        .await
        .expect_err("retired session observations must be refused");

        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn enqueuing_turn_input_to_a_retired_session_returns_the_typed_conflict() {
    run_async_test_on_stack_budget("retired-session-turn-input-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let error = enqueue_turn_input(
            State(state),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
            Json(TurnInputRequest {
                text: "must not be queued".to_string(),
                ingress: TurnInputIngressRequest::NextTurn,
            }),
        )
        .await
        .expect_err("retired session turn input must be refused");

        assert_deleted_session_conflict(&error, &session_id);
    });
}

#[test]
fn retired_session_cancel_and_tool_refresh_return_the_typed_conflict() {
    run_async_test_on_stack_budget("retired-session-secondary-surfaces-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let cancel_error = state
            .cancel_turns_for_session(&session_id)
            .await
            .expect_err("retired session cancellation must be refused");
        assert_deleted_session_conflict(&cancel_error, &session_id);

        let refresh_error = enqueue_tool_catalog_refresh(&state, "retired_session_test")
            .await
            .expect_err("retired session tool refresh must be refused");
        assert_deleted_session_conflict(&refresh_error, &session_id);
    });
}

#[test]
fn retired_session_http_refusals_record_structured_admission_evidence() {
    run_async_test_on_stack_budget("retired-session-refusal-evidence-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let trace_path = data_dir.path().join("refusals.jsonl");
        let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
        state.trace_sink = Some(Arc::new(JsonlTraceSink::new(trace_path.clone())));
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        let state_error = app_state(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
        )
        .await
        .expect_err("retired session state read must be refused");
        assert_deleted_session_conflict(&state_error, &session_id);

        let observation_error = session_observations(
            State(state.clone()),
            Query(EventsQuery {
                cursor: None,
                session_id: Some(session_id.clone()),
            }),
        )
        .await
        .expect_err("retired session observation must be refused");
        assert_deleted_session_conflict(&observation_error, &session_id);

        let turn_error = send_turn(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
            Json(TurnRequest {
                text: "must not be accepted".to_string(),
                model: Some("test-model".to_string()),
                model_variant: None,
                attachment_id: None,
            }),
        )
        .await
        .expect_err("retired session turn must be refused");
        assert_deleted_session_conflict(&turn_error, &session_id);

        let input_error = enqueue_turn_input(
            State(state),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
            Json(TurnInputRequest {
                text: "must not be queued".to_string(),
                ingress: TurnInputIngressRequest::NextTurn,
            }),
        )
        .await
        .expect_err("retired session turn input must be refused");
        assert_deleted_session_conflict(&input_error, &session_id);

        let records = std::fs::read_to_string(&trace_path).expect("read refusal trace");
        assert!(
            !records.contains("agent_workbench.api.turn.request"),
            "a refused turn must not be traced as accepted"
        );
        let surfaces = records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("decode refusal trace record"))
            .filter(|record| {
                record.get("name").and_then(Value::as_str)
                    == Some("agent_workbench.session.admission_refused")
            })
            .map(|record| {
                assert_eq!(
                    record.pointer("/payload/session_id").and_then(Value::as_str),
                    Some(session_id.as_str())
                );
                assert_eq!(
                    record
                        .pointer("/payload/consulted_state/kind")
                        .and_then(Value::as_str),
                    Some("session_store_tombstone")
                );
                assert_eq!(
                    record
                        .pointer("/payload/consulted_state/freshness")
                        .and_then(Value::as_str),
                    Some("admission_read")
                );
                assert_eq!(
                    record
                        .pointer("/payload/tombstone_outcome")
                        .and_then(Value::as_str),
                    Some("retired")
                );
                assert_eq!(
                    record.pointer("/payload/outcome").and_then(Value::as_str),
                    Some("refused")
                );
                record
                    .pointer("/payload/surface")
                    .and_then(Value::as_str)
                    .expect("refusal surface")
                    .to_string()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            surfaces,
            ["api.observations", "api.state", "api.turn", "api.turn.input"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
    });
}

#[test]
fn every_terminalize_branch_makes_runtime_shaped_session_deletion_terminal() {
    run_async_test_on_stack_budget("retired-session-settlement-callers-test", || async {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let state = recoverable_chat_test_state(data_dir.path(), 16).await;
        let session_id = state.current_session_id();
        retire_workbench_session(&state, &session_id).await;

        type TerminalizeResult =
            Result<Result<(), AppError>, Box<dyn std::any::Any + Send>>;
        let cases: Vec<(&str, TerminalizeResult)> = vec![
            ("successful_turn", Ok(Ok(()))),
            (
                "failed_turn",
                Ok(Err(AppError::internal("original turn failure"))),
            ),
            ("panicked_turn", Err(Box::new("original turn panic"))),
        ];

        for (case, result) in cases {
            let turn_id = format!("{case}-turn");
            state.track_turn(&session_id, &turn_id);
            let error = crate::restate::terminalize_turn_execution(
                &state,
                &session_id,
                &turn_id,
                "test.turn.failed",
                result,
            )
            .await
            .expect_err("settlement against a retired session must fail");
            let rendered =
                <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
                    .to_string();
            assert!(
                rendered.starts_with("Terminal error"),
                "{case} must terminalize the runtime-shaped SessionDeleted, got {rendered}"
            );
        }
    });
}

#[test]
fn workbench_browser_recovery_projection_preserves_rows_and_scopes_session_cursors() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/browser_projection.mjs");
    let trigger_identities = serde_json::json!({
        "session_a": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("session-a"),
            "derived/v2/content-address",
        ),
        "session_b": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("session-b"),
            "derived/v2/content-address",
        ),
        "wired": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("wired-session"),
            "wired-key",
        ),
    });
    let output = std::process::Command::new("node")
        .arg("--test")
        .arg(&script)
        .env(
            "LASH_WORKBENCH_TRIGGER_IDENTITIES",
            trigger_identities.to_string(),
        )
        .output()
        .expect("Node.js is required for the agent-workbench browser projection gate");
    assert!(
        output.status.success(),
        "browser projection gate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_event_registry_isolates_channels_and_recreates_after_removal() {
    let registry = SessionEventRegistry::new(4);
    let mut session_a = registry.subscribe("session-a");
    let mut session_b = registry.subscribe("session-b");

    registry.publish(
        "session-a",
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        session_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_b.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    registry.remove("session-a");
    assert!(!registry.contains("session-a"));
    let mut replacement_a = registry.subscribe("session-a");
    registry.publish(
        "session-a",
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        replacement_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_a.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
}

#[test]
fn settled_product_reconciliation_keeps_the_cursor_monotonic() {
    let registry = SessionEventRegistry::new(4);
    let session_id = "reconciled-session";
    let committed_id = workbench_turn_user_message_id("reconciled-turn");
    registry.publish_identified(
        session_id,
        "provisional-message",
        StreamItem::Message {
            message: ChatMessage {
                id: committed_id.to_string(),
                role: "user".to_string(),
                text: "settled prompt".to_string(),
                at: String::new(),
            },
        },
    );
    registry.publish_identified(
        session_id,
        "turn-done",
        StreamItem::Done {
            turn_id: Some("reconciled-turn".to_string()),
            outcome: TurnDoneOutcome::Completed,
        },
    );

    registry.reconcile_settled(
        session_id,
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    let reconciled = registry.snapshot(session_id);
    assert_eq!(reconciled.cursor, 2);
    assert!(reconciled.events.is_empty());
    assert!(
        !registry.publish_identified(
            session_id,
            "turn-done",
            StreamItem::Done {
                turn_id: Some("reconciled-turn".to_string()),
                outcome: TurnDoneOutcome::Completed,
            },
        ),
        "compaction must retain event identity for idempotent workflow replay"
    );
    assert_eq!(registry.snapshot(session_id).cursor, 2);

    registry.publish_identified(
        session_id,
        "host-only-event",
        StreamItem::Message {
            message: ChatMessage {
                id: "host-only".to_string(),
                role: "event".to_string(),
                text: "host event".to_string(),
                at: String::new(),
            },
        },
    );
    assert_eq!(
        registry.snapshot(session_id).events[0].sequence,
        3,
        "compaction must not reuse a cursor already observed by a client"
    );
}

#[tokio::test]
async fn product_event_route_lag_emits_durable_ordered_resync() {
    let data_dir = tempfile::tempdir().expect("workbench lag tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 1).await;
    let session_id = state.current_session_id();
    let response = session_events(
        State(state.clone()),
        Query(ProductEventsQuery {
            session_id: None,
            cursor: Some(0),
        }),
    )
    .await
    .expect("open production product-event route");
    let mut body = response.into_body().into_data_stream();

    for sequence in 1..=3 {
        state.event_tx.publish_identified(
            &session_id,
            format!("event-{sequence}"),
            StreamItem::Message {
                message: ChatMessage {
                    id: format!("message-{sequence}"),
                    role: "event".to_string(),
                    text: format!("event {sequence}"),
                    at: String::new(),
                },
            },
        );
    }

    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let bytes = body
                .next()
                .await
                .expect("product route remains open")
                .expect("product route bytes");
            let item: Value = serde_json::from_slice(&bytes).expect("product stream item");
            if item.get("type").and_then(Value::as_str) == Some("resync") {
                break serde_json::from_value::<ProductEventSnapshot>(
                    item.get("snapshot").cloned().expect("resync snapshot"),
                )
                .expect("decode resync snapshot");
            }
        }
    })
    .await
    .expect("lagged route never emitted a resync");

    assert_eq!(snapshot.cursor, 3);
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| (event.sequence, event.event_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "event-1"), (2, "event-2"), (3, "event-3")]
    );
}

#[tokio::test]
async fn workbench_state_snapshot_merges_canonical_history_with_partial_product_log() {
    let data_dir = tempfile::tempdir().expect("workbench state merge tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open canonical session");
    session
        .admin()
        .state()
        .append_messages(vec![
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::User,
                "canonical question",
            )
            .with_id("canonical-user"),
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "canonical answer",
            )
            .with_id("canonical-assistant"),
        ])
        .await
        .expect("append canonical history");
    session.close().await.expect("close canonical session");

    state.push_message_with_id_for_session(
        &session_id,
        "canonical-assistant",
        "assistant",
        "stale mirrored answer",
    );
    state.push_message_with_id_for_session(
        &session_id,
        "host-only-event",
        "event",
        "host-only row",
    );

    let Json(snapshot) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize merged state");
    assert_eq!(
        snapshot
            .messages
            .iter()
            .map(|message| (message.id.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("canonical-user", "canonical question"),
            ("canonical-assistant", "canonical answer"),
            ("host-only-event", "host-only row"),
        ],
        "the Lash read view remains authoritative and product-only rows supplement it"
    );
}

#[tokio::test]
async fn one_send_renders_one_user_row_while_running_and_after_the_ui_row_is_reconciled() {
    let data_dir = tempfile::tempdir().expect("single user row tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let turn_id = "workbench-turn-fig972";

    // What `send_turn` publishes: the workbench's own optimistic row for a turn
    // it just submitted, in the workbench's id namespace.
    state.track_turn_prompt(&session_id, turn_id, "one send".to_string());
    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(turn_id),
        "user",
        "one send",
    );
    // What the runtime commits for the same send: a runtime-minted id — here the
    // queued-ingress spelling, which no host can predict — carrying the turn
    // provenance the runtime stamps.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session for the committed turn input");
    session
        .admin()
        .state()
        .append_messages(vec![
            lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, "one send")
                .with_id("m_ingress_workbench-input-1")
                .with_origin(lash::messages::MessageOrigin::TurnInput {
                    turn_id: turn_id.to_string(),
                    input_id: Some("workbench-input-1".to_string()),
                }),
        ])
        .await
        .expect("commit the runtime's copy of the turn input");
    session.close().await.expect("close session");
    // The workbench also mirrors committed ingress messages into its product log
    // so the live page replaces ingress receipts; that mirror must not become a
    // second row either.
    state.push_message_with_id_for_session(
        &session_id,
        "m_ingress_workbench-input-1",
        "user",
        "one send",
    );

    let ui_row = (
        workbench_turn_user_message_id(turn_id),
        "one send".to_string(),
    );
    let committed_row = (
        "m_ingress_workbench-input-1".to_string(),
        "one send".to_string(),
    );

    let Json(running) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("materialize the running snapshot");
    assert_eq!(
        user_rows(&running),
        vec![ui_row.clone()],
        "the UI-owned row renders once and the committed copy stays provenance"
    );
    assert_eq!(
        transcript_user_rows(&running),
        vec![ui_row],
        "the transcript projection suppresses the same committed copy"
    );

    // Backfill: once the turn settles the UI-owned row is reconciled away, and
    // the committed copy is what a reload renders — still exactly one row.
    crate::restate::settle_workbench_turn(&state, &session_id, turn_id)
        .await
        .expect("settle the turn");
    let Json(settled) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize the settled snapshot");
    assert_eq!(
        user_rows(&settled),
        vec![committed_row.clone()],
        "with no UI-owned row left, the committed copy backfills the send"
    );
    assert_eq!(
        transcript_user_rows(&settled),
        vec![committed_row],
        "the transcript backfills from the same committed copy"
    );
}

fn user_rows(snapshot: &StateReadSnapshot) -> Vec<(String, String)> {
    snapshot
        .state
        .messages
        .iter()
        .filter(|message| message.role == "user")
        .map(|message| (message.id.clone(), message.text.clone()))
        .collect()
}

fn transcript_user_rows(snapshot: &StateReadSnapshot) -> Vec<(String, String)> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::Message { message } if message.role == "user" => {
                Some((message.id.clone(), message.text.clone()))
            }
            TranscriptRow::Message { .. }
            | TranscriptRow::Reasoning { .. }
            | TranscriptRow::CodeBlock { .. } => None,
        })
        .collect()
}

#[tokio::test]
async fn send_turn_state_projection_stays_readable_and_settles_to_durable_truth() {
    let data_dir = tempfile::tempdir().expect("send turn projection tempdir");
    let (provider_entered_tx, mut provider_entered_rx) = mpsc::unbounded_channel();
    let provider_release = Arc::new(tokio::sync::Notify::new());
    let provider_release_for_completion = Arc::clone(&provider_release);
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("send-turn-state-projection")
        .complete(move |_| {
            let provider_entered_tx = provider_entered_tx.clone();
            let provider_release = Arc::clone(&provider_release_for_completion);
            let response_index = Arc::clone(&response_index_for_completion);
            async move {
                let call = response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = provider_entered_tx.send(call);
                if call == 0 {
                    provider_release.notified().await;
                }
                Ok(match call {
                    0 => {
                        let mut response = text_response(
                            "<lashlang>\nprint(\"durable execution disclosure\")\n</lashlang>",
                        );
                        response.parts.insert(
                            0,
                            lash::direct::LlmOutputPart::Reasoning {
                                text: "durable reasoning disclosure".to_string(),
                                replay: None,
                            },
                        );
                        response
                    }
                    1 => text_response("<lashlang>\nfinish \"settled answer\"\n</lashlang>"),
                    other => panic!("unexpected provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let session_id = state.current_session_id();
    let turn_text = "exercise the user-facing send path";

    let _ = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: turn_text.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send turn through the production handler");
    let submitted = restate_requests
        .recv()
        .await
        .expect("capture submitted Restate turn");
    let turn_id = submitted
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("submitted turn id")
        .to_string();

    let run_state = state.clone();
    let run_turn_id = turn_id.clone();
    let turn = tokio::spawn(async move {
        let session = run_state
            .core
            .session(session_id)
            .open()
            .await
            .expect("open submitted turn session");
        let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let output = session
            .turn(lash::TurnInput::text(turn_text))
            .turn_id(run_turn_id.clone())
            .require_finish()
            .expect("require finish")
            .stream_to(&ChannelTurnEvents {
                turn_state: Arc::clone(&turn_state),
            })
            .await
            .expect("run submitted turn");
        crate::restate::record_turn_output(
            &run_state,
            &session,
            &run_turn_id,
            output,
            turn_state,
            "test.send_turn.completed",
        )
        .await
        .expect("record submitted turn output");
        crate::restate::settle_workbench_turn(&run_state, &session.session_id(), &run_turn_id)
            .await
            .expect("settle submitted turn");
    });

    assert_eq!(
        provider_entered_rx.recv().await,
        Some(0),
        "the first provider call must be blocked before the mid-turn read"
    );
    let Json(running) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("/api/state must remain readable while the turn lease is held");
    assert_eq!(running.active_turns.len(), 1);

    provider_release.notify_one();
    turn.await.expect("submitted turn task");
    assert_eq!(
        provider_entered_rx.recv().await,
        Some(1),
        "the turn must execute the terminal provider iteration"
    );

    let Json(settled) = app_state(State(state), Query(SessionQuery::default()))
        .await
        .expect("materialize settled state");
    assert_eq!(
        settled
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        vec![("user", turn_text), ("assistant", "settled answer")],
        "settlement must contain exactly the committed transcript rows"
    );
    assert_eq!(
        settled
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message } => {
                    Some((message.role.as_str(), message.text.as_str()))
                }
                TranscriptRow::Reasoning { .. } | TranscriptRow::CodeBlock { .. } => None,
            })
            .collect::<Vec<_>>(),
        vec![("user", turn_text), ("assistant", "settled answer")],
        "the browser transcript projection must contain the committed message set once"
    );
    assert!(
        settled.transcript.iter().any(|row| matches!(
            row,
            TranscriptRow::Reasoning { text, .. }
                if text == "durable reasoning disclosure"
        )),
        "settled state must reconstruct reasoning disclosure from durable history"
    );
    assert!(
        settled.transcript.iter().any(|row| matches!(
            row,
            TranscriptRow::CodeBlock { code, output, .. }
                if code.contains("durable execution disclosure")
                    && output.contains("durable execution disclosure")
        )),
        "settled state must reconstruct code execution and output from durable history"
    );
    assert!(
        settled.product_events.events.iter().all(|event| {
            !matches!(
                &event.item,
                StreamItem::Message { message }
                    if settled.messages.iter().any(|committed| committed.id == message.id)
            ) && !matches!(
                &event.item,
                StreamItem::Done {
                    turn_id: Some(done_turn_id),
                    ..
                } if done_turn_id == &turn_id
            )
        }),
        "settled message and Done rows must leave the product-event lane"
    );
}

#[tokio::test]
async fn workbench_sequential_settled_turn_cancels_each_emit_done() {
    let data_dir = tempfile::tempdir().expect("workbench cancel identity tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open cancel identity session");

    for turn_id in ["settled-turn-a", "settled-turn-b"] {
        session
            .turn(lash::TurnInput::text(format!("complete {turn_id}")))
            .turn_id(turn_id)
            .require_finish()
            .expect("require finish")
            .run()
            .await
            .expect("complete turn before stale cancel");
        state.track_turn(&session_id, turn_id);
        let receipts = state
            .cancel_turns_for_session(&session_id)
            .await
            .expect("cancel settled turn");
        assert!(matches!(
            receipts.as_slice(),
            [TurnCancelReceipt {
                outcome: lash::TurnCancelOutcome::CompletionWonRace,
                ..
            }]
        ));
    }

    let done_ids = state
        .event_tx
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter_map(|event| {
            matches!(event.item, StreamItem::Done { .. }).then_some(event.event_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        done_ids.len(),
        2,
        "distinct evidence-free cancel operations must not collide"
    );
    assert_ne!(done_ids[0], done_ids[1]);
}

#[tokio::test]
async fn product_event_identity_deduplicates_real_live_and_canonical_turn_output() {
    let data_dir = tempfile::tempdir().expect("product event tempdir");
    let path = data_dir.path().join("product-events.json");
    let mut state = recoverable_chat_test_state(data_dir.path(), 4).await;
    state.event_tx =
        SessionEventRegistry::persistent(path.clone(), 4).expect("persistent product events");
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open real turn session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("produce one stable answer"))
        .turn_id("stable-turn")
        .require_finish()
        .expect("require finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run real streamed turn");
    crate::restate::record_turn_output(
        &state,
        &session,
        "stable-turn",
        output,
        turn_state,
        "test.real_turn.completed",
    )
    .await
    .expect("record real turn output");
    assert!(
        session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == "workbench-assistant:stable-turn"),
        "the production turn recorder must commit the canonical assistant row"
    );
    session.close().await.expect("close real turn session");

    let reopened =
        SessionEventRegistry::persistent(path, 4).expect("reopen persistent product events");
    let assistant_events = reopened
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter(|event| {
            matches!(
                &event.item,
                StreamItem::Message { message }
                    if message.id == "workbench-assistant:stable-turn"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_events.len(),
        1,
        "the real live product row must persist exactly once across reload"
    );
}

/// The duplicate agent reply FIG-984 removed was a property of how a turn
/// *terminates*, not of which path ran it: `record_turn_output` is the same code
/// for an interactive send and for a background wake. The test above pins the
/// terminal-value regime, where the workbench owns the committed copy; this one
/// pins the bare-prose regime, where the runtime already committed the reply as
/// the turn's terminal message and the workbench must add nothing.
#[tokio::test]
async fn interactive_bare_prose_termination_leaves_one_committed_agent_reply() {
    const BARE_PROSE_REPLY: &str = "bare prose answer";
    let data_dir = tempfile::tempdir().expect("bare prose tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-bare-prose")
        .complete(|_| async { Ok(text_response(BARE_PROSE_REPLY)) })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id)
        .open()
        .await
        .expect("open bare prose session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    // Deliberately no `require_finish`: this is the termination an interactive
    // turn reaches when the send path does not force the answer through
    // `finish`, and the one every queued turn reaches.
    let output = session
        .turn(lash::TurnInput::text("answer in prose"))
        .turn_id("bare-prose-turn")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run bare prose turn");
    assert!(
        matches!(
            &output.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                if text == BARE_PROSE_REPLY
        ),
        "unexpected termination for a bare prose reply: {:?}",
        output.outcome
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "bare-prose-turn",
        output,
        turn_state,
        "test.bare_prose.completed",
    )
    .await
    .expect("record bare prose turn output");
    let committed_agent_replies = session
        .read_view()
        .messages()
        .iter()
        .filter(|message| {
            lash::message_role(message) == "assistant"
                && lash::message_text(message).contains(BARE_PROSE_REPLY)
        })
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        committed_agent_replies.len(),
        1,
        "a bare-prose termination must commit the agent reply exactly once, \
         got {committed_agent_replies:?}"
    );
    assert!(
        !committed_agent_replies[0].starts_with("workbench-assistant:"),
        "the runtime's own terminal message is the committed copy on this path, \
         got {committed_agent_replies:?}"
    );
}

#[tokio::test]
async fn workbench_provider_failure_emits_only_fixed_public_product_copy() {
    const INTERNAL_PROVIDER_FAILURE: &str = "provider rejected credentials for secret account";
    let data_dir = tempfile::tempdir().expect("provider failure tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-provider-failure")
        .complete_error(INTERNAL_PROVIDER_FAILURE)
        .build()
        .into_handle();
    let state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open provider failure session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("fail through the provider"))
        .turn_id("provider-failure-turn")
        .require_finish()
        .expect("require finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("provider failure is represented as a stopped turn");
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.message.contains(INTERNAL_PROVIDER_FAILURE)),
        "the real provider diagnostic must reach the internal turn result"
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "provider-failure-turn",
        output,
        turn_state,
        "test.provider.failed",
    )
    .await
    .expect("project provider failure through the production recorder");

    let serialized = serde_json::to_string(&state.event_tx.snapshot(&session_id))
        .expect("serialize provider failure projection");
    assert!(serialized.contains(PUBLIC_TURN_FAILURE_MESSAGE));
    assert!(!serialized.contains(INTERNAL_PROVIDER_FAILURE));

    let response = AppError::internal(INTERNAL_PROVIDER_FAILURE).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read internal error response");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("decode internal error response"),
        json!({ "error": "internal server error" })
    );
}

#[test]
fn authorization_seam_can_deny_observation_without_product_specific_auth() {
    struct DenyObservation;

    impl WorkbenchAuthorizer for DenyObservation {
        fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
            match action {
                WorkbenchAuthorizationAction::Observe { .. } => {
                    Err(AppError::forbidden("observation denied by host policy"))
                }
                _ => Ok(()),
            }
        }
    }

    let authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyObservation));
    let denied = authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: "auth-session".to_string(),
        })
        .expect_err("host policy must be able to deny observation");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: "auth-session".to_string(),
        })
        .expect("independent enqueue policy remains pluggable");
}
