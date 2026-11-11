//! Canonical page shape (Seam H) and frozen boundary vectors over the live
//! tree and payload code: a tree's pages depend only on its rows, and leaf
//! and payload boundaries follow the frozen rules.
use super::super::*;
use super::pages::{Pages, fresh};
use lash_kernel_doc::heap::*;
use std::ops::ControlFlow;

/// A law-only cell: the trees here hold plain scalars; the core's own cells
/// price and walk them, and page shape depends on rows alone.
impl tree::Cell for PageValue {
    fn cost(&self) -> u64 {
        0
    }
}

fn ordinal_key(at: u64) -> [u8; 8] {
    at.to_be_bytes()
}

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

/// Seals `tree` through a budget of a few pages at a time, so every
/// resumed seal continues a projection an earlier one left incomplete.
fn seal_in_slices(tree: &mut tree::Tree<PageValue>, access: &tree::Access<'_>) {
    for _ in 0..100_000 {
        let mut work = tree::SealWork::new(SealBudget {
            pages: 3,
            bytes: u64::MAX,
        });
        if tree.seal(access, &mut work).unwrap() {
            return;
        }
    }
    panic!("a sliced seal made no progress");
}

/// Seam H invariant: the same rows built by different insert, overwrite
/// and delete orders, including deletes of rows that were present, with
/// seals between edits and seals resumed under a small budget, seal to
/// one root page.
#[test]
fn trees_built_by_different_histories_seal_to_one_root() {
    const N: u64 = 6_000;
    for sequence in [false, true] {
        let (access, _) = fresh();
        let access = &access;
        let key = |i: u64| {
            if sequence {
                ordinal_key(i).to_vec()
            } else {
                // Scattered keys: ascending i is not ascending key order.
                i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes().to_vec()
            }
        };
        let value = |i: u64| int(i as i64);

        let mut ascending = tree::Tree::<PageValue>::new(sequence);
        for i in 0..N {
            ascending.write(key(i), value(i), access).unwrap();
        }
        assert!(ascending.seal(access, &mut full_work()).unwrap());

        // Descending, with surplus rows and wrong values, sealed between
        // edits; then the surplus and a present middle band are removed and
        // every row is rewritten.
        let mut descending = tree::Tree::<PageValue>::new(sequence);
        for i in (0..N + 700).rev() {
            descending.write(key(i), int(-1), access).unwrap();
            if i % 997 == 0 {
                assert!(descending.seal(access, &mut full_work()).unwrap());
            }
        }
        for i in (N..N + 700).chain(1_000..1_500) {
            assert!(descending.remove(&key(i), access).unwrap().is_some());
        }
        assert!(descending.seal(access, &mut full_work()).unwrap());
        for i in 0..N {
            descending.write(key(i), value(i), access).unwrap();
            if i % 1_201 == 0 {
                seal_in_slices(&mut descending, access);
            }
        }
        assert!(descending.seal(access, &mut full_work()).unwrap());

        // A scattered permutation, sliced seals, then 30% of the present
        // rows removed and inserted again.
        let mut scattered = tree::Tree::<PageValue>::new(sequence);
        for n in 0..N {
            let i = (n * 2_741) % N;
            scattered.write(key(i), value(i), access).unwrap();
        }
        seal_in_slices(&mut scattered, access);
        let dropped: Vec<u64> = (0..N).filter(|i| (i * 7_919) % 10 < 3).collect();
        for i in &dropped {
            assert!(scattered.remove(&key(*i), access).unwrap().is_some());
        }
        seal_in_slices(&mut scattered, access);
        for i in dropped.iter().rev() {
            scattered.write(key(*i), value(*i), access).unwrap();
        }
        assert!(scattered.seal(access, &mut full_work()).unwrap());

        let root = ascending.page().expect("sealed root").id;
        assert_eq!(descending.len(), N);
        assert_eq!(scattered.len(), N);
        assert_eq!(
            descending.page().expect("sealed root").id,
            root,
            "{sequence}"
        );
        assert_eq!(
            scattered.page().expect("sealed root").id,
            root,
            "{sequence}"
        );

        // Shrinking a large tree to a few rows gives the small tree's pages.
        let mut small = tree::Tree::<PageValue>::new(sequence);
        for i in 0..3 {
            small.write(key(i), value(i), access).unwrap();
        }
        assert!(small.seal(access, &mut full_work()).unwrap());
        for i in 3..N {
            ascending.remove(&key(i), access).unwrap();
        }
        assert!(ascending.seal(access, &mut full_work()).unwrap());
        assert_eq!(ascending.page(), small.page(), "{sequence}");
    }
}

/// Seam H invariant for a fresh object's tree (`Tree::nested_from_rows`):
/// built from all its rows at once, in or out of key order and with a
/// repeated key, it holds and seals to what writing each row in turn does,
/// the last write of a key winning, and later edits of it seal as edits of
/// the written tree do. One that fits inline is built already in the form a
/// nested seal gives the written tree (FIG-6886).
#[test]
fn a_tree_built_from_its_rows_seals_as_one_written_row_by_row() {
    const N: u64 = 6_000;
    for sequence in [false, true] {
        let (access, _) = fresh();
        let access = &access;
        let key = |i: u64| {
            if sequence {
                ordinal_key(i).to_vec()
            } else {
                i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes().to_vec()
            }
        };
        // Every row once, a repeated key whose later value stands, and the
        // keyed rows out of key order.
        let mut rows: Vec<(Vec<u8>, PageValue)> = (0..N).map(|i| (key(i), int(i as i64))).collect();
        rows.insert(17, (key(4_000), int(-1)));
        let mut written = tree::Tree::<PageValue>::new(sequence);
        for (key, value) in rows.clone() {
            written.write(key, value, access).unwrap();
        }
        let mut built = tree::Tree::nested_from_rows(rows, sequence, access).unwrap();
        assert_eq!(built.len(), N, "{sequence}");
        assert!(
            is_int(built.get(&key(4_000), access).unwrap(), 4_000),
            "{sequence}"
        );
        assert!(written.seal(access, &mut full_work()).unwrap());
        assert!(built.seal(access, &mut full_work()).unwrap());
        assert_eq!(built.page(), written.page(), "{sequence}");

        for tree in [&mut written, &mut built] {
            tree.write(key(N), int(-2), access).unwrap();
            assert!(tree.remove(&key(9), access).unwrap().is_some());
            seal_in_slices(tree, access);
        }
        assert_eq!(built.page(), written.page(), "{sequence}");

        let empty = tree::Tree::<PageValue>::nested_from_rows(
            Vec::<(Vec<u8>, PageValue)>::new(),
            sequence,
            access,
        )
        .unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.page(), None);

        for n in [1, 3, 64] {
            let rows: Vec<_> = (0..n).rev().map(|i| (key(i), int(i as i64))).collect();
            let mut written = tree::Tree::<PageValue>::new(sequence);
            for (key, value) in rows.clone() {
                written.write(key, value, access).unwrap();
            }
            let mut built = tree::Tree::nested_from_rows(rows, sequence, access).unwrap();
            assert!(written.seal_nested(access, &mut full_work()).unwrap());
            assert!(built.seal_nested(access, &mut full_work()).unwrap());
            assert_eq!(
                lash_kernel_value::tokens::encode_body(&built),
                lash_kernel_value::tokens::encode_body(&written),
                "{sequence} {n}"
            );
        }
    }
}

/// A named edit of a tree.
type Edit<'a> = (&'static str, Box<dyn Fn(&mut tree::Tree<PageValue>) + 'a>);

/// The number of index pages from `root` down to a leaf.
fn index_height(pages: &Pages, root: PageId) -> u64 {
    let mut height = 0;
    let mut page = &pages.pages[&root];
    while matches!(page.kind, PageKind::KEYED_INDEX | PageKind::SEQUENCE_INDEX) {
        height += 1;
        page = &pages.pages[&page.children[0].id];
    }
    height
}

/// Seam H locality: an update, an insert and a removal of a present row in
/// a large keyed tree, and an update, an append and a removal of the last
/// cell of a large sequence, each re-encode a bounded number of leaves
/// plus the index path.
#[test]
fn a_point_edit_reseals_a_bounded_number_of_leaves_and_its_index_path() {
    const N: u64 = 100_000;
    for sequence in [false, true] {
        let (access, pages) = fresh();
        let access = &access;
        let key = |i: u64| {
            if sequence {
                ordinal_key(i).to_vec()
            } else {
                i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes().to_vec()
            }
        };
        let mut rows = tree::Tree::<PageValue>::new(sequence);
        for i in 0..N {
            rows.write(key(i), int(i as i64), access).unwrap();
        }
        assert!(rows.seal(access, &mut full_work()).unwrap());
        let height = index_height(&pages.borrow(), rows.page().unwrap().id);
        assert!(height >= 2, "a deep tree: {height}");
        let (leaf, index) = if sequence {
            (PageKind::SEQUENCE_LEAF, PageKind::SEQUENCE_INDEX)
        } else {
            (PageKind::KEYED_LEAF, PageKind::KEYED_INDEX)
        };
        let middle = N / 2 + 17;
        let edits: [Edit<'_>; 3] = if sequence {
            [
                (
                    "update",
                    Box::new(|t| t.write(key(middle), int(-1), access).unwrap()),
                ),
                (
                    "append",
                    Box::new(|t| t.write(key(N), int(-2), access).unwrap()),
                ),
                (
                    "remove last",
                    Box::new(|t| assert!(t.remove(&key(N), access).unwrap().is_some())),
                ),
            ]
        } else {
            [
                (
                    "update",
                    Box::new(|t| t.write(key(middle), int(-1), access).unwrap()),
                ),
                (
                    "insert",
                    Box::new(|t| t.write(key(N), int(-2), access).unwrap()),
                ),
                (
                    "remove",
                    Box::new(|t| assert!(t.remove(&key(middle + 1), access).unwrap().is_some())),
                ),
            ]
        };
        for (name, edit) in edits {
            let before = pages.borrow().kinds.clone();
            edit(&mut rows);
            assert!(rows.seal(access, &mut full_work()).unwrap());
            let after = &pages.borrow().kinds;
            let delta = |kind| after.get(&kind).unwrap_or(&0) - before.get(&kind).unwrap_or(&0);
            let (leaves, indexes) = (delta(leaf), delta(index));
            println!("{sequence} {name}: height={height} leaves={leaves} index_pages={indexes}");
            assert!(
                (1..=3).contains(&leaves),
                "{sequence} {name}: {leaves} leaves"
            );
            assert!(
                indexes <= 2 * height,
                "{sequence} {name}: {indexes} index pages over height {height}"
            );
        }
    }
}

/// The leaves of `kind` in a store, in key order of their first rows, with
/// each row's key and boolean cell.
/// Sealed leaves, each as its rows' keys and boolean cells.
type Leaves = Vec<Vec<(Vec<u8>, bool)>>;

fn sealed_leaves(pages: &Pages, kind: PageKind) -> Leaves {
    use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenReader};
    let mut leaves = Vec::new();
    for leaf in pages.pages.values().filter(|p| p.kind == kind) {
        let mut input = TokenReader::new(&leaf.tokens);
        let len = input.seq().unwrap();
        let mut rows = Vec::new();
        for _ in 0..len {
            input.expect_struct(2).unwrap();
            let key = input.bytes().unwrap().to_vec();
            let PageValue::Bool(cell) = PageValue::decode(&mut input).unwrap() else {
                panic!("bool cell");
            };
            rows.push((key, cell));
        }
        input.finish().unwrap();
        leaves.push(rows);
    }
    leaves.sort();
    leaves
}

fn seal_bools(keys: &[Vec<u8>], sequence: bool) -> (Leaves, PageKind) {
    let (access, pages) = fresh();
    let access = &access;
    let mut rows = tree::Tree::<PageValue>::new(sequence);
    for (n, key) in keys.iter().enumerate() {
        rows.write(key.clone(), PageValue::Bool(n % 2 == 0), access)
            .unwrap();
    }
    assert!(rows.seal(access, &mut full_work()).unwrap());
    let store = pages.borrow();
    let top = store.pages[&rows.page().unwrap().id].kind;
    let leaf = if sequence {
        PageKind::SEQUENCE_LEAF
    } else {
        PageKind::KEYED_LEAF
    };
    let leaves = sealed_leaves(&store, leaf);
    drop(store);
    access.clear();
    for (n, key) in keys.iter().enumerate() {
        assert!(
            matches!(rows.get(key, access).unwrap(), Some(PageValue::Bool(b)) if b == (n % 2 == 0))
        );
    }
    (leaves, top)
}

/// Seam H: a sequence leaf `k` holds cells `64k ..< 64(k+1)`; literal
/// vectors, independent of the packing constants.
#[test]
fn sequence_leaves_hold_64_cells_by_position() {
    for (count, sizes) in [
        (63, vec![63]),
        (64, vec![64]),
        (65, vec![64, 1]),
        (129, vec![64, 64, 1]),
    ] {
        let keys: Vec<_> = (0..count).map(|i| ordinal_key(i).to_vec()).collect();
        let (leaves, top) = seal_bools(&keys, true);
        assert_eq!(
            top,
            if count <= 64 {
                PageKind::SEQUENCE_LEAF
            } else {
                PageKind::SEQUENCE_INDEX
            }
        );
        let mut actual: Vec<_> = leaves.iter().map(Vec::len).collect();
        actual.sort_by(|a, b| b.cmp(a));
        assert_eq!(actual, sizes, "{count} cells");
        // Leaf k starts at cell 64k.
        for leaf in &leaves {
            let first = u64::from_be_bytes(leaf[0].0.clone().try_into().unwrap());
            assert_eq!(first % 64, 0, "{count} cells");
        }
    }
}

/// Seam H: `H` is fixed and seedless (literal vectors).
#[test]
fn the_boundary_hash_is_fixed() {
    assert_eq!(tree::boundary_hash(0, b""), 0x813f_0174_a236_7c13);
    assert_eq!(tree::boundary_hash(1, b"abc"), 0xd05d_b19d_7771_29f7);
    assert_eq!(tree::boundary_hash(0, b"lash"), 0x1d92_f986_4f93_e9b7);
}

/// Whether a keyed row of `size` packing bytes under `key` ends its leaf by
/// its boundary hash, stated from seam H's rule with literal constants: it
/// ends where `H(0, key)` falls in the share `max(1/64, size/8192)` of the
/// hash range.
fn natural(key: &[u8], size: u64) -> bool {
    let weight = if size * 64 <= 8192 {
        u64::MAX / 64
    } else {
        (u64::MAX / 8192).saturating_mul(size)
    };
    tree::boundary_hash(0, key) <= weight
}

/// A keyed row's packing bytes: its encoded row tokens.
fn row(key: &[u8]) -> u64 {
    use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenWriter};
    let mut out = TokenWriter::new();
    out.seq(2);
    out.bytes(key);
    PageValue::Bool(true).encode(&mut out);
    out.len() as u64
}

/// `prefix` extended by the first byte that makes a key whose row does not
/// end its leaf.
fn plain(prefix: &[u8]) -> Vec<u8> {
    (0..=255u8)
        .map(|b| [prefix, &[b]].concat())
        .find(|key| !natural(key, row(key)))
        .unwrap()
}

/// Seam H: a keyed leaf ends after a row whose `H(0, key)` falls in the
/// share `max(1/64, size/8192)` of the hash range, so a large row ends it
/// more often and a leaf holds about 8192 bytes; after its 128th row; or
/// after the row that brings its encoded rows to 32768 bytes, each counted
/// from its own start. Where a leaf ends depends on its last row alone
/// until a cap binds: an 8192-byte cap ended leaves by their start, so one
/// removed row re-cut every leaf after it and a one-binding commit grew
/// with the session.
#[test]
fn keyed_leaves_end_at_natural_rows_and_their_caps() {
    // The count cap: 129 rows with no natural key.
    let keys: Vec<_> = (0..129u16).map(|i| plain(&i.to_be_bytes())).collect();
    let (leaves, top) = seal_bools(&keys, false);
    assert_eq!(top, PageKind::KEYED_INDEX);
    let sizes: Vec<_> = leaves.iter().map(Vec::len).collect();
    assert_eq!(sizes, [128, 1]);

    // A natural key ends its leaf.
    let natural_key = (0..=u16::MAX)
        .map(|i| [&[5u8][..], &i.to_be_bytes()].concat())
        .find(|key| tree::boundary_hash(0, key) <= u64::MAX / 64)
        .unwrap();
    let keys = [
        plain(&[1]),
        plain(&[2]),
        natural_key,
        plain(&[6]),
        plain(&[7]),
    ];
    let (leaves, _) = seal_bools(&keys, false);
    let firsts: Vec<_> = leaves
        .iter()
        .map(|leaf| (leaf[0].0.clone(), leaf.len()))
        .collect();
    assert_eq!(firsts, [(keys[0].clone(), 3), (keys[3].clone(), 2)]);

    // A large row ends its leaf by its share of 8192 bytes: a row of about
    // 4000 bytes whose hash is past 1/64 of the range but within its share
    // (about half) ends the leaf; one past its share does not.
    let large = |ends: bool| {
        (0..=u16::MAX)
            .map(|i| {
                let mut key = vec![5u8; 4_000];
                key.extend(i.to_be_bytes());
                key
            })
            .find(|key| {
                let hash = tree::boundary_hash(0, key);
                hash > u64::MAX / 64 && natural(key, row(key)) == ends
            })
            .unwrap()
    };
    for ends in [true, false] {
        let keys = [plain(&[1]), large(ends), plain(&[6])];
        let (leaves, _) = seal_bools(&keys, false);
        let sizes: Vec<_> = leaves.iter().map(Vec::len).collect();
        assert_eq!(
            sizes,
            if ends { vec![2, 1] } else { vec![3] },
            "ends {ends}"
        );
    }

    // The byte cap: the leaf ends after the row that reaches 32768 bytes.
    for (total, split) in [(32_767, false), (32_768, true)] {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 1..=32u8 {
            let mut key = vec![i; 1_000];
            while natural(&key, row(&key)) {
                *key.last_mut().unwrap() ^= 0x55;
                key.push(i);
            }
            keys.push(key);
        }
        let mut last = vec![33u8; 8];
        let filled =
            |keys: &[Vec<u8>], last: &[u8]| keys.iter().map(|k| row(k)).sum::<u64>() + row(last);
        while filled(&keys, &last) < total {
            last.insert(1, 0);
            if natural(&last, row(&last)) {
                *last.last_mut().unwrap() ^= 0x80;
            }
        }
        assert_eq!(filled(&keys, &last), total);
        assert!(!natural(&last, row(&last)));
        keys.push(last);
        keys.push(plain(&[40]));
        let (leaves, _) = seal_bools(&keys, false);
        let sizes: Vec<_> = leaves.iter().map(Vec::len).collect();
        assert_eq!(
            sizes,
            if split { vec![33, 1] } else { vec![34] },
            "{total} bytes"
        );
    }
}

/// D-IMM: greedy UTF-8 chunks end at scalar boundaries at or below 4096 bytes.
#[test]
fn payload_leaf_boundaries_follow_the_frozen_4096_byte_rule() {
    use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenReader};
    for (len, chunks) in [
        (4095, vec![4095]),
        (4096, vec![4096]),
        (4097, vec![1, 4096]),
    ] {
        let (access, pages) = fresh();
        let value = format!("{}éx", "a".repeat(len - 3));
        let a = payload::build(&access, value.as_bytes(), true).unwrap();
        let b = payload::build(&access, value.as_bytes(), true).unwrap();
        assert_eq!(a.id, b.id, "canonical partitions share a root");
        let store = pages.borrow();
        assert_eq!(
            store.pages[&a.page.id].kind,
            if len <= 4096 {
                PageKind::PAYLOAD_LEAF
            } else {
                PageKind::PAYLOAD_INDEX
            }
        );
        let mut actual = Vec::new();
        for leaf in store
            .pages
            .values()
            .filter(|p| p.kind == PageKind::PAYLOAD_LEAF)
        {
            let mut input = TokenReader::new(&leaf.tokens);
            input.expect_struct(3).unwrap();
            assert!(input.bool().unwrap());
            let summary = TextSummary::decode(&mut input).unwrap();
            let chunk = input.text().unwrap();
            assert_eq!(summary.utf8_bytes, chunk.len() as u64);
            actual.push(chunk.len());
            input.finish().unwrap();
        }
        actual.sort();
        assert_eq!(actual, chunks);
        drop(store);
        access.clear();
        let mut read = Vec::new();
        payload::visit(
            &access,
            a,
            lash_kernel_doc::heap_values::text_summary(&value),
            true,
            0..len as u64,
            &mut |chunk| {
                read.extend_from_slice(chunk);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(read, value.as_bytes());
    }
}
