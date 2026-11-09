//! The run directory's laws (cutover contract §6): seal-time pruning by
//! edges, retained directories, cross-page cycles and id exhaustion.
use super::super::run::directory::{Directory, DirectoryRoot, Row};
use super::super::run::ids::{HighWater, IdsExhausted, ObjectId};
use super::super::run::objects::{ObjectKind, Record, RunValue};
use super::heap::{Pages, Source};
use lash_kernel_value::tokens::{decode_body, encode_body};
use std::cell::RefCell;
use std::rc::Rc;

fn list(members: &[u64]) -> Record {
    Record::List(
        members
            .iter()
            .map(|id| RunValue::Object(ObjectKind::List, ObjectId(*id)))
            .collect(),
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
    let mut row = Row::of(list(&[1, 2]));
    row.edges.pop();
    assert!(decode_body::<Row>(&encode_body(&row)).is_err());
    let root = DirectoryRoot {
        page: None,
        rows: 3,
    };
    assert!(decode_body::<DirectoryRoot>(&encode_body(&root)).is_err());
}
