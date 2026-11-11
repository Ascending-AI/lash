//! The run directory's laws (cutover contract §6): seal-time pruning by
//! edges, retained directories, cross-page cycles and id exhaustion.
use super::super::run::directory::{Directory, DirectoryRoot, Row};
use super::super::run::ids::{HighWater, IdsExhausted, ObjectId};
use super::super::run::objects::{Head, Heads, ObjectKind, Record, RunValue};
use super::super::run::root::HeapRoot;
use super::pages::{Pages, Source};
use crate::arena::{Arena, text_key};
use lash_kernel_doc::Integer;
use lash_kernel_doc::arena::{Body, Chunk, LongText};
use lash_kernel_doc::heap::{EntityId, FamilyId, IntSummary, NormalKey, TextSummary};
use lash_kernel_value::tokens::{decode_body, encode_body};
use std::cell::RefCell;
use std::rc::Rc;

fn list(members: &[u64]) -> Row {
    Row::of(
        Record::List(
            members
                .iter()
                .map(|id| RunValue::Object(ObjectKind::List, ObjectId(*id)))
                .collect(),
        ),
        Heads::default(),
    )
}

fn ids(directory: &Directory, upto: u64) -> Vec<u64> {
    (0..upto)
        .filter(|id| directory.get(ObjectId(*id)).unwrap().is_some())
        .collect()
}

/// R11 and contract §6 "current versus retained": a seal drops the rows
/// its roots do not reach through edges, cycles included, from the new
/// directory only. The retained root still reads every row it held. The
/// rows span several leaves, so the marks cross pages.
#[test]
fn a_seal_prunes_the_new_directory_and_keeps_the_retained_one() {
    let pages = Rc::new(RefCell::new(Pages::default()));
    let mut directory = Directory::new(Box::new(Source(pages.clone())));
    // 0 -> 1 -> 2 -> 1 (a live cycle); 3 <-> 4 (a dead cycle); 5..300 a
    // chain hanging off 2, so the live set spans every leaf.
    directory.put(ObjectId(0), list(&[1])).unwrap();
    directory.put(ObjectId(1), list(&[2])).unwrap();
    directory.put(ObjectId(2), list(&[1, 5])).unwrap();
    directory.put(ObjectId(3), list(&[4])).unwrap();
    directory.put(ObjectId(4), list(&[3])).unwrap();
    for id in 5..300 {
        let next = if id + 1 < 300 { vec![id + 1] } else { vec![] };
        directory.put(ObjectId(id), list(&next)).unwrap();
    }
    assert_eq!(directory.prune([ObjectId(0)]).unwrap(), 2);
    let first = directory.seal().unwrap();
    assert_eq!(first.rows, 298);
    assert!(pages.borrow().seals > 2, "the rows span several pages");

    // The run cuts 2 -> 5: the chain dies with the next seal.
    directory.put(ObjectId(2), list(&[1])).unwrap();
    assert_eq!(directory.prune([ObjectId(0)]).unwrap(), 295);
    let second = directory.seal().unwrap();
    assert_eq!(second.rows, 3);

    let reopened = Directory::open(second, Box::new(Source(pages.clone())));
    assert_eq!(ids(&reopened, 300), vec![0, 1, 2]);
    let retained = Directory::open(first, Box::new(Source(pages.clone())));
    let mut expected = vec![0, 1, 2];
    expected.extend(5..300);
    assert_eq!(ids(&retained, 300), expected);
    assert_eq!(
        retained.get(ObjectId(2)).unwrap().map(|row| row.edges),
        Some(vec![ObjectId(1), ObjectId(5)])
    );
}

/// Contract §6: an edge to an id the directory does not hold is a broken
/// graph, refused, never silently pruned around.
#[test]
fn a_mark_refuses_an_edge_to_an_unwritten_object() {
    let pages = Rc::new(RefCell::new(Pages::default()));
    let mut directory = Directory::new(Box::new(Source(pages)));
    directory.put(ObjectId(0), list(&[7])).unwrap();
    assert!(directory.prune([ObjectId(0)]).is_err());
}

/// Contract §6: ids are minted in order, never reused, and minting at
/// `u64::MAX` refuses rather than wrapping onto id 0.
#[test]
fn the_high_water_mark_refuses_at_the_last_id() {
    let mut mark = HighWater(u64::MAX - 1);
    assert_eq!(mark.mint(), Ok(ObjectId(u64::MAX - 1)));
    assert_eq!(mark.mint(), Err(IdsExhausted));
    assert_eq!(mark, HighWater(u64::MAX));
}

/// A row whose stated edges are not its record's would hide an edge from
/// every mark: decoding refuses it.
#[test]
fn a_row_with_edges_its_record_does_not_hold_is_refused() {
    let mut row = list(&[1, 2]);
    row.edges.pop();
    assert!(decode_body::<Row>(&encode_body(&row)).is_err());
    let root = DirectoryRoot {
        page: None,
        rows: 3,
    };
    assert!(decode_body::<DirectoryRoot>(&encode_body(&root)).is_err());
}

const TEXT: &str = "ünïcødé, then 😀 past the inline bound";

/// The head of [`TEXT`], from its counts as `str` gives them and its text
/// key.
fn text_head(text: &str) -> Head {
    Head::LongText {
        summary: TextSummary {
            utf8_bytes: text.len() as u64,
            chars: text.chars().count() as u64,
            utf16_units: text.encode_utf16().count() as u64,
        },
        normal: text_key(text),
    }
}

fn holding(kind: ObjectKind, id: u64, head: Head) -> Row {
    Row::of(
        Record::List(vec![RunValue::Object(kind, ObjectId(id))]),
        Heads(vec![(ObjectId(id), head)]),
    )
}

fn object(id: u64) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: id,
    }
}

/// Astra §1: a head is a certificate of one immutable. A row whose head is
/// of another kind than its record names the object by is refused when
/// decoded; two rows that certify one id with different summaries or keys,
/// and a row body that is not what its head certifies, are refused when
/// opened. Every page involved hashes correctly.
#[test]
fn immutable_heads_reject_conflicting_kind_summary_or_key() {
    let mut row = holding(ObjectKind::LongText, 1, text_head(TEXT));
    row.record = Record::List(vec![RunValue::Object(ObjectKind::Tuple, ObjectId(1))]);
    assert!(decode_body::<Row>(&encode_body(&row)).is_err(), "kind");
    row.heads = Heads::default();
    assert!(
        decode_body::<Row>(&encode_body(&row)).is_err(),
        "a missing head"
    );

    let big = |bits| Head::BigInt {
        summary: IntSummary {
            bits,
            negative: true,
        },
        normal: NormalKey::Digest([7; 32]),
    };
    let Head::LongText { summary, .. } = text_head(TEXT) else {
        unreachable!()
    };
    let other_key = Head::LongText {
        summary,
        normal: NormalKey::Digest([9; 32]),
    };
    for (first, second) in [(big(101), big(102)), (text_head(TEXT), other_key)] {
        let pages = Rc::new(RefCell::new(Pages::default()));
        let mut directory = Directory::new(Box::new(Source(pages.clone())));
        let kind = first.kind();
        directory
            .put(ObjectId(0), holding(kind, 2, first.clone()))
            .unwrap();
        directory
            .put(ObjectId(1), holding(kind, 2, second.clone()))
            .unwrap();
        let root = directory.seal().unwrap();
        let reopened = Directory::open(root, Box::new(Source(pages)));
        let mut arena = Arena::new();
        for id in [0, 1] {
            let row = reopened.get(ObjectId(id)).unwrap().unwrap();
            let head = row.heads.get(ObjectId(2)).unwrap().clone();
            let opened = arena.open_immutable(object(2), &head);
            assert_eq!(opened.is_ok(), id == 0, "{kind:?} row {id}");
        }
        assert!(arena.open_immutable(object(2), &first).is_ok(), "agreement");
    }

    let body = |text: &str| {
        Body::LongText(LongText {
            summary: lash_kernel_doc::heap_values::text_summary(text),
            chunks: Box::new([Chunk::Resident(text.to_owned())]),
        })
    };
    let arena = Arena::new();
    assert_eq!(arena.detached_head(&body(TEXT)).unwrap(), text_head(TEXT));
    let swapped = TEXT.replace('😀', "😁");
    assert_ne!(
        arena.detached_head(&body(&swapped)).unwrap(),
        text_head(TEXT)
    );
    let int = Integer::parse("-1267650600228229401496703205376").unwrap();
    let Head::BigInt { summary, .. } = arena.detached_head(&Body::BigInt(int)).unwrap() else {
        panic!("a bigint head");
    };
    assert_eq!(
        summary,
        IntSummary {
            bits: 101,
            negative: true
        },
        "-(2^100)"
    );
}

/// Astra §1: heads live in the rows that name their objects, so a seal
/// that rewrites a row leaves the retained root reading the heads it
/// sealed, and a sibling directory that reuses an id for another object
/// names it by its own head.
#[test]
fn immutable_heads_survive_retained_and_sibling_roots() {
    let pages = Rc::new(RefCell::new(Pages::default()));
    let mut directory = Directory::new(Box::new(Source(pages.clone())));
    directory
        .put(
            ObjectId(0),
            holding(ObjectKind::LongText, 1, text_head(TEXT)),
        )
        .unwrap();
    directory
        .put(
            ObjectId(1),
            Row::of(Record::LongText(TEXT.into()), Heads::default()),
        )
        .unwrap();
    let first = directory.seal().unwrap();

    let next = "a different text past the inline bound";
    directory
        .put(
            ObjectId(0),
            holding(ObjectKind::LongText, 2, text_head(next)),
        )
        .unwrap();
    directory
        .put(
            ObjectId(2),
            Row::of(Record::LongText(next.into()), Heads::default()),
        )
        .unwrap();
    assert_eq!(directory.prune([ObjectId(0)]).unwrap(), 1);
    let second = directory.seal().unwrap();

    let mut sibling = Directory::new(Box::new(Source(pages.clone())));
    sibling
        .put(
            ObjectId(0),
            holding(ObjectKind::LongText, 1, text_head(next)),
        )
        .unwrap();
    sibling
        .put(
            ObjectId(1),
            Row::of(Record::LongText(next.into()), Heads::default()),
        )
        .unwrap();
    let sibling = sibling.seal().unwrap();

    let head = |root: DirectoryRoot, id: u64| {
        Directory::open(root, Box::new(Source(pages.clone())))
            .get(ObjectId(0))
            .unwrap()
            .unwrap()
            .heads
            .get(ObjectId(id))
            .cloned()
    };
    assert_eq!(head(first, 1), Some(text_head(TEXT)));
    assert_eq!(head(second, 2), Some(text_head(next)));
    assert_eq!(head(sibling, 1), Some(text_head(next)));
}

/// Astra §5: a root's heads decode with the root at the edge of the id
/// space, and a root whose heads are not exactly its immutables' is
/// refused.
#[test]
fn head_encoding_preserves_high_water_and_exhaustion() {
    let last = ObjectId(u64::MAX - 1);
    let mut mark = HighWater(last.0);
    assert_eq!(mark.mint(), Ok(last));
    let mut root = HeapRoot {
        high_water: mark,
        directory: DirectoryRoot {
            page: None,
            rows: 0,
        },
        roots: vec![RunValue::Object(ObjectKind::LongText, last)],
        heads: Heads(vec![(last, text_head(TEXT))]),
    };
    assert_eq!(decode_body::<HeapRoot>(&encode_body(&root)).unwrap(), root);
    assert_eq!(mark.mint(), Err(IdsExhausted));
    root.heads = Heads::default();
    assert!(decode_body::<HeapRoot>(&encode_body(&root)).is_err());
}
