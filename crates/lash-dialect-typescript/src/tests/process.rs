//! Process literals: an `async` arrow a host starts is an entry of the
//! document, and its value in the cell is a reference to it.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, EffectName, Name, Param, Signature, Type, print_document};

use super::machine;
use crate::DiagnosticCode;

/// The laws' tools and the process effects.
fn effects() -> BTreeMap<EffectName, Signature> {
    let mut effects = machine::effects();
    for name in ["processes.start", "processes.await"] {
        effects.insert(
            EffectName::new(name).expect("an effect's name"),
            Signature {
                params: vec![Param {
                    name: Name::new("input"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
    }
    effects
}

fn lower(source: &str) -> Result<lash_kernel_dialect::Lowered, crate::Diagnostic> {
    super::lower_against(source, &[], &effects())
}

const REVIEW: &str = r#"
const review = async (request: string, limit?: number): Promise<string> => {
  const got = await echo(request);
  return got;
};
const handle = await processes.start({ definition: review, args: { request: "x" } });
"#;

#[test]
fn a_named_process_arrow_is_an_entry_under_its_typed_signature_and_its_value_is_a_reference() {
    let document = lower(REVIEW).expect("the cell lowers").document;
    let review = Name::new("review");
    assert_eq!(
        document.entries.get(&review),
        Some(&Signature {
            params: vec![
                Param {
                    name: Name::new("request"),
                    ty: Type::Text,
                    optional: false,
                },
                Param {
                    name: Name::new("limit"),
                    ty: Type::Union(vec![Type::Number, Type::Null, Type::Absent]),
                    optional: true,
                },
            ],
            result: Type::Text,
        })
    );
    assert_eq!(
        document
            .functions
            .get(&review)
            .map(|function| &function.params),
        Some(&vec![Name::new("request"), Name::new("limit")])
    );
    let text = print_document(&document);
    assert!(
        text.contains("&review"),
        "the cell holds the reference:\n{text}"
    );
}

#[test]
fn an_async_arrow_is_a_function_of_the_cell_where_no_process_effect_is_offered() {
    let document = super::lower_with_effects(
        "const twice = async (x: string) => { return await echo(x); };\nconst got = await twice(\"a\");",
    )
    .expect("the cell lowers")
    .document;
    assert!(document.entries.is_empty());
    assert!(document.functions.is_empty());
}

#[test]
fn a_process_written_as_an_objects_definition_is_an_entry_of_its_own() {
    let document = lower(
        "const handle = await processes.start({ definition: async (n: number) => { return n; }, args: { n: 1 } });",
    )
    .expect("the cell lowers")
    .document;
    let [(entry, signature)] = document.entries.iter().collect::<Vec<_>>()[..] else {
        panic!("one entry: {:?}", document.entries);
    };
    assert_eq!(signature.params[0].ty, Type::Number);
    assert!(print_document(&document).contains(&format!("&{entry}")));
}

#[test]
fn an_entry_runs_the_arrows_body_with_its_start_arguments() {
    let document = lower(REVIEW).expect("the cell lowers").document;
    let (asked, ended) = machine::end_of_entry(
        document,
        "review",
        vec![Datum::Text("held".to_owned()), Datum::Absent],
    );
    assert_eq!(asked, ["echo:held"]);
    assert_eq!(ended, Datum::Text("held".to_owned()));
}

#[test]
fn a_process_that_returns_nothing_ends_with_null() {
    let document = lower("const quiet = async () => { await echo(\"once\"); };")
        .expect("the cell lowers")
        .document;
    let (asked, ended) = machine::end_of_entry(document, "quiet", Vec::new());
    assert_eq!(asked, ["echo:once"]);
    assert_eq!(ended, Datum::Null);
}

#[test]
fn a_const_in_a_block_of_the_cell_names_a_process_too() {
    let document = lower("{ const inner = async () => 11; }")
        .expect("the cell lowers")
        .document;
    assert!(document.entries.contains_key(&Name::new("inner")));
}

#[test]
fn a_process_body_that_reads_a_binding_of_the_cell_is_refused() {
    let refused = lower("const limit = 3;\nconst worker = async () => { return limit; };")
        .expect_err("the body reads the cell's `limit`");
    assert_eq!(refused.code, DiagnosticCode::NonLiftableCapture);
    let refused = super::lower_against(
        "const worker = async () => { carried = 1; };",
        &["carried"],
        &effects(),
    )
    .expect_err("the body writes the session's `carried`");
    assert_eq!(refused.code, DiagnosticCode::NonLiftableCapture);
}

#[test]
fn a_process_signature_outside_the_durable_types_is_refused() {
    let refused = lower("const worker = async (run: () => void) => { return 1; };")
        .expect_err("a function is not a start argument");
    assert_eq!(refused.code, DiagnosticCode::ProcessParamTypeUnsupported);
    let refused = lower("const worker = async (): Promise<() => void> => { return () => {}; };")
        .expect_err("a function is not a process result");
    assert_eq!(refused.code, DiagnosticCode::ProcessReturnTypeUnsupported);
    let refused = lower("const worker = async ({ a }: { a: string }) => { return a; };")
        .expect_err("start arguments are keyed by parameter names");
    assert_eq!(refused.code, DiagnosticCode::ProcessParamTypeUnsupported);
}

#[test]
fn awaiting_a_value_waits_for_the_process_a_handle_names() {
    let document = lower(
        "const handle = await processes.start({ definition: async () => 1 });\nconst ended = await handle;",
    )
    .expect("the cell lowers")
    .document;
    let text = print_document(&document);
    assert!(
        text.contains("perform processes.await("),
        "`await handle` waits as `processes.await` does:\n{text}"
    );
    // Where no process effect is offered, an await is an await.
    let plain = super::lower_with_effects("const got = await Promise.resolve(3);")
        .expect("the cell lowers")
        .document;
    assert!(!print_document(&plain).contains("processes.await"));
}
