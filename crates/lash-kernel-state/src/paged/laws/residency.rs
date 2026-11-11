//! Page residency on the live tree and access code: the read cache keeps
//! a reread working set through a cold scan, and dirty working leaves
//! spill under their bound before any seal, keeping older working versions
//! readable and the sealed root a function of the rows alone.
use super::super::*;
use super::pages::{Pages, Source, fresh};
use lash_kernel_doc::heap::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

fn int(v: i64) -> PageValue {
    PageValue::Int(IntRef::Inline(v.into()))
}
fn is_int(value: Option<PageValue>, expected: i64) -> bool {
    matches!(value, Some(PageValue::Int(IntRef::Inline(i))) if i == expected.into())
}
fn full_work() -> tree::SealWork {
    tree::SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
}

/// A page read once and read again after every short burst of single-use
/// pages stays resident: the scan pages never push the reread working set
/// out, so the port answers each page exactly once.
#[test]
fn a_cold_scan_does_not_evict_the_reread_working_set() {
    let pages = Rc::new(RefCell::new(Pages::default()));
    let tokens: Arc<[u8]> = vec![0; 8192].into();
    let page = |n: u64| {
        let mut id = [0; 32];
        id[..8].copy_from_slice(&n.to_be_bytes());
        PageRef {
            id: PageId(id),
            grant: PageGrant::NONE,
        }
    };
    let (hot, rounds, scan) = (7_000, 32, 128);
    for n in 0..hot + rounds * scan {
        pages.borrow_mut().pages.insert(
            page(n).id,
            PageAnswer {
                page: page(n),
                kind: PageKind::PAYLOAD_LEAF,
                tokens: Arc::clone(&tokens),
                children: vec![],
            },
        );
    }
    let access = tree::Access::new(Box::new(Source(Rc::clone(&pages))));
    for n in 0..hot {
        access.read(page(n)).expect("hot page");
    }
    for round in 0..rounds {
        for n in 0..scan {
            access
                .read(page(hot + round * scan + n))
                .expect("cold page");
        }
        for n in 0..hot {
            access.read(page(n)).expect("hot page");
        }
    }
    assert_eq!(pages.borrow().reads, hot + rounds * scan);
}

/// D-CAP/D-SEALTIME: a working tree past the dirty bound spills leaves to
/// pages before any seal, and stays within the bound while it grows. A
/// version taken before an edit still reads its own rows, the edited tree
/// reads the edit, and the sealed tree reopens holding every row.
#[test]
fn dirty_residency_spills_before_checkpoint_and_preserves_working_versions() {
    const N: u64 = 250_000;
    let (access, pages) = fresh();
    let key = |i: u64| i.to_be_bytes().to_vec();
    let mut list = tree::Tree::<PageValue>::new(true);
    for i in 0..10_000 {
        list.write(key(i), int(i as i64), &access).unwrap();
    }
    let prior = list.clone();
    list.write(key(0), int(-1), &access).unwrap();
    for i in 10_000..N {
        list.write(key(i), int(i as i64), &access).unwrap();
        assert!(
            access.dirty_bytes() <= 32 << 20,
            "dirty resident bytes at row {i}"
        );
    }
    assert!(pages.borrow().seals > 0, "no leaf spilled before the seal");
    assert!(is_int(prior.get(&key(0), &access).unwrap(), 0));
    assert!(is_int(list.get(&key(0), &access).unwrap(), -1));
    assert!(list.seal(&access, &mut full_work()).unwrap());
    let reopened = tree::Tree::<PageValue>::open(list.page(), N, LogicalBytes(0), true);
    let cold = tree::Access::new(Box::new(Source(Rc::clone(&pages))));
    assert!(is_int(
        reopened.get(&key(N - 1), &cold).unwrap(),
        N as i64 - 1
    ));
    assert!(is_int(reopened.get(&key(0), &cold).unwrap(), -1));
    assert!(is_int(reopened.get(&key(5_000), &cold).unwrap(), 5_000));
}

/// FIG-6902: a spilled leaf whose rows are one canonical chunk is the page
/// the seal builds from them, so the seal takes it whole: rows written in
/// key order past the dirty bound spill leaves, and the final seal reads
/// none of them back and seals none of them again. The root is the one a
/// history whose spills are not chunks seals.
#[test]
fn a_spilled_canonical_leaf_is_the_seals_own_page() {
    const N: u64 = 80_000;
    let build = |order: &mut dyn Iterator<Item = u64>| {
        let (access, pages) = fresh();
        let mut tree = tree::Tree::<PageValue>::new(false);
        for i in order {
            tree.write(
                i.to_be_bytes().to_vec(),
                int(((i * 7_919) % N) as i64),
                &access,
            )
            .unwrap();
        }
        let spilled = pages.borrow().kinds.get(&PageKind::KEYED_LEAF).copied();
        let (reads, resealed) = (pages.borrow().reads, pages.borrow().resealed);
        assert!(tree.seal(&access, &mut full_work()).unwrap());
        let pages = pages.borrow();
        (
            tree.page().expect("a sealed root").id,
            spilled.unwrap_or(0),
            pages.reads - reads,
            pages.resealed - resealed,
        )
    };
    let (root, spilled, reads, resealed) = build(&mut (0..N));
    assert!(spilled > 0, "the tree stages under dirty pressure");
    assert_eq!(reads, 0, "the seal read a spilled leaf back");
    assert_eq!(resealed, 0, "the seal sealed a spilled leaf again");
    let (reversed, spilled, _, _) = build(&mut (0..N).rev());
    assert!(spilled > 0, "the reversed tree stages too");
    assert_eq!(root, reversed, "one root whatever was spilled");
}
