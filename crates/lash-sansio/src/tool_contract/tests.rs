#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_definition_uses_canonical_model_schemas() {
        let tool = ToolDefinition::raw(
            "tool:mcp__demo__search",
            "mcp__demo__search",
            "Search demo server",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "hits": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["hits"],
                "additionalProperties": false
            }),
        );

        let model_tool = tool.model_tool();
        assert_eq!(
            model_tool.input_schema.canonical["properties"]["limit"]["type"],
            serde_json::json!("integer")
        );
        assert_eq!(
            model_tool.output_schema.canonical["properties"]["hits"]["type"],
            serde_json::json!("array")
        );
    }

    #[test]
    fn tool_retry_policy_defaults_to_never_and_is_omitted_from_manifest_json() {
        let tool = ToolDefinition::raw(
            "tool:demo",
            "demo",
            "Demo",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        );

        assert_eq!(tool.manifest.retry_policy, ToolRetryPolicy::Never);
        let manifest = tool.manifest();
        assert_eq!(manifest.retry_policy, ToolRetryPolicy::Never);
        let encoded = serde_json::to_value(&manifest).expect("manifest json");
        assert!(encoded.get("retry_policy").is_none());
    }

    #[test]
    fn tool_retry_policy_propagates_through_manifest_and_definition_roundtrip() {
        let tool = ToolDefinition::raw(
            "tool:demo",
            "demo",
            "Demo",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .with_retry_policy(ToolRetryPolicy::safe(3, 10, 100));

        let manifest = tool.manifest();
        assert_eq!(
            manifest.retry_policy,
            ToolRetryPolicy::Safe {
                max_attempts: 3,
                base_delay_ms: 10,
                max_delay_ms: 100,
            }
        );

        let roundtrip = ToolDefinition::from_parts(manifest, tool.contract());
        assert_eq!(roundtrip.manifest.retry_policy, tool.manifest.retry_policy);
        let encoded = serde_json::to_value(roundtrip.manifest()).expect("manifest json");
        assert_eq!(encoded["retry_policy"]["type"], serde_json::json!("safe"));
    }

    #[test]
    fn tool_argument_projection_defaults_to_materialize_and_is_omitted_from_manifest_json() {
        let tool = ToolDefinition::raw(
            "tool:demo",
            "demo",
            "Demo",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        );

        assert_eq!(
            tool.manifest.argument_projection,
            ToolArgumentProjectionPolicy::MaterializeProjectedValues
        );
        let manifest = tool.manifest();
        assert_eq!(
            manifest.argument_projection,
            ToolArgumentProjectionPolicy::MaterializeProjectedValues
        );
        let encoded = serde_json::to_value(&manifest).expect("manifest json");
        assert!(encoded.get("argument_projection").is_none());
    }

    #[test]
    fn tool_argument_projection_propagates_through_manifest_and_definition_roundtrip() {
        let tool = ToolDefinition::raw(
            "tool:demo",
            "demo",
            "Demo",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .with_argument_projection(
            ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        );

        let manifest = tool.manifest();
        assert_eq!(
            manifest.argument_projection,
            tool.manifest.argument_projection
        );

        let roundtrip = ToolDefinition::from_parts(manifest, tool.contract());
        assert_eq!(
            roundtrip.manifest.argument_projection,
            tool.manifest.argument_projection
        );
        let encoded = serde_json::to_value(roundtrip.manifest()).expect("manifest json");
        assert_eq!(
            encoded["argument_projection"],
            serde_json::json!({
                "kind": "preserve_projected_refs_in_field",
                "field": "seed"
            })
        );
    }

    #[test]
    fn model_tool_preserves_schema_projection_overrides() {
        let tool = ToolDefinition::raw(
            "tool:demo",
            "demo",
            "Demo",
            serde_json::json!({
                "type": "object",
                "properties": { "raw": { "const": "x" } }
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_input_schema_projection(
            "provider.tool_parameters",
            serde_json::json!({
                "type": "object",
                "properties": { "raw": { "type": "string", "enum": ["x"] } }
            }),
        )
        .with_output_schema_projection(
            "provider.structured_output",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        );

        let model_tool = tool.model_tool();
        assert_eq!(
            model_tool.input_schema.canonical["properties"]["raw"]["const"],
            "x"
        );
        assert_eq!(
            model_tool.input_schema.projection.overrides[0].schema["properties"]["raw"]["enum"],
            serde_json::json!(["x"])
        );
        assert_eq!(
            model_tool.output_schema.projection.overrides[0].dialect,
            "provider.structured_output"
        );
    }

    #[test]
    fn typed_tool_definition_generates_input_and_output_schema() {
        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        enum Mode {
            Fast,
            Slow,
        }

        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Args {
            query: String,
            #[schemars(range(max = 20))]
            page_limit: u8,
            #[schemars(length(min = 1, max = 3))]
            tags: Vec<String>,
            mode: Option<Mode>,
        }

        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Output {
            answer: String,
            #[schemars(range(min = 0))]
            confidence: f32,
        }

        let tool = ToolDefinition::typed::<Args, Output>("tool:test/demo", "demo", "Demo");
        let metadata = tool.parameter_metadata();
        assert!(metadata.iter().any(|param| {
            param["name"] == "page_limit"
                && param["type"] == "int"
                && param["maximum"].as_f64() == Some(20.0)
        }));
        assert!(metadata.iter().any(|param| {
            param["name"] == "tags"
                && param["type"] == "list[str]"
                && param["min_items"] == 1
                && param["max_items"] == 3
        }));
        assert!(
            metadata
                .iter()
                .any(|param| { param["name"] == "mode" && param["nullable"] == true })
        );
        assert_eq!(
            tool.contract.output_schema.canonical["properties"]["answer"]["type"],
            "string"
        );
        assert_eq!(
            tool.contract.output_schema.canonical["properties"]["confidence"]["minimum"].as_f64(),
            Some(0.0)
        );
    }

    #[test]
    fn raw_tool_definition_preserves_caller_provided_schemas() {
        let input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 3 }
            },
            "required": ["query"],
            "x-custom": { "keep": true }
        });
        let output_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" }
            },
            "required": ["ok"],
            "x-result": ["exact"]
        });

        let tool = ToolDefinition::raw(
            "tool:raw_demo",
            "raw_demo",
            "Raw demo",
            input_schema.clone(),
            output_schema.clone(),
        );

        assert_eq!(tool.contract.input_schema.canonical, input_schema);
        assert_eq!(tool.contract.output_schema.canonical, output_schema);
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
                    "default": 5,
                    "maximum": 10,
                    "signature": "limit?: int <= 10 = 5"
                }),
            ]
        );
        assert_eq!(contract.examples.len(), 2);

        let docs = ToolDefinition::format_tool_docs(&[tool]);
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
        );

        let signature = tool.compact_contract().render_signature();

        assert!(
            signature.contains("module?: list[str] | str | null"),
            "{signature}"
        );
        assert!(!signature.contains("module?: any"), "{signature}");
    }

    #[test]
    fn static_output_contract_keeps_existing_compact_docs_and_serde_shape() {
        let tool = ToolDefinition::raw(
            "tool:read_text",
            "read_text",
            "Read text",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        );
        let explicit_static = tool
            .clone()
            .with_output_contract(ToolOutputContract::Static);

        assert_eq!(
            ToolDefinition::format_tool_docs(std::slice::from_ref(&tool)),
            ToolDefinition::format_tool_docs(&[explicit_static])
        );
        assert_eq!(tool.compact_contract().returns, "str");

        let serialized = serde_json::to_value(&tool).expect("serialize");
        assert!(serialized.get("output_contract").is_none());
        let deserialized: ToolDefinition = serde_json::from_value(serialized).expect("deserialize");
        assert!(deserialized.contract.output_contract.is_static());
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
            ToolDefinition::format_tool_docs(&[tool]),
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
        .with_output_from_input_schema("output", Some(serde_json::json!({ "type": "string" })));

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
    fn json_schema_loaded_contract_matches_hardcoded_renderer() {
        let tool: ToolDefinition = serde_json::from_value(serde_json::json!({
            "id": "tool:mcp__appworld__spotify_search_songs",
            "name": "mcp__appworld__spotify_search_songs",
            "description": "[MCP appworld] Search for songs with a query.",
            "examples": ["search songs by genre"],
            "input_schema": {
                "canonical": {
                "type": "object",
                "properties": {
                    "access_token": {
                        "type": "string",
                        "description": "Access token obtained from spotify app login."
                    },
                    "genre": {
                        "type": ["string", "null"],
                        "description": "Only include songs from this genre.",
                        "default": null
                    },
                    "page_limit": {
                        "type": "integer",
                        "description": "Maximum number of songs to return.",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 5
                    },
                    "sort_by": {
                        "type": ["string", "null"],
                        "description": "Field to sort by. Prefix with '-' for descending order.",
                        "default": null
                    }
                },
                "required": ["access_token"],
                "additionalProperties": false
                }
            },
            "output_schema": {
                "canonical": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "response": {
                                "type": "array",
                                "description": "Matched songs.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "album_id": {
                                            "type": ["integer", "null"],
                                            "description": "Album identifier when the song belongs to an album."
                                        },
                                        "album_title": { "type": ["string", "null"] },
                                        "artists": {
                                            "type": "array",
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "id": { "type": "integer" },
                                                    "name": { "type": "string" }
                                                },
                                                "required": ["id", "name"]
                                            }
                                        },
                                        "duration": { "type": "integer" },
                                        "genre": { "type": "string" },
                                        "like_count": { "type": "integer" },
                                        "play_count": {
                                            "type": "integer",
                                            "description": "Number of times the song was played.",
                                            "minimum": 0
                                        },
                                        "rating": { "type": "number" },
                                        "release_date": {
                                            "type": "string",
                                            "description": "Song release date in YYYY-MM-DD format."
                                        },
                                        "song_id": {
                                            "type": "integer",
                                            "description": "Stable song identifier."
                                        },
                                        "title": {
                                            "type": "string",
                                            "description": "Song title."
                                        }
                                    },
                                    "required": [
                                        "album_id",
                                        "album_title",
                                        "artists",
                                        "duration",
                                        "genre",
                                        "like_count",
                                        "play_count",
                                        "rating",
                                        "release_date",
                                        "song_id",
                                        "title"
                                    ]
                                }
                            }
                        },
                        "required": ["response"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "response": {
                                "type": "object",
                                "properties": {
                                    "message": {
                                        "type": "string",
                                        "description": "Failure or status message."
                                    }
                                },
                                "required": ["message"]
                            }
                        },
                        "required": ["response"]
                    }
                ]
                }
            }
        }))
        .unwrap();

        let contract = tool.compact_contract();
        assert_eq!(
            serde_json::to_value(&contract).unwrap(),
            serde_json::json!({
                "name": "mcp__appworld__spotify_search_songs",
                "signature": "mcp__appworld__spotify_search_songs({ access_token: str, genre?: str | null = null, page_limit?: int >= 1 <= 20 = 5, sort_by?: str | null = null })",
                "returns": "record{response: list[record{album_id: int | null, album_title: str | null, artists: list[record{id: int, name: str}], duration: int, genre: str, like_count: int, play_count: int, rating: float, release_date: str, song_id: int, title: str}]} | record{response: record{message: str}}",
                "parameters": [
                    {
                        "name": "access_token",
                        "type": "str",
                        "required": true,
                        "description": "Access token obtained from spotify app login.",
                        "signature": "access_token: str"
                    },
                    {
                        "name": "genre",
                        "type": "str | null",
                        "required": false,
                        "nullable": true,
                        "description": "Only include songs from this genre.",
                        "default": null,
                        "signature": "genre?: str | null = null"
                    },
                    {
                        "name": "page_limit",
                        "type": "int",
                        "required": false,
                        "description": "Maximum number of songs to return.",
                        "default": 5,
                        "minimum": 1,
                        "maximum": 20,
                        "signature": "page_limit?: int >= 1 <= 20 = 5"
                    },
                    {
                        "name": "sort_by",
                        "type": "str | null",
                        "required": false,
                        "nullable": true,
                        "description": "Field to sort by. Prefix with '-' for descending order.",
                        "default": null,
                        "signature": "sort_by?: str | null = null"
                    }
                ],
                "return_fields": [
                    {
                        "path": "response",
                        "type": "list[record]",
                        "required": true,
                        "description": "Matched songs.",
                        "items": "record",
                        "signature": "response: list[record]"
                    },
                    {
                        "path": "response[].album_id",
                        "type": "int | null",
                        "required": true,
                        "nullable": true,
                        "description": "Album identifier when the song belongs to an album.",
                        "signature": "response[].album_id: int | null"
                    },
                    {
                        "path": "response[].album_title",
                        "type": "str | null",
                        "required": true,
                        "nullable": true,
                        "signature": "response[].album_title: str | null"
                    },
                    {
                        "path": "response[].artists[].id",
                        "type": "int",
                        "required": true,
                        "signature": "response[].artists[].id: int"
                    },
                    {
                        "path": "response[].artists[].name",
                        "type": "str",
                        "required": true,
                        "signature": "response[].artists[].name: str"
                    },
                    {
                        "path": "response[].duration",
                        "type": "int",
                        "required": true,
                        "signature": "response[].duration: int"
                    },
                    {
                        "path": "response[].genre",
                        "type": "str",
                        "required": true,
                        "signature": "response[].genre: str"
                    },
                    {
                        "path": "response[].like_count",
                        "type": "int",
                        "required": true,
                        "signature": "response[].like_count: int"
                    },
                    {
                        "path": "response[].play_count",
                        "type": "int",
                        "required": true,
                        "description": "Number of times the song was played.",
                        "minimum": 0,
                        "signature": "response[].play_count: int >= 0"
                    },
                    {
                        "path": "response[].rating",
                        "type": "float",
                        "required": true,
                        "signature": "response[].rating: float"
                    },
                    {
                        "path": "response[].release_date",
                        "type": "str",
                        "required": true,
                        "description": "Song release date in YYYY-MM-DD format.",
                        "signature": "response[].release_date: str"
                    },
                    {
                        "path": "response[].song_id",
                        "type": "int",
                        "required": true,
                        "description": "Stable song identifier.",
                        "signature": "response[].song_id: int"
                    },
                    {
                        "path": "response[].title",
                        "type": "str",
                        "required": true,
                        "description": "Song title.",
                        "signature": "response[].title: str"
                    },
                    {
                        "path": "response.message",
                        "type": "str",
                        "required": true,
                        "description": "Failure or status message.",
                        "signature": "response.message: str"
                    }
                ],
                "description": "[MCP appworld] Search for songs with a query.",
                "examples": ["search songs by genre"]
            })
        );

        assert_eq!(
            contract.render_markdown(),
            "### mcp__appworld__spotify_search_songs({ access_token: str, genre?: str | null = null, page_limit?: int >= 1 <= 20 = 5, sort_by?: str | null = null }) -> record{response: list[record{album_id: int | null, album_title: str | null, artists: list[record{id: int, name: str}], duration: int, genre: str, like_count: int, play_count: int, rating: float, release_date: str, song_id: int, title: str}]} | record{response: record{message: str}}\n[MCP appworld] Search for songs with a query.\nParameters:\n- `access_token: str` — Access token obtained from spotify app login.\n- `genre?: str | null = null` — Only include songs from this genre.\n- `page_limit?: int >= 1 <= 20 = 5` — Maximum number of songs to return.\n- `sort_by?: str | null = null` — Field to sort by. Prefix with '-' for descending order.\nReturn fields:\n- `response: list[record]` — Matched songs.\n- `response[].album_id: int | null` — Album identifier when the song belongs to an album.\n- `response[].album_title: str | null`\n- `response[].artists[].id: int`\n- `response[].artists[].name: str`\n- `response[].duration: int`\n- `response[].genre: str`\n- `response[].like_count: int`\n- `response[].play_count: int >= 0` — Number of times the song was played.\n- `response[].rating: float`\n- `response[].release_date: str` — Song release date in YYYY-MM-DD format.\n- `response[].song_id: int` — Stable song identifier.\n- `response[].title: str` — Song title.\n- `response.message: str` — Failure or status message.\nExamples: search songs by genre"
        );
        assert_eq!(
            contract.render_signature(),
            "mcp__appworld__spotify_search_songs({ access_token: str, genre?: str | null = null, page_limit?: int >= 1 <= 20 = 5, sort_by?: str | null = null }) -> record{response: list[record{album_id: int | null, album_title: str | null, artists: list[record{id: int, name: str}], duration: int, genre: str, like_count: int, play_count: int, rating: float, release_date: str, song_id: int, title: str}]} | record{response: record{message: str}}\nParameters:\n- `access_token: str` — Access token obtained from spotify app login.\n- `genre?: str | null = null` — Only include songs from this genre.\n- `page_limit?: int >= 1 <= 20 = 5` — Maximum number of songs to return.\n- `sort_by?: str | null = null` — Field to sort by. Prefix with '-' for descending order.\nReturn fields:\n- `response: list[record]` — Matched songs.\n- `response[].album_id: int | null` — Album identifier when the song belongs to an album.\n- `response[].album_title: str | null`\n- `response[].artists[].id: int`\n- `response[].artists[].name: str`\n- `response[].duration: int`\n- `response[].genre: str`\n- `response[].like_count: int`\n- `response[].play_count: int >= 0` — Number of times the song was played.\n- `response[].rating: float`\n- `response[].release_date: str` — Song release date in YYYY-MM-DD format.\n- `response[].song_id: int` — Stable song identifier.\n- `response[].title: str` — Song title.\n- `response.message: str` — Failure or status message."
        );
        assert_eq!(
            contract.render_returns(),
            "Return fields:\n- `response: list[record]` — Matched songs.\n- `response[].album_id: int | null` — Album identifier when the song belongs to an album.\n- `response[].album_title: str | null`\n- `response[].artists[].id: int`\n- `response[].artists[].name: str`\n- `response[].duration: int`\n- `response[].genre: str`\n- `response[].like_count: int`\n- `response[].play_count: int >= 0` — Number of times the song was played.\n- `response[].rating: float`\n- `response[].release_date: str` — Song release date in YYYY-MM-DD format.\n- `response[].song_id: int` — Stable song identifier.\n- `response[].title: str` — Song title.\n- `response.message: str` — Failure or status message."
        );
    }

    #[test]
    fn json_schema_loaded_contract_merges_nullable_anyof_return_fields() {
        let tool: ToolDefinition = serde_json::from_value(serde_json::json!({
            "id": "tool:mcp__appworld__spotify_show_album_library",
            "name": "mcp__appworld__spotify_show_album_library",
            "description": "[MCP appworld] Search or show a list of albums in your album library.",
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
        }))
        .unwrap();

        let contract = tool.compact_contract();
        assert_eq!(
            serde_json::to_value(&contract).unwrap(),
            serde_json::json!({
                "name": "mcp__appworld__spotify_show_album_library",
                "signature": "mcp__appworld__spotify_show_album_library({ access_token: str, page_index?: int >= 0 = 0, page_limit?: int >= 1 <= 20 = 5 })",
                "returns": "record{response: list[record{added_at: null | str, album_id: int, genre: str, song_ids: list[int], title: str}] | record{message: str}}",
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
                        "default": 0,
                        "minimum": 0,
                        "signature": "page_index?: int >= 0 = 0"
                    },
                    {
                        "name": "page_limit",
                        "type": "int",
                        "required": false,
                        "description": "The maximum number of results to return per page.",
                        "default": 5,
                        "minimum": 1,
                        "maximum": 20,
                        "signature": "page_limit?: int >= 1 <= 20 = 5"
                    }
                ],
                "return_fields": [
                    {
                        "path": "response",
                        "type": "list[record]",
                        "required": true,
                        "description": "Albums in the user's library.",
                        "items": "record",
                        "signature": "response: list[record]"
                    },
                    {
                        "path": "response[].added_at",
                        "type": "str | null",
                        "required": true,
                        "nullable": true,
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
                        "min_length": 1,
                        "signature": "response[].genre: str min_len 1"
                    },
                    {
                        "path": "response[].song_ids[]",
                        "type": "int",
                        "required": true,
                        "signature": "response[].song_ids[]: int"
                    },
                    {
                        "path": "response[].title",
                        "type": "str",
                        "required": true,
                        "min_length": 1,
                        "signature": "response[].title: str min_len 1"
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
            "### mcp__appworld__spotify_show_album_library({ access_token: str, page_index?: int >= 0 = 0, page_limit?: int >= 1 <= 20 = 5 }) -> record{response: list[record{added_at: null | str, album_id: int, genre: str, song_ids: list[int], title: str}] | record{message: str}}\n[MCP appworld] Search or show a list of albums in your album library.\nParameters:\n- `access_token: str` — Access token obtained from spotify app login.\n- `page_index?: int >= 0 = 0` — The index of the page to return.\n- `page_limit?: int >= 1 <= 20 = 5` — The maximum number of results to return per page.\nReturn fields:\n- `response: list[record]` — Albums in the user's library.\n- `response[].added_at: str | null` — When the album was added to the library.\n- `response[].album_id: int`\n- `response[].genre: str min_len 1` — Album genre.\n- `response[].song_ids[]: int`\n- `response[].title: str min_len 1`\n- `response.message: str` — Failure or status message.\nExamples: show album library"
        );
    }

    #[test]
    fn tool_bindings_serde_defaults_are_empty() {
        let tool: ToolDefinition = serde_json::from_value(serde_json::json!({
            "id": "tool:read_file",
            "name": "read_file",
            "description": "Read a file"
        }))
        .unwrap();
        assert!(tool.manifest.bindings.is_empty());
    }

    #[test]
    fn tool_bindings_round_trip_as_opaque_metadata() {
        let mut with_metadata = ToolDefinition::raw(
            "tool:read_file",
            "read_file",
            "Read a file",
            ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "string"}),
        );
        with_metadata
            .manifest
            .bindings
            .insert("example.binding".to_string(), serde_json::json!({ "name": "read" }));

        let encoded = serde_json::to_value(&with_metadata).expect("tool json");
        assert_eq!(encoded["bindings"]["example.binding"]["name"], "read");
        let decoded: ToolDefinition = serde_json::from_value(encoded).expect("round trip");
        assert_eq!(
            decoded.manifest.bindings["example.binding"]["name"],
            "read"
        );
    }
}
