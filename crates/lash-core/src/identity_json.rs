//! Canonical opaque leaves for arbitrary JSON carried by durable definitions.
//!
//! Identity families frame each JSON-bearing field as one byte leaf. They do
//! not admit JSON arrays, objects, numbers, or future object members into the
//! family grammar. A byte change in a payload leaf is an executable definition
//! conflict. Schema leaves additionally discard JSON Schema annotations that
//! do not affect validation; a byte change after that reduction is a schema
//! definition conflict.
//!
//! Leaf bytes pin `serde_json`'s number rendering as part of the family
//! grammar. A dependency upgrade that changes that rendering requires a family
//! version bump and refreshed goldens rather than silent identity drift.

const SCHEMA_ANNOTATIONS: &[&str] = &[
    "$comment",
    "default",
    "deprecated",
    "description",
    "examples",
    "readOnly",
    "title",
    "writeOnly",
];

pub(crate) fn payload_leaf(value: &serde_json::Value) -> Vec<u8> {
    encode_leaf(normalize_payload(value))
}

pub(crate) fn payloads_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    payload_leaf(left) == payload_leaf(right)
}

/// Compares optional JSON through the pre-cutover serialized semantics:
/// absent, `None`, and an explicit JSON `null` are the same opaque leaf.
pub(crate) fn optional_payloads_equal(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
) -> bool {
    let null = serde_json::Value::Null;
    payloads_equal(left.unwrap_or(&null), right.unwrap_or(&null))
}

pub(crate) fn schema_leaf(value: &serde_json::Value) -> Vec<u8> {
    encode_leaf(normalize_schema(value))
}

fn encode_leaf(normalized: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&normalized).expect("serializing a JSON value cannot fail")
}

fn normalize_payload(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            value.clone()
        }
        serde_json::Value::Number(number) => {
            if number.as_f64() == Some(0.0) && number.to_string().starts_with('-') {
                serde_json::Value::Number(
                    serde_json::Number::from_f64(0.0).expect("zero is a finite JSON number"),
                )
            } else {
                value.clone()
            }
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(normalize_payload).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let mut normalized = serde_json::Map::new();
            for (key, value) in entries {
                normalized.insert(key.clone(), normalize_payload(value));
            }
            serde_json::Value::Object(normalized)
        }
    }
}

fn normalize_schema(value: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(values) = value else {
        return normalize_payload(value);
    };
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    let mut normalized = serde_json::Map::new();
    for (key, value) in entries {
        if SCHEMA_ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }
        // Keep this schema-position map aligned with the Draft-7 semantics
        // compiled by lash-sansio's `jsonschema::JSONSchema::compile` call.
        // Draft-7 schema positions are definitions, properties,
        // patternProperties, object-valued dependencies, additionalProperties,
        // propertyNames, items (schema or tuple), additionalItems, contains,
        // allOf/anyOf/oneOf, not, and if/then/else. Later-draft positions that
        // the dependency already accepts are reduced here as well.
        let value = match key.as_str() {
            "$defs" | "definitions" | "dependentSchemas" | "patternProperties" | "properties" => {
                normalize_schema_map(value)
            }
            "dependencies" => normalize_draft7_dependencies(value),
            "additionalItems"
            | "additionalProperties"
            | "contains"
            | "contentSchema"
            | "else"
            | "if"
            | "items"
            | "not"
            | "propertyNames"
            | "then"
            | "unevaluatedItems"
            | "unevaluatedProperties" => normalize_schema_or_sequence(value),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => normalize_schema_sequence(value),
            // These keywords contain instance values, not nested schemas.
            "const" | "enum" => normalize_payload(value),
            // Constraints such as type, required, minimum, and format contain
            // primitives or instance-name collections. Unknown future
            // keywords remain opaque rather than silently becoming schema
            // grammar under this family version.
            _ => normalize_payload(value),
        };
        normalized.insert(key.clone(), value);
    }
    serde_json::Value::Object(normalized)
}

fn normalize_draft7_dependencies(value: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(values) = value else {
        return normalize_payload(value);
    };
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    let mut normalized = serde_json::Map::new();
    for (key, value) in entries {
        let value = match value {
            // Draft-7 dependency schemas occupy schema positions. Arrays are
            // property-name lists and must remain identity-bearing instance
            // data rather than being traversed as tuple schemas.
            serde_json::Value::Object(_) | serde_json::Value::Bool(_) => normalize_schema(value),
            _ => normalize_payload(value),
        };
        normalized.insert(key.clone(), value);
    }
    serde_json::Value::Object(normalized)
}

fn normalize_schema_map(value: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(values) = value else {
        return normalize_payload(value);
    };
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    let mut normalized = serde_json::Map::new();
    for (key, value) in entries {
        normalized.insert(key.clone(), normalize_schema(value));
    }
    serde_json::Value::Object(normalized)
}

fn normalize_schema_sequence(value: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Array(values) = value else {
        return normalize_payload(value);
    };
    serde_json::Value::Array(values.iter().map(normalize_schema).collect())
}

fn normalize_schema_or_sequence(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(_) => normalize_schema_sequence(value),
        serde_json::Value::Object(_) | serde_json::Value::Bool(_) => normalize_schema(value),
        _ => normalize_payload(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_leaf_has_pinned_feature_independent_bytes() {
        let first =
            serde_json::from_str(r#"{"b":-0.0,"a":[true,null]}"#).expect("parse payload leaf");
        let second = serde_json::from_str(r#"{"a":[true,null],"b":0.0}"#)
            .expect("parse reordered payload leaf");
        assert_eq!(payload_leaf(&first), br#"{"a":[true,null],"b":0.0}"#);
        assert_eq!(payload_leaf(&first), payload_leaf(&second));
        assert!(payloads_equal(&first, &second));
        assert!(optional_payloads_equal(
            None,
            Some(&serde_json::Value::Null)
        ));
    }

    #[test]
    fn payload_equality_numeric_relationships_are_the_contract() {
        let parse = |json: &str| serde_json::from_str(json).expect("parse contract payload");
        let cases = vec![
            ("signed zero", parse("-0.0"), parse("0.0"), true),
            (
                "integer and float",
                serde_json::json!(1),
                serde_json::json!(1.0),
                false,
            ),
            (
                "same signed and unsigned integer",
                serde_json::Value::Number(serde_json::Number::from(i64::MAX)),
                serde_json::Value::Number(serde_json::Number::from(i64::MAX as u64)),
                true,
            ),
            (
                "large unsigned integer and float",
                serde_json::json!(u64::MAX),
                serde_json::json!(u64::MAX as f64),
                false,
            ),
            ("exponent and plain float", parse("1e0"), parse("1.0"), true),
            (
                "nested signed zero and object order",
                parse(r#"{"outer":[{"zero":-0.0,"count":1}]}"#),
                parse(r#"{"outer":[{"count":1,"zero":0.0}]}"#),
                true,
            ),
            (
                "nested integer and float",
                parse(r#"{"outer":[1]}"#),
                parse(r#"{"outer":[1.0]}"#),
                false,
            ),
        ];

        // This table is the durable canonical numeric equality contract. Any
        // changed relationship requires a family version bump and new goldens.
        for (name, left, right, expected) in cases {
            assert_eq!(payloads_equal(&left, &right), expected, "{name}");
            assert_eq!(payloads_equal(&right, &left), expected, "{name} reversed");
        }
        assert_eq!(payload_leaf(&parse("1e0")), b"1.0");

        let null = serde_json::Value::Null;
        let one = serde_json::json!(1);
        let optional_cases = [
            ("absent and absent", None, None, true),
            ("absent and null", None, Some(&null), true),
            ("null and absent", Some(&null), None, true),
            ("null and value", Some(&null), Some(&one), false),
        ];
        for (name, left, right, expected) in optional_cases {
            assert_eq!(optional_payloads_equal(left, right), expected, "{name}");
        }
    }

    #[test]
    fn schema_leaf_excludes_non_executable_annotations() {
        let annotated = serde_json::json!({
            "title": "display only",
            "description": "display only",
            "type": "object",
            "properties": {
                "value": {"type": "string", "description": "display only"}
            },
            "required": ["value"]
        });
        let semantic = serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        });
        assert_eq!(schema_leaf(&annotated), schema_leaf(&semantic));
        assert_eq!(
            schema_leaf(&semantic),
            br#"{"properties":{"value":{"type":"string"}},"required":["value"],"type":"object"}"#
        );

        let title_property = serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"]
        });
        let no_properties = serde_json::json!({"type": "object"});
        assert_ne!(schema_leaf(&title_property), schema_leaf(&no_properties));

        let title_in_const = serde_json::json!({"const": {"title": "identity-bearing value"}});
        let empty_const = serde_json::json!({"const": {}});
        assert_ne!(schema_leaf(&title_in_const), schema_leaf(&empty_const));
    }

    #[test]
    fn schema_leaf_reduces_draft7_dependency_schemas_but_not_property_lists() {
        let annotated = serde_json::json!({
            "dependencies": {
                "credit_card": {
                    "title": "display only",
                    "description": "display only",
                    "required": ["billing_address"]
                },
                "billing_address": ["credit_card"]
            }
        });
        let semantic = serde_json::json!({
            "dependencies": {
                "credit_card": {"required": ["billing_address"]},
                "billing_address": ["credit_card"]
            }
        });
        assert_eq!(schema_leaf(&annotated), schema_leaf(&semantic));

        let changed_property_list = serde_json::json!({
            "dependencies": {
                "credit_card": {"required": ["billing_address"]},
                "billing_address": ["postal_code"]
            }
        });
        assert_ne!(schema_leaf(&semantic), schema_leaf(&changed_property_list));
    }
}
