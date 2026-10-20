//! The checkpoint graph's store laws (`lash_durable::laws::graph`) over
//! SQLite, in memory and in a file.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core_ids::test_clock::TestClock;
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
                    .query_map([], |row| crate::codec::hash_column(row, 0))?
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
    rusqlite::types::Value::Blob(node.0.to_vec())
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
                    [crate::codec::SqlHash(&hash)],
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
                .query_map([crate::codec::SqlHash(&parent)], |row| {
                    crate::codec::hash_column(row, 0)
                })?
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
            ForeignWrite::ModelCallsDelete { session } => {
                self.store
                    .connection_for_testing()
                    .call(move |conn| {
                        crate::durable::retire_session_rows(conn, &session)?;
                        Ok(())
                    })
                    .await
                    .expect("physical session row retirement");
                return;
            }

            ForeignWrite::OpaqueBlob { hash, bytes } => (
                "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 0)",
                vec![
                    rusqlite::types::Value::Blob(NodeId::from_hex(&hash).unwrap().0.to_vec()),
                    Value::Blob(bytes),
                ],
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
            ForeignWrite::TerminalTranscript {
                session,
                run,
                root: Some(root),
            } => (
                "INSERT INTO session_runs
                     (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms,
                      terminal_transcript_ref)
                 VALUES (?1, ?2, 'cancelled', '{\"cause\":\"operator_cancelled\"}', 1, ?3)",
                vec![text(session.as_str()), text(run.as_str()), hex(&root)],
            ),
            ForeignWrite::TerminalTranscript {
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

    async fn edge_rows(&self) -> BTreeSet<(NodeId, NodeId)> {
        self.store
            .connection_for_testing()
            .call(|conn| {
                conn.prepare("SELECT parent_hash, child_hash FROM blob_edges")?
                    .query_map([], |row| {
                        Ok((
                            crate::codec::hash_column(row, 0)?,
                            crate::codec::hash_column(row, 1)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
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

    async fn drop_blob(&self, node: &NodeId) {
        for sql in [
            "DELETE FROM blob_edges WHERE parent_hash = ?1 OR child_hash = ?1",
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

    async fn delete_session(&self, session: &str) -> u64 {
        let store = self._set.open_store().await.expect("the session store");
        let session =
            lash_protocol_types::SessionId::try_from(session.to_owned()).expect("a law session id");
        let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
            "pages.session-delete",
            "sqlite",
            lash_core_store::store::SessionCatalogStore::delete_session(store.as_ref(), &session),
        )
        .await;
        answer.expect("the session delete commits");
        receipt.rows_affected
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

/// D-RET (FIG-6423): a deleted session's 50,000-page heap retires in
/// bounded steps, not in the delete's transaction.
mod deleting_a_session_retires_its_heap_in_bounded_steps {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory() {
        let tier = tier(false).await;
        pages::deleting_a_session_retires_its_heap_in_bounded_steps(&tier, 50_000)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file() {
        let tier = tier(true).await;
        pages::deleting_a_session_retires_its_heap_in_bounded_steps(&tier, 50_000)
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

/// GRAPH-PEEL-WORK: retirement touches edges only of nodes it deletes, and
/// reads their children from their own bytes (FIG-6939). A pinned root
/// and a subtree another parent reaches both stop the peel before their
/// children; releasing the last references reclaims them.
#[tokio::test]
async fn retiring_a_root_does_not_read_edges_of_retained_descendants() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    use lash_durable::{ActorKey, CommitLabel, Epoch};

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
    let pinned = encode("pinned root", &leaves[..16]);
    let shared = encode("shared subtree", &leaves[16..]);
    let holder = encode("other parent", &[shared]);
    let retired = encode("retired root", &[pinned, shared]);
    let retire = unpin;
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
    let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
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
    let edge_rows =
        |verb: &str, rows: fn(&lash_core_ids::operational_metrics::sql::Shape) -> u64| -> u64 {
            receipt
                .shapes
                .iter()
                .filter(|(sql, _)| sql.starts_with(verb) && sql.contains("main.blob_edges"))
                .map(|(_, shape)| rows(shape))
                .sum()
        };
    eprintln!(
        "GRAPH-PEEL SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(
        edge_rows("delete from main.blob_edges", |shape| shape.rows_affected),
        2,
        "only the deleted root's edges go"
    );
    assert_eq!(
        edge_rows("select ", |shape| shape.rows_returned),
        0,
        "retained descendants' edges are never read"
    );

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

/// GRAPH-PEEL-EDGES (FIG-6939): a peeled node's edges go with it even when
/// its bytes are no longer the node it is stored under, so a child it
/// reached is reclaimed with it rather than held by an edge forever.
#[tokio::test]
async fn a_peeled_node_whose_bytes_changed_still_releases_its_children() {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    use lash_durable::{ActorKey, CommitLabel, Epoch};

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
    let leaves: Vec<_> = (0..3)
        .map(|index| encode(&format!("leaf-{index}"), &[]))
        .collect();
    let root = encode("changed root", &leaves);
    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.stage(staging.into_nodes().into_values());
    tx.write(pin("changed", root));
    tier.store
        .commit(tx, CommitLabel::new("peel.install"))
        .await
        .unwrap();
    assert_eq!(tier.edges(&root).await.len(), leaves.len());
    let (mut bytes, _) = tier.blob(&root.to_hex()).await.unwrap();
    bytes.truncate(bytes.len() / 2);
    tier.foreign(ForeignWrite::OverwriteBlob { node: root, bytes })
        .await;

    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.write(unpin("changed", root));
    tier.store
        .commit(tx, CommitLabel::new("peel.retire"))
        .await
        .unwrap();
    settle(&tier.store).await.unwrap();
    assert!(
        tier.edges(&root).await.is_empty(),
        "the peeled root's edges went with it"
    );
    assert_eq!(
        tier.blobs().await,
        initial,
        "the children it reached are reclaimed with it"
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
    let session = lash_protocol_types::SessionId::parse("catalog-law").expect("a session id");
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
    use lash_durable::{ActorKey, CommitLabel, Epoch};

    let tier = tier(false).await;
    let identity = lash_core_execution::testing::sql_work::owner_commit(&tier._set).await;
    let actor = ActorKey::session("sql-work-owner").unwrap();
    let count = |receipt: &lash_core_ids::operational_metrics::sql::Receipt, table: &str| {
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
                tx.write(pin(&format!("batch-{size}-{attempt}"), root));
            }
            let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
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
                tx.write(pin("refused-batch", added));
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

/// FIG-6420: one park publication writes a counted number of SQL
/// statements, and none of them records a semantic admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_park_publication_writes_a_counted_number_of_statements() {
    use lash_core_store::checkpoint_graph::stage::{
        CandidateManifest, EdgeLog, ManifestEntry, ManifestSummary, MemoryEdgeLog, StageFault,
        node_range,
    };
    use lash_core_store::checkpoint_graph::{
        CommitId, EncodeBudget, EncodedNode, ExplicitLimit, GraphStaging, NodeKind, TokenWriter,
        TypedBody,
    };
    use lash_durable::domain::graph::{EffectAdmissionWrite, ExecRootWrite};
    use lash_durable::domain::pages::{CandidateOwner, CandidatePublication};
    use lash_durable::domain::{CellId, DomainWrite, EffectKey, ExecKey};
    use lash_durable::{ActorKey, CommitLabel, Epoch};

    /// A page of `kind` naming `tag` and `children`.
    fn encode(kind: NodeKind, tag: &str, children: &[NodeId]) -> EncodedNode {
        let mut out = TokenWriter::new();
        out.seq(1 + children.len() as u64);
        out.text(tag);
        for child in children {
            out.child(child.child_ref());
        }
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let id = staging
            .encode(kind, TypedBody::logical(out.as_bytes()))
            .expect("a counted page encodes");
        staging.into_nodes().remove(&id).expect("the staged page")
    }

    /// A candidate held whole: its entries in digest order.
    struct Listed {
        summary: ManifestSummary,
        entries: Vec<ManifestEntry>,
    }

    impl Listed {
        fn new(root: NodeId, commit: CommitId, fresh: Vec<EncodedNode>) -> Self {
            let summary = ManifestSummary {
                root,
                base: None,
                commit,
                fresh_nodes: fresh.len() as u64,
                fresh_bytes: fresh.iter().map(EncodedNode::len).sum(),
                fresh_edges: fresh
                    .iter()
                    .map(|node| node.children().map_or(0, |children| children.len() as u64))
                    .sum(),
                external: 0,
            };
            let mut entries: Vec<ManifestEntry> = fresh
                .into_iter()
                .map(|node| {
                    ManifestEntry::Fresh(
                        lash_core_store::checkpoint_graph::node::VerifiedNode::check(
                            node,
                            &lash_core_store::checkpoint_graph::DecodeLimits::within_total(
                                u64::MAX,
                            ),
                        )
                        .expect("a staged page checks"),
                    )
                })
                .collect();
            entries.sort_by_key(|entry| match entry {
                ManifestEntry::External(id) => *id,
                ManifestEntry::Fresh(node) => node.id,
            });
            Self { summary, entries }
        }
    }

    impl CandidateManifest for Listed {
        fn summary(&self) -> ManifestSummary {
            self.summary
        }

        fn entries(
            &mut self,
            after: Option<NodeId>,
            limit: usize,
        ) -> Result<Vec<ManifestEntry>, StageFault> {
            let id = |entry: &ManifestEntry| match entry {
                ManifestEntry::External(id) => *id,
                ManifestEntry::Fresh(node) => node.id,
            };
            Ok(self
                .entries
                .iter()
                .filter(|entry| after.is_none_or(|after| id(entry) > after))
                .take(limit)
                .cloned()
                .collect())
        }

        fn fresh_range(
            &mut self,
            id: &NodeId,
            offset: u64,
            len: usize,
        ) -> Result<Vec<u8>, StageFault> {
            match self
                .entries
                .binary_search_by_key(id, |entry| match entry {
                    ManifestEntry::External(id) => *id,
                    ManifestEntry::Fresh(node) => node.id,
                })
                .map(|at| &self.entries[at])
            {
                Ok(ManifestEntry::Fresh(node)) => node_range(node, offset, len),
                _ => Err(StageFault::Missing(*id)),
            }
        }

        fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
            Ok(Box::new(MemoryEdgeLog::default()))
        }
    }

    let tier = tier(false).await;
    let identity = lash_core_execution::testing::sql_work::owner_commit(&tier._set).await;
    let actor = ActorKey::session("sql-work-owner").expect("actor");
    let exec = ExecKey::Cell(
        lash_protocol_types::SessionId::parse("counted-park").expect("a session id"),
        lash_protocol_types::TurnId::try_from("counted-turn".to_owned()).expect("a turn id"),
        CellId::new("counted-cell"),
    );
    let leaf = encode(NodeKind::PAYLOAD_LEAF, "counted leaf", &[]);
    let checkpoint = encode(NodeKind::CHECKPOINT_ROOT, "counted root", &[leaf.id]);
    let exec_root = encode(NodeKind::FROZEN_VALUE, "exec root", &[checkpoint.id]);
    let effect_node = encode(NodeKind::FROZEN_VALUE, "admission", &[]);
    let commit = CommitId::mint();
    let manifest = Listed::new(checkpoint.id, commit, vec![leaf, checkpoint]);
    let mut tx = tier
        .store
        .begin(&actor, Epoch(identity.epoch))
        .await
        .unwrap();
    tx.stage([exec_root.clone(), effect_node.clone()]);
    tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
        exec: exec.clone(),
        effect: EffectKey(Arc::from(&b"counted"[..])),
        member: None,
        admission: effect_node.id,
    }));
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: None,
        expected_root: None,
        root: exec_root.id,
        commit_id: commit,
    }));
    let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "park.publish",
        "sqlite",
        tier.store.commit_candidate(
            tx,
            CandidatePublication {
                owner: CandidateOwner::ExecRootPut(exec),
                manifest: Box::new(manifest),
            },
            CommitLabel::new("park.publish"),
        ),
    )
    .await;
    answer.unwrap();
    eprintln!(
        "PARK-PUBLICATION SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    // The park's write statements: inserts, updates and deletes only.
    let writes: u64 = receipt
        .shapes
        .iter()
        .filter(|(sql, _)| {
            ["insert", "update", "delete"]
                .iter()
                .any(|verb| sql.starts_with(verb))
        })
        .map(|(_, shape)| shape.statements)
        .sum();
    // The park's write count, pinned: the publication's rows and its effect
    // admission, with no semantic-admission row.
    const EXPECTED: u64 = 5;
    assert_eq!(writes, EXPECTED, "the park's write count: {receipt:?}");
}

mod turn_and_owned_model_calls_survive_an_idempotent_outcome_commit {
    use super::*;

    async fn run(file: bool) {
        let tier = two_node_tier(file).await;
        let peer = tier.peer.as_ref().map_or(&tier.store, |(_, store)| store);
        laws::turn_and_owned_model_calls_survive_an_idempotent_outcome_commit(&tier, peer)
            .await
            .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test]
    async fn memory() {
        run(false).await;
    }

    #[tokio::test]
    async fn file() {
        run(true).await;
    }
}

/// The execution of a root a test pins at will under `key`: one effect
/// admission of a cell of a session no test owns.
fn pin_exec(key: &str) -> lash_durable::domain::ExecKey {
    lash_durable::domain::ExecKey::Cell(
        lash_protocol_types::SessionId::parse("test-pins").expect("a session id"),
        lash_protocol_types::TurnId::try_from("pins".to_owned()).expect("a turn id"),
        lash_durable::domain::CellId::new(key),
    )
}

/// Pin `root` under `key`.
fn pin(key: &str, root: NodeId) -> lash_durable::DomainWrite {
    lash_durable::DomainWrite::EffectAdmission(
        lash_durable::domain::graph::EffectAdmissionWrite::Put {
            exec: pin_exec(key),
            effect: lash_durable::domain::graph::EffectKey(vec![0].into()),
            member: None,
            admission: root,
        },
    )
}

/// Retire the pin of `expected` under `key`.
fn unpin(key: &str, expected: NodeId) -> lash_durable::DomainWrite {
    lash_durable::DomainWrite::EffectAdmission(
        lash_durable::domain::graph::EffectAdmissionWrite::Remove {
            exec: pin_exec(key),
            effect: lash_durable::domain::graph::EffectKey(vec![0].into()),
            expected,
        },
    )
}
