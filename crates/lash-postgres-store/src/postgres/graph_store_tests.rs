//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! PostgreSQL, each on its own isolated database: PC-ATOMIC, PC-ROOTS,
//! PC-GC, PC-READ, PC-SIZE, PC-GATE and the read, collection and fork
//! races.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::testing::TestClock;
use lash_core_store::checkpoint_graph::{GraphRefusal, NodeId};
use lash_durable::domain::graph::{GraphStore, ReadPin};
use lash_durable::domain::graph_commit::{CollectionLimits, GraphCollection};
use lash_durable::laws::graph::{self as laws, ForeignWrite, GateControl, GateHold, GraphTier};
use lash_durable::{ActorKey, DurableStore};
use sqlx::{PgPool, Postgres, Transaction};

use super::super::PostgresDurableStore;
use crate::testing::IsolatedDatabase;

/// The graph gate's key (`connection_sql`'s `lock_xact_graph_gate`).
const GATE_KEY: i64 = 5_494_764_590_268_039_504;

struct Tier {
    _database: IsolatedDatabase,
    _storage: crate::PostgresStorage,
    pool: PgPool,
    store: PostgresDurableStore,
    clock: Arc<TestClock>,
}

async fn tier() -> Option<Tier> {
    let database_url = crate::postgres_test_support::database_url()?;
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let clock = Arc::new(TestClock::new(1_000_000));
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    Some(Tier {
        pool: storage.pool().clone(),
        _database: database,
        _storage: storage,
        store,
        clock,
    })
}

fn hex(node: &NodeId) -> String {
    node.to_hex()
}

#[async_trait::async_trait]
impl GraphTier for Tier {
    fn store(&self) -> &dyn DurableStore {
        &self.store
    }

    fn graph(&self) -> &dyn GraphStore {
        &self.store
    }

    fn advance(&self, millis: u64) {
        self.clock.advance(millis);
    }

    async fn collect(&self, limits: CollectionLimits) -> Result<GraphCollection, GraphRefusal> {
        self.store.collect_graph(limits).await
    }

    async fn release_read_pin(&self, pin: ReadPin) -> Result<(), GraphRefusal> {
        self.store.release_read_pin(pin).await
    }

    async fn blob(&self, hash: &str) -> Option<(Vec<u8>, u8)> {
        let row: Option<(Vec<u8>, i16)> =
            sqlx::query_as("SELECT content, encoding FROM lash_blobs WHERE hash = $1")
                .bind(hash)
                .fetch_optional(&self.pool)
                .await
                .expect("a blob read");
        row.map(|(bytes, encoding)| (bytes, u8::try_from(encoding).expect("a stored encoding")))
    }

    async fn edges(&self, parent: &NodeId) -> Vec<NodeId> {
        let children: Vec<String> = sqlx::query_scalar(
            "SELECT child_hash FROM lash_blob_edges WHERE parent_hash = $1 ORDER BY child_hash",
        )
        .bind(hex(parent))
        .fetch_all(&self.pool)
        .await
        .expect("an edge read");
        children
            .iter()
            .map(|child| NodeId::from_hex(child).expect("a stored child"))
            .collect()
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
            ForeignWrite::OpaqueBlob { hash, bytes } => {
                sqlx::query("INSERT INTO lash_blobs (hash, content, encoding) VALUES ($1, $2, 0)")
                    .bind(hash.clone())
                    .bind(bytes.clone())
            }
            ForeignWrite::OverwriteBlob { node, bytes } => {
                sqlx::query("UPDATE lash_blobs SET content = $2 WHERE hash = $1")
                    .bind(hex(node))
                    .bind(bytes.clone())
            }
            ForeignWrite::DeleteEdges { parent } => {
                sqlx::query("DELETE FROM lash_blob_edges WHERE parent_hash = $1").bind(hex(parent))
            }
            ForeignWrite::DeleteBlob { node } => {
                sqlx::query("DELETE FROM lash_blobs WHERE hash = $1").bind(hex(node))
            }
            ForeignWrite::RevisionRoot {
                session,
                revision,
                root: Some(root),
            } => sqlx::query(
                "INSERT INTO lash_session_revisions
                     (session_id, head_revision, leaf_node_id, checkpoint_ref, head_json)
                 VALUES ($1, $2, NULL, $3, '{}')",
            )
            .bind(session.as_str().to_owned())
            .bind(i64::try_from(*revision).expect("a small revision"))
            .bind(hex(root)),
            ForeignWrite::RevisionRoot {
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
            .bind(hex(root)),
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
            .bind(hex(root)),
            ForeignWrite::ArtifactBridge {
                namespace,
                artifact,
                root: None,
            } => sqlx::query(
                "DELETE FROM lash_lash_vm_artifacts WHERE namespace = $1 AND artifact_ref = $2",
            )
            .bind(namespace.clone())
            .bind(artifact.clone()),
            ForeignWrite::TerminalWindow {
                session,
                run,
                root: Some(root),
            } => sqlx::query(
                "WITH head AS (
                     INSERT INTO lash_session_run_window_heads (session_id, head_digest, head_json)
                     VALUES ($1, 'head', '{}')
                 )
                 INSERT INTO lash_session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_window_revision, terminal_window_checkpoint_ref,
                      terminal_window_head_digest)
                 VALUES ($1, $2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1,
                         1, $3, 'head')",
            )
            .bind(session.as_str().to_owned())
            .bind(run.as_str().to_owned())
            .bind(hex(root)),
            ForeignWrite::TerminalWindow {
                session,
                run,
                root: None,
            } => sqlx::query("DELETE FROM lash_session_runs WHERE session_id = $1 AND run = $2")
                .bind(session.as_str().to_owned())
                .bind(run.as_str().to_owned()),
            ForeignWrite::BreakCatalog => sqlx::query("DROP VIEW lash_graph_root_catalog"),
        };
        query
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|error| panic!("a foreign write {write:?}: {error}"));
        tx.commit().await.expect("a foreign write commits");
    }
}

/// The gate held by a transaction of the law's own.
struct Held {
    tx: Transaction<'static, Postgres>,
}

#[async_trait::async_trait]
impl GateHold for Held {
    async fn owner_unlocked(&mut self, actor: &ActorKey) -> bool {
        sqlx::query("SAVEPOINT probe")
            .execute(&mut *self.tx)
            .await
            .expect("a probe savepoint");
        let locked = sqlx::query(super::OWNER_ROW_NOWAIT)
            .bind(actor.as_str())
            .fetch_optional(&mut *self.tx)
            .await;
        sqlx::query(if locked.is_ok() {
            "RELEASE SAVEPOINT probe"
        } else {
            "ROLLBACK TO SAVEPOINT probe"
        })
        .execute(&mut *self.tx)
        .await
        .expect("the probe ends");
        locked.is_ok_and(|row| row.is_some())
    }

    async fn release(self: Box<Self>) {
        self.tx.commit().await.expect("the gate's holder commits");
    }
}

#[async_trait::async_trait]
impl GateControl for Tier {
    async fn hold(&self, exclusive: bool) -> Box<dyn GateHold> {
        let mut tx = self.pool.begin().await.expect("a gate holder");
        let sql = if exclusive {
            "SELECT pg_advisory_xact_lock($1)"
        } else {
            "SELECT pg_advisory_xact_lock_shared($1)"
        };
        sqlx::query(sql)
            .bind(GATE_KEY)
            .execute(&mut *tx)
            .await
            .expect("the gate is taken");
        Box::new(Held { tx })
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
law!(a_collection_sweeps_only_what_it_proved_unreachable);
law!(a_read_is_one_protected_root);
law!(an_owner_commit_is_admitted_on_its_whole_encoding);
law!(a_read_racing_replacement_and_collection_reads_one_whole_root);
law!(a_collection_racing_commits_sweeps_nothing_they_rely_on);
law!(a_reference_racing_its_roots_retirement_installs_whole_or_refuses);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_graph_gate_orders_collection_against_every_guarded_transaction() {
    let Some(tier) = tier().await else {
        eprintln!("skipping the graph gate law: database URL is not set");
        return;
    };
    laws::the_graph_gate_orders_collection_against_every_guarded_transaction(&tier, &tier)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}
