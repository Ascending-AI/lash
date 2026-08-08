use std::future::Future;
use std::sync::Arc;

use lash_core::testing::conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    ReopenableProcessRegistry, ReopenableRuntimePersistence, ReopenableTriggerStore,
};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    ProcessExecutionEnvStore, ProcessRegistry, QueuedWorkStore, Resolution, ResolveOutcome,
    RuntimePersistence, SessionExecutionLeaseStore, SessionStoreFactory, StoreError, TriggerStore,
};
use lash_postgres_store::{
    PostgresEffectReplayOptions, PostgresRuntimeEffectController, PostgresStorage,
    PostgresStoreConfig,
};

mod support;

#[path = "../../lash-core/tests/support/cold_process_turn_parent.rs"]
mod cold_process_turn_parent;

use support::{SharedDatabaseLock, database_url};

fn sync_await<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    // Drive the future on the CURRENT (multi-thread) test runtime rather than a
    // throwaway one. The sqlx pool's connections are bound to this runtime's
    // reactor; polling them from a different runtime wedges the connection (it
    // never returns to the pool), which starves the pool and surfaces as
    // PoolTimedOut. `block_in_place` lets this worker block while tokio spins up a
    // replacement, so the conformance harness keeps making progress.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

async fn reset(storage: &PostgresStorage) {
    let pool = storage.pool();
    // Derive the truncate set from the live catalog rather than hand-maintaining
    // a table list: a new `lash_*` table can no longer silently bleed state
    // between conformance cases. `lash_schema_versions` is excluded — it holds
    // the component schema version gate, not per-case fixture rows.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* conformance tables");
    assert!(
        !tables.is_empty(),
        "expected the lash_* schema tables to exist before reset"
    );
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(pool)
        .await
        .expect("reset postgres conformance tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process change clock");
}

struct PostgresFenceIntegrityInjector {
    _database_lock: SharedDatabaseLock,
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl FenceIntegrityInjector for PostgresFenceIntegrityInjector {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
        let result = match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => {
                sqlx::query(
                    "UPDATE lash_queued_work_batches
                 SET claim_fencing_token = $1 WHERE batch_id = $2",
                )
                .bind(value)
                .bind(batch_id)
                .execute(self.storage.pool())
                .await
            }
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                sqlx::query("UPDATE lash_sessions SET head_revision = $1 WHERE session_id = $2")
                    .bind(value)
                    .bind(session_id)
                    .execute(self.storage.pool())
                    .await
            }
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => {
                sqlx::query(
                    "UPDATE lash_session_execution_leases
                 SET lease_fencing_token = $1 WHERE session_id = $2",
                )
                .bind(value)
                .bind(session_id)
                .execute(self.storage.pool())
                .await
            }
            FenceIntegrityTarget::TriggerRevision { subscription_id } => {
                sqlx::query(
                    "UPDATE lash_trigger_subscriptions
                 SET revision = $1,
                     record_json = jsonb_set(
                         record_json::jsonb,
                         '{revision}',
                         to_jsonb($1::bigint)
                     )::text
                 WHERE subscription_id = $2",
                )
                .bind(value)
                .bind(subscription_id)
                .execute(self.storage.pool())
                .await
            }
        }
        .expect("inject raw Postgres fence value");
        assert_eq!(
            result.rows_affected(),
            1,
            "raw Postgres fence injection must target one row"
        );
    }

    async fn observe_raw_value(&self, target: &FenceIntegrityTarget) -> FenceIntegrityObservation {
        match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => {
                let (value, claim_id, claim_token, generation): (
                    i64,
                    Option<String>,
                    Option<String>,
                    i64,
                ) = sqlx::query_as(
                    "SELECT claim_fencing_token, claim_id, claim_token,
                            claim_session_lease_generation
                     FROM lash_queued_work_batches WHERE batch_id = $1",
                )
                .bind(batch_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres queued-work fence");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{claim_id:?}:{claim_token:?}:{generation}"),
                }
            }
            FenceIntegrityTarget::SessionHeadRevision { session_id } => {
                let (value, head_json, leaf, checkpoint): (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                ) = sqlx::query_as(
                    "SELECT head_revision, head_json, leaf_node_id, checkpoint_ref
                     FROM lash_sessions WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres session-head revision");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{head_json}:{leaf:?}:{checkpoint:?}"),
                }
            }
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => {
                let (value, owner, token, claimed, expires): (
                    i64,
                    Option<String>,
                    Option<String>,
                    i64,
                    i64,
                ) = sqlx::query_as(
                    "SELECT lease_fencing_token, lease_owner_id, lease_token,
                            lease_claimed_at_ms, lease_expires_at_ms
                     FROM lash_session_execution_leases WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres session-lease fence");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{owner:?}:{token:?}:{claimed}:{expires}"),
                }
            }
            FenceIntegrityTarget::TriggerRevision { subscription_id } => {
                let (value, json, enabled, tombstoned): (i64, String, bool, bool) = sqlx::query_as(
                    "SELECT revision, record_json, enabled, tombstoned
                         FROM lash_trigger_subscriptions WHERE subscription_id = $1",
                )
                .bind(subscription_id)
                .fetch_one(self.storage.pool())
                .await
                .expect("observe Postgres trigger revision");
                FenceIntegrityObservation {
                    value,
                    mutation_fingerprint: format!("{json}:{enabled}:{tombstoned}"),
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_fence_integrity_conformance_when_configured() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres fence-integrity conformance: database is not configured");
        return;
    };
    lash_core::testing::conformance::fence_integrity_conformance(|_| {
        let database_url = database_url.clone();
        async move {
            let database_lock = SharedDatabaseLock::acquire(&database_url).await;
            let storage = Arc::new(
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("open Postgres fence fixture"),
            );
            reset(&storage).await;
            FenceIntegrityHandles {
                runtime: Arc::new(storage.unbound_session_store()),
                triggers: Arc::new(storage.trigger_store()),
                injector: Arc::new(PostgresFenceIntegrityInjector {
                    _database_lock: database_lock,
                    storage,
                }),
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_signed_counter_write_domain_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres signed-write conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::signed_counter_write_domain_conformance(Arc::new(
        storage.unbound_session_store(),
    ))
    .await;
}

async fn wait_for_session_lease_advisory_waiters(pool: &sqlx::PgPool, at_least: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiters: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)
                 FROM pg_stat_activity
                 WHERE wait_event_type = 'Lock'
                   AND wait_event = 'advisory'
                   AND query LIKE '%pg_advisory_xact_lock(hashtextextended($1, 0::bigint))%'",
            )
            .fetch_one(pool)
            .await
            .expect("inspect session-lease advisory-lock waiters");
            if waiters >= at_least {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected at least {at_least} session-lease advisory-lock waiters"));
}

fn durable_turn_scope(session_id: impl Into<String>, turn_id: impl Into<String>) -> ExecutionScope {
    let session_id = session_id.into();
    ExecutionScope::turn(&session_id, turn_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_graph_node_primary_key_is_global_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres global node-id schema test: database is not configured");
        return;
    };
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE conrelid = 'lash_graph_nodes'::regclass
           AND contype = 'p'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read graph-node primary key");

    assert_eq!(definition, "PRIMARY KEY (node_id)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_persistence_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    let clock = Arc::new(lash_core::testing::TestClock::new(10_000));
    lash_core::testing::conformance::runtime_persistence_reopenable(
        || {
            let storage = Arc::clone(&storage);
            let database_url = database_url.clone();
            let clock = Arc::clone(&clock);
            sync_await(async move {
                reset(&storage).await;
                let open_storage = PostgresStorage::connect(&database_url)
                    .await
                    .expect("open first Postgres conformance pool");
                let reopen_storage = PostgresStorage::connect(&database_url)
                    .await
                    .expect("open independent Postgres conformance pool");
                ReopenableRuntimePersistence {
                    open: Arc::new(
                        open_storage
                            .unbound_session_store()
                            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
                    ),
                    reopen: Arc::new(
                        reopen_storage
                            .unbound_session_store()
                            .with_clock(clock as Arc<dyn lash_core::Clock>),
                    ),
                }
            })
        },
        lash_core::testing::conformance::RuntimePersistenceLeaseTiming::Realtime,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_negative_and_exhausted_queued_work_fences_are_typed_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres fence corruption test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(&storage).await;
    let session_id = "postgres-fence-corrupt";
    let store = storage.session_store(session_id);
    let owner = lash_core::LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
    let lease = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            &lash_core::LeaseClaimNonce::new(),
            120_000,
        )
        .await
        .expect("claim session lease")
        .acquired()
        .expect("session lease acquired");
    let batch = store
        .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
            session_id,
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            lash_core::SlotPolicy::Exclusive,
            vec![lash_core::runtime::QueuedWorkPayload::session_command(
                lash_core::runtime::SessionCommand::RefreshToolCatalog {
                    reason: "fence test".to_string(),
                },
            )],
        ))
        .await
        .expect("enqueue queued work");

    sqlx::query("UPDATE lash_queued_work_batches SET claim_fencing_token = -1 WHERE batch_id = $1")
        .bind(&batch.batch_id)
        .execute(storage.pool())
        .await
        .expect("inject negative fence");
    let corrupt = store
        .list_queued_work(session_id)
        .await
        .expect_err("negative fence must refuse");
    assert!(matches!(
        corrupt,
        StoreError::StoredDataCorrupt {
            record_kind: "QueuedWorkBatch",
            ..
        }
    ));

    sqlx::query("UPDATE lash_queued_work_batches SET claim_fencing_token = $1 WHERE batch_id = $2")
        .bind(i64::MAX)
        .bind(&batch.batch_id)
        .execute(storage.pool())
        .await
        .expect("seed exhausted fence");
    let exhausted = store
        .claim_leading_ready_session_command(session_id, &lease.authority(), &owner)
        .await
        .expect_err("exhausted SQL fence must refuse");
    assert!(matches!(
        exhausted,
        StoreError::MonotonicCounterOverflow {
            counter: "queued_work_claim_fencing_token",
            current,
        } if current == i64::MAX as u64
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_store_enforces_core_lease_fence_authority_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres lease-fence conformance: database URL is not set");
        return;
    };
    reset(&storage).await;
    let store = storage.unbound_session_store();
    lash_core::testing::conformance::session_execution_lease_fence_authority(&store).await;
}

/// Pins the PostgreSQL-local hardening rule that claims and renewals join the
/// same per-session advisory-lock queue before taking the lease-row lock. The
/// claim is queued first, so it legally rotates token A to token B before the
/// predecessor renewal runs and receives the named refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_claim_and_renewal_share_session_advisory_lock_ordering() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres concurrent renewal regression: database is not configured");
        return;
    };
    reset(&storage).await;
    let store = Arc::new(storage.unbound_session_store());
    let session_id = "postgres-concurrent-renewal-rotation";
    let owner = lash_core::LeaseOwnerIdentity::opaque("renewal-owner", "renewal-incarnation");
    let predecessor = store
        .try_claim_session_execution_lease(session_id, &owner, 120_000)
        .await
        .expect("claim renewal predecessor")
        .acquired()
        .expect("renewal predecessor acquired");

    let mut blocker = storage
        .pool()
        .begin()
        .await
        .expect("begin advisory blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0::bigint))")
        .bind(session_id)
        .execute(&mut *blocker)
        .await
        .expect("hold session-lease advisory lock");

    let claim_store = Arc::clone(&store);
    let claim_owner = owner.clone();
    let claim = tokio::spawn(async move {
        claim_store
            .try_claim_session_execution_lease(session_id, &claim_owner, 120_000)
            .await
    });
    wait_for_session_lease_advisory_waiters(storage.pool(), 1).await;

    let renew_store = Arc::clone(&store);
    let predecessor_fence = predecessor.fence();
    let mut renewal = tokio::spawn(async move {
        renew_store
            .renew_session_execution_lease(&predecessor_fence, 120_000)
            .await
    });
    tokio::select! {
        result = &mut renewal => {
            panic!("renewal did not wait on the shared session advisory lock: {result:?}")
        }
        () = wait_for_session_lease_advisory_waiters(storage.pool(), 2) => {}
    }

    blocker
        .rollback()
        .await
        .expect("release session-lease advisory blocker");
    let successor = claim
        .await
        .expect("join rotating claim")
        .expect("rotating claim")
        .acquired()
        .expect("same-incarnation rotating claim acquired");
    assert_ne!(successor.lease_token, predecessor.lease_token);
    let renewal_error = renewal
        .await
        .expect("join stale renewal")
        .expect_err("renewal queued after rotation must be refused");
    assert!(matches!(
        renewal_error,
        StoreError::SessionExecutionLeaseRenewalRefused { .. }
    ));
    let durable = store
        .get_session_execution_lease(session_id)
        .await
        .expect("read successor lease")
        .expect("successor lease remains live");
    assert_eq!(durable.lease_token, successor.lease_token);
    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("release successor lease");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_persistence_recovery_laws_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres store-recovery laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    lash_core::testing::conformance::runtime_persistence_recovery_laws(|_| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("construct fresh Postgres store-recovery pool")
        });
        Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_real_turn_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres real-turn crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    Box::pin(lash_core::testing::conformance::turn_crash_matrix_level_1(
        |scenario| {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("construct fresh Postgres real-turn crash-matrix pool")
            });
            Arc::new(storage.session_store(format!("trace-derived-real-turn:{scenario}")))
                as Arc<dyn RuntimePersistence>
        },
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_checkpoint_component_refs_survive_cold_reopens_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres checkpoint-component recovery: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    lash_core::testing::conformance::checkpoint_component_refs_survive_cold_reopens(|| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("construct post-write Postgres checkpoint pool")
        });
        Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_turn_receipt_identity_columns_are_nullable_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres receipt-schema test: database is not configured");
        return;
    };
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'lash_runtime_turn_commits'
           AND column_name = ANY($1)",
    )
    .bind(
        &[
            "request_identity_hash",
            "requested_node_count",
            "requested_ancestor_node_id",
            "identity_encoding_version",
        ][..],
    )
    .fetch_all(storage.pool())
    .await
    .expect("read Postgres receipt schema");
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|(_, nullable)| nullable == "YES"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_append_receipt_replays_after_ancestor_superseded_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres append-receipt conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    lash_core::testing::conformance::append_request_receipt_replays_after_ancestor_superseded(
        Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
        move |leaf_node_id| async move {
            sqlx::query(
                "UPDATE lash_sessions
                 SET leaf_node_id = $1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
            )
            .bind(leaf_node_id)
            .execute(&pool)
            .await
            .expect("switch Postgres active branch");
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_inactive_append_ancestor_precedes_stale_head_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres append-precedence conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    lash_core::testing::conformance::inactive_append_ancestor_precedes_stale_head(
        Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
        move |leaf_node_id| async move {
            sqlx::query(
                "UPDATE lash_sessions
                 SET leaf_node_id = $1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
            )
            .bind(leaf_node_id)
            .execute(&pool)
            .await
            .expect("switch Postgres active branch");
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_tombstoned_old_leaf_is_rejected_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres tombstoned-leaf conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    lash_core::testing::conformance::tombstoned_old_leaf_is_rejected(
        Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
        move |node_id| async move {
            sqlx::query("UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = $1")
                .bind(node_id)
                .execute(&pool)
                .await
                .expect("tombstone Postgres old leaf");
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_append_receipt_restores_mixed_usage_envelope_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres mixed-envelope receipt conformance: database is not configured"
        );
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::append_receipt_mixed_usage_envelope(Arc::new(
        storage.unbound_session_store(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_old_format_append_receipt_returns_public_leaf_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres old-format receipt conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    lash_core::testing::conformance::old_format_append_receipt_returns_public_leaf(
        Arc::new(storage.unbound_session_store()),
        move || async move {
            sqlx::query(
                "UPDATE lash_runtime_turn_commits
                 SET result_json = ((result_json::jsonb
                     - 'committed_leaf_node_id'
                     - 'receipt_replayed')::text)
                 WHERE turn_id LIKE '%old-format-append-receipt%'
                   AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
            )
            .execute(&pool)
            .await
            .expect("install raw pre-upgrade Postgres receipt fixture");
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_artifact_store_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres artifact-store conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_lashlang_runtime::testing::conformance::artifact_store_reopenable(|| {
        let storage = Arc::clone(&storage);
        let database_url = database_url.clone();
        sync_await(async move {
            reset(&storage).await;
            let open_storage = PostgresStorage::connect(&database_url)
                .await
                .expect("open first Postgres artifact pool");
            let open = lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
                artifacts: Arc::new(open_storage.lashlang_artifact_store())
                    as Arc<dyn lashlang::LashlangArtifactStore>,
                process_env: Arc::new(open_storage.process_env_store())
                    as Arc<dyn ProcessExecutionEnvStore>,
            };
            let reopen_url = database_url.clone();
            lash_lashlang_runtime::testing::conformance::ReopenableArtifactStore {
                open,
                reopen: Arc::new(move || {
                    let reopen_url = reopen_url.clone();
                    let reopened = sync_await(async move {
                        PostgresStorage::connect(&reopen_url)
                            .await
                            .expect("construct post-write Postgres artifact pool")
                    });
                    lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
                        artifacts: Arc::new(reopened.lashlang_artifact_store())
                            as Arc<dyn lashlang::LashlangArtifactStore>,
                        process_env: Arc::new(reopened.process_env_store())
                            as Arc<dyn ProcessExecutionEnvStore>,
                    }
                }),
            }
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_store_factory_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-store-factory conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::session_store_factory(
        || {
            let storage = Arc::clone(&storage);
            sync_await(async move {
                reset(&storage).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>
            })
        },
        || Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_fork_observer_intent_transient_failure_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres fork-observer intent conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::fork_observer_intent_transient_failure(Arc::new(
        storage.session_store_factory(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_graph_append_branch_liveness_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-graph append branch-liveness conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::session_graph_append_branch_liveness(Arc::new(
        storage.session_store_factory(),
    )
        as Arc<dyn SessionStoreFactory>)
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_wake_delivery_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake-delivery crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let factory = Arc::new(
        storage
            .session_store_factory()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn SessionStoreFactory>;
    let registry = Arc::new(
        storage
            .process_registry_with_wake_delivery_config(
                lash_core::WakeDeliveryConfig::new(10_000)
                    .expect("valid test retention")
                    .with_enqueuing_stale_after_ms(25)
                    .expect("valid short stale-claim age"),
            )
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn ProcessRegistry>;
    Box::pin(lash_core::testing::conformance::wake_delivery_crash_matrix(
        factory, registry, clock,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_wake_enqueue_serializes_with_consumption_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake enqueue interleaving test: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let factory = storage.session_store_factory();
    let session_id = "wake-source-lock-target";
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create source-lock target");
    let wake = lash_core::ProcessWakeDelivery {
        wake_id: "wake:source-lock".to_string(),
        target_session_id: session_id.to_string(),
        process_id: "wake-source-lock-process".to_string(),
        sequence: 1,
        event_type: "producer.wake".to_string(),
        event_invocation: lash_core::RuntimeInvocation::effect(
            lash_core::RuntimeScope::new(session_id),
            "wake-source-lock",
            lash_core::RuntimeEffectKind::Process,
            "wake-source-lock",
        ),
        process_caused_by: None,
        input: "wake".to_string(),
        created_at_ms: lash_core::Clock::timestamp_ms(&lash_core::facade_support::SystemClock),
    };
    let draft = lash_core::runtime::process_wake_batch_draft(wake.clone());
    let first = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect("enqueue original wake");
    let owner = lash_core::LeaseOwnerIdentity::opaque("wake-source-lock", "test");
    let lease = match store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim target session")
    {
        lash_core::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
        lash_core::SessionExecutionLeaseClaimOutcome::Busy { .. } => {
            panic!("fresh source-lock target lease must be available")
        }
    };
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &lease.fence(),
            &owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
        )
        .await
        .expect("claim source-lock wake")
        .expect("source-lock wake claim");

    let source_key = draft.source_key.as_deref().expect("wake source key");
    let mut source_blocker = storage.pool().begin().await.expect("begin source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(source_key)
    .execute(&mut *source_blocker)
    .await
    .expect("take source-only advisory lock");
    let redelivery_store = Arc::clone(&store);
    let redelivery_draft = draft.clone();
    let redelivery =
        tokio::spawn(async move { redelivery_store.enqueue_queued_work(redelivery_draft).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !redelivery.is_finished(),
        "enqueue must block on the independently-held source lock"
    );
    source_blocker
        .rollback()
        .await
        .expect("release source-only enqueue blocker");
    redelivery
        .await
        .expect("join source-blocked redelivery")
        .expect("redelivery returns the live batch");

    let mut completion_source_blocker = storage
        .pool()
        .begin()
        .await
        .expect("begin completion source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(source_key)
    .execute(&mut *completion_source_blocker)
    .await
    .expect("take completion source-only advisory lock");
    let completion_store = Arc::clone(&store);
    let completion = tokio::spawn(async move {
        let state = lash_core::RuntimeSessionState {
            session_id: session_id.to_string(),
            ..lash_core::RuntimeSessionState::default()
        };
        completion_store
            .commit_runtime_state(
                lash_core::RuntimeCommit::persisted_state_for_test(&state, &[])
                    .completing_queue_claim(claim.completion())
                    .releasing_session_execution_lease(lease.completion()),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !completion.is_finished(),
        "consumption must block on the independently-held source lock"
    );
    completion_source_blocker
        .rollback()
        .await
        .expect("release source-only completion blocker");
    completion
        .await
        .expect("join wake consumption")
        .expect("consume wake after source lock release");
    assert!(
        store
            .list_queued_work(session_id)
            .await
            .expect("list queue after forced interleaving")
            .iter()
            .all(|batch| batch.source_key.as_deref() != draft.source_key.as_deref()),
        "forced evidence-check/drain/live-check interleaving must not recreate the wake"
    );
    let late_redelivery = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect_err("a no-live-row wake at the receiver floor is a typed rewind");
    assert!(matches!(
        late_redelivery,
        lash_core::StoreError::ProcessWakeSequenceRewound {
            sequence: 1,
            allocation_floor: 1,
            ..
        }
    ));
    assert!(
        store
            .list_queued_work(session_id)
            .await
            .expect("list queue after late redelivery")
            .iter()
            .all(|batch| batch.source_key.as_deref() != draft.source_key.as_deref())
    );

    let bounded_config = PostgresStoreConfig {
        lock_timeout: Some(std::time::Duration::from_millis(50)),
        ..PostgresStoreConfig::default()
    };
    let bounded_storage = PostgresStorage::connect_with(
        &database_url().expect("configured Postgres URL"),
        bounded_config,
    )
    .await
    .expect("connect storage with short lock timeout");
    let bounded_store = bounded_storage.unbound_session_store();
    let mut timeout_wake = wake;
    timeout_wake.wake_id = "wake:source-lock-timeout".to_string();
    timeout_wake.sequence = 2;
    timeout_wake.event_invocation.subject = lash_core::runtime::RuntimeSubject::ProcessEvent {
        process_id: timeout_wake.process_id.clone(),
        sequence: timeout_wake.sequence,
        event_type: timeout_wake.event_type.clone(),
    };
    let timeout_draft = lash_core::runtime::process_wake_batch_draft(timeout_wake);
    let timeout_retry_draft = timeout_draft.clone();
    let timeout_source_key = timeout_draft
        .source_key
        .as_deref()
        .expect("timeout wake source key");
    let mut timeout_blocker = storage
        .pool()
        .begin()
        .await
        .expect("begin timeout source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(timeout_source_key)
    .execute(&mut *timeout_blocker)
    .await
    .expect("take timeout source-only advisory lock");
    let timeout_error = bounded_store
        .enqueue_queued_work(timeout_draft)
        .await
        .expect_err("source lock wait must be bounded");
    assert!(
        matches!(timeout_error, lash_core::StoreError::Contended),
        "source lock timeout must surface as retryable contention: {timeout_error}"
    );
    timeout_blocker
        .rollback()
        .await
        .expect("release timeout source blocker");
    let second = store
        .enqueue_queued_work(timeout_retry_draft)
        .await
        .expect("enqueue second sequence after source lock release");
    let second_owner = lash_core::LeaseOwnerIdentity::opaque("wake-source-lock-second", "test");
    let second_lease = store
        .try_claim_session_execution_lease(session_id, &second_owner, 60_000)
        .await
        .expect("claim target for second sequence")
        .acquired()
        .expect("second-sequence target lease");
    let second_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &second_lease.fence(),
            &second_owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
        )
        .await
        .expect("claim second wake sequence")
        .expect("second wake sequence claim");
    let state = lash_core::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load target state before second wake settlement")
        .expect("persisted target state");
    store
        .commit_runtime_state(
            lash_core::RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(second_claim.completion())
                .releasing_session_execution_lease(second_lease.completion()),
        )
        .await
        .expect("consume second wake sequence");
    let fence_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_redelivery_fences WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count receiver high-water rows");
    assert_eq!(
        fence_rows, 1,
        "two consumed sequences for one process must occupy one allocation-fence row"
    );
    let allocation_floor: i64 = sqlx::query_scalar(
        "SELECT allocation_floor FROM lash_wake_redelivery_fences
         WHERE session_id = $1 AND process_id = $2",
    )
    .bind(session_id)
    .bind("wake-source-lock-process")
    .fetch_one(storage.pool())
    .await
    .expect("read receiver allocation floor");
    assert_eq!(allocation_floor, 2);
    sqlx::query(
        "INSERT INTO lash_wake_allocation_floors (
            target_session_id, process_id, allocation_floor
         ) VALUES ($1, $2, $3)",
    )
    .bind(session_id)
    .bind("wake-source-lock-process")
    .bind(2_i64)
    .execute(storage.pool())
    .await
    .expect("seed matching sender allocation floor");
    factory
        .delete_session(session_id)
        .await
        .expect("delete high-water target session");
    let fence_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_redelivery_fences WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count receiver high-water rows after session delete");
    assert_eq!(
        fence_rows, 0,
        "session deletion must remove its receiver allocation-fence rows"
    );
    let sender_floor_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_allocation_floors
         WHERE target_session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count sender allocation floors after session delete");
    assert_eq!(
        sender_floor_rows, 0,
        "session deletion must remove sender and receiver floors together"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_attachment_owner_cold_replay_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres attachment-owner conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let storage = Arc::new(storage);
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(storage.runtime_effect_controller(scope.clone()))
        as Arc<dyn lash_core::RuntimeEffectController>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let reopen_effect_controller = {
        let storage = Arc::clone(&storage);
        Arc::new(move || {
            let controller = Arc::new(storage.runtime_effect_controller(scope.clone()))
                as Arc<dyn lash_core::RuntimeEffectController>;
            Box::pin(async move { controller })
                as std::pin::Pin<
                    Box<dyn Future<Output = Arc<dyn lash_core::RuntimeEffectController>> + Send>,
                >
        })
    };
    let clock = Arc::new(lash_core::testing::TestClock::new(
        lash_core::Clock::timestamp_ms(&lash_core::facade_support::SystemClock)
            .saturating_sub(100_000),
    ));
    let factory = Arc::new(
        storage
            .session_store_factory_with_shared_process_registry()
            .with_clock(clock.clone()),
    ) as Arc<dyn SessionStoreFactory>;
    let advance_clock = {
        let clock = Arc::clone(&clock);
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };

    lash_core::testing::conformance::attachment_owner_cold_replay(
        lash_core::testing::conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store: Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_deletes_owned_session_stores_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres process-owned session prune conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let factory = Arc::new(storage.session_store_factory_with_shared_process_registry())
        as Arc<dyn SessionStoreFactory>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    lash_core::testing::conformance::process_prune_deletes_owned_session_stores(factory, registry)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_commit_stamps_use_injected_store_clock_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres injected commit clock regression: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    const SESSION_ID: &str = "postgres-injected-commit-clock";
    const TURN_ID: &str = "postgres-injected-clock-turn";
    const NOW_MS: u64 = 1_234_567;
    let clock = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let factory = storage
        .session_store_factory_with_shared_process_registry()
        .with_clock(clock);
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: SESSION_ID.to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create clocked Postgres session store");
    store
        .record_intent(lash_core::AttachmentIntent {
            attachment_id: lash_core::AttachmentId::new("postgres-clock-attachment"),
            session_id: SESSION_ID.to_string(),
            canonical_uri: "lash-attachment://postgres-clock-attachment".to_string(),
            intent_at_epoch_ms: NOW_MS.saturating_sub(1),
            owner_kind: Some(lash_core::AttachmentOwnerKind::Turn),
            owner_id: Some(TURN_ID.to_string()),
        })
        .expect("record turn-owned intent");
    let owner = lash_core::LeaseOwnerIdentity::opaque("clock-test", "clock-test-incarnation");
    let lease = store
        .try_claim_session_execution_lease(SESSION_ID, &owner, 60_000)
        .await
        .expect("claim clock test lease")
        .acquired()
        .expect("clock test lease acquired");
    let state = lash_core::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..Default::default()
    };
    let operation = lash_core::OperationId::turn(SESSION_ID, TURN_ID, "final");
    let operation_key = operation.storage_key().expect("canonical operation key");
    let (commit, _) = lash_core::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation)
        .expect("stamp clock test commit");
    let commit = commit.releasing_session_execution_lease(lease.completion());
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit with injected clock");

    let manifest_stamp: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_attachment_manifest
         WHERE session_id = $1 AND owner_id = $2",
    )
    .bind(SESSION_ID)
    .bind(TURN_ID)
    .fetch_one(storage.pool())
    .await
    .expect("read manifest commit stamp");
    let turn_stamp: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_runtime_turn_commits
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(SESSION_ID)
    .bind(operation_key)
    .fetch_one(storage.pool())
    .await
    .expect("read turn commit stamp");
    assert_eq!(manifest_stamp as u64, NOW_MS);
    assert_eq!(turn_stamp as u64, NOW_MS);
}

// Blocker 1: `from_pool` must enforce the same component schema-version gate as
// `connect`/`connect_with`. Writing the immediately preceding version into
// `lash_schema_versions` and then constructing over the pool must fail loudly
// with the mismatch error, so a pre-cutover database can never be adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_from_pool_enforces_schema_version_gate_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres from_pool gate test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let pool = storage.pool().clone();
    let current_version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&pool)
    .await
    .expect("read current schema version");
    assert_eq!(current_version, 38, "Postgres component schema pin");
    let payload_hash_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'lash_usage_deltas'
           AND column_name = 'payload_hash'",
    )
    .fetch_one(&pool)
    .await
    .expect("payload_hash column exists");
    assert_eq!(payload_hash_nullable, "NO");
    let payload_encoding_version_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'lash_usage_deltas'
           AND column_name = 'payload_encoding_version'",
    )
    .fetch_one(&pool)
    .await
    .expect("payload_encoding_version column exists");
    assert_eq!(payload_encoding_version_nullable, "NO");
    let usage_identity_constraint: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE conrelid = 'lash_usage_deltas'::regclass
           AND contype = 'u'",
    )
    .fetch_one(&pool)
    .await
    .expect("read usage identity uniqueness constraint");
    assert!(
        usage_identity_constraint.contains(
            "session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash"
        ),
        "usage identity uniqueness must include the payload encoding version and canonical hash: {usage_identity_constraint}"
    );
    let stale_version = current_version - 1;
    // Force the recorded component version to a stale value.
    sqlx::query(
        "INSERT INTO lash_schema_versions (component, version) VALUES ('lash-postgres-store', $1)
         ON CONFLICT (component) DO UPDATE SET version = EXCLUDED.version",
    )
    .bind(stale_version)
    .execute(&pool)
    .await
    .expect("write stale schema version");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    // Restore the correct version BEFORE asserting so a failed assert never leaves
    // the shared database wedged for other cases.
    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1 WHERE component = 'lash-postgres-store'",
    )
    .bind(current_version)
    .execute(&pool)
    .await
    .expect("restore schema version");

    let message = match result {
        Ok(_) => panic!("from_pool must reject a stale schema version"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains(&format!("version {stale_version}"))
            && message.contains(&format!("expected {current_version}")),
        "expected a schema-version mismatch error, got: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_from_pool_rejects_unstamped_existing_schema_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres unstamped-schema gate test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let pool = storage.pool().clone();
    let current_version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&pool)
    .await
    .expect("read current schema version");
    sqlx::query("DELETE FROM lash_schema_versions WHERE component = 'lash-postgres-store'")
        .execute(&pool)
        .await
        .expect("remove component version stamp");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    sqlx::query(
        "INSERT INTO lash_schema_versions (component, version)
         VALUES ('lash-postgres-store', $1)
         ON CONFLICT (component) DO UPDATE SET version = EXCLUDED.version",
    )
    .bind(current_version)
    .execute(&pool)
    .await
    .expect("restore component version stamp");

    let message = match result {
        Ok(_) => panic!("from_pool must reject an unstamped existing Lash schema"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("has no version stamp")
            && message.contains(&format!("expected version {current_version}")),
        "expected an unstamped-schema error, got: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_instance_await_event_conformance_when_configured() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-instance AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_core::testing::conformance::effect_host_await_events_cold_instance(|| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("cold PostgreSQL effect host")
        });
        Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
    })
    .await;
    drop(database_lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_await_event_key_mint_is_pure_and_signatures_match_sqlite_when_seeded() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres await-event signing test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let scope = durable_turn_scope("pure-key-session", "pure-key-turn");
    let wait = AwaitEventWaitIdentity::tool_completion("pure-key-call");

    let (first, second) = tokio::join!(
        async {
            PostgresStorage::connect(&database_url)
                .await
                .expect("first concurrent storage")
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("first concurrent key")
        },
        async {
            PostgresStorage::connect(&database_url)
                .await
                .expect("second concurrent storage")
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("second concurrent key")
        },
    );
    assert_eq!(
        first, second,
        "concurrent openers must read one store secret"
    );
    let wait_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_await_event_waits")
        .fetch_one(storage.pool())
        .await
        .expect("count await-event waits");
    assert_eq!(wait_count, 0, "key mint must not register a promise row");
    let secret: Vec<u8> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read PostgreSQL await-event signer");
    assert_eq!(secret.len(), 32);

    let directory = tempfile::tempdir().expect("SQLite parity tempdir");
    let sqlite_path = directory.path().join("signature-parity.db");
    drop(
        lash_sqlite_store::SqliteEffectHost::open(&sqlite_path)
            .await
            .expect("initialize SQLite parity store"),
    );
    let connection =
        rusqlite::Connection::open(&sqlite_path).expect("open raw SQLite parity store");
    connection
        .execute(
            "UPDATE await_event_meta SET signing_secret = ?1 WHERE singleton = 1",
            rusqlite::params![secret],
        )
        .expect("seed SQLite with PostgreSQL signing secret");
    drop(connection);
    let sqlite_key = lash_sqlite_store::SqliteEffectHost::open(&sqlite_path)
        .await
        .expect("reopen seeded SQLite parity store")
        .await_event_key(&scope, wait)
        .await
        .expect("SQLite parity key");
    assert_eq!(
        first, sqlite_key,
        "PostgreSQL and SQLite must emit byte-identical keys for identical secret and identity"
    );
}

/// The PostgreSQL half of the shared decode vocabulary.
///
/// Both backends decode the persisted terminal in the coordinator, so a corrupt
/// row is a `{backend}_await_event_decode` failure on every promise path. The
/// SQLite twin is
/// `sqlite_await_event_terminal_decode_failures_report_the_decode_vocabulary`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_await_event_terminal_decode_failures_report_the_decode_vocabulary_when_configured()
 {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres await-event decode vocabulary test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let host = storage.effect_host();
    let key = host
        .await_event_key(
            &durable_turn_scope("corrupt-terminal-session", "corrupt-terminal-turn"),
            AwaitEventWaitIdentity::tool_completion("call"),
        )
        .await
        .expect("mint key");
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("winner")))
            .await
            .expect("resolve promise"),
        ResolveOutcome::Accepted
    );
    sqlx::query("UPDATE lash_await_event_waits SET terminal_json = $2 WHERE key_id = $1")
        .bind(&key.key_id)
        .bind("not-json")
        .execute(storage.pool())
        .await
        .expect("corrupt the persisted terminal");

    let peek_error = host
        .peek_await_event(&key)
        .await
        .expect_err("corrupt terminal must fail the peek");
    let resolve_error = host
        .resolve_await_event(&key, Resolution::Cancelled)
        .await
        .expect_err("corrupt terminal must fail the duplicate resolve");
    for error in [peek_error, resolve_error] {
        assert_eq!(error.code.as_str(), "postgres_await_event_decode");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_process_await_event_conformance_when_configured() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::process::Command;

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-process AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_postgres-await-event-helper"))
            .arg(identity)
            .arg(&nonce)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn cold-process helper for {identity}: {error}"));
        let stdout = child.stdout.take().expect("helper stdout pipe");
        let mut lines = BufReader::new(stdout).lines();
        let encoded_key =
            tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("helper did not mint {identity} key"))
                .expect("read helper key")
                .unwrap_or_else(|| panic!("helper exited before printing {identity} key"));
        let key: AwaitEventKey = serde_json::from_str(&encoded_key)
            .unwrap_or_else(|error| panic!("decode helper {identity} key: {error}"));

        child
            .kill()
            .await
            .unwrap_or_else(|error| panic!("kill parked {identity} helper: {error}"));
        let status = child
            .wait()
            .await
            .unwrap_or_else(|error| panic!("reap parked {identity} helper: {error}"));
        assert!(
            !status.success(),
            "killed {identity} helper exited successfully"
        );

        let resolver = Arc::new(
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process resolver")
                .effect_host(),
        );
        let terminal = if identity == "turn_cancel_gate" {
            let address = lash_core::runtime::TurnAddress::new(
                format!("cold-process-{nonce}-session"),
                format!("cold-process-{nonce}-turn"),
            );
            let receipt = lash_core::runtime::TurnWorkDriver::new(
                Arc::clone(&resolver) as Arc<dyn EffectHost>
            )
            .request_cancel(lash_core::runtime::TurnCancelRequest::new(
                address,
                format!("cold-process-{nonce}-cancel"),
                None,
            ))
            .await
            .expect("request cancellation through a successor owner");
            assert!(matches!(
                receipt.outcome,
                lash_core::runtime::TurnCancelOutcome::Requested(_)
            ));
            resolver
                .peek_await_event(&key)
                .await
                .expect("peek successor cancellation")
                .expect("successor cancellation resolves the killed owner's gate")
        } else {
            let terminal = Resolution::Ok(serde_json::json!({
                "cold_process": true,
                "identity": identity,
                "nonce": nonce,
            }));
            assert_eq!(
                resolver
                    .resolve_await_event(&key, terminal.clone())
                    .await
                    .unwrap_or_else(|error| panic!(
                        "resolve killed-helper {identity} key: {error}"
                    )),
                ResolveOutcome::Accepted
            );
            terminal
        };
        drop(resolver);

        let observer =
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process observer")
                .effect_host();
        assert_eq!(
            observer
                .peek_await_event(&key)
                .await
                .unwrap_or_else(|error| panic!("peek killed-helper {identity} key: {error}")),
            Some(terminal.clone())
        );
        assert_eq!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                .await
                .unwrap_or_else(|error| panic!("observe killed-helper {identity} key: {error}")),
            terminal
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_replay_satisfies_cold_process_crash_conformance_when_configured() {
    use tokio::process::Command;

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-process effect replay conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let dir = tempfile::tempdir().expect("cold-process effect replay tempdir");
    let marker = dir.path().join("external-effect.log");
    let nonce = uuid::Uuid::new_v4().to_string();
    let run = |action: &'static str| {
        let marker = marker.clone();
        let nonce = nonce.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Command::new(env!("CARGO_BIN_EXE_postgres-await-event-helper"))
                    .arg(action)
                    .arg(nonce)
                    .arg(marker)
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{action} helper timed out"))
            .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"))
        }
    };

    let crashed = run("effect_crash").await;
    assert_eq!(crashed.status.code(), Some(86));
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read crashed effect marker")
            .lines()
            .count(),
        1
    );

    let completed = run("effect_complete").await;
    assert!(
        completed.status.success(),
        "successor helper failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read re-executed effect marker")
            .lines()
            .count(),
        2,
        "at-least-once means re-execution before outcome recording"
    );

    let replayed = run("effect_replay").await;
    assert!(
        replayed.status.success(),
        "replay helper failed: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read replay effect marker")
            .lines()
            .count(),
        2,
        "recorded effect outcome replays without re-execution"
    );
}

#[tokio::test]
async fn postgres_real_turn_satisfies_cold_process_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping PostgreSQL cold-process real-turn matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let url = database_url().expect("configured PostgreSQL database URL");
    let dir = tempfile::tempdir().expect("PostgreSQL cold-process real-turn tempdir");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command =
                tokio::process::Command::new(env!("CARGO_BIN_EXE_postgres-await-event-helper"));
            command
                .env("LASH_POSTGRES_DATABASE_URL", &url)
                .arg(action)
                .arg(nonce)
                .arg(marker);
            command
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_effect_controller_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-effect conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;

    let host = storage.effect_host();
    lash_core::testing::conformance::effect_host_retires_session_journal(&host).await;
    lash_core::testing::conformance::effect_host_retires_process_journal(&host).await;
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_runtime_effect_replay
         WHERE session_id = $1",
    )
    .bind("retired-journal-session")
    .fetch_one(storage.pool())
    .await
    .expect("count retained Postgres session journal rows");
    assert_eq!(retained, 0);

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-conformance",
    ));
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(&controller, || {
        controller.start_replay()
    })
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-mismatch-conformance",
    ));
    lash_core::testing::conformance::effect_controller_replay_mismatch_diagnostics(
        &controller,
        "postgres_effect_replay_hash_conflict",
    )
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-concurrent-conformance",
    ));
    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-tool-conformance",
    ));
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_controller_satisfies_lease_fencing_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres effect lease-fencing conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;

    let make_storage = storage.clone();
    let steal_pool = storage.pool().clone();
    let expire_pool = storage.pool().clone();
    lash_core::testing::conformance::effect_controller_lease_fencing(
        lash_core::testing::conformance::EffectLeaseFencingBackend {
            make_controller: Box::new(move |ttl| {
                let storage = make_storage.clone();
                Box::pin(async move {
                    let controller = PostgresRuntimeEffectController::with_options(
                        &storage,
                        durable_turn_scope("session", "turn"),
                        PostgresEffectReplayOptions {
                            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(ttl)
                                .expect("conformance lease timings"),
                        },
                    );
                    let for_replay = controller.clone();
                    lash_core::testing::conformance::LeaseFencingController {
                        controller: Arc::new(controller),
                        start_replay: Box::new(move || for_replay.start_replay()),
                    }
                })
            }),
            steal_lease: Box::new(move |replay_key| {
                let pool = steal_pool.clone();
                Box::pin(async move {
                    let stolen_until = epoch_ms_for_test().saturating_add(10_000) as i64;
                    let changed = sqlx::query(
                        "UPDATE lash_runtime_effect_replay
                         SET lease_owner_id = 'stolen-owner',
                             lease_token = 'stolen-token',
                             lease_expires_at_ms = $1
                         WHERE replay_key = $2",
                    )
                    .bind(stolen_until)
                    .bind(&replay_key)
                    .execute(&pool)
                    .await
                    .expect("steal lease row")
                    .rows_affected();
                    assert_eq!(changed, 1);
                })
            }),
            expire_lease: Box::new(move |replay_key| {
                let pool = expire_pool.clone();
                Box::pin(async move {
                    let changed = sqlx::query(
                        "UPDATE lash_runtime_effect_replay
                         SET lease_expires_at_ms = 0
                         WHERE replay_key = $1",
                    )
                    .bind(&replay_key)
                    .execute(&pool)
                    .await
                    .expect("expire lease row")
                    .rows_affected();
                    assert_eq!(changed, 1);
                })
            }),
        },
    )
    .await;
}

fn epoch_ms_for_test() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_registry_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::process_registry_reopenable(|| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(&storage).await;
            let open = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
            let reopen = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
            ReopenableProcessRegistry { open, reopen }
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_batch_tombstones_are_ordered_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process prune batch conformance: database URL is not set");
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::process_prune_batch_tombstones(Arc::new(
        storage.process_registry(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_scopes_to_the_retention_filter_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres scoped process prune conformance: database URL is not set");
        return;
    };
    reset(&storage).await;
    lash_core::testing::conformance::process_prune_scoped_by_originator(Arc::new(
        storage.process_registry(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_leased_completion_replay_repairs_projection_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres leased replay repair: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    lash_core::testing::conformance::leased_completion_replay_repairs_projection(
        registry,
        move |stale| async move {
            let changed =
                sqlx::query("UPDATE lash_processes SET record_json = $2 WHERE process_id = $1")
                    .bind(&stale.id)
                    .bind(serde_json::to_string(&stale).expect("encode stale process projection"))
                    .execute(&pool)
                    .await
                    .expect("corrupt Postgres process projection")
                    .rows_affected();
            assert_eq!(changed, 1);
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_trigger_retention_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres process-trigger retention conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::process_trigger_retention(|| {
        let storage = Arc::clone(&storage);
        async move {
            reset(&storage).await;
            lash_core::testing::conformance::ProcessTriggerRetentionHandles {
                registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
                triggers: Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>,
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_store_contract_state_machine_properties_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres store-contract properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::store_contract_state_machine("postgres", move |_| {
        let storage = Arc::clone(&storage);
        async move {
            reset(&storage).await;
            lash_core::testing::conformance::StoreContractHandles {
                registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
                runtime: Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_persistence_state_machine_properties_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-persistence properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::runtime_persistence_state_machine("postgres", move |_| {
        let storage = Arc::clone(&storage);
        async move {
            reset(&storage).await;
            lash_core::testing::conformance::RuntimePersistenceStateMachineHandles::create(
                Arc::new(storage.session_store_factory_with_shared_process_registry()),
                true,
            )
            .await
            .expect("create Postgres runtime-persistence property handles")
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_graph_state_machine_properties_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-graph properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::session_graph_state_machine("postgres", move |_| {
        let storage = Arc::clone(&storage);
        async move {
            reset(&storage).await;
            Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_continuation_store_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres continuation conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let process_storage = Arc::new(storage.process_registry());
    let registry = Arc::clone(&process_storage) as Arc<dyn lash_core::ProcessRegistry>;
    let store = process_storage as Arc<dyn lash_core::ProcessContinuationStore>;
    lash_core::testing::conformance::process_continuation_store(registry, store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_trigger_store_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres trigger conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::trigger_store_reopenable(|| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(&storage).await;
            let open = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
            let reopen = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
            ReopenableTriggerStore { open, reopen }
        })
    })
    .await;
}

/// Drive one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn ProcessRegistry,
    process_id: &str,
) {
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register waiting retention process");
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "waiting-retention-run")
            .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("invocation authority carries its start fact");
    registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .expect("start waiting retention process");
    let waiting = registry
        .set_process_wait_with_authority(
            process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "retention".to_string(),
                    event_type: "retention.signal".to_string(),
                    key: format!("{process_id}:wait"),
                    ordinal: 1,
                },
            },
            &authority,
        )
        .await
        .expect("enter wait");
    assert_eq!(
        waiting.status.label(),
        "waiting",
        "the wait must land in the persisted status label the retention SQL reads"
    );
    assert!(!waiting.is_terminal(), "a waiting process is not terminal");

    let live = registry
        .list_non_terminal()
        .await
        .expect("list non-terminal processes");
    assert!(
        live.iter().any(|record| record.id == process_id),
        "a waiting process must be listed as live"
    );

    let report = registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes");
    assert_eq!(
        report.pruned_processes, 0,
        "a waiting process must never be a prune candidate, whatever the cutoff"
    );
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read waiting retention process")
            .is_some(),
        "the waiting process row must survive the prune"
    );
}

/// A waiting process is live, not prunable. The PostgreSQL half of
/// `sqlite_waiting_processes_are_live_not_prunable`: both backends spell
/// `NON_TERMINAL_PROCESS_STATUS_LABELS` out as SQL literals, so both need the
/// behavioural referee.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_waiting_processes_are_live_not_prunable_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping PostgreSQL waiting-retention regression: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let process_id = format!("waiting-retention:{}", uuid::Uuid::new_v4());
    assert_waiting_process_is_live_not_prunable(&registry, &process_id).await;
}

/// Lexical half of the retention contract, mirroring
/// `sqlite_status_list_literals_derive_from_the_shared_constant`: every
/// `status IN`/`status NOT IN` literal in this backend's SQL must spell
/// exactly the label list rendered from `NON_TERMINAL_PROCESS_STATUS_LABELS`,
/// so a grown constant with a stale SQL literal fails here instead of
/// silently pruning live rows.
#[test]
fn postgres_status_list_literals_derive_from_the_shared_constant() {
    let expected = format!(
        "({})",
        lash_core::facade_support::registry_transitions::NON_TERMINAL_PROCESS_STATUS_LABELS
            .iter()
            .map(|label| format!("'{label}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let source = include_str!("../src/postgres/process_registry.rs");
    let mut total = 0usize;
    for delimiter in ["status IN ", "status NOT IN "] {
        for site in source.split(delimiter).skip(1) {
            assert!(
                site.starts_with(&expected),
                "process_registry.rs: a `{delimiter}` list literal diverged from \
                 NON_TERMINAL_PROCESS_STATUS_LABELS: expected {expected}, found {}",
                &site[..site.len().min(40)]
            );
            total += 1;
        }
    }
    assert_eq!(
        total, 2,
        "expected exactly two status-list literal sites in the PostgreSQL backend; \
         update this count when adding one"
    );
}
