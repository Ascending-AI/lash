use crate::{ToolContract, ValueMismatch};

pub fn validate_tool_input(
    contract: &ToolContract,
    args: &serde_json::Value,
) -> Result<(), ValueMismatch> {
    contract.input_schema.canonical.validate(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JsonSchema, ToolDefinition};
    use std::time::{Duration, Instant};

    #[test]
    fn unversioned_schemas_keep_draft7_validation() {
        let email = JsonSchema::admit(serde_json::json!({ "type": "string", "format": "email" }))
            .expect("valid declared payload schema");
        assert!(
            email
                .validate(&serde_json::json!("sam@example.com"))
                .is_ok()
        );
        assert!(email.validate(&serde_json::json!("invalid")).is_err());

        let reference = JsonSchema::admit(serde_json::json!({
            "$ref": "#/definitions/Value",
            "definitions": { "Value": { "type": "string" } },
            "maxLength": 1
        }))
        .expect("valid declared payload schema");
        assert!(reference.validate(&serde_json::json!("long")).is_ok());
        assert!(reference.validate(&serde_json::json!(42)).is_err());

        let tuple = JsonSchema::admit(serde_json::json!({
            "type": "array", "items": [{ "type": "string" }], "additionalItems": false
        }))
        .expect("valid declared payload schema");
        assert!(tuple.validate(&serde_json::json!(["item"])).is_ok());
        assert!(tuple.validate(&serde_json::json!([42])).is_err());
        assert!(
            tuple
                .validate(&serde_json::json!(["item", "extra"]))
                .is_err()
        );
    }

    #[test]
    fn declared_draft4_and_draft6_keep_their_keywords() {
        for (draft, accepts_other) in [("04", true), ("06", false)] {
            let schema = JsonSchema::admit(serde_json::json!({
                "$schema": format!("http://json-schema.org/draft-{draft}/schema#"),
                "type": "string", "const": "expected"
            }))
            .expect("valid declared payload schema");
            assert!(schema.validate(&serde_json::json!("expected")).is_ok());
            assert_eq!(
                schema.validate(&serde_json::json!("other")).is_ok(),
                accepts_other
            );
            assert!(schema.validate(&serde_json::json!(42)).is_err());
        }
    }

    #[test]
    fn international_formats_remain_assertions() {
        for (format, valid, invalid) in [
            ("idn-hostname", "münchen.de", "bad..hostname"),
            ("idn-email", "sam@münchen.de", "invalid"),
        ] {
            let schema =
                JsonSchema::admit(serde_json::json!({ "type": "string", "format": format }))
                    .expect("valid declared payload schema");
            assert!(
                schema.validate(&serde_json::json!(valid)).is_ok(),
                "{format}"
            );
            assert!(
                schema.validate(&serde_json::json!(invalid)).is_err(),
                "{format}"
            );
        }
    }

    #[test]
    fn declared_draft202012_validates_prefix_items_and_closed_unevaluated_properties() {
        let schema = JsonSchema::admit(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "$defs": {
                "Pair": {
                    "type": "array",
                    "prefixItems": [{ "type": "string" }, { "type": "integer" }],
                    "items": false,
                    "minItems": 2
                }
            },
            "allOf": [{ "properties": { "pair": { "$ref": "#/$defs/Pair" } }, "required": ["pair"] }],
            "unevaluatedProperties": false
        })).expect("valid declared payload schema");
        assert!(
            schema
                .validate(&serde_json::json!({ "pair": ["item", 42] }))
                .is_ok()
        );
        for invalid in [
            serde_json::json!({ "pair": [42, "item"] }),
            serde_json::json!({ "pair": ["item", 42, "extra"] }),
            serde_json::json!({ "pair": ["item"] }),
            serde_json::json!({ "pair": ["item", 42], "extra": true }),
            serde_json::json!({}),
        ] {
            assert!(schema.validate(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validation_rejects_values_that_violate_local_refs() {
        for definitions_key in ["$defs", "definitions"] {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "item": { "$ref": format!("#/{definitions_key}/Item") }
                },
                "required": ["item"],
                "additionalProperties": false,
                (definitions_key): {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    }
                }
            });

            let error = JsonSchema::admit(schema)
                .expect("valid declared payload schema")
                .validate(&serde_json::json!({ "item": { "name": 42 } }))
                .unwrap_err();

            assert_eq!(
                error.to_string(),
                "/item/name: 42 is not of type \"string\""
            );
        }
    }

    #[test]
    fn validation_rejects_bad_value_through_all_of_wrapped_ref() {
        let schema = JsonSchema::admit(serde_json::json!({
            "definitions": {
                "Inner": { "type": "string" }
            },
            "allOf": [
                { "$ref": "#/definitions/Inner" }
            ],
            "description": "A documented field"
        }))
        .expect("valid declared payload schema");

        let error = schema.validate(&serde_json::json!(42)).unwrap_err();

        assert_eq!(error.to_string(), "42 is not of type \"string\"");
    }

    #[test]
    fn validation_of_chained_doubling_refs_is_bounded() {
        // Keep the adversarial fanout large enough to catch eager expansion,
        // but small enough that a regression cannot exhaust the test machine.
        const DEPTH: usize = 16;
        const TIME_LIMIT: Duration = Duration::from_secs(1);

        let mut definitions = serde_json::Map::new();
        for depth in 0..DEPTH {
            definitions.insert(
                format!("D{depth}"),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "left": { "$ref": format!("#/definitions/D{}", depth + 1) },
                        "right": { "$ref": format!("#/definitions/D{}", depth + 1) }
                    }
                }),
            );
        }
        definitions.insert(format!("D{DEPTH}"), serde_json::json!({ "type": "string" }));
        let schema = JsonSchema::admit(serde_json::json!({
            "$ref": "#/definitions/D0",
            "definitions": definitions
        }))
        .expect("valid declared payload schema");
        let mut value = serde_json::json!(42);
        for _ in 0..DEPTH {
            value = serde_json::json!({ "left": value });
        }

        let started = Instant::now();
        let error = schema.validate(&value).unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            error
                .to_string()
                .ends_with(": 42 is not of type \"string\""),
            "{error}"
        );
        assert!(error.to_string().contains("/left"), "{error}");
        assert!(
            elapsed < TIME_LIMIT,
            "chained reference validation took {elapsed:?}, limit is {TIME_LIMIT:?}"
        );
    }

    #[test]
    fn admission_rejects_unresolvable_local_ref() {
        let error =
            JsonSchema::admit(serde_json::json!({ "$ref": "#/definitions/Missing" })).unwrap_err();
        assert!(matches!(
            error,
            crate::SchemaAdmissionError::Compilation { .. }
        ));
    }

    #[test]
    fn admission_rejects_external_references() {
        for keyword in ["$ref", "$dynamicRef"] {
            let error = JsonSchema::admit(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                (keyword): "https://example.com/schema.json"
            }))
            .unwrap_err();
            assert!(matches!(
                error,
                crate::SchemaAdmissionError::NonLocalReference { .. }
            ));
        }
    }

    #[test]
    fn validation_rejects_deep_recursive_violation() {
        let schema = JsonSchema::admit(serde_json::json!({
            "$ref": "#/definitions/Node",
            "definitions": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "child": { "$ref": "#/definitions/Node" }
                    },
                    "required": ["name"]
                }
            }
        }))
        .expect("valid declared payload schema");

        let error = schema
            .validate(&serde_json::json!({
                "name": "root",
                "child": {
                    "name": "level 1",
                    "child": {
                        "name": "level 2",
                        "child": { "name": 123 }
                    }
                }
            }))
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "/child/child/child/name: 123 is not of type \"string\""
        );
    }

    #[test]
    fn validation_allows_unknown_property_when_additional_properties_is_omitted() {
        let tool = ToolDefinition::raw(
            "tool:mcp__appworld__venmo_show_transactions",
            "mcp__appworld__venmo_show_transactions",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "min_created_at": { "type": "string" },
                    "max_created_at": { "type": "string" },
                    "limit": { "type": "integer", "maximum": 100 }
                },
                "required": ["limit"]
            }),
            serde_json::json!({}),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120));

        validate_tool_input(
            &tool.contract(),
            &serde_json::json!({
                "min_datetime": "2024-01-01T00:00:00Z",
                "limit": 20
            }),
        )
        .unwrap();
    }

    #[test]
    fn validation_allows_unknown_property_when_additional_properties_is_true() {
        let tool = ToolDefinition::raw(
            "tool:open",
            "open",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "additionalProperties": true
            }),
            serde_json::json!({}),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120));

        validate_tool_input(
            &tool.contract(),
            &serde_json::json!({
                "path": "README.md",
                "unknown": "preserved"
            }),
        )
        .unwrap();
    }
}
