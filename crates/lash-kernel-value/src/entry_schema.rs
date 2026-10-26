//! Faithful schemas for the entry JSON wire profile. Numbers on this boundary
//! are finite JSON numbers; optional values are omitted, never made nullable.

use crate::{JsonSchema, RecordType, RecordTypeField, Signature, Type};
use serde_json::{Value, json};

/// Why an event schema could not be proved to fit an entry parameter.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EventContainmentRefusal {
    /// The schema has no representable projection; retains its schema path
    /// and typed conflict.
    #[error(transparent)]
    Projection(#[from] crate::schema::SchemaProjectionRefusal),
    /// The projected event does not fit; retains the type path and both types.
    #[error("event does not fit entry parameter: {0}")]
    Mismatch(crate::SignatureMismatch),
}

/// Prove that every value of an event schema fits an entry parameter.
/// Projection only widens: if its superset fits, every event value fits too.
/// Widenings therefore do not refuse this direction of containment.
///
/// # Errors
/// Returns a typed projection refusal or the first mismatch, rooted at
/// `/input`, with the expected parameter and provided event types there.
pub fn event_fits_entry_parameter(
    event_schema: &Value,
    parameter: &Type,
) -> Result<(), EventContainmentRefusal> {
    let (event, _) = crate::schema::type_of_schema(event_schema)?;
    match crate::type_relations::type_mismatch(&event, parameter) {
        Some(mismatch) => Err(EventContainmentRefusal::Mismatch(mismatch)),
        None => Ok(()),
    }
}

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
        rest: crate::RecordRest::Closed,
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
                let mut schema =
                    json!({"type": "object", "properties": properties, "required": required});
                match &record.rest {
                    crate::RecordRest::Closed => {
                        schema["additionalProperties"] = Value::Bool(false)
                    }
                    crate::RecordRest::Drop => {}
                    crate::RecordRest::Keep(rest) => {
                        schema["additionalProperties"] = if **rest == Type::Json {
                            Value::Bool(true)
                        } else {
                            self.schema(rest, &child(path, "*"), depth + 1, false)?
                        };
                    }
                }
                schema
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
            rest: crate::RecordRest::Closed,
        });
        let parameter = Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "kind".into(),
                ty: Type::Text,
                optional: false,
            }],
            rest: crate::RecordRest::Keep(Box::new(Type::Json)),
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
        open.rest = crate::RecordRest::Keep(Box::new(Type::Json));
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

    /// ENTRY-RECORD-CONTAINMENT: dropping parameters accept closed and open
    /// events; closure and typed retention keep their stricter requirements.
    #[test]
    fn record_modes_preserve_event_containment_and_declared_field_checks() {
        use crate::RecordRest;
        let field = RecordTypeField {
            name: "name".into(),
            ty: Type::Text,
            optional: false,
        };
        for rest in [
            RecordRest::Closed,
            RecordRest::Drop,
            RecordRest::Keep(Box::new(Type::Json)),
        ] {
            let parameter = Type::Record(RecordType {
                fields: vec![field.clone()],
                rest: rest.clone(),
            });
            for additional in [json!(false), json!(true)] {
                let event = json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": additional});
                assert_eq!(
                    event_fits_entry_parameter(&event, &parameter).is_ok(),
                    additional == json!(false) || rest != RecordRest::Closed
                );
            }
            let extra = json!({"type": "object", "properties": {"name": {"type": "string"}, "extra": {"type": "boolean"}}, "required": ["name"], "additionalProperties": false});
            assert_eq!(
                event_fits_entry_parameter(&extra, &parameter).is_ok(),
                rest != RecordRest::Closed
            );
            for event in [
                json!({"type": "object", "properties": {"name": {"type": "boolean"}}, "required": ["name"]}),
                json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            ] {
                assert!(event_fits_entry_parameter(&event, &parameter).is_err());
            }
        }
        let typed = Type::Record(RecordType {
            fields: vec![field],
            rest: RecordRest::Keep(Box::new(Type::Int)),
        });
        let event = json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]});
        assert!(event_fits_entry_parameter(&event, &typed).is_err());
    }

    /// ENTRY-RECORD-SCHEMA: Drop omits additionalProperties, Closed forbids it,
    /// and Keep publishes its type. Required and optional fields remain intact.
    #[test]
    fn record_modes_publish_their_input_acceptance_schemas() {
        use crate::RecordRest;
        for (rest, additional) in [
            (RecordRest::Closed, Some(json!(false))),
            (RecordRest::Drop, None),
            (RecordRest::Keep(Box::new(Type::Json)), Some(json!(true))),
            (
                RecordRest::Keep(Box::new(Type::Int)),
                Some(json!({"type": "integer"})),
            ),
        ] {
            let ty = Type::Record(RecordType {
                fields: vec![
                    RecordTypeField {
                        name: "name".into(),
                        ty: Type::Text,
                        optional: false,
                    },
                    RecordTypeField {
                        name: "note".into(),
                        ty: Type::Text,
                        optional: true,
                    },
                ],
                rest,
            });
            let schemas = entry_schemas(
                &Signature {
                    params: vec![Param {
                        name: Name::new("event"),
                        ty,
                        optional: false,
                    }],
                    result: Type::Bool,
                },
                EntrySchemaPolicy::default(),
            )
            .unwrap();
            let record = &schemas.input.as_value()["properties"]["event"];
            assert_eq!(record.get("additionalProperties"), additional.as_ref());
            assert_eq!(record["required"], json!(["name"]));
            assert!(
                schemas
                    .input
                    .validate(&json!({"event": {"name": "Ada"}}))
                    .is_ok()
            );
            assert!(schemas.input.validate(&json!({"event": {}})).is_err());
            assert!(
                schemas
                    .input
                    .validate(&json!({"event": {"name": false}}))
                    .is_err()
            );
            assert!(
                schemas
                    .input
                    .validate(&json!({"event": {"name": "Ada", "note": 1}}))
                    .is_err()
            );
        }
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
            rest: crate::RecordRest::Closed,
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
                rest: crate::RecordRest::Keep(Box::new(Type::Json)),
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
    /// recursion, unsupported constraints and budgets retain their losses.
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

    /// EVENT-CONTAINMENT: ordinary event shapes fit Text and record parameters;
    /// narrowing a nested parameter refuses at its escaped type path.
    #[test]
    fn figments_events_fit_and_narrow_parameters_keep_the_exact_mismatch() {
        for schema in [
            json!({"type": "string", "format": "date-time"}),
            json!({"type": "string", "format": "uuid"}),
            json!({"oneOf": [
                {"const": "human", "description": "A person"},
                {"const": "agent", "description": "An agent"}
            ]}),
        ] {
            assert_eq!(event_fits_entry_parameter(&schema, &Type::Text), Ok(()));
        }
        let object = Type::Record(RecordType {
            fields: vec![],
            rest: crate::RecordRest::Keep(Box::new(Type::Json)),
        });
        assert_eq!(
            event_fits_entry_parameter(&json!({"type": "object"}), &object),
            Ok(())
        );
        let schema = json!({"type": "object", "properties": {
            "a/b~c": {"type": "string", "format": "uuid"},
            "values": {"type": "object"}
        }, "required": ["a/b~c", "values"]});
        let mut parameter = Type::Record(RecordType {
            fields: vec![
                RecordTypeField {
                    name: "a/b~c".into(),
                    ty: Type::Text,
                    optional: false,
                },
                RecordTypeField {
                    name: "values".into(),
                    ty: object,
                    optional: false,
                },
            ],
            rest: crate::RecordRest::Keep(Box::new(Type::Json)),
        });
        assert_eq!(event_fits_entry_parameter(&schema, &parameter), Ok(()));
        let Type::Record(record) = &mut parameter else {
            unreachable!()
        };
        record.fields[0].ty = Type::Enum(vec!["one-id".into()]);
        assert_eq!(
            event_fits_entry_parameter(&schema, &parameter),
            Err(EventContainmentRefusal::Mismatch(
                crate::SignatureMismatch {
                    side: crate::SignatureSide::Input,
                    path: "/input/properties/a~1b~0c".into(),
                    expected: Type::Enum(vec!["one-id".into()]),
                    provided: Type::Text,
                }
            ))
        );
    }

    /// EVENT-CONTAINMENT-SOUNDNESS: lost validation evidence can only prevent
    /// a proof, while an unrepresentable projection remains a typed refusal.
    #[test]
    fn widened_events_fit_but_projection_conflicts_refuse() {
        let schema = json!({"type": "string", "minLength": 2});
        assert_eq!(
            type_of_schema(&schema).unwrap().1,
            vec![crate::schema::Widening {
                schema_path: "/minLength".into(),
                reason: WideningReason::UnsupportedKeyword("minLength".into()),
            }]
        );
        assert_eq!(event_fits_entry_parameter(&schema, &Type::Text), Ok(()));
        let schema = json!({"type": "object", "properties": {"value": {
            "allOf": [{"type": "string"}, {"type": "integer"}]
        }}});
        assert_eq!(
            event_fits_entry_parameter(&schema, &Type::Json),
            Err(EventContainmentRefusal::Projection(
                crate::schema::SchemaProjectionRefusal {
                    path: "/properties/value/allOf".into(),
                    conflict: Box::new(crate::schema::SchemaConflict::Meet {
                        fields: vec![],
                        left: Type::Text,
                        right: Type::Int,
                    }),
                }
            ))
        );
    }
}
