use lash::sync::MutexExt;
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

#[derive(Clone, Default)]
struct ScriptedCronObjectSurface {
    infos: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Option<serde_json::Value>>>>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ScriptedCronObjectSurface {
    fn arm(&self, job_key: &str) {
        self.infos.lock_recover().insert(
            job_key.to_string(),
            Some(serde_json::json!({
                "source_key": "cron-source:fig1067",
                "expr": "*/10 * * * * *",
                "tz": "UTC",
                "next_execution_time": "2026-08-08T12:00:10+00:00",
                "next_execution_id": "invocation-fig1067-armed"
            })),
        );
    }

    fn info(&self, job_key: &str) -> Option<serde_json::Value> {
        self.infos
            .lock_recover()
            .get(job_key)
            .cloned()
            .flatten()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock_recover().clone()
    }
}

async fn spawn_scripted_cron_object_surface(surface: ScriptedCronObjectSurface) -> String {
    async fn invoke(
        axum::extract::Path(path): axum::extract::Path<String>,
        axum::extract::State(surface): axum::extract::State<ScriptedCronObjectSurface>,
        body: axum::body::Bytes,
    ) -> axum::Json<serde_json::Value> {
        surface
            .calls
            .lock_recover()
            .push(path.clone());
        let mut parts = path.split('/');
        let object = parts.next();
        let job_key = parts.next();
        let handler = parts.next();
        assert_eq!(object, Some("WorkbenchCronJob"));
        assert_eq!(parts.next(), None);
        let job_key = job_key.expect("scripted cron job key");
        match handler {
            Some("upsert") => {
                let request: serde_json::Value =
                    serde_json::from_slice(&body).expect("decode scripted cron upsert");
                let info = serde_json::json!({
                    "source_key": request["source_key"],
                    "expr": request["expr"],
                    "tz": request["tz"],
                    "next_execution_time": "2026-08-08T12:00:10+00:00",
                    "next_execution_id": "invocation-fig1067-upserted"
                });
                surface
                    .infos
                    .lock_recover()
                    .insert(job_key.to_string(), Some(info.clone()));
                axum::Json(info)
            }
            Some("cancel") => {
                surface
                    .infos
                    .lock_recover()
                    .insert(job_key.to_string(), None);
                axum::Json(serde_json::Value::Null)
            }
            Some("info") => axum::Json(surface.info(job_key).unwrap_or(serde_json::Value::Null)),
            other => panic!("unexpected scripted cron handler: {other:?}"),
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted cron object surface");
    let addr = listener
        .local_addr()
        .expect("scripted cron object surface addr");
    let app = axum::Router::new()
        .route("/{*path}", axum::routing::post(invoke))
        .with_state(surface);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve scripted cron object surface");
    });
    format!("http://{addr}")
}

async fn register_fig1067_cron_subscription(
    trigger_store: &lash::triggers::InMemoryTriggerStore,
    session_id: &str,
) -> lash::triggers::TriggerSubscriptionRecord {
    let source_key = "cron-source:fig1067";
    let outcome = lash::triggers::TriggerStore::execute_command(
        trigger_store,
        "register:fig1067",
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                session_id,
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                "cron-test:fig1067",
                lash::process::ProcessExecutionEnvRef::new("process-env:fig1067"),
                crate::CRON_SCHEDULE_SOURCE_TYPE,
                source_key,
                lash::process::ProcessInput::Engine {
                    kind: "cron-test-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("cron-test-engine"),
            )
            .with_source(
                lashlang::HostDescriptor::encode(
                    crate::CRON_SCHEDULE_SOURCE_TYPE,
                    serde_json::json!({
                        "expr": "*/10 * * * * *",
                        "tz": "UTC"
                    }),
                )
                .expect("encode FIG-1067 cron source"),
            )
            .with_payload_schema(lash::triggers::LashSchema::any()),
        },
    )
    .await
    .expect("register FIG-1067 cron trigger")
    .expect("FIG-1067 cron trigger mutation");
    let lash::triggers::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("register must return a mutation receipt");
    };
    receipt.record_snapshot
}

fn fig1067_cron_registration(
    session_id: &str,
    source_key: &str,
    enabled: bool,
) -> lash::triggers::TriggerRegistration {
    let record = lash::triggers::TriggerSubscriptionRecord {
        subscription_id: format!("subscription:{source_key}"),
        owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
        subscription_key: format!("cron-test:{source_key}"),
        incarnation: "fig1067-incarnation".to_string(),
        revision: 1,
        definition_fingerprint: "fig1067-fingerprint".to_string(),
        registrant: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
            session_id,
        )),
        env_ref: lash::process::ProcessExecutionEnvRef::new("process-env:fig1067-plan"),
        wake_target: None,
        name: None,
        source_type: crate::CRON_SCHEDULE_SOURCE_TYPE.to_string(),
        source_key: source_key.to_string(),
        source: lashlang::HostDescriptor::encode(
            crate::CRON_SCHEDULE_SOURCE_TYPE,
            serde_json::json!({ "expr": "*/10 * * * * *", "tz": "UTC" }),
        )
        .expect("encode FIG-1067 cron source"),
        payload_schema: lash::triggers::LashSchema::any(),
        target: lash::process::ProcessInput::Engine {
            kind: "cron-test-engine".to_string(),
            payload: serde_json::json!({}),
        },
        target_identity: lash::process::ProcessIdentity::new("cron-test-engine"),
        event_types: Vec::new(),
        input_template: std::collections::BTreeMap::new(),
        target_label: None,
        enabled,
        tombstoned: false,
        deleted_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    lash::triggers::TriggerRegistration::from(&record)
}

async fn register_fig1067_button_subscription(
    trigger_store: &lash::triggers::InMemoryTriggerStore,
    session_id: &str,
) -> lash::triggers::TriggerSubscriptionRecord {
    let source_key = lash::triggers::empty_trigger_source_key(crate::BUTTON_TRIGGER_SOURCE_TYPE)
        .expect("button source key");
    let outcome = lash::triggers::TriggerStore::execute_command(
        trigger_store,
        "register:fig1067-button",
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                session_id,
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                "button-test:fig1067",
                lash::process::ProcessExecutionEnvRef::new("process-env:fig1067-button"),
                crate::BUTTON_TRIGGER_SOURCE_TYPE,
                source_key,
                lash::process::ProcessInput::Engine {
                    kind: "button-test-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("button-test-engine"),
            )
            .with_payload_schema(lash::triggers::LashSchema::any()),
        },
    )
    .await
    .expect("register FIG-1067 button trigger")
    .expect("FIG-1067 button trigger mutation");
    let lash::triggers::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("register must return a mutation receipt");
    };
    receipt.record_snapshot
}

fn assert_fig1067_cron_sync_trace(
    trace_path: &std::path::Path,
    name: &str,
    reason: &str,
    session_id: &str,
    job_key: &str,
) {
    let trace = std::fs::read_to_string(trace_path).expect("read FIG-1067 cron sync trace");
    let record = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| {
            record.get("name").and_then(serde_json::Value::as_str) == Some(name)
                && record
                    .pointer("/payload/job_key")
                    .and_then(serde_json::Value::as_str)
                    == Some(job_key)
        })
        .unwrap_or_else(|| panic!("missing `{name}` for `{job_key}` in:\n{trace}"));
    assert_eq!(
        record
            .pointer("/payload/reason")
            .and_then(serde_json::Value::as_str),
        Some(reason)
    );
    assert_eq!(
        record
            .pointer("/payload/job_session_id")
            .and_then(serde_json::Value::as_str),
        Some(session_id)
    );
}

#[test]
fn cron_sync_plan_for_one_session_never_cancels_another_sessions_job() {
    let session_a = "fig1067:session:a";
    let session_b = "fig1067:session:a:other";
    let key_a = super::cron_job_key(session_a, "cron-source:a");
    let key_b = super::cron_job_key(session_b, "cron-source:b");
    let known_by_session = std::collections::BTreeMap::from([
        (
            session_a.to_string(),
            std::collections::BTreeSet::from([key_a.clone()]),
        ),
        (
            session_b.to_string(),
            std::collections::BTreeSet::from([key_b.clone()]),
        ),
    ]);
    let plan = super::cron_sync_plan(
        session_a,
        &[fig1067_cron_registration(session_a, "cron-source:a", true)],
        known_by_session[session_a].clone(),
    );
    assert_eq!(plan.upserts.keys().collect::<Vec<_>>(), vec![&key_a]);
    assert!(plan.cancels.is_empty());
    assert_eq!(
        known_by_session[session_b],
        std::collections::BTreeSet::from([key_b])
    );
}

#[test]
fn undecodable_cron_registration_remains_cancellable() {
    let session_id = "fig1067-invalid-source";
    let source_key = "cron-source:invalid";
    let mut registration = fig1067_cron_registration(session_id, source_key, true);
    registration.source = serde_json::Value::Null;

    let plan = super::cron_sync_plan(session_id, &[registration], Default::default());

    assert!(plan.upserts.is_empty());
    assert_eq!(
        plan.cancels,
        std::collections::BTreeSet::from([super::cron_job_key(session_id, source_key)])
    );
}

#[tokio::test]
async fn disabling_a_trigger_cancels_its_armed_cron_before_the_route_returns() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    drop(
        state
            .core
            .session(&session_id)
            .open()
            .await
            .expect("materialize FIG-1067 cron session"),
    );
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    let job_key = format!("{session_id}:{}", record.source_key);
    let surface = ScriptedCronObjectSurface::default();
    surface.arm(&job_key);
    state.restate_ingress_url = spawn_scripted_cron_object_surface(surface.clone()).await;
    let trace_path = data_dir.path().join("fig1067-disable-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    let _ = crate::set_trigger_enabled(
        axum::extract::Path(record.subscription_key),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id.clone()),
        }),
        axum::Json(crate::TriggerEnabledRequest { enabled: false }),
    )
    .await
    .expect("disable route must synchronize Restate cron");

    assert_eq!(surface.info(&job_key), None, "cron info must be null");
    assert_eq!(
        surface.calls(),
        vec![format!("WorkbenchCronJob/{job_key}/cancel")]
    );
    assert_fig1067_cron_sync_trace(
        &trace_path,
        "agent_workbench.cron.restate.sync_cancelled",
        "trigger_disabled",
        &session_id,
        &job_key,
    );
}

#[tokio::test]
async fn enabling_a_trigger_rearms_its_cron_before_the_route_returns() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    drop(
        state
            .core
            .session(&session_id)
            .open()
            .await
            .expect("materialize FIG-1067 cron session"),
    );
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    lash::triggers::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "disable:fig1067-enable-test",
        lash::triggers::TriggerCommand::Disable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                &session_id,
            )),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        },
    )
    .await
    .expect("disable FIG-1067 cron trigger")
    .expect("FIG-1067 disable mutation");
    let job_key = format!("{session_id}:{}", record.source_key);
    let surface = ScriptedCronObjectSurface::default();
    state.restate_ingress_url = spawn_scripted_cron_object_surface(surface.clone()).await;
    let trace_path = data_dir.path().join("fig1067-enable-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    let axum::Json(response) = crate::set_trigger_enabled(
        axum::extract::Path(record.subscription_key),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id.clone()),
        }),
        axum::Json(crate::TriggerEnabledRequest { enabled: true }),
    )
    .await
    .expect("enable route must synchronize Restate cron");

    assert!(response.changed);
    assert!(
        response
            .registration
            .expect("enabled registration response")
            .enabled
    );
    assert!(surface.info(&job_key).is_some(), "cron info must be armed");
    assert_eq!(
        surface.calls(),
        vec![format!("WorkbenchCronJob/{job_key}/upsert")]
    );
    assert_fig1067_cron_sync_trace(
        &trace_path,
        "agent_workbench.cron.restate.sync_upserted",
        "trigger_enabled",
        &session_id,
        &job_key,
    );
}

#[tokio::test]
async fn deleting_a_trigger_cancels_its_armed_cron_before_the_route_returns() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    drop(
        state
            .core
            .session(&session_id)
            .open()
            .await
            .expect("materialize FIG-1067 cron session"),
    );
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    let job_key = format!("{session_id}:{}", record.source_key);
    let surface = ScriptedCronObjectSurface::default();
    surface.arm(&job_key);
    state.restate_ingress_url = spawn_scripted_cron_object_surface(surface.clone()).await;
    let trace_path = data_dir.path().join("fig1067-delete-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    let axum::Json(response) = crate::delete_trigger(
        axum::extract::Path(record.subscription_key),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("delete route must synchronize Restate cron");

    assert!(response.changed);
    assert!(response.registration.is_none());
    assert_eq!(surface.info(&job_key), None, "cron info must be null");
    assert_eq!(
        surface.calls(),
        vec![format!("WorkbenchCronJob/{job_key}/cancel")]
    );
    assert_fig1067_cron_sync_trace(
        &trace_path,
        "agent_workbench.cron.restate.sync_cancelled",
        "trigger_deleted",
        &session_id,
        &job_key,
    );
}

#[tokio::test]
async fn syncing_session_a_leaves_session_bs_armed_cron_untouched() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_a = state.current_session_id();
    materialize_cron_test_session(&state, &session_a).await;
    let record_a = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_a).await;
    let key_a = super::cron_job_key(&session_a, &record_a.source_key);
    let session_b = "fig1067-session-b";
    let key_b = super::cron_job_key(session_b, "cron-source:b");
    state
        .restate_cron_job_keys
        .lock_recover()
        .insert(
            session_b.to_string(),
            std::collections::BTreeSet::from([key_b.clone()]),
        );
    let surface = ScriptedCronObjectSurface::default();
    surface.arm(&key_b);
    state.restate_ingress_url = spawn_scripted_cron_object_surface(surface.clone()).await;
    let trace_path = data_dir.path().join("fig1067-two-session-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    super::sync_cron_jobs_after_trigger_mutation(
        &state,
        &session_a,
        "two_session_regression",
        &record_a,
    )
    .await
    .expect("sync session A");

    assert!(
        surface.info(&key_b).is_some(),
        "session B cron must stay armed"
    );
    assert_eq!(
        surface.calls(),
        vec![format!("WorkbenchCronJob/{key_a}/upsert")]
    );
    let sync_records = std::fs::read_to_string(&trace_path)
        .expect("read two-session trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("decode trace"))
        .filter(|record| {
            record
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| name.starts_with("agent_workbench.cron.restate.sync_"))
        })
        .collect::<Vec<_>>();
    assert!(!sync_records.is_empty());
    for record in sync_records {
        assert_eq!(
            record.pointer("/payload/job_key"),
            Some(&serde_json::json!(key_a))
        );
        assert_eq!(
            record.pointer("/payload/job_session_id"),
            Some(&serde_json::json!(session_a))
        );
    }
    assert_eq!(
        state
            .restate_cron_job_keys
            .lock_recover()
            .get(session_b),
        Some(&std::collections::BTreeSet::from([key_b]))
    );
}

#[tokio::test]
async fn disabling_a_button_trigger_makes_zero_cron_ingress_calls() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    let record = register_fig1067_button_subscription(trigger_store.as_ref(), &session_id).await;
    let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dead ingress probe");
    state.restate_ingress_url = format!(
        "http://{}",
        dead_listener.local_addr().expect("dead ingress probe addr")
    );
    drop(dead_listener);

    let axum::Json(response) = crate::set_trigger_enabled(
        axum::extract::Path(record.subscription_key),
        axum::extract::State(state.clone()),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id),
        }),
        axum::Json(crate::TriggerEnabledRequest { enabled: false }),
    )
    .await
    .expect("non-cron mutation must succeed with cron ingress dead");

    assert!(response.changed);
    assert!(!response.registration.expect("button registration").enabled);
    assert!(
        state
            .restate_cron_job_keys
            .lock_recover()
            .is_empty()
    );
}

#[tokio::test]
async fn a_redundant_disable_reconciles_a_stale_armed_cron() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    materialize_cron_test_session(&state, &session_id).await;
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    let disabled = lash::triggers::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "disable:fig1067-reconciliation-pin",
        lash::triggers::TriggerCommand::Disable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                &session_id,
            )),
            subscription_key: record.subscription_key,
            expected_revision: record.revision,
        },
    )
    .await
    .expect("disable cron trigger")
    .expect("cron disable mutation");
    let lash::triggers::TriggerCommandOutcome::Mutation { receipt } = disabled else {
        panic!("disable must return a mutation receipt");
    };
    let job_key = super::cron_job_key(&session_id, &receipt.record_snapshot.source_key);
    let surface = ScriptedCronObjectSurface::default();
    surface.arm(&job_key);
    state.restate_ingress_url = spawn_scripted_cron_object_surface(surface.clone()).await;

    let axum::Json(response) = crate::set_trigger_enabled(
        axum::extract::Path(receipt.record_snapshot.subscription_key),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id),
        }),
        axum::Json(crate::TriggerEnabledRequest { enabled: false }),
    )
    .await
    .expect("redundant disable must reconcile Restate");

    assert!(!response.changed);
    assert_eq!(surface.info(&job_key), None);
    assert_eq!(
        surface.calls(),
        vec![format!("WorkbenchCronJob/{job_key}/cancel")]
    );
}

#[tokio::test]
async fn a_failed_disable_sync_still_traces_the_committed_mutation() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    materialize_cron_test_session(&state, &session_id).await;
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dead ingress probe");
    state.restate_ingress_url = format!(
        "http://{}",
        dead_listener.local_addr().expect("dead ingress probe addr")
    );
    drop(dead_listener);
    let trace_path = data_dir.path().join("fig1067-failed-disable-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    let result = crate::set_trigger_enabled(
        axum::extract::Path(record.subscription_key.clone()),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id.clone()),
        }),
        axum::Json(crate::TriggerEnabledRequest { enabled: false }),
    )
    .await;

    assert!(result.is_err(), "dead cron ingress must fail the route");
    let durable = lash::triggers::TriggerStore::list_subscriptions(
        trigger_store.as_ref(),
        lash::triggers::TriggerSubscriptionFilter::for_session(&session_id),
    )
    .await
    .expect("read durable trigger");
    assert!(!durable[0].enabled, "disable must remain committed");
    assert!(
        std::fs::read_to_string(trace_path)
            .expect("read failed-disable trace")
            .contains("agent_workbench.api.triggers.enabled")
    );
}

#[tokio::test]
async fn a_failed_delete_sync_still_traces_the_committed_mutation() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let mut state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    materialize_cron_test_session(&state, &session_id).await;
    let record = register_fig1067_cron_subscription(trigger_store.as_ref(), &session_id).await;
    let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dead ingress probe");
    state.restate_ingress_url = format!(
        "http://{}",
        dead_listener.local_addr().expect("dead ingress probe addr")
    );
    drop(dead_listener);
    let trace_path = data_dir.path().join("fig1067-failed-delete-trace.jsonl");
    state.trace_sink = Some(Arc::new(lash::tracing::JsonlTraceSink::new(
        trace_path.clone(),
    )));

    let result = crate::delete_trigger(
        axum::extract::Path(record.subscription_key),
        axum::extract::State(state),
        axum::extract::Query(crate::SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await;

    assert!(result.is_err(), "dead cron ingress must fail the route");
    let durable = lash::triggers::TriggerStore::list_subscriptions(
        trigger_store.as_ref(),
        lash::triggers::TriggerSubscriptionFilter::for_session(&session_id),
    )
    .await
    .expect("read durable triggers");
    assert!(durable.is_empty(), "delete must remain committed");
    assert!(
        std::fs::read_to_string(trace_path)
            .expect("read failed-delete trace")
            .contains("agent_workbench.api.triggers.delete")
    );
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
            .lock_recover()
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
            .lock_recover()
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

    let super::CronTick::Cancel { trace } =
        super::cron_tick_decision(CronSessionDisposition::Retired, &state, "cron-job-retired")
    else {
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

    let super::CronTick::Cancel { trace } =
        super::cron_tick_decision(CronSessionDisposition::Unknown, &state, "cron-job-absent")
    else {
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
