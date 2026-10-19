//! Laws of the kernel types JSON schemas project to (`K-VAL-015`,
//! `K-EFF-002`): a JSON schema describes data, and only an `x-lash` marker
//! makes a slot admit a function, through the one decode the presenters
//! read too.

use lash_kernel_doc::{MapType, Name, Param, RecordType, RecordTypeField, Signature, Type};
use lash_tool_shape::{SchemaDocument, ShapeKind};
use serde_json::{Value, json};

use crate::{result_type_of_schema, signature_serves, type_of_schema};

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
