//! Laws of operation names in Python cells: OP-NAME, NS-ESCAPE
//! and PRINT-ROUNDTRIP.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{CellEnvironment, Environment, Lowered, Mode};
use lash_kernel_doc::{self as k};

use lash_vocabulary::{EffectName, Name, Param, Signature, Type};

use super::machine;

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

/// Lowers `source` as a cell of a session holding `bindings` whose host
/// offers `names`.
fn lower(
    source: &str,
    bindings: &[&str],
    names: &[&str],
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    let namespaces = crate::tests::linked::namespaces(&effects(names), &BTreeMap::new())
        .expect("the law's effects are operations");
    let bindings = bindings
        .iter()
        .map(|name| Name::new(*name))
        .collect::<BTreeSet<_>>();
    crate::lower(
        source,
        &Environment::new(machine::library(), &namespaces),
        Mode::Cell(CellEnvironment {
            bindings: &bindings,
            functions: &BTreeMap::new(),
        }),
    )
}

/// OP-NAME: a Python cell names an operation by its canonical path, with
/// no flattening, and performs it by its canonical effect name.
#[test]
fn a_dotted_call_performs_the_operation_by_its_canonical_name() {
    for (source, operation) in [
        (
            "await web.fetch({\"url\": \"https://example.test\"})\n",
            "web.fetch",
        ),
        (
            "await inbox.send_item({\"item_id\": 1})\n",
            "inbox.send_item",
        ),
    ] {
        let lowered = lower(source, &[], &[operation]).unwrap_or_else(|error| panic!("{error}"));
        let text = k::print_document(&lowered.document);
        assert!(
            text.contains(&format!(
                "perform {}(",
                super::linked::operation_slot(&lowered.document, operation)
                    .expect("operation import")
                    .0
            )),
            "{text}"
        );
    }
    let error = lower("await web_fetch({})\n", &[], &["web.fetch"])
        .expect_err("the flattened name is no operation");
    assert!(!error.code.is_empty(), "{error}");
}

/// NS-ESCAPE: an operation whose spelling is a Python keyword stays
/// callable through the qualified escape.
#[test]
fn a_keyword_operation_is_callable_through_the_qualified_escape() {
    let lowered = lower(
        "await getattr(processes, \"await\")({\"id\": 1})\n",
        &[],
        &["processes.await"],
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let text = k::print_document(&lowered.document);
    assert!(
        text.contains(&format!(
            "perform {}(",
            super::linked::operation_slot(&lowered.document, "processes.await")
                .expect("operation import")
                .0
        )),
        "{text}"
    );
}
