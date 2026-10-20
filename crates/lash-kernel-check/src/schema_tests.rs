//! Laws of the kernel types JSON schemas project to (`K-VAL-015`,
//! `K-EFF-002`): a JSON schema describes data, and only an `x-lash` marker
//! makes a slot admit a function, through the one decode the presenters
//! read too.

use lash_kernel_doc::{MapType, Name, Param, RecordType, RecordTypeField, Signature, Type};
use lash_tool_shape::{SchemaDocument, ShapeKind};
use serde_json::{Value, json};

use crate::{SchemaConflict, SchemaProjectionRefusal, signature_serves};

/// The kernel type `schema` projects to as an input.
fn type_of_schema(schema: &Value) -> Type {
    crate::type_of_schema(schema).unwrap_or_else(|refusal| panic!("{schema}: {refusal}"))
}

/// The kernel type `schema` projects to as a result.
fn result_type_of_schema(schema: &Value) -> Type {
    crate::result_type_of_schema(schema).unwrap_or_else(|refusal| panic!("{schema}: {refusal}"))
}

/// Some value of `ty` holds a function somewhere: what the crossing walk
/// lets cross.
fn admits_function(ty: &Type) -> bool {
    match ty {
        Type::Any | Type::Function(_) => true,
        Type::Json
        | Type::Null
        | Type::Absent
        | Type::Bool
        | Type::Int
        | Type::Float
        | Type::Number
        | Type::Text
        | Type::Bytes
        | Type::Timestamp
        | Type::Enum(_)
        | Type::Error
        | Type::Handle(_) => false,
        Type::Tuple(members) | Type::Union(members) => members.iter().any(admits_function),
        Type::List(item) | Type::Set(item) | Type::Task(item) => admits_function(item),
        Type::Map(map) => admits_function(&map.key) || admits_function(&map.value),
        Type::Record(record) => {
            record.fields.iter().any(|field| admits_function(&field.ty))
                || record.rest.as_deref().is_some_and(admits_function)
        }
    }
}

/// Every unmarked JSON-schema form: each fallback, each exact type, and
/// each container of them.
fn unmarked_forms() -> Vec<Value> {
    vec![
        json!(true),
        json!(false),
        json!(1),
        json!({}),
        json!({"type": "string"}),
        json!({"type": "number"}),
        json!({"type": "integer"}),
        json!({"type": "boolean"}),
        json!({"type": "null"}),
        json!({"type": ["string", "null"]}),
        json!({"type": "array"}),
        json!({"type": "array", "items": {}}),
        json!({"type": "object"}),
        json!({"type": "object", "properties": {"a": {}}}),
        json!({"type": "object", "properties": {"a": {}}, "additionalProperties": true}),
        json!({"type": "object", "additionalProperties": {"type": "string"}}),
        json!({"type": "object", "properties": {"a": {}, "b": {"type": "object"}},
            "additionalProperties": false}),
        json!({"enum": []}),
        json!({"enum": ["a", "b"]}),
        json!({"enum": [1, 2]}),
        json!({"const": "a"}),
        json!({"anyOf": [{"type": "string"}, {"type": "object"}]}),
        json!({"oneOf": [{"type": "string"}]}),
        json!({"allOf": [{"type": "object"}]}),
        json!({"$ref": "#/$defs/thing", "$defs": {"thing": {}}}),
        json!({"x-lash": {"kind": "closure"}}),
        json!({"x-lash": "function"}),
    ]
}

/// The marked slots, each with the kernel type it projects to.
fn marked_forms() -> Vec<(Value, Type)> {
    let signature = json!({
        "params": [{"name": "event", "schema": {"type": "string"}}],
        "output": {"type": "object"}
    });
    vec![
        (
            json!({"x-lash": {"kind": "function", "signature": signature}}),
            Type::Function(Box::new(Signature {
                params: vec![Param {
                    name: Name::new("event"),
                    ty: Type::Text,
                    optional: false,
                }],
                result: Type::Json,
            })),
        ),
        (json!({"x-lash": {"kind": "function"}}), Type::Any),
        (
            json!({"x-lash": {"kind": "process", "signature": signature}}),
            Type::Any,
        ),
        (json!({"x-lash": {"kind": "process_unknown"}}), Type::Any),
        // The marker is what the slot is: a type beside it says nothing.
        (
            json!({"x-lash": {"kind": "function"}, "type": "object"}),
            Type::Any,
        ),
    ]
}

/// SCHEMA-JSON: every JSON-schema form projects to a type that admits no
/// function, as a tool's input and as its open result, except a slot whose
/// `x-lash` kind is `function`, `process` or `process_unknown`, wherever
/// in the schema it stands.
#[test]
fn a_json_schema_admits_a_function_only_where_an_x_lash_marker_says_so() {
    for schema in unmarked_forms() {
        for ty in [type_of_schema(&schema), result_type_of_schema(&schema)] {
            assert!(!admits_function(&ty), "{schema}: {ty:?}");
        }
    }
    for (schema, expected) in marked_forms() {
        assert_eq!(type_of_schema(&schema), expected, "{schema}");
        assert!(admits_function(&expected), "{schema}");
        let nested = json!({
            "type": "object",
            "properties": {"plan": {"type": "array", "items": schema}, "label": {"type": "string"}},
            "additionalProperties": false
        });
        let Type::Record(record) = type_of_schema(&nested) else {
            panic!("{nested}: a closed record");
        };
        let field = |name: &str| {
            record
                .fields
                .iter()
                .find(|field| field.name == name)
                .map(|field| field.ty.clone())
        };
        assert_eq!(
            field("plan"),
            Some(Type::List(Box::new(expected.clone()))),
            "{nested}"
        );
        assert_eq!(field("label"), Some(Type::Text), "{nested}");
    }
    // The rest of an open result record is data too.
    let open = json!({"type": "object", "properties": {"a": {"type": "string"}},
        "additionalProperties": false});
    assert_eq!(
        result_type_of_schema(&open),
        Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "a".into(),
                ty: Type::Text,
                optional: true,
            }],
            rest: Some(Box::new(Type::Json)),
        })
    );
}

/// X-LASH-ONE (T1): each `x-lash` kind maps to one kernel type and one
/// shape kind through the one decode, and a slot is presented as callable
/// exactly when its kernel type admits a function.
#[test]
fn the_presenter_shows_a_slot_callable_exactly_when_its_kernel_type_admits_a_function() {
    let callable =
        |kind: &ShapeKind| matches!(kind, ShapeKind::Function(_) | ShapeKind::Process(_));
    let presented = |schema: &Value| {
        SchemaDocument::from_json_schema(schema, 8)
            .unwrap_or_else(|refusal| panic!("{schema}: {refusal:?}"))
            .roots[0]
            .kind
            .clone()
    };
    let table = marked_forms();
    let kinds: Vec<ShapeKind> = table.iter().map(|(schema, _)| presented(schema)).collect();
    assert!(
        matches!(&kinds[0], ShapeKind::Function(Some(signature)) if signature.params.len() == 1)
    );
    assert_eq!(kinds[1], ShapeKind::Function(None));
    assert!(matches!(&kinds[2], ShapeKind::Process(Some(_))));
    assert_eq!(kinds[3], ShapeKind::Process(None));
    assert_eq!(kinds[4], ShapeKind::Function(None));
    for ((schema, ty), kind) in table.iter().zip(&kinds) {
        assert!(callable(kind) && admits_function(ty), "{schema}");
    }
    for schema in unmarked_forms() {
        let Ok(document) = SchemaDocument::from_json_schema(&schema, 8) else {
            continue;
        };
        assert!(
            !callable(&document.roots[0].kind),
            "{schema}: presented callable, projected {:?}",
            type_of_schema(&schema)
        );
    }
}

/// `K-VAL-015`: `Json` serves where `Any` is expected and not the other
/// way round; a type whose every value is data serves where `Json` is
/// expected, one that may hold a function does not; and a function, task
/// or handle is never `Json`.
#[test]
fn json_is_the_data_part_of_any() {
    let returns = |result: Type| Signature {
        params: Vec::new(),
        result,
    };
    let takes = |ty: Type| Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty,
            optional: false,
        }],
        result: Type::Any,
    };
    let function = Type::Function(Box::new(returns(Type::Any)));
    let data = Type::Record(RecordType {
        fields: vec![RecordTypeField {
            name: "rows".into(),
            ty: Type::List(Box::new(Type::Map(Box::new(MapType {
                key: Type::Text,
                value: Type::Union(vec![Type::Int, Type::Null]),
            })))),
            optional: false,
        }],
        rest: None,
    });
    assert!(signature_serves(&returns(Type::Json), &returns(Type::Any)));
    assert!(!signature_serves(&returns(Type::Any), &returns(Type::Json)));
    assert!(signature_serves(
        &returns(data.clone()),
        &returns(Type::Json)
    ));
    assert!(!signature_serves(
        &returns(Type::List(Box::new(function.clone()))),
        &returns(Type::Json)
    ));
    assert!(signature_serves(&takes(Type::Any), &takes(Type::Json)));
    assert!(!signature_serves(&takes(Type::Json), &takes(Type::Any)));
    for never in [
        function,
        Type::Task(Box::new(Type::Any)),
        Type::Handle("rows".into()),
    ] {
        assert!(crate::types::disjoint(&never, &Type::Json), "{never:?}");
    }
    for some in [Type::Any, data, Type::Error, Type::Text] {
        assert!(!crate::types::disjoint(&some, &Type::Json), "{some:?}");
    }
}

/// A closed object schema of `properties`, requiring `required`.
fn closed_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn field(name: &str, ty: Type, optional: bool) -> RecordTypeField {
    RecordTypeField {
        name: name.into(),
        ty,
        optional,
    }
}

fn refusal(schema: &Value) -> SchemaProjectionRefusal {
    crate::type_of_schema(schema).expect_err("no kernel type")
}

/// SCHEMA-UNION (FIG-6951): `anyOf`, `oneOf` and a `type` list project to
/// the union of their members, flattened and with `{"type": "null"}` as the
/// null member, `oneOf` typed as `anyOf`; `Json` absorbs the members whose
/// every value is data; a composition with no member is refused.
#[test]
fn an_alternative_composition_projects_to_the_union_of_its_members() {
    let cases = [
        (
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
            Type::Union(vec![Type::Text, Type::Null]),
        ),
        (
            json!({"oneOf": [{"type": "integer"}, {"type": "string"}]}),
            Type::Union(vec![Type::Int, Type::Text]),
        ),
        (
            json!({"type": ["string", "null"]}),
            Type::Union(vec![Type::Text, Type::Null]),
        ),
        (
            json!({"anyOf": [
                {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                {"type": "null"},
                {"type": "string"}
            ]}),
            Type::Union(vec![Type::Text, Type::Int, Type::Null]),
        ),
        (json!({"anyOf": [{"type": "null"}]}), Type::Null),
        (
            json!({"enum": ["low", "high", null]}),
            Type::Union(vec![
                Type::Enum(vec!["low".into(), "high".into()]),
                Type::Null,
            ]),
        ),
        (
            json!({"anyOf": [{"type": "string"}, {"$ref": "#/$defs/thing"}],
                "$defs": {"thing": {}}}),
            Type::Json,
        ),
        (
            json!({"type": "array", "items": {"anyOf": [{"type": "number"}, {"type": "null"}]}}),
            Type::List(Box::new(Type::Union(vec![Type::Number, Type::Null]))),
        ),
    ];
    for (schema, expected) in cases {
        assert_eq!(type_of_schema(&schema), expected, "{schema}");
    }
    for (schema, path) in [
        (json!({"anyOf": []}), "/anyOf"),
        (json!({"oneOf": []}), "/oneOf"),
        (json!({"allOf": []}), "/allOf"),
        (
            closed_object(json!({"mode": {"type": []}}), &[]),
            "/properties/mode/type",
        ),
    ] {
        assert_eq!(
            refusal(&schema),
            SchemaProjectionRefusal {
                path: path.into(),
                conflict: SchemaConflict::NoMembers
            },
            "{schema}"
        );
    }
}

/// SCHEMA-MEET (FIG-6951): `allOf`, and a composition beside a schema's
/// own type, projects to the meet of its members: compatible records merge
/// their fields, each required when either member requires it and closed
/// out where the schema's own `additionalProperties: false` says so. A meet
/// no kernel type states refuses the schema, naming the composition.
#[test]
fn a_conjunction_projects_to_the_meet_of_its_members_or_is_refused_naming_where() {
    let narrowed = json!({"allOf": [
        closed_object(json!({"mode": {"type": "string"}, "limit": {"type": "number"}}), &[]),
        closed_object(
            json!({"mode": {"enum": ["read", "write"]}, "limit": {"type": "integer"}}),
            &["mode"],
        ),
    ]});
    let mode = Type::Enum(vec!["read".into(), "write".into()]);
    assert_eq!(
        type_of_schema(&narrowed),
        Type::Record(RecordType {
            fields: vec![field("limit", Type::Int, true), field("mode", mode, false),],
            rest: None,
        })
    );
    // A required member beside the record it refines adds nothing the
    // grammar states: the record stands.
    let refined = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        "additionalProperties": false,
        "oneOf": [{"required": ["a"]}, {"required": ["b"]}]
    });
    assert_eq!(
        type_of_schema(&refined),
        Type::Record(RecordType {
            fields: vec![field("a", Type::Text, true), field("b", Type::Text, true)],
            rest: None,
        })
    );
    // Each member's own openness: as an input, closed members leave no room
    // for each other's optional fields; as an open result, they merge.
    let split = json!({"allOf": [
        closed_object(json!({"a": {"type": "string"}}), &[]),
        closed_object(json!({"b": {"type": "integer"}}), &[]),
    ]});
    assert_eq!(
        type_of_schema(&split),
        Type::Record(RecordType {
            fields: Vec::new(),
            rest: None
        })
    );
    assert_eq!(
        result_type_of_schema(&split),
        Type::Record(RecordType {
            fields: vec![field("a", Type::Text, true), field("b", Type::Int, true)],
            rest: Some(Box::new(Type::Json)),
        })
    );
    // A meet distributes over a union and keeps the members that meet.
    assert_eq!(
        type_of_schema(&json!({
            "type": "string",
            "anyOf": [{"enum": ["a", "b"]}, {"type": "null"}, {"type": "string"}]
        })),
        Type::Union(vec![Type::Enum(vec!["a".into(), "b".into()]), Type::Text])
    );

    let disjoint = closed_object(
        json!({"limit": {"allOf": [{"type": "string"}, {"type": "integer"}]}}),
        &[],
    );
    let refused = refusal(&disjoint);
    assert_eq!(
        refused,
        SchemaProjectionRefusal {
            path: "/properties/limit/allOf".into(),
            conflict: SchemaConflict::Meet {
                fields: Vec::new(),
                left: Type::Text,
                right: Type::Int,
            },
        }
    );
    assert_eq!(
        refused.to_string(),
        "the schema at `/properties/limit/allOf` has no kernel type: no kernel type is both `Text` and `Int`"
    );
    let required_out = json!({"allOf": [
        closed_object(json!({"a": {"type": "string"}}), &["a"]),
        closed_object(json!({"b": {"type": "integer"}}), &[]),
    ]});
    assert_eq!(
        refusal(&required_out),
        SchemaProjectionRefusal {
            path: "/allOf".into(),
            conflict: SchemaConflict::ClosedOut {
                fields: Vec::new(),
                field: "a".into()
            },
        }
    );
    // The open result has room for the required field.
    assert!(crate::result_type_of_schema(&required_out).is_ok());
    let nested = json!({"allOf": [
        closed_object(json!({"plan": closed_object(json!({"step": {"type": "string"}}), &[])}), &[]),
        closed_object(json!({"plan": closed_object(json!({"step": {"type": "boolean"}}), &[])}), &[]),
    ]});
    assert_eq!(
        refusal(&nested).conflict,
        SchemaConflict::Meet {
            fields: vec!["plan".into(), "step".into()],
            left: Type::Text,
            right: Type::Bool,
        }
    );
    assert_eq!(
        refusal(&json!({"anyOf": [{"type": "string"}], "allOf": [{"type": "null"}]})).path,
        "/anyOf"
    );
}

/// X-LASH-ONE under composition (FIG-6951): a marker under `anyOf` or
/// `oneOf` beside the null member types its slot as the marked kind or
/// null, and under `allOf` as the marked kind, through the one decode; the
/// presenter shows the same member callable, and the null member null.
#[test]
fn a_marker_under_a_composition_types_and_presents_its_slot_as_the_marked_kind() {
    let presented = |schema: &Value| {
        SchemaDocument::from_json_schema(schema, 8)
            .unwrap_or_else(|refusal| panic!("{schema}: {refusal:?}"))
            .roots[0]
            .kind
            .clone()
    };
    let callable =
        |kind: &ShapeKind| matches!(kind, ShapeKind::Function(_) | ShapeKind::Process(_));
    for (marked, expected) in marked_forms() {
        for (schema, nullable) in [
            (
                json!({"anyOf": [marked, {"type": "null"}]}),
                crate::types::union([expected.clone(), Type::Null]),
            ),
            (
                json!({"oneOf": [{"type": "null"}, marked]}),
                crate::types::union([Type::Null, expected.clone()]),
            ),
        ] {
            assert_eq!(type_of_schema(&schema), nullable, "{schema}");
            assert!(admits_function(&nullable), "{schema}");
            let ShapeKind::Union(members) = presented(&schema) else {
                panic!("{schema}: presented as a union");
            };
            assert_eq!(members.len(), 2, "{schema}");
            assert!(
                members.iter().any(|member| callable(&member.kind)),
                "{schema}"
            );
            assert!(
                members.iter().any(|member| member.kind == ShapeKind::Null),
                "{schema}"
            );
            let slot = closed_object(json!({"definition": schema}), &[]);
            assert_eq!(
                type_of_schema(&slot),
                Type::Record(RecordType {
                    fields: vec![field("definition", nullable.clone(), true)],
                    rest: None,
                }),
                "{slot}"
            );
        }
        let conjunction = json!({"allOf": [marked]});
        assert_eq!(type_of_schema(&conjunction), expected, "{conjunction}");
        assert!(callable(&presented(&conjunction)), "{conjunction}");
    }
    // A signature under a composition is projected as one beside none.
    let typed = json!({"anyOf": [
        {"x-lash": {"kind": "function", "signature": {
            "params": [{"name": "event", "schema": {"anyOf": [{"type": "string"}, {"type": "null"}]}}],
            "output": {"type": "string"}
        }}},
        {"type": "null"}
    ]});
    assert_eq!(
        type_of_schema(&typed),
        Type::Union(vec![
            Type::Function(Box::new(Signature {
                params: vec![Param {
                    name: Name::new("event"),
                    ty: Type::Union(vec![Type::Text, Type::Null]),
                    optional: false,
                }],
                result: Type::Text,
            })),
            Type::Null,
        ])
    );
}
