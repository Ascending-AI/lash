//! Laws of the catalogue a code-mode prompt lists (FIG-1435): a discovery
//! session lists what fits its recorded budget, fairly across namespaces,
//! and labels the listing complete or partial; a member appended to the
//! catalogue leaves every byte listed before it as it was; and a tool a
//! host's discovery finds carries its field notes.

use serde_json::json;

use super::{
    ToolListing, discovered_tool_doc, discovered_tool_signature, discovery_text,
    rlm_prompt_tool_docs, tool_catalogue_disclosure,
};
use crate::dialect::typescript_test_dialect;
use crate::plugin::RlmDiscovery;

fn tool(namespace: &str, operation: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{namespace}/{operation}"),
        format!("{namespace}.{operation}"),
        format!("The {namespace} {operation} operation."),
        json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

fn operation(text: &str) -> lash_vocabulary::names::QualifiedOperation {
    lash_vocabulary::tool_contract::IntoOperation::into_operation(text)
        .expect("a canonical operation")
}

fn catalog(tools: Vec<lash_vocabulary::tool_contract::ToolDefinition>) -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn discovery(catalogue_bytes: usize) -> RlmDiscovery {
    RlmDiscovery {
        operation: "tools.search".to_string(),
        catalogue_bytes,
    }
}

/// The bytes one listed doc takes, as the budget counts it.
fn doc_bytes(tool: &lash_vocabulary::tool_contract::ToolDefinition) -> usize {
    rlm_prompt_tool_docs(
        &catalog(vec![tool.clone()]),
        &typescript_test_dialect(),
        Default::default(),
    )
    .len()
}

fn declarations(catalog: &lash_core::ToolCatalog, discovery: Option<&RlmDiscovery>) -> String {
    typescript_test_dialect()
        .execution_section(Default::default(), catalog, discovery)
        .expect("validated catalog")
        .declarations
}

/// A partial catalogue takes the namespaces in turn, so a namespace of one
/// tool is listed beside a namespace of many: the discovery tool first,
/// then each namespace's next tool while it fits the budget. What does not
/// fit is counted, and the label names how many tools the listing leaves
/// out and the call that finds them.
#[test]
fn a_partial_catalogue_lists_namespaces_in_turn_and_counts_what_it_leaves_out() {
    let search = tool("tools", "search");
    let big = ["a", "b", "c", "d"].map(|operation| tool("big", operation));
    let small = tool("small", "only");
    let mut members = vec![search.clone()];
    members.extend(big.iter().cloned());
    members.push(small.clone());
    let members = catalog(members);
    // Room for the discovery tool and three more docs.
    let budget = doc_bytes(&search) + doc_bytes(&big[0]) + doc_bytes(&big[1]) + doc_bytes(&small);

    let disclosure = tool_catalogue_disclosure(
        &crate::dialect::CellDialect::typescript(),
        &json!({}),
        Default::default(),
        &members,
        &discovery(budget),
    )
    .expect("the catalogue presents");
    assert_eq!(
        disclosure.listed,
        ["tools.search", "big.a", "big.b", "small.only"]
    );
    assert_eq!(disclosure.omitted, ["big.c", "big.d"]);
    assert!(!disclosure.is_complete());

    let selected = ToolListing::of(
        &members,
        &typescript_test_dialect(),
        Default::default(),
        Some(&discovery(budget)),
    )
    .expect("budgeted listing");
    assert_eq!(
        selected.docs.len(),
        budget + 6,
        "only the three blank separators are outside the catalogue budget"
    );
    let rendered = declarations(&members, Some(&discovery(budget)));
    insta::assert_snapshot!(rendered, @r"
    ### Tools

    `tools.search({ query: string }): Promise<string>`
    The tools search operation.

    `big.a({ query: string }): Promise<string>`
    The big a operation.

    `big.b({ query: string }): Promise<string>`
    The big b operation.

    `small.only({ query: string }): Promise<string>`
    The small only operation.

    Catalogue: PARTIAL. 4 of 6 tools are listed above; find the other 2 with `await tools.search({ ... })`.
    ");
    assert!(rendered.contains("small.only("), "{rendered}");
    assert!(!rendered.contains("big.c("), "{rendered}");
    assert!(
        rendered.ends_with(
            "Catalogue: PARTIAL. 4 of 6 tools are listed above; find the other 2 with `await tools.search({ ... })`."
        ),
        "{rendered}"
    );
    // The listing is the budget's: the discovery tool always, and nothing
    // the budget has no room for.
    let none = ToolListing::of(
        &members,
        &typescript_test_dialect(),
        Default::default(),
        Some(&discovery(0)),
    )
    .expect("the catalogue presents");
    assert_eq!(
        none.disclosure.expect("a discovery listing").listed,
        ["tools.search"]
    );
}

/// A catalogue that fits says so.
#[test]
fn a_catalogue_that_fits_its_budget_is_labelled_complete() {
    let members = catalog(vec![tool("tools", "search"), tool("files", "grep")]);
    let complete = ToolListing::of(
        &members,
        &typescript_test_dialect(),
        Default::default(),
        Some(&discovery(usize::MAX)),
    )
    .expect("complete budget");
    assert_eq!(
        complete.docs,
        rlm_prompt_tool_docs(&members, &typescript_test_dialect(), Default::default())
    );
    let rendered = declarations(&members, Some(&discovery(usize::MAX)));
    assert!(
        rendered.ends_with("files.grep({ query: string }): Promise<string>`\nThe files grep operation.\n\nCatalogue: COMPLETE. All 2 tools are listed above."),
        "{rendered}"
    );
}

/// A tool appended to the catalogue appends its doc: the declarations
/// rendered before it are, byte for byte, the start of those rendered after
/// it, in a namespace that sorts before the others and with or without
/// discovery, so a prompt cache keeps the listing's prefix across the
/// change. The label follows the docs.
#[test]
fn an_appended_tool_leaves_every_listed_byte_before_it_unchanged() {
    let before = vec![
        tool("files", "grep"),
        tool("tracker", "search"),
        tool("tracker", "open"),
        tool("tools", "search"),
    ];
    let mut after = before.clone();
    after.push(tool("alpha", "fetch"));
    let (before, after) = (catalog(before), catalog(after));
    let listing = |catalog| {
        ToolListing::of(
            catalog,
            &typescript_test_dialect(),
            Default::default(),
            None,
        )
        .expect("complete catalogue")
        .docs
    };
    assert!(listing(&after).starts_with(&listing(&before)));

    let (old, new) = (declarations(&before, None), declarations(&after, None));
    assert!(new.starts_with(&old), "{old}\n---\n{new}");
    assert!(
        new.ends_with(
            "`alpha.fetch({ query: string }): Promise<string>`\nThe alpha fetch operation."
        ),
        "{new}"
    );

    let discovering = discovery(usize::MAX);
    let docs = |rendered: String| {
        rendered
            .split_once("\n\nCatalogue: ")
            .expect("a labelled listing")
            .0
            .to_string()
    };
    let (old, new) = (
        docs(declarations(&before, Some(&discovering))),
        docs(declarations(&after, Some(&discovering))),
    );
    assert!(new.starts_with(&old), "{old}\n---\n{new}");
    assert_ne!(old, new);
}

/// A tool a host's discovery finds is shown the doc a listed tool has, its
/// field notes included where its signature collapses a nested field, and
/// its field descriptions are in the text a search matches.
#[test]
fn a_discovered_tool_carries_its_field_notes() {
    let search = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "tool:tracker/issues_search",
        "tracker.issues_search",
        "Search issues.",
        json!({
            "type": "object",
            "properties": {
                "sort": {
                    "type": "array",
                    "description": "Sort keys, most significant first.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "field": { "type": "string", "description": "The issue field to order by." }
                        },
                        "required": ["field"]
                    }
                }
            }
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    let found = discovered_tool_doc(
        &crate::dialect::CellDialect::typescript(),
        &json!({}),
        &search,
    )
    .expect("an addressable tool");
    assert_eq!(
        found,
        rlm_prompt_tool_docs(
            &catalog(vec![search.clone()]),
            &typescript_test_dialect(),
            Default::default(),
        )
    );
    assert!(
        found.contains("- `input.sort[].field` — The issue field to order by."),
        "{found}"
    );
    let text = discovery_text(
        &crate::dialect::CellDialect::typescript(),
        &json!({}),
        &search,
    )
    .expect("the presenter writes discovery text");
    assert!(
        text.contains("sort[].field: The issue field to order by."),
        "{text}"
    );
    assert!(text.contains("Search issues."), "{text}");
}

/// FIG-6336: a discovered path and schemas render the loaded tool's signature
/// without fabricating an execution definition, in each dialect.
#[test]
fn a_path_and_schemas_render_the_loaded_tool_signature_in_each_dialect() {
    let tool = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "tool:tracker/search",
        "tracker.issues.search",
        "Search issues.",
        json!({
            "$defs": { "Query": { "type": "string" } },
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": { "query": { "$ref": "#/$defs/Query" } },
                    "required": ["query"],
                    "additionalProperties": false
                }
            },
            "required": ["filter"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "issue": {
                    "type": "object",
                    "properties": { "title": { "type": "string" } },
                    "required": ["title"],
                    "additionalProperties": false
                }
            },
            "required": ["issue"],
            "additionalProperties": false
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    let presentation = json!({});
    for dialect in [
        crate::dialect::CellDialect::typescript(),
        crate::dialect::CellDialect::python(),
    ] {
        let signature = discovered_tool_signature(
            &dialect,
            &presentation,
            &operation("tracker.issues.search"),
            &tool.contract.input_schema.canonical,
            &tool.contract.output_schema.canonical,
        )
        .expect("an addressable tool");
        if dialect.name() == "typescript" {
            assert!(
                signature.starts_with("type Query = string;\n"),
                "{signature}"
            );
        }
        let doc = discovered_tool_doc(&dialect, &presentation, &tool).expect("an addressable tool");
        let mut bundle = signature.lines().collect::<Vec<_>>();
        let signature_line = format!("`{}`", bundle.pop().expect("signature"));
        for definition in &bundle {
            assert!(doc.contains(definition), "{doc}");
        }
        assert!(doc.lines().any(|line| line == signature_line));
        let prompt = crate::dialect::SessionDialect::prompt_only(dialect)
            .render_execution_section(Default::default(), &catalog(vec![tool.clone()]), None)
            .expect("the tool can be listed");
        assert!(
            prompt.lines().any(|line| line == signature_line),
            "{prompt}"
        );
    }
}

/// S1 / FIG-6552: select options ten containers deep keep their exact field
/// types in discovered signatures, whether anonymous or behind a named edge.
#[test]
fn discovered_signatures_preserve_ten_level_select_options_in_every_dialect() {
    use lash_tool_presenter_api::ToolPresenter;

    for named in [false, true] {
        let option = json!({
            "type": "object",
            "properties": {"value": {"type": "string"}, "label": {"type": "string"}},
            "required": ["value", "label"],
            "additionalProperties": false
        });
        let mut input = json!({"type": "array", "items": if named {
            json!({"$ref": "#/$defs/Option"})
        } else {
            option.clone()
        }});
        for _ in 0..9 {
            input = json!({
                "type": "object", "properties": {"child": input},
                "required": ["child"], "additionalProperties": false
            });
        }
        if named {
            input["$defs"] = json!({"Option": option});
        }
        let input = lash_vocabulary::json_schema::JsonSchema::admit(input)
            .expect("valid deep select options");
        let output =
            lash_vocabulary::json_schema::JsonSchema::admit(json!({"type": "string"})).unwrap();
        for (dialect, field_type) in [
            (crate::dialect::CellDialect::typescript(), "string"),
            (crate::dialect::CellDialect::python(), "str"),
        ] {
            let signature = discovered_tool_signature(
                &dialect,
                &json!({}),
                &operation("inbox.send_item"),
                &input,
                &output,
            )
            .expect("a deep schema is spelled completely");
            for field in ["value", "label"] {
                assert!(
                    signature.contains(&format!("{field}: {field_type}"))
                        || signature.contains(&format!("\"{field}\": {field_type}")),
                    "{signature}"
                );
            }
            assert!(!signature.contains("unknown"), "{signature}");
            assert!(!signature.contains("Any"), "{signature}");
        }
        let presenter = lash_tool_presenter_standard::StandardToolPresenter;
        let signature = presenter
            .tool_signature(
                "inbox.send_item",
                &input,
                &output,
                &presenter.admit_config(&json!({})).unwrap(),
            )
            .expect("a deep compact signature is spelled completely");
        for field in ["value", "label"] {
            assert!(signature.contains(&format!("{field}: str")), "{signature}");
        }
        assert!(!signature.contains("any"), "{signature}");
    }
}

/// FIG-6699: discovery preserves the presenter's typed refusal and stable code.
#[test]
fn discovered_signature_refusals_retain_the_presenter_cause() {
    use lash_tool_presenter_api::{PresentationRefusal, ToolPresenter};
    use std::error::Error;

    let config = json!({"unsupported_host_option": true});
    let schema =
        lash_vocabulary::json_schema::JsonSchema::admit(json!({"type": "string"})).unwrap();
    let dialect = crate::dialect::CellDialect::typescript();
    let expected = lash_tool_presenter_typescript::TypescriptToolPresenter
        .admit_config(&config)
        .expect_err("the presenter refuses an unknown config option");
    let refused = discovered_tool_signature(
        &dialect,
        &config,
        &operation("tools.search"),
        &schema,
        &schema,
    )
    .expect_err("discovery carries the config refusal");
    let cause = refused
        .source()
        .and_then(|cause| cause.downcast_ref::<PresentationRefusal>())
        .expect("discovery must retain the typed presenter cause");
    assert_eq!(cause, &expected);
    assert_eq!(cause.code(), "config.invalid");
}
