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
    recoverable_chat_test_state_with_dependencies_and_context(
        data_dir,
        channel_capacity,
        provider,
        trigger_store,
        store_factory,
        queued_work_driver,
        4096,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn recoverable_chat_test_state_with_dependencies_and_context(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    queued_work_driver: Option<lash::runtime::QueuedWorkDriver>,
    context_window_tokens: usize,
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
        lash::ModelSpec::builder("test-model")
            .context_window_tokens(context_window_tokens)
            .build()
            .expect("model spec"),
    );
    let mut core_builder = explicit_durable_test_facets(data_dir)
        .provider(provider)
        .model(model)
        .store_factory(Arc::clone(&store_factory))
        .process_registry(Arc::clone(&process_registry))
        .trigger_store(Arc::clone(&trigger_store));
    if let Some(queued_work_driver) = queued_work_driver {
        core_builder = core_builder.queued_work_driver(queued_work_driver);
    }
    let core = core_builder
        .build(crate::test_core_owner())
        .expect("build test core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    AppState {
        core,
        rlm_dialect: lash::rlm::RlmDialect::Lashlang,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&store_factory),
        trigger_store,
        process_observer,
        process_work_driver: inert_process_work_driver(process_registry),
        sessions: WorkbenchSessions::fresh(),
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
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
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
            .lock_recover() = Some(session_id.to_string());
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
            .lock_recover()
            .take();
        if let Some(session_id) = session_id {
            self.store_factory
                .delete_session(&session_id)
                .await
                .map_err(|error| lash::plugins::PluginError::Session(error.to_string()))?;
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
    ) -> std::result::Result<lash::triggers::TriggerIngressReceipt, lash::plugins::PluginError> {
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

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<String>, lash::plugins::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash::triggers::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[String],
    ) -> Result<
        lash::triggers::TriggerRetentionReconciliationReport,
        lash::plugins::PluginError,
    > {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash::triggers::TriggerDeliveryRetentionCandidate],
    ) -> std::result::Result<usize, lash::plugins::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash::triggers::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
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
            .lock_recover() = Some(session_id.to_string());
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
            .lock_recover()
            .take();
        if let Some(session_id) = session_id {
            self.store_factory
                .delete_session(&session_id)
                .await
                .map_err(|error| {
                    lash::runtime::QueuedWorkRunError::terminal(
                        lash::plugins::PluginError::Session(error.to_string()),
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
    assert_eq!(error.verdict, crate::AppErrorVerdict::Terminal);
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

#[tokio::test]
async fn workbench_browser_recovery_projection_preserves_rows_and_scopes_session_cursors() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/browser_projection.mjs");
    let trigger_identities = browser_projection_trigger_identities();
    let to_event_value = |event: lash::TurnEvent| {
        serde_json::to_value(
            lash::remote::usage::RemoteTurnEvent::try_from(event)
                .expect("convert browser turn event"),
        )
        .expect("serialize browser turn event")
    };
    let usage_event = to_event_value(lash::TurnEvent::Usage {
        protocol_iteration: 2,
        usage: lash::usage::TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 2,
            reasoning_output_tokens: 5,
        },
        cumulative: lash::usage::TokenUsage {
            input_tokens: 31,
            output_tokens: 17,
            cache_read_input_tokens: 13,
            cache_write_input_tokens: 12,
            reasoning_output_tokens: 9,
        },
    });
    assert_eq!(
        usage_event,
        serde_json::json!({
            "type": "usage",
            "protocol_iteration": 2,
            "usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "cache_read_input_tokens": 3,
                "cache_write_input_tokens": 2,
                "reasoning_output_tokens": 5,
            },
            "cumulative": {
                "input_tokens": 31,
                "output_tokens": 17,
                "cache_read_input_tokens": 13,
                "cache_write_input_tokens": 12,
                "reasoning_output_tokens": 9,
            },
        })
    );
    assert_eq!(usage_event["type"], "usage");
    assert_eq!(usage_event["protocol_iteration"], 2);
    assert_eq!(usage_event["usage"]["input_tokens"], 11);
    assert_eq!(usage_event["cumulative"]["input_tokens"], 31);
    let reset_event = to_event_value(lash::TurnEvent::ModelAttemptReset {
        assistant_prose_correlation_ids: vec![lash::TurnActivityId::new("prose-superseded")],
        reasoning_correlation_ids: vec![lash::TurnActivityId::new("reasoning-superseded")],
    });
    assert_eq!(
        reset_event,
        serde_json::json!({
            "type": "model_attempt_reset",
            "assistant_prose_correlation_ids": ["prose-superseded"],
            "reasoning_correlation_ids": ["reasoning-superseded"],
        })
    );
    assert_eq!(reset_event["type"], "model_attempt_reset");
    assert_eq!(reset_event["assistant_prose_correlation_ids"][0], "prose-superseded");
    assert_eq!(reset_event["reasoning_correlation_ids"][0], "reasoning-superseded");
    let retry_event = to_event_value(lash::TurnEvent::RetryStatus {
        wait_seconds: 2,
        attempt: 1,
        max_attempts: 3,
        reason: "deterministic retry law".to_string(),
    });
    assert_eq!(
        retry_event,
        serde_json::json!({
            "type": "retry_status",
            "wait_seconds": 2,
            "attempt": 1,
            "max_attempts": 3,
            "reason": "deterministic retry law",
        })
    );
    assert_eq!(retry_event["type"], "retry_status");
    assert_eq!(retry_event["wait_seconds"], 2);
    assert_eq!(retry_event["attempt"], 1);
    assert_eq!(retry_event["max_attempts"], 3);
    assert_eq!(retry_event["reason"], "deterministic retry law");
    let error_event = to_event_value(lash::TurnEvent::Error {
        message: "provider exhausted deterministic retries".to_string(),
    });
    assert_eq!(
        error_event,
        serde_json::json!({
            "type": "error",
            "message": "provider exhausted deterministic retries",
        })
    );
    let code_started_event = to_event_value(lash::TurnEvent::CodeBlockStarted {
        language: "lashlang".to_string(),
        code: "web.search({ query: \"FIG-1350\" })".to_string(),
        graph_key: None,
    });
    let tool_started_event = to_event_value(lash::TurnEvent::ToolCallStarted {
        call_id: Some("tool-call-1".to_string()),
        name: "search_web".to_string(),
        args: serde_json::json!({ "query": "FIG-1350" }),
        graph_key: None,
        parent_call_id: None,
    });
    let tool_completed_event = to_event_value(lash::TurnEvent::ToolCallCompleted {
        call_id: Some("tool-call-1".to_string()),
        name: "search_web".to_string(),
        args: serde_json::json!({ "query": "FIG-1350" }),
        output: lash::tools::ToolCallOutput::success(
            serde_json::json!({ "results": [{ "title": "judged row" }] }),
        ),
        duration_ms: 4,
        graph_key: None,
        parent_call_id: None,
    });
    let no_id_tool_started_event = to_event_value(lash::TurnEvent::ToolCallStarted {
        call_id: None,
        name: "search_web".to_string(),
        args: serde_json::json!({ "query": "FIG-1350 no id" }),
        graph_key: None,
        parent_call_id: None,
    });
    let no_id_tool_completed_event = to_event_value(lash::TurnEvent::ToolCallCompleted {
        call_id: None,
        name: "search_web".to_string(),
        args: serde_json::json!({ "query": "FIG-1350 no id" }),
        output: lash::tools::ToolCallOutput::success(
            serde_json::json!({ "results": [{ "title": "no-id row" }] }),
        ),
        duration_ms: 5,
        graph_key: None,
        parent_call_id: None,
    });
    let code_completed_event = to_event_value(lash::TurnEvent::CodeBlockCompleted {
        language: "lashlang".to_string(),
        output: "completed".to_string(),
        error: None,
        success: true,
        duration_ms: 9,
        tool_call_ids: vec!["tool-call-1".to_string()],
        graph_key: None,
    });
    let no_id_code_completed_event = to_event_value(lash::TurnEvent::CodeBlockCompleted {
        language: "lashlang".to_string(),
        output: "completed without call id".to_string(),
        error: None,
        success: true,
        duration_ms: 10,
        tool_call_ids: Vec::new(),
        graph_key: None,
    });
    let turn_events = serde_json::json!({
        "usage": usage_event,
        "reset": reset_event,
        "retry": retry_event,
        "error": error_event,
        "codeStarted": code_started_event,
        "toolStarted": tool_started_event,
        "toolCompleted": tool_completed_event,
        "codeCompleted": code_completed_event,
        "noIdToolStarted": no_id_tool_started_event,
        "noIdToolCompleted": no_id_tool_completed_event,
        "noIdCodeCompleted": no_id_code_completed_event,
    });
    let evidence_scenarios = Box::pin(provider_execution_evidence_scenarios()).await;

    // RLM's printed-image projection can commit more than one stored image
    // part on a single message. Feed that production projection to the browser
    // gate so its numbered-alt branch is covered from the real wire shape.
    let data_dir = tempfile::tempdir().expect("multi-attachment browser tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 4).await;
    let session = state
        .core
        .session(state.current_session_id())
        .open()
        .await
        .expect("open multi-attachment browser session");
    let mut committed = lash::plugins::PluginMessage::text(
        lash::messages::MessageRole::User,
        "two printed images",
    )
    .with_id("rlm-printed-images");
    for id in ["sha256:rlm-printed-image-a", "sha256:rlm-printed-image-b"] {
        committed
            .attachments
            .push(lash::direct::AttachmentSource::stored(
                lash::attachments::AttachmentRef {
                    id: lash::attachments::AttachmentId::parse(id).expect("valid attachment id"),
                    media_type: lash::attachments::MediaType::parse("image/png")
                        .expect("valid PNG media type"),
                    byte_len: 68,
                    type_metadata: None,
                    label: None,
                },
            ));
    }
    session
        .admin()
        .state()
        .append_messages(vec![committed])
        .await
        .expect("commit RLM printed-image shape");
    let mut persisted = session
        .admin()
        .state()
        .persist_current()
        .await
        .expect("persist multi-attachment state before durable tool fixture");
    persisted.session_graph.append_protocol_event(
        lash_protocol_rlm::rlm_protocol_event(
            lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(
                lash_rlm_types::RlmTrajectoryEntry {
                    id: "durable-tool-trajectory".to_string(),
                    protocol_iteration: 1,
                    code: "durable.tool_projection()".to_string(),
                    output: vec!["durable projection".to_string()],
                    calls: vec![
                        lash::persistence::ExecutedCallRecord {
                            operation: "durable.success".to_string(),
                            outcome: lash::persistence::ExecutedCallOutcome::Ok,
                        },
                        lash::persistence::ExecutedCallRecord {
                            operation: "durable.failure".to_string(),
                            outcome: lash::persistence::ExecutedCallOutcome::Err,
                        },
                    ],
                    calls_omitted: 3,
                    ..lash_rlm_types::RlmTrajectoryEntry::default()
                },
            ),
        ),
    );
    session
        .admin()
        .state()
        .set_persisted(persisted)
        .await
        .expect("install durable tool trajectory fixture");
    session
        .admin()
        .state()
        .persist_current()
        .await
        .expect("commit durable tool trajectory fixture");
    let committed_message = session
        .read_view()
        .messages()
        .iter()
        .find(|message| message.id == "rlm-printed-images")
        .map(chat_message_from_committed)
        .expect("project committed RLM printed images");
    session.close().await.expect("close multi-attachment session");
    let Json(durable_tool_state) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("reload and project committed durable tool trajectory");

    let output = std::process::Command::new("node")
        .arg("--test")
        .arg(&script)
        .env(
            "LASH_WORKBENCH_TRIGGER_IDENTITIES",
            trigger_identities.to_string(),
        )
        .env(
            "LASH_WORKBENCH_MULTI_ATTACHMENT_MESSAGE",
            serde_json::to_string(&committed_message)
                .expect("serialize committed multi-attachment message"),
        )
        .env("LASH_WORKBENCH_TURN_EVENTS", turn_events.to_string())
        .env(
            "LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT",
            serde_json::to_string(&durable_tool_state.transcript)
                .expect("serialize Rust-produced durable tool transcript"),
        )
        .env(
            "LASH_WORKBENCH_EXECUTION_EVIDENCE_SCENARIOS",
            serde_json::to_string(&evidence_scenarios)
                .expect("serialize provider evidence runtime scenarios"),
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
fn attachment_urls_percent_encode_the_id_path_segment() {
    let attachment = ChatAttachment::from_id("sha256:folder/image 1.png");
    assert_eq!(
        attachment.retrieve_url,
        "/api/attachments/sha256%3Afolder%2Fimage%201%2Epng"
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
                attachments: Vec::new(),
            },
        },
    );
    let expected_record = lash::remote::llm::RemoteLlmCallRecord {
        call_id: "call-1".to_string(),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![
            lash::remote::llm::RemoteAttemptRecord {
                ordinal: 1,
                started_at_ms: 7,
                duration_ms: 3,
                outcome: lash::remote::llm::RemoteAttemptOutcome::Failed,
                protocol_position: lash::remote::llm::RemoteProtocolPosition::NoResponse,
                retry_budget_consumed: true,
                retry_decision: Some(lash::remote::llm::RemoteRetryDecision {
                    scheduled: true,
                    delay_ms: Some(1),
                    reason: Some("retry".to_string()),
                }),
                error: Some(lash::remote::llm::RemoteNormalizedError {
                    class: "transport".to_string(),
                    provider_code: Some("connection_reset".to_string()),
                    http_status: Some(503),
                    provider_request_id: Some("request-1".to_string()),
                    retry_after_ms: Some(25),
                }),
                evidence: Some(lash::remote::llm::RemoteExecutionEvidence {
                    collection_interruption: Some(
                        lash::remote::llm::RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort,
                    ),
                    ..Default::default()
                }),
                generation_disposition: Some(lash::remote::llm::RemoteGenerationReceipt {
                    output_token_cap:
                        lash::remote::llm::RemoteGenerationOptionOutcome::ClampedToCapacity,
                    temperature:
                        lash::remote::llm::RemoteGenerationOptionOutcome::OmittedSamplingPinned,
                    seed: lash::remote::llm::RemoteGenerationOptionOutcome::OmittedUnsupported,
                    stop_sequences:
                        lash::remote::llm::RemoteGenerationOptionOutcome::SuppressedProtocolOwned,
                    cache: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
                }),
                usage: Some(lash::remote::usage::RemoteUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 5,
                }),
            },
            lash::remote::llm::RemoteAttemptRecord {
                ordinal: 2,
                started_at_ms: 11,
                duration_ms: 2,
                outcome: lash::remote::llm::RemoteAttemptOutcome::Completed,
                protocol_position: lash::remote::llm::RemoteProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
            },
        ],
    };
    registry.publish_identified(
        session_id,
        "model-call",
        StreamItem::ModelCallRecorded {
            record: expected_record.clone(),
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
        &BTreeSet::from(["reconciled-turn".to_string()]),
        &BTreeSet::new(),
    );
    let reconciled = registry.snapshot(session_id);
    assert_eq!(reconciled.cursor, 3);
    assert_eq!(reconciled.events.len(), 2);
    assert!(matches!(
        &reconciled.events[0].item,
        StreamItem::Message { message }
            if message.id == workbench_turn_user_message_id("reconciled-turn")
    ));
    let StreamItem::ModelCallRecorded { record } = &reconciled.events[1].item else {
        panic!("reconciliation must retain the model-call record");
    };
    assert_eq!(record, &expected_record);
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
    assert_eq!(registry.snapshot(session_id).cursor, 3);

    registry.publish_identified(
        session_id,
        "host-only-event",
        StreamItem::Message {
            message: ChatMessage {
                id: "host-only".to_string(),
                role: "event".to_string(),
                text: "host event".to_string(),
                at: String::new(),
                attachments: Vec::new(),
            },
        },
    );
    assert_eq!(
        registry.snapshot(session_id).events[2].sequence,
        4,
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
                    attachments: Vec::new(),
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
    state.track_turn_prompt(&session_id, turn_id, "one send".to_string(), None);
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
        vec![ui_row.clone()],
        "the transcript projection suppresses the same committed copy"
    );

    // Settlement leaves the session-scoped UI-owned row in place. The runtime
    // copy remains typed provenance, not rendering authority.
    crate::restate::settle_workbench_turn(&state, &session_id, turn_id)
        .await
        .expect("settle the turn");
    let Json(settled) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize the settled snapshot");
    assert_eq!(
        user_rows(&settled),
        vec![ui_row.clone()],
        "the settled projection keeps the UI-owned send"
    );
    assert_eq!(
        transcript_user_rows(&settled),
        vec![ui_row],
        "the settled transcript keeps the UI-owned send"
    );
    assert_ne!(
        user_rows(&settled),
        vec![committed_row],
        "the model-facing graph must not become the user-row authority"
    );
}

#[tokio::test]
async fn submit_failure_retires_a_user_row_for_a_turn_that_never_commits() {
    let data_dir = tempfile::tempdir().expect("submit failure projection tempdir");
    let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failing Restate ingress");
    let addr = listener.local_addr().expect("failing Restate ingress addr");
    let app = Router::new().route(
        "/{*path}",
        post(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "submission refused" })),
            )
        }),
    );
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("failing Restate ingress stopped: {err}");
        }
    });
    state.restate_ingress_url = format!("http://{addr}");
    let never_committed = "optimistic row whose turn never commits";

    let _ = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: never_committed.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect_err("failing Restate submission must reject the send");

    let Json(settled) = app_state(State(state), Query(SessionQuery::default()))
        .await
        .expect("project the refused send");
    assert!(settled.active_turns.is_empty());
    assert!(settled.messages.iter().all(|message| {
        message.text != never_committed && !message.id.starts_with("workbench-user:")
    }));
    assert!(settled.transcript.iter().all(|row| match row {
        TranscriptRow::Message { message } => {
            message.text != never_committed && !message.id.starts_with("workbench-user:")
        }
        TranscriptRow::Reasoning { .. } | TranscriptRow::CodeBlock { .. } => true,
    }));
    assert!(settled.product_events.events.iter().all(|event| match &event.item {
        StreamItem::Message { message } => {
            message.text != never_committed && !message.id.starts_with("workbench-user:")
        }
        StreamItem::TurnInput { .. }
        | StreamItem::ModelCallRecorded { .. }
        | StreamItem::Done { .. } => true,
    }));
}

#[tokio::test]
async fn continue_as_keeps_session_user_rows_collapses_old_assistant_and_survives_reload() {
    let data_dir = tempfile::tempdir().expect("continue_as projection tempdir");
    let product_events_path = data_dir.path().join("product-events.json");
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("continue-as-workbench-projection")
        .complete(move |_| {
            let call = response_index_for_completion
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => text_response(
                        "<lashlang>\nawait control.continue_as({ task: \"finish in the follow frame\", seed: { boundary_marker: \"protocol-only-seed\" } })?\n</lashlang>",
                    ),
                    1 => text_response("<lashlang>\nfinish \"follow frame answer\"\n</lashlang>"),
                    other => panic!("unexpected continue_as provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    state.event_tx = SessionEventRegistry::persistent(product_events_path.clone(), 16)
        .expect("open persistent product event registry");
    let session_id = state.current_session_id();

    let first_turn_id = "workbench-turn-before-continue-as";
    let first_prompt = "first submitted row";
    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(first_turn_id),
        "user",
        first_prompt,
    );
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open pre-switch session");
    session
        .admin()
        .state()
        .append_messages(vec![
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::User,
                first_prompt,
            )
            .with_id("runtime-first-user")
            .with_origin(lash::messages::MessageOrigin::TurnInput {
                turn_id: first_turn_id.to_string(),
                input_id: Some("first-input".to_string()),
            }),
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "old frame answer",
            )
            .with_id(workbench_turn_assistant_message_id(first_turn_id)),
        ])
        .await
        .expect("seed the durable pre-switch conversation");

    let switch_turn_id = "workbench-turn-continue-as";
    let switch_prompt = "switch frames now";
    state.track_turn_prompt(
        &session_id,
        switch_turn_id,
        switch_prompt.to_string(),
        None,
    );
    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(switch_turn_id),
        "user",
        switch_prompt,
    );
    let switch_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let switch_output = session
        .turn(lash::TurnInput::text(switch_prompt))
        .turn_id(switch_turn_id)
        .require_finish()
        .expect("require follow-frame finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&switch_turn_state),
        })
        .await
        .expect("run continue_as turn through its follow frame");
    assert_eq!(
        switch_output.final_value(),
        Some(&json!("follow frame answer"))
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        switch_turn_id,
        switch_output,
        switch_turn_state,
        "test.continue_as.follow_frame.completed",
    )
    .await
    .expect("record follow-frame turn");
    crate::restate::settle_workbench_turn(&state, &session_id, switch_turn_id)
        .await
        .expect("settle continue_as turn");

    let durable_rows = session
        .read_view()
        .message_tree()
        .into_iter()
        .flat_map(|root| {
            let mut rows = vec![(root.active, root.message)];
            let mut pending = root.children;
            while let Some(node) = pending.pop() {
                rows.push((node.active, node.message));
                pending.extend(node.children);
            }
            rows
        })
        .map(|(active, message)| (active, lash::message_role(&message), lash::message_text(&message)))
        .collect::<Vec<_>>();
    assert!(
        durable_rows
            .iter()
            .any(|(_, role, text)| *role == "user" && text == first_prompt),
        "the durable graph must retain the pre-switch user input"
    );
    assert!(
        durable_rows
            .iter()
            .any(|(_, role, text)| *role == "assistant" && text == "old frame answer"),
        "the old assistant must remain in the durable graph even though the current-frame projection collapses it"
    );
    session.close().await.expect("close switched session");

    let Json(boundary) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("project continue_as boundary state");
    let expected_rows = vec![
        (
            workbench_turn_user_message_id(first_turn_id),
            "user".to_string(),
            first_prompt.to_string(),
        ),
        (
            workbench_turn_user_message_id(switch_turn_id),
            "user".to_string(),
            switch_prompt.to_string(),
        ),
        (
            workbench_turn_assistant_message_id(switch_turn_id),
            "assistant".to_string(),
            "follow frame answer".to_string(),
        ),
    ];
    let projected_rows = boundary
        .state
        .messages
        .iter()
        .map(|message| (message.id.clone(), message.role.clone(), message.text.clone()))
        .collect::<Vec<_>>();
    assert_eq!(projected_rows, expected_rows);
    assert!(boundary.state.messages.iter().all(|message| {
        message.text != "old frame answer" && message.text != "protocol-only-seed"
    }));
    assert_eq!(
        boundary
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message } => {
                    Some((message.id.clone(), message.role.clone(), message.text.clone()))
                }
                TranscriptRow::Reasoning { .. } | TranscriptRow::CodeBlock { .. } => None,
            })
            .collect::<Vec<_>>(),
        expected_rows,
        "the browser transcript projection must match /api/state at the boundary"
    );

    drop(state);
    let reload_provider = lash::testing::TestProvider::builder()
        .kind("continue-as-workbench-reload")
        .complete(|_| async { panic!("projection reload must not call the provider") })
        .build()
        .into_handle();
    let mut reloaded_state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, reload_provider).await;
    reloaded_state.event_tx = SessionEventRegistry::persistent(product_events_path, 16)
        .expect("reload persistent product event registry");
    let Json(reloaded) = Box::pin(app_state(
        State(reloaded_state),
        Query(SessionQuery {
            session_id: Some(session_id),
        }),
    ))
    .await
    .expect("rebuild continue_as projection after reload");
    assert_eq!(
        reloaded
            .state
            .messages
            .iter()
            .map(|message| (message.id.clone(), message.role.clone(), message.text.clone()))
            .collect::<Vec<_>>(),
        expected_rows,
        "reload must reproduce the same session-scoped projection"
    );
}

#[tokio::test]
async fn attachment_ref_stays_on_the_single_user_row_through_committed_backfill() {
    let data_dir = tempfile::tempdir().expect("attachment backfill tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let turn_id = "workbench-turn-fig994";
    let attachment = lash::attachments::AttachmentRef {
        id: lash::attachments::AttachmentId::parse("sha256:fig994-backfill").expect("valid attachment id"),
        media_type: lash::attachments::MediaType::parse("image/png")
            .expect("valid test media type"),
        byte_len: 68,
        type_metadata: Some(lash::attachments::AttachmentTypeMetadata::image(
            Some(1),
            Some(1),
        )),
        label: Some("backfill.png".to_string()),
    };
    let expected_attachment = (
        attachment.id.to_string(),
        attachment_retrieve_url(&attachment.id.to_string()),
    );

    state.track_turn_prompt(
        &session_id,
        turn_id,
        "one attached send".to_string(),
        Some(attachment.id.to_string()),
    );
    state.push_message_with_id_and_attachments_for_session(
        &session_id,
        workbench_turn_user_message_id(turn_id),
        "user",
        "one attached send",
        vec![ChatAttachment::from_id(attachment.id.to_string())],
    );

    let Json(optimistic) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("materialize optimistic attachment snapshot");
    assert_eq!(
        user_row_attachments(&optimistic),
        vec![(
            workbench_turn_user_message_id(turn_id),
            vec![expected_attachment.clone()],
        )],
        "the live UI-owned row carries the uploaded attachment reference once"
    );

    let mut committed = lash::plugins::PluginMessage::text(
        lash::messages::MessageRole::User,
        "one attached send",
    )
    .with_id("m_ingress_workbench-input-fig994")
    .with_origin(lash::messages::MessageOrigin::TurnInput {
        turn_id: turn_id.to_string(),
        input_id: Some("workbench-input-fig994".to_string()),
    });
    committed
        .attachments
        .push(lash::direct::AttachmentSource::stored(attachment));
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open attachment backfill session");
    session
        .admin()
        .state()
        .append_messages(vec![committed])
        .await
        .expect("commit attached turn input");
    session.close().await.expect("close attachment backfill session");

    let Json(running) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("materialize running attachment snapshot");
    assert_eq!(
        user_row_attachments(&running),
        vec![(
            workbench_turn_user_message_id(turn_id),
            vec![expected_attachment.clone()],
        )],
        "the committed copy stays suppressed while the attached UI row survives"
    );

    crate::restate::settle_workbench_turn(&state, &session_id, turn_id)
        .await
        .expect("settle attached workbench turn");
    let Json(settled) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize settled attachment snapshot");
    assert_eq!(
        user_row_attachments(&settled),
        vec![(
            workbench_turn_user_message_id(turn_id),
            vec![expected_attachment],
        )],
        "the UI-owned attachment row remains session-scoped after settlement"
    );
}

#[tokio::test]
async fn replayed_prompt_keeps_its_attachment_when_the_product_row_was_lost() {
    let data_dir = tempfile::tempdir().expect("attachment replay tempdir");
    let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let active_turns_path = data_dir.path().join("active-turns.json");
    state.active_turns =
        ActiveTurns::persistent(active_turns_path.clone()).expect("persistent active turns");
    let session_id = state.current_session_id();
    let turn_id = "workbench-turn-fig994-replay";
    let attachment = lash::attachments::AttachmentRef {
        id: lash::attachments::AttachmentId::parse("sha256:fig994-replay").expect("valid attachment id"),
        media_type: lash::attachments::MediaType::parse("image/png")
            .expect("valid test media type"),
        byte_len: 68,
        type_metadata: Some(lash::attachments::AttachmentTypeMetadata::image(
            Some(1),
            Some(1),
        )),
        label: Some("replay.png".to_string()),
    };
    let expected_attachment = (
        attachment.id.to_string(),
        attachment_retrieve_url(&attachment.id.to_string()),
    );

    state.track_turn_prompt(
        &session_id,
        turn_id,
        "one lost attached send".to_string(),
        Some(attachment.id.to_string()),
    );
    let mut committed = lash::plugins::PluginMessage::text(
        lash::messages::MessageRole::User,
        "one lost attached send",
    )
    .with_id("m_ingress_workbench-input-fig994-replay")
    .with_origin(lash::messages::MessageOrigin::TurnInput {
        turn_id: turn_id.to_string(),
        input_id: Some("workbench-input-fig994-replay".to_string()),
    });
    committed
        .attachments
        .push(lash::direct::AttachmentSource::stored(attachment));
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open attachment replay session");
    session
        .admin()
        .state()
        .append_messages(vec![committed])
        .await
        .expect("commit attached turn input");
    session.close().await.expect("close attachment replay session");
    state.active_turns =
        ActiveTurns::persistent(active_turns_path).expect("reopen persistent active turns");

    let Json(replayed) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize replayed attachment snapshot");
    assert_eq!(
        user_row_attachments(&replayed),
        vec![(
            workbench_turn_user_message_id(turn_id),
            vec![expected_attachment],
        )],
        "the lost-product-row replay must retain the uploaded attachment reference"
    );
}

#[tokio::test]
async fn committed_attachment_ref_is_exposed_in_the_workbench_snapshot() {
    let data_dir = tempfile::tempdir().expect("committed attachment snapshot tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let attachment = lash::attachments::AttachmentRef {
        id: lash::attachments::AttachmentId::parse("sha256:fig994-committed").expect("valid attachment id"),
        media_type: lash::attachments::MediaType::parse("image/png")
            .expect("valid test media type"),
        byte_len: 68,
        type_metadata: Some(lash::attachments::AttachmentTypeMetadata::image(
            Some(1),
            Some(1),
        )),
        label: Some("committed.png".to_string()),
    };
    let mut message =
        lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, "see image")
            .with_id("committed-attachment-message");
    message
        .attachments
        .push(lash::direct::AttachmentSource::stored(attachment.clone()));
    let session = state
        .core
        .session(session_id)
        .open()
        .await
        .expect("open committed attachment session");
    session
        .admin()
        .state()
        .append_messages(vec![message])
        .await
        .expect("append committed attachment message");
    session.close().await.expect("close committed attachment session");

    let Json(snapshot) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("read committed attachment snapshot");
    let wire = serde_json::to_value(snapshot).expect("serialize workbench snapshot");
    assert_eq!(
        wire["messages"][0]["attachments"],
        json!([{
            "attachment_id": attachment.id,
            "retrieve_url": attachment_retrieve_url(&attachment.id.to_string()),
        }]),
        "the committed message must expose the stored attachment reference"
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

fn user_row_attachments(snapshot: &StateReadSnapshot) -> Vec<(String, Vec<(String, String)>)> {
    snapshot
        .state
        .messages
        .iter()
        .filter(|message| message.role == "user")
        .map(|message| {
            (
                message.id.clone(),
                message
                    .attachments
                    .iter()
                    .map(|attachment| {
                        (
                            attachment.attachment_id.clone(),
                            attachment.retrieve_url.clone(),
                        )
                    })
                    .collect(),
            )
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
    let in_flight_store = state
        .session_store_factory
        .create_store(&lash::persistence::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: state.current_session_id(),
            relation: lash::persistence::SessionRelation::Root,
            policy: lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded),
        })
        .await
        .expect("open the in-flight session store");
    let in_flight = lash::persistence::load_persisted_session_state(&*in_flight_store)
        .await
        .expect("read the admitted in-flight durable state");
    assert!(
        in_flight.as_ref().is_none_or(|state| state
            .read_view()
            .messages()
            .iter()
            .all(|message| {
                !matches!(
                    message.origin.as_ref(),
                    Some(lash::messages::MessageOrigin::TurnInput {
                        turn_id: committed_turn_id,
                        ..
                    }) if committed_turn_id == &turn_id
                )
            })),
        "the initial turn input is not committed while the first provider call is in flight"
    );

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
            TranscriptRow::Reasoning { id, text }
                if id == &format!("m_rlm_{turn_id}_0_assistant_content.p0")
                    && text == "durable reasoning disclosure"
        )),
        "settled state must reconstruct reasoning disclosure with durable part provenance"
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
    assert_eq!(
        settled
            .product_events
            .events
            .iter()
            .filter_map(|event| match &event.item {
                StreamItem::Message { message } => {
                    Some((message.id.clone(), message.role.clone(), message.text.clone()))
                }
                StreamItem::TurnInput { .. }
                | StreamItem::ModelCallRecorded { .. }
                | StreamItem::Done { .. } => None,
            })
            .collect::<Vec<_>>(),
        vec![(
            workbench_turn_user_message_id(&turn_id),
            "user".to_string(),
            turn_text.to_string(),
        )],
        "settlement must retain only the session-scoped UI-owned user row"
    );
    assert!(
        settled.product_events.events.iter().all(|event| !matches!(
            &event.item,
            StreamItem::Done {
                turn_id: Some(done_turn_id),
                ..
            } if done_turn_id == &turn_id
        )),
        "settled Done rows must leave the product-event lane"
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

/// Build the shared state used to prove bare-prose replies are not duplicated.
pub(crate) async fn recoverable_chat_test_state_with_store_factory_and_trigger_store(
    data_dir: &std::path::Path,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
) -> AppState {
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-store-factory-test")
        .complete(|_| async {
            Ok(text_response(
                "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
            ))
        })
        .build()
        .into_handle();
    recoverable_chat_test_state_with_dependencies(
        data_dir,
        16,
        provider,
        trigger_store,
        store_factory,
        None,
    )
    .await
}
