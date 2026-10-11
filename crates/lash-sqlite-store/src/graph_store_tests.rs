//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! SQLite, in memory and in a file.

use std::sync::Arc;

use lash_core_execution::testing::TestClock;
use lash_core_store::checkpoint_graph::{GraphRefusal, NodeId};
use lash_durable::DurableStore;
use lash_durable::domain::graph::{GraphStore, ReadPin};
use lash_durable::domain::graph_commit::{CollectionLimits, GraphCollection};
use lash_durable::laws::graph::{self as laws, ForeignWrite, GraphTier};

use crate::{SqliteDurableStore, SqliteStoreSet};

struct Tier {
    _root: Option<tempfile::TempDir>,
    _set: SqliteStoreSet,
    store: SqliteDurableStore,
    clock: Arc<TestClock>,
}

async fn tier(file: bool) -> Tier {
    let clock = Arc::new(TestClock::new(1_000_000));
    let (root, set) = if file {
        let root = tempfile::tempdir().expect("store root");
        let set = SqliteStoreSet::open_with_clock(
            root.path().join("lash.db"),
            crate::SqliteSynchronous::Normal,
            clock.clone(),
        )
        .await
        .expect("open the file store set");
        (Some(root), set)
    } else {
        let set = SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("open the memory store set");
        (None, set)
    };
    let store = set.durable_store();
    Tier {
        _root: root,
        _set: set,
        store,
        clock,
    }
}

impl Tier {
    /// Runs `sql` past the schema's foreign keys: foreign data does not ask
    /// them.
    async fn raw(&self, sql: String, params: Vec<rusqlite::types::Value>) {
        self.store
            .connection_for_testing()
            .call(move |conn| {
                conn.execute_batch("PRAGMA foreign_keys = OFF")?;
                let result = conn.execute(&sql, rusqlite::params_from_iter(params));
                conn.execute_batch("PRAGMA foreign_keys = ON")?;
                result.map(drop)
            })
            .await
            .expect("a foreign write");
    }
}

fn text(value: impl Into<String>) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(value.into())
}

fn hex(node: &NodeId) -> rusqlite::types::Value {
    text(node.to_hex())
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
        let hash = hash.to_owned();
        self.store
            .connection_for_testing()
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(
                    "SELECT content, encoding FROM blobs WHERE hash = ?1",
                    [hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .await
            .expect("a blob read")
    }

    async fn edges(&self, parent: &NodeId) -> Vec<NodeId> {
        let parent = parent.to_hex();
        self.store
            .connection_for_testing()
            .call(move |conn| {
                conn.prepare(
                    "SELECT child_hash FROM blob_edges WHERE parent_hash = ?1 ORDER BY child_hash",
                )?
                .query_map([parent], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .expect("an edge read")
            .iter()
            .map(|child| NodeId::from_hex(child).expect("a stored child"))
            .collect()
    }

    async fn foreign(&self, write: ForeignWrite) {
        use rusqlite::types::Value;
        let (sql, params): (&str, Vec<Value>) = match write {
            ForeignWrite::OpaqueBlob { hash, bytes } => (
                "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 0)",
                vec![text(hash), Value::Blob(bytes)],
            ),
            ForeignWrite::OverwriteBlob { node, bytes } => (
                "UPDATE blobs SET content = ?2 WHERE hash = ?1",
                vec![hex(&node), Value::Blob(bytes)],
            ),
            ForeignWrite::DeleteEdges { parent } => (
                "DELETE FROM blob_edges WHERE parent_hash = ?1",
                vec![hex(&parent)],
            ),
            ForeignWrite::DeleteBlob { node } => {
                ("DELETE FROM blobs WHERE hash = ?1", vec![hex(&node)])
            }
            ForeignWrite::RevisionRoot {
                session,
                revision,
                root: Some(root),
            } => (
                "INSERT INTO session_revisions
                     (session_id, head_revision, leaf_node_id, checkpoint_ref, head_json)
                 VALUES (?1, ?2, NULL, ?3, '{}')",
                vec![
                    text(session.as_str()),
                    Value::Integer(i64::try_from(revision).expect("a small revision")),
                    hex(&root),
                ],
            ),
            ForeignWrite::RevisionRoot {
                session,
                revision,
                root: None,
            } => (
                "DELETE FROM session_revisions WHERE session_id = ?1 AND head_revision = ?2",
                vec![
                    text(session.as_str()),
                    Value::Integer(i64::try_from(revision).expect("a small revision")),
                ],
            ),
            ForeignWrite::AdmissionBase {
                session,
                root: Some(root),
            } => (
                "INSERT INTO session_meta
                     (session_id, relation_kind, retention_kind, admission_base_checkpoint_ref)
                 VALUES (?1, 'root', 'until_gc', ?2)",
                vec![text(session.as_str()), hex(&root)],
            ),
            ForeignWrite::AdmissionBase {
                session,
                root: None,
            } => (
                "DELETE FROM session_meta WHERE session_id = ?1",
                vec![text(session.as_str())],
            ),
            ForeignWrite::ArtifactBridge {
                namespace,
                artifact,
                root: Some(root),
            } => (
                "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref) VALUES (?1, ?2, ?3)",
                vec![text(namespace), text(artifact), hex(&root)],
            ),
            ForeignWrite::ArtifactBridge {
                namespace,
                artifact,
                root: None,
            } => (
                "DELETE FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2",
                vec![text(namespace), text(artifact)],
            ),
            ForeignWrite::TerminalWindow {
                session,
                run,
                root: Some(root),
            } => (
                "INSERT INTO session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_window_revision, terminal_window_checkpoint_ref,
                      terminal_window_head_digest)
                 VALUES (?1, ?2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1,
                         1, ?3, 'head')",
                vec![text(session.as_str()), text(run.as_str()), hex(&root)],
            ),
            ForeignWrite::TerminalWindow {
                session,
                run,
                root: None,
            } => (
                "DELETE FROM session_runs WHERE session_id = ?1 AND run = ?2",
                vec![text(session.as_str()), text(run.as_str())],
            ),
            ForeignWrite::BreakCatalog => ("DROP VIEW graph_root_catalog", Vec::new()),
        };
        self.raw(sql.to_owned(), params).await;
    }
}

macro_rules! law {
    ($name:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn memory() {
                let tier = tier(false).await;
                laws::$name(&tier)
                    .await
                    .unwrap_or_else(|broken| panic!("{broken}"));
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn file() {
                let tier = tier(true).await;
                laws::$name(&tier)
                    .await
                    .unwrap_or_else(|broken| panic!("{broken}"));
            }
        }
    };
}

law!(a_refused_graph_commit_writes_nothing);
law!(a_lost_ack_retry_answers_once_and_a_reused_identity_refuses);
law!(every_root_source_retains_its_graph_and_shared_leaves_outlive_one_owner);
law!(a_collection_sweeps_only_what_it_proved_unreachable);
law!(a_read_is_one_protected_root);
law!(a_commit_is_admitted_on_its_whole_encoding);
law!(a_read_racing_replacement_and_collection_reads_one_whole_root);
law!(a_collection_racing_commits_sweeps_nothing_they_rely_on);
law!(a_reference_racing_its_roots_retirement_installs_whole_or_refuses);

/// FIG-5912: a stored node's member of the build catalog hydrates with all
/// it reaches, byte for byte as stored, and every node of that closure
/// counts against each hydration limit, whether its bytes are read back or
/// taken from the build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_catalog_closure_hydrates_whole_within_every_limit() {
    use lash_core_store::checkpoint_graph::{
        DecodeDimension, DecodeLimits, EncodeBudget, ExplicitLimit, GraphStaging, NodeKind,
        TokenWriter, TypedBody,
    };
    use lash_durable::domain::graph::RootOwner;
    use lash_durable::{DurableError, DurableReads as _};

    let body = |write: &dyn Fn(&mut TokenWriter)| {
        let mut out = TokenWriter::new();
        write(&mut out);
        out.into_bytes()
    };
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let leaf_body = body(&|out| out.text(&"x".repeat(200)));
    let leaf = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&leaf_body))
        .expect("a leaf");
    let member_body = body(&|out| out.child(leaf.child_ref()));
    let member = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&member_body))
        .expect("a member");
    let catalog = staging.nodes().clone();
    lash_core_store::checkpoint_graph::node::register_immutable_catalog(&catalog);
    let root_body = body(&|out| {
        out.seq(2);
        out.child(member.child_ref());
        out.text("root");
    });
    let root = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&root_body))
        .expect("a root");
    let nodes = staging.into_nodes();

    let tier = tier(false).await;
    let installed = nodes.clone();
    tier.store
        .connection_for_testing()
        .call(move |conn| {
            crate::durable::install_staged(conn, &installed)
                .map(|installed| installed.expect("the graph installs"))
        })
        .await
        .expect("an install");
    let session = lash_sansio::SessionId::parse("catalog-law").expect("a session id");
    tier.foreign(ForeignWrite::RevisionRoot {
        session: session.clone(),
        revision: 1,
        root: Some(root),
    })
    .await;
    let owner = RootOwner::SessionRevision {
        session,
        revision: 1,
    };
    let hydrate = |limits: DecodeLimits| {
        let owner = owner.clone();
        let store = &tier.store;
        async move { store.hydrate(owner, Some(root), limits).await }
    };

    let total: u64 = nodes.values().map(|node| node.len()).sum();
    let graph = hydrate(DecodeLimits::within_total(total))
        .await
        .expect("the whole closure hydrates");
    assert_eq!(graph.nodes.len(), nodes.len());
    for (id, node) in &nodes {
        assert_eq!(&*graph.nodes[id].bytes, &*node.bytes, "node {id:?}");
    }
    let limit = |dimension, actual, limit| {
        Err::<(), _>(DurableError::Graph(GraphRefusal::DecodeLimit {
            dimension,
            actual,
            limit,
        }))
    };
    assert_eq!(
        hydrate(DecodeLimits {
            nodes: 2,
            ..DecodeLimits::within_total(total)
        })
        .await
        .map(drop),
        limit(DecodeDimension::Nodes, 3, 2)
    );
    assert_eq!(
        hydrate(DecodeLimits::within_total(total - 1))
            .await
            .map(drop),
        limit(DecodeDimension::TotalBytes, total, total - 1)
    );
    let leaf_len = nodes[&leaf].len();
    assert_eq!(
        hydrate(DecodeLimits {
            node_bytes: leaf_len - 1,
            ..DecodeLimits::within_total(total)
        })
        .await
        .map(drop),
        limit(DecodeDimension::NodeBytes, leaf_len, leaf_len - 1)
    );
}
