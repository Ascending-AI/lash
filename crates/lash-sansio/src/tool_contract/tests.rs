use super::*;

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
    .with_examples(vec![
        "await tools.search_docs({ query: \"rust\" })?".to_string(),
        "await tools.search_docs({ query: \"rust\", limit: 3 })?".to_string(),
        "await tools.search_docs({ query: \"ignored\" })?".to_string(),
    ]);

    let contract = tool.compact_contract();
    assert_eq!(
        contract.signature,
        "search_docs({ query: str, limit?: int <= 10 = 5 })"
    );
    assert_eq!(
        contract.returns,
        "record{matches: list[str], next_page?: str | null}"
    );
    assert_eq!(
        contract.parameters,
        vec![
            serde_json::json!({
                "name": "query",
                "type": "str",
                "required": true,
                "signature": "query: str"
            }),
            serde_json::json!({
                "name": "limit",
                "type": "int",
                "required": false,
                "signature": "limit?: int <= 10 = 5"
            }),
        ]
    );
    assert_eq!(contract.examples.len(), 2);

    let docs = tool.compact_contract().render_markdown();
    assert!(docs.contains(
        "### search_docs({ query: str, limit?: int <= 10 = 5 }) -> record{matches: list[str], next_page?: str | null}"
    ));
    assert!(!docs.contains("Returns:"));
    assert!(docs.contains("Parameters:\n- `query: str`\n- `limit?: int <= 10 = 5`"));
    assert!(docs.contains(
        "Examples: await tools.search_docs({ query: \"rust\" })?; await tools.search_docs({ query: \"rust\", limit: 3 })?"
    ));
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
    .expect("valid declared tool schemas");

    let signature = tool.compact_contract().render_signature();

    assert!(
        signature.contains("module?: str | list[str] | null"),
        "{signature}"
    );
    assert!(!signature.contains("module?: any"), "{signature}");
}

#[test]
fn dynamic_output_contract_renders_schema_from_input_without_return_fields() {
    let tool = ToolDefinition::raw(
        "tool:spawn_agent",
        "spawn_agent",
        "Run a subagent",
        serde_json::json!({
            "type": "object",
            "properties": {
                "output": { "type": "object", "additionalProperties": true }
            }
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_output_from_input_schema("output", None);

    let contract = tool.compact_contract();
    assert_eq!(
        contract.signature,
        "spawn_agent<T = any>({ output?: TypeSpec<T> })"
    );
    assert_eq!(contract.returns, "T");
    assert!(contract.return_fields.is_empty());
    assert_eq!(contract.render_returns(), "");
    assert_eq!(
        tool.compact_contract().render_markdown(),
        "### spawn_agent<T = any>({ output?: TypeSpec<T> }) -> T\nRun a subagent\nParameters:\n- `output?: TypeSpec<T>`"
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
    .with_output_from_input_schema(
        "output",
        Some(
            crate::JsonSchema::admit(serde_json::json!({ "type": "string" }))
                .expect("valid output default schema"),
        ),
    );

    let contract = tool.compact_contract();
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
        },
        "contract": {
            "examples": ["show album library"],
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

    let contract = tool.compact_contract();
    assert_eq!(
        serde_json::to_value(&contract).unwrap(),
        serde_json::json!({
            "name": "mcp__appworld__spotify_show_album_library",
            "signature": "mcp__appworld__spotify_show_album_library({ access_token: str, page_index?: int >= 0 = 0, page_limit?: int >= 1 <= 20 = 5 })",
            "returns": "record{response: list[record{added_at: str | null, album_id: int, genre: str, song_ids: list[int], title: str}] | record{message: str}}",
            "parameters": [
                {
                    "name": "access_token",
                    "type": "str",
                    "required": true,
                    "description": "Access token obtained from spotify app login.",
                    "signature": "access_token: str"
                },
                {
                    "name": "page_index",
                    "type": "int",
                    "required": false,
                    "description": "The index of the page to return.",
                    "signature": "page_index?: int >= 0 = 0"
                },
                {
                    "name": "page_limit",
                    "type": "int",
                    "required": false,
                    "description": "The maximum number of results to return per page.",
                    "signature": "page_limit?: int >= 1 <= 20 = 5"
                }
            ],
            "return_fields": [
                {
                    "path": "response",
                    "type": "list[record]",
                    "required": true,
                    "description": "Albums in the user's library.",
                    "signature": "response: list[record]"
                },
                {
                    "path": "response[].added_at",
                    "type": "str | null",
                    "required": true,
                    "description": "When the album was added to the library.",
                    "signature": "response[].added_at: str | null"
                },
                {
                    "path": "response[].album_id",
                    "type": "int",
                    "required": true,
                    "signature": "response[].album_id: int"
                },
                {
                    "path": "response[].genre",
                    "type": "str",
                    "required": true,
                    "description": "Album genre.",
                    "signature": "response[].genre: str min length 1"
                },
                {
                    "path": "response[].song_ids",
                    "type": "list[int]",
                    "required": true,
                    "signature": "response[].song_ids: list[int]"
                },
                {
                    "path": "response[].title",
                    "type": "str",
                    "required": true,
                    "signature": "response[].title: str min length 1"
                },
                {
                    "path": "response.message",
                    "type": "str",
                    "required": true,
                    "description": "Failure or status message.",
                    "signature": "response.message: str"
                }
            ],
            "description": "[MCP appworld] Search or show a list of albums in your album library.",
            "examples": ["show album library"]
        })
    );
    assert_eq!(
        contract.render_markdown(),
        "### mcp__appworld__spotify_show_album_library({ access_token: str, page_index?: int >= 0 = 0, page_limit?: int >= 1 <= 20 = 5 }) -> record{response: list[record{added_at: str | null, album_id: int, genre: str, song_ids: list[int], title: str}] | record{message: str}}\n[MCP appworld] Search or show a list of albums in your album library.\nParameters:\n- `access_token: str` — Access token obtained from spotify app login.\n- `page_index?: int >= 0 = 0` — The index of the page to return.\n- `page_limit?: int >= 1 <= 20 = 5` — The maximum number of results to return per page.\nReturn fields:\n- `response: list[record]` — Albums in the user's library.\n- `response[].added_at: str | null` — When the album was added to the library.\n- `response[].album_id: int`\n- `response[].genre: str min length 1` — Album genre.\n- `response[].song_ids: list[int]`\n- `response[].title: str min length 1`\n- `response.message: str` — Failure or status message.\nExamples: show album library"
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
    .expect("valid declared tool schemas");

    assert_eq!(
        tool.compact_contract().render_markdown(),
        concat!(
            "### issues_search({ query: str min length 1, ",
            "filter?: record{labels?: list[str], state?: enum[\"open\", \"closed\"], ...: str}, ",
            "sort?: list[record{field: str, descending?: bool}], ... }) -> list[record{id: int}]\n",
            "Search issues.\n",
            "Parameters:\n",
            "- `query: str min length 1`\n",
            "- `filter.labels?: list[str] max items 5`\n",
            "- `filter.state?: enum[\"open\", \"closed\"]` — Issue state.\n",
            "- `sort[].field: str`\n",
            "- `sort[].descending?: bool = false`\n",
            "Return fields:\n",
            "- `[].id: int > 0`"
        )
    );
}
