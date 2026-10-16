//! S4 root grammar laws.
use super::super::*;
use lash_kernel_doc::DocumentId;
use lash_kernel_doc::graph_tokens::{TokenCodec, decode_body, encode_body};
use lash_kernel_doc::heap::*;
fn empty() -> KeyedRoot {
    KeyedRoot::Empty
}
fn seq() -> SequenceRoot {
    SequenceRoot::Empty
}
/// FIG-6197 F3: an external tree root is empty, with no page and no
/// summary, or nonempty, with a page and at least one row. A page-less
/// root with rows, or a paged one without, is refused at construction and
/// at decoding, before any tree opens it, as a nested tree's root is.
#[test]
fn a_tree_root_admits_only_its_occupancy() {
    let page = PageRef {
        id: PageId([7; 32]),
        grant: PageGrant::NONE,
    };
    assert!(KeyedRoot::of(None, 3, LogicalBytes(9)).is_err());
    assert!(KeyedRoot::of(None, 0, LogicalBytes(9)).is_err());
    assert!(SequenceRoot::of(Some(page), 0, LogicalBytes(0)).is_err());
    assert_eq!(
        KeyedRoot::of(None, 0, LogicalBytes(0)).unwrap(),
        KeyedRoot::Empty
    );
    let full = KeyedRoot::of(Some(page), 3, LogicalBytes(9)).unwrap();
    assert_eq!(decode_body::<KeyedRoot>(&encode_body(&full)).unwrap(), full);
    // A nonempty tag holding no rows, and an empty tag with fields, do not
    // decode.
    let mut rowless = lash_kernel_doc::graph_tokens::TokenWriter::new();
    rowless.sum(1, 3);
    page.encode(&mut rowless);
    0_u64.encode(&mut rowless);
    LogicalBytes(0).encode(&mut rowless);
    assert!(decode_body::<KeyedRoot>(&rowless.into_bytes()).is_err());
    let mut padded = lash_kernel_doc::graph_tokens::TokenWriter::new();
    padded.sum(0, 1);
    3_u64.encode(&mut padded);
    assert!(decode_body::<SequenceRoot>(&padded.into_bytes()).is_err());
}
#[test]
fn a_checkpoint_root_fits_its_root_bound() {
    // FIG-6499 / S1: reserve the complete envelope as well as the largest
    // scalar encodings, every nonempty tree and a terminal result descriptor.
    // The admission budget is 4096 - 773 fixed token bytes - 256 framing bytes.
    let page = |byte| PageRef {
        id: PageId([byte; 32]),
        grant: PageGrant::NONE,
    };
    let keyed = |byte| KeyedRoot::NonEmpty {
        page: page(byte),
        len: u64::MAX,
        cost: LogicalBytes(u64::MAX),
    };
    let sequence = |byte| SequenceRoot::NonEmpty {
        page: page(byte),
        len: u64::MAX,
        cost: LogicalBytes(u64::MAX),
    };
    for host in [
        vec![lash_kernel_doc::Name::new("h".repeat(3062))],
        vec![lash_kernel_doc::Name::new(""); 1532],
    ] {
        let root = CheckpointRoot {
            kernel: u32::MAX,
            document: DocumentId::from_bytes([1; 32]),
            release: PageId([2; 32]),
            owner: PageId([3; 32]),
            target: CellTargetKind::Entry,
            functions: keyed(4),
            status: RootStatus::Ended(EndedRecord::Returned(FrozenRef {
                root: page(5),
                summary: FrozenSummary {
                    entities: u64::MAX,
                    cost: LogicalBytes(u64::MAX),
                    data_only: true,
                },
            })),
            heap: page(6),
            tasks: keyed(7),
            ready: sequence(8),
            waits: keyed(9),
            joins: keyed(10),
            withdrawn: keyed(11),
            unreported: sequence(12),
            reads: keyed(13),
            session: keyed(14),
            host,
            meters: SavedMeters {
                charged: u64::MAX,
                waits_issued: u64::MAX,
                next_join: u64::MAX,
                live_tasks: u32::MAX,
            },
        };
        assert_eq!(
            CheckpointRoot::host_metadata_bytes(root.host.iter().map(|name| name.as_str())),
            3067,
        );
        assert_eq!(CheckpointRoot::HOST_METADATA_LIMIT, 3067);
        let body = encode_body(&root);
        assert!(body.len() + 256 <= 4096, "{} token bytes", body.len());
        assert_eq!(
            encode_body(&decode_body::<CheckpointRoot>(&body).expect("canonical root")),
            body
        );
    }
}

/// S4/S7/K-BND-003: identity cursors add no Value header; tuple cursors
/// retain their members. The frame's fixed 64-byte offset survives the codec.
#[test]
fn loop_holdings_keep_their_exact_frame_offsets_after_open() {
    use lash_kernel_doc::{Object, Site, Unit};
    use tree::{Cell, SealWork, Tree};
    let (mut heap, _) = super::heap::fresh();
    let list = Value::List(heap.allocate(Object::List(vec![Value::Null])).unwrap());
    let map = Value::Map(heap.allocate(Object::Map(Vec::new())).unwrap());
    let tuple = Value::Tuple(heap.tuple(vec![Value::Null, Value::Bool(true)]).unwrap());
    for (value, expected) in [(list, 64), (map, 64), (tuple, 96)] {
        let mut frame =
            FrameRecord::fresh(SiteRef::new(&Site::new(Unit::Main, [0]), &heap.access).unwrap());
        frame
            .iterated
            .write(
                directory::ordinal_key(0),
                IteratedRecord(value),
                &heap.access,
            )
            .unwrap();
        assert_eq!(frame.cost(), expected);
        frame
            .prepare(
                &heap.access,
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                }),
            )
            .unwrap();
        let reopened: FrameRecord = decode_body(&encode_body(&frame)).unwrap();
        assert_eq!(reopened.cost(), expected);
        let mut frames = Tree::new(true);
        frames
            .write(directory::ordinal_key(0), reopened, &heap.access)
            .unwrap();
        assert_eq!(frames.cost(), expected);
    }
}
#[test]
fn a_kernel_whole_root_is_refused() {
    // A whole root is a many-field struct, while the heap-header is a
    // one-field tagged record. It cannot be interpreted by the new reader.
    let (mut heap, pages) = super::heap::fresh();
    super::heap::seal(&mut heap);
    let header = heap.sealed_root().expect("header");
    pages
        .borrow_mut()
        .pages
        .get_mut(&header.id)
        .expect("page")
        .kind = PageKind(16);
    assert!(
        PagedHeap::open_header(header, Box::new(super::heap::Source(pages)), EditEpoch(2)).is_err()
    );
}

#[test]
fn a_large_join_resumes_by_reading_only_member_paths() {
    use super::super::tree::{Cell, SealWork, Tree};
    use super::heap::{Pages, Source};
    use lash_kernel_doc::{JoinMode, TaskId};
    use std::cell::RefCell;
    use std::rc::Rc;
    let pages = Rc::new(RefCell::new(Pages::default()));
    let access = tree::Access::new(Box::new(Source(Rc::clone(&pages))));
    let mut join = ListJoinRecord {
        joiner: TaskId(0),
        mode: JoinMode::All,
        members: Tree::new(true),
    };
    for ordinal in 0..100_000 {
        join.members
            .write(
                directory::ordinal_key(ordinal),
                TaskId(ordinal + 1),
                &access,
            )
            .expect("member");
    }
    assert!(
        join.prepare(
            &access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX
            })
        )
        .expect("seal")
    );
    let tokens = encode_body(&join);
    assert!(tokens.len() < 4096);
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|p| p.tokens.len() + 32 * p.children.len() + 32 <= 65_536)
    );
    let opened: ListJoinRecord = decode_body(&tokens).expect("join");
    access.clear();
    pages.borrow_mut().reads = 0;
    assert_eq!(
        opened
            .members
            .rank_get(52_345, &access)
            .expect("one member")
            .map(|(_, task)| task),
        Some(TaskId(52_346))
    );
    assert!(
        pages.borrow().reads <= 5,
        "one index path, not the member list"
    );
}

#[test]
fn every_record_round_trips_canonically() {
    use lash_kernel_doc::graph_tokens::TokenCodec;
    use lash_kernel_doc::{JoinMode, Name, Site, TaskId, Unit};
    use tree::{Cell, SealWork, Tree};
    fn round<T: TokenCodec>(value: &T) {
        let tokens = encode_body(value);
        assert_eq!(
            encode_body(&decode_body::<T>(&tokens).expect("canonical record")),
            tokens
        );
    }
    let (mut heap, _) = super::heap::fresh();
    super::heap::seal(&mut heap);
    let site = Site::new(Unit::Main, [0]);
    let value = Value::Null;
    round(&empty());
    round(&seq());
    round(&heap.certificate());
    round(&heap.heap_root().unwrap());
    round(&heap.data_only_certificate());
    let frozen = FrozenRef {
        root: PageRef {
            id: PageId([17; 32]),
            grant: PageGrant::NONE,
        },
        summary: FrozenSummary {
            entities: 1,
            cost: LogicalBytes(48),
            data_only: true,
        },
    };
    round(&frozen);
    round(&frozen.summary);
    round(&MembershipRun {
        start: 0,
        len: 1,
        live: true,
    });
    round(&FamilyRecord {
        family: FamilyId(1),
        base: frozen,
        extent: 1,
        membership: empty(),
    });
    round(&SessionRoot {
        heap: heap.sealed_root().expect("sealed header"),
        bindings: empty(),
        functions: empty(),
        certificate: heap.data_only_certificate(),
    });
    for ended in [
        EndedRecord::Returned(frozen),
        EndedRecord::Finished(frozen),
        EndedRecord::Failed(frozen),
        EndedRecord::Stopped,
    ] {
        round(&ended);
        round(&RootStatus::Ended(ended));
    }
    round(&RootStatus::Parked);
    round(&BindingRecord {
        name: NameRef::new(&(Name::from("v")), &heap.access).unwrap(),
        declared: SiteRef::new(&(site.clone()), &heap.access).unwrap(),
        value: BoundRecord::Value(value.clone()),
    });
    round(&BoundRecord::Shared(EntityId {
        family: FamilyId(1),
        ordinal: 0,
    }));
    round(&LoopRecord {
        site: SiteRef::new(&(site.clone()), &heap.access).unwrap(),
        started: 7,
        position: Some(8),
    });
    for entered in [
        EnteredRecord::Normal,
        EnteredRecord::Throw,
        EnteredRecord::Return,
        EnteredRecord::Break,
        EnteredRecord::Continue,
    ] {
        round(&FinallyRecord {
            site: SiteRef::new(&(site.clone()), &heap.access).unwrap(),
            entered,
        });
    }
    let mut frame = FrameRecord::fresh(SiteRef::new(&(site.clone()), &heap.access).unwrap());
    frame
        .bindings
        .write(
            b"v".to_vec(),
            BindingRecord {
                name: NameRef::new(&("v".into()), &heap.access).unwrap(),
                declared: SiteRef::new(&(site.clone()), &heap.access).unwrap(),
                value: BoundRecord::Value(value.clone()),
            },
            &heap.access,
        )
        .expect("binding");
    frame
        .prepare(
            &heap.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .expect("prepare");
    round(&frame);
    let mut frames = Tree::new(true);
    frames
        .write(directory::ordinal_key(0), frame, &heap.access)
        .expect("frame");
    for state in [
        TaskStateRecord::Ready,
        TaskStateRecord::Waiting(2),
        TaskStateRecord::Joining(TaskId(3)),
        TaskStateRecord::JoiningMany(4),
        TaskStateRecord::Resuming(IncomingRecord::Value(value.clone())),
        TaskStateRecord::Resuming(IncomingRecord::Raise(value.clone())),
        TaskStateRecord::Resuming(IncomingRecord::Joined(TaskId(3))),
        TaskStateRecord::Ended(EndedTask::Returned(value.clone())),
        TaskStateRecord::Ended(EndedTask::Raised(value.clone())),
    ] {
        let mut task = TaskRecord {
            identity: TaskIdentityRef::MAIN,
            state,
            result_retained: true,
            joiners: Tree::new(true),
            observed: false,
            passed: false,
            failed: false,
            occurrences: Tree::new(false),
            frames: frames.clone(),
        };
        task.prepare(
            &heap.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .expect("prepare task");
        round(&task);
    }
    round(&JoinerRecord::Single(TaskId(7)));
    round(&JoinerRecord::List(8));
    for kind in [WaitKindRecord::Effect, WaitKindRecord::Sleep] {
        for phase in [
            WaitPhase::Issued { request: frozen },
            WaitPhase::Admitted,
            WaitPhase::Committed(CommittedOutcomeRecord::Completed(frozen)),
            WaitPhase::Committed(CommittedOutcomeRecord::Failed(frozen)),
            WaitPhase::Committed(CommittedOutcomeRecord::Rejected(frozen)),
            WaitPhase::Committed(CommittedOutcomeRecord::Elapsed),
        ] {
            round(&WaitRecord {
                task: TaskId(1),
                kind,
                phase,
            });
        }
    }
    for mode in [
        JoinMode::All,
        JoinMode::AllSettled,
        JoinMode::Race,
        JoinMode::Any,
    ] {
        round(&ListJoinRecord {
            joiner: TaskId(1),
            mode,
            members: Tree::new(true),
        });
    }
    round(&SavedMeters {
        charged: 3,
        waits_issued: 5,
        next_join: 8,
        live_tasks: 2,
    });
}
