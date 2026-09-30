//! PostgreSQL ingress laws on the same Restate recording controller as the
//! SQLite laws. PostgreSQL supplies storage only; the controller journals
//! acceptance and scopes each runtime turn.

use super::*;
use sqlx::{Connection as _, PgConnection};

const DATABASE_LOCK_KEY: i64 = 0x4c41_5348_5f50_4754;

struct DatabaseLock {
    _connection: PgConnection,
}

impl DatabaseLock {
    async fn acquire(url: &str) -> Self {
        let mut connection = PgConnection::connect(url)
            .await
            .expect("connect PostgreSQL law lock");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(DATABASE_LOCK_KEY)
            .execute(&mut connection)
            .await
            .expect("acquire PostgreSQL law lock");
        sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
            .execute(&mut connection)
            .await
            .expect("provision PostgreSQL law schema");
        Self {
            _connection: connection,
        }
    }
}

async fn reset(pool: &sqlx::PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity', 'lash_fleet_format',
                                 'lash_migrations')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list PostgreSQL law tables");
    assert!(!tables.is_empty(), "the PostgreSQL schema is provisioned");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(pool)
    .await
    .expect("reset PostgreSQL law tables");
    for table in [
        "lash_process_change_clock",
        "lash_turn_park_clock",
        "lash_process_park_clock",
    ] {
        sqlx::query(&format!(
            "INSERT INTO {table} (singleton, current_seq) VALUES (TRUE, 0)
             ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq"
        ))
        .execute(pool)
        .await
        .expect("reset PostgreSQL law clock");
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the service-gated test fixture reads its injected PostgreSQL URL"
)]
fn database_url() -> Option<String> {
    match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL is required for the PostgreSQL ingress laws")
        }
        _ => None,
    }
}

async fn backend_for(
    session_id: &str,
) -> Option<(
    (DatabaseLock, tempfile::TempDir),
    lash_core::Backend,
    Arc<dyn lash_core::RuntimeStore>,
)> {
    let url = database_url()?;
    let lock = DatabaseLock::acquire(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(&url)
        .await
        .expect("connect PostgreSQL ingress law store");
    reset(storage.pool()).await;
    let attachments = tempfile::tempdir().expect("attachment directory");
    let stores: Arc<dyn lash_core::StoreSet> =
        Arc::new(lash_postgres_store::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                attachments.path(),
            )),
        ));
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(
        Arc::new(RecordingContext::default()),
    ));
    let backend = lash_conformance::backend_over(Arc::clone(&stores), host);
    let store: Arc<dyn lash_core::RuntimeStore> = Arc::clone(
        lash_core::runtime::admit_session_view(
            &stores.session_store_factory(),
            &lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(session_id),
                relation: lash_core::SessionRelation::Root,
                config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
                head: lash_core::SessionCreationHead::CommittedByCreator,
            },
        )
        .await
        .expect("create PostgreSQL ingress law session")
        .store(),
    );
    Some(((lock, attachments), backend, store))
}

mod direct_turn_acceptance {
    use super::*;

    lash_conformance::direct_turn_acceptance_tests!(
        #[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
        {
            let Some((guard, backend, store)) = backend_for("root").await else {
                eprintln!("skipping PostgreSQL ingress law: database is not configured");
                return;
            };
            (guard, "postgres", backend, store)
        }
    );
}

mod usage_accounting {
    use super::super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
    use super::super::usage_accounting_on_the_double::SettleCrashes;
    use super::*;

    async fn tier() -> (impl Send, lash_conformance::UsageAccountingTier) {
        let url = database_url().expect("usage laws require the PostgreSQL service");
        let lock = DatabaseLock::acquire(&url).await;
        let storage = lash_postgres_store::PostgresStorage::connect(&url)
            .await
            .expect("connect PostgreSQL accounting law store");
        reset(storage.pool()).await;
        let attachments = tempfile::tempdir().expect("attachment directory");
        let stores: Arc<dyn lash_core::StoreSet> =
            Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachments.path(),
                )),
            ));
        let harness = LiveConformanceHarness::start_for_tool_children_settling_into(
            HarnessServer::in_process(),
            stores.usage_accounting(),
        )
        .await;
        let server = harness
            .server_double()
            .expect("PostgreSQL storage runs on the double");
        let tier = lash_conformance::UsageAccountingTier {
            prefix: format!("postgres-usage-{}", harness.run_nonce()),
            effect_host: harness.endpoint_host(),
            stores,
            runner: harness.turn_runner(),
            continuation: Arc::new(SettleCrashes { server }),
        };
        ((lock, attachments, harness), tier)
    }

    lash_conformance::usage_accounting_engine_tests!(
        #[ignore = "PostgreSQL usage-accounting service gate"]
        {
            tier().await
        }
    );

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "PostgreSQL usage-accounting service gate"]
    async fn a_second_core_reads_build_drift_parked_usage() {
        let (_guard, tier) = tier().await;
        super::super::usage_accounting_on_the_double::read_parked_usage_from_second_core(&tier)
            .await;
    }
}

mod cancelled_turn_withheld_input {
    use super::*;

    lash_conformance::cancelled_turn_withheld_input_tests!(
        #[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
        {
            let Some((guard, backend, store)) =
                backend_for(lash_conformance::CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID).await
            else {
                eprintln!("skipping PostgreSQL ingress law: database is not configured");
                return;
            };
            (guard, "postgres", backend, store)
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
async fn process_start_store_refusals_and_transient_faults_on_postgres() {
    use super::process_start_store_refusals::{Fault, Step, server_config, start_store_fault_law};

    let url = database_url().expect("the PostgreSQL start laws require a database");
    let _lock = DatabaseLock::acquire(&url).await;
    for seed in 0x4204_0000..0x4204_0014 {
        for step in [Step::Claim, Step::Settle] {
            for fault in [Fault::WriterFenced, Fault::Incompatible, Fault::Transient] {
                let storage = lash_postgres_store::PostgresStorage::connect(&url)
                    .await
                    .expect("connect PostgreSQL start law store");
                reset(storage.pool()).await;
                let attachments = tempfile::tempdir().expect("attachment directory");
                let backend = lash_restate_test::backend_with_store_set(
                    seed,
                    server_config(),
                    lash_restate_test::DeploymentHooks::default(),
                    |clock| async {
                        Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                            &storage,
                            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                                attachments.path(),
                            )),
                            lash_core::WakeDeliveryConfig::default(),
                            clock,
                        )) as Arc<dyn lash_core::StoreSet>)
                    },
                )
                .await
                .expect("the Restate SDK over PostgreSQL");
                start_store_fault_law(&backend, step, fault).await;
                eprintln!(
                    "start-store-fault PostgreSQL/Restate {step:?} {fault:?} seed={seed:x} PASS"
                );
            }
        }
    }
}

/// FIG-4272 on PostgreSQL: a reattached trigger emission reports the
/// deliveries its committed attempt started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
async fn a_reattached_emission_reports_the_deliveries_its_committed_attempt_started_on_postgres() {
    use super::trigger_emit_reattach_on_the_double::{
        SEEDS, a_reattached_emission_reports_the_deliveries_its_committed_attempt_started,
    };

    let url = database_url().expect("the PostgreSQL reattach law requires a database");
    let _lock = DatabaseLock::acquire(&url).await;
    for seed in SEEDS {
        let storage = lash_postgres_store::PostgresStorage::connect(&url)
            .await
            .expect("connect the PostgreSQL reattach law store");
        reset(storage.pool()).await;
        let attachments = tempfile::tempdir().expect("attachment directory");
        let backend = lash_restate_test::backend_with_store_set(
            seed,
            lash_restate_test::ServerConfig::default(),
            lash_restate_test::DeploymentHooks::default(),
            |clock| async {
                Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                    &storage,
                    Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                        attachments.path(),
                    )),
                    lash_core::WakeDeliveryConfig::default(),
                    clock,
                )) as Arc<dyn lash_core::StoreSet>)
            },
        )
        .await
        .expect("the Restate SDK over PostgreSQL");
        a_reattached_emission_reports_the_deliveries_its_committed_attempt_started(&backend, seed)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PostgreSQL service leg: kiln gate with pg16"]
async fn remote_after_step_waits_for_committed_boundary_postgres() {
    let url = database_url().expect("PostgreSQL service is required");
    let _lock = DatabaseLock::acquire(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(&url)
        .await
        .expect("connect PostgreSQL");
    reset(storage.pool()).await;
    let attachments = tempfile::tempdir().expect("attachment directory");
    let double = lash_restate_test::backend_with_store_set(
        0x4282,
        lash_restate_test::ServerConfig::default(),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async {
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachments.path(),
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("start the PostgreSQL Restate double");
    super::remote_turn_cancel::held_step_law(
        double.lash_backend(),
        super::remote_turn_cancel::TurnRunner::Double(double),
        "remote-postgres",
    )
    .await;
}

/// A process registry over a freshly reset PostgreSQL database, and the
/// attachment directory its store set holds.
async fn postgres_process_registry(url: &str) -> (Arc<dyn ProcessRegistry>, tempfile::TempDir) {
    let storage = lash_postgres_store::PostgresStorage::connect(url)
        .await
        .expect("connect PostgreSQL signal admission store");
    reset(storage.pool()).await;
    let attachments = tempfile::tempdir().expect("attachment directory");
    let stores = lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    );
    (stores.process_registry(), attachments)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
async fn duplicate_signal_after_wait_advances_does_not_resolve_next_wait_on_postgres() {
    use super::process_signal_admission::{
        duplicate_signal_after_append_to_journal_crash_law,
        duplicate_signal_after_wait_advances_law,
    };

    let url = database_url().expect("the PostgreSQL signal laws require a database");
    let _lock = DatabaseLock::acquire(&url).await;
    let (registry, _attachments) = postgres_process_registry(&url).await;
    duplicate_signal_after_wait_advances_law(registry).await;
    let (registry, _attachments) = postgres_process_registry(&url).await;
    duplicate_signal_after_append_to_journal_crash_law(registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PostgreSQL service leg: scripts/ci/store-tests.sh pg-store"]
async fn partial_signal_replay_rejects_changed_request_before_resolution_on_postgres() {
    use super::process_signal_admission::partial_signal_replay_rejects_changed_request_law;

    let url = database_url().expect("the PostgreSQL signal laws require a database");
    let _lock = DatabaseLock::acquire(&url).await;
    let (registry, _attachments) = postgres_process_registry(&url).await;
    partial_signal_replay_rejects_changed_request_law(registry).await;
}
