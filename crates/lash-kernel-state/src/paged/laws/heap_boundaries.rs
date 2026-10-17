// Independent canonical page-boundary vectors, included in heap laws.
/// ADR 0145 D-CAP: literal 63/64/65-cell vectors, independent of packing constants.
#[test]
fn list_leaf_boundaries_follow_the_frozen_64_cell_rule() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenReader};
    use tree::{SealWork, Tree};
    for (count, sizes) in [(63, vec![63]), (64, vec![64]), (65, vec![32, 33])] {
        let (heap, pages) = fresh();
        let access = heap.pages();
        let mut rows = Tree::<PageValue>::new(true);
        for n in 0..count {
            rows.write(directory::ordinal_key(n), PageValue::Bool(n % 2 == 0), access)
                .unwrap();
        }
        assert!(
            rows.seal(
                access,
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX
                })
            )
            .unwrap()
        );
        let root = rows.sequence_root().unwrap();
        let store = pages.borrow();
        let top = &store.pages[&root.page().unwrap().id];
        assert_eq!(
            top.kind,
            if count <= 64 {
                PageKind::SEQUENCE_LEAF
            } else {
                PageKind::SEQUENCE_INDEX
            }
        );
        let mut actual = Vec::new();
        for leaf in store
            .pages
            .values()
            .filter(|p| p.kind == PageKind::SEQUENCE_LEAF)
        {
            let mut input = TokenReader::new(&leaf.tokens);
            let len = input.seq().unwrap();
            actual.push(len);
            for _ in 0..len {
                input.expect_struct(2).unwrap();
                let key = input.bytes().unwrap();
                let n = u64::from_be_bytes(key.try_into().unwrap());
                assert!(
                    matches!(PageValue::decode(&mut input).unwrap(), PageValue::Bool(b) if b == (n % 2 == 0))
                );
            }
            input.finish().unwrap();
        }
        actual.sort();
        assert_eq!(actual, sizes, "{count} cells");
        drop(store);
        access.clear();
        let opened = Tree::<PageValue>::of_sequence(&root);
        for n in 0..count {
            assert!(
                matches!(opened.rank_get(n, access).unwrap().unwrap().1, PageValue::Bool(b) if b == (n % 2 == 0))
            );
        }
    }
}

/// D-CAP's keyed packing estimate is key bytes + encoded cell bytes + 8 per row.
/// Two rows below/at/above 8192 must stay one leaf/one leaf/split.
#[test]
fn keyed_leaf_boundaries_follow_the_frozen_8192_byte_rule() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenReader};
    use tree::{SealWork, Tree};
    for (budget, second_key_len, split) in
        [(8191, 8166, false), (8192, 8167, false), (8193, 8168, true)]
    {
        let (heap, pages) = fresh();
        let access = heap.pages();
        let mut rows = Tree::<PageValue>::new(false);
        let keys = [vec![0], vec![1; second_key_len]];
        // PageValue::Bool is sum(2, 1) + bool: four bytes.
        // (1 + 4 + 8) + (key_len + 4 + 8).
        assert_eq!(13 + second_key_len + 12, budget);
        for (key, value) in keys.iter().zip([false, true]) {
            rows.write(key.clone(), PageValue::Bool(value), access).unwrap();
        }
        assert!(
            rows.seal(
                access,
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX
                })
            )
            .unwrap()
        );
        let root = rows.keyed_root().unwrap();
        let store = pages.borrow();
        assert_eq!(
            store.pages[&root.page().unwrap().id].kind,
            if split {
                PageKind::KEYED_INDEX
            } else {
                PageKind::KEYED_LEAF
            },
            "{budget} packing bytes"
        );
        let mut decoded = Vec::new();
        for leaf in store
            .pages
            .values()
            .filter(|p| p.kind == PageKind::KEYED_LEAF)
        {
            let mut input = TokenReader::new(&leaf.tokens);
            let count = input.seq().unwrap();
            assert_eq!(count, if split { 1 } else { 2 });
            for _ in 0..count {
                input.expect_struct(2).unwrap();
                let key = input.bytes().unwrap().to_vec();
                let PageValue::Bool(value) = PageValue::decode(&mut input).unwrap() else {
                    panic!("bool cell");
                };
                decoded.push((key, value));
            }
            input.finish().unwrap();
        }
        decoded.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            decoded,
            vec![(keys[0].clone(), false), (keys[1].clone(), true)]
        );
        drop(store);
        access.clear();
        let opened = Tree::<PageValue>::of_keyed(&root);
        for (key, value) in keys.iter().zip([false, true]) {
            assert!(matches!(opened.get(key, access).unwrap(), Some(PageValue::Bool(b)) if b == value));
        }
    }
}

/// D-IMM: greedy UTF-8 chunks end at scalar boundaries at or below 4096 bytes.
#[test]
fn payload_leaf_boundaries_follow_the_frozen_4096_byte_rule() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenReader};
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
