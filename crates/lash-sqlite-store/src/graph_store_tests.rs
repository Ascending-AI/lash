//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! SQLite, in memory and in a file.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core_execution::testing::TestClock;
use lash_core_store::checkpoint_graph::{GraphRefusal, NodeId};
use lash_durable::DurableStore;
use lash_durable::domain::pages::PageStore;
use lash_durable::laws::graph::{self as laws, ForeignWrite, GraphTier};
use lash_durable::laws::pages::{self, PageTier};

use crate::{SqliteDurableStore, SqliteStoreSet};

struct Tier {
    _root: Option<tempfile::TempDir>,
    _set: SqliteStoreSet,
    store: SqliteDurableStore,
    /// A second node's store set and store over the same file.
    peer: Option<(SqliteStoreSet, SqliteDurableStore)>,
    clock: Arc<TestClock>,
}

async fn open_set(root: Option<&std::path::Path>, clock: &Arc<TestClock>) -> SqliteStoreSet {
    match root {
        Some(root) => SqliteStoreSet::open_with_clock(
            root.join("lash.db"),
            crate::SqliteSynchronous::Normal,
            clock.clone(),
        )
        .await
        .expect("open the file store set"),
        None => SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("open the memory store set"),
    }
}

async fn tier(file: bool) -> Tier {
    let clock = Arc::new(TestClock::new(1_000_000));
    let root = file.then(|| tempfile::tempdir().expect("store root"));
    let set = open_set(root.as_ref().map(tempfile::TempDir::path), &clock).await;
    let store = set.durable_store();
    Tier {
        _root: root,
        _set: set,
        store,
        peer: None,
        clock,
    }
}

/// [`tier`] with a second node over the file, when it is one.
async fn two_node_tier(file: bool) -> Tier {
    let mut tier = tier(file).await;
    if let Some(root) = tier._root.as_ref().map(|root| root.path().to_owned()) {
        let set = open_set(Some(&root), &tier.clock).await;
        let store = set.durable_store();
        tier.peer = Some((set, store));
    }
    tier
}

/// Runs retirement steps until nothing is held: how many nodes they
/// deleted.
async fn settle(store: &SqliteDurableStore) -> Result<u64, lash_durable::DurableError> {
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

impl Tier {
    /// The one text column `sql` selects, row by row.
    async fn texts(&self, sql: &'static str) -> Vec<String> {
        self.store
            .connection_for_testing()
            .call(move |conn| {
                conn.prepare(sql)?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .expect("a raw read")
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

    async fn audit(&self) -> Result<u64, GraphRefusal> {
        self.store.audit_orphans().await
    }

    async fn retire(&self) -> Result<u64, lash_durable::DurableError> {
        settle(&self.store).await
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

    async fn blobs(&self) -> BTreeSet<String> {
        self.texts("SELECT hash FROM blobs")
            .await
            .into_iter()
            .collect()
    }

    async fn roots(&self) -> Vec<String> {
        self.texts("SELECT root_ref FROM graph_root_catalog").await
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
            ForeignWrite::RevisionRoot {
                config,
                session,
                revision,
                root: Some(root),
            } => (
                "INSERT INTO session_revisions
                     (session_id, head_revision, leaf_node_id, checkpoint_ref, config_ref)
                 VALUES (?1, ?2, NULL, ?3, ?4)",
                vec![
                    text(session.as_str()),
                    Value::Integer(i64::try_from(revision).expect("a small revision")),
                    if config { Value::Null } else { hex(&root) },
                    hex(&root),
                ],
            ),
            ForeignWrite::RevisionRoot {
                config: _,
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
                config,
                session,
                run,
                root: Some(root),
            } => (
                "INSERT INTO session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_window_revision, terminal_window_checkpoint_ref,
                      terminal_window_config_ref)
                 VALUES (?1, ?2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1,
                         1, ?3, ?4)",
                vec![
                    text(session.as_str()),
                    text(run.as_str()),
                    if config { Value::Null } else { hex(&root) },
                    hex(&root),
                ],
            ),
            ForeignWrite::TerminalWindow {
                config: _,
                session,
                run,
                root: None,
            } => (
                "DELETE FROM session_runs WHERE session_id = ?1 AND run = ?2",
                vec![text(session.as_str()), text(run.as_str())],
            ),
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

#[async_trait::async_trait]
impl PageTier for Tier {
    fn pages(&self) -> &dyn PageStore {
        &self.store
    }

    fn peer(&self) -> &dyn PageStore {
        self.peer.as_ref().map_or(&self.store, |(_, store)| store)
    }

    async fn stored(&self) -> BTreeSet<NodeId> {
        self.texts("SELECT hash FROM blobs")
            .await
            .iter()
            .map(|hash| NodeId::from_hex(hash).expect("a stored digest"))
            .collect()
    }

    async fn catalog(&self) -> Vec<NodeId> {
        self.texts("SELECT root_ref FROM graph_root_catalog")
            .await
            .iter()
            .map(|hash| NodeId::from_hex(hash).expect("a stored root"))
            .collect()
    }

    async fn drop_blob(&self, node: &NodeId) {
        for sql in [
            "DELETE FROM blob_edges WHERE parent_hash = ?1 OR child_hash = ?1",
            "DELETE FROM semantic_admissions WHERE root_ref = ?1",
            "DELETE FROM blobs WHERE hash = ?1",
        ] {
            self.raw(sql.to_owned(), vec![hex(node)]).await;
        }
    }

    fn advance(&self, millis: u64) {
        self.clock.advance(millis);
    }

    fn peak_buffered(&self) -> u64 {
        SqliteDurableStore::candidate_peak_buffered_for_testing()
    }

    async fn reopen(&mut self) {
        let Some(root) = self._root.as_ref().map(|root| root.path().to_owned()) else {
            return;
        };
        let set = open_set(Some(&root), &self.clock).await;
        self.store = set.durable_store();
        self._set = set;
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
}

macro_rules! page_law {
    ($name:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn memory() {
                let tier = tier(false).await;
                pages::$name(&tier)
                    .await
                    .unwrap_or_else(|broken| panic!("{broken}"));
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn file() {
                let tier = tier(true).await;
                pages::$name(&tier)
                    .await
                    .unwrap_or_else(|broken| panic!("{broken}"));
            }
        }
    };
}

page_law!(a_candidate_installs_only_new_nodes_and_direct_references);
page_law!(publication_is_atomic_with_admissions_and_semantic_admission);
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

/// D-RET: a node killed in a retirement batch, before and after its commit.
mod a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory() {
        let mut tier = tier(false).await;
        pages::a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free(&mut tier)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file() {
        let mut tier = tier(true).await;
        pages::a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free(&mut tier)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }
}

/// D-RET, D-LEASE under load (FIG-6193): a memory store races itself; a
/// file has a second node.
mod publications_and_retirement_on_two_nodes_never_deadlock {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory() {
        let tier = two_node_tier(false).await;
        pages::publications_and_retirement_on_two_nodes_never_deadlock(&tier, None, 48)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file() {
        let tier = two_node_tier(true).await;
        pages::publications_and_retirement_on_two_nodes_never_deadlock(&tier, None, 48)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }
}

/// D-RET over retired leaves, with a restart between steps.
mod retirement_advances_in_bounded_steps_and_reclaims_everything_unowned {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory() {
        let mut tier = tier(false).await;
        pages::retirement_advances_in_bounded_steps_and_reclaims_everything_unowned(
            &mut tier, 5_000,
        )
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file() {
        let mut tier = tier(true).await;
        pages::retirement_advances_in_bounded_steps_and_reclaims_everything_unowned(
            &mut tier, 5_000,
        )
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "benchmark: run with --ignored to measure 100 000-leaf retirement on a file"]
    async fn hundred_thousand_on_file() {
        let mut tier = tier(true).await;
        pages::retirement_advances_in_bounded_steps_and_reclaims_everything_unowned(
            &mut tier, 100_000,
        )
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    }
}

/// S5/S6: a candidate larger than every buffer installs within them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_candidate_installs_within_bounded_buffers() {
    let tier = tier(true).await;
    pages::a_large_candidate_installs_within_bounded_buffers(&tier, 20_000)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

/// The bounded-memory install of one million new pages, on a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run with --ignored to measure a one-million-page install"]
async fn a_million_page_candidate_installs_within_bounded_buffers() {
    let tier = tier(true).await;
    pages::a_large_candidate_installs_within_bounded_buffers(&tier, 1_000_000)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}

law!(a_refused_owner_graph_commit_writes_nothing);
law!(an_owner_refuses_a_second_outcome_for_one_call);
law!(every_root_source_retains_its_graph_and_shared_leaves_outlive_one_owner);
law!(the_orphan_audit_peels_only_what_no_root_reaches);
law!(an_owner_commit_is_admitted_on_its_whole_encoding);
law!(a_read_racing_replacement_and_its_peel_reads_one_whole_root);
law!(an_audit_racing_commits_peels_nothing_they_rely_on);
law!(a_reference_racing_its_roots_retirement_installs_whole_or_refuses);

/// GRAPH-PEEL-WORK: retirement reads edges only from nodes it can delete.
/// A pinned manifest and a subtree another parent reaches both stop the
/// peel before their children; releasing the last references reclaims them.
#[tokio::test]
async fn retiring_a_root_does_not_read_edges_of_retained_descendants() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    use lash_durable::domain::graph::ReleasePinWrite;
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, Epoch};

    let tier = tier(false).await;
    let identity = lash_core_execution::testing::sql_work::owner_commit(&tier._set).await;
    let actor = ActorKey::session("sql-work-owner").unwrap();
    let initial = tier.blobs().await;
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut encode = |tag: &str, children: &[NodeId]| {
        let mut body = TokenWriter::new();
        body.seq(children.len() as u64 + 1);
        body.text(tag);
        for child in children {
            body.child(child.child_ref());
        }
        staging
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
            .unwrap()
    };
    let leaves: Vec<_> = (0..32)
        .map(|index| encode(&format!("leaf-{index}"), &[]))
        .collect();
    let pinned = encode("pinned manifest", &leaves[..16]);
    let shared = encode("shared subtree", &leaves[16..]);
    let holder = encode("other parent", &[shared]);
    let retired = encode("retired root", &[pinned, shared]);
    let pin = |release: &str, root| {
        DomainWrite::ReleasePin(ReleasePinWrite::Install {
            release: release.into(),
            root,
        })
    };
    let retire = |release: &str, expected| {
        DomainWrite::ReleasePin(ReleasePinWrite::Retire {
            release: release.into(),
            expected,
        })
    };
    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.stage(staging.into_nodes().into_values());
    for write in [
        pin("manifest", pinned),
        pin("holder", holder),
        pin("retired", retired),
    ] {
        tx.write(write);
    }
    tier.store
        .commit(tx, CommitLabel::new("peel.install"))
        .await
        .unwrap();

    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.write(retire("retired", retired));
    tier.store
        .commit(tx, CommitLabel::new("peel.retire"))
        .await
        .unwrap();
    let (answer, receipt) = lash_core_execution::facade_support::sql::collect(
        "peel.retire",
        "sqlite",
        settle(&tier.store),
    )
    .await;
    answer.unwrap();
    let stored = tier.blobs().await;
    assert!(!stored.contains(&retired.to_hex()));
    assert!(stored.contains(&pinned.to_hex()));
    assert!(stored.contains(&shared.to_hex()));
    for leaf in &leaves {
        assert!(stored.contains(&leaf.to_hex()));
    }
    let edge_rows: u64 = receipt
        .shapes
        .iter()
        .filter(|(sql, _)| sql.starts_with("select ") && sql.contains("main.blob_edges"))
        .map(|(_, shape)| shape.rows_returned)
        .sum();
    eprintln!(
        "GRAPH-PEEL SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(edge_rows, 2, "retained descendants' edges are never read");

    // Put a parent and its child in the same retirement frontier. The
    // child's edges must be read on the next level, after the parent goes.
    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.write(pin("shared", shared));
    tier.store
        .commit(tx, CommitLabel::new("peel.shared_pin"))
        .await
        .unwrap();
    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.write(retire("manifest", pinned));
    tx.write(retire("holder", holder));
    tx.write(retire("shared", shared));
    tier.store
        .commit(tx, CommitLabel::new("peel.last_refs"))
        .await
        .unwrap();
    settle(&tier.store).await.unwrap();
    assert_eq!(
        tier.blobs().await,
        initial,
        "the last references reclaim the whole graph"
    );
}

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
        config: false,
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

/// GRAPH-INSTALL-BATCH-WORK: large installs need at most ceil(absent / 32)
/// body statements; small installs (at most 64 bodies) need at most one per
/// body, independent of held bodies. Edges need at most one statement.
/// Each body statement installs at most 32 rows (64 bound parameters).
/// A held content or encoding mismatch refuses the whole batch before writes.
#[tokio::test]
async fn absent_graph_bodies_install_in_bounded_batches() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    use lash_durable::domain::graph::ReleasePinWrite;
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, Epoch};

    let tier = tier(false).await;
    let identity = lash_core_execution::testing::sql_work::owner_commit(&tier._set).await;
    let actor = ActorKey::session("sql-work-owner").unwrap();
    let count = |receipt: &lash_core_execution::facade_support::sql::Receipt, table: &str| {
        receipt
            .shapes
            .iter()
            .filter(|(sql, _)| sql.starts_with(&format!("insert into main.{table} ")))
            .map(|(_, shape)| shape.statements)
            .sum::<u64>()
    };
    for size in [0_usize, 1, 31, 32, 33, 63, 64, 65, 129] {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut encode = |tag: &str, children: &[NodeId]| {
            let mut body = TokenWriter::new();
            body.seq(children.len() as u64 + 1);
            body.text(tag);
            for child in children {
                body.child(child.child_ref());
            }
            staging
                .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
                .unwrap()
        };
        let leaves: Vec<_> = (1..size)
            .map(|index| encode(&format!("batch-{size}-leaf-{index}"), &[]))
            .collect();
        let root = (size > 0).then(|| encode(&format!("batch-{size}-root"), &leaves));
        let nodes = staging.into_nodes();
        for (attempt, absent) in [(0, size), (1, 0)] {
            let mut tx = tier
                .store
                .begin(&actor, Epoch(identity.epoch))
                .await
                .unwrap();
            tx.stage(nodes.values().cloned());
            if let Some(root) = root {
                tx.write(DomainWrite::ReleasePin(ReleasePinWrite::Install {
                    release: format!("batch-{size}-{attempt}"),
                    root,
                }));
            }
            let (answer, receipt) = lash_core_execution::facade_support::sql::collect(
                "graph.batch",
                "sqlite",
                tier.store.commit(tx, CommitLabel::new("graph.batch")),
            )
            .await;
            answer.unwrap();
            assert_eq!(receipt.unretained_shape_statements, 0, "{receipt:?}");
            for (sql, shape) in &receipt.shapes {
                if sql.starts_with("insert into main.blobs ") {
                    assert!(shape.rows_affected <= 32 * shape.statements, "{receipt:?}");
                }
            }
            let inserts = count(&receipt, "blobs");
            assert!(
                inserts
                    <= if absent <= 64 {
                        absent as u64
                    } else {
                        absent.div_ceil(32) as u64
                    },
                "{absent} absent bodies used {inserts} statements: {receipt:?}"
            );
            assert!(
                count(&receipt, "blob_edges") <= u64::from(absent > 1),
                "{receipt:?}"
            );
            let bytes = if absent == 0 {
                0
            } else {
                nodes.values().map(|node| node.len()).sum()
            };
            assert_eq!(receipt.newly_installed_graph_bytes, bytes);
            for (id, node) in &nodes {
                assert_eq!(
                    tier.blob(&id.to_hex()).await,
                    Some((node.bytes.to_vec(), 1))
                );
                assert_eq!(tier.edges(id).await, node.children().unwrap());
            }
        }
        if size == 65 {
            // Held bytes and encoding are checked before any batch writes,
            // including when the commit also supplies an absent body.
            let held = root.unwrap();
            let mut extra = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            let mut body = TokenWriter::new();
            body.child(held.child_ref());
            let added = extra
                .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(body.as_bytes()))
                .unwrap();
            let mut mixed = nodes.clone();
            mixed.extend(extra.into_nodes());
            let before = tier.blobs().await;
            let mut corrupt = nodes[&held].bytes.to_vec();
            *corrupt.last_mut().unwrap() ^= 1;
            tier.foreign(ForeignWrite::OverwriteBlob {
                node: held,
                bytes: corrupt,
            })
            .await;
            for refusal in [
                GraphRefusal::ContentConflict(held),
                GraphRefusal::EncodingConflict(held),
            ] {
                if matches!(refusal, GraphRefusal::EncodingConflict(_)) {
                    tier.raw(
                        "UPDATE blobs SET encoding = 0 WHERE hash = ?1".into(),
                        vec![hex(&held)],
                    )
                    .await;
                }
                let mut tx = tier
                    .store
                    .begin(&actor, Epoch(identity.epoch))
                    .await
                    .unwrap();
                tx.stage(mixed.values().cloned());
                tx.write(DomainWrite::ReleasePin(ReleasePinWrite::Install {
                    release: "refused-batch".into(),
                    root: added,
                }));
                assert_eq!(
                    tier.store
                        .commit(tx, CommitLabel::new("graph.batch.refused"))
                        .await,
                    Err(lash_durable::DurableError::Graph(refusal))
                );
                assert_eq!(
                    tier.blobs().await,
                    before,
                    "a refused batch installs nothing"
                );
                assert!(tier.edges(&added).await.is_empty());
                assert!(!tier.roots().await.contains(&added.to_hex()));
            }
        }
    }
}
