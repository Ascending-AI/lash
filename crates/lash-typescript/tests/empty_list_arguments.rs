//! An empty list literal must reach a typed-list parameter.
//!
//! `[]` is the most natural spelling of "no items", and hosts document it as
//! such: the jitindex research loop's `run` tool shipped `edits: []` as its
//! worked example, and agents copying that example verbatim were rejected in
//! five of eight live episodes. The linker types `[]` as the empty-list
//! sentinel `list[null]`, which used to fail against `list[dict] | null`
//! (FIG-1421). A populated list still has to type-check for real.

use lashlang::{LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, TypeExpr};

/// A host module whose `run` operation takes `{ edits: list[dict] | null }`,
/// the declaration shape from the ticket.
fn edits_environment() -> LashlangHostEnvironment {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["index"],
            "IndexModule",
            "run",
            "tool:index/run",
            TypeExpr::Object(vec![lashlang::TypeField {
                name: "edits".into(),
                ty: TypeExpr::Union(vec![
                    TypeExpr::List(Box::new(TypeExpr::Dict)),
                    TypeExpr::Null,
                ]),
                optional: false,
            }]),
            TypeExpr::Any,
        )
        .expect("index module operation");
    LashlangHostEnvironment::new(catalog, LashlangAbilities::default())
}

#[test]
fn an_empty_list_argument_links_against_a_typed_list_parameter() {
    let environment = edits_environment();

    lash_typescript::link("await index.run({ edits: [] });", &environment)
        .expect("an empty list must reach a `list[dict] | null` parameter");

    let bound = lash_typescript::link(
        "const edits = []; await index.run({ edits: edits });",
        &environment,
    );
    assert!(
        bound.is_ok(),
        "an empty list behind a binding must reach it too: {bound:?}"
    );
}

#[test]
fn a_populated_list_still_has_to_match_the_element_type() {
    let error = lash_typescript::link("await index.run({ edits: [1] });", &edits_environment())
        .expect_err("`[1]` is not a list of dicts and must still be refused");

    assert!(
        error.to_string().contains("edits"),
        "the refusal must name the offending input: {error}"
    );
}
