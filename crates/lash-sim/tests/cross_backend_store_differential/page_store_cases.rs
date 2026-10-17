//! The persistent-heap page store (FIG-6191, S6) compared across the three
//! backends: one scripted sequence of a candidate's publication and
//! execution row, a root lease's reads, renewal and release, a replacing
//! publication, the retirement steps that reclaim the replaced root, and a
//! reader's unfenced lease must answer identically.
//!
//! Retirement steps are store-wide, so each backend runs on a database of
//! its own: PostgreSQL on an isolated database beside the shared one, and
//! the pages' digests and the steps' counts compare as they are.

use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EdgeLog, ManifestEntry, ManifestSummary, MemoryEdgeLog, StageFault,
    node_range,
};
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, EncodedNode, ExplicitLimit, GraphRefusal, GraphStaging, NodeId,
    NodeKind, TokenWriter, TypedBody,
};
use lash_durable::domain::graph::{ExecRootWrite, RootOwner};
use lash_durable::domain::pages::{
    CandidatePublication, LeaseFence, PageStore, RetirementBudget, RootLeaseRequest,
};
use lash_durable::domain::{CellId, DomainWrite, ExecKey, SnapshotRev};
use lash_durable::{ActorKey, DurableError, Epoch, FormatSet, MailTx, NodeSpec};

use super::*;

/// One backend's answers to the script.
type Transcript = Vec<String>;

/// A page of `kind` naming `tag` and `children`.
#[expect(
    clippy::expect_used,
    reason = "test support: a page the script builds always encodes"
)]
fn page(kind: NodeKind, tag: &str, children: &[NodeId]) -> EncodedNode {
    let mut out = TokenWriter::new();
    out.seq(1 + children.len() as u64);
    out.text(tag);
    for child in children {
        out.child(child.child_ref());
    }
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let id = staging
        .encode(kind, TypedBody::logical(out.as_bytes()))
        .expect("encode a scripted page");
    staging.into_nodes().remove(&id).expect("the staged page")
}

/// A candidate held whole: its entries in digest order.
struct Scripted {
    summary: ManifestSummary,
    entries: Vec<ManifestEntry>,
}

impl Scripted {
    fn new(
        root: NodeId,
        base: Option<NodeId>,
        commit: CommitId,
        fresh: Vec<EncodedNode>,
        external: Vec<NodeId>,
    ) -> Self {
        let summary = ManifestSummary {
            root,
            base,
            commit,
            fresh_nodes: fresh.len() as u64,
            fresh_bytes: fresh.iter().map(EncodedNode::len).sum(),
            fresh_edges: fresh
                .iter()
                .map(|node| node.children().map_or(0, |children| children.len() as u64))
                .sum(),
            external: external.len() as u64,
        };
        let mut entries: Vec<ManifestEntry> = fresh
            .into_iter()
            .map(ManifestEntry::Fresh)
            .chain(external.into_iter().map(ManifestEntry::External))
            .collect();
        entries.sort_by_key(entry_id);
        Self { summary, entries }
    }
}

fn entry_id(entry: &ManifestEntry) -> NodeId {
    match entry {
        ManifestEntry::External(id) => *id,
        ManifestEntry::Fresh(node) => node.id,
    }
}

impl CandidateManifest for Scripted {
    fn summary(&self) -> ManifestSummary {
        self.summary
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        Ok(self
            .entries
            .iter()
            .filter(|entry| after.is_none_or(|after| entry_id(entry) > after))
            .take(limit)
            .cloned()
            .collect())
    }

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        match self
            .entries
            .binary_search_by_key(id, entry_id)
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

/// Publishes `manifest` under the execution root over its CheckpointRoot.
async fn publish(
    store: &dyn PageStore,
    actor: &ActorKey,
    epoch: Epoch,
    exec: &ExecKey,
    base: Option<(u64, NodeId)>,
    manifest: Scripted,
) -> Result<(NodeId, CommitId, NodeId), DurableError> {
    let summary = manifest.summary;
    let exec_root = page(NodeKind::FROZEN_VALUE, "exec root", &[summary.root]);
    let mut tx = store.begin(actor, epoch).await?;
    tx.stage([exec_root.clone()]);
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: base.map(|(rev, _)| SnapshotRev(rev)),
        expected_root: base.map(|(_, root)| root),
        root: exec_root.id,
        commit_id: summary.commit,
    }));
    let receipt = store
        .commit_candidate(
            tx,
            CandidatePublication {
                owner: lash_durable::domain::pages::CandidateOwner::ExecRootPut(exec.clone()),
                manifest: Box::new(manifest),
            },
            lash_durable::CommitLabel::new("pages.publish"),
        )
        .await?;
    Ok((receipt.root, receipt.commit, exec_root.id))
}

/// Runs retirement steps until nothing is held: how many nodes they
/// deleted.
#[expect(
    clippy::expect_used,
    reason = "test support: a refused step panics the harness with its backend by design"
)]
async fn drain(store: &dyn PageStore) -> u64 {
    let mut deleted = 0;
    loop {
        let progress = store
            .retire_step(RetirementBudget { nodes: 2 })
            .await
            .expect("a retirement step");
        if progress.holds_advanced == 0 && progress.leases_reaped == 0 {
            return deleted;
        }
        deleted += progress.nodes_deleted;
    }
}

fn answer<T: std::fmt::Debug>(result: &Result<T, DurableError>) -> String {
    match result {
        Ok(value) => format!("ok {value:?}"),
        Err(DurableError::Graph(refusal)) => format!("refused {refusal:?}"),
        Err(error) => format!("failed {error}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot run the script panics the harness with its name by design"
)]
async fn page_transcript(store: &dyn PageStore) -> Transcript {
    let mut out = Transcript::new();
    let actor = ActorKey::session("fig-6191-pages").expect("a session actor");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), FormatSet::new("pages-formats"));
    store
        .commit_mail(create, lash_durable::CommitLabel::new("pages.create"))
        .await
        .expect("create the actor");
    let node = store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("fig-6191-node"),
            decodes: vec![FormatSet::new("pages-formats")],
            serves: Vec::new(),
            ttl_millis: 60_000,
        })
        .await
        .expect("register the node");
    let epoch = store
        .claim(&node, 1)
        .await
        .expect("claim the actor")
        .first()
        .map(|claimed| claimed.epoch)
        .expect("the actor was claimed");
    let exec = ExecKey::Cell(
        SessionId::try_from("fig-6191-pages".to_owned()).expect("a session id"),
        lash_protocol_types::TurnId::try_from("pages-turn".to_owned()).expect("a turn id"),
        CellId::new("pages-cell"),
    );

    // The first candidate: two leaves under an index under its root.
    let kept = page(NodeKind::KEYED_LEAF, "kept leaf", &[]);
    let dropped = page(NodeKind::KEYED_LEAF, "dropped leaf", &[]);
    let index = page(NodeKind::KEYED_INDEX, "first index", &[kept.id, dropped.id]);
    let first = page(NodeKind::CHECKPOINT_ROOT, "first root", &[index.id]);
    let first_commit = CommitId([1; 32]);
    let (root, commit, first_exec) = publish(
        store,
        &actor,
        epoch,
        &exec,
        None,
        Scripted::new(
            first.id,
            None,
            first_commit,
            vec![kept.clone(), dropped, index, first.clone()],
            Vec::new(),
        ),
    )
    .await
    .expect("publish the first candidate");
    out.push(format!(
        "published {} {}",
        root == first.id,
        commit == first_commit
    ));
    out.push(answer(&store.candidate_state(&exec).await));

    let lease = store
        .acquire_root_lease(RootLeaseRequest {
            owner: RootOwner::Exec(exec.clone()),
            expected: first_exec,
            fence: LeaseFence::Actor {
                actor: actor.clone(),
                epoch,
            },
            ttl_millis: 60_000,
        })
        .await
        .expect("lease the first root");
    out.push(format!("leased {}", lease.root == first_exec));
    let read = store.read_pages(&lease, &[first.id, kept.id]).await;
    out.push(answer(&read.map(|pages| {
        pages
            .iter()
            .map(|page| (page.id, page.bytes.len()))
            .collect::<Vec<_>>()
    })));
    let lease = store
        .renew_root_lease(&lease)
        .await
        .expect("renew the lease");

    // The second candidate replaces the first and names its kept leaf.
    let fresh = page(NodeKind::KEYED_LEAF, "fresh leaf", &[]);
    let second_index = page(NodeKind::KEYED_INDEX, "second index", &[kept.id, fresh.id]);
    let second = page(NodeKind::CHECKPOINT_ROOT, "second root", &[second_index.id]);
    let second_page = second.id;
    let replaced = publish(
        store,
        &actor,
        epoch,
        &exec,
        Some((1, first_exec)),
        Scripted::new(
            second.id,
            Some(first_exec),
            CommitId([2; 32]),
            vec![fresh, second_index, second],
            vec![kept.id],
        ),
    )
    .await;
    let second_exec = replaced.as_ref().ok().map(|(_, _, exec_root)| *exec_root);
    out.push(answer(&replaced.map(|(root, _, _)| root)));
    out.push(format!(
        "backlog {:?}",
        store
            .retirement_backlog()
            .await
            .map(|backlog| backlog.holds)
    ));

    // The lease keeps the replaced root whole through retirement.
    out.push(format!("retired under the lease {}", drain(store).await));
    let read = store.read_pages(&lease, &[first.id]).await;
    out.push(answer(&read.map(|pages| pages.len())));

    store
        .release_root_lease(lease.id)
        .await
        .expect("release the lease");
    out.push(answer(&store.renew_root_lease(&lease).await.map(|_| ())));
    out.push(answer(
        &store
            .read_pages(&lease, &[first.id])
            .await
            .map(|pages| pages.len()),
    ));
    out.push(format!("retired after release {}", drain(store).await));
    out.push(format!(
        "backlog {:?}",
        store
            .retirement_backlog()
            .await
            .map(|backlog| backlog.holds)
    ));
    let stale = store
        .acquire_root_lease(RootLeaseRequest {
            owner: RootOwner::Exec(exec.clone()),
            expected: first_exec,
            fence: LeaseFence::Actor {
                actor: actor.clone(),
                epoch,
            },
            ttl_millis: 60_000,
        })
        .await;
    out.push(answer(&stale));
    assert!(
        matches!(
            stale,
            Err(DurableError::Graph(GraphRefusal::RevisionConflict))
        ),
        "a lease on a replaced root was not refused: {stale:?}"
    );

    // A reader's lease is checked against no fence: it reads and renews the
    // current root, keeps it whole through retirement until released, and
    // its row names no actor.
    let second_exec = second_exec.expect("the second candidate's execution root");
    let reader = store
        .acquire_root_lease(RootLeaseRequest {
            owner: RootOwner::Exec(exec.clone()),
            expected: second_exec,
            fence: LeaseFence::Reader,
            ttl_millis: 60_000,
        })
        .await
        .expect("a reader's lease on the current root");
    out.push(format!(
        "reader {:?} {}",
        reader.fence,
        reader.root == second_exec
    ));
    out.push(answer(
        &store
            .read_pages(&reader, &[second_page])
            .await
            .map(|pages| pages.len()),
    ));
    let reader = store
        .renew_root_lease(&reader)
        .await
        .expect("renew the reader's lease");
    out.push(format!("retired under the reader {}", drain(store).await));
    store
        .release_root_lease(reader.id)
        .await
        .expect("release the reader's lease");
    out.push(answer(&store.renew_root_lease(&reader).await.map(|_| ())));
    out
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open panics the harness with its name by design"
)]
pub(super) async fn compare_page_stores(sqlite_root: &Path, database_url: &str) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the SQLite memory page store");
    let file = lash_sqlite_store::SqliteStoreSet::open(
        sqlite_root.join("page-store.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("open the SQLite file page store");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(database_url).await;
    let postgres = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("open the isolated PostgreSQL page store");
    let (memory, file, postgres) = (
        memory.durable_store(),
        file.durable_store(),
        postgres.durable_store(),
    );
    let backends: [(&str, &dyn PageStore); 3] = [
        ("sqlite-memory", &memory),
        ("sqlite", &file),
        ("postgres", &postgres),
    ];
    let mut observations = Vec::new();
    for (name, store) in backends {
        observations.push((name, page_transcript(store).await));
    }
    for pair in observations.windows(2) {
        let ((left, left_answers), (right, right_answers)) = (&pair[0], &pair[1]);
        assert_eq!(
            left_answers, right_answers,
            "page store answers differ between {left} and {right}"
        );
    }
    eprintln!(
        "PASS page_store: backends=3 steps={}",
        observations[0].1.len()
    );
}
