use super::*;
use lash_sansio::ToolDefinition;
use serde_json::json;

#[test]
fn recursive_filter_has_one_named_body_and_a_recursive_array() {
    let tool = ToolDefinition::raw(
        "filter",
        "filter",
        "Filter records.",
        json!({
            "$defs": {"Filter": {
                "type":"object",
                "properties": {"and":{"type":"array","items":{"$ref":"#/$defs/Filter"}}},
                "required":["and"], "additionalProperties":false
            }},
            "type":"object", "properties":{"filter":{"$ref":"#/$defs/Filter"}},
            "required":["filter"], "additionalProperties":false
        }),
        json!({"type":"boolean"}),
    )
    .expect("admitted recursive filter")
    .with_execution(std::time::Duration::from_secs(120));
    let contract = tool.contract();
    let text = TypescriptToolPresenter
        .tool_doc(
            &ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &contract,
                call_path: "tools.filter",
                pinned: false,
            },
            &TypescriptToolPresenter
                .admit_config(&json!({}))
                .expect("default config"),
        )
        .expect("recursive contract is presentable");
    assert_eq!(text.matches("type Filter = ").count(), 1, "{text}");
    assert!(text.contains("and: Array<Filter>"), "{text}");
    assert!(!text.contains("unknown"), "{text}");
}
fn definition(name: &str, input: Value) -> lash_sansio::ToolDefinition {
    ToolDefinition::raw(
        name,
        name,
        "Select records.",
        input,
        json!({"type":"string"}),
    )
    .expect("valid tool")
    .with_execution(std::time::Duration::from_secs(120))
}

fn document(tool: &lash_sansio::ToolDefinition) -> String {
    let contract = tool.contract();
    TypescriptToolPresenter
        .tool_doc(
            &ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &contract,
                call_path: "tools.select",
                pinned: false,
            },
            &TypescriptToolPresenter
                .admit_config(&json!({}))
                .expect("config"),
        )
        .expect("document")
}

#[test]
fn explicit_shared_enum_is_declared_once_across_six_operands_and_tools() {
    let input = json!({
        "$defs":{"Field":{"enum":["title","body"],"description":"Indexed field."}},
        "type":"object",
        "properties": (0..6).map(|index| (format!("operand{index}"), json!({"$ref":"#/$defs/Field"}))).collect::<serde_json::Map<_,_>>(),
        "additionalProperties":false
    });
    let tools = [
        definition("first", input.clone()),
        definition("second", input),
    ];
    let contracts = tools.iter().map(|tool| tool.contract()).collect::<Vec<_>>();
    let entries = tools
        .iter()
        .zip(&contracts)
        .map(|(tool, contract)| ToolPresentationEntry {
            manifest: &tool.manifest,
            contract,
            call_path: &tool.manifest.name,
            pinned: false,
        })
        .collect::<Vec<_>>();
    let section = TypescriptToolPresenter
        .present(ToolPresentationRequest {
            tools: &entries,
            config: &TypescriptToolPresenter
                .admit_config(&json!({}))
                .expect("config"),
            disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
        })
        .expect("listing");
    assert_eq!(
        section
            .text
            .matches("type Field = \"title\" | \"body\";")
            .count(),
        1,
        "{}",
        section.text
    );
    assert_eq!(
        section.text.matches(": Field").count(),
        12,
        "{}",
        section.text
    );
    assert_eq!(
        section.text.matches("Indexed field.").count(),
        1,
        "{}",
        section.text
    );
    let index = TypescriptToolPresenter
        .discovery_text(
            &tools[0].manifest,
            &contracts[0],
            &TypescriptToolPresenter
                .admit_config(&json!({}))
                .expect("config"),
        )
        .expect("search index");
    assert!(index.contains("operand0"), "{index}");
    assert_eq!(index.matches("Indexed field.").count(), 1, "{index}");
}

#[test]
fn path_only_notes_preserve_constraints_defaults_nullability_and_use_site_ownership() {
    let text = document(&definition(
        "select",
        json!({
            "$defs":{"Field":{"enum":["title","body"],"description":"Indexed field."}},
            "type":"object", "properties":{
                "field":{"$ref":"#/$defs/Field","description":"Choose this field."},
                "limit":{"type":["integer","null"],"minimum":1,"default":10,"description":"Page size."},
                "nullable":{"anyOf":[{"type":"string","description":"Text branch."},{"type":"null","description":"Null branch."}]}
            }, "required":["limit"], "additionalProperties":false
        }),
    ));
    assert!(
        text.contains("limit: number | null; field?: Field; nullable?: string | null"),
        "{text}"
    );
    assert!(text.contains("- `Field` — Indexed field."), "{text}");
    assert!(
        text.contains("- `input.field` — Choose this field."),
        "{text}"
    );
    assert!(
        text.contains("- `input.limit` (>= 1, default 10) — Page size."),
        "{text}"
    );
    assert!(
        text.contains("- `input.nullable|0` — Text branch."),
        "{text}"
    );
    assert!(
        text.contains("- `input.nullable|1` — Null branch."),
        "{text}"
    );
    for row in text.lines().filter(|line| line.starts_with("- ")) {
        assert!(
            !row.contains("\"title\" | \"body\"") && !row.contains("number | null"),
            "{row}"
        );
    }
}

#[test]
fn dependency_groups_and_recursive_components_have_stable_order() {
    let text = document(&definition(
        "select",
        json!({
            "$defs": {
                "Filter":{"type":"object","properties":{"next":{"$ref":"#/$defs/Comparison"},"field":{"$ref":"#/$defs/Field"}}},
                "Comparison":{"type":"object","properties":{"filter":{"$ref":"#/$defs/Filter"}}},
                "Field":{"enum":["name","age"]}
            },
            "type":"object","properties":{"filter":{"$ref":"#/$defs/Filter"}}
        }),
    ));
    let field = text.find("type Field = ").expect("dependency");
    let comparison = text.find("type Comparison = ").expect("first SCC member");
    let filter = text.find("type Filter = ").expect("second SCC member");
    assert!(field < comparison && comparison < filter, "{text}");
    assert!(
        text.contains("next?: Comparison") && text.contains("filter?: Filter"),
        "{text}"
    );
    assert!(!text.contains("unknown"), "{text}");
}

#[test]
fn readable_names_reserve_keywords_host_names_and_later_distinct_closures() {
    let first = SchemaDocument::from_json_schema(
        &json!({"$defs":{"Field":{"type":"string"}},"$ref":"#/$defs/Field"}),
        8,
    )
    .expect("graph");
    let second = SchemaDocument::from_json_schema(
        &json!({"$defs":{"Field":{"type":"integer"}},"$ref":"#/$defs/Field"}),
        8,
    )
    .expect("graph");
    let keyword = SchemaDocument::from_json_schema(
        &json!({"$defs":{"class":{"type":"boolean"}},"$ref":"#/$defs/class"}),
        8,
    )
    .expect("graph");
    let mut pool = TypescriptDefinitions::new(["Field"]);
    let first = pool.prepare(&first).expect("first");
    let second = pool.prepare(&second).expect("second");
    let keyword = pool.prepare(&keyword).expect("keyword");
    assert_eq!(first.declarations, ["type Field_2 = string;"]);
    assert_eq!(second.declarations, ["type Field_3 = number;"]);
    assert_eq!(keyword.declarations, ["type class_2 = boolean;"]);
    let history = SchemaDocument::from_json_schema(
        &json!({"$defs":{"HistoryItem":{"type":"string"}},"$ref":"#/$defs/HistoryItem"}),
        8,
    )
    .expect("graph");
    assert_eq!(
        pool.prepare(&history)
            .expect("history name reservation")
            .declarations,
        ["type HistoryItem_2 = string;"]
    );
    assert!(
        render_schema_shape(
            &lash_tool_shape::ShapeKind::Ref(lash_tool_shape::DefinitionId(1)).into()
        )
        .is_err()
    );
}

#[test]
fn a_referenced_document_root_emits_its_body_and_notes_once() {
    let text = document(&definition(
        "select",
        json!({
            "title":"Filter", "type":"object", "description":"Root filter.",
            "properties":{"and":{"type":"array","items":{"$ref":"#"}}},
            "required":["and"],"additionalProperties":false
        }),
    ));
    assert_eq!(text.matches("{ and: Array<Filter> }").count(), 1, "{text}");
    assert_eq!(text.matches("Root filter.").count(), 1, "{text}");
    assert!(text.contains("tools.select(Filter)"), "{text}");
}
