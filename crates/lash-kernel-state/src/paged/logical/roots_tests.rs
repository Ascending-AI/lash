//! Same-price boundary values cannot authenticate another source transfer.
use super::*;
use crate::arena::Arena;
use lash_kernel_doc::{Value, heap::PageValue};

pub(super) fn exact_values() {
    let (heap, _) = fresh();
    let mut ledger = Ledger::fresh();
    let mut arena = Arena::new();
    let seven = Value::Int(7);
    let eight = Value::Int(8);
    let outputs = [(owner(0), RootValue::Value(&seven))];
    let mut prepared = ledger
        .prepare_source_boundary(arena.registration().unwrap(), &heap, &[], &outputs)
        .unwrap();
    assert!(prepared.matches_roots(&ledger, &[], &outputs));
    assert!(
        !prepared.matches_roots(&ledger, &[], &[(owner(0), RootValue::Value(&eight))]),
        "same-price output substitution must not authenticate a source boundary"
    );
    assert!(!prepared.matches_roots(&ledger, &[], &[(owner(1), RootValue::Value(&seven))]));
    assert!(!prepared.matches_roots(
        &ledger,
        &[],
        &[
            (owner(0), RootValue::Value(&seven)),
            (owner(0), RootValue::Value(&seven))
        ]
    ));
    let page = PageValue::Int(7.into());
    assert!(
        !prepared.matches_roots(&ledger, &[], &[(owner(0), RootValue::Page(&page))]),
        "a different provenance cannot reuse a prepared native root"
    );
    prepared
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |_, _| {})
        .unwrap();
    let mut moved = ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            &heap,
            &[owner(0)],
            &[(owner(1), RootValue::Existing(owner(0)))],
        )
        .unwrap();
    assert!(moved.matches_roots(
        &ledger,
        &[owner(0)],
        &[(owner(1), RootValue::Existing(owner(0)))]
    ));
    assert!(!moved.matches_roots(
        &ledger,
        &[owner(0), owner(0)],
        &[(owner(1), RootValue::Existing(owner(0)))]
    ));
    assert!(!moved.matches_roots(
        &ledger,
        &[owner(0)],
        &[(owner(1), RootValue::Existing(owner(9)))]
    ));
    ledger.handoff().unwrap();
    assert!(
        !moved.matches_roots(
            &ledger,
            &[owner(0)],
            &[(owner(1), RootValue::Existing(owner(0)))]
        ),
        "a source handoff expires exact root facts"
    );
    drop(moved);
    let created = ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            &heap,
            &[],
            &[(owner(2), RootValue::Value(&seven))],
        )
        .unwrap();
    let mut combined = created
        .then_ownership(
            &ledger,
            heap.pages(),
            &[owner(2)],
            &[(owner(3), RootValue::Existing(owner(2)))],
        )
        .unwrap();
    assert!(
        combined.matches_roots(&ledger, &[], &[(owner(3), RootValue::Value(&seven))]),
        "interior roots cancel while final values retain their exact identity"
    );
    drop(combined);
    let mut page_root = ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            &heap,
            &[],
            &[(owner(2), RootValue::Page(&page))],
        )
        .unwrap();
    assert!(page_root.matches_roots(&ledger, &[], &[(owner(2), RootValue::Page(&page))]));
    let wrong = PageValue::Int(8.into());
    assert!(!page_root.matches_roots(&ledger, &[], &[(owner(2), RootValue::Page(&wrong))]));
}

pub(super) fn cached_transfers() {
    use crate::paged::census_impl::TraceLimits;
    use crate::paged::logical::Change;

    fn holding(
        value: Option<Holding>,
        heap: &crate::paged::PagedHeap,
    ) -> Option<(u64, Vec<(Identity, u64)>)> {
        value.map(|value| {
            let mut edges = Vec::new();
            value
                .visit_edges(heap.pages(), &mut |entity, count| {
                    edges.push((entity, count));
                    Ok(())
                })
                .unwrap();
            (value.bytes(), edges)
        })
    }
    fn check(
        label: &'static str,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &crate::paged::PagedHeap,
        failures: &mut Vec<&'static str>,
        projection: Value,
    ) {
        let oracle = ledger.sibling_for_law().without_facts_for_law();
        let mut agrees = true;
        for index in 0..4 {
            // The second lookup exercises the present-only observation.
            let _ = ledger.holding(owner(index), heap.pages()).unwrap();
            agrees &= holding(ledger.holding(owner(index), heap.pages()).unwrap(), heap)
                == holding(oracle.holding(owner(index), heap.pages()).unwrap(), heap);
        }
        for value in [Value::Int(7), Value::Int(8), projection] {
            // Compare complete transfers with a fresh canonical query as oracle.
            let inputs = if oracle.holding(owner(0), heap.pages()).unwrap().is_some() {
                vec![owner(0)]
            } else {
                vec![]
            };
            let outputs = [(owner(0), RootValue::Value(&value))];
            let _ = ledger
                .prepare_source_ownership(arena, heap, &inputs, &outputs)
                .unwrap();
            let cached = ledger
                .prepare_source_ownership(arena, heap, &inputs, &outputs)
                .unwrap();
            let fresh = oracle
                .prepare_source_ownership(arena, heap, &inputs, &outputs)
                .unwrap();
            agrees &= cached.balance().unwrap() == fresh.balance().unwrap()
                && cached.peak() == fresh.peak();
        }
        if !agrees {
            failures.push(label);
        }
    }
    fn transfer(
        ledger: &mut Ledger,
        arena: &mut Arena,
        heap: &crate::paged::PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) {
        let prepared = ledger
            .prepare_source_ownership(arena, heap, inputs, outputs)
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
    }
    let (heap, _) = fresh();
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let mut failures = Vec::new();
    let child = arena.create_list(&[Value::Int(7)]);
    arena.identify(child.handle().unwrap()).unwrap();
    let parent = arena.create_list(&[child]);
    arena.identify(parent.handle().unwrap()).unwrap();
    let tuple = arena.tuple(vec![parent, child], 256).unwrap();
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[],
        &[(owner(0), RootValue::Value(&tuple))],
    );
    check(
        "initial projection",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[],
        &[(owner(1), RootValue::Existing(owner(0)))],
    );
    check(
        "alias creation",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    // Changing the child must not turn its parent's direct holding into a
    // cached mutable-body or release summary.
    arena
        .list_set(child.handle().unwrap(), 0, Value::Null)
        .unwrap();
    let entity = arena
        .facts(child.handle().unwrap())
        .unwrap()
        .ledger_entity()
        .unwrap();
    let body = Holding::native_body(&mut arena, heap.pages(), child.handle().unwrap()).unwrap();
    let prepared = ledger
        .prepare([Change::Mutate { entity, body }], heap.pages())
        .unwrap();
    ledger
        .commit(prepared.admit(u64::MAX).ok().unwrap())
        .unwrap();
    check(
        "member mutation",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    transfer(&mut ledger, &mut arena, &heap, &[owner(0)], &[]);
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[],
        &[(owner(0), RootValue::Value(&Value::Null))],
    );
    check(
        "release and recreate",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    for value in [Value::Int(7), Value::Int(8)] {
        transfer(
            &mut ledger,
            &mut arena,
            &heap,
            &[owner(0)],
            &[(owner(0), RootValue::Value(&value))],
        );
        check(
            "equal-price replacement",
            &ledger,
            &mut arena,
            &heap,
            &mut failures,
            tuple,
        );
    }
    let mut sibling = ledger.sibling_for_law();
    transfer(
        &mut sibling,
        &mut arena,
        &heap,
        &[owner(0)],
        &[(owner(0), RootValue::Value(&Value::Null))],
    );
    // Both drafts have the same identity and revision but different rows.
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[owner(0)],
        &[(owner(0), RootValue::Value(&tuple))],
    );
    check(
        "sibling draft",
        &sibling,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    check(
        "committed view after sibling",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    let provisional = arena.create_list(&[]);
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[],
        &[(owner(2), RootValue::Value(&provisional))],
    );
    let _ = ledger.holding(owner(2), heap.pages()).unwrap();
    arena.identify(provisional.handle().unwrap()).unwrap();
    ledger.rekey(&arena.take_rekeyed(), heap.pages()).unwrap();
    check(
        "provisional rekey",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    ledger
        .normalize(heap.pages(), TraceLimits::default())
        .unwrap();
    check(
        "normalization",
        &ledger,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    // Only durable session roots survive sealing; the immutable tuple retains
    // the same aliases after the warm cache is discarded by reopening.
    transfer(
        &mut ledger,
        &mut arena,
        &heap,
        &[owner(0), owner(1), owner(2)],
        &[(Owner::Session([0; 32]), RootValue::Existing(owner(0)))],
    );
    let root = ledger.seal(heap.pages(), budget()).unwrap().unwrap();
    let mut opened = Ledger::open(root).unwrap();
    transfer(
        &mut opened,
        &mut arena,
        &heap,
        &[],
        &[(owner(0), RootValue::Existing(Owner::Session([0; 32])))],
    );
    check(
        "cold reopen",
        &opened,
        &mut arena,
        &heap,
        &mut failures,
        tuple,
    );
    let mut foreign = Arena::from_ordinal(99);
    let foreign_child = foreign.create_list(&[Value::Int(7)]);
    foreign.identify(foreign_child.handle().unwrap()).unwrap();
    let foreign_parent = foreign.create_list(&[foreign_child]);
    foreign.identify(foreign_parent.handle().unwrap()).unwrap();
    let foreign_tuple = foreign
        .tuple(vec![foreign_parent, foreign_child], 256)
        .unwrap();
    assert!(foreign_tuple.same(&tuple));
    check(
        "foreign arena",
        &opened,
        &mut foreign,
        &heap,
        &mut failures,
        foreign_tuple,
    );
    assert!(
        failures.is_empty(),
        "cached transfers differ from fresh queries: {failures:?}"
    );
}
