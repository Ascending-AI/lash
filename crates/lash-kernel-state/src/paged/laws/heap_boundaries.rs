// Independent canonical page-boundary vectors, included in heap laws.
/// The leaves of `kind` in a store, in key order of their first rows, with
/// each row's key and boolean cell.
/// Sealed leaves, each as its rows' keys and boolean cells.
type Leaves = Vec<Vec<(Vec<u8>, bool)>>;

fn sealed_leaves(pages: &Pages, kind: PageKind) -> Leaves {
    use lash_vocabulary::tokens::{TokenCodec, TokenReader};
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
    let (heap, pages) = fresh();
    let access = heap.pages();
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
        assert!(matches!(rows.get(key, access).unwrap(), Some(PageValue::Bool(b)) if b == (n % 2 == 0)));
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
        let keys: Vec<_> = (0..count).map(directory::ordinal_key).collect();
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

/// `prefix` extended by the first byte that makes a key whose leaf
/// boundary hash is not natural.
fn plain(prefix: &[u8]) -> Vec<u8> {
    (0..=255u8)
        .map(|b| [prefix, &[b]].concat())
        .find(|key| tree::boundary_hash(0, key) & 63 != 0)
        .unwrap()
}

/// Seam H: a keyed leaf ends after a row whose `H(0, key)` has its low
/// six bits zero, after its 128th row, or after the row that brings its
/// encoded rows to 8192 bytes, each counted from its own start.
#[test]
fn keyed_leaves_end_at_natural_rows_and_their_caps() {
    use lash_vocabulary::tokens::{TokenCodec, TokenWriter};
    // The count cap: 129 rows with no natural key.
    let keys: Vec<_> = (0..129u16).map(|i| plain(&i.to_be_bytes())).collect();
    let (leaves, top) = seal_bools(&keys, false);
    assert_eq!(top, PageKind::KEYED_INDEX);
    let sizes: Vec<_> = leaves.iter().map(Vec::len).collect();
    assert_eq!(sizes, [128, 1]);

    // A natural key ends its leaf.
    let natural = (0..=u16::MAX)
        .map(|i| [&[5u8][..], &i.to_be_bytes()].concat())
        .find(|key| tree::boundary_hash(0, key) & 63 == 0)
        .unwrap();
    let keys = [plain(&[1]), plain(&[2]), natural.clone(), plain(&[6]), plain(&[7])];
    let (leaves, _) = seal_bools(&keys, false);
    let firsts: Vec<_> = leaves.iter().map(|leaf| (leaf[0].0.clone(), leaf.len())).collect();
    assert_eq!(firsts, [(keys[0].clone(), 3), (keys[3].clone(), 2)]);

    // The byte cap: the leaf ends after the row that reaches 8192 bytes.
    let row = |key: &[u8]| {
        let mut out = TokenWriter::new();
        out.seq(2);
        out.bytes(key);
        PageValue::Bool(true).encode(&mut out);
        out.len()
    };
    for (total, split) in [(8191, false), (8192, true)] {
        let first = plain(&[0]);
        let mut big = vec![1; 8_000];
        big.extend(plain(&[1]));
        while row(&first) + row(&big) < total {
            big.insert(0, 1);
            if tree::boundary_hash(0, &big) & 63 == 0 {
                *big.last_mut().unwrap() ^= 0x80;
            }
        }
        assert_eq!(row(&first) + row(&big), total);
        assert!(tree::boundary_hash(0, &big) & 63 != 0);
        let keys = [first, big, plain(&[2])];
        let (leaves, _) = seal_bools(&keys, false);
        let sizes: Vec<_> = leaves.iter().map(Vec::len).collect();
        assert_eq!(sizes, if split { vec![2, 1] } else { vec![3] }, "{total} bytes");
    }
}

/// D-IMM: greedy UTF-8 chunks end at scalar boundaries at or below 4096 bytes.
#[test]
fn payload_leaf_boundaries_follow_the_frozen_4096_byte_rule() {
    use lash_vocabulary::tokens::{TokenCodec, TokenReader};
    for (len, chunks) in [
        (4095, vec![4095]),
        (4096, vec![4096]),
        (4097, vec![1, 4096]),
    ] {
        let (heap, pages) = fresh();
        let value = format!("{}éx", "a".repeat(len - 3));
        let a = heap.text(&value).unwrap();
        let b = heap.text(&value).unwrap();
        let (Payload::Paged(a), Payload::Paged(b)) = (a.body, b.body) else {
            panic!("paged");
        };
        assert_eq!(a.id, b.id, "canonical partitions share a root");
        let store = pages.borrow();
        assert_eq!(
            store.pages[&a.id].kind,
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
        heap.pages().clear();
        let mut read = Vec::new();
        heap.payload_visit(
            &PageValue::Text(TextRef {
                normal: Default::default(),
                body: Payload::Paged(a),
                summary: lash_kernel_doc::heap_values::text_summary(&value),
            }),
            0..len as u64,
            &mut |chunk| {
                let Chunk::Text(text) = chunk else {
                    panic!("text chunk");
                };
                read.extend_from_slice(text.as_bytes());
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(read, value.as_bytes());
    }
}
