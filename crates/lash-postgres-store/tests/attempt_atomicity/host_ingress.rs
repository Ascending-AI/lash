use super::*;

/// The facade-level duplicate law on the real key-addressed journal: the first
/// submission executes locally, the second returns the byte-equivalent recorded
/// outcome, and only one process event exists.
#[tokio::test(flavor = "multi_thread")]
async fn host_ingress_duplicate_replays_the_same_outcome_once_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL host-ingress law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the PostgreSQL host-ingress law");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let session_id = format!("pg-tool-intent-ingress-session-{suffix}");
    let scope_id = format!("pg-tool-intent-ingress-scope-{suffix}");
    let process_id = format!("pg-tool-intent-ingress-process-{suffix}");
    let event_type = "pg.tool_intent_ingress.realized";
    let registry = Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                &process_id,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash::process::ProcessEventType {
                name: event_type.to_string(),
                payload_schema: lash::triggers::LashSchema::any(),
                semantics: lash::process::ProcessEventSemanticsSpec::default(),
            }]),
            std::slice::from_ref(&session_id),
        )
        .await
        .expect("register the PostgreSQL host-ingress target");

    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("pg-tool-intent-ingress-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid PostgreSQL ingress model"),
        )
        .effect_host(Arc::new(storage.effect_host()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(storage.process_env_store()))
        .store_factory(Arc::new(
            storage.session_store_factory_with_shared_process_registry(),
        ))
        .process_registry(Arc::clone(&registry) as Arc<dyn lash::process::ProcessRegistry>)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "pg-fig1294-worker",
            "pg-fig1294-boot",
        ))
        .expect("build the PostgreSQL host-ingress facade");
    let _session = core
        .session(&session_id)
        .open()
        .await
        .expect("open the PostgreSQL host-ingress session");
    let ingress = core
        .tool_intents(
            &session_id,
            lash::runtime::ExecutionScope::turn(&session_id, &scope_id),
        )
        .expect("bind the PostgreSQL host ingress");
    let key = ingress.key("pg-host-ingress-call", 0);
    let intent = || {
        lash::tools::ToolIntent::EmitProcessEvent(lash::tools::EmitProcessEventIntent {
            session_id: session_id.clone(),
            process_id: process_id.clone(),
            event_type: event_type.to_string(),
            payload: serde_json::json!({"law": "postgres-host-ingress-duplicate"}),
        })
    };

    let first = ingress.submit(key.clone(), intent()).await;
    let duplicate = ingress.submit(key, intent()).await;
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome: first_outcome,
        replayed: false,
    } = first
    else {
        panic!("the first PostgreSQL ingress submission must execute")
    };
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome: duplicate_outcome,
        replayed: true,
    } = duplicate
    else {
        panic!("the duplicate PostgreSQL ingress submission must replay")
    };
    assert_eq!(
        duplicate_outcome, first_outcome,
        "the real key-addressed journal returns the first typed outcome"
    );
    assert_eq!(
        registry
            .events_after(&process_id, 0)
            .await
            .expect("read PostgreSQL ingress events")
            .iter()
            .filter(|event| event.event_type == event_type)
            .count(),
        1,
        "the real PostgreSQL front door realizes the identity once"
    );

    let cancel_key = ingress.key("pg-host-ingress-cancel", 1);
    let cancel_intent = |reason: &str| {
        lash::tools::ToolIntent::CancelProcess(lash::tools::CancelProcessIntent {
            session_id: session_id.clone(),
            process_id: process_id.clone(),
            reason: Some(reason.to_string()),
        })
    };
    let first_cancel = ingress
        .submit(cancel_key.clone(), cancel_intent("first reason"))
        .await;
    let duplicate_cancel = ingress
        .submit(cancel_key.clone(), cancel_intent("first reason"))
        .await;
    let conflicting_cancel = ingress
        .submit(cancel_key, cancel_intent("conflicting reason"))
        .await;
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome: first_cancel_outcome,
        replayed: false,
    } = first_cancel
    else {
        panic!("the first PostgreSQL cancel ingress submission must execute")
    };
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome: duplicate_cancel_outcome,
        replayed: true,
    } = duplicate_cancel
    else {
        panic!("the duplicate PostgreSQL cancel ingress submission must replay")
    };
    assert_eq!(
        duplicate_cancel_outcome, first_cancel_outcome,
        "the real key-addressed journal returns the first cancel outcome"
    );
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash::tools::ToolIntentExecutionOutcome::Refused {
                kind: lash_core::ToolIntentKind::CancelProcess,
                refusal: lash_core::ToolIntentRefusalReason::CommandFailed { message, .. },
                ..
            },
        replayed: false,
    } = conflicting_cancel
    else {
        panic!("the conflicting PostgreSQL cancel ingress submission must be refused")
    };
    assert!(
        message.contains("postgres_effect_replay_hash_conflict")
            && message.contains("command.command.reason"),
        "the refusal must identify the canonical command conflict: {message}"
    );
    let cancel_events = registry
        .events_after(&process_id, 0)
        .await
        .expect("read PostgreSQL cancel ingress events")
        .into_iter()
        .filter(|event| event.event_type == "process.cancel_requested")
        .collect::<Vec<_>>();
    assert_eq!(cancel_events.len(), 1);
    assert_eq!(
        cancel_events[0].payload["reason"],
        serde_json::json!("first reason"),
        "the first admitted cancel payload remains authoritative"
    );
}
