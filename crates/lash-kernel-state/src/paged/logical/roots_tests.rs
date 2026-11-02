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
