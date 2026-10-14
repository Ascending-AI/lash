//! S3's laws (FIG-6190): the heap's page port on a worker, over the run's
//! real host wire, against the parent's page service, its disk spool and a
//! leased store.
//!
//! No machine runs on the paged heap yet (FIG-6188), so each law's worker
//! drives its page source with a scripted reader over the same page shapes
//! the heap uses: a checkpoint root, a heap header and an order-statistic
//! sequence tree. The store is a fake behind the page service's leased
//! reads; the SQLite store's `PageStore` is the publication layer's
//! (FIG-6191).
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a law fails on a broken fixture assumption"
)]

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_store::checkpoint_graph::node::frame_direct;
use lash_core_store::checkpoint_graph::stage::StageLeaseId;
use lash_core_store::checkpoint_graph::{
    EncodedNode, MAX_NODE_BYTES, MAX_ROOT_BYTES, NodeId, NodeKind, TokenReader, TokenWriter,
};
use lash_durable::domain::ExecKey;
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::pages::{RootLease, RootLeaseId, StoredPage};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageKind, PageSource};
use lash_vm_broker::kernel::pages::{
    ActivationPages, CapabilityKey, LeasedReads, PageExchange, PageLimits, SpoolRoot,
};
use lash_vm_client::ipc::{FrameSource, read_frame, write_frame};
use lash_vm_protocol::page::{PageAuthority, PageCapability};
use lash_vm_protocol::*;

use crate::host::{PageTraffic, Wire, WirePageSource, grant_of};
use crate::worker::Fences;

/// The committed pages behind the activation's root lease.
#[derive(Default)]
struct Store {
    pages: Mutex<BTreeMap<NodeId, Arc<[u8]>>>,
}

impl Store {
    fn commit(&self, node: &EncodedNode) {
        self.pages
            .lock()
            .expect("pages")
            .insert(node.id, node.bytes.clone());
    }
}

#[async_trait::async_trait]
impl LeasedReads for Store {
    async fn read_pages(
        &self,
        _lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
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

fn frame(kind: PageKind, tokens: &TokenWriter) -> EncodedNode {
    let bound = if kind == PageKind::CHECKPOINT_ROOT {
        MAX_ROOT_BYTES
    } else {
        MAX_NODE_BYTES
    };
    frame_direct(NodeKind(kind.0), tokens.as_bytes(), bound).expect("a page frames")
}

/// A committed list of `len` integers as a sequence tree (64 cells a leaf,
/// fanout 32), under a heap header and a checkpoint root. Returns the root
/// and how many pages the tree holds.
fn committed_list(store: &Store, len: u64) -> (EncodedNode, usize) {
    let mut level: Vec<(u64, EncodedNode)> = (0..len)
        .step_by(64)
        .map(|start| {
            let end = (start + 64).min(len);
            let mut tokens = TokenWriter::new();
            tokens.seq(end - start);
            for value in start..end {
                tokens.unsigned(value * 3);
            }
            (end - start, frame(PageKind::SEQUENCE_LEAF, &tokens))
        })
        .collect();
    let mut pages = level.len();
    for (_, leaf) in &level {
        store.commit(leaf);
    }
    while level.len() > 1 {
        level = level
            .chunks(32)
            .map(|children| {
                let mut tokens = TokenWriter::new();
                tokens.seq(2 * children.len() as u64);
                for (count, child) in children {
                    tokens.unsigned(*count);
                    tokens.child(child.id.child_ref());
                }
                let count = children.iter().map(|(count, _)| count).sum();
                let index = frame(PageKind::SEQUENCE_INDEX, &tokens);
                store.commit(&index);
                (count, index)
            })
            .collect();
        pages += level.len();
    }
    let list = &level[0].1;
    // The heap header (a keyed leaf) names the list's tree; the checkpoint
    // root names the heap header.
    let mut tokens = TokenWriter::new();
    tokens.seq(2);
    tokens.unsigned(len);
    tokens.child(list.id.child_ref());
    let heap = frame(PageKind::KEYED_LEAF, &tokens);
    store.commit(&heap);
    let mut tokens = TokenWriter::new();
    tokens.seq(2);
    tokens.text("checkpoint");
    tokens.child(heap.id.child_ref());
    let root = frame(PageKind::CHECKPOINT_ROOT, &tokens);
    store.commit(&root);
    (root, pages + 2)
}

/// The child of `answer` its body names at `slot` of its token sequence.
fn child_at(answer: &PageAnswer, digest: [u8; 32]) -> PageRef {
    *answer
        .children
        .iter()
        .find(|child| child.id.0 == digest)
        .expect("a child the envelope declares")
}

fn read_one(source: &mut dyn PageSource, page: PageRef) -> Result<PageAnswer, HeapFault> {
    Ok(source.read(&[page])?.remove(0))
}

/// Opens the scripted checkpoint: the control header, then the heap header.
/// Returns the list's length and tree root.
fn open(source: &mut dyn PageSource, root: PageRef) -> Result<(u64, PageRef), HeapFault> {
    let header = read_one(source, root)?;
    assert_eq!(header.kind, PageKind::CHECKPOINT_ROOT);
    let mut tokens = TokenReader::new(&header.tokens);
    tokens.seq().expect("root");
    tokens.text().expect("root");
    let heap = child_at(&header, tokens.child().expect("heap").0);
    let heap = read_one(source, heap)?;
    let mut tokens = TokenReader::new(&heap.tokens);
    tokens.seq().expect("heap");
    let len = tokens.unsigned().expect("len");
    Ok((len, child_at(&heap, tokens.child().expect("list").0)))
}

/// Element `index` of the list under `page`: one index path to one leaf.
fn element(source: &mut dyn PageSource, page: PageRef, index: u64) -> Result<u64, HeapFault> {
    let answer = read_one(source, page)?;
    let mut tokens = TokenReader::new(&answer.tokens);
    let count = tokens.seq().expect("cells");
    if answer.kind == PageKind::SEQUENCE_LEAF {
        for at in 0..count {
            let value = tokens.unsigned().expect("cell");
            if at == index {
                return Ok(value);
            }
        }
        panic!("index {index} is past the leaf");
    }
    let mut skipped = 0;
    for _ in 0..count / 2 {
        let held = tokens.unsigned().expect("count");
        let child = tokens.child().expect("child");
        if index < skipped + held {
            return element(source, child_at(&answer, child.0), index - skipped);
        }
        skipped += held;
    }
    panic!("index {index} is past the list")
}

/// A native traversal: visits every element of the tree under `page` in
/// order on one Rust stack, charging one unit per element. A page fault
/// inside it blocks in the source and returns into the same frame.
fn visit(
    source: &mut dyn PageSource,
    page: PageRef,
    seen: &mut Vec<u64>,
    charge: &mut u64,
) -> Result<(), HeapFault> {
    let answer = read_one(source, page)?;
    let mut tokens = TokenReader::new(&answer.tokens);
    let count = tokens.seq().expect("cells");
    if answer.kind == PageKind::SEQUENCE_LEAF {
        for _ in 0..count {
            seen.push(tokens.unsigned().expect("cell"));
            *charge += 1;
        }
        return Ok(());
    }
    for _ in 0..count / 2 {
        tokens.unsigned().expect("count");
        let child = tokens.child().expect("child");
        visit(source, child_at(&answer, child.0), seen, charge)?;
    }
    Ok(())
}

fn lease(root: NodeId) -> RootLease {
    RootLease {
        id: RootLeaseId(41),
        owner: RootOwner::Exec(ExecKey::Process(lash_sansio::ProcessId::fixture("paging"))),
        root,
        actor: ActorKey::process("paging").expect("actor"),
        epoch: Epoch(1),
        expires_at: DurableInstant(i64::MAX),
    }
}

/// One activation's parent: its page service over `store`, leased on
/// `root`, with its spool under `spools`.
fn activation(
    store: &Arc<Store>,
    spools: &SpoolRoot,
    root: NodeId,
    key: CapabilityKey,
    epoch: u64,
) -> ActivationPages<Store> {
    ActivationPages::new(
        Arc::clone(store),
        lease(root),
        Duration::from_secs(20),
        key,
        epoch,
        Box::new(spools.spool(StageLeaseId(epoch)).expect("spool")),
        Instant::now() + Duration::from_secs(120),
        PageLimits::within(ProtocolBounds::standard().decode),
    )
}

fn fence() -> MessageFence {
    MessageFence::new(ExecutionLease(9), OwnerEpoch(2), FrameEpoch(1))
}

/// Runs `workload` on a worker thread over the run's real host wire, with
/// a page source keeping `clean` bytes resident, while this thread is its
/// parent: it answers each page exchange from `pages` and nothing else.
/// The worker's statement waits in its exchange; the parent holds no other
/// worker to serve it with.
fn served<T: Send + 'static>(
    pages: &mut ActivationPages<Store>,
    clean: u64,
    workload: impl FnOnce(&mut WirePageSource) -> T + Send + 'static,
) -> (T, PageTraffic) {
    let (worker_pipe, mut pipe) = UnixStream::pair().expect("socket pair");
    let codec = FrameCodec::new(ProtocolBounds::standard().decode);
    let worker = std::thread::spawn({
        let codec = codec.clone();
        move || {
            let wire = Wire::new(
                worker_pipe,
                Arc::new(Mutex::new(FrameSource::with_capacity(
                    NonZeroUsize::new(16 * 1024).expect("nonzero"),
                ))),
                codec,
                Arc::new(Mutex::new(Fences {
                    incoming: Some(fence()),
                    outgoing: fence(),
                    next_read: 0,
                    next_page: 0,
                })),
                Duration::from_secs(5),
                Duration::from_secs(60),
                1 << 20,
            );
            let mut source = WirePageSource::new(Arc::new(wire), clean);
            let out = workload(&mut source);
            (out, source.traffic())
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let (mut incoming, mut outgoing) = (fence(), fence());
    runtime.block_on(async {
        // The worker hangs up when its workload ends.
        while let Ok(bytes) =
            read_frame(&mut pipe, &codec, Instant::now() + Duration::from_secs(60))
        {
            let frame = codec.decode_worker(&bytes).expect("a worker frame");
            incoming.admit(&frame.header).expect("fenced");
            match frame.message {
                WorkerMessage::Progress { .. } => {}
                WorkerMessage::Pages { id, request } => {
                    let reply = pages.serve(request).await;
                    let bytes = codec
                        .encode_parent(&ParentFrame {
                            header: outgoing.next_header(),
                            message: ParentMessage::Pages { id, reply },
                        })
                        .expect("a reply frames");
                    write_frame(&mut pipe, &bytes, Instant::now() + Duration::from_secs(5))
                        .expect("sent");
                }
                other => panic!("the worker only pages: {other:?}"),
            }
        }
    });
    worker.join().expect("the worker runs")
}

fn root_ref(pages: &ActivationPages<Store>) -> PageRef {
    let root = pages.root();
    PageRef {
        id: PageId(root.page),
        grant: grant_of(&root),
    }
}

/// The cold continuation: a fresh worker resuming a checkpoint that
/// touches one element of a 100,000-element list reads the control
/// header, the heap header and that element's index path, counted by the
/// pages its requests named over the wire, and nothing else of the
/// list's 1,616 pages.
#[test]
fn a_fresh_worker_reads_the_header_and_touched_pages_only() {
    let store = Arc::new(Store::default());
    let (root, held) = committed_list(&store, 100_000);
    let spools = tempfile::tempdir().expect("dir");
    let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
    let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 1);
    let start = root_ref(&pages);
    let (value, traffic) = served(&mut pages, 64 << 20, move |source| {
        let (len, list) = open(source, start).expect("opens");
        assert_eq!(len, 100_000);
        element(source, list, 77_777).expect("one element")
    });
    assert_eq!(value, 77_777 * 3);
    // Leaves 1,563; index levels 49, 2 and 1: a path of four pages.
    assert_eq!(held, 1_563 + 49 + 2 + 1 + 2);
    assert_eq!(traffic.pages_requested, 2 + 4, "header, heap header, path");
    assert_eq!(traffic.exchanges, 6);
    assert_eq!(pages.stats().pages_read, 6);
}

/// A page fault inside a native traversal blocks on the traversal's own
/// stack and returns into it: a cold traversal that faults in every page
/// it visits sees each element once, in order, and is charged exactly
/// what the same traversal is charged with every page resident, which
/// faults never. Nothing is rerun or recharged; residency changes latency
/// only.
#[test]
fn a_fault_mid_native_resumes_on_the_same_stack() {
    let store = Arc::new(Store::default());
    let (root, _) = committed_list(&store, 5_000);
    let spools = tempfile::tempdir().expect("dir");
    let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
    let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 1);
    let start = root_ref(&pages);
    let ((cold, warm), traffic) = served(&mut pages, 64 << 20, move |source| {
        let (_, list) = open(source, start).expect("opens");
        let run = |source: &mut WirePageSource| {
            let before = source.traffic().exchanges;
            let (mut seen, mut charge, mut entered) = (Vec::new(), 0, 0);
            entered += 1;
            visit(source, list, &mut seen, &mut charge).expect("visits");
            (seen, charge, entered, source.traffic().exchanges - before)
        };
        let cold = run(source);
        let warm = run(source);
        (cold, warm)
    });
    let (seen, charge, entered, faults) = cold;
    assert!(faults > 0, "the cold traversal faulted mid-native");
    assert_eq!(entered, 1, "the native was entered once");
    assert_eq!(seen, (0..5_000).map(|value| value * 3).collect::<Vec<_>>());
    assert_eq!(charge, 5_000);
    assert_eq!(
        (warm.0, warm.1, warm.3),
        (seen, charge, 0),
        "warm: no fault, same charge"
    );
    assert_eq!(traffic.exchanges, 2 + faults);
}

/// A one-worker pool keeps serving: the parent holds its only worker
/// blocked in the fault and answers every exchange from its store and
/// spool lane, the page service holding no pool, so no second worker is
/// ever needed. A worker with a clean budget smaller than its working set
/// evicts and faults pages in again, each exchange still served.
#[test]
fn a_one_worker_pool_serves_pages_without_another_worker() {
    let store = Arc::new(Store::default());
    let (root, _) = committed_list(&store, 2_000);
    let spools = tempfile::tempdir().expect("dir");
    let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
    let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 1);
    let start = root_ref(&pages);
    // A few leaves' worth resident: a pass evicts what it read first.
    let (passes, traffic) = served(&mut pages, 1_000, move |source| {
        let (_, list) = open(source, start).expect("opens");
        let mut once = || {
            let before = source.traffic().exchanges;
            let (mut seen, mut charge) = (Vec::new(), 0);
            visit(source, list, &mut seen, &mut charge).expect("visits");
            (seen, charge, source.traffic().exchanges - before)
        };
        [once(), once()]
    });
    let [(first, first_charge, _), (second, second_charge, refaults)] = passes;
    assert_eq!(
        (first, first_charge),
        (second, second_charge),
        "eviction changes no value and no charge"
    );
    assert!(
        refaults > 0,
        "the second pass faulted its evicted pages in again"
    );
    assert_eq!(pages.stats().exchanges, traffic.exchanges);
    assert!(pages.refusal().is_none());
}

/// A page's authority is its capability: a forged MAC, a digest the worker
/// guessed, another activation's capability and one of an earlier epoch
/// each read nothing. Each is refused as infrastructure, never as an
/// absent value, and the refusal ends the activation.
#[test]
fn a_forged_foreign_or_stale_capability_reads_nothing() {
    let store = Arc::new(Store::default());
    let (root, _) = committed_list(&store, 100);
    let key = CapabilityKey::fresh();
    let spools = tempfile::tempdir().expect("dir");
    let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
    let valid = activation(&store, &spools, root.id, key.clone(), 7).root();
    let mut forged = valid;
    forged.mac[0] ^= 1;
    let guessed = PageCapability {
        mac: [0; 32],
        ..valid
    };
    let foreign = activation(&store, &spools, root.id, CapabilityKey::fresh(), 8).root();
    let foreign = PageCapability {
        epoch: 7,
        ..foreign
    };
    let stale = key.issue(root.id, PageAuthority::Root(41), 6);
    let other_lease = key.issue(root.id, PageAuthority::Root(40), 7);
    for (case, capability) in [
        ("forged", forged),
        ("guessed", guessed),
        ("foreign", foreign),
        ("stale", stale),
        ("another lease", other_lease),
    ] {
        let mut pages = activation(&store, &spools, root.id, key.clone(), 7);
        let (read, _) = served(&mut pages, 1 << 20, move |source| {
            let page = PageRef {
                id: PageId(capability.page),
                grant: grant_of(&capability),
            };
            let first = source.read(&[page]).map(|answers| answers.len());
            // The activation has ended: even its own root is refused now.
            let valid = PageRef {
                id: PageId(valid.page),
                grant: grant_of(&valid),
            };
            (first, source.read(&[valid]).map(|answers| answers.len()))
        });
        let refused = Err(HeapFault::Unauthorized {
            page: PageId(root.id.0),
        });
        assert_eq!(read, (refused.clone(), refused), "{case}");
        assert_eq!(pages.stats().pages_read, 0, "{case}");
    }
    // An unforged grant of the same activation reads.
    let mut pages = activation(&store, &spools, root.id, key, 7);
    let (read, _) = served(&mut pages, 1 << 20, move |source| {
        source
            .read(&[PageRef {
                id: PageId(valid.page),
                grant: grant_of(&valid),
            }])
            .map(|answers| answers[0].kind)
    });
    assert_eq!(read, Ok(PageKind::CHECKPOINT_ROOT));
    // The kernel's in-process grant names nothing on a worker.
    let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 9);
    let (read, _) = served(&mut pages, 1 << 20, move |source| {
        source
            .read(&[PageRef {
                id: PageId(root.id.0),
                grant: PageGrant::NONE,
            }])
            .map(|answers| answers.len())
    });
    assert_eq!(
        read,
        Err(HeapFault::Unauthorized {
            page: PageId(root.id.0)
        })
    );
}

/// A page the leased store lacks, or holds with bytes that do not hash to
/// its identity, is an infrastructure fault the heap propagates (`Missing`,
/// `Corrupt`): never an absent element, a null or a type error.
#[test]
fn a_missing_or_corrupt_page_is_infrastructure_never_absence() {
    for corrupt in [false, true] {
        let store = Arc::new(Store::default());
        let (root, _) = committed_list(&store, 100);
        // Every leaf of the list goes missing, or corrupt.
        let leaves: Vec<NodeId> = store
            .pages
            .lock()
            .expect("pages")
            .iter()
            .filter(|(_, bytes)| {
                lash_core_store::checkpoint_graph::parse_envelope(bytes, None)
                    .expect("page")
                    .kind
                    .0
                    == PageKind::SEQUENCE_LEAF.0
            })
            .map(|(id, _)| *id)
            .collect();
        for leaf in &leaves {
            let mut held = store.pages.lock().expect("pages");
            if corrupt {
                let mut bytes = held[leaf].to_vec();
                *bytes.last_mut().expect("a byte") ^= 1;
                held.insert(*leaf, bytes.into());
            } else {
                held.remove(leaf);
            }
        }
        let spools = tempfile::tempdir().expect("dir");
        let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
        let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 1);
        let start = root_ref(&pages);
        let (read, _) = served(&mut pages, 1 << 20, move |source| {
            let (_, list) = open(source, start).expect("the headers read");
            element(source, list, 5)
        });
        let faulted = match &read {
            Err(HeapFault::Corrupt { page, .. }) if corrupt => *page,
            Err(HeapFault::Missing { page }) if !corrupt => *page,
            other => panic!("corrupt {corrupt}: {other:?}"),
        };
        assert!(leaves.contains(&NodeId(faulted.0)));
    }
}

/// A worker seals pages into its parent's spool and reads them back
/// through the stage capabilities the seals returned, after its clean
/// budget evicted them. A sealed page may name only committed pages its
/// capabilities protect or pages it sealed before; a child it holds no
/// grant for is refused before anything crosses. Sealing published
/// nothing: the store holds what it held.
#[test]
fn a_worker_reads_back_what_it_sealed_through_its_stage_capability() {
    let store = Arc::new(Store::default());
    let (root, _) = committed_list(&store, 100);
    let committed = store.pages.lock().expect("pages").len();
    let spools = tempfile::tempdir().expect("dir");
    let spools = SpoolRoot::open(spools.path(), 1 << 20).expect("spools");
    let mut pages = activation(&store, &spools, root.id, CapabilityKey::fresh(), 1);
    let start = root_ref(&pages);
    let (outcome, traffic) = served(&mut pages, 0, move |source| {
        let header = read_one(source, start).expect("root");
        let heap = header.children[0];
        let body = |label: &str, children: Vec<PageRef>| {
            let mut tokens = TokenWriter::new();
            tokens.seq(children.len() as u64 + 1);
            tokens.text(label);
            for child in &children {
                tokens.child(lash_core_store::checkpoint_graph::ChildRef(child.id.0));
            }
            PageBody {
                kind: PageKind::SEQUENCE_LEAF,
                tokens: tokens.into_bytes(),
                children,
            }
        };
        let leaf = source
            .seal(body("leaf", vec![heap]))
            .expect("a committed child");
        let parent = source
            .seal(body("parent", vec![leaf, heap]))
            .expect("a sealed and a committed child");
        // Nothing stays resident under a zero budget: this reads the spool.
        let back = read_one(source, parent).expect("reads back");
        let ungranted = PageRef {
            id: PageId([5; 32]),
            grant: PageGrant::NONE,
        };
        let refused = source.seal(PageBody {
            kind: PageKind::SEQUENCE_LEAF,
            tokens: {
                let mut tokens = TokenWriter::new();
                tokens.child(lash_core_store::checkpoint_graph::ChildRef(ungranted.id.0));
                tokens.into_bytes()
            },
            children: Vec::new(),
        });
        (
            back.children
                .iter()
                .map(|child| child.id)
                .collect::<Vec<_>>(),
            [leaf.id, heap.id],
            refused,
        )
    });
    let (children, sealed_children, refused) = outcome;
    let mut expected = sealed_children.to_vec();
    expected.sort();
    assert_eq!(children, expected);
    assert_eq!(
        refused,
        Err(HeapFault::Unauthorized {
            page: PageId([5; 32])
        })
    );
    assert_eq!(traffic.pages_sealed, 2);
    assert_eq!(pages.usage().pages, 2);
    assert_eq!(store.pages.lock().expect("pages").len(), committed);
}
