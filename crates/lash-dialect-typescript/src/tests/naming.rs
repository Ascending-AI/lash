//! Laws of operation names in TypeScript cells: OP-NAME,
//! OP-SPELLING-REPAIR, NS-ESCAPE and PRINT-ROUNDTRIP.

use std::collections::BTreeMap;

use lash_vocabulary::{EffectName, Name, Param, Signature, Type};

use super::{lower_against, print_document};

/// Each of `names` as an operation taking one `input` and giving anything.
fn effects(names: &[&str]) -> BTreeMap<EffectName, Signature> {
    names
        .iter()
        .map(|name| {
            (
                EffectName::new(*name).expect("an effect name"),
                Signature {
                    params: vec![Param {
                        name: Name::new("input"),
                        ty: Type::Any,
                        optional: false,
                    }],
                    result: Type::Any,
                },
            )
        })
        .collect()
}

/// OP-NAME: a TypeScript cell names `inbox.send_item` by its camelCase
/// spelling and performs it by its canonical effect name; a record's field
/// names are data and pass as written.
#[test]
fn a_multiword_operation_is_written_in_camel_case_and_performed_canonically() {
    let lowered = lower_against(
        "await inbox.sendItem({ item_id: 1 });",
        &[],
        &effects(&["inbox.send_item"]),
    )
    .expect("the camelCase spelling lowers");
    let text = print_document(&lowered.document);
    assert!(
        text.contains(&format!(
            "perform {}(",
            super::linked_inspection::operation_slot(&lowered.document, "inbox.send_item")
                .expect("operation import")
                .0
        )),
        "{text}"
    );
    assert!(text.contains("item_id"), "{text}");
}

/// OP-SPELLING-REPAIR: the canonical spelling of a multiword operation is
/// another dialect's, refused with the camelCase repair.
#[test]
fn the_canonical_spelling_of_a_multiword_operation_is_refused_with_its_repair() {
    let error = lower_against(
        "await inbox.send_item({});",
        &[],
        &effects(&["inbox.send_item"]),
    )
    .expect_err("TypeScript spells `send_item` as `sendItem`");
    assert_eq!(error.code.as_str(), "TS_OPERATION_SPELLING", "{error}");
    assert!(
        error
            .suggestions
            .iter()
            .any(|repair| repair == "write `inbox.sendItem`"),
        "{error:?}"
    );
}

/// NS-ESCAPE: an operation whose spelling is a keyword stays callable
/// through the qualified escape.
#[test]
fn a_keyword_operation_is_callable_through_the_qualified_escape() {
    let lowered = lower_against(
        "await records[\"delete\"]({ id: 1 });",
        &[],
        &effects(&["records.delete"]),
    )
    .expect("the escape names the operation");
    let text = print_document(&lowered.document);
    assert!(
        text.contains(&format!(
            "perform {}(",
            super::linked_inspection::operation_slot(&lowered.document, "records.delete")
                .expect("operation import")
                .0
        )),
        "{text}"
    );
}
