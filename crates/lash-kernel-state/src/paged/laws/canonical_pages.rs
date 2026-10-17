// Seam H laws: canonical page shape, included in heap laws.

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
        let (heap, _) = fresh();
        let access = heap.pages();
        let key = |i: u64| {
            if sequence {
                directory::ordinal_key(i)
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
        assert_eq!(descending.page().expect("sealed root").id, root, "{sequence}");
        assert_eq!(scattered.page().expect("sealed root").id, root, "{sequence}");

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
        let (heap, pages) = fresh();
        let access = heap.pages();
        let key = |i: u64| {
            if sequence {
                directory::ordinal_key(i)
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
                ("update", Box::new(|t| t.write(key(middle), int(-1), access).unwrap())),
                ("append", Box::new(|t| t.write(key(N), int(-2), access).unwrap())),
                (
                    "remove last",
                    Box::new(|t| assert!(t.remove(&key(N), access).unwrap().is_some())),
                ),
            ]
        } else {
            [
                ("update", Box::new(|t| t.write(key(middle), int(-1), access).unwrap())),
                ("insert", Box::new(|t| t.write(key(N), int(-2), access).unwrap())),
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
            assert!((1..=3).contains(&leaves), "{sequence} {name}: {leaves} leaves");
            assert!(
                indexes <= 2 * height,
                "{sequence} {name}: {indexes} index pages over height {height}"
            );
        }
    }
}
