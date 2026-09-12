use super::*;

#[test]
fn strict_projection_records_only_introduced_omission_null_paths() {
    let schema = json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer" },
            "note": { "type": ["string", "null"] },
            "nested": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "count": { "type": "integer" }
                },
                "required": ["id"]
            },
            "rows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "count": { "type": "integer" }
                    },
                    "required": ["id"]
                }
            },
            "referenced": { "$ref": "#/$defs/Referenced" }
        },
        "required": ["nested", "rows", "referenced"],
        "$defs": {
            "Referenced": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "count": { "type": "integer" }
                },
                "required": ["id"]
            }
        }
    });

    let projected = project_strict_tool_parameters(&schema).unwrap();
    let paths = projected
        .omission_null_paths
        .iter()
        .map(|path| path.segments())
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![
            [OmissionNullPathSegment::Property("limit".to_string())].as_slice(),
            [
                OmissionNullPathSegment::Property("nested".to_string()),
                OmissionNullPathSegment::Property("count".to_string()),
            ]
            .as_slice(),
            [
                OmissionNullPathSegment::Property("referenced".to_string()),
                OmissionNullPathSegment::Property("count".to_string()),
            ]
            .as_slice(),
            [
                OmissionNullPathSegment::Property("rows".to_string()),
                OmissionNullPathSegment::ArrayItem,
                OmissionNullPathSegment::Property("count".to_string()),
            ]
            .as_slice(),
        ]
    );
    assert!(
        paths
            .iter()
            .all(|path| !path.contains(&OmissionNullPathSegment::Property("note".to_string())))
    );
}

#[test]
fn strict_projection_leaves_ambiguous_union_descendants_unmapped() {
    let schema = json!({
        "type": "object",
        "properties": {
            "choice": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "const": "left" },
                            "left": { "type": "integer" }
                        },
                        "required": ["kind"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "const": "right" },
                            "right": { "type": "string" }
                        },
                        "required": ["kind"],
                        "additionalProperties": false
                    }
                ]
            }
        },
        "required": ["choice"]
    });

    let projected = project_strict_tool_parameters(&schema).unwrap();

    assert!(projected.omission_null_paths.is_empty());
    assert!(projected.diagnostics.iter().any(|diagnostic| {
        diagnostic.contains("multi-branch anyOf") && diagnostic.contains("omission-null")
    }));
}

#[test]
fn strict_projection_does_not_record_canonically_nullable_local_ref() {
    let schema = json!({
        "type": "object",
        "properties": {
            "note": { "$ref": "#/$defs/Nullable" }
        },
        "$defs": {
            "Nullable": { "type": ["string", "null"] }
        }
    });

    let projected = project_strict_tool_parameters(&schema).unwrap();

    assert!(projected.omission_null_paths.is_empty());
    assert_eq!(projected.schema["required"], json!(["note"]));
}

#[test]
fn strict_projection_leaves_ref_backed_ambiguous_union_descendants_unmapped() {
    let schema = json!({
        "type": "object",
        "properties": {
            "choice": {
                "anyOf": [
                    { "$ref": "#/$defs/Left" },
                    { "$ref": "#/$defs/Right" }
                ]
            }
        },
        "required": ["choice"],
        "$defs": {
            "Left": {
                "type": "object",
                "properties": {
                    "kind": { "const": "left" },
                    "value": { "type": "integer" }
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            "Right": {
                "type": "object",
                "properties": {
                    "kind": { "const": "right" },
                    "value": { "type": ["integer", "null"] }
                },
                "required": ["kind", "value"],
                "additionalProperties": false
            }
        }
    });

    let projected = project_strict_tool_parameters(&schema).unwrap();

    assert!(projected.omission_null_paths.is_empty());
    assert!(projected.diagnostics.iter().any(|diagnostic| {
        diagnostic.contains("multi-branch anyOf") && diagnostic.contains("omission-null")
    }));
}
