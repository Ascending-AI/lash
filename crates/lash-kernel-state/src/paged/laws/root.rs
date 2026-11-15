//! S4 root grammar laws.
use super::super::*;
use lash_kernel_doc::heap::*;
use lash_kernel_value::names::DocumentId;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::decode_body,
    lash_kernel_value::tokens::encode_body,
};

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
    let mut rowless = lash_kernel_value::tokens::TokenWriter::new();
    rowless.sum(1, 3);
    page.encode(&mut rowless);
    0_u64.encode(&mut rowless);
    LogicalBytes(0).encode(&mut rowless);
    assert!(decode_body::<KeyedRoot>(&rowless.into_bytes()).is_err());
    let mut padded = lash_kernel_value::tokens::TokenWriter::new();
    padded.sum(0, 1);
    3_u64.encode(&mut padded);
    assert!(decode_body::<SequenceRoot>(&padded.into_bytes()).is_err());
}
#[test]
fn a_checkpoint_root_fits_its_root_bound() {
    // FIG-6499 / S1: reserve the complete envelope as well as the largest
    // scalar encodings, every nonempty tree and a terminal result descriptor.
    // S1 includes the Link's 34 token bytes: 4096 - 865 fixed - 256 framing.
    assert_eq!(CheckpointRoot::HOST_METADATA_LIMIT, 2975);
    let limit = CheckpointRoot::HOST_METADATA_LIMIT;
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
        vec![lash_kernel_value::names::Name::new("h".repeat(limit - 5))],
        vec![lash_kernel_value::names::Name::new(""); (limit - 3) / 2],
    ] {
        let root = CheckpointRoot {
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            limits: SavedLimits {
                memory: Some(u64::MAX),
                charge: u64::MAX,
            },
            kernel: u32::MAX,
            document: DocumentId::from_bytes([1; 32]),
            closure: PageId([2; 32]),
            helper_library: lash_kernel_value::HelperLibraryId::from_bytes([6; 32]),
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
                charged: Some(u64::MAX),
                waits_issued: u64::MAX,
                next_join: u64::MAX,
                live_tasks: u32::MAX,
            },
        };
        assert_eq!(
            CheckpointRoot::host_metadata_bytes(root.host.iter().map(|name| name.as_str())),
            limit,
        );
        let body = encode_body(&root);
        assert!(body.len() + 256 <= 4096, "{} token bytes", body.len());
        assert_eq!(
            encode_body(&decode_body::<CheckpointRoot>(&body).expect("canonical root")),
            body
        );
    }
}

/// A page of another kind is never read as a checkpoint root: the root's
/// own kind is checked before its tokens are decoded.
#[test]
fn a_page_of_another_kind_is_refused_as_a_root() {
    let page = PageRef {
        id: PageId([7; 32]),
        grant: PageGrant::NONE,
    };
    let answer = |kind| PageAnswer {
        page,
        kind,
        tokens: Vec::new().into(),
        children: Vec::new(),
    };
    for kind in [PageKind(16), PageKind::KEYED_LEAF, PageKind::PAYLOAD_LEAF] {
        assert!(matches!(
            CheckpointRoot::of_answer(&answer(kind)),
            Err(HeapFault::Corrupt { reason, .. }) if reason == "not a checkpoint root"
        ));
    }
}

#[test]
fn a_large_join_resumes_by_reading_only_member_paths() {
    use super::super::tree::{SealWork, Tree};
    let (access, pages) = super::pages::fresh();
    let mut members = Tree::<u32>::new(true);
    for ordinal in 0..100_000_u32 {
        members
            .write(u64::from(ordinal).to_be_bytes(), ordinal + 1, &access)
            .expect("member");
    }
    assert!(
        members
            .seal_nested(
                &access,
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX
                })
            )
            .expect("seal")
    );
    let tokens = encode_body(&members);
    assert!(tokens.len() < 4096);
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|p| p.tokens.len() + 32 * p.children.len() + 32 <= 65_536)
    );
    let opened: Tree<u32> = decode_body(&tokens).expect("join");
    access.clear();
    pages.borrow_mut().reads = 0;
    assert_eq!(
        opened
            .rank_get(52_345, &access)
            .expect("one member")
            .map(|(_, task)| task),
        Some(52_346)
    );
    assert!(
        pages.borrow().reads <= 5,
        "one index path, not the member list"
    );
    for (start, end) in [
        (0, 0),
        (0, 65),
        (63, 129),
        (52_345, 52_410),
        (99_999, u64::MAX),
        (100_000, u64::MAX),
    ] {
        let mut got = Vec::new();
        opened
            .visit_ranks(&access, start, end, &mut |task| got.push(*task))
            .expect("range");
        let expected: Vec<_> = (start..end.min(100_000)).map(|at| at as u32 + 1).collect();
        assert_eq!(got, expected, "ranks {start}..{end}");
    }
}

/// FIG-7455: contextual inline rows retain the decode already paid for,
/// while serialization keeps their original token grammar.
#[test]
fn contextual_inline_reads_reuse_their_validated_rows() {
    use super::super::tree::Tree;
    let (access, _) = super::pages::fresh();
    let rows = Tree::nested_from_rows(vec![(0_u64.to_be_bytes(), 7_u32)], true, &access)
        .unwrap()
        .in_family(lash_kernel_doc::heap::FamilyId(8));
    let tokens = encode_body(&rows);
    crate::perf_witness::enable_work();
    let opened: Tree<u32> = decode_body(&tokens).unwrap();
    for _ in 0..3 {
        assert_eq!(
            opened.rank_get(0, &access).unwrap().map(|(_, value)| value),
            Some(7)
        );
    }
    assert_eq!(encode_body(&opened), tokens);
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert_eq!(work.inline_decodes, 1, "one parse of the contextual leaf");
}

/// S4: a checkpoint preserves every saved meter, including the next join identity.
#[test]
fn saved_meters_preserve_a_nonzero_next_join_on_decode() {
    let original = SavedMeters {
        charged: Some(131),
        waits_issued: 5,
        next_join: 8,
        live_tasks: 2,
    };
    // seq(4); the unsigned meter itself discriminates Exact.
    let vector = [9, 4, 3, 131, 1, 3, 5, 3, 8, 3, 2];
    assert_eq!(encode_body(&original), vector);
    assert_eq!(decode_body::<SavedMeters>(&vector).unwrap(), original);
    let off = SavedMeters {
        charged: None,
        ..original
    };
    let off_vector = [9, 4, 13, 3, 5, 3, 8, 3, 2];
    assert_eq!(encode_body(&off), off_vector);
    assert_eq!(decode_body::<SavedMeters>(&off_vector).unwrap(), off);
    assert_eq!(off.charge(), super::super::Charge::Off);
    assert_eq!(
        decode_body::<SavedMeters>(&encode_body(&original)).unwrap(),
        original
    );
}

/// S4: a fixed root grammar retains each field's meaning across codec changes.
#[test]
fn a_baseline_checkpoint_root_keeps_every_field() {
    // seq(21), limits seq(2), memory 31, charge 37, kernel 1.
    let vector: &[u8] = &[
        9, 21, 9, 2, 3, 31, 3, 37, 3, 1, // document: bytes(32), digest 1.
        8, 32, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, // link: bytes(32), digest 2.
        8, 32, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2, // closure: child reference 3.
        11, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        3, 3, 3, // helper library: bytes(32), digest 4.
        8, 32, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
        4, 4, 4, 4,
        // owner: child reference 5; Entry tag 1; functions: nonempty tag 1, three fields.
        11, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
        5, 5, 5, 12, 1, 0, 12, 1, 3, // functions page 6, two rows, cost 41; Parked tag 0.
        11, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 3, 2, 3, 41, 12, 0, 0, // heap page 7.
        11, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
        7, 7, 7, // tasks: nonempty, page 8, 3 rows, cost 43.
        12, 1, 3, 11, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 3, 3, 3, 43, // ready: nonempty, page 9, 4 rows, cost 47.
        12, 1, 3, 11, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
        9, 9, 9, 9, 9, 9, 3, 4, 3, 47, // waits: nonempty, page 10, 5 rows, cost 53.
        12, 1, 3, 11, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 3, 5, 3, 53,
        // joins: nonempty, page 11, 6 rows, cost 59.
        12, 1, 3, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
        11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 3, 6, 3, 59,
        // withdrawn: nonempty, page 12, 7 rows, cost 61.
        12, 1, 3, 11, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
        12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 3, 7, 3, 61,
        // unreported: nonempty, page 13, 8 rows, cost 67.
        12, 1, 3, 11, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13,
        13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 3, 8, 3, 67,
        // reads: nonempty, page 14, 9 rows, cost 71.
        12, 1, 3, 11, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14,
        14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 3, 9, 3, 71,
        // session: nonempty, page 15, 10 rows, cost 73.
        12, 1, 3, 11, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15,
        15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 3, 10, 3, 73,
        // host: two text names, "host" and "input"; meters seq(4): 79, 83, 89, 97.
        9, 2, 7, 4, 104, 111, 115, 116, 7, 5, 105, 110, 112, 117, 116, 9, 4, 3, 79, 3, 83, 3, 89, 3,
        97,
    ];
    let root: CheckpointRoot = decode_body(vector).expect("baseline root");
    let page = |byte| PageRef {
        id: PageId([byte; 32]),
        grant: PageGrant::NONE,
    };
    let keyed = |byte, len, cost| KeyedRoot::NonEmpty {
        page: page(byte),
        len,
        cost: LogicalBytes(cost),
    };
    let sequence = |byte, len, cost| SequenceRoot::NonEmpty {
        page: page(byte),
        len,
        cost: LogicalBytes(cost),
    };
    assert_eq!(root.limits.memory, Some(31));
    assert_eq!(root.limits.charge, 37);
    assert_eq!(root.kernel, 1);
    assert_eq!(root.document, DocumentId::from_bytes([1; 32]));
    assert_eq!(root.link, lash_kernel_value::LinkId::from_bytes([2; 32]));
    assert_eq!(root.closure, PageId([3; 32]));
    assert_eq!(
        root.helper_library,
        lash_kernel_value::HelperLibraryId::from_bytes([4; 32])
    );
    assert_eq!(root.owner, PageId([5; 32]));
    assert_eq!(root.target, CellTargetKind::Entry);
    assert!(matches!(root.status, RootStatus::Parked));
    assert_eq!(root.functions, keyed(6, 2, 41));
    assert_eq!(root.heap, page(7));
    assert_eq!(root.tasks, keyed(8, 3, 43));
    assert_eq!(root.ready, sequence(9, 4, 47));
    assert_eq!(root.waits, keyed(10, 5, 53));
    assert_eq!(root.joins, keyed(11, 6, 59));
    assert_eq!(root.withdrawn, keyed(12, 7, 61));
    assert_eq!(root.unreported, sequence(13, 8, 67));
    assert_eq!(root.reads, keyed(14, 9, 71));
    assert_eq!(root.session, keyed(15, 10, 73));
    assert_eq!(
        root.host,
        ["host", "input"].map(lash_kernel_value::names::Name::new)
    );
    assert_eq!(root.meters.charged, Some(79));
    assert_eq!(root.meters.waits_issued, 83);
    assert_eq!(root.meters.next_join, 89);
    assert_eq!(root.meters.live_tasks, 97);
    assert_eq!(encode_body(&root), vector);
}
