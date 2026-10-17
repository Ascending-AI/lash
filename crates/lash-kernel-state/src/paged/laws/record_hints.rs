//! PLAN-INDEP for verified record-position hints.

use super::super::*;
use super::heap::{fresh, reopen};
use lash_kernel_doc::heap::*;

fn int(value: i64) -> Value {
    Value::Int(IntRef::Inline(value.into()))
}

/// PLAN-INDEP: a field position is a hint, never a record-layout proof.
/// Verify its name on another record, after deletion/reinsertion and after
/// a cold open, including a field whose name lives in payload pages.
#[test]
fn record_read_hints_verify_names_against_the_current_rows() {
    let (mut heap, pages) = fresh();
    let field = "field";
    let field_name = heap.text(field).unwrap();
    let other_name = heap.text("other").unwrap();
    let record = heap
        .allocate_object(ObjectInit::Record(vec![
            (field_name.clone(), int(1)),
            (other_name.clone(), int(2)),
        ]))
        .unwrap();
    let reordered = heap
        .allocate_object(ObjectInit::Record(vec![
            (other_name, int(3)),
            (field_name, int(4)),
        ]))
        .unwrap();
    let check = |heap: &PagedHeap, record, name: &str, hint| {
        let (actual, position) = heap
            .record_get_hinted(record, FieldName::Static(name), hint)
            .unwrap();
        let expected = heap.record_get(record, FieldName::Static(name)).unwrap();
        match (actual, expected) {
            (Some(actual), Some(expected)) => assert!(heap.equal(&actual, &expected).unwrap()),
            (None, None) => {}
            pair => panic!("hinted and generic reads differ: {pair:?}"),
        }
        position
    };
    let first = check(&heap, record, field, None);
    assert!(first.is_some());
    let second = check(&heap, reordered, field, first);
    assert_ne!(second, first);
    check(&heap, record, field, second);
    heap.record_remove(record, FieldName::Static(field))
        .unwrap();
    assert_eq!(check(&heap, record, field, first), None);
    heap.record_write(record, FieldName::Static(field), int(5))
        .unwrap();
    let reinserted = check(&heap, record, field, first);
    assert_ne!(reinserted, first);
    heap.record_write(record, FieldName::Static(field), int(6))
        .unwrap();
    check(&heap, record, field, reinserted);
    let long = "long-field-name".repeat(1024);
    heap.record_write(record, FieldName::Static(&long), int(7))
        .unwrap();
    let paged = check(&heap, record, &long, first);
    let heap = reopen(&mut heap, &pages);
    check(&heap, record, field, reinserted);
    check(&heap, record, &long, paged);
    check(&heap, reordered, field, reinserted);
    let text = match Value::text(field) {
        Value::Text(text) => text,
        _ => unreachable!(),
    };
    let (value, _) = heap
        .record_get_hinted(record, FieldName::Text(&text), first)
        .unwrap();
    assert!(heap.equal(&value.unwrap(), &int(6)).unwrap());
    assert!(
        heap.record_get_hinted(record, FieldName::Static("missing"), first)
            .unwrap()
            .0
            .is_none()
    );
}
