//! Faithful schemas for the entry JSON wire profile. Numbers on this boundary
//! are finite JSON numbers; optional values are omitted, never made nullable.

use crate::{JsonSchema, RecordType, RecordTypeField, Signature, Type};
use serde_json::{Value, json};

/// JSON Schema draft 2020-12 is fixed. Hosts may narrow or raise the traversal
/// limits; defaults match the schema projector (64 levels, 4096 nodes).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EntrySchemaPolicy {
    pub max_depth: usize,
    pub max_nodes: usize,
    /// Normalize only a settled result's Absent (including union arms) to null.
    /// This is the existing TypeScript entry transport convention.
    pub settled_result_absent_as_null: bool,
}

impl Default for EntrySchemaPolicy {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_nodes: crate::schema::MAX_PROJECTED_TYPE_NODES,
            settled_result_absent_as_null: true,
        }
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TypeSchemaRefusalReason {
    UnsupportedType,
    Limit,
    EncodingRequired,
}

/// A path through the type structure with RFC 6901 escaped components, the
/// type there, and a closed refusal reason. `*` denotes a collection member.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TypeSchemaRefusal {
    pub path: String,
    pub ty: Type,
    pub reason: TypeSchemaRefusalReason,
}

impl std::fmt::Display for TypeSchemaRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "type schema refused at {}: {:?}", self.path, self.reason)
    }
}
impl std::error::Error for TypeSchemaRefusal {}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EntrySchemas {
    pub input: JsonSchema,
    pub output: JsonSchema,
}

/// Publish a type under the finite-number JSON wire profile. Absence has no
/// standalone JSON encoding. Bytes are lowercase hex; timestamps decimal ns.
///
/// # Errors
/// Refuses types without a faithful encoding and traversals past the policy.
pub fn json_schema_of_type(
    ty: &Type,
    policy: EntrySchemaPolicy,
) -> Result<JsonSchema, TypeSchemaRefusal> {
    let mut writer = Writer { policy, nodes: 0 };
    let value = writer.schema(ty, "", 0, false)?;
    admit(value, ty, "")
}

/// Publish a closed input record of public parameter names and the settled
/// result. Only the latter uses the policy's absence convention.
///
/// # Errors
/// Refuses unsupported types and traversals past the policy.
pub fn entry_schemas(
    signature: &Signature,
    policy: EntrySchemaPolicy,
) -> Result<EntrySchemas, TypeSchemaRefusal> {
    let input = Type::Record(RecordType {
        fields: signature
            .params
            .iter()
            .map(|param| RecordTypeField {
                name: param.name.to_string(),
                ty: param.ty.clone(),
                optional: param.optional,
            })
            .collect(),
        rest: None,
    });
    let mut writer = Writer { policy, nodes: 0 };
    let input_schema = writer.schema(&input, "/input", 0, false)?;
    let output_schema = writer.schema(
        &signature.result,
        "/output",
        0,
        policy.settled_result_absent_as_null,
    )?;
    Ok(EntrySchemas {
        input: admit(input_schema, &input, "/input")?,
        output: admit(output_schema, &signature.result, "/output")?,
    })
}

fn refusal(ty: &Type, path: &str, reason: TypeSchemaRefusalReason) -> TypeSchemaRefusal {
    TypeSchemaRefusal {
        path: path.into(),
        ty: ty.clone(),
        reason,
    }
}

fn admit(mut schema: Value, ty: &Type, path: &str) -> Result<JsonSchema, TypeSchemaRefusal> {
    schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    JsonSchema::admit(schema)
        .map_err(|_| refusal(ty, path, TypeSchemaRefusalReason::UnsupportedType))
}

fn child(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

struct Writer {
    policy: EntrySchemaPolicy,
    nodes: usize,
}

impl Writer {
    fn schema(
        &mut self,
        ty: &Type,
        path: &str,
        depth: usize,
        settled: bool,
    ) -> Result<Value, TypeSchemaRefusal> {
        if depth >= self.policy.max_depth || self.nodes >= self.policy.max_nodes {
            return Err(refusal(ty, path, TypeSchemaRefusalReason::Limit));
        }
        self.nodes += 1;
        Ok(match ty {
            Type::Json => json!({}),
            Type::Null => json!({"type": "null"}),
            Type::Absent if settled => json!({"type": "null"}),
            Type::Absent => {
                return Err(refusal(ty, path, TypeSchemaRefusalReason::EncodingRequired));
            }
            Type::Bool => json!({"type": "boolean"}),
            Type::Int => json!({"type": "integer"}),
            Type::Float | Type::Number => json!({"type": "number"}),
            Type::Text => json!({"type": "string"}),
            Type::Bytes => {
                json!({"type": "string", "pattern": "^(?:[0-9a-f]{2})*$", "x-lash": {"kind": "bytes"}})
            }
            Type::Timestamp => {
                json!({"type": "string", "pattern": "^-?[0-9]+$", "x-lash": {"kind": "timestamp"}})
            }
            Type::Enum(values) if !values.is_empty() => json!({"type": "string", "enum": values}),
            Type::List(item) => {
                json!({"type": "array", "items": self.schema(item, &child(path, "*"), depth + 1, false)?})
            }
            Type::Tuple(items) => {
                let members = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.schema(item, &child(path, &index.to_string()), depth + 1, false)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if members.is_empty() {
                    json!({"type": "array", "items": false, "minItems": 0, "maxItems": 0})
                } else {
                    json!({"type": "array", "prefixItems": members, "items": false, "minItems": items.len(), "maxItems": items.len()})
                }
            }
            Type::Record(record) => {
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();
                for field in &record.fields {
                    if properties.contains_key(&field.name) {
                        return Err(refusal(ty, path, TypeSchemaRefusalReason::UnsupportedType));
                    }
                    properties.insert(
                        field.name.clone(),
                        self.schema(&field.ty, &child(path, &field.name), depth + 1, false)?,
                    );
                    if !field.optional {
                        required.push(field.name.clone());
                    }
                }
                let rest = match record.rest.as_deref() {
                    None => Value::Bool(false),
                    Some(Type::Json) => Value::Bool(true),
                    Some(rest) => self.schema(rest, &child(path, "*"), depth + 1, false)?,
                };
                json!({"type": "object", "properties": properties, "required": required, "additionalProperties": rest})
            }
            Type::Union(members) if !members.is_empty() => {
                json!({"anyOf": members.iter().enumerate().map(|(index, member)| self.schema(member, &child(path, &index.to_string()), depth + 1, settled)).collect::<Result<Vec<_>, _>>()?})
            }
            Type::Any
            | Type::Map(_)
            | Type::Set(_)
            | Type::Function(_)
            | Type::Task(_)
            | Type::Error
            | Type::Handle(_)
            | Type::Enum(_)
            | Type::Union(_) => {
                return Err(refusal(ty, path, TypeSchemaRefusalReason::UnsupportedType));
            }
        })
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::{
        MapType, Name, Param,
        schema::{WideningReason, type_of_schema},
        type_relations::type_mismatch,
    };

    /// ENTRY-CONTAINMENT: event <: parameter; closed annotations retain closure.
    #[test]
    fn event_containment_preserves_variance_and_open_records() {
        let event = Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "kind".into(),
                ty: Type::Enum(vec!["answer".into()]),
                optional: false,
            }],
            rest: None,
        });
        let parameter = Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "kind".into(),
                ty: Type::Text,
                optional: false,
            }],
            rest: Some(Box::new(Type::Json)),
        });
        assert_eq!(type_mismatch(&event, &parameter), None);
        assert_eq!(type_mismatch(&event, &Type::Json), None);
        let mismatch = type_mismatch(&parameter, &event).unwrap();
        assert_eq!(mismatch.path, "/input/properties/kind");
        assert_eq!(mismatch.expected, Type::Enum(vec!["answer".into()]));
        assert_eq!(mismatch.provided, Type::Text);
        let Type::Record(mut open) = event.clone() else {
            unreachable!()
        };
        open.rest = Some(Box::new(Type::Json));
        assert_eq!(
            type_mismatch(&Type::Record(open), &event).unwrap().path,
            "/input/additionalProperties"
        );
        let mut optional = event.clone();
        if let Type::Record(record) = &mut optional {
            record.fields[0].optional = true;
        }
        assert!(type_mismatch(&optional, &event).is_some());
        assert!(type_mismatch(&event, &optional).is_none());
        assert!(type_mismatch(&Type::Union(vec![event.clone(), Type::Null]), &parameter).is_some());
    }

    /// ENTRY-SCHEMA-ROUNDTRIP: declared structure and optionality survive the
    /// wire profile; values are validated by the generated schemas too.
    #[test]
    fn supported_entry_schemas_round_trip_and_validate_the_wire_profile() {
        let record = Type::Record(RecordType {
            fields: vec![
                RecordTypeField {
                    name: "a".into(),
                    ty: Type::Tuple(vec![Type::Text, Type::Int]),
                    optional: false,
                },
                RecordTypeField {
                    name: "b".into(),
                    ty: Type::List(Box::new(Type::Bool)),
                    optional: true,
                },
            ],
            rest: None,
        });
        for ty in [
            Type::Json,
            Type::Null,
            Type::Bool,
            Type::Int,
            Type::Number,
            Type::Text,
            Type::Bytes,
            Type::Tuple(vec![]),
            Type::Timestamp,
            Type::Enum(vec!["a".into(), "b".into()]),
            record.clone(),
            Type::Record(RecordType {
                fields: vec![],
                rest: Some(Box::new(Type::Json)),
            }),
            Type::Union(vec![Type::Text, Type::Null]),
        ] {
            let schema = json_schema_of_type(&ty, EntrySchemaPolicy::default()).unwrap();
            assert_eq!(type_of_schema(schema.as_value()).unwrap(), (ty, vec![]));
        }
        let signature = Signature {
            params: vec![Param {
                name: Name::new("event"),
                ty: record,
                optional: true,
            }],
            result: Type::Union(vec![Type::Text, Type::Absent]),
        };
        let schemas = entry_schemas(&signature, EntrySchemaPolicy::default()).unwrap();
        assert!(schemas.input.validate(&json!({})).is_ok());
        assert!(
            schemas
                .input
                .validate(&json!({"event": {"a": ["s", 3]}}))
                .is_ok()
        );
        for value in [
            json!({"event": null}),
            json!({"event": {"a": ["s"]}}),
            json!({"event": {"a": ["s", 3, 4]}}),
            json!({"other": 1}),
        ] {
            assert!(schemas.input.validate(&value).is_err(), "{value}");
        }
        assert!(schemas.output.validate(&Value::Null).is_ok());
        assert!(schemas.output.validate(&json!(3)).is_err());
        assert_eq!(
            type_of_schema(schemas.output.as_value()).unwrap().0,
            Type::Union(vec![Type::Text, Type::Null])
        );
        assert_eq!(
            json_schema_of_type(&Type::Absent, EntrySchemaPolicy::default())
                .unwrap_err()
                .reason,
            TypeSchemaRefusalReason::EncodingRequired
        );
        let mut nested = signature;
        nested.result = Type::List(Box::new(Type::Absent));
        assert_eq!(
            entry_schemas(&nested, EntrySchemaPolicy::default())
                .unwrap_err()
                .path,
            "/output/*"
        );
        assert!(
            entry_schemas(
                &Signature {
                    params: vec![],
                    result: Type::Absent
                },
                EntrySchemaPolicy {
                    settled_result_absent_as_null: false,
                    ..Default::default()
                }
            )
            .is_err()
        );
        let bytes = json_schema_of_type(&Type::Bytes, EntrySchemaPolicy::default()).unwrap();
        assert!(bytes.validate(&json!("00af")).is_ok());
        assert!(bytes.validate(&json!("AF")).is_err());
    }

    /// ENTRY-SCHEMA-REFUSAL: no plausible encoding is substituted for an
    /// unsupported type, and the host's limits name the refused position.
    #[test]
    fn unsupported_entry_types_and_host_bounds_are_typed_refusals() {
        for ty in [
            Type::Any,
            Type::Map(Box::new(MapType {
                key: Type::Text,
                value: Type::Json,
            })),
            Type::Set(Box::new(Type::Int)),
            Type::Task(Box::new(Type::Text)),
            Type::Handle("file".into()),
            Type::Error,
            Type::Function(Box::new(Signature {
                params: vec![],
                result: Type::Text,
            })),
        ] {
            assert_eq!(
                json_schema_of_type(&ty, EntrySchemaPolicy::default()).unwrap_err(),
                TypeSchemaRefusal {
                    path: "".into(),
                    ty,
                    reason: TypeSchemaRefusalReason::UnsupportedType
                }
            );
        }
        let ty = Type::List(Box::new(Type::Text));
        for policy in [
            EntrySchemaPolicy {
                max_depth: 1,
                ..Default::default()
            },
            EntrySchemaPolicy {
                max_nodes: 1,
                ..Default::default()
            },
        ] {
            assert_eq!(
                json_schema_of_type(&ty, policy).unwrap_err(),
                TypeSchemaRefusal {
                    path: "/*".into(),
                    ty: Type::Text,
                    reason: TypeSchemaRefusalReason::Limit
                }
            );
        }
    }

    /// ENTRY-SCHEMA-EVIDENCE: references resolve at the original root;
    /// recursion, unsupported constraints and budgets never prove a fit.
    #[test]
    fn schema_losses_keep_their_paths_and_reasons() {
        let schema = json!({"$defs": {"E": {"type": "string"}}, "type": "array", "items": {"$ref": "#/$defs/E"}});
        assert_eq!(
            type_of_schema(&schema).unwrap(),
            (Type::List(Box::new(Type::Text)), vec![])
        );
        assert_eq!(
            type_of_schema(&json!({"$ref": "#"})).unwrap().1[0].reason,
            WideningReason::Cycle
        );
        let schema =
            json!({"type": "object", "properties": {"a/b": {"type": "string", "minLength": 1}}});
        assert_eq!(
            type_of_schema(&schema).unwrap().1[0].schema_path,
            "/properties/a~1b/minLength"
        );
        let mut deep = json!({"type": "string"});
        for _ in 0..64 {
            deep = json!({"type": "array", "items": deep});
        }
        assert_eq!(
            type_of_schema(&deep).unwrap().1[0].reason,
            WideningReason::Budget
        );
    }
}
