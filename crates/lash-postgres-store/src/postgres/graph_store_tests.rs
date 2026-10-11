//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! PostgreSQL, each on its own isolated database: PC-ATOMIC, PC-ROOTS,
//! PC-GC, PC-SIZE, PC-GATE and the collection and fork races; and what
//! PostgreSQL's graph traffic costs.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_execution::facade_support::sql;
use lash_core_execution::testing::TestClock;
use lash_core_store::checkpoint_graph::{
    EncodeBudget, EncodedNode, ExplicitLimit, GraphRefusal, GraphStaging, NodeId, NodeKind,
    TokenWriter, TypedBody,
};
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

    async fn collect(&self, limits: CollectionLimits) -> Result<GraphCollection, GraphRefusal> {
        self.store.collect_graph(limits).await
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
            .bind(if *config { None } else { Some(hex(root)) })
            .bind(hex(root)),
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
                config,
                session,
                run,
                root: Some(root),
            } => sqlx::query(
                "                 INSERT INTO lash_session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_window_revision, terminal_window_checkpoint_ref,
                      terminal_window_config_ref)
                 VALUES ($1, $2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1,
                         1, $3, $4)",
            )
            .bind(session.as_str().to_owned())
            .bind(run.as_str().to_owned())
            .bind(if *config { None } else { Some(hex(root)) })
            .bind(hex(root)),
            ForeignWrite::TerminalWindow {
                config: _,
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
law!(an_owner_commit_is_admitted_on_its_whole_encoding);
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
    let session = lash_sansio::SessionId::parse("catalog-law").expect("a session id");
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
