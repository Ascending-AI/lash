/// V20: a provider dialect has exactly one explicit projection.
#[test]
fn duplicate_dialect_overrides_are_refused_at_decode() {
    let policy = serde_json::json!({"overrides": [
        {"dialect": "google_schema", "schema": {"type": "string"}},
        {"dialect": "google_schema", "schema": {"type": "number"}}
    ]});
    assert!(serde_json::from_value::<super::SchemaProjectionPolicy>(policy).is_err());
    let contract = SchemaContract::default()
        .with_override(
            SchemaDialect::GoogleSchema,
            JsonSchema::admit(json!({"type": "string"})).unwrap(),
        )
        .with_override(SchemaDialect::AnthropicToolInput, JsonSchema::any())
        .with_override(
            SchemaDialect::GoogleSchema,
            JsonSchema::admit(json!({"type": "number"})).unwrap(),
        );
    assert_eq!(contract.projection.overrides().len(), 2);
    let mut encoded = serde_json::to_value(&contract.projection).unwrap();
    encoded["overrides"].as_array_mut().unwrap().reverse();
    let decoded: SchemaProjectionPolicy = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, contract.projection);
    let resolved = resolve_schema(
        &contract,
        SchemaResolutionRequest {
            provider: "test",
            purpose: SchemaPurpose::ToolInput,
            dialects: &[
                SchemaDialect::GoogleSchema,
                SchemaDialect::AnthropicToolInput,
            ],
        },
    )
    .unwrap();
    assert_eq!(resolved.dialect, SchemaDialect::GoogleSchema);
    assert_eq!(resolved.schema, json!({"type": "number"}));
}

use super::*;
use serde_json::json;

fn required_names(schema: &Value) -> Vec<String> {
    let mut names = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn tool_parameters_repairs_empty_root_object() {
    let projected = project_tool_parameters(&json!({})).unwrap();
    assert_eq!(projected.schema["type"], "object");
    assert_eq!(projected.schema["properties"], json!({}));
    assert!(projected.diagnostics.iter().any(|d| d.contains("inferred")));
}

#[test]
fn tool_parameters_repairs_missing_properties_missing_type_and_const() {
    let schema = json!({
        "properties": {
            "mode": { "const": "fast" }
        }
    });
    let projected = project_tool_parameters(&schema).unwrap();
    assert_eq!(projected.schema["type"], "object");
    assert_eq!(
        projected.schema["properties"]["mode"]["enum"],
        json!(["fast"])
    );
    assert!(
        projected.schema["properties"]["mode"]
            .get("const")
            .is_none()
    );
}

#[test]
fn tool_parameters_infers_array_and_enum_types() {
    let schema = json!({
        "type": "object",
        "properties": {
            "tags": { "items": { "type": "string" } },
            "level": { "enum": [1, 2, 3] }
        }
    });
    let projected = project_tool_parameters(&schema).unwrap();
    assert_eq!(projected.schema["properties"]["tags"]["type"], "array");
    assert_eq!(projected.schema["properties"]["level"]["type"], "integer");
}

#[test]
fn strict_projection_requires_optional_nullable_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "required_name": { "type": "string" },
            "optional_count": { "type": "integer" }
        },
        "required": ["required_name"]
    });
    let projected = project_strict_tool_parameters(&schema).unwrap();
    assert_eq!(
        required_names(&projected.schema),
        vec!["optional_count", "required_name"]
    );
    assert_eq!(
        projected.schema["properties"]["optional_count"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(projected.schema["additionalProperties"], false);
}

#[test]
fn strict_projection_preserves_required_nonnullable_fields() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer" }
        },
        "required": ["name", "age"]
    });
    let projected = project_strict_tool_parameters(&schema).unwrap();
    assert_eq!(projected.schema["properties"]["name"]["type"], "string");
    assert_eq!(projected.schema["properties"]["age"]["type"], "integer");
}

#[test]
fn strict_projection_does_not_duplicate_existing_nullable_type() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": ["string", "null"] }
        }
    });
    let projected = project_strict_tool_parameters(&schema).unwrap();
    assert_eq!(
        projected.schema["properties"]["name"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn strict_projection_adds_null_branch_to_optional_any_of() {
    let schema = json!({
        "type": "object",
        "properties": {
            "value": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "integer" }
                ]
            }
        }
    });
    let projected = project_strict_tool_parameters(&schema).unwrap();
    assert_eq!(
        projected.schema["properties"]["value"]["anyOf"][2],
        json!({ "type": "null" })
    );
}

#[test]
fn strict_projection_flattens_single_branch_all_of_before_nullable() {
    let schema = json!({
        "type": "object",
        "properties": {
            "limit": {
                "description": "Maximum number of results.",
                "allOf": [
                    {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100
                    }
                ]
            }
        }
    });
    let projected = project_strict_tool_parameters(&schema).unwrap();
    let limit = &projected.schema["properties"]["limit"];
    assert!(limit.get("allOf").is_none());
    assert_eq!(limit["description"], "Maximum number of results.");
    assert_eq!(limit["type"], json!(["integer", "null"]));
    assert_eq!(limit["minimum"], 1);
    assert_eq!(limit["maximum"], 100);
    assert!(
        projected
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("flattened single-branch allOf"))
    );
}

#[test]
fn structured_output_enforces_strict_objects() {
    let schema = json!({
        "type": "object",
        "properties": {
            "value": { "type": "string" }
        }
    });
    let projected = project_structured_output(&schema).unwrap();
    assert_eq!(projected.schema["required"], json!(["value"]));
    assert_eq!(projected.schema["additionalProperties"], false);
}

#[test]
fn structured_output_recurses_into_nested_objects_and_arrays() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "score": { "type": "number" }
                    },
                    "required": ["title"]
                }
            }
        }
    });
    let projected = project_structured_output(&schema).unwrap();
    let nested = &projected.schema["properties"]["items"]["items"];
    assert_eq!(nested["additionalProperties"], false);
    assert_eq!(required_names(nested), vec!["score", "title"]);
    assert_eq!(
        nested["properties"]["score"]["type"],
        json!(["number", "null"])
    );
}

#[test]
fn structured_output_recurses_into_defs_and_definitions() {
    let schema = json!({
        "type": "object",
        "properties": {
            "item": { "$ref": "#/$defs/Item" },
            "legacy": { "$ref": "#/definitions/Legacy" }
        },
        "$defs": {
            "Item": {
                "type": "object",
                "properties": { "id": { "type": "string" } }
            }
        },
        "definitions": {
            "Legacy": {
                "type": "object",
                "properties": { "flag": { "type": "boolean" } }
            }
        }
    });
    let projected = project_structured_output(&schema).unwrap();
    assert_eq!(
        projected.schema["$defs"]["Item"]["additionalProperties"],
        false
    );
    assert_eq!(projected.schema["$defs"]["Item"]["required"], json!(["id"]));
    assert_eq!(
        projected.schema["definitions"]["Legacy"]["additionalProperties"],
        false
    );
    assert_eq!(
        projected.schema["definitions"]["Legacy"]["required"],
        json!(["flag"])
    );
}

#[test]
fn structured_output_allows_nested_any_of_and_projects_branches() {
    let schema = json!({
        "type": "object",
        "properties": {
            "value": {
                "anyOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": { "count": { "type": "integer" } }
                    }
                ]
            }
        }
    });
    let projected = project_structured_output(&schema).unwrap();
    let object_branch = &projected.schema["properties"]["value"]["anyOf"][1];
    assert_eq!(object_branch["required"], json!(["count"]));
    assert_eq!(object_branch["additionalProperties"], false);
    assert_eq!(
        projected.schema["properties"]["value"]["anyOf"][2],
        json!({ "type": "null" })
    );
}

#[test]
fn structured_output_rejects_root_scalar() {
    let err = project_structured_output(&json!({ "type": "string" })).unwrap_err();
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("root schema must be an object"))
    );
}

#[test]
fn structured_output_rejects_root_any_of() {
    let err = project_structured_output(&json!({
        "anyOf": [
            { "type": "object", "properties": {} },
            { "type": "object", "properties": {} }
        ]
    }))
    .unwrap_err();
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("root anyOf"))
    );
}

#[test]
fn projection_rejects_unsupported_lossy_keywords() {
    let err = project_structured_output(&json!({
        "type": "object",
        "properties": {},
        "allOf": [
            { "type": "object", "properties": {} },
            { "type": "object", "properties": {} }
        ],
        "patternProperties": {}
    }))
    .unwrap_err();
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("allOf"))
    );
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("patternProperties"))
    );
}

#[test]
fn projection_rejects_non_object_properties() {
    let err = project_tool_parameters(&json!({
        "type": "object",
        "properties": []
    }))
    .unwrap_err();
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("properties must be an object"))
    );
}

/// An explicit dialect override takes precedence over automatic canonical repair.
#[test]
fn resolver_auto_tool_parameters_uses_the_explicit_override() {
    let wire = json!({
        "type": "object",
        "properties": {"mode": {"enum": ["override"]}}
    });
    let contract = SchemaContract::admit(json!({
        "type": "object",
        "properties": {"mode": {"const": "x"}}
    }))
    .expect("canonical schema")
    .with_override(
        SchemaDialect::OpenaiToolParameters,
        JsonSchema::admit(wire.clone()).expect("explicit wire schema"),
    );
    let resolved = resolve_schema(
        &contract,
        SchemaResolutionRequest {
            provider: "test",
            purpose: SchemaPurpose::ToolInput,
            dialects: &[SchemaDialect::OpenaiToolParameters],
        },
    )
    .expect("resolve explicit override");
    assert_eq!(resolved.schema, wire);
    assert_eq!(resolved.dialect, SchemaDialect::OpenaiToolParameters);
    assert!(resolved.diagnostics.is_empty());
}

#[test]
fn resolver_explicit_only_fails_without_matching_override() {
    let mut contract = SchemaContract::admit(json!({
        "type": "object",
        "properties": {}
    }))
    .expect("valid declared schema");
    contract.projection.mode = ProjectionMode::ExplicitOnly;

    let err = resolve_schema(
        &contract,
        SchemaResolutionRequest {
            provider: "test",
            purpose: SchemaPurpose::StructuredOutput,
            dialects: &[SchemaDialect::OpenaiStructuredOutput],
        },
    )
    .unwrap_err();

    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("no explicit projection override"))
    );
}

#[test]
fn bedrock_projection_strips_array_constraints_from_wire_schema_only() {
    let contract = SchemaContract::admit(json!({
        "type": "object",
        "required": ["ranked"],
        "properties": {
            "ranked": {
                "type": "array",
                "minItems": 3,
                "maxItems": 3,
                "items": { "type": "string" }
            }
        }
    }))
    .expect("valid declared schema");

    let resolved = resolve_schema(
        &contract,
        SchemaResolutionRequest {
            provider: "test",
            purpose: SchemaPurpose::StructuredOutput,
            dialects: &[SchemaDialect::BedrockClaudeOutputConfigJsonSchema],
        },
    )
    .unwrap();

    let ranked = &resolved.schema["properties"]["ranked"];
    assert!(ranked.get("minItems").is_none());
    assert!(ranked.get("maxItems").is_none());
    assert_eq!(
        contract.canonical.as_value()["properties"]["ranked"]["minItems"],
        3
    );
    assert!(
        ranked["description"]
            .as_str()
            .is_some_and(|description| description.contains("minItems=3"))
    );
}
