/// FIG-6613: only a retained snapshot justifies copying owned tree cells.
#[test]
fn fresh_dirty_nodes_do_not_clone_owned_cells() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
    use std::cell::Cell as Counter;
    thread_local! {
        static COPIES: Counter<u64> = const { Counter::new(0) };
    }
    struct Counted(u64);
    impl Clone for Counted {
        fn clone(&self) -> Self {
            COPIES.set(COPIES.get() + 1);
            Self(self.0)
        }
    }
    impl TokenCodec for Counted {
        fn encode(&self, out: &mut TokenWriter) {
            self.0.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(Self(u64::decode(input)?))
        }
    }
    impl tree::Cell for Counted {
        fn cost(&self) -> u64 {
            self.0 + 1
        }
        fn task_edges(&self) -> u64 {
            self.0 % 2
        }
        fn live_members(&self) -> u64 {
            1
        }
    }
    for sequence in [false, true] {
        let (heap, _) = fresh();
        let mut rows = tree::Tree::new(sequence);
        COPIES.set(0);
        rows.write(directory::ordinal_key(0), Counted(0), heap.pages())
            .unwrap();
        let first = COPIES.replace(0);
        // Cross both leaf and index split boundaries without retaining a version.
        for n in 1..2200 {
            rows.write(directory::ordinal_key(n), Counted(n), heap.pages())
                .unwrap();
        }
        let splits = COPIES.replace(0);
        assert_eq!(rows.len(), 2200);
        assert_eq!(rows.cost(), 2200 * 2201 / 2);
        assert_eq!(rows.task_edges(), 1100);
        assert_eq!(rows.live_members(heap.pages()).unwrap(), 2200);
        let prior = rows.clone();
        rows.write(directory::ordinal_key(0), Counted(9999), heap.pages())
            .unwrap();
        let cow = COPIES.replace(0);
        assert!(cow > 0, "a retained snapshot requires real COW");
        assert_eq!(
            prior
                .get_with(&directory::ordinal_key(0), heap.pages(), |v| v.unwrap().0)
                .unwrap(),
            0
        );
        assert_eq!(
            rows.get_with(&directory::ordinal_key(0), heap.pages(), |v| v.unwrap().0)
                .unwrap(),
            9999
        );
        println!("sequence={sequence} first={first} splits={splits} snapshot_copies={cow}");
        assert_eq!(
            (first, splits),
            (0, 0),
            "fresh owned cells move into dirty nodes"
        );
    }
}

/// NATIVE-PROPOSAL-MOVE-ONCE: preparation runs once across a refused
/// admission; the retry traces the proposed native-only child and spends
/// the same logical bytes as an ordinary allocation.
#[test]
fn native_proposal_builds_its_head_once_across_precision() {
    use super::super::heap_impl::HEAD_BUILDS;
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(16384)).unwrap(),
    ))
    .unwrap();
    heap.execution_roots_moved();
    heap.allocate(Object::List(vec![Value::Null; 700])).unwrap();
    let child = heap.allocate(Object::List(vec![Value::Null; 100])).unwrap();
    let ordinal = heap.next_ordinal();
    let population = heap.data_only_certificate();
    HEAD_BUILDS.set(0);
    let mut proposal = heap
        .prepare_allocation(Object::List(vec![Value::List(child); 257]))
        .unwrap();
    let before = heap.certificate().refinements;
    assert!(matches!(
        heap.allocate_prepared(&mut proposal),
        Err(NativeError::Memory)
    ));
    assert!(heap.take_refusal().is_some());
    assert_eq!(heap.next_ordinal(), ordinal);
    assert_eq!(heap.data_only_certificate(), population);
    heap.set_execution_roots(heap.tasks.clone(), heap.joins.clone(), &[], &[], 0, &[])
        .unwrap();
    let parent = heap.allocate_prepared(&mut proposal).unwrap();
    let builds = HEAD_BUILDS.replace(0);
    assert_eq!(builds, 1);
    assert_eq!(heap.certificate().refinements, before + 1);
    assert_eq!(parent.ordinal, ordinal);
    assert_eq!(heap.len(child).unwrap(), 100);
    assert_eq!(heap.len(parent).unwrap(), 257);
    assert!(matches!(heap.list_get(parent, 256).unwrap(), Some(Value::List(id)) if id == child));
    println!("head_builds={builds} precision_retries=1 parent_members=257 child_members=100");
}
