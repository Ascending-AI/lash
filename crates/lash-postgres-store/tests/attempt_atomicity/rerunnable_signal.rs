use super::*;

/// Race-free control geometry for the FIG-1293 signal path: the target is
/// worker-owned (`Rerunnable`), but this focused host deliberately installs no
/// process worker. The law therefore retains the production disposition
/// without allowing a worker's `first_started` write to race the literal
/// observer/signal sequence pinned below.
#[tokio::test(flavor = "multi_thread")]
async fn public_provider_signal_intent_retains_rerunnable_target_geometry_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL Rerunnable signal law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL Rerunnable signal host");
    reset(&storage).await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "pg-public-intent-target",
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "fig1293-rerunnable-control"}),
                },
                lash_core::RecoveryDisposition::Rerunnable,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SESSION.to_string()],
        )
        .await
        .expect("register PostgreSQL Rerunnable signal target");

    let wait_controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        storage.runtime_effect_controller(ExecutionScope::process("pg-public-intent-target")),
    );
    let wake_key = wait_controller
        .await_event_key(
            &ExecutionScope::process("pg-public-intent-target"),
            lash_core::AwaitEventWaitIdentity::process_signal(
                "pg-public-intent-target",
                "resume",
                1,
            ),
        )
        .await
        .expect("mint Rerunnable process-signal wait");
    let wake = tokio::spawn(async move {
        wait_controller
            .await_await_event(&wake_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let effect_host = Arc::new(storage.effect_host());
    let mut runtime = public_signal_runtime(
        effect_host,
        Arc::clone(&registry),
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::Signal,
    )
    .await;
    let turn = runtime
        .stream_turn(
            public_runtime_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                postgres_public_turn_scope(&storage, Arc::new(Mutex::new(Vec::new()))),
            ),
        )
        .await
        .expect("run Rerunnable signal-intent turn");
    assert!(matches!(
        turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), wake)
            .await
            .expect("Rerunnable signal must wake")
            .expect("Rerunnable wake task")
            .expect("Rerunnable wake resolution"),
        lash_core::Resolution::Ok(serde_json::json!({
            "source": "postgres-public-caller"
        }))
    );

    let record = registry
        .get_process("pg-public-intent-target")
        .await
        .expect("read Rerunnable target")
        .expect("Rerunnable target exists");
    assert_eq!(
        record.disposition,
        lash_core::RecoveryDisposition::Rerunnable
    );
    assert!(
        record.first_started.is_none(),
        "the focused law installs no worker, so scheduler timing cannot alter the event sequence"
    );
    let events = registry
        .events_after("pg-public-intent-target", 0)
        .await
        .expect("read Rerunnable target events");
    assert_eq!(
        events
            .iter()
            .map(|event| (event.sequence, event.event_type.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "process.observer_added"), (2, "signal.resume")]
    );

    reset(&storage).await;
}
