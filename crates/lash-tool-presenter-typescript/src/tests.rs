use super::*;
use lash_vocabulary::tool_contract::ToolDefinition;
use serde_json::json;

#[test]
fn recursive_filter_has_one_named_body_and_a_recursive_array() {
    let tool = ToolDefinition::raw(
        "filter",
        "host.filter",
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
fn definition(name: &str, input: Value) -> lash_vocabulary::tool_contract::ToolDefinition {
    ToolDefinition::raw(
        name,
        format!("host.{name}"),
        "Select records.",
        input,
        json!({"type":"string"}),
    )
    .expect("valid tool")
    .with_execution(std::time::Duration::from_secs(120))
}

fn document(tool: &lash_vocabulary::tool_contract::ToolDefinition) -> String {
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
    let paths = tools
        .iter()
        .map(|tool| tool.operation().name.to_string())
        .collect::<Vec<_>>();
    let entries = tools
        .iter()
        .zip(&contracts)
        .zip(&paths)
        .map(|((tool, contract), call_path)| ToolPresentationEntry {
            manifest: &tool.manifest,
            contract,
            call_path,
            pinned: false,
        })
        .collect::<Vec<_>>();
    let section = TypescriptToolPresenter
        .present(ToolPresentationRequest {
            reserved_names: &[],
            max_section_bytes: None,
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
    assert!(text.contains("- `input.nullable` — Text branch."), "{text}");
    assert!(
        text.contains("- `input.nullable|null` — Null branch."),
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

/// FIG-6495: shared definition bodies and notes are charged at first use only.
#[test]
fn catalogue_budget_charges_a_shared_closure_once() {
    let schema = json!({"$defs":{"Field":{"enum":["title","body"],"description":"Indexed field."}},
        "type":"object","properties":{"field":{"$ref":"#/$defs/Field"}},
        "required":["field"],"additionalProperties":false});
    let tools = [
        definition("first", schema.clone()),
        definition("second", schema),
    ];
    let contracts = tools.iter().map(|tool| tool.contract()).collect::<Vec<_>>();
    let paths = tools
        .iter()
        .map(|tool| tool.operation().name.to_string())
        .collect::<Vec<_>>();
    let entries = tools
        .iter()
        .zip(&contracts)
        .zip(&paths)
        .map(|((tool, contract), call_path)| ToolPresentationEntry {
            manifest: &tool.manifest,
            contract,
            call_path,
            pinned: false,
        })
        .collect::<Vec<_>>();
    let config = TypescriptToolPresenter
        .admit_config(&json!({}))
        .expect("config");
    let full = TypescriptToolPresenter
        .present(ToolPresentationRequest {
            reserved_names: &[],
            max_section_bytes: None,
            tools: &entries,
            config: &config,
            disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
        })
        .expect("complete listing");
    let budget = full.text.len() - 2; // Catalogue bytes exclude the inter-tool separator.
    let limited = TypescriptToolPresenter
        .present(ToolPresentationRequest {
            reserved_names: &[],
            max_section_bytes: None,
            tools: &entries,
            config: &config,
            disclosure: lash_tool_presenter_api::ToolDisclosure::Budget {
                catalogue_bytes: budget,
            },
        })
        .expect("budget listing");
    assert_eq!(limited.listed.len(), 2, "{}", limited.text);
    assert_eq!(limited.text, full.text);
}

// FIG-6697: the catalog's annotations do not change its accepted value type.
#[test]
fn catalog_annotation_keywords_do_not_refuse_tools() {
    for (keyword, value) in [
        ("example", json!("hello")),
        ("human_parameter_name", json!("Message")),
        ("human_parameter_description", json!("Shown to people")),
        ("file_uploadable", json!(true)),
        ("file_downloadable", json!(true)),
        ("discriminator", json!({"propertyName":"kind"})),
        ("inventedKeyword", json!({"if":false})),
    ] {
        let schema = json!({"type":"object", "properties":{
            "message":{"type":"string", keyword:value}
        }});
        let doc = SchemaDocument::from_json_schema(&schema, 16).expect(keyword);
        assert_eq!(
            render_schema_shape(&doc.roots[0]).unwrap(),
            "{ message?: string }"
        );
    }
}

fn schema_text(schema: &Value) -> String {
    let doc = SchemaDocument::from_json_schema(schema, 32).expect("presentable schema");
    let mut pool = TypescriptDefinitions::new([] as [&str; 0]);
    pool.reserve_document(&doc);
    let prepared = pool.prepare(&doc).unwrap();
    let mut text = prepared.declarations;
    text.push(definitions::root_expression(&doc, &prepared.names).unwrap());
    text.extend(note_rows(&doc.roots[0], "input"));
    text.join("\n")
}

#[test]
fn one_of_sibling_properties_and_required_apply_to_every_arm_without_expansion() {
    let schema = json!({"type":"object", "properties":{
        "query":{"type":"string"}, "cursor":{"type":"string"},
        "limit":{"type":"integer", "minimum":1}
    }, "required":["limit"], "additionalProperties":false,
    "oneOf":[{"required":["query"]},{"required":["cursor"]}]});
    let text = schema_text(&schema);
    assert!(text.contains("limit: number"), "{text}");
    assert!(text.contains("query: unknown"), "{text}");
    assert!(text.contains("cursor: unknown"), "{text}");
    assert_eq!(text.matches("limit: number").count(), 1, "{text}");
    assert!(text.contains(" & ("), "{text}");
    assert!(text.contains("input.limit` (>= 1)"), "{text}");
    assert!(text.len() <= schema.to_string().len(), "{text}");
}

#[test]
fn all_of_sibling_properties_and_required_stay_conjunctive() {
    let schema = json!({"properties":{"query":{"type":"string"}},
        "required":["query"], "allOf":[{"properties":{"limit":{"type":"integer"}}},
        {"required":["limit"]}]});
    let text = schema_text(&schema);
    assert!(text.contains("query: string"), "{text}");
    assert!(text.contains("limit?: number"), "{text}");
    assert!(text.contains("limit: unknown"), "{text}");
    assert_eq!(text.matches("query: string").count(), 1, "{text}");
    assert!(text.contains(" & "), "{text}");
}

#[test]
fn composition_sibling_narrowing_is_a_shared_note() {
    for composition in ["oneOf", "allOf"] {
        let schema = json!({composition:[{"type":"string"}], "minLength":3,
            "pattern":"^abc", "not":{"const":"abcdef"}});
        let text = schema_text(&schema);
        assert!(text.starts_with("string\n"), "{text}");
        assert!(
            text.contains("input` (min length 3, pattern ^abc, not {\"const\":\"abcdef\"})"),
            "{text}"
        );
        let text =
            schema_text(&json!({composition:[{"type":"string", "minLength":5}], "minLength":3}));
        assert!(
            text.contains("min length 3") && text.contains("min length 5"),
            "{text}"
        );
    }
}

#[test]
fn union_notes_name_titles_discriminants_types_and_readable_unnamed_arms() {
    let schema = json!({"oneOf":[
        {"title":"Calendar", "properties":{"date":{"type":"string", "description":"Day"}}},
        {"properties":{"kind":{"const":"mail"}, "date":{"type":"string", "description":"Sent"}}},
        {"type":"string", "description":"Text"},
        {"properties":{"date":{"type":"string", "description":"Other"}}},
        {"properties":{"date":{"type":"string", "description":"Last"}}}
    ]});
    let text = schema_text(&schema);
    for path in [
        "input|Calendar.date",
        "input|kind=\"mail\".date",
        "input|string",
        "input|object.date",
        "input|object (alternative 2).date",
    ] {
        assert!(text.contains(path), "missing {path}: {text}");
    }
    assert!(!text.contains("|0") && !text.contains("|1"), "{text}");
}

fn shape_text(schema: &Value) -> String {
    schema_text(schema)
}
fn document_text(doc: &SchemaDocument) -> String {
    let mut pool = TypescriptDefinitions::default();
    pool.reserve_document(doc);
    let prepared = pool.prepare(doc).unwrap();
    let mut text = prepared.declarations;
    text.push(definitions::root_expression(doc, &prepared.names).unwrap());
    text.extend(note_rows(&doc.roots[0], "input"));
    text.join("\n")
}

fn conditional_fixture() -> Value {
    json!({"type":"object", "properties":{"kind":{"type":"string"}, "url":{"type":"string"}},
        "if":{"properties":{"kind":{"const":"web"}}, "required":["kind"]},
        "then":{"required":["url"]}, "else":{"not":{"required":["url"]}},
        "dependentRequired":{"url":["kind"]},
        "dependentSchemas":{"url":{"required":["kind"]}}})
}

#[test]
fn conditional_and_dependent_rules_are_presented_in_words() {
    let text = shape_text(&conditional_fixture());
    assert!(
        text.contains("url: required when kind is \"web\""),
        "{text}"
    );
    assert!(
        text.contains("kind: required when url is present"),
        "{text}"
    );
}

#[test]
fn inexpressible_schema_rules_keep_an_honest_type_and_a_note() {
    for schema in [
        json!({"$ref":"https://example.com/value.json", "description":"Host description"}),
        json!({"$ref":"#/missing"}),
        json!({"$id":"https://example.com/root", "$defs":{
            "Nested":{"$id":"nested", "$defs":{"Value":{"type":"integer"}},
                "type":"object", "properties":{"v":{"$ref":"#/$defs/Value"}}},
            "Value":{"type":"string"}}, "$ref":"#/$defs/Nested"}),
        json!({"$dynamicRef":"#node"}),
        json!({"$recursiveRef":"#"}),
        json!({"type":"object", "unevaluatedProperties":false}),
        json!({"type":"array", "prefixItems":[{"type":"string"}], "items":{"type":"number"}}),
        json!({"type":"object", "patternProperties":{"^a":{"type":"string"}}}),
        json!({"type":"array", "contains":{"type":"integer"}}),
        json!({"enum":[{"key":1}, 1.5]}),
    ] {
        let text = shape_text(&schema);
        assert!(!text.is_empty(), "{schema}");
        assert!(
            text.contains("validated")
                || text.contains("patternProperties")
                || text.contains("contains")
                || text.contains("one of"),
            "{schema}: {text}"
        );
    }
}

#[test]
fn presentation_limits_keep_a_note_instead_of_refusing_a_valid_tool() {
    let mut schema = json!({"type":"string"});
    for _ in 0..10 {
        schema = json!({"type":"array", "items":schema});
    }
    let doc = lash_kernel_value::tool_shape::SchemaDocument::import(
        &schema,
        lash_kernel_value::tool_shape::ImportLimits {
            max_depth: 2,
            max_nodes: 2,
        },
    )
    .expect("valid tool");
    let text = document_text(&doc);
    assert!(text.contains("validated"), "{text}");
}

#[test]
fn original_figments_schemas_print_no_larger_than_source() {
    for (name, source) in [
        (
            "people.lookup",
            include_str!("fixtures/figments/people.lookup.json"),
        ),
        (
            "information.search",
            include_str!("fixtures/figments/information.search.json"),
        ),
        (
            "information.read",
            include_str!("fixtures/figments/information.read.json"),
        ),
        (
            "information.list",
            include_str!("fixtures/figments/information.list.json"),
        ),
    ] {
        let schemas: Value = serde_json::from_str(source).unwrap();
        for direction in ["input", "output"] {
            let schema = &schemas[direction];
            let text = schema_text(schema);
            assert!(
                text.len() <= schema.to_string().len(),
                "{name} {direction}: {} > {}: {text}",
                text.len(),
                schema.to_string().len()
            );
            println!(
                "{name} {direction}: {} printed <= {} source bytes",
                text.len(),
                schema.to_string().len()
            );
        }
    }
}

/// FIG-6807: an optional value's notes sit on its field path; only a real
/// union names its arms.
#[test]
fn a_nullable_field_is_noted_on_its_own_path_and_a_real_union_names_its_arms() {
    let text = document(&definition(
        "select",
        json!({"type":"object", "properties":{
            "endCursor":{"anyOf":[{"type":"string","title":"End Cursor","description":"Cursor of the last item."},{"type":"null"}]},
            "assignee":{"anyOf":[{"type":"object","title":"Assignee","description":"Who owns the issue.",
                "properties":{"name":{"type":"string","description":"Display name."}},"required":["name"]},{"type":"null"}]},
            "target":{"anyOf":[{"type":"string","title":"Url","description":"A link."},{"type":"integer","title":"Id","description":"An identifier."}]}
        }, "required":["endCursor","assignee","target"], "additionalProperties":false}),
    ));
    for row in [
        "- `input.endCursor` — Cursor of the last item.",
        "- `input.assignee` — Who owns the issue.",
        "- `input.assignee.name` — Display name.",
        "- `input.target|Url` — A link.",
        "- `input.target|Id` — An identifier.",
    ] {
        assert!(text.contains(row), "{row}\n{text}");
    }
    assert!(
        !text.contains("|End Cursor") && !text.contains("|Assignee"),
        "{text}"
    );
}
