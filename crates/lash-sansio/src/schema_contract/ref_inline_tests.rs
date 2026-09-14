use super::*;

fn ref_with_sibling_description_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "polarity": {
                "$ref": "#/$defs/Polarity",
                "description": "how the caller feels about it"
            }
        },
        "required": ["polarity"],
        "$defs": {
            "Polarity": {
                "type": "string",
                "enum": ["positive", "negative"],
                "description": "canonical polarity"
            }
        }
    })
}

#[test]
fn structured_output_inlines_ref_carrying_sibling_keywords() {
    let projected = project_structured_output(&ref_with_sibling_description_schema()).unwrap();
    let property = &projected.schema["properties"]["polarity"];
    assert!(property.get("$ref").is_none(), "{property}");
    assert_eq!(property["type"], json!("string"));
    assert_eq!(property["enum"], json!(["positive", "negative"]));
    assert_eq!(
        property["description"],
        json!("how the caller feels about it")
    );
    assert!(
        projected
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("$.properties.polarity")
                && diagnostic.contains("inlined `$ref` `#/$defs/Polarity`")
                && diagnostic.contains("description")),
        "{:?}",
        projected.diagnostics
    );
}

#[test]
fn strict_tool_parameters_inlines_ref_carrying_sibling_keywords() {
    let projected = project_strict_tool_parameters(&ref_with_sibling_description_schema()).unwrap();
    let property = &projected.schema["properties"]["polarity"];
    assert!(property.get("$ref").is_none(), "{property}");
    assert_eq!(property["type"], json!("string"));
    assert_eq!(
        property["description"],
        json!("how the caller feels about it")
    );
    assert!(
        projected
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("inlined `$ref` `#/$defs/Polarity`")),
        "{:?}",
        projected.diagnostics
    );
}

#[test]
fn projection_leaves_a_bare_ref_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": {
            "polarity": { "$ref": "#/$defs/Polarity" }
        },
        "required": ["polarity"],
        "$defs": {
            "Polarity": { "type": "string", "enum": ["positive", "negative"] }
        }
    });

    for projected in [
        project_structured_output(&schema).unwrap(),
        project_strict_tool_parameters(&schema).unwrap(),
    ] {
        assert_eq!(
            projected.schema["properties"]["polarity"],
            json!({ "$ref": "#/$defs/Polarity" })
        );
        assert!(
            !projected
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("inlined `$ref`")),
            "{:?}",
            projected.diagnostics
        );
    }
}

#[test]
fn projection_bounds_a_cyclic_ref_carrying_sibling_keywords() {
    let schema = json!({
        "type": "object",
        "properties": {
            "node": {
                "$ref": "#/$defs/Node",
                "description": "the tree root"
            }
        },
        "required": ["node"],
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "child": { "$ref": "#/$defs/Node" }
                },
                "required": ["child"]
            }
        }
    });

    for projected in [
        project_structured_output(&schema).unwrap(),
        project_strict_tool_parameters(&schema).unwrap(),
    ] {
        let property = &projected.schema["properties"]["node"];
        assert!(property.get("$ref").is_none(), "{property}");
        assert_eq!(property["description"], json!("the tree root"));
        assert_eq!(property["properties"]["child"], json!({}));
        assert!(
            projected.diagnostics.iter().any(|diagnostic| {
                diagnostic.contains("$.properties.node")
                    && diagnostic.contains("truncated recursive `$ref` `#/$defs/Node`")
            }),
            "{:?}",
            projected.diagnostics
        );
    }
}

#[test]
fn projection_rejects_an_unresolvable_ref_carrying_sibling_keywords() {
    let err = project_structured_output(&json!({
        "type": "object",
        "properties": {
            "polarity": {
                "$ref": "https://example.invalid/Polarity",
                "description": "remote"
            }
        },
        "required": ["polarity"]
    }))
    .unwrap_err();
    assert!(
        err.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("could not be resolved")),
        "{:?}",
        err.diagnostics
    );
}
