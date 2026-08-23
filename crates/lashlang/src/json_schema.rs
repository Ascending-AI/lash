use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::{TypeExpr, TypeField};

const MAX_SCHEMA_DEPTH: usize = 32;

/// Best-effort conversion from JSON Schema into Lashlang's value-type vocabulary.
///
/// JSON Schema is more expressive than [`TypeExpr`]. Unsupported or ambiguous
/// constructs deliberately widen to [`TypeExpr::Dict`] or [`TypeExpr::Any`]
/// instead of rejecting a host-declared capability.
pub fn json_schema_to_type_expr(schema: &Value) -> TypeExpr {
    SchemaImporter { root: schema }.import(schema, 0)
}

struct SchemaImporter<'a> {
    root: &'a Value,
}

impl SchemaImporter<'_> {
    fn import(&self, schema: &Value, depth: usize) -> TypeExpr {
        if depth >= MAX_SCHEMA_DEPTH {
            return TypeExpr::Any;
        }

        let Some(schema) = schema.as_object() else {
            return TypeExpr::Any;
        };
        if schema.is_empty() {
            return TypeExpr::Any;
        }

        if let Some(all_of) = schema.get("allOf") {
            let Some(branches) = all_of.as_array() else {
                return TypeExpr::Any;
            };
            return match branches.as_slice() {
                [branch] => self.import(branch, depth + 1),
                _ => TypeExpr::Any,
            };
        }

        if let Some(reference) = schema.get("$ref") {
            return reference
                .as_str()
                .and_then(|reference| reference.strip_prefix('#'))
                .and_then(|pointer| self.root.pointer(pointer))
                .map_or(TypeExpr::Any, |target| self.import(target, depth + 1));
        }

        let has_any_of = schema.contains_key("anyOf");
        let has_one_of = schema.contains_key("oneOf");
        if has_any_of && has_one_of {
            return TypeExpr::Any;
        }
        if let Some(branches) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
            return self.import_union(branches, depth);
        }

        if let Some(values) = schema.get("enum") {
            return import_enum(values);
        }
        if let Some(value) = schema.get("const") {
            return import_enum(&Value::Array(vec![value.clone()]));
        }

        match schema.get("type") {
            Some(Value::String(kind)) => self.import_type(kind, schema, depth),
            Some(Value::Array(kinds)) => {
                let variants = kinds
                    .iter()
                    .map(|kind| {
                        kind.as_str()
                            .map_or(TypeExpr::Any, |kind| self.import_type(kind, schema, depth))
                    })
                    .collect();
                union_type(variants)
            }
            Some(_) => TypeExpr::Any,
            None if has_object_keywords(schema) => self.import_object(schema, depth),
            None if has_array_keywords(schema) => self.import_array(schema, depth),
            None => TypeExpr::Any,
        }
    }

    fn import_union(&self, branches: &Value, depth: usize) -> TypeExpr {
        let Some(branches) = branches.as_array() else {
            return TypeExpr::Any;
        };
        union_type(
            branches
                .iter()
                .map(|branch| self.import(branch, depth + 1))
                .collect(),
        )
    }

    fn import_type(&self, kind: &str, schema: &Map<String, Value>, depth: usize) -> TypeExpr {
        match kind {
            "string" => TypeExpr::Str,
            "integer" => TypeExpr::Int,
            "number" => TypeExpr::Float,
            "boolean" => TypeExpr::Bool,
            "null" => TypeExpr::Null,
            "object" => self.import_object(schema, depth),
            "array" => self.import_array(schema, depth),
            _ => TypeExpr::Any,
        }
    }

    fn import_object(&self, schema: &Map<String, Value>, depth: usize) -> TypeExpr {
        if schema.contains_key("patternProperties")
            || !matches!(schema.get("additionalProperties"), Some(Value::Bool(false)))
        {
            return TypeExpr::Dict;
        }

        let properties = match schema.get("properties") {
            Some(Value::Object(properties)) => properties,
            Some(_) => return TypeExpr::Dict,
            None => return TypeExpr::Object(Vec::new()),
        };
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|required| {
                required
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        TypeExpr::Object(
            properties
                .iter()
                .map(|(name, property)| TypeField {
                    name: name.as_str().into(),
                    ty: self.import(property, depth + 1),
                    optional: !required.contains(name.as_str()),
                })
                .collect(),
        )
    }

    fn import_array(&self, schema: &Map<String, Value>, depth: usize) -> TypeExpr {
        if schema.contains_key("prefixItems") || schema.get("items").is_some_and(Value::is_array) {
            return TypeExpr::List(Box::new(TypeExpr::Any));
        }
        let item = schema
            .get("items")
            .map_or(TypeExpr::Any, |item| self.import(item, depth + 1));
        TypeExpr::List(Box::new(item))
    }
}

fn has_object_keywords(schema: &Map<String, Value>) -> bool {
    [
        "properties",
        "required",
        "additionalProperties",
        "patternProperties",
    ]
    .iter()
    .any(|key| schema.contains_key(*key))
}

fn has_array_keywords(schema: &Map<String, Value>) -> bool {
    schema.contains_key("items") || schema.contains_key("prefixItems")
}

fn import_enum(values: &Value) -> TypeExpr {
    let Some(values) = values.as_array().filter(|values| !values.is_empty()) else {
        return TypeExpr::Any;
    };
    let mut strings = Vec::new();
    let mut has_null = false;
    for value in values {
        match value {
            Value::String(value) => strings.push(value.as_str().into()),
            Value::Null => has_null = true,
            _ => return TypeExpr::Any,
        }
    }

    let mut variants = Vec::new();
    if !strings.is_empty() {
        variants.push(TypeExpr::Enum(strings));
    }
    if has_null {
        variants.push(TypeExpr::Null);
    }
    union_type(variants)
}

fn union_type(variants: Vec<TypeExpr>) -> TypeExpr {
    let mut flattened = Vec::new();
    for variant in variants {
        match variant {
            TypeExpr::Any => return TypeExpr::Any,
            TypeExpr::Union(nested) => flattened.extend(nested),
            variant => flattened.push(variant),
        }
    }
    let mut unique = Vec::new();
    for variant in flattened {
        if !unique.contains(&variant) {
            unique.push(variant);
        }
    }
    match unique.len() {
        // Unlike the linker's empty-list element union, this is the
        // JSON-Schema importer domain: an empty `anyOf` widens to `Any`.
        // The divergence is intentional and pinned separately (FIG-1878).
        0 => TypeExpr::Any,
        1 => unique.pop().expect("single union variant"),
        _ => TypeExpr::Union(unique),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn field(name: &str, ty: TypeExpr, optional: bool) -> TypeField {
        TypeField {
            name: name.into(),
            ty,
            optional,
        }
    }

    #[test]
    fn imports_empty_schema_as_any() {
        assert_eq!(json_schema_to_type_expr(&json!({})), TypeExpr::Any);
        assert_eq!(json_schema_to_type_expr(&Value::Null), TypeExpr::Any);
        assert_eq!(json_schema_to_type_expr(&Value::Bool(true)), TypeExpr::Any);
    }

    #[test]
    fn imports_primitive_types_and_drops_refinements() {
        for (schema, expected) in [
            (
                json!({ "type": "string", "minLength": 2, "maxLength": 8, "pattern": "x", "format": "email" }),
                TypeExpr::Str,
            ),
            (
                json!({ "type": "integer", "minimum": 1, "maximum": 5 }),
                TypeExpr::Int,
            ),
            (
                json!({ "type": "number", "exclusiveMinimum": 0 }),
                TypeExpr::Float,
            ),
            (json!({ "type": "boolean" }), TypeExpr::Bool),
            (json!({ "type": "null" }), TypeExpr::Null),
        ] {
            assert_eq!(json_schema_to_type_expr(&schema), expected);
        }
    }

    #[test]
    fn imports_closed_object_properties_and_required_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        });
        assert_eq!(
            json_schema_to_type_expr(&schema),
            TypeExpr::Object(vec![
                field("count", TypeExpr::Int, true),
                field("name", TypeExpr::Str, false),
            ])
        );
    }

    #[test]
    fn imports_arrays_and_missing_items() {
        assert_eq!(
            json_schema_to_type_expr(&json!({ "type": "array", "items": { "type": "string" } })),
            TypeExpr::List(Box::new(TypeExpr::Str))
        );
        assert_eq!(
            json_schema_to_type_expr(&json!({ "type": "array" })),
            TypeExpr::List(Box::new(TypeExpr::Any))
        );
    }

    #[test]
    fn imports_string_enums_and_representable_singleton_unions() {
        assert_eq!(
            json_schema_to_type_expr(&json!({ "enum": ["fast", "safe"] })),
            TypeExpr::Enum(vec!["fast".into(), "safe".into()])
        );
        assert_eq!(
            json_schema_to_type_expr(&json!({ "enum": ["ready", null] })),
            TypeExpr::Union(vec![TypeExpr::Enum(vec!["ready".into()]), TypeExpr::Null])
        );
    }

    #[test]
    fn imports_nullable_type_arrays_and_union_keywords() {
        assert_eq!(
            json_schema_to_type_expr(&json!({ "type": ["string", "null"] })),
            TypeExpr::Union(vec![TypeExpr::Str, TypeExpr::Null])
        );
        for keyword in ["anyOf", "oneOf"] {
            let schema = json!({ (keyword): [{ "type": "integer" }, { "type": "null" }] });
            assert_eq!(
                json_schema_to_type_expr(&schema),
                TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Null])
            );
        }
    }

    #[test]
    fn open_and_patterned_objects_degrade_to_dict() {
        for schema in [
            json!({ "type": "object", "properties": { "name": { "type": "string" } } }),
            json!({ "type": "object", "additionalProperties": true }),
            json!({ "type": "object", "additionalProperties": { "type": "string" } }),
            json!({ "type": "object", "patternProperties": { "^x": { "type": "integer" } }, "additionalProperties": false }),
        ] {
            assert_eq!(json_schema_to_type_expr(&schema), TypeExpr::Dict);
        }
    }

    #[test]
    fn tuple_arrays_degrade_to_list_any() {
        for schema in [
            json!({ "type": "array", "prefixItems": [{ "type": "string" }] }),
            json!({ "type": "array", "items": [{ "type": "string" }, { "type": "integer" }] }),
        ] {
            assert_eq!(
                json_schema_to_type_expr(&schema),
                TypeExpr::List(Box::new(TypeExpr::Any))
            );
        }
    }

    #[test]
    fn intersections_and_non_string_enums_degrade_to_any() {
        for schema in [
            json!({ "allOf": [{ "type": "string" }, { "maxLength": 8 }] }),
            json!({ "enum": [1, 2] }),
            json!({ "enum": ["one", 2] }),
        ] {
            assert_eq!(json_schema_to_type_expr(&schema), TypeExpr::Any);
        }
        assert_eq!(
            json_schema_to_type_expr(&json!({ "allOf": [{ "type": "string", "pattern": "x" }] })),
            TypeExpr::Str
        );
    }

    #[test]
    fn local_refs_resolve_and_recursive_refs_hit_the_depth_cap() {
        let direct = json!({
            "$defs": { "Name": { "type": "string" } },
            "$ref": "#/$defs/Name"
        });
        assert_eq!(json_schema_to_type_expr(&direct), TypeExpr::Str);

        let recursive = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "next": { "$ref": "#/$defs/Node" } },
                    "required": ["next"],
                    "additionalProperties": false
                }
            },
            "$ref": "#/$defs/Node"
        });
        let mut imported = json_schema_to_type_expr(&recursive);
        for _ in 0..MAX_SCHEMA_DEPTH {
            match imported {
                TypeExpr::Object(mut fields) => imported = fields.remove(0).ty,
                TypeExpr::Any => return,
                other => panic!("recursive ref widened to unexpected type: {other:?}"),
            }
        }
        panic!("recursive ref did not widen to any at the depth cap");
    }

    #[test]
    fn malformed_and_external_refs_degrade_without_errors() {
        for schema in [
            json!({ "$ref": "https://example.com/schema.json" }),
            json!({ "$ref": "#/$defs/Missing" }),
            json!({ "anyOf": "not-an-array" }),
            json!({ "type": "unknown" }),
        ] {
            assert_eq!(json_schema_to_type_expr(&schema), TypeExpr::Any);
        }
    }

    #[test]
    fn empty_any_of_imports_as_any() {
        assert_eq!(
            json_schema_to_type_expr(&json!({ "anyOf": [] })),
            TypeExpr::Any
        );
    }
}
