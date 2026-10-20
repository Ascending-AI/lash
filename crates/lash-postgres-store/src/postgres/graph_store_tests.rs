//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! PostgreSQL, each on its own isolated database: PC-ATOMIC, PC-ROOTS with
//! R-LIVE and R-DEAD, the orphan audit, PC-SIZE, the read, audit and fork
//! (R-RACE) races, the one-statement hydration and the audit under load;
//! the persistent-heap page laws (`lash_durable::laws::pages`); and what
//! PostgreSQL's graph traffic costs.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_ids::operational_metrics::sql;
use lash_core_ids::test_clock::TestClock;
use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphRefusal, GraphStaging, NodeId,
    NodeKind, TokenWriter, TypedBody,
};
use lash_durable::domain::graph::{HydratedGraph, RootOwner};
use lash_durable::domain::pages::PageStore;
use lash_durable::laws::graph::{self as laws, ForeignWrite, GraphTier, LockHold, ServerProbe};
use lash_durable::laws::pages::{self, PageTier};
use lash_durable::{DurableError, DurableReads as _, DurableStore};
use sqlx::{PgPool, Postgres, Transaction};

use super::super::PostgresDurableStore;
use crate::testing::IsolatedDatabase;

struct Tier {
    _database: IsolatedDatabase,
    _storage: crate::PostgresStorage,
    pool: PgPool,
    store: PostgresDurableStore,
    /// A second node's storage and store over the same database.
    peer: Option<(crate::PostgresStorage, PostgresDurableStore)>,
    config: crate::PostgresHostConfig,
    clock: Arc<TestClock>,
}

async fn tier() -> Option<Tier> {
    tier_with(crate::testing::fixture_config(), false).await
}

/// A tier under `config`, with a second node when `peer`.
async fn tier_with(config: crate::PostgresHostConfig, peer: bool) -> Option<Tier> {
    let database_url = crate::postgres_test_support::database_url()?;
    let database = IsolatedDatabase::create(&database_url).await;
    let clock = Arc::new(TestClock::new(1_000_000));
    let (storage, store) = open(&database, &clock, &config).await;
    let peer = if peer {
        Some(open(&database, &clock, &config).await)
    } else {
        None
    };
    Some(Tier {
        pool: storage.pool().clone(),
        _database: database,
        _storage: storage,
        store,
        peer,
        config,
        clock,
    })
}

async fn open(
    database: &IsolatedDatabase,
    clock: &Arc<TestClock>,
    config: &crate::PostgresHostConfig,
) -> (crate::PostgresStorage, PostgresDurableStore) {
    let storage = crate::testing::connect_with(database.url(), config)
        .await
        .expect("open the isolated store");
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    (storage, store)
}

/// Runs retirement steps until nothing is held: how many nodes they
/// deleted.
async fn settle(store: &PostgresDurableStore) -> Result<u64, DurableError> {
    let mut deleted = 0;
    loop {
        let progress = store
            .retire_step(lash_durable::domain::pages::RetirementBudget { nodes: 4_096 })
            .await?;
        if progress.holds_advanced == 0 && progress.leases_reaped == 0 {
            return Ok(deleted);
        }
        deleted += progress.nodes_deleted;
    }
}

fn hex(node: &NodeId) -> String {
    node.to_hex()
}

#[async_trait::async_trait]
impl GraphTier for Tier {
    fn store(&self) -> &dyn DurableStore {
        &self.store
    }

    async fn audit(&self) -> Result<u64, GraphRefusal> {
        self.store.audit_orphans().await
    }

    async fn retire(&self) -> Result<u64, DurableError> {
        settle(&self.store).await
    }

    async fn blob(&self, hash: &str) -> Option<(Vec<u8>, u8)> {
        let row: Option<(Vec<u8>, i16)> =
            sqlx::query_as("SELECT content, encoding FROM lash_blobs WHERE hash = $1")
                .bind(crate::keys::key(hash))
                .fetch_optional(&self.pool)
                .await
                .expect("a blob read");
        row.map(|(bytes, encoding)| (bytes, u8::try_from(encoding).expect("a stored encoding")))
    }

    async fn edges(&self, parent: &NodeId) -> Vec<NodeId> {
        let children: Vec<String> = sqlx::query_scalar(
            "SELECT encode(child_hash, 'hex') FROM lash_blob_edges WHERE parent_hash = $1 ORDER BY child_hash",
        )
        .bind(crate::keys::key(hex(parent)))
        .fetch_all(&self.pool)
        .await
        .expect("an edge read");
        children
            .iter()
            .map(|child| NodeId::from_hex(child).expect("a stored child"))
            .collect()
    }

    async fn blobs(&self) -> BTreeSet<String> {
        sqlx::query_scalar("SELECT encode(hash, 'hex') FROM lash_blobs")
            .fetch_all(&self.pool)
            .await
            .expect("a blob listing")
            .into_iter()
            .collect()
    }

    async fn roots(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT encode(root_ref, 'hex') FROM lash_graph_root_catalog")
            .fetch_all(&self.pool)
            .await
            .expect("a root listing")
    }

    /// Runs past the schema's foreign keys and triggers: foreign data does
    /// not ask them.
    async fn foreign(&self, write: ForeignWrite) {
        let mut tx = self.pool.begin().await.expect("a foreign transaction");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .expect("foreign data skips the foreign keys");
        let query = match &write {
            ForeignWrite::ModelCallsDelete { session } => {
                crate::durable::retire_session_rows(&mut tx, session)
                    .await
                    .expect("physical session row retirement");
                tx.commit().await.expect("physical deletion commits");
                return;
            }
            ForeignWrite::OpaqueBlob { hash, bytes } => {
                sqlx::query("INSERT INTO lash_blobs (hash, content, encoding) VALUES ($1, $2, 0)")
                    .bind(crate::keys::key(hash))
                    .bind(bytes.clone())
            }
            ForeignWrite::OverwriteBlob { node, bytes } => {
                sqlx::query("UPDATE lash_blobs SET content = $2 WHERE hash = $1")
                    .bind(crate::keys::key(hex(node)))
                    .bind(bytes.clone())
            }
            ForeignWrite::RevisionRoot {
                config,
                session,
                revision,
                root: Some(root),
            } => sqlx::query(
                "INSERT INTO lash_session_revisions
                     (session_id, head_revision, leaf_node_id, checkpoint_ref, config_ref)
                 VALUES ($1, $2, NULL, $3, $4)",
            )
            .bind(session.as_str().to_owned())
            .bind(i64::try_from(*revision).expect("a small revision"))
            .bind(if *config {
                None
            } else {
                Some(crate::keys::key(hex(root)))
            })
            .bind(crate::keys::key(hex(root))),
            ForeignWrite::RevisionRoot {
                config: _,
                session,
                revision,
                root: None,
            } => sqlx::query(
                "DELETE FROM lash_session_revisions WHERE session_id = $1 AND head_revision = $2",
            )
            .bind(session.as_str().to_owned())
            .bind(i64::try_from(*revision).expect("a small revision")),
            ForeignWrite::AdmissionBase {
                session,
                root: Some(root),
            } => sqlx::query(
                "INSERT INTO lash_session_meta
                     (session_id, relation_kind, retention_kind, admission_base_checkpoint_ref)
                 VALUES ($1, 'root', 'until_gc', $2)",
            )
            .bind(session.as_str().to_owned())
            .bind(crate::keys::key(hex(root))),
            ForeignWrite::AdmissionBase {
                session,
                root: None,
            } => sqlx::query("DELETE FROM lash_session_meta WHERE session_id = $1")
                .bind(session.as_str().to_owned()),
            ForeignWrite::ArtifactBridge {
                namespace,
                artifact,
                root: Some(root),
            } => sqlx::query(
                "INSERT INTO lash_lash_vm_artifacts
                     (namespace, artifact_ref, storage_kind, graph_ref)
                 VALUES ($1, $2, 1, $3)",
            )
            .bind(namespace.clone())
            .bind(artifact.clone())
            .bind(crate::keys::key(hex(root))),
            ForeignWrite::ArtifactBridge {
                namespace,
                artifact,
                root: None,
            } => sqlx::query(
                "DELETE FROM lash_lash_vm_artifacts WHERE namespace = $1 AND artifact_ref = $2",
            )
            .bind(namespace.clone())
            .bind(artifact.clone()),
            ForeignWrite::TerminalTranscript {
                session,
                run,
                root: Some(root),
            } => sqlx::query(
                "INSERT INTO lash_session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_transcript_ref)
                 VALUES ($1, $2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1, $3)",
            )
            .bind(session.as_str().to_owned())
            .bind(run.as_str().to_owned())
            .bind(crate::keys::key(hex(root))),
            ForeignWrite::TerminalTranscript {
                session,
                run,
                root: None,
            } => sqlx::query("DELETE FROM lash_session_runs WHERE session_id = $1 AND run = $2")
                .bind(session.as_str().to_owned())
                .bind(run.as_str().to_owned()),
        };
        query
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|error| panic!("a foreign write {write:?}: {error}"));
        tx.commit().await.expect("a foreign write commits");
    }
}

/// A row lock held by a transaction of the law's own.
struct Held {
    tx: Transaction<'static, Postgres>,
    pool: PgPool,
    pid: i32,
}

#[async_trait::async_trait]
impl LockHold for Held {
    async fn wait_for_blocked(&mut self) {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND $1 = ANY(pg_blocking_pids(pid))
                 )",
            )
            .bind(self.pid)
            .fetch_one(&self.pool)
            .await
            .expect("observe waiters blocked by the orphan holder");
            if blocked {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    async fn release(self: Box<Self>) {
        self.tx.commit().await.expect("the lock's holder commits");
    }
}

impl Tier {
    /// Locks blob `hash` in `mode` in a transaction of its own.
    async fn lock_blob(&self, hash: &str, mode: &str) -> Box<dyn LockHold> {
        let mut tx = self.pool.begin().await.expect("a lock holder");
        let locked: Option<(crate::keys::Key, i32)> = sqlx::query_as(&format!(
            "SELECT hash, pg_backend_pid() FROM lash_blobs WHERE hash = $1 FOR {mode}"
        ))
        .bind(crate::keys::key(hash))
        .fetch_optional(&mut *tx)
        .await
        .expect("the blob is locked");
        let (_, pid) = locked.unwrap_or_else(|| panic!("the held blob {hash} is stored"));
        Box::new(Held {
            tx,
            pool: self.pool.clone(),
            pid,
        })
    }
}

#[async_trait::async_trait]
impl pages::PageLockProbe for Tier {
    async fn peel_hold(&self, hash: &str) -> Box<dyn LockHold> {
        self.lock_blob(hash, "UPDATE").await
    }

    async fn name_hold(&self, hash: &str) -> Box<dyn LockHold> {
        self.lock_blob(hash, "KEY SHARE").await
    }

    async fn wait_for_lock_waiters(&self, count: usize) {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&self.pool)
            .await
            .expect("observe the lock waiters");
            if usize::try_from(waiting).unwrap_or(usize::MAX) >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}

#[async_trait::async_trait]
impl ServerProbe for Tier {
    async fn hold(&self, hash: &str) -> Box<dyn LockHold> {
        self.lock_blob(hash, "KEY SHARE").await
    }

    async fn hydration_round_trips(
        &self,
        owner: RootOwner,
        limits: DecodeLimits,
    ) -> (Result<HydratedGraph, GraphRefusal>, u64) {
        let trips = &crate::observed_sql::ROUND_TRIPS;
        trips
            .scope(std::cell::Cell::new(0), async {
                let answer = self.store.hydrate(owner, None, limits).await;
                let answer = answer.map_err(|error| match error {
                    DurableError::Graph(refusal) => refusal,
                    other => panic!("a hydration failed: {other}"),
                });
                (answer, trips.with(std::cell::Cell::get))
            })
            .await
    }
}

macro_rules! law {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(tier) = tier().await else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            laws::$name(&tier)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    };
}

law!(a_refused_owner_graph_commit_writes_nothing);
law!(an_owner_refuses_a_second_outcome_for_one_call);
law!(every_root_source_retains_its_graph_and_shared_leaves_outlive_one_owner);
law!(the_orphan_audit_peels_only_what_no_root_reaches);
law!(an_owner_commit_is_admitted_on_its_whole_encoding);
law!(a_read_racing_replacement_and_its_peel_reads_one_whole_root);
law!(an_audit_racing_commits_peels_nothing_they_rely_on);
law!(a_reference_racing_its_roots_retirement_installs_whole_or_refuses);

#[async_trait::async_trait]
impl PageTier for Tier {
    fn pages(&self) -> &dyn PageStore {
        &self.store
    }

    fn peer(&self) -> &dyn PageStore {
        self.peer.as_ref().map_or(&self.store, |(_, store)| store)
    }

    async fn stored(&self) -> BTreeSet<NodeId> {
        GraphTier::blobs(self)
            .await
            .iter()
            .map(|hash| NodeId::from_hex(hash).expect("a stored digest"))
            .collect()
    }

    async fn catalog(&self) -> Vec<NodeId> {
        GraphTier::roots(self)
            .await
            .iter()
            .map(|hash| NodeId::from_hex(hash).expect("a stored root"))
            .collect()
    }

    async fn edge_rows(&self) -> BTreeSet<(NodeId, NodeId)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT encode(parent_hash, 'hex'), encode(child_hash, 'hex') FROM lash_blob_edges",
        )
        .fetch_all(&self.pool)
        .await
        .expect("a raw read")
        .iter()
        .map(|(parent, child)| {
            (
                NodeId::from_hex(parent).expect("a stored parent"),
                NodeId::from_hex(child).expect("a stored child"),
            )
        })
        .collect()
    }

    /// Runs past the schema's foreign keys: foreign damage does not ask
    /// them.
    async fn drop_blob(&self, node: &NodeId) {
        let mut tx = self.pool.begin().await.expect("a foreign transaction");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .expect("foreign damage skips the foreign keys");
        for sql in [
            "DELETE FROM lash_blob_edges WHERE parent_hash = $1 OR child_hash = $1",
            "DELETE FROM lash_blobs WHERE hash = $1",
        ] {
            sqlx::query(sql)
                .bind(crate::keys::key(hex(node)))
                .execute(&mut *tx)
                .await
                .expect("a foreign delete");
        }
        tx.commit().await.expect("commit the foreign damage");
    }

    fn advance(&self, millis: u64) {
        self.clock.advance(millis);
    }

    fn peak_buffered(&self) -> u64 {
        PostgresDurableStore::candidate_peak_buffered_for_testing()
    }

    async fn reopen(&mut self) {
        let (storage, store) = open(&self._database, &self.clock, &self.config).await;
        self.pool = storage.pool().clone();
        self._storage = storage;
        self.store = store;
    }

    fn resident_high_water(&self) -> Option<u64> {
        // Test tooling: the law reads the platform's own report.
        #[allow(clippy::disallowed_methods)]
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
    }

    async fn delete_session(&self, session: &str) -> u64 {
        let factory = self._storage.session_store_factory();
        let session =
            lash_protocol_types::SessionId::try_from(session.to_owned()).expect("a law session id");
        let (answer, receipt) = sql::collect(
            "pages.session-delete",
            "postgres",
            lash_core_store::store::SessionCatalogStore::delete_session(&factory, &session),
        )
        .await;
        answer.expect("the session delete commits");
        receipt.rows_affected
    }
}

macro_rules! page_law {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(tier) = tier().await else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            pages::$name(&tier)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    };
}

page_law!(a_candidate_installs_only_new_nodes_and_direct_references);
page_law!(publication_is_atomic_with_its_effect_admissions);
page_law!(a_step_outcome_owns_its_candidate);
page_law!(a_pruned_step_outcome_frees_exactly_its_unshared_pages);
page_law!(a_host_read_never_descends_into_a_heap_page);
page_law!(a_leased_root_survives_concurrent_retirement);
page_law!(an_expired_lease_reads_nothing);
page_law!(the_lost_receipt_resolves_by_commit_identity);
page_law!(publication_keeps_global_digest_lock_order);
page_law!(an_ownership_transfer_fences_the_old_activation);
page_law!(a_node_retires_sealed_pages_on_its_own);
page_law!(backpressure_holds_claims_until_retirement_catches_up);
page_law!(candidate_edge_pass_does_not_reread_fresh_bodies);
page_law!(a_page_read_is_one_statement_that_answers_its_lease_first);
page_law!(a_candidate_check_fetches_only_the_bytes_it_compares);

/// D-RET: a node killed in a retirement batch, before and after its commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free() {
    let Some(mut tier) = tier().await else {
        eprintln!("skipping the retirement crash: database URL is not set");
        return;
    };
    pages::a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free(&mut tier)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

/// D-RET, D-LEASE under load (FIG-6193): two nodes whose durable writes
/// are never retried, so a deadlock or a lock timeout fails the write that
/// met it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publications_and_retirement_on_two_nodes_never_deadlock() {
    let mut config = crate::testing::fixture_config();
    config.retry.durable = crate::host::RetryPolicy {
        attempts: 1,
        ..config.retry.durable
    };
    let Some(tier) = tier_with(config, true).await else {
        eprintln!("skipping the two-node race: database URL is not set");
        return;
    };
    pages::publications_and_retirement_on_two_nodes_never_deadlock(&tier, Some(&tier), 64)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

/// D-RET over retired leaves, with a reconnect between steps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_advances_in_bounded_steps_and_reclaims_everything_unowned() {
    let Some(mut tier) = tier().await else {
        eprintln!("skipping retirement: database URL is not set");
        return;
    };
    pages::retirement_advances_in_bounded_steps_and_reclaims_everything_unowned(&mut tier, 5_000)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

/// D-RET (FIG-6423): a deleted session's heap retires in
/// bounded steps, not in the delete's transaction, and no node a peel
/// skipped is left to the orphan audit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_session_retires_its_heap_in_bounded_steps() {
    // Compare 8 and 2,000 pages within the standard operation deadline;
    // SQLite's mounts exercise the same law at 50,000 pages.
    let Some(tier) = tier().await else {
        eprintln!("skipping the session heap retirement: database URL is not set");
        return;
    };
    pages::deleting_a_session_retires_its_heap_in_bounded_steps(&tier, 2_000)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

/// S5/S6: a candidate larger than every buffer installs within them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_candidate_installs_within_bounded_buffers() {
    let Some(tier) = tier().await else {
        eprintln!("skipping the bounded install: database URL is not set");
        return;
    };
    // Observe server waits without borrowing the measured work pool's
    // capacity. This task never retries or extends the candidate operation.
    type BackendWait = (i32, Option<String>, Option<String>, Option<String>);
    let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_waits = Arc::clone(&waits);
    let diagnostics_pool = tier.store.pools.session.clone();
    let observer = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for _ in 0..24 {
            interval.tick().await;
            let sample: Result<Vec<BackendWait>, _> = crate::observed_sql::query_as(
                "SELECT pid, state, wait_event_type, wait_event FROM pg_stat_activity \
                     WHERE datname = current_database() AND pid <> pg_backend_pid() ORDER BY pid",
            )
            .fetch_all(crate::observed_sql::executor(&diagnostics_pool))
            .await;
            observed_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    started.elapsed().as_millis(),
                    sample.map_err(|error| error.to_string()),
                ));
        }
    });
    let resources_before = bounded_install_resources(&tier);
    let (result, sql) = lash_core_ids::operational_metrics::sql::collect(
        "bounded-candidate-install",
        "postgres",
        pages::a_large_candidate_installs_within_bounded_buffers(&tier, 20_000),
    )
    .await;
    observer.abort();
    let _ = observer.await;
    let resources_after = bounded_install_resources(&tier);
    let evidence = format!(
        "resources_before={resources_before:?} resources_after={resources_after:?} server_waits={:?} \
         sql_elapsed_ns={} sql_statements={} sql_rows_returned={} sql_rows_affected={} installed_graph_bytes={}",
        waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        sql.elapsed_nanos,
        sql.statements,
        sql.rows_returned,
        sql.rows_affected,
        sql.newly_installed_graph_bytes,
    );
    eprintln!("{evidence}");
    result.unwrap_or_else(|broken| panic!("{broken}; {evidence}"));
}

/// Keep unavailable observations explicit; these are the action worker's
/// resources, rather than the machine that invoked the build.
fn bounded_install_resources(tier: &Tier) -> std::collections::BTreeMap<String, String> {
    let mut observations = std::collections::BTreeMap::new();
    for path in [
        "/proc/sys/kernel/hostname",
        "/proc/loadavg",
        "/proc/self/stat",
        "/proc/self/schedstat",
        "/proc/self/cgroup",
        "/proc/pressure/cpu",
        "/proc/pressure/memory",
        "/proc/pressure/io",
        "/sys/fs/cgroup/cpu.max",
        "/sys/fs/cgroup/cpu.stat",
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory.peak",
        "/sys/fs/cgroup/memory.events",
        "/sys/fs/cgroup/cpu.pressure",
        "/sys/fs/cgroup/memory.pressure",
    ] {
        let value =
            std::fs::read_to_string(path).unwrap_or_else(|error| format!("unavailable: {error}"));
        observations.insert(path.to_owned(), value);
    }
    observations.insert(
        "work_pool".to_owned(),
        format!(
            "size={} idle={} admission_available={}",
            tier.store.pools.work.size(),
            tier.store.pools.work.num_idle(),
            tier.store.pools.admission.available_permits(),
        ),
    );
    observations
}

macro_rules! server_law {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(tier) = tier().await else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            laws::$name(&tier, &tier)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    };
}

server_law!(a_hydration_is_one_statement);
server_law!(commits_and_reads_progress_while_the_audit_runs);

/// Runs `work` on this task, with the SQL it sent and read.
async fn counted<T>(work: impl std::future::Future<Output = T>) -> (T, sql::Receipt) {
    let (value, receipt) = sql::collect("graph-traffic-law", "postgres", work).await;
    assert_eq!(receipt.unmeasured_bound_statements, 0, "{receipt:?}");
    (value, receipt)
}

/// A body of `text`, or of `child` and then `text`.
fn frozen(staging: &mut GraphStaging, child: Option<NodeId>, text: &str) -> NodeId {
    let mut body = TokenWriter::new();
    if let Some(child) = child {
        body.seq(2);
        body.child(child.child_ref());
    }
    body.text(text);
    staging
        .encode(
            NodeKind::FROZEN_VALUE,
            TypedBody::logical(&body.into_bytes()),
        )
        .expect("a node stages")
}

/// Installs `nodes`, naming `reads`, in a transaction of its own,
/// committed when they install.
async fn install(
    tier: &Tier,
    nodes: &BTreeMap<NodeId, EncodedNode>,
    reads: &BTreeSet<NodeId>,
) -> Result<(), GraphRefusal> {
    let checked = lash_durable::domain::graph_commit::check_graph(nodes, reads, &[])?;
    let mut tx = tier.pool.begin().await.expect("a transaction");
    let answer = super::install(&mut tx, &checked).await.map(drop);
    if answer.is_ok() {
        tx.commit().await.expect("the install commits");
    }
    answer
}

/// FIG-5924: a commit that names a stored node, as a park names its
/// release, and supplies nodes the store already holds binds only the
/// bodies it adds; each held body crosses the wire once, read back with the
/// lock and compared byte for byte, so a stored body that differs still
/// refuses, as it does after the insert of a commit that names none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_node_crosses_the_wire_once_and_still_compares_exactly() {
    let Some(tier) = tier().await else {
        eprintln!("skipping the held-node law: database URL is not set");
        return;
    };
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let anchor = frozen(&mut staging, None, "anchor");
    let held = frozen(&mut staging, None, &"h".repeat(8 * 1024));
    let parent = frozen(&mut staging, Some(held), "parent");
    let first = staging.nodes().clone();
    let added = frozen(&mut staging, Some(parent), "added");
    let mut again = staging.into_nodes();
    again.remove(&anchor);
    let named = BTreeSet::from([anchor]);
    install(&tier, &first, &BTreeSet::new())
        .await
        .expect("the first graph installs");

    let (answer, receipt) = counted(install(&tier, &again, &named)).await;
    answer.expect("the graph installs again with one node added");
    eprintln!(
        "install naming a stored node: {} statements, {} bound bytes, {} result bytes, {} installed bytes",
        receipt.statements,
        receipt.bound_bytes,
        receipt.result_bytes,
        receipt.newly_installed_graph_bytes
    );
    let new_bytes = again[&added].len();
    assert_eq!(receipt.newly_installed_graph_bytes, new_bytes);
    assert!(receipt.bound_bytes < new_bytes + 1024, "{receipt:?}");
    let held_bytes: u64 = again
        .values()
        .filter(|node| node.id != added)
        .map(EncodedNode::len)
        .sum();
    assert!(
        receipt.result_bytes >= held_bytes && receipt.result_bytes < held_bytes + 1024,
        "{receipt:?} for {held_bytes} held bytes"
    );

    let mut changed = first[&held].bytes.to_vec();
    *changed.last_mut().expect("a body") ^= 1;
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: held,
        bytes: changed,
    })
    .await;
    for reads in [&named, &BTreeSet::new()] {
        assert_eq!(
            install(&tier, &again, reads).await,
            Err(GraphRefusal::ContentConflict(held))
        );
    }
}

/// FIG-5924: a stored graph that reaches the build catalog hydrates whole,
/// byte for byte, within every limit, and reads none of what the catalog
/// holds back from the server: not below a stored node that reaches a
/// member, nor below a root that is one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_catalog_closure_hydrates_whole_without_reading_it_back() {
    use lash_core_store::checkpoint_graph::{DecodeDimension, DecodeLimits};
    use lash_durable::domain::graph::RootOwner;
    use lash_durable::{DurableError, DurableReads as _};

    let Some(tier) = tier().await else {
        eprintln!("skipping the catalog hydration law: database URL is not set");
        return;
    };
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let leaf = frozen(&mut staging, None, &"x".repeat(8 * 1024));
    let member = frozen(&mut staging, Some(leaf), "member");
    lash_core_store::checkpoint_graph::node::register_immutable_catalog(&staging.nodes().clone());
    let root = frozen(&mut staging, Some(member), "root");
    let nodes = staging.into_nodes();
    install(&tier, &nodes, &BTreeSet::new())
        .await
        .expect("the graph installs");
    let session = lash_protocol_types::SessionId::parse("catalog-law").expect("a session id");
    for (revision, top) in [(1, root), (2, member)] {
        tier.foreign(ForeignWrite::RevisionRoot {
            config: false,
            session: session.clone(),
            revision,
            root: Some(top),
        })
        .await;
    }
    let hydrate = |revision: u64, top: NodeId, limits: DecodeLimits| {
        let owner = RootOwner::SessionRevision {
            session: session.clone(),
            revision,
        };
        let store = &tier.store;
        async move { store.hydrate(owner, Some(top), limits).await }
    };

    let largest = nodes.values().map(EncodedNode::len).max().expect("nodes");
    for (revision, top) in [(1, root), (2, member)] {
        let reached: BTreeMap<_, _> = nodes
            .iter()
            .filter(|(id, _)| **id != root || top == root)
            .collect();
        let total: u64 = reached.values().map(|node| node.len()).sum();
        let (graph, receipt) =
            counted(hydrate(revision, top, DecodeLimits::within_total(total))).await;
        let graph = graph.expect("the whole graph hydrates");
        eprintln!(
            "hydrating revision {revision}: {} statements, {} bound bytes, {} result bytes",
            receipt.statements, receipt.bound_bytes, receipt.result_bytes
        );
        assert_eq!(graph.nodes.len(), reached.len());
        for (id, node) in &reached {
            assert_eq!(&*graph.nodes[*id].bytes, &*node.bytes, "node {id:?}");
        }
        // The catalog holds over 8 KiB; the read returns one small row.
        assert!(receipt.result_bytes < 1024, "{receipt:?} reading {top:?}");

        let limit = |dimension, actual, limit| {
            Err::<(), _>(DurableError::Graph(GraphRefusal::DecodeLimit {
                dimension,
                actual,
                limit,
            }))
        };
        let count = reached.len() as u64;
        assert_eq!(
            hydrate(
                revision,
                top,
                DecodeLimits {
                    nodes: count - 1,
                    ..DecodeLimits::within_total(total)
                }
            )
            .await
            .map(drop),
            limit(DecodeDimension::Nodes, count, count - 1)
        );
        assert_eq!(
            hydrate(revision, top, DecodeLimits::within_total(total - 1))
                .await
                .map(drop),
            limit(DecodeDimension::TotalBytes, total, total - 1)
        );
        assert_eq!(
            hydrate(
                revision,
                top,
                DecodeLimits {
                    node_bytes: largest - 1,
                    ..DecodeLimits::within_total(total)
                }
            )
            .await
            .map(drop),
            limit(DecodeDimension::NodeBytes, largest, largest - 1)
        );
    }
}

#[tokio::test]
async fn turn_and_owned_model_calls_survive_an_idempotent_outcome_commit() {
    let tier = tier_with(crate::testing::fixture_config(), true)
        .await
        .expect("the hermetic PostgreSQL target supplies a database");
    let peer = &tier.peer.as_ref().expect("a cold peer").1;
    laws::turn_and_owned_model_calls_survive_an_idempotent_outcome_commit(&tier, peer)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}
