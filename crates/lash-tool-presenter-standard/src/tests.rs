use super::*;
use lash_sansio::{JsonSchema, ToolDefinition};

/// `tool` compacted under its own name and the standard config.
fn compact(tool: &ToolDefinition) -> CompactToolContract {
    compact_contract(
        &tool.manifest,
        &tool.contract,
        tool.name(),
        &StandardPresenterConfig::standard(),
    )
    .expect("representable tool")
}

#[test]
fn compact_tool_contract_renders_prompt_and_search_shape_from_schemas() {
    let tool = ToolDefinition::raw(
        "tool:search_docs",
        "search_docs",
        "Search indexed docs",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "maximum": 10, "default": 5 }
            },
            "required": ["query"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "matches": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "next_page": { "type": ["string", "null"] }
            },
            "required": ["matches"]
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));

    let contract = compact(&tool);
    assert_eq!(
        contract.signature,
        "search_docs({ query: str, limit?: int })"
    );
    assert_eq!(
        contract.returns,
        "record{matches: list[str], next_page?: str | null}"
    );
    assert_eq!(
        contract
            .parameters
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![serde_json::json!({"path":"input.limit", "notes":["<= 10", "default 5"]})]
    );

    let docs = compact(&tool).render_markdown();
    assert!(docs.contains(
        "### search_docs({ query: str, limit?: int }) -> record{matches: list[str], next_page?: str | null}"
    ));
    assert!(!docs.contains("Returns:"));
    assert!(docs.contains("Parameters:\n- `input.limit` (<= 10, default 5)"));
}

#[test]
fn compact_tool_contract_resolves_local_refs_in_string_or_list_parameters() {
    let tool = ToolDefinition::raw(
        "tool:search_tools",
        "search_tools",
        "Search tools",
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "ModuleFilter": {
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    ]
                }
            },
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "module": {
                    "anyOf": [
                        { "$ref": "#/$defs/ModuleFilter" },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["query"]
        }),
        serde_json::json!({
            "type": "array",
            "items": { "type": "object" }
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));

    let signature = compact(&tool).render_signature();

    assert!(
        signature.contains("module?: ModuleFilter | null"),
        "{signature}"
    );
    assert!(
        signature.contains("type ModuleFilter = str | list[str]"),
        "{signature}"
    );
}

#[test]
fn dynamic_output_contract_renders_schema_from_input_without_return_fields() {
    let tool = ToolDefinition::raw(
        "tool:spawn_agent",
        "spawn_agent",
        "Run a child session",
        serde_json::json!({
            "type": "object",
            "properties": {
                "output": { "type": "object", "additionalProperties": true }
            }
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_output_from_input_schema("output", None);

    let contract = compact(&tool);
    assert_eq!(
        contract.signature,
        "spawn_agent<T = any>({ output?: TypeSpec<T> })"
    );
    assert_eq!(contract.returns, "T");
    assert!(contract.return_fields.is_empty());
    assert_eq!(contract.render_returns(), "");
    assert_eq!(
        compact(&tool).render_markdown(),
        "### spawn_agent<T = any>({ output?: TypeSpec<T> }) -> T\nRun a child session"
    );
}

#[test]
fn dynamic_output_contract_renders_default_schema() {
    let tool = ToolDefinition::raw(
        "tool:llm_query",
        "llm_query",
        "Run a lightweight LLM query",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" },
                "output": { "type": "object", "additionalProperties": true }
            },
            "required": ["task"]
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_output_from_input_schema(
        "output",
        Some(
            JsonSchema::admit(serde_json::json!({ "type": "string" }))
                .expect("valid output default schema"),
        ),
    );

    let contract = compact(&tool);
    assert_eq!(
        contract.signature,
        "llm_query<T = str>({ task: str, output?: TypeSpec<T> })"
    );
    assert_eq!(contract.returns, "T");
    assert!(contract.return_fields.is_empty());
    assert_eq!(contract.render_returns(), "");
}

#[test]
fn json_schema_loaded_contract_merges_nullable_anyof_return_fields() {
    let tool: ToolDefinition = serde_json::from_value(serde_json::json!({
        "manifest": {
            "id": "tool:mcp__appworld__spotify_show_album_library",
            "name": "mcp__appworld__spotify_show_album_library",
            "description": "[MCP appworld] Search or show a list of albums in your album library.",
            "execution": { "secs": 30, "nanos": 0 },
        },
        "contract": {
            "input_schema": {
                "canonical": {
                "type": "object",
                "properties": {
                    "access_token": {
                        "type": "string",
                        "description": "Access token obtained from spotify app login."
                    },
                    "page_index": {
                        "type": "integer",
                        "description": "The index of the page to return.",
                        "minimum": 0,
                        "default": 0
                    },
                    "page_limit": {
                        "type": "integer",
                        "description": "The maximum number of results to return per page.",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 5
                    }
                },
                "required": ["access_token"]
                }
            },
            "output_schema": {
                "canonical": {
                "type": "object",
                "properties": {
                    "response": {
                        "anyOf": [
                            {
                                "type": "array",
                                "description": "Albums in the user's library.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "added_at": {
                                            "description": "When the album was added to the library.",
                                            "anyOf": [
                                                { "type": "string" },
                                                { "type": "null" }
                                            ]
                                        },
                                        "album_id": { "type": "integer" },
                                        "genre": {
                                            "type": "string",
                                            "description": "Album genre.",
                                            "minLength": 1
                                        },
                                        "song_ids": {
                                            "type": "array",
                                            "items": { "type": "integer" }
                                        },
                                        "title": {
                                            "type": "string",
                                            "minLength": 1
                                        }
                                    },
                                    "required": ["added_at", "album_id", "genre", "song_ids", "title"]
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "message": {
                                        "type": "string",
                                        "description": "Failure or status message."
                                    }
                                },
                                "required": ["message"]
                            }
                        ]
                    }
                },
                "required": ["response"]
                }
            }
        }
    }))
    .unwrap();

    let contract = compact(&tool);
    // Branch-qualified paths retain annotations without repeating type text.
    assert_eq!(
        contract
            .parameters
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![
            serde_json::json!({"path":"input.access_token", "description":"Access token obtained from spotify app login."}),
            serde_json::json!({"path":"input.page_index", "notes":[">= 0", "default 0"], "description":"The index of the page to return."}),
            serde_json::json!({"path":"input.page_limit", "notes":[">= 1", "<= 20", "default 5"], "description":"The maximum number of results to return per page."}),
        ]
    );
    assert_eq!(
        contract
            .return_fields
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![
            serde_json::json!({"path":"output.response|0", "description":"Albums in the user's library."}),
            serde_json::json!({"path":"output.response|0[].added_at", "description":"When the album was added to the library."}),
            serde_json::json!({"path":"output.response|0[].genre", "description":"Album genre.", "notes":["min length 1"]}),
            serde_json::json!({"path":"output.response|0[].title", "notes":["min length 1"]}),
            serde_json::json!({"path":"output.response|1.message", "description":"Failure or status message."}),
        ]
    );
    assert!(
        serde_json::from_value::<CompactNoteRow>(
            serde_json::json!({"path":"input.q", "type":"str", "signature":"q: str"})
        )
        .is_err()
    );
    let docs = contract.render_markdown();
    assert!(
        docs.contains("- `output.response|0[].genre` (min length 1) — Album genre."),
        "{docs}"
    );
    assert!(
        !docs.contains("- `output.response|0[].genre: str"),
        "{docs}"
    );
}

/// FIG-4544. The compact contract is the schema docs every non-dialect
/// surface shows: catalog projections, discovery results, the manifest. An
/// MCP-style schema — no `additionalProperties`, nested objects and arrays, a
/// field without a description — keeps every field, its notes, and what the
/// schema says about extra keys.
#[test]
fn compact_contract_renders_an_open_nested_schema_with_full_fidelity() {
    let tool = ToolDefinition::raw(
        "tool:mcp/issues_search",
        "issues_search",
        "Search issues.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1 },
                "filter": {
                    "type": "object",
                    "properties": {
                        "state": { "enum": ["open", "closed"], "description": "Issue state." },
                        "labels": { "type": "array", "items": { "type": "string" }, "maxItems": 5 }
                    },
                    "additionalProperties": { "type": "string" }
                },
                "sort": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "field": { "type": "string" },
                            "descending": { "type": "boolean", "default": false }
                        },
                        "required": ["field"]
                    }
                }
            },
            "required": ["query"],
            "additionalProperties": true
        }),
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": { "id": { "type": "integer", "exclusiveMinimum": 0 } },
                "required": ["id"]
            }
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));

    assert_eq!(
        compact(&tool).render_markdown(),
        concat!(
            "### issues_search({ query: str, ",
            "filter?: record{labels?: list[str], state?: enum[\"open\", \"closed\"], ...: str}, ",
            "sort?: list[record{field: str, descending?: bool}], ... }) -> list[record{id: int}]\n",
            "Search issues.\n",
            "Parameters:\n",
            "- `input.query` (min length 1)\n",
            "- `input.filter.labels` (max items 5)\n",
            "- `input.filter.state` — Issue state.\n",
            "- `input.sort[].descending` (default false)\n",
            "Return fields:\n",
            "- `output[].id` (> 0)"
        )
    );
}

/// FIG-6494: recursive references are finite named edges, never widened types.
#[test]
fn recursive_filter_has_one_named_body_and_path_only_notes() {
    let tool = ToolDefinition::raw(
        "filter",
        "filter",
        "Filter records",
        serde_json::json!({
            "type":"object", "properties":{"filter":{"$ref":"#/$defs/Filter"}},
            "$defs":{"Filter":{"type":"object", "additionalProperties":false,
                "properties":{
                    "and":{"type":"array", "items":{"$ref":"#/$defs/Filter"}},
                    "field":{"type":"string", "description":"Field to compare", "minLength":1}
                }}}
        }),
        serde_json::json!({"$defs":{"Filter":{"enum":["ok","failed"]}}, "$ref":"#/$defs/Filter"}),
    )
    .expect("admitted recursive contract")
    .with_execution(std::time::Duration::from_secs(30));
    let docs = compact(&tool).render_markdown();
    assert_eq!(docs.matches("type Filter =").count(), 1, "{docs}");
    assert!(docs.contains("and?: list[Filter]"), "{docs}");
    assert!(
        docs.contains("type Filter_2 = enum[\"ok\", \"failed\"]"),
        "{docs}"
    );
    assert!(
        docs.contains("- `Filter.field` (min length 1) — Field to compare"),
        "{docs}"
    );
    assert!(!docs.contains("any"), "{docs}");
    assert_eq!(docs.matches("Field to compare").count(), 1, "{docs}");
}

/// S1: a graph edge without an allocated name cannot become `any`.
#[test]
fn s1_compact_helpers_refuse_unbound_references() {
    let document = SchemaDocument::from_json_schema(&serde_json::json!({"$ref":"#"}), 8).unwrap();
    let names = BTreeMap::new();
    assert!(compact_type(&document.roots[0], &names).is_err());
    assert!(compact_definition(&document, DefinitionId(0), &names).is_err());
    assert!(compact_signature("query", &document.roots[0], &names).is_err());
}

/// FIG-6494: sharing, mutual recursion and use-site notes retain their owners.
#[test]
fn reference_closures_preserve_mutual_edges_sharing_and_use_site_notes() {
    let schema = serde_json::json!({
        "$defs":{
            "Left":{"type":"object", "properties":{"right":{"$ref":"#/$defs/Right"}}},
            "Right":{"type":"object", "properties":{"left":{"$ref":"#/$defs/Left"}}},
            "Field":{"enum":["id","title"], "description":"Comparison field"},
            "type":{"type":"string"}, "input":{"type":"integer"}
        },
        "type":"object", "properties":{
            "tree":{"$ref":"#/$defs/Left"},
            "keyword":{"$ref":"#/$defs/type"}, "namespace":{"$ref":"#/$defs/input"},
            "a":{"$ref":"#/$defs/Field", "description":"First operand"},
            "b":{"$ref":"#/$defs/Field", "description":"Second operand"},
            "c":{"$ref":"#/$defs/Field"}, "d":{"$ref":"#/$defs/Field"},
            "e":{"$ref":"#/$defs/Field"}, "f":{"$ref":"#/$defs/Field"},
            "a.b`":{"type":["string","null"], "description":"Quoted key"}
        }
    });
    let tool = ToolDefinition::raw("Field", "Field", "Compare", schema.clone(), schema)
        .unwrap()
        .with_execution(std::time::Duration::from_secs(30));
    let docs = compact(&tool).render_markdown();
    for name in ["Left", "Right", "Field_2", "type_2", "input_2"] {
        assert_eq!(docs.matches(&format!("type {name} =")).count(), 1, "{docs}");
    }
    assert!(docs.contains("right?: Right"), "{docs}");
    assert!(docs.contains("left?: Left"), "{docs}");
    assert_eq!(docs.matches("enum[\"id\", \"title\"]").count(), 1, "{docs}");
    assert_eq!(docs.matches("Comparison field").count(), 1, "{docs}");
    assert!(docs.contains("- `input.a` — First operand"), "{docs}");
    assert!(docs.contains("- `output.b` — Second operand"), "{docs}");
    assert!(docs.contains("\"a.b`\"?: str | null"), "{docs}");
    assert!(
        docs.contains("- `input[\"a.b\\u0060\"]` — Quoted key"),
        "{docs}"
    );
    let config = serde_json::to_value(StandardPresenterConfig::standard()).unwrap();
    let signature = StandardToolPresenter
        .tool_signature(
            "Field",
            &tool.contract.input_schema.canonical,
            &tool.contract.output_schema.canonical,
            &config,
        )
        .unwrap();
    assert_eq!(
        signature.matches("type Field_2 =").count(),
        1,
        "{signature}"
    );
    let search = StandardToolPresenter
        .discovery_text(&tool.manifest, &tool.contract, &config)
        .unwrap();
    assert_eq!(search.matches("Comparison field").count(), 1, "{search}");
    assert_eq!(search.matches("type Left =").count(), 1, "{search}");
}

/// The dynamic-output witness rule also applies to a referenced input root;
/// only the input and dynamic default's closures belong in its documentation.
#[test]
fn dynamic_witness_and_default_closure_survive_a_referenced_input_root() {
    let tool = ToolDefinition::raw(
        "ask",
        "ask",
        "Ask",
        serde_json::json!({"$ref":"#/$defs/Input", "$defs":{"Input":{
            "type":"object", "properties":{"output":{"type":"object"}}, "required":["output"]
        }}}),
        serde_json::json!({"$ref":"#/$defs/Unused", "$defs":{"Unused":{"type":"string"}}}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(30))
    .with_output_from_input_schema(
        "output",
        Some(
            JsonSchema::admit(serde_json::json!({
                "$ref":"#/$defs/Result", "$defs":{"Result":{"enum":["yes","no"]}}
            }))
            .unwrap(),
        ),
    );
    let compact = compact(&tool);
    let docs = compact.render_markdown();
    assert!(
        docs.contains("type Input = record{output: TypeSpec<T>}"),
        "{docs}"
    );
    assert!(
        docs.contains("type Result = enum[\"yes\", \"no\"]"),
        "{docs}"
    );
    assert!(docs.contains("### ask<T = Result>(Input) -> T"), "{docs}");
    assert!(!docs.contains("Unused"), "{docs}");
    assert!(compact.return_fields.is_empty());
}

/// FIG-6495: a dynamic output witness changes the displayed input contract;
/// its named body cannot share the static tool's unprojected closure.
#[test]
fn static_and_dynamic_witness_closures_have_distinct_definition_identities() {
    let input = serde_json::json!({"$ref":"#/$defs/Input", "$defs":{"Input":{
        "type":"object", "properties":{"output":{"type":"object"}}, "required":["output"]
    }}});
    let plain = ToolDefinition::raw(
        "plain",
        "plain",
        "Static result",
        input.clone(),
        serde_json::json!({"type":"string"}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(30));
    let dynamic = ToolDefinition::raw(
        "dynamic",
        "dynamic",
        "Dynamic result",
        input,
        serde_json::json!({"type":"string"}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(30))
    .with_output_from_input_schema("output", None);
    let tools = [plain, dynamic];
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
    let text = StandardToolPresenter
        .present(ToolPresentationRequest {
            tools: &entries,
            config: &StandardToolPresenter
                .admit_config(&serde_json::json!({}))
                .unwrap(),
            disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
            reserved_names: &[],
            max_section_bytes: None,
        })
        .unwrap()
        .text;
    assert!(
        text.contains("type Input_2 = record{output: TypeSpec<T>}"),
        "{text}"
    );
    assert!(text.contains("dynamic<T = any>(Input_2) -> T"), "{text}");
    assert!(
        !text.contains("type Input = record{output: TypeSpec<T>}"),
        "{text}"
    );
}

/// FIG-6490: the discovery projection is a wire boundary too; removed
/// authored examples cannot be accepted and discarded there.
#[test]
fn compact_discovery_refuses_legacy_authored_examples() {
    let tool = ToolDefinition::raw(
        "tool:lookup",
        "lookup",
        "Lookup",
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"string"}),
    )
    .expect("schemas")
    .with_execution(std::time::Duration::from_secs(30));
    let mut wire = serde_json::to_value(compact(&tool)).expect("compact discovery serializes");
    assert!(wire.get("examples").is_none());
    wire["examples"] = serde_json::json!([]);
    let refused = serde_json::from_value::<CompactToolContract>(wire)
        .expect_err("removed authored examples are not ignored");
    assert!(
        refused.to_string().contains("unknown field `examples`"),
        "{refused}"
    );
}
