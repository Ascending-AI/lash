async fn register_cron_test_subscription(
    trigger_store: &lash::triggers::InMemoryTriggerStore,
    session_id: &str,
    source_key: &str,
) {
    lash::triggers::TriggerStore::execute_command(
        trigger_store,
        &format!("register:{session_id}:{source_key}"),
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                session_id,
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                format!("cron-test:{source_key}"),
                lash::process::ProcessExecutionEnvRef::new(format!("process-env:{source_key}")),
                crate::CRON_SCHEDULE_SOURCE_TYPE,
                source_key,
                lash::process::ProcessInput::Engine {
                    kind: "cron-test-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("cron-test-engine"),
            )
            .with_payload_schema(lash::triggers::LashSchema::any()),
        },
    )
    .await
    .expect("register cron trigger")
    .expect("cron trigger mutation");
}

struct MetaLossSessionStoreFactory {
    inner: lash::persistence::InMemorySessionStoreFactory,
    absent_session_ids: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl MetaLossSessionStoreFactory {
    fn new() -> Self {
        Self {
            inner: lash::persistence::InMemorySessionStoreFactory::new(),
            absent_session_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn remove_session_meta(&self, session_id: &str) {
        self.absent_session_ids
            .lock()
            .expect("absent session ids lock")
            .insert(session_id.to_string());
    }
}

#[async_trait::async_trait]
impl lash::persistence::SessionStoreFactory for MetaLossSessionStoreFactory {
    async fn create_store(
        &self,
        request: &lash::persistence::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn lash::persistence::RuntimePersistence>, lash::persistence::StoreError> {
        lash::persistence::SessionStoreFactory::create_store(&self.inner, request).await
    }

    async fn open_existing_store(
        &self,
        request: &lash::persistence::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn lash::persistence::RuntimePersistence>>, String> {
        if self
            .absent_session_ids
            .lock()
            .expect("absent session ids lock")
            .contains(&request.session_id)
        {
            return Ok(None);
        }
        lash::persistence::SessionStoreFactory::open_existing_store(&self.inner, request).await
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        lash::persistence::SessionStoreFactory::session_was_deleted(&self.inner, session_id).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        lash::persistence::SessionStoreFactory::delete_session(&self.inner, session_id).await
    }
}

async fn materialize_cron_test_session(state: &crate::AppState, session_id: &str) {
    drop(
        state
            .core
            .session(session_id)
            .open()
            .await
            .expect("materialize cron test session"),
    );
}

async fn retire_cron_test_session(state: &crate::AppState, session_id: &str) {
    let scope = state
        .core
        .session_delete_scope(session_id)
        .await
        .expect("build cron test session delete scope");
    let effect_host = state.core.effect_host();
    let scoped = effect_host
        .scoped(scope)
        .expect("scope cron test session deletion");
    state
        .core
        .delete_session(session_id, scoped)
        .await
        .expect("retire cron test session");
}

fn cron_tick_test_state(session_id: &str) -> super::WorkbenchCronState {
    super::WorkbenchCronState {
        request: WorkbenchCronRequest {
            session_id: session_id.to_string(),
            source_key: "cron-source:fig1018-decision".to_string(),
            expr: "*/10 * * * * *".to_string(),
            tz: Some("UTC".to_string()),
            name: Some("FIG-1018 cron decision".to_string()),
        },
        next_execution_time: "2026-08-08T12:00:10+00:00".to_string(),
        next_execution_id: "invocation-fig1018-decision".to_string(),
        last_fired_at: None,
    }
}

#[test]
fn cron_tick_decision_runs_for_a_live_session() {
    let state = cron_tick_test_state("live-cron-session");

    assert_eq!(
        super::cron_tick_decision(CronSessionDisposition::Live, &state, "cron-job-live"),
        super::CronTick::Run
    );
}

#[test]
fn cron_tick_decision_cancels_a_retired_session_with_typed_trace() {
    let state = cron_tick_test_state("retired-cron-session");

    let super::CronTick::Cancel { trace } = super::cron_tick_decision(
        CronSessionDisposition::Retired,
        &state,
        "cron-job-retired",
    ) else {
        panic!("a retired session must cancel its cron tick");
    };
    assert_eq!(trace["job_key"], "cron-job-retired");
    assert_eq!(trace["job_session_id"], "retired-cron-session");
    assert_eq!(trace["decision_basis"], "deleted_session_tombstone");
    assert_eq!(trace["session_state"], "retired");
    assert_eq!(trace["reason"], "session_retired");
}

#[test]
fn cron_tick_decision_cancels_an_unknown_session_with_typed_trace() {
    let state = cron_tick_test_state("absent-cron-session");

    let super::CronTick::Cancel { trace } = super::cron_tick_decision(
        CronSessionDisposition::Unknown,
        &state,
        "cron-job-absent",
    ) else {
        panic!("an absent session must cancel its orphaned cron tick");
    };
    assert_eq!(trace["job_key"], "cron-job-absent");
    assert_eq!(trace["job_session_id"], "absent-cron-session");
    assert_eq!(trace["decision_basis"], "session_store_meta_absent");
    assert_eq!(trace["session_state"], "unknown");
    assert_eq!(trace["reason"], "session_absent");
}

#[tokio::test]
async fn cron_session_disposition_is_unknown_when_store_meta_is_absent_without_a_tombstone() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let store_factory = Arc::new(MetaLossSessionStoreFactory::new());
    let state = crate::tests::recoverable_chat_test_state_with_store_factory_and_trigger_store(
        data_dir.path(),
        Arc::clone(&store_factory) as Arc<dyn lash::persistence::SessionStoreFactory>,
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = "meta-less-cron-session";
    let source_key = "cron-source:fig1018-meta-less";
    materialize_cron_test_session(&state, session_id).await;
    register_cron_test_subscription(trigger_store.as_ref(), session_id, source_key).await;

    assert!(
        state
            .core
            .session_exists(session_id)
            .await
            .expect("read materialized session metadata")
    );
    store_factory.remove_session_meta(session_id);
    assert!(
        !state
            .core
            .session_was_deleted(session_id)
            .await
            .expect("read absent-session tombstone")
    );
    assert!(
        !state
            .core
            .session_exists(session_id)
            .await
            .expect("read absent-session metadata")
    );
    assert_eq!(
        cron_session_disposition(&state.core, session_id)
            .await
            .expect("classify absent cron session"),
        CronSessionDisposition::Unknown
    );
}

#[tokio::test]
async fn cron_tick_allows_a_live_non_current_session_to_emit_a_delivery() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = "live-non-current-cron-session";
    let source_key = "cron-source:fig1018-live";
    assert_ne!(session_id, state.current_session_id());
    materialize_cron_test_session(&state, session_id).await;
    register_cron_test_subscription(trigger_store.as_ref(), session_id, source_key).await;

    assert_eq!(
        cron_session_disposition(&state.core, session_id)
            .await
            .expect("read live session tombstone state"),
        CronSessionDisposition::Live
    );
    let controller = CountingProcessEffectController::default();
    let scoped = lash::runtime::ScopedEffectController::borrowed(
        &controller,
        lash::runtime::ExecutionScope::runtime_operation("fig1018-live-non-current-cron"),
    )
    .expect("scope live non-current cron occurrence");
    emit_cron_occurrence_with_effect_controller(
        state,
        WorkbenchCronRequest {
            session_id: session_id.to_string(),
            source_key: source_key.to_string(),
            expr: "*/10 * * * * *".to_string(),
            tz: Some("UTC".to_string()),
            name: Some("FIG-1018 live non-current cron".to_string()),
        },
        "2026-08-08T12:00:00+00:00".to_string(),
        "fig1018-live-non-current-job",
        scoped,
    )
    .await
    .expect("live non-current cron tick must emit");

    assert_eq!(
        lash::triggers::TriggerStore::list_occurrences(
            trigger_store.as_ref(),
            lash::triggers::TriggerOccurrenceFilter::default(),
        )
        .await
        .expect("list cron occurrences")
        .len(),
        1
    );
    assert_eq!(
        lash::triggers::TriggerStore::list_deliveries(trigger_store.as_ref())
            .await
            .expect("list cron deliveries")
            .len(),
        1
    );
}

#[tokio::test]
async fn cron_tick_cancels_a_retired_session_with_typed_decision() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = "retired-cron-session";
    let source_key = "cron-source:fig1018-retired";
    materialize_cron_test_session(&state, session_id).await;
    register_cron_test_subscription(trigger_store.as_ref(), session_id, source_key).await;
    retire_cron_test_session(&state, session_id).await;

    let disposition = cron_session_disposition(&state.core, session_id)
        .await
        .expect("read retired session tombstone state");
    let cron_state = cron_tick_test_state(session_id);
    let super::CronTick::Cancel { trace } =
        super::cron_tick_decision(disposition, &cron_state, "cron-job-retired-integration")
    else {
        panic!("a retired session must produce a cancel decision");
    };
    assert_eq!(
        trace,
        serde_json::json!({
            "job_key": "cron-job-retired-integration",
            "job_session_id": session_id,
            "decision_basis": "deleted_session_tombstone",
            "session_state": "retired",
            "reason": "session_retired",
        })
    );
}
