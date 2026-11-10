// Independent canonical page-boundary vectors, included in heap laws.
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
        let keys: Vec<_> = (0..count).map(|i| directory::ordinal_key(i).to_vec()).collect();
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
    let keys = [plain(&[1]), plain(&[2]), natural_key, plain(&[6]), plain(&[7])];
    let (leaves, _) = seal_bools(&keys, false);
    let firsts: Vec<_> = leaves.iter().map(|leaf| (leaf[0].0.clone(), leaf.len())).collect();
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
        assert_eq!(sizes, if ends { vec![2, 1] } else { vec![3] }, "ends {ends}");
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
        let filled = |keys: &[Vec<u8>], last: &[u8]| keys.iter().map(|k| row(k)).sum::<u64>() + row(last);
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
        assert_eq!(sizes, if split { vec![33, 1] } else { vec![34] }, "{total} bytes");
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
        let (heap, pages) = fresh();
        let value = format!("{}éx", "a".repeat(len - 3));
        let a = heap.text(&value).unwrap();
        let b = heap.text(&value).unwrap();
        let (TextPayload::Paged(a), TextPayload::Paged(b)) = (a.body, b.body) else {
            panic!("paged");
        };
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
        heap.pages().clear();
        let mut read = Vec::new();
        heap.payload_visit(
            &PageValue::Text(TextRef {
                normal: Default::default(),
                body: TextPayload::Paged(a),
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
