//! Laws of the catalogue a code-mode prompt lists (FIG-1435): a discovery
//! session lists what fits its recorded budget, fairly across namespaces,
//! and labels the listing complete or partial; a member appended to the
//! catalogue leaves every byte listed before it as it was; and a tool a
//! host's discovery finds carries its field notes.

use std::sync::Arc;

use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::json;

use super::{
    ToolListing, discovered_tool_doc, discovery_text, rlm_prompt_tool_docs,
    tool_catalogue_disclosure,
};
use crate::dialect::typescript_test_dialect;
use crate::plugin::RlmDiscovery;

fn tool(namespace: &str, operation: &str, module: Option<&str>) -> lash_core::ToolDefinition {
    let mut tool = lash_core::ToolDefinition::raw(
        format!("tool:{namespace}/{operation}"),
        format!("{namespace}_{operation}"),
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
    .with_tool_binding(ToolBinding::new([namespace], operation));
    tool.manifest.module = module.map(|name| {
        Arc::new(lash_core::ToolModule {
            name: name.to_string(),
        })
    });
    tool
}

fn catalog(tools: Vec<lash_core::ToolDefinition>) -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn discovery(catalogue_bytes: usize) -> RlmDiscovery {
    RlmDiscovery {
        operation: "tools.search".to_string(),
        catalogue_bytes,
    }
}

/// The bytes one listed doc takes, as the budget counts it.
fn doc_bytes(tool: &lash_core::ToolDefinition) -> usize {
    rlm_prompt_tool_docs(
        &catalog(vec![tool.clone()]),
        &typescript_test_dialect(),
        Default::default(),
    )
    .len()
}

fn declarations(catalog: &lash_core::ToolCatalog, discovery: Option<&RlmDiscovery>) -> String {
    typescript_test_dialect()
        .execution_section(
            Default::default(),
            catalog,
            crate::plugin::RlmChannel::Cell,
            discovery,
        )
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
    let search = tool("tools", "search", None);
    let big = ["a", "b", "c", "d"].map(|operation| tool("big", operation, None));
    let small = tool("small", "only", None);
    let mut members = vec![search.clone()];
    members.extend(big.iter().cloned());
    members.push(small.clone());
    let members = catalog(members);
    // Room for the discovery tool and three more docs.
    let budget = doc_bytes(&search) + doc_bytes(&big[0]) + doc_bytes(&big[1]) + doc_bytes(&small);

    let disclosure = tool_catalogue_disclosure(
        &crate::dialect::CellDialect::typescript(),
        crate::RlmPresentationConfig::standard(),
        Default::default(),
        &members,
        &discovery(budget),
    );
    assert_eq!(
        disclosure.listed,
        ["tools_search", "big_a", "big_b", "small_only"]
    );
    assert_eq!(disclosure.omitted, ["big_c", "big_d"]);
    assert!(!disclosure.is_complete());

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
    );
    assert_eq!(
        none.disclosure.expect("a discovery listing").listed,
        ["tools_search"]
    );
}

/// A catalogue that fits says so.
#[test]
fn a_catalogue_that_fits_its_budget_is_labelled_complete() {
    let members = catalog(vec![
        tool("tools", "search", None),
        tool("files", "grep", None),
    ]);
    let rendered = declarations(&members, Some(&discovery(usize::MAX)));
    assert!(
        rendered.ends_with("files.grep({ query: string }): Promise<string>`\nThe files grep operation.\n\nCatalogue: COMPLETE. All 2 tools are listed above."),
        "{rendered}"
    );
}

/// A tool appended to the catalogue appends its doc: the declarations
/// rendered before it are, byte for byte, the start of those rendered after
/// it, in a module that sorts before the others and with or without
/// discovery, so a prompt cache keeps the listing's prefix across the
/// change. The label follows the docs.
#[test]
fn an_appended_tool_leaves_every_listed_byte_before_it_unchanged() {
    let before = vec![
        tool("files", "grep", None),
        tool("tracker", "search", Some("tracker")),
        tool("tracker", "open", Some("tracker")),
        tool("tools", "search", None),
    ];
    let mut after = before.clone();
    after.push(tool("alpha", "fetch", Some("alpha")));
    let (before, after) = (catalog(before), catalog(after));

    let (old, new) = (declarations(&before, None), declarations(&after, None));
    assert!(new.starts_with(&old), "{old}\n---\n{new}");
    assert!(new.ends_with("#### alpha\n\n`alpha.fetch({ query: string }): Promise<string>`\nThe alpha fetch operation."), "{new}");
    // A tool of no module after a module's tools is not read as one of
    // them.
    assert!(
        old.contains("#### Ungrouped tools\n\n`tools.search("),
        "{old}"
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
    let mut search = lash_core::ToolDefinition::raw(
        "tool:tracker/issues_search",
        "tracker_issues_search",
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
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(ToolBinding::new(["tracker"], "issues_search"));
    let found = discovered_tool_doc(
        &crate::dialect::CellDialect::typescript(),
        crate::RlmPresentationConfig::standard(),
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
        found.contains("- `sort[].field: string` — The issue field to order by."),
        "{found}"
    );
    let text = discovery_text(crate::RlmPresentationConfig::standard(), &search);
    assert!(
        text.contains("sort[].field: The issue field to order by."),
        "{text}"
    );
    assert!(text.contains("Search issues."), "{text}");

    search.manifest.bindings.clear();
    assert!(
        discovered_tool_doc(
            &crate::dialect::CellDialect::typescript(),
            crate::RlmPresentationConfig::standard(),
            &search,
        )
        .is_err()
    );
}
