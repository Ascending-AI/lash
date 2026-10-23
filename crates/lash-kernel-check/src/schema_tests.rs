//! Laws of the kernel types JSON schemas project to (`K-VAL-015`,
//! `K-EFF-002`): a JSON schema describes data, and only an `x-lash` marker
//! makes a slot admit a function, through the one decode the presenters
//! read too.

use lash_vocabulary::names::Name;
use lash_vocabulary::tool_shape::{SchemaDocument, ShapeKind};
use lash_vocabulary::types::MapType;
use lash_vocabulary::types::Param;
use lash_vocabulary::types::RecordType;
use lash_vocabulary::types::RecordTypeField;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;
use serde_json::{Value, json};

use lash_vocabulary::schema::SchemaConflict;
use lash_vocabulary::schema::SchemaProjectionRefusal;
use lash_vocabulary::schema::signature_serves;

/// The kernel type `schema` projects to as an input.
fn type_of_schema(schema: &Value) -> Type {
    lash_vocabulary::schema::type_of_schema(schema)
        .unwrap_or_else(|refusal| panic!("{schema}: {refusal}"))
        .0
}

/// The kernel type `schema` projects to as a result.
fn result_type_of_schema(schema: &Value) -> Type {
    lash_vocabulary::schema::result_type_of_schema(schema)
        .unwrap_or_else(|refusal| panic!("{schema}: {refusal}"))
        .0
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
                || record.rest.kept_type().is_some_and(admits_function)
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
                result: Type::Record(RecordType {
                    fields: Vec::new(),
                    rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
                }),
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
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
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
        rest: lash_vocabulary::RecordRest::Closed,
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
        assert!(
            lash_vocabulary::type_relations::disjoint(&never, &Type::Json),
            "{never:?}"
        );
    }
    for some in [Type::Any, data, Type::Error, Type::Text] {
        assert!(
            !lash_vocabulary::type_relations::disjoint(&some, &Type::Json),
            "{some:?}"
        );
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
    lash_vocabulary::schema::type_of_schema(schema).expect_err("no kernel type")
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
                conflict: Box::new(SchemaConflict::NoMembers)
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
            rest: lash_vocabulary::RecordRest::Closed,
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
            rest: lash_vocabulary::RecordRest::Closed,
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
            rest: lash_vocabulary::RecordRest::Closed
        })
    );
    assert_eq!(
        result_type_of_schema(&split),
        Type::Record(RecordType {
            fields: vec![field("a", Type::Text, true), field("b", Type::Int, true)],
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
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
            conflict: Box::new(SchemaConflict::Meet {
                fields: Vec::new(),
                left: Type::Text,
                right: Type::Int,
            }),
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
            conflict: Box::new(SchemaConflict::ClosedOut {
                fields: Vec::new(),
                field: "a".into()
            }),
        }
    );
    // The open result has room for the required field.
    assert!(lash_vocabulary::schema::result_type_of_schema(&required_out).is_ok());
    let nested = json!({"allOf": [
        closed_object(json!({"plan": closed_object(json!({"step": {"type": "string"}}), &[])}), &[]),
        closed_object(json!({"plan": closed_object(json!({"step": {"type": "boolean"}}), &[])}), &[]),
    ]});
    assert_eq!(
        *refusal(&nested).conflict,
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
                lash_vocabulary::type_relations::union([expected.clone(), Type::Null]),
            ),
            (
                json!({"oneOf": [{"type": "null"}, marked]}),
                lash_vocabulary::type_relations::union([Type::Null, expected.clone()]),
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
                    rest: lash_vocabulary::RecordRest::Closed,
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

/// An open object schema of `properties`, requiring `required`, as pydantic
/// and MCP servers write one: no `additionalProperties`.
fn open_object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// SCHEMA-OPEN (FIG-6966): an object schema with `properties` is a record
/// of its declared fields, typed, whose rest is what
/// `additionalProperties` states: `Json` when it states nothing or `true`,
/// the projected schema when it is one, and none when it is `false`
/// (closed as an input, `Json` as an open result). A key a
/// `patternProperties` pattern matches may hold anything that pattern
/// states, so the rest is `Json`. One that states no `type` also admits
/// every non-object value, so it is the record beside `Json`, which
/// absorbs it unless it may hold a function.
#[test]
fn an_open_object_schema_projects_to_a_record_of_its_typed_fields_and_a_json_rest() {
    let fields = json!({
        "label": {"type": "string"},
        "limit": {"anyOf": [{"type": "integer"}, {"type": "null"}], "default": null}
    });
    let typed = vec![
        field("label", Type::Text, false),
        field("limit", Type::Union(vec![Type::Int, Type::Null]), true),
    ];
    let open = |rest: Type| {
        Type::Record(RecordType {
            fields: typed.clone(),
            rest: lash_vocabulary::RecordRest::Keep(Box::new(rest)),
        })
    };
    let pydantic = open_object(fields.clone(), &["label"]);
    assert_eq!(type_of_schema(&pydantic), open(Type::Json));
    assert_eq!(result_type_of_schema(&pydantic), open(Type::Json));
    let mut stated = pydantic.clone();
    stated["additionalProperties"] = json!(true);
    assert_eq!(type_of_schema(&stated), open(Type::Json));
    stated["additionalProperties"] = json!({"type": "string"});
    assert_eq!(type_of_schema(&stated), open(Type::Text));
    assert_eq!(result_type_of_schema(&stated), open(Type::Text));
    stated["patternProperties"] = json!({"^x-": {"type": "integer"}});
    assert_eq!(type_of_schema(&stated), open(Type::Json));
    let closed = closed_object(fields, &["label"]);
    assert_eq!(
        type_of_schema(&closed),
        Type::Record(RecordType {
            fields: typed.clone(),
            rest: lash_vocabulary::RecordRest::Closed,
        })
    );
    assert_eq!(result_type_of_schema(&closed), open(Type::Json));
    // A bare object is exactly an open empty record.
    assert_eq!(
        type_of_schema(&json!({"type": "object"})),
        Type::Record(RecordType {
            fields: Vec::new(),
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
        })
    );
    assert_eq!(
        type_of_schema(&json!({"type": "object", "additionalProperties": {"type": "integer"}})),
        Type::Record(RecordType {
            fields: Vec::new(),
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Int)),
        })
    );
    // Without a `type`, the record of data is absorbed by `Json`, and the
    // record that may hold a function stands beside it.
    assert_eq!(
        type_of_schema(&json!({"properties": {"a": {"type": "string"}}})),
        Type::Json
    );
    let marked = json!({"properties": {"run": {"x-lash": {"kind": "function"}}}});
    assert_eq!(
        type_of_schema(&marked),
        Type::Union(vec![
            Type::Json,
            Type::Record(RecordType {
                fields: vec![field("run", Type::Any, true)],
                rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
            }),
        ])
    );
}

/// X-LASH-ONE on open objects and local references (FIG-6966): a marker on
/// a field of an open object, and one a local `$ref` names (under `$defs`
/// or `definitions`), types its slot as the marked kind, and the presenter
/// shows the same slot callable through the same decode. A reference the
/// projection cannot resolve to its target is `Json`: a non-local one, and
/// any within a schema that declares a nested `$id`, whose references the
/// presenter leaves to the tool too.
#[test]
fn a_marker_on_an_open_object_field_or_behind_a_local_reference_types_and_presents_its_slot() {
    let callable =
        |kind: &ShapeKind| matches!(kind, ShapeKind::Function(_) | ShapeKind::Process(_));
    let presented_field = |schema: &Value, name: &str| {
        let document = SchemaDocument::from_json_schema(schema, 8)
            .unwrap_or_else(|refusal| panic!("{schema}: {refusal:?}"));
        let ShapeKind::Object(object) = &document.roots[0].kind else {
            panic!("{schema}: presented as an object");
        };
        let shape = object
            .fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.shape.clone())
            .unwrap_or_else(|| panic!("{schema}: presents `{name}`"));
        match shape.kind {
            ShapeKind::Ref(id) => document.definitions[id.0].shape.kind.clone(),
            kind => kind,
        }
    };
    let slot = |schema: &Value| {
        let Type::Record(record) = type_of_schema(schema) else {
            panic!("{schema}: a record");
        };
        assert_eq!(record.rest.kept_type(), Some(&Type::Json), "{schema}");
        record
            .fields
            .into_iter()
            .find(|field| field.name == "slot")
            .unwrap_or_else(|| panic!("{schema}: types `slot`"))
            .ty
    };
    for (marked, expected) in marked_forms() {
        let schemas = [
            open_object(json!({"slot": marked, "label": {"type": "string"}}), &[]),
            json!({
                "$defs": {"Marked": marked},
                "type": "object",
                "properties": {"slot": {"$ref": "#/$defs/Marked", "description": "A slot."}}
            }),
            json!({
                "definitions": {"Marked": marked},
                "type": "object",
                "properties": {"slot": {"$ref": "#/definitions/Marked"}}
            }),
        ];
        for schema in schemas {
            assert_eq!(slot(&schema), expected, "{schema}");
            assert!(callable(&presented_field(&schema, "slot")), "{schema}");
        }
    }
    let unresolved = [
        json!({
            "type": "object",
            "properties": {"slot": {"$ref": "https://example.com/marked.json"}}
        }),
        json!({
            "$defs": {"Marked": {"$id": "marked", "x-lash": {"kind": "function"}}},
            "type": "object",
            "properties": {"slot": {"$ref": "#/$defs/Marked"}}
        }),
    ];
    for schema in unresolved {
        assert_eq!(slot(&schema), Type::Json, "{schema}");
        assert!(!callable(&presented_field(&schema, "slot")), "{schema}");
    }
}

/// `$ref` cycles terminate (FIG-6966): the kernel grammar has no recursive
/// type, so a reference into a target it is already resolving, the root
/// included, stops there: at `Json` when nothing the cycle reaches carries
/// an `x-lash` marker, and at `Any` when something does, so a function
/// still crosses into the recursive slot the presenter shows callable.
#[test]
fn a_reference_cycle_terminates_at_json_or_at_any_where_it_reaches_a_marker() {
    let node = |rest: Type, next: Type| {
        Type::Record(RecordType {
            fields: vec![field("next", next, true), field("value", Type::Int, true)],
            rest: lash_vocabulary::RecordRest::Keep(Box::new(rest)),
        })
    };
    let list = json!({
        "$defs": {"Node": open_object(
            json!({"next": {"$ref": "#/$defs/Node"}, "value": {"type": "integer"}}),
            &[],
        )},
        "$ref": "#/$defs/Node"
    });
    assert_eq!(type_of_schema(&list), node(Type::Json, Type::Json));
    let rooted = open_object(
        json!({"next": {"$ref": "#"}, "value": {"type": "integer"}}),
        &[],
    );
    assert_eq!(type_of_schema(&rooted), node(Type::Json, Type::Json));
    assert_eq!(type_of_schema(&json!({"$ref": "#"})), Type::Json);
    // Mutually recursive definitions resolve each once along a path.
    let mutual = json!({
        "$defs": {
            "A": open_object(json!({"b": {"$ref": "#/$defs/B"}}), &[]),
            "B": open_object(json!({"a": {"$ref": "#/$defs/A"}}), &[])
        },
        "$ref": "#/$defs/A"
    });
    let record = |name: &str, ty: Type| {
        Type::Record(RecordType {
            fields: vec![field(name, ty, true)],
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
        })
    };
    assert_eq!(
        type_of_schema(&mutual),
        record("b", record("a", Type::Json))
    );
    // A cycle that reaches a marked slot stops at `Any`.
    let handlers = json!({
        "$defs": {"Step": open_object(json!({
            "run": {"x-lash": {"kind": "function"}},
            "then": {"anyOf": [{"$ref": "#/$defs/Step"}, {"type": "null"}]}
        }), &[])},
        "$ref": "#/$defs/Step"
    });
    let Type::Record(step) = type_of_schema(&handlers) else {
        panic!("a record");
    };
    assert_eq!(step.fields[0], field("run", Type::Any, true));
    let then = &step.fields[1];
    assert_eq!(then.name, "then");
    assert_eq!(then.ty, Type::Any, "`Any` absorbs the null beside it");
}

/// SCHEMA-CONST (FIG-6966): a `const` is its literal's type: a text the
/// enum of that one text, a number spelled as an integer `Int` and any
/// other `Number`, a boolean `Bool` and null `Null`; a list or object
/// literal is data.
#[test]
fn a_const_projects_to_its_literal_type() {
    for (literal, expected) in [
        (json!("read"), Type::Enum(vec!["read".into()])),
        (json!(3), Type::Int),
        (json!(-3), Type::Int),
        (json!(1.5), Type::Number),
        (json!(true), Type::Bool),
        (json!(null), Type::Null),
        (json!([1]), Type::Json),
        (json!({"a": 1}), Type::Json),
    ] {
        let schema = json!({"const": literal});
        assert_eq!(type_of_schema(&schema), expected, "{schema}");
        assert_eq!(result_type_of_schema(&schema), expected, "{schema}");
    }
    // A pydantic tagged union discriminates its members by a `const`.
    let tagged = json!({"oneOf": [
        closed_object(json!({"kind": {"const": "a"}, "a": {"type": "string"}}), &["kind"]),
        closed_object(json!({"kind": {"const": "b"}}), &["kind"]),
    ]});
    assert_eq!(
        type_of_schema(&tagged),
        Type::Union(vec![
            Type::Record(RecordType {
                fields: vec![
                    field("a", Type::Text, true),
                    field("kind", Type::Enum(vec!["a".into()]), false),
                ],
                rest: lash_vocabulary::RecordRest::Closed,
            }),
            Type::Record(RecordType {
                fields: vec![field("kind", Type::Enum(vec!["b".into()]), false)],
                rest: lash_vocabulary::RecordRest::Closed,
            }),
        ])
    );
}

/// SCHEMA-BOUND (FIG-6966): a meet distributes over a union, so two
/// unions meet as their product, and a meet that would hold more type nodes
/// than both its sides together and than `MAX_PROJECTED_TYPE_NODES` refuses
/// the schema, naming the conjunction, before the product is built. A union
/// is as large as its members, so a large one stands. Local references a
/// schema shares down many paths are resolved at most
/// `MAX_RESOLVED_REFERENCES` times; past that a reference is typed as at a
/// cycle, so such a schema still projects, in bounded work.
#[test]
fn a_meet_past_its_size_bound_is_refused_naming_where_and_shared_references_are_bounded() {
    let members = |prefix: &str| {
        (0..80)
            .map(|index| open_object(json!({format!("{prefix}{index}"): {"type": "string"}}), &[]))
            .collect::<Vec<_>>()
    };
    let product = closed_object(
        json!({"query": {"allOf": [{"anyOf": members("a")}, {"anyOf": members("b")}]}}),
        &[],
    );
    let refused = refusal(&product);
    assert_eq!(
        refused,
        SchemaProjectionRefusal {
            path: "/properties/query/allOf".into(),
            conflict: Box::new(SchemaConflict::TooLarge {
                limit: lash_vocabulary::schema::MAX_PROJECTED_TYPE_NODES
            }),
        }
    );
    assert_eq!(
        refused.to_string(),
        "the schema at `/properties/query/allOf` has no kernel type: the meet of its members would hold more than 4096 type nodes"
    );
    // A product within the bound stands.
    let small = json!({"allOf": [
        {"anyOf": [{"type": "string"}, {"type": "integer"}, {"type": "null"}]},
        {"anyOf": [{"enum": ["a"]}, {"type": "number"}]}
    ]});
    assert_eq!(
        type_of_schema(&small),
        Type::Union(vec![Type::Enum(vec!["a".into()]), Type::Int])
    );
    // A union larger than the bound is as large as its members: it stands,
    // and so does its meet with a type that only narrows it.
    let modes: Vec<Value> = (0..=lash_vocabulary::schema::MAX_PROJECTED_TYPE_NODES)
        .map(|index| json!({"const": format!("v{index}")}))
        .collect();
    let Type::Union(wide) = type_of_schema(&json!({"type": "string", "anyOf": modes})) else {
        panic!("a union");
    };
    assert_eq!(
        wide.len(),
        lash_vocabulary::schema::MAX_PROJECTED_TYPE_NODES + 1
    );
    // Sixteen definitions each naming the next twice resolve 2^17 - 1
    // references in full: the first resolutions are typed, and those past
    // the budget are typed as at a cycle.
    let depth = 16;
    let mut definitions = serde_json::Map::new();
    for index in 0..depth {
        let next = json!({"$ref": format!("#/definitions/D{}", index + 1)});
        definitions.insert(
            format!("D{index}"),
            open_object(json!({"left": next, "right": next}), &[]),
        );
    }
    definitions.insert(format!("D{depth}"), json!({"type": "string"}));
    let shared = json!({"$ref": "#/definitions/D0", "definitions": definitions});
    let mut ty = type_of_schema(&shared);
    for _ in 0..depth {
        let Type::Record(record) = ty else {
            panic!("the first path is typed in full");
        };
        ty = record.fields[0].ty.clone();
    }
    assert_eq!(ty, Type::Text);
    let mut ty = type_of_schema(&shared);
    let mut cut = false;
    for _ in 0..depth {
        let Type::Record(record) = ty else {
            cut = true;
            break;
        };
        ty = record.fields[1].ty.clone();
    }
    assert!(cut && ty == Type::Json, "the last path is cut: {ty:?}");
}
