use super::*;

lash_conformance::effect_host_cold_await_event_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-instance AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    (database_lock, move || {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("cold PostgreSQL effect host")
        });
        Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
    })
});

lash_conformance::effect_host_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres effect-host conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    (database_lock, move || {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("PostgreSQL effect host")
        });
        Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
    })
});

lash_conformance::cell_binding_drift_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cell binding-drift conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    // The law's host is a bare effect host with no backend; the RLM factory
    // keeps its Lashlang artifacts in a memory backend of its own.
    let artifacts = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open the artifact backend");
    let host = Arc::new(storage.effect_host());
    let faults = host.effect_journal_faults();
    let host = host as Arc<dyn EffectHost>;
    (
        database_lock,
        "postgres",
        Arc::clone(&host),
        lash_conformance::HostTurnRunner::with_journal_faults(host, faults),
        vec![rlm_factory(&artifacts, false)],
    )
});

lash_conformance::model_call_drift_park_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres model-call drift conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    // The law's host is a bare effect host with no backend; the RLM factory
    // keeps its Lashlang artifacts in a memory backend of its own.
    let artifacts = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open the artifact backend");
    let host = Arc::new(storage.effect_host()) as Arc<dyn EffectHost>;
    (
        database_lock,
        "postgres",
        Arc::clone(&host),
        lash_conformance::HostTurnRunner::shared(host),
        vec![rlm_factory(&artifacts, false)],
    )
});

lash_conformance::tool_batch_parallelism_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres tool-batch parallelism conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let host = Arc::new(storage.effect_host()) as Arc<dyn EffectHost>;
    let storage = Arc::new(storage);
    // The law's host is a bare effect host with no backend; the RLM factories
    // keep their Lashlang artifacts in a memory backend of their own.
    let artifacts = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open the artifact backend");
    (
        database_lock,
        "postgres",
        Arc::clone(&host),
        // Every producer this tier reaches: the turn's own parallel model tool
        // calls, `Promise.all` on the RLM cell bridge, and the same aggregate
        // on the process bridge.
        vec![
            lash_conformance::parallel_model_tool_calls_producer(),
            lash_conformance::rlm_promise_all_producer(vec![rlm_factory(&artifacts, false)]),
            lash_conformance::lashlang_process_aggregate_producer(
                vec![
                    rlm_factory(&artifacts, true),
                    Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
                ],
                Arc::new(move || {
                    Arc::new(storage.process_registry())
                        as Arc<dyn lash_core_execution::ProcessRegistry>
                }),
            ),
        ],
        lash_conformance::HostTurnRunner::shared(host),
    )
});

/// The RLM protocol plugin, and with it the Lashlang process engine it
/// contributes. `process_lifecycle` is this deployment's honest answer to "can
/// a cell start a process here", and it differs between the cell-bridge and
/// process-bridge producers.
fn rlm_factory(
    artifacts: &dyn lash_lashlang_runtime::LashlangArtifactBackend,
    process_lifecycle: bool,
) -> Arc<dyn lash_core_execution::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            artifacts,
        )
        .with_process_lifecycle(process_lifecycle),
    )
}

lash_conformance::turn_work_driver_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres turn-work-driver conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let host = Arc::new(storage.effect_host()) as Arc<dyn EffectHost>;
    (
        database_lock,
        host,
        lash_conformance::await_event_registration_observed,
    )
});

lash_conformance::effect_host_await_event_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres warm AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    (
        database_lock,
        move || {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("PostgreSQL effect host")
            });
            Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
        },
        lash_conformance::effect_host_journaled_wait_registration_witness,
    )
});

// The durable PostgreSQL tier answers the effect-group contract the same way
// the in-memory reference host does (FIG-1564).
lash_conformance::effect_group_host_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres effect-group conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    (database_lock, move |executors| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("PostgreSQL effect-group host")
        });
        let host = storage.effect_host();
        // Registration is what makes the host support groups at all: since
        // FIG-1578 a group carries envelopes, and what runs a child is the
        // resolver its host was built with. `None` is the unregistered host two
        // laws are about, over the same database as the wired ones.
        if let Some(executors) = executors {
            host.register_group_executors(executors)
                .expect("a freshly connected host has no resolver yet");
        }
        Arc::new(host) as Arc<dyn EffectHost>
    })
});

// A cancelled child's cancellation is journaled as its terminal, and a host
// that was not running when the close happened reads it back (FIG-1564).
lash_conformance::effect_group_cancelled_child_terminal_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cancelled-child terminal test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    (database_lock, move |executors| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("PostgreSQL effect-group host")
        });
        let host = storage.effect_host();
        if let Some(executors) = executors {
            host.register_group_executors(executors)
                .expect("a freshly connected host has no resolver yet");
        }
        Arc::new(host) as Arc<dyn EffectHost>
    })
});

// Retiring a runtime-operation scope removes its group and child rows in one
// transaction and leaves the fence, while an in-flight operation keeps every
// row (FIG-2500).
lash_conformance::effect_group_runtime_retirement_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-operation retirement test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    (
        database_lock,
        move |executors| {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("PostgreSQL effect-group host")
            });
            let host = storage.effect_host();
            if let Some(executors) = executors {
                host.register_group_executors(executors)
                    .expect("a freshly connected host has no resolver yet");
            }
            Arc::new(host) as Arc<dyn EffectHost>
        },
        move |(retired, in_flight): (String, String)| async move {
            let count = |sql: &'static str, scope_id: String| {
                let pool = storage.pool().clone();
                async move {
                    sqlx::query_scalar::<_, i64>(sql)
                        .bind(scope_id)
                        .fetch_one(&pool)
                        .await
                        .expect("count journal rows")
                }
            };
            let groups = "SELECT COUNT(*) FROM lash_runtime_effect_group WHERE scope_id = $1";
            let children = "SELECT COUNT(*) FROM lash_runtime_effect_replay WHERE scope_id = $1";
            let fences = "SELECT COUNT(*) FROM lash_effect_scope_retirements WHERE scope_id = $1";
            assert_eq!(
                count(groups, retired.clone()).await,
                0,
                "retired scope keeps no group row"
            );
            assert_eq!(
                count(children, retired.clone()).await,
                0,
                "retired scope keeps no child row"
            );
            assert_eq!(
                count(fences, retired).await,
                1,
                "retired scope leaves one fence"
            );
            assert_eq!(
                count(groups, in_flight.clone()).await,
                1,
                "in-flight scope keeps its group"
            );
            assert_eq!(
                count(children, in_flight.clone()).await,
                2,
                "in-flight scope keeps its children"
            );
            assert_eq!(
                count(fences, in_flight).await,
                0,
                "in-flight scope is not fenced"
            );
        },
    )
});

/// The turn-driving laws' fixture: a reset database's effect host and process
/// registry, a native process-work substrate over that registry, and a runner
/// that scopes each turn on the same host.
type PostgresTurnRunnerFixture = (
    SharedDatabaseLock,
    &'static str,
    Arc<dyn EffectHost>,
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core_execution::ProcessWorkSubstrate>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
    fn(&'static str) -> std::future::Ready<()>,
);

async fn postgres_turn_runner_fixture() -> Option<PostgresTurnRunnerFixture> {
    let (database_lock, storage) = storage().await?;
    reset(storage.pool()).await;
    let host = Arc::new(storage.effect_host()) as Arc<dyn EffectHost>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry),
    )) as Arc<dyn lash_core_execution::ProcessWorkSubstrate>;
    let runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&host));
    Some((
        database_lock,
        "postgres-turn-runner",
        host,
        registry,
        process_work,
        runner,
        // The Postgres host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    ))
}

lash_conformance::turn_runner_tests!({
    let Some(fixture) = postgres_turn_runner_fixture().await else {
        eprintln!(
            "skipping Postgres turn-runner conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    fixture
});

lash_conformance::tool_child_turn_cancel_tests!({
    let Some(fixture) = postgres_turn_runner_fixture().await else {
        eprintln!(
            "skipping Postgres tool-child turn-cancel conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    fixture
});

// The orchestration plugins sit above lash-conformance, so the tier supplies
// them to the FIG-1293 migrated-tools law.
lash_conformance::migrated_tools_redrive_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres migrated-tools conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let host = Arc::new(storage.effect_host()) as Arc<dyn EffectHost>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&host));
    let orchestration: Vec<Arc<dyn lash_core_execution::facade_support::PluginFactory>> = vec![
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
        Arc::new(lash_subagents::SubagentsPluginFactory::new(Arc::new(
            lash_subagents::CapabilityRegistry::new().with(Arc::new(
                lash_subagents::StaticCapability::new(
                    "default",
                    lash_core_execution::facade_support::SessionSpec::inherit(),
                ),
            )),
        ))),
    ];
    (
        database_lock,
        "postgres-migrated-tools",
        host,
        registry,
        runner,
        orchestration,
    )
});

lash_conformance::backend_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres backend conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(storage.pool()).await;
    let attachments = tempfile::tempdir().expect("attachment root");
    let backend = Arc::new(lash_postgres_store::PostgresBackend::new(
        &storage,
        Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(attachments.path())),
    )) as Arc<dyn lash_core_execution::Backend>;
    ((database_lock, attachments), backend)
});
