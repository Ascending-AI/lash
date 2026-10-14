//! S4 root grammar laws.
use super::super::*;
use lash_kernel_doc::DocumentId;
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::*;
fn empty() -> KeyedRoot {
    KeyedRoot {
        page: None,
        len: 0,
        cost: LogicalBytes(0),
    }
}
fn seq() -> SequenceRoot {
    SequenceRoot {
        page: None,
        len: 0,
        cost: LogicalBytes(0),
    }
}
#[test]
fn a_checkpoint_root_fits_its_root_bound() {
    let root = CheckpointRoot {
        kernel: 1,
        document: DocumentId::from_bytes([1; 32]),
        release: PageId([2; 32]),
        owner: PageId([3; 32]),
        status: RootStatus::Parked,
        heap: PageRef {
            id: PageId([4; 32]),
            grant: PageGrant::NONE,
        },
        tasks: empty(),
        ready: seq(),
        waits: empty(),
        joins: empty(),
        withdrawn: empty(),
        unreported: seq(),
        reads: empty(),
        session: empty(),
        meters: SavedMeters {
            charged: 0,
            waits_issued: 0,
            next_join: 0,
            live_tasks: 0,
        },
    };
    let body = encode_body(&root);
    assert!(body.len() + 256 < 4096);
    assert_eq!(
        encode_body(&decode_body::<CheckpointRoot>(&body).expect("canonical root")),
        body
    );
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
    use lash_kernel_doc::{EffectIdentity, JoinMode, Name, Site, TaskId, TaskIdentity, Unit};
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
    round(&heap.heap_root());
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
        name: Name::from("v"),
        declared: site.clone(),
        value: BoundRecord::Value(value.clone()),
    });
    round(&BoundRecord::Shared(EntityId {
        family: FamilyId(1),
        ordinal: 0,
    }));
    round(&LoopRecord {
        site: site.clone(),
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
            site: site.clone(),
            entered,
        });
    }
    let mut frame = FrameRecord::fresh(site.clone());
    frame
        .bindings
        .write(
            b"v".to_vec(),
            BindingRecord {
                name: "v".into(),
                declared: site.clone(),
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
            identity: TaskIdentity::Main,
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
        for state in [
            WaitStateRecord::Admitted,
            WaitStateRecord::Committed(CommittedOutcomeRecord::Completed(frozen)),
            WaitStateRecord::Committed(CommittedOutcomeRecord::Failed(frozen)),
            WaitStateRecord::Committed(CommittedOutcomeRecord::Elapsed),
        ] {
            round(&WaitRecord {
                wait: 5,
                task: TaskId(1),
                identity: EffectIdentity {
                    task: TaskIdentity::Main,
                    site: site.clone(),
                    occurrence: 3,
                    loops: vec![],
                },
                kind,
                state,
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
