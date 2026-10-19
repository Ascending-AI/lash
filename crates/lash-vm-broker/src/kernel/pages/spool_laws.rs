//! S5's laws: the staging spool and the candidate manifest (FIG-6190), on
//! a disk spool behind the parent's page service and a fake leased store.
#![expect(
    clippy::disallowed_methods,
    reason = "the crash laws list the staging directory a test owns"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lash_core_store::checkpoint_graph::node::frame_direct;
use lash_core_store::checkpoint_graph::stage::{
    ManifestEntry, StageFault, StageLeaseId, StageSpool,
};
use lash_core_store::checkpoint_graph::{
    CommitId, EncodedNode, GraphRefusal, MAX_NODE_BYTES, NodeId, NodeKind, TokenWriter,
};
use lash_durable::domain::ExecKey;
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::pages::{LeaseFence, RootLease, RootLeaseId, StoredPage};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch};
use lash_kernel_doc::heap::{HeapFault, HostResource, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageBody, PageKind, PageSource};
use lash_vm_protocol::page::{PageCapability, PageEnvelope, PageRefusal, PageReply, PageRequest};
use lash_vm_protocol::{InfrastructureOutcome, ProtocolBounds};

use lash_core_execution::runtime::SystemClock;

use super::shared::renew_when_due;
use super::spool::ENTRY_BYTES;
use super::{
    ActivationPages, CapabilityKey, CommittedPages, LeasedReads, LocalPages, PageExchange,
    PageLimits, Resolution, SpoolRoot, cache,
};

/// A sequence leaf naming `children`, told apart by `label`.
fn page(label: &str, children: &[NodeId]) -> EncodedNode {
    let mut tokens = TokenWriter::new();
    tokens.seq(children.len() as u64 + 1);
    tokens.text(label);
    for child in children {
        tokens.child(child.child_ref());
    }
    frame_direct(
        NodeKind(PageKind::SEQUENCE_LEAF.0),
        tokens.as_bytes(),
        MAX_NODE_BYTES,
    )
    .expect("a leaf frames")
}

/// The committed pages behind a root lease.
#[derive(Default)]
struct Committed {
    pages: std::sync::Mutex<BTreeMap<NodeId, Arc<[u8]>>>,
    lost: AtomicBool,
}

impl Committed {
    fn commit(&self, node: &EncodedNode) {
        self.pages
            .lock()
            .expect("pages")
            .insert(node.id, node.bytes.clone());
    }
}

#[async_trait::async_trait]
impl LeasedReads for Committed {
    async fn read_pages(
        &self,
        _lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        if self.lost.load(Ordering::SeqCst) {
            return Err(DurableError::Graph(GraphRefusal::RootRetired));
        }
        let held = self.pages.lock().expect("pages");
        Ok(pages
            .iter()
            .filter_map(|id| {
                held.get(id).map(|bytes| StoredPage {
                    id: *id,
                    bytes: bytes.clone(),
                })
            })
            .collect())
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        Ok(lease.clone())
    }
}

impl CommittedPages for Arc<Committed> {
    fn page(&self, page: &NodeId) -> Option<Arc<[u8]>> {
        self.pages.lock().expect("pages").get(page).cloned()
    }
}

fn lease(root: NodeId) -> RootLease {
    RootLease {
        id: RootLeaseId(7),
        owner: RootOwner::Exec(ExecKey::Process(lash_protocol_types::ProcessId::fixture(
            "pages",
        ))),
        root,
        fence: LeaseFence::Actor {
            actor: ActorKey::process("pages").expect("actor"),
            epoch: Epoch(1),
        },
        expires_at: DurableInstant(i64::MAX),
    }
}

/// An activation leased on `root`, staging under `dir` within `quota`.
fn activation(
    store: &Arc<Committed>,
    root: &SpoolRoot,
    base: NodeId,
) -> ActivationPages<Committed> {
    ActivationPages::new(
        Arc::clone(store),
        Some(lease(base)),
        Duration::from_secs(20),
        CapabilityKey::fresh(),
        3,
        Box::new(root.spool(StageLeaseId(11)).expect("spool")),
        Arc::new(SystemClock),
        Duration::from_secs(60),
        PageLimits::within(ProtocolBounds::standard().decode),
    )
}

async fn seal(
    pages: &mut ActivationPages<Committed>,
    node: &EncodedNode,
    children: Vec<PageCapability>,
) -> PageReply {
    pages
        .serve(PageRequest::Seal {
            pages: vec![PageEnvelope {
                id: node.id.0,
                envelope: node.bytes.to_vec(),
            }],
            children,
        })
        .await
}

fn sealed(reply: PageReply) -> PageCapability {
    match reply {
        PageReply::Sealed { pages } => pages[0],
        other => panic!("the page seals: {other:?}"),
    }
}

/// The manifest's entries, `limit` at a time from `after`.
fn drain(
    manifest: &mut dyn lash_core_store::checkpoint_graph::stage::CandidateManifest,
    mut after: Option<NodeId>,
    limit: usize,
) -> Vec<ManifestEntry> {
    let mut all = Vec::new();
    loop {
        let batch = manifest.entries(after, limit).expect("entries");
        if batch.is_empty() {
            return all;
        }
        after = Some(match batch.last().expect("an entry") {
            ManifestEntry::External(id) => *id,
            ManifestEntry::Fresh(node) => node.id,
        });
        all.extend(batch);
    }
}

fn id_of(entry: &ManifestEntry) -> NodeId {
    match entry {
        ManifestEntry::External(id) => *id,
        ManifestEntry::Fresh(node) => node.id,
    }
}

/// A candidate's manifest is exactly the staged DAG its root reaches,
/// stopping at committed references: one ascending digest stream of
/// fresh and external entries, restartable from any cursor. A staged page
/// the root does not reach stays until the owner's retain drops it.
#[test]
fn a_manifest_is_the_reachable_staged_dag_in_digest_order() {
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let mut spool = root.spool(StageLeaseId(1)).expect("spool");
    let (c1, c2) = (page("c1", &[]), page("c2", &[]));
    let committed: BTreeSet<NodeId> = [c1.id, c2.id].into();
    let protected = |id: &NodeId| committed.contains(id);
    let a = page("a", &[]);
    let b = page("b", &[a.id, c1.id]);
    let top = page("top", &[b.id, a.id, c2.id]);
    let orphan = page("orphan", &[c1.id]);
    for node in [&a, &b, &orphan, &top] {
        spool.seal(node.clone(), &protected).expect("staged");
    }
    // Sealing the same page twice is one page.
    spool.seal(a.clone(), &protected).expect("staged again");
    assert_eq!(spool.usage().pages, 4);

    let commit = CommitId([9; 32]);
    let mut manifest = spool
        .candidate(top.id, Some(c1.id), commit)
        .expect("candidate");
    let summary = manifest.summary();
    assert_eq!((summary.fresh_nodes, summary.external), (3, 2));
    assert_eq!(summary.fresh_edges, 2 + 3);
    assert_eq!(
        summary.fresh_bytes,
        [&a, &b, &top].iter().map(|node| node.len()).sum::<u64>()
    );
    let entries = drain(manifest.as_mut(), None, 2);
    let ids: Vec<NodeId> = entries.iter().map(id_of).collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "one ascending stream"
    );
    let fresh: BTreeSet<NodeId> = entries
        .iter()
        .filter_map(|entry| match entry {
            ManifestEntry::Fresh(node) => {
                assert_eq!(spool.read(&node.id).expect("staged").as_ref(), &*node.bytes);
                Some(node.id)
            }
            ManifestEntry::External(_) => None,
        })
        .collect();
    assert_eq!(fresh, [a.id, b.id, top.id].into());
    let external: BTreeSet<NodeId> = ids
        .iter()
        .copied()
        .filter(|id| !fresh.contains(id))
        .collect();
    assert_eq!(external, committed);
    // Restartable: from any cursor the stream is its own tail.
    for cut in 0..ids.len() {
        let tail: Vec<NodeId> = drain(manifest.as_mut(), Some(ids[cut]), 1)
            .iter()
            .map(id_of)
            .collect();
        assert_eq!(tail, ids[cut + 1..]);
    }
    // The orphan stays until the spool's owner retains only what it
    // needs, and the spool counts what it keeps.
    assert!(spool.contains(&orphan.id));
    spool.retain(&[top.id]).expect("retained");
    assert_eq!(spool.read(&orphan.id), Err(StageFault::Missing(orphan.id)));
    assert_eq!(spool.usage().pages, 3);
    assert_eq!(spool.usage().bytes, summary.fresh_bytes);
    // A page staged beside the candidate survives its settlement, which
    // deletes only the candidate's own pages.
    let beside = page("beside", &[c2.id]);
    spool.seal(beside.clone(), &protected).expect("staged");
    spool.settle(commit).expect("settled");
    assert!(!spool.contains(&top.id) && !spool.contains(&a.id));
    assert!(spool.contains(&beside.id));
    assert_eq!(spool.usage().pages, 1);
}

/// SMALL-MANIFEST-BOUNDED: a small candidate reserves one sort record per
/// reachable fresh or external page, plus the two fixed streaming buffers.
/// Ordering, child authority and settlement still apply at every size.
#[test]
fn small_manifest_bounded() {
    const RECORD_BYTES: u64 = 33;
    const STREAM_BUFFERS: u64 = 2 * (8 << 10);
    const LARGE_RUN_BYTES: u64 = 4096 * RECORD_BYTES;
    for leaves in [0, 6, 62] {
        let dir = tempfile::tempdir().expect("dir");
        let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
        let mut spool = root.spool(StageLeaseId(1)).expect("spool");
        let committed = page("committed", &[]);
        let mut nodes: Vec<_> = (0..leaves)
            .map(|at| page(&format!("leaf {at}"), &[]))
            .collect();
        for node in &nodes {
            spool.seal(node.clone(), &|_| false).expect("staged");
        }
        let mut children: Vec<_> = nodes.iter().map(|node| node.id).collect();
        children.push(committed.id);
        let top = page("top", &children);
        assert_eq!(
            spool.seal(top.clone(), &|_| false),
            Err(StageFault::Unauthorized {
                page: top.id,
                child: committed.id,
            })
        );
        spool
            .seal(top.clone(), &|id| *id == committed.id)
            .expect("authorized child");
        nodes.push(top.clone());
        let meter = spool.meter();
        let commit = CommitId([8; 32]);
        let mut manifest = spool
            .candidate(top.id, Some(committed.id), commit)
            .expect("candidate");
        let summary = manifest.summary();
        assert_eq!((summary.fresh_nodes, summary.external), (leaves + 1, 1));
        let records = summary.fresh_nodes + summary.external;
        let sort_bytes = records * RECORD_BYTES;
        assert!(sort_bytes < LARGE_RUN_BYTES);
        assert_eq!(
            meter.peak(),
            STREAM_BUFFERS + sort_bytes,
            "SMALL-MANIFEST-BOUNDED: {records} records reserve their actual bytes"
        );
        let entries = drain(manifest.as_mut(), None, 3);
        let ids: Vec<_> = entries.iter().map(id_of).collect();
        let mut expected: Vec<_> = nodes.iter().map(|node| node.id).collect();
        expected.push(committed.id);
        expected.sort_unstable();
        assert_eq!(ids, expected);
        for entry in entries {
            match entry {
                ManifestEntry::Fresh(node) => assert_eq!(
                    node,
                    *nodes
                        .iter()
                        .find(|known| known.id == node.id)
                        .expect("fresh page")
                ),
                ManifestEntry::External(id) => assert_eq!(id, committed.id),
            }
        }
        let beside = page("beside", &[]);
        spool.seal(beside.clone(), &|_| false).expect("beside");
        spool.settle(commit).expect("settled");
        assert!(nodes.iter().all(|node| !spool.contains(&node.id)));
        assert!(spool.contains(&beside.id));
        assert_eq!(spool.usage().pages, 1);
    }
}

/// The most digest-record bytes a spool holds in memory at once while it
/// builds, streams and settles a candidate, whatever the candidate's size.
const METADATA_BOUND: u64 = 1 << 20;
/// How many times a candidate's metadata may touch each page's record,
/// from the walk through two manifest passes to settlement.
const TOUCHES_PER_PAGE: u64 = 12;

/// A 200,000-page candidate is created, streamed twice in the store's
/// 64-entry batches (nodes, then edges) and settled while the spool
/// holds at most [`METADATA_BOUND`] bytes of digest records at once. Each
/// page's record is touched a fixed number of times, so the total work is
/// linear in the candidate (FIG-6421). Both are counted, not timed.
#[test]
fn a_large_candidate_builds_streams_and_settles_in_bounded_memory_and_linear_work() {
    let dir = tempfile::tempdir().expect("dir");
    // On disk from the first seal, where a spool holds nothing per page.
    let root = SpoolRoot::open(dir.path(), 64 << 20)
        .expect("root")
        .resident_within(0);
    let mut spool = root.spool(StageLeaseId(1)).expect("spool");
    let committed = page("committed", &[]);
    let protected = |id: &NodeId| *id == committed.id;
    let mut level: Vec<NodeId> = (0..193_750)
        .map(|leaf| {
            let node = page(&format!("leaf {leaf}"), &[]);
            spool.seal(node.clone(), &protected).expect("staged");
            node.id
        })
        .collect();
    let mut staged = level.len() as u64;
    let mut depth = 0;
    while level.len() > 1 {
        depth += 1;
        level = level
            .chunks(32)
            .enumerate()
            .map(|(at, children)| {
                let node = page(&format!("index {depth}/{at}"), children);
                spool.seal(node.clone(), &protected).expect("staged");
                node.id
            })
            .collect();
        staged += level.len() as u64;
    }
    let top = page("top", &[level[0], committed.id]);
    spool.seal(top.clone(), &protected).expect("staged");
    staged += 1;
    assert!(staged >= 200_000);
    let meter = spool.meter();
    let commit = CommitId([4; 32]);
    let mut manifest = spool.candidate(top.id, None, commit).expect("candidate");
    assert_eq!(
        (manifest.summary().fresh_nodes, manifest.summary().external),
        (staged, 1)
    );
    for pass in ["nodes", "edges"] {
        let (mut after, mut fresh, mut external) = (None, 0_u64, 0_u64);
        loop {
            let batch = manifest.entries(after, 64).expect("entries");
            if batch.is_empty() {
                break;
            }
            for entry in &batch {
                assert!(after < Some(id_of(entry)), "{pass}: one ascending stream");
                after = Some(id_of(entry));
                match entry {
                    ManifestEntry::Fresh(_) => fresh += 1,
                    ManifestEntry::External(_) => external += 1,
                }
            }
        }
        assert_eq!((fresh, external), (staged, 1), "{pass}");
    }
    spool.settle(commit).expect("settled");
    assert_eq!(spool.usage().pages, 0);
    let (touched, peak) = (meter.touched(), meter.peak());
    eprintln!("candidate of {staged} pages: records touched {touched}, peak held {peak} bytes");
    assert!(
        peak <= METADATA_BOUND,
        "the spool held {peak} record bytes against a bound of {METADATA_BOUND}"
    );
    assert!(
        touched <= TOUCHES_PER_PAGE * staged,
        "the spool touched {touched} records for {staged} pages"
    );
}

/// Regular files and links under `dir`, at any depth.
fn files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("dir")
        .map(|entry| entry.expect("entry").path())
        .map(|path| {
            if path.is_dir() && !path.is_symlink() {
                files(&path)
            } else {
                1
            }
        })
        .sum()
}

/// FIG-6902: within its resident bound a spool stages a cell's pages,
/// builds its candidate and settles it without a file per page: past the
/// parents' locks, its directory only ever holds the standing candidate's
/// manifest. A spool whose seals pass the bound moves every page and
/// count to its directory once and keeps them exactly: the same seals
/// give the same usage, retention, manifest and settlement.
#[test]
fn a_cell_within_the_resident_bound_opens_no_file_per_page() {
    let committed = page("committed", &[]);
    let protected = |id: &NodeId| *id == committed.id;
    let run = |root: &SpoolRoot, dir: &std::path::Path| {
        let idle = files(dir);
        let mut spool = root.spool(StageLeaseId(1)).expect("spool");
        let orphan = page("orphan", &[committed.id]);
        spool.seal(orphan.clone(), &protected).expect("staged");
        let mut level: Vec<NodeId> = (0..2_048)
            .map(|leaf| {
                let node = page(&format!("leaf {leaf}"), &[]);
                spool.seal(node.clone(), &protected).expect("staged");
                node.id
            })
            .collect();
        let mut depth = 0;
        while level.len() > 1 {
            depth += 1;
            level = level
                .chunks(32)
                .enumerate()
                .map(|(at, children)| {
                    let node = page(&format!("index {depth}/{at}"), children);
                    spool.seal(node.clone(), &protected).expect("staged");
                    node.id
                })
                .collect();
        }
        let top = page("top", &[level[0], committed.id]);
        spool.seal(top.clone(), &protected).expect("staged");
        let staged = (spool.usage(), spool.entries(), files(dir) - idle);
        spool.retain(&[top.id]).expect("retained");
        assert_eq!(spool.read(&orphan.id), Err(StageFault::Missing(orphan.id)));
        let commit = CommitId([4; 32]);
        let mut manifest = spool.candidate(top.id, None, commit).expect("candidate");
        let entries: Vec<(NodeId, Option<Arc<[u8]>>)> = drain(manifest.as_mut(), None, 64)
            .into_iter()
            .map(|entry| match entry {
                ManifestEntry::Fresh(node) => (node.id, Some(node.bytes)),
                ManifestEntry::External(id) => (id, None),
            })
            .collect();
        let candidate = files(dir) - idle;
        spool.settle(commit).expect("settled");
        assert_eq!(spool.usage().pages, 0);
        assert_eq!(files(dir), idle, "settlement leaves no file behind");
        (spool.on_disk(), staged, entries, candidate)
    };
    let dir = tempfile::tempdir().expect("dir");
    let resident = run(
        &SpoolRoot::open(dir.path(), 64 << 20).expect("root"),
        dir.path(),
    );
    let (on_disk, (usage, _, staged_files), _, candidate_files) = &resident;
    assert_eq!(*staged_files, 0, "staging opened a file per page");
    assert!(!on_disk, "the cell stays within the bound");
    assert_eq!(
        *candidate_files, 1,
        "a standing candidate holds its manifest"
    );

    let dir = tempfile::tempdir().expect("dir");
    let bound = usage.bytes / 2;
    let moved = run(
        &SpoolRoot::open(dir.path(), 64 << 20)
            .expect("root")
            .resident_within(bound),
        dir.path(),
    );
    assert!(moved.0, "the seals past the bound moved the spool");
    assert!(
        moved.1.2 as u64 >= usage.pages,
        "every page moved to a file"
    );
    assert_eq!((moved.1.0, moved.1.1), (resident.1.0, resident.1.1));
    assert_eq!(moved.2, resident.2, "one manifest wherever the pages are");
}

/// A staged page may name only committed pages a capability protects or
/// pages staged before it: anything else is refused, so the staged graph
/// is a DAG by construction. A forged capability protects nothing, and
/// bytes that do not hash to their identity never stage.
#[tokio::test]
async fn staging_refuses_an_unprotected_child() {
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let mut spool = root.spool(StageLeaseId(1)).expect("spool");
    let stranger = page("stranger", &[]);
    let parent = page("parent", &[stranger.id]);
    assert_eq!(
        spool.seal(parent.clone(), &|_| false),
        Err(StageFault::Unauthorized {
            page: parent.id,
            child: stranger.id
        })
    );
    let mut forged = parent.bytes.to_vec();
    *forged.last_mut().expect("a byte") ^= 1;
    assert_eq!(
        spool.seal(
            EncodedNode {
                id: parent.id,
                bytes: forged.into()
            },
            &|_| true
        ),
        Err(StageFault::HashMismatch(parent.id))
    );
    assert_eq!(spool.usage().pages, 0);

    // Through the page service: a committed child the activation holds a
    // capability for stages; a guessed digest under a forged MAC does not.
    let store = Arc::new(Committed::default());
    let base = page("base", &[stranger.id]);
    store.commit(&stranger);
    store.commit(&base);
    let mut pages = activation(&store, &root, base.id);
    let PageReply::Read { pages: read } = pages
        .serve(PageRequest::Read {
            pages: vec![pages.issue(base.id)],
        })
        .await
    else {
        panic!("the root reads")
    };
    let child = read[0].as_ref().expect("page reads").children[0];
    assert_eq!(child.page, stranger.id.0);
    sealed(seal(&mut pages, &parent, vec![child]).await);
    let other = page("other parent", &[stranger.id]);
    let mut guessed = child;
    guessed.mac = [0; 32];
    assert_eq!(
        seal(&mut pages, &other, vec![guessed]).await,
        PageReply::Refused(PageRefusal::Unauthorized)
    );
    // The refusal ends the activation.
    assert_eq!(
        pages
            .serve(PageRequest::Read {
                pages: vec![pages.issue(base.id)],
            })
            .await,
        PageReply::Refused(PageRefusal::Unauthorized)
    );
}

/// A child's capability is its provenance (FIG-6197 F2): a staged page's
/// child that is a reused committed page is issued under the root lease,
/// never the spool's. After a spill the worker reads the staged parent
/// again and seals a new page over the reused child before any
/// publication, and the child still resolves: it reads from the store and
/// protects the new seal.
#[tokio::test]
async fn a_reused_committed_child_resolves_after_a_spill() {
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let store = Arc::new(Committed::default());
    let reused = page("reused", &[]);
    let base = page("base", &[reused.id]);
    store.commit(&reused);
    store.commit(&base);
    let mut pages = activation(&store, &root, base.id);
    let PageReply::Read { pages: read } = pages
        .serve(PageRequest::Read {
            pages: vec![pages.issue(base.id)],
        })
        .await
    else {
        panic!("the root reads")
    };
    let child = read[0].as_ref().expect("page reads").children[0];
    // Stage a parent over the reused committed child, as a spill does.
    let parent = page("parent", &[reused.id]);
    let staged = sealed(seal(&mut pages, &parent, vec![child]).await);
    // Read the spilled parent again: its reused child keeps root authority.
    let PageReply::Read { pages: reread } = pages
        .serve(PageRequest::Read {
            pages: vec![staged],
        })
        .await
    else {
        panic!("the staged parent reads")
    };
    let again = reread[0].as_ref().expect("page reads").children[0];
    assert_eq!(again.page, reused.id.0);
    assert_eq!(again.authority, pages.issue(base.id).authority);
    // The child resolves from the store, and protects a new seal over it.
    let PageReply::Read { pages: leaf } =
        pages.serve(PageRequest::Read { pages: vec![again] }).await
    else {
        panic!("the reused child resolves")
    };
    assert_eq!(
        leaf[0].as_ref().expect("page reads").page.envelope,
        reused.bytes.to_vec()
    );
    let top = page("top", &[parent.id, reused.id]);
    let top_capability = sealed(seal(&mut pages, &top, vec![staged, again]).await);
    let mut manifest = pages
        .candidate(
            NodeId(top_capability.page),
            Some(base.id),
            page("exec root", &[top.id]).id,
            CommitId([4; 32]),
        )
        .expect("candidate");
    let summary = manifest.summary();
    assert_eq!((summary.fresh_nodes, summary.external), (2, 1));
    assert_eq!(drain(manifest.as_mut(), None, 8).len(), 3);
}

/// D-SPOOL, and the lost-receipt crash cut: a publication whose outcome
/// is unknown keeps its spool. Read against the execution row, a row that
/// holds the base or another root is not committed and the candidate
/// stands; the candidate's own commit settles and deletes it. The
/// activation's staged pages stay readable until then, and a committed
/// candidate's pages read from the store after.
#[tokio::test]
async fn an_ambiguous_publication_keeps_its_spool_until_resolved() {
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let store = Arc::new(Committed::default());
    let kept = page("kept", &[]);
    let base = page("base", &[kept.id]);
    store.commit(&kept);
    store.commit(&base);
    let mut pages = activation(&store, &root, base.id);
    let PageReply::Read { pages: read } = pages
        .serve(PageRequest::Read {
            pages: vec![pages.issue(base.id)],
        })
        .await
    else {
        panic!("the root reads")
    };
    let kept_cap = read[0].as_ref().expect("page reads").children[0];
    let fresh = page("fresh", &[kept.id]);
    let fresh_cap = sealed(seal(&mut pages, &fresh, vec![kept_cap]).await);
    let top = page("top", &[fresh.id, kept.id]);
    let top_cap = sealed(seal(&mut pages, &top, vec![fresh_cap, kept_cap]).await);

    let commit = CommitId([4; 32]);
    let wrapper = page("exec root", &[top.id]).id;
    pages
        .candidate(NodeId(top_cap.page), Some(base.id), wrapper, commit)
        .expect("candidate");
    // The receipt is lost. Worker disconnection settles nothing: the spool
    // still holds the candidate, and its pages still read.
    let held = pages.usage();
    assert_eq!(held.pages, 2);
    assert!(matches!(
        pages
            .serve(PageRequest::Read {
                pages: vec![top_cap]
            })
            .await,
        PageReply::Read { .. }
    ));
    // A row that still holds the base, or names the commit under another
    // root (FIG-6197 F1), is not this candidate's publication; its spool
    // stands.
    assert!(matches!(
        pages
            .resolve(Some((base.id, CommitId([1; 32]))))
            .expect("resolves"),
        Resolution::NotCommitted
    ));
    assert!(matches!(
        pages.resolve(Some((top.id, commit))).expect("resolves"),
        Resolution::NotCommitted
    ));
    assert_eq!(pages.usage(), held);
    // The receipt naming the candidate's root and commit: settled,
    // deleted.
    assert!(matches!(
        pages.resolve(Some((wrapper, commit))).expect("resolves"),
        Resolution::Committed
    ));
    assert_eq!((pages.usage().pages, pages.usage().bytes), (0, 0));
    // The committed pages are the store's now, and read under the lease.
    store.commit(&fresh);
    store.commit(&top);
    let PageReply::Read { pages: read } = pages
        .serve(PageRequest::Read {
            pages: vec![top_cap],
        })
        .await
    else {
        panic!("a committed candidate's page reads from the store")
    };
    assert_eq!(read[0].as_ref().expect("page reads").page.id, top.id.0);

    // Another activation's candidate loses to a root it did not build on.
    let mut loser = ActivationPages::new(
        Arc::clone(&store),
        Some(lease(base.id)),
        Duration::from_secs(20),
        CapabilityKey::fresh(),
        5,
        Box::new(root.spool(StageLeaseId(12)).expect("spool")),
        Arc::new(SystemClock),
        Duration::from_secs(60),
        PageLimits::within(ProtocolBounds::standard().decode),
    );
    let lone = page("lone", &[]);
    let lone_cap = sealed(seal(&mut loser, &lone, Vec::new()).await);
    loser
        .candidate(
            NodeId(lone_cap.page),
            Some(base.id),
            page("lone root", &[lone.id]).id,
            CommitId([5; 32]),
        )
        .expect("candidate");
    assert!(matches!(
        loser.resolve(Some((wrapper, commit))).expect("resolves"),
        Resolution::NotCommitted
    ));
    assert_eq!(
        loser.usage().pages,
        1,
        "kept until the activation discards it"
    );
    loser.discard();
}

/// Staging quota exhaustion is a host outcome: the page service refuses
/// with `StagingExhausted`, the run's outcome is the retryable
/// infrastructure `PageRefused`, and the heap sees `HeapFault::Exhausted`
/// on staging bytes, never the guest's memory bound. Spilling published
/// nothing: the committed root's pages are untouched.
#[tokio::test]
async fn a_quota_failure_is_a_host_outcome() {
    let dir = tempfile::tempdir().expect("dir");
    let small = page("small", &[]);
    let large = page(&"x".repeat(2_000), &[]);
    let quota = small.len() + ENTRY_BYTES + 10;
    let root = SpoolRoot::open(dir.path(), quota).expect("root");
    let store = Arc::new(Committed::default());
    let base = page("base", &[]);
    store.commit(&base);
    let mut pages = activation(&store, &root, base.id);
    sealed(seal(&mut pages, &small, Vec::new()).await);
    let refusal = PageRefusal::StagingExhausted { bytes: quota };
    assert_eq!(
        seal(&mut pages, &large, Vec::new()).await,
        PageReply::Refused(refusal.clone())
    );
    let outcome = InfrastructureOutcome::PageRefused { refusal };
    assert!(
        outcome.is_retryable(),
        "a host verdict, redriven from the committed root"
    );
    assert_eq!(store.pages.lock().expect("pages").len(), 1);

    // The in-process port states the same as the heap's host fault.
    let mut local = LocalPages::new(
        Arc::clone(&store),
        Box::new(root.spool(StageLeaseId(2)).expect("spool")),
    );
    let body = |text: &str| {
        let mut tokens = TokenWriter::new();
        tokens.text(text);
        PageBody {
            kind: PageKind::PAYLOAD_LEAF,
            tokens: tokens.into_bytes(),
            children: Vec::new(),
        }
    };
    let sealed_ref = local.seal(body("fits")).expect("fits the quota");
    assert_eq!(
        local.read(&[sealed_ref]).expect("reads back")[0]
            .as_ref()
            .expect("page reads")
            .kind,
        PageKind::PAYLOAD_LEAF
    );
    assert_eq!(
        local.seal(body(&"y".repeat(4_000))),
        Err(HeapFault::Exhausted {
            resource: HostResource::StagingBytes
        })
    );
    // A committed page reads; an unknown one is missing, never absent.
    let unknown = PageRef {
        id: PageId([3; 32]),
        grant: PageGrant::NONE,
    };
    assert_eq!(
        local
            .read(&[unknown])
            .and_then(|answers| answers.into_iter().collect::<Result<Vec<_>, _>>())
            .map(|answers| answers.len()),
        Err(HeapFault::Missing { page: unknown.id })
    );
}

/// The committed pages behind a root lease that lives on `clock`: the
/// lease expires `ttl` after its last renewal, `renew_root_lease` succeeds
/// only before then (and counts itself), and `read_pages` refuses an
/// expired lease.
struct Expiring {
    committed: Committed,
    clock: Arc<lash_durable_test::SimClock>,
    ttl: Duration,
    expires_at: std::sync::Mutex<Instant>,
    renewals: AtomicUsize,
    /// Signalled on every renewal.
    renewed: tokio::sync::Notify,
}

impl Expiring {
    /// A store over `clock` whose lease expires `ttl` after its last
    /// renewal, `now` from when it is built.
    fn new(clock: &Arc<lash_durable_test::SimClock>, ttl: Duration) -> Self {
        use lash_core_execution::runtime::Clock;
        Self {
            committed: Committed::default(),
            expires_at: std::sync::Mutex::new(clock.now() + ttl),
            clock: Arc::clone(clock),
            ttl,
            renewals: AtomicUsize::new(0),
            renewed: tokio::sync::Notify::new(),
        }
    }

    fn commit(&self, node: &EncodedNode) {
        self.committed.commit(node);
    }

    fn renewals(&self) -> usize {
        self.renewals.load(Ordering::SeqCst)
    }

    /// Returns once the store has counted `count` renewals.
    async fn renewed(&self, count: usize) {
        loop {
            let renewed = self.renewed.notified();
            if self.renewals() >= count {
                return;
            }
            renewed.await;
        }
    }
}

#[async_trait::async_trait]
impl LeasedReads for Expiring {
    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        use lash_core_execution::runtime::Clock;
        if self.clock.now() >= *self.expires_at.lock().expect("expiry") {
            return Err(DurableError::Graph(GraphRefusal::LeaseLost));
        }
        self.committed.read_pages(lease, pages).await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        use lash_core_execution::runtime::Clock;
        let now = self.clock.now();
        let mut expires_at = self.expires_at.lock().expect("expiry");
        if now >= *expires_at {
            return Err(DurableError::Graph(GraphRefusal::LeaseLost));
        }
        *expires_at = now + self.ttl;
        self.renewals.fetch_add(1, Ordering::SeqCst);
        self.renewed.notify_waiters();
        Ok(lease.clone())
    }
}

/// D-LEASE: an activation's lease is checked on every exchange, not only
/// when a read misses the cache: a warm read after the lease expired is
/// refused exactly like a cold one. The activation renews its lease while
/// it is live; after it lapses the store decides, and the cached bytes of
/// a committed page answer nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_cache_read_after_the_lease_expired_is_refused_like_a_cold_one() {
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let clock = lash_durable_test::SimClock::new();
    let ttl = Duration::from_secs(3);
    let store = Arc::new(Expiring::new(&clock, ttl));
    let base = page("base", &[]);
    store.commit(&base);
    let build = |stage: u64| {
        ActivationPages::new(
            Arc::clone(&store),
            Some(lease(base.id)),
            ttl / 3,
            CapabilityKey::fresh(),
            stage,
            Box::new(root.spool(StageLeaseId(stage)).expect("spool")),
            clock.clone(),
            Duration::from_secs(60),
            PageLimits::within(ProtocolBounds::standard().decode),
        )
    };
    let mut warm = build(21).caching(cache::PageCache::new(1 << 20));
    let mut cold = build(22);
    // The root reads once through the cached activation, which keeps its
    // bytes for the next read.
    let capability = warm.issue(base.id);
    let PageReply::Read { .. } = warm
        .serve(PageRequest::Read {
            pages: vec![capability],
        })
        .await
    else {
        panic!("the root reads")
    };
    clock.advance_by(2 * ttl.as_millis() as u64).await;
    let capability = warm.issue(base.id);
    let warmed = warm
        .serve(PageRequest::Read {
            pages: vec![capability],
        })
        .await;
    let capability = cold.issue(base.id);
    let read = cold
        .serve(PageRequest::Read {
            pages: vec![capability],
        })
        .await;
    assert_eq!(warmed, read);
    assert_eq!(warmed, PageReply::Refused(PageRefusal::LeaseLost));
    assert_eq!(warm.refusal(), Some(&PageRefusal::LeaseLost));
}

/// D-LEASE: an activation parked on a long tool keeps its lease: the
/// schedule the held activation arms renews it at a third of its ttl, so a
/// tool twice the ttl does not lose it and a read after it still answers.
///
/// The law drives the clock from one thread with the held loop, so it runs
/// only while the loop is parked: a sleeper it sees armed is one the loop
/// already waits on, never one the loop armed and is still polling. On two
/// worker threads the clock could fire the renewal due with the tool's
/// before the loop's poll that armed it returned, and that poll then found
/// it due and renewed a sixth time (FIG-6792).
#[tokio::test]
async fn a_held_activation_renews_its_lease_through_a_tool_twice_its_ttl() {
    use lash_core_execution::runtime::Clock;
    let dir = tempfile::tempdir().expect("dir");
    let root = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
    let clock = lash_durable_test::SimClock::new();
    let ttl = Duration::from_secs(3);
    let store = Arc::new(Expiring::new(&clock, ttl));
    let base = page("base", &[]);
    store.commit(&base);
    let pages = ActivationPages::new(
        Arc::clone(&store),
        Some(lease(base.id)),
        ttl / 3,
        CapabilityKey::fresh(),
        23,
        Box::new(root.spool(StageLeaseId(23)).expect("spool")),
        clock.clone(),
        Duration::from_secs(60),
        PageLimits::within(ProtocolBounds::standard().decode),
    );
    let pages = Arc::new(tokio::sync::Mutex::new(pages));
    let held = tokio::spawn({
        let pages = Arc::clone(&pages);
        let clock = Arc::clone(&clock);
        async move {
            let mut tool = std::pin::pin!(clock.sleep(2 * ttl));
            loop {
                tokio::select! {
                    biased;
                    () = &mut tool => break,
                    () = renew_when_due(&pages) => {}
                }
            }
        }
    });
    // Walk the clock to each renewal due and wait for the store to count
    // that renewal before moving on, so the tool's own due never overtakes
    // a renewal still in flight. The tool's sleep is registered first: it
    // measures its 6 s from the start.
    let renew_every = (ttl / 3).as_millis() as u64;
    let tool_due = 2 * ttl.as_millis() as u64;
    clock.wait_for_sleep(tool_due).await;
    for (count, due) in (renew_every..tool_due)
        .step_by(renew_every as usize)
        .enumerate()
    {
        clock.advance_to(due).await;
        tokio::time::timeout(Duration::from_secs(10), store.renewed(count + 1))
            .await
            .unwrap_or_else(|_| panic!("the renewal due at {due} ms never ran"));
    }
    // The renewal due with the tool's own is armed beside it before the
    // clock moves there: the held loop is then parked on both, and the
    // tool, armed first and polled first, wins the tie. Moving earlier
    // lets the loop, still returning from the last renewal, find the
    // renewal already due and renew a sixth time (FIG-6683).
    tokio::time::timeout(
        Duration::from_secs(10),
        clock.wait_for_sleepers(tool_due, 2),
    )
    .await
    .expect("the held loop arms its renewal beside the tool");
    clock.advance_to(tool_due).await;
    held.await.expect("the held task ran");
    // A 1 s renewal schedule through a 6 s tool: renewals at each second's
    // tick before the tool's own.
    assert_eq!(store.renewals(), 5, "the held activation's renewals");
    assert!(pages.lock().await.refusal().is_none());
    let capability = pages.lock().await.issue(base.id);
    let PageReply::Read { .. } = pages
        .lock()
        .await
        .serve(PageRequest::Read {
            pages: vec![capability],
        })
        .await
    else {
        panic!("the root still reads")
    };
}

/// The parents' directories under the staging directory `dir`.
fn parent_dirs(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("dir")
        .filter(|entry| entry.as_ref().expect("entry").path().is_dir())
        .count()
}

/// The parent crash cuts: after a spill, after a candidate is sealed, and
/// after the spool moved past its resident bound to its directory. The
/// spool dies with its parent, in memory or on disk; the next parent reaps
/// its directory (and never a live parent's), and a fresh activation
/// leases the committed root, which the crash left exactly as it was.
#[tokio::test]
async fn a_crashed_parents_spool_is_reaped_and_the_committed_root_stands() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(Committed::default());
    let kept = page("kept", &[]);
    let base = page("base", &[kept.id]);
    store.commit(&kept);
    store.commit(&base);
    let before = store.pages.lock().expect("pages").clone();
    for cut in ["after a spill", "after a candidate", "after a move to disk"] {
        let crashed = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
        let crashed = match cut {
            "after a move to disk" => crashed.resident_within(0),
            _ => crashed,
        };
        let mut pages = activation(&store, &crashed, base.id);
        let top = page(cut, &[]);
        let top_cap = sealed(seal(&mut pages, &top, Vec::new()).await);
        if cut == "after a candidate" {
            pages
                .candidate(
                    NodeId(top_cap.page),
                    Some(base.id),
                    page("exec root", &[top.id]).id,
                    CommitId([6; 32]),
                )
                .expect("candidate");
        }
        let live = SpoolRoot::open(dir.path(), 1 << 20).expect("a live sibling");
        assert_eq!(
            parent_dirs(dir.path()),
            2,
            "{cut}: a live parent's spool is never reaped"
        );
        // The parent dies: nothing runs its spool's drop.
        std::mem::forget(pages);
        drop(crashed);
        drop(live);
        let next = SpoolRoot::open(dir.path(), 1 << 20).expect("root");
        assert_eq!(
            parent_dirs(dir.path()),
            1,
            "{cut}: only the new parent's directory is left"
        );
        assert_eq!(*store.pages.lock().expect("pages"), before, "{cut}");
        let mut fresh = activation(&store, &next, base.id);
        let PageReply::Read { pages: read } = fresh
            .serve(PageRequest::Read {
                pages: vec![fresh.issue(base.id)],
            })
            .await
        else {
            panic!("{cut}: the committed root reads")
        };
        assert_eq!(read[0].as_ref().expect("page reads").page.id, base.id.0);
        drop(fresh);
        drop(next);
    }
}

/// Parents that open their spools under one staging directory at once
/// never reap each other (FIG-6438): each one's directory stands, and
/// stages, from its open on, while a sibling's open reaps the parents
/// that died around it.
#[test]
fn parents_opening_at_once_never_reap_a_live_sibling() {
    const PARENTS: usize = 8;
    const OPENS: usize = 200;
    let dir = tempfile::tempdir().expect("dir");
    let start = std::sync::Barrier::new(PARENTS);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let parents: Vec<_> = (0..PARENTS)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let mut failures = Vec::new();
                    for open in 0..OPENS {
                        // A parent that drops its root dies: the next open
                        // reaps its directory.
                        let root = match SpoolRoot::open(dir.path(), 1 << 20) {
                            Ok(root) => root,
                            Err(error) => {
                                failures.push(format!("open {open}: {error}"));
                                continue;
                            }
                        };
                        if let Err(error) = root.spool(StageLeaseId(open as u64)) {
                            failures.push(format!("spool {open}: {error}"));
                        }
                    }
                    failures
                })
            })
            .collect();
        parents
            .into_iter()
            .flat_map(|parent| parent.join().expect("a parent"))
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} opens failed: {:?}",
        failures.len(),
        PARENTS * OPENS,
        &failures[..failures.len().min(5)]
    );
}

/// FIG-6512: the quota charges each staged page and each distinct
/// external reference its manifest scratch once, beside its page bytes:
/// duplicate seals and shared references add nothing, a reference that
/// becomes staged keeps its one entry, a page that would pass the quota is
/// refused before anything is written, and retention and settlement
/// release what they remove.
#[test]
fn the_quota_charges_each_distinct_entry_once_and_releases_it() {
    let dir = tempfile::tempdir().expect("dir");
    let a = page("external-a", &[]);
    let b = page("external-b", &[]);
    let first = page("first", &[a.id, a.id]);
    let second = page("second", &[a.id]);
    let third = page("third", &[b.id]);
    let len = |node: &EncodedNode| node.bytes.len() as u64;
    let quota = len(&first) + len(&second) + len(&a) + 3 * ENTRY_BYTES;
    let root = SpoolRoot::open(dir.path(), quota).expect("root");
    let mut spool = root.spool(StageLeaseId(1)).expect("spool");
    spool
        .seal(first.clone(), &|_| true)
        .expect("a page and its one distinct ref");
    assert_eq!(spool.entries(), 2);
    spool
        .seal(first.clone(), &|_| true)
        .expect("a duplicate seal");
    assert_eq!(spool.entries(), 2);
    spool
        .seal(second.clone(), &|_| true)
        .expect("a shared ref counts once");
    assert_eq!(spool.entries(), 3);
    let before = spool.usage();
    assert_eq!(
        spool.seal(third.clone(), &|_| true),
        Err(StageFault::Quota { quota })
    );
    assert_eq!((spool.usage(), spool.entries()), (before, 3));
    assert!(!spool.contains(&third.id));
    spool
        .seal(a.clone(), &|_| true)
        .expect("a staged ref keeps its entry, exactly filling the quota");
    assert_eq!(spool.entries(), 3);
    assert_eq!(spool.usage().bytes + 3 * ENTRY_BYTES, quota);
    assert_eq!(
        spool.seal(page("extra", &[]), &|_| true),
        Err(StageFault::Quota { quota })
    );

    spool.retain(&[second.id]).expect("keeps second and a");
    assert_eq!(spool.entries(), 2);
    assert_eq!(spool.usage().bytes, len(&second) + len(&a));
    let commit = CommitId([61; 32]);
    drop(spool.candidate(second.id, None, commit).expect("candidate"));
    spool.settle(commit).expect("settles second and a");
    assert_eq!((spool.entries(), spool.usage().bytes), (0, 0));
    spool
        .seal(third.clone(), &|_| true)
        .expect("settlement freed the quota");
    assert_eq!(spool.entries(), 2);
    let commit = CommitId([62; 32]);
    drop(
        spool
            .candidate(third.id, None, commit)
            .expect("external candidate"),
    );
    spool
        .settle(commit)
        .expect("settlement releases the last external ref");
    assert_eq!((spool.entries(), spool.usage().bytes), (0, 0));
}

/// HEAP-SPOOL-OWNER: a measured runtime counts its own successful writes,
/// including successive activations, while another runtime stages pages
/// during the same collector interval. Deduplicated pages count once.
#[cfg(feature = "perf-witness")]
#[test]
fn spool_bytes_exclude_an_overlapping_runtime() {
    let fixture = tempfile::tempdir().unwrap();
    let measured = std::sync::Arc::new(SpoolRoot::open(fixture.path(), 1 << 20).unwrap());
    let foreign = SpoolRoot::open(fixture.path(), 1 << 20).unwrap();
    let page = |kind, text: &str| {
        let mut tokens = TokenWriter::new();
        tokens.text(text);
        frame_direct(NodeKind(kind), tokens.as_bytes(), MAX_NODE_BYTES).unwrap()
    };
    let own_page = page(1002, "measured");
    let other_page = page(1005, &"foreign".repeat(4096));
    let mut seed = measured.spool(StageLeaseId(0)).unwrap();
    seed.seal(own_page.clone(), &|_| false).unwrap();
    let counter = std::sync::Arc::clone(&measured);
    let collector = lash_core_execution::perf_witness::Collector::install()
        .unwrap()
        .with_spool_bytes(move || counter.spooled_bytes());
    let mut first = measured.spool(StageLeaseId(1)).unwrap();
    first.seal(own_page.clone(), &|_| false).unwrap();
    first.seal(own_page.clone(), &|_| false).unwrap();
    std::thread::spawn(move || {
        let mut other = foreign.spool(StageLeaseId(2)).unwrap();
        other.seal(other_page, &|_| false).unwrap();
    })
    .join()
    .unwrap();
    let mut second = measured.spool(StageLeaseId(3)).unwrap();
    second.seal(own_page.clone(), &|_| false).unwrap();
    assert_eq!(
        collector.snapshot().spool_bytes,
        2 * own_page.bytes.len() as u64,
        "only the measured runtime's two activations wrote these bytes"
    );
}
