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

pub fn payload_leaf(value: &serde_json::Value) -> Vec<u8> {
    encode_leaf(value, Position::Payload)
}

pub fn payloads_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    payload_leaf(left) == payload_leaf(right)
}

/// Compares optional JSON through the pre-cutover serialized semantics:
/// absent, `None`, and an explicit JSON `null` are the same opaque leaf.
pub fn optional_payloads_equal(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
) -> bool {
    let null = serde_json::Value::Null;
    payloads_equal(left.unwrap_or(&null), right.unwrap_or(&null))
}

pub fn schema_leaf(value: &serde_json::Value) -> Vec<u8> {
    encode_leaf(value, Position::Schema)
}

#[expect(
    clippy::expect_used,
    reason = "serializing an in-memory serde_json::Value cannot fail"
)]
fn encode_leaf(value: &serde_json::Value, position: Position) -> Vec<u8> {
    serde_json::to_vec(&Canonical { value, position })
        .expect("serializing a JSON value cannot fail")
}

#[derive(Clone, Copy)]
enum Position {
    Payload,
    Schema,
    SchemaMap,
    Dependencies,
    SchemaSequence,
    SchemaOrSequence,
}

/// Normalize at the byte boundary, borrowing the input tree. Identity checks
/// must not build a second JSON tree merely to serialize its canonical leaf.
struct Canonical<'a> {
    value: &'a serde_json::Value,
    position: Position,
}

impl<'a> Canonical<'a> {
    fn object<S: serde::Serializer>(
        serializer: S,
        entries: impl Iterator<Item = (&'a String, &'a serde_json::Value)> + Clone,
        position: Position,
    ) -> Result<S::Ok, S::Error> {
        use Position::*;
        use serde::ser::SerializeMap as _;
        use serde_json::Value;
        let mut map = serializer.serialize_map(Some(entries.clone().count()))?;
        for (key, value) in entries {
            let child = match position {
                Schema => match key.as_str() {
                    "$defs" | "definitions" | "dependentSchemas" | "patternProperties"
                    | "properties" => SchemaMap,
                    "dependencies" => Dependencies,
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
                    | "unevaluatedProperties" => SchemaOrSequence,
                    "allOf" | "anyOf" | "oneOf" | "prefixItems" => SchemaSequence,
                    _ => Payload,
                },
                SchemaMap => Schema,
                Dependencies if matches!(value, Value::Object(_) | Value::Bool(_)) => Schema,
                _ => Payload,
            };
            map.serialize_entry(
                key,
                &Canonical {
                    value,
                    position: child,
                },
            )?;
        }
        map.end()
    }
}

impl serde::Serialize for Canonical<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use Position::*;
        use serde::ser::SerializeSeq as _;
        use serde_json::Value;

        let position = match (self.position, self.value) {
            (SchemaOrSequence, Value::Array(_)) => SchemaSequence,
            (SchemaOrSequence, Value::Object(_) | Value::Bool(_)) => Schema,
            (Schema | SchemaMap | Dependencies, Value::Object(_)) => self.position,
            (SchemaSequence, Value::Array(_)) => SchemaSequence,
            _ => Payload,
        };
        match self.value {
            Value::Object(values) => {
                let entries = values.iter().filter(|(key, _)| {
                    !matches!(position, Schema) || !SCHEMA_ANNOTATIONS.contains(&key.as_str())
                });
                if values.keys().is_sorted() {
                    Self::object(serializer, entries, position)
                } else {
                    let mut entries: Vec<_> = entries.collect();
                    entries.sort_unstable_by_key(|(key, _)| *key);
                    Self::object(serializer, entries.into_iter(), position)
                }
            }
            Value::Array(values) => {
                let child = if matches!(position, SchemaSequence) {
                    Schema
                } else {
                    Payload
                };
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&Canonical {
                        value,
                        position: child,
                    })?;
                }
                sequence.end()
            }
            Value::Number(number) if lash_kernel_value::json_number::is_negative_zero(number) => {
                serializer.serialize_f64(0.0)
            }
            Value::Number(number) => {
                lash_kernel_value::json_number::serialize_canonical(number, serializer)
            }
            value => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opaque payload leaves preserve every field and scalar spelling except
    /// negative zero, with recursive key order independent of input order.
    #[test]
    fn payload_leaf_streams_the_canonical_opaque_grammar() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"z":[{"title":"kept","n":-0.0},1,1.0,-2.5,true,null,"\n"],"a":{"b":2,"a":1}}"#,
        )
        .unwrap();
        assert_eq!(
            payload_leaf(&value),
            br#"{"a":{"a":1,"b":2},"z":[{"n":0.0,"title":"kept"},1,1.0,-2.5,true,null,"\n"]}"#
        );
        for spelling in ["1.00000000000000000000000001", "1e+400", "-1e-400"] {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(spelling)
                && value.to_string() == spelling
            {
                assert_eq!(payload_leaf(&value), spelling.as_bytes());
                assert_eq!(schema_leaf(&value), spelling.as_bytes());
            }
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
