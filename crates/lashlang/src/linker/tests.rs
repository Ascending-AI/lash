use super::*;
use crate::testing::ast_builders as builders;

mod canonical_walk_tests;
#[path = "catalog_tests.rs"]
mod catalog_tests;
mod diagnostic_tests;
mod identity_tests;
mod module_link_tests;
mod open_shape_tests;
mod process_literal_tests;
mod schema_witness_tests;
mod type_flow_tests;
mod workflow_classification_tests;

fn full_host_environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(resources())
}

fn full_label_environment() -> LashlangHostEnvironment {
    full_host_environment()
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

#[test]
fn typescript_lowering_intrinsics_link_through_the_production_registry_path() {
    let builtin = |name: &str, args: Vec<Expr>| Expr::BuiltinCall {
        name: name.into(),
        args,
    };
    let function = || {
        Expr::Function(Box::new(crate::FunctionExpr {
            name: None,
            js_name: None,
            receiver: None,
            params: vec!["value".into()],
            captures: Vec::new(),
            body: Box::new(Expr::FunctionReturn(Box::new(Expr::Variable(
                "value".into(),
            )))),
        }))
    };
    let cases = [
        (
            "instanceof",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_heap_instanceof",
                vec![
                    builtin(
                        "__lashlang_heap_new",
                        vec![Expr::String("TypeError".into())],
                    ),
                    Expr::String("TypeError".into()),
                ],
            )))]),
        ),
        (
            "global delete",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_global_delete",
                vec![Expr::String("state".into())],
            )))]),
        ),
        (
            "global read",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_global_get",
                vec![Expr::String("state".into())],
            )))]),
        ),
        (
            "global presence",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_global_has",
                vec![Expr::String("state".into())],
            )))]),
        ),
        (
            "spread dynamic call",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_call_dynamic",
                vec![function(), Expr::List(vec![Expr::Number(1.0)])],
            )))]),
        ),
        (
            "async map",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_async_map",
                vec![Expr::List(vec![Expr::Number(1.0)]), function()],
            )))]),
        ),
        (
            "default and rest closure metadata",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_closure",
                vec![function(), Expr::Number(1.0), Expr::Bool(false)],
            )))]),
        ),
        (
            "nested global set",
            Program::block(vec![Expr::Finish(Box::new(builtin(
                "__lashlang_global_set",
                vec![Expr::String("state".into()), Expr::Number(1.0)],
            )))]),
        ),
        (
            "URI codec globals",
            Program::block(vec![Expr::Finish(Box::new(Expr::List(
                [
                    "__lashlang_encode_uri_component",
                    "__lashlang_decode_uri_component",
                    "__lashlang_encode_uri",
                    "__lashlang_decode_uri",
                ]
                .into_iter()
                .map(|name| builtin(name, vec![Expr::String("value".into())]))
                .collect(),
            )))]),
        ),
    ];
    for (shape, program) in cases {
        LinkedModule::link(program, full_host_environment())
            .unwrap_or_else(|error| panic!("{shape} must link: {error}"));
    }
}

fn timer_tick_type_with_field(field: &'static str) -> NamedDataType {
    NamedDataType::object(
        "timer.Tick",
        vec![TypeField {
            name: field.into(),
            ty: TypeExpr::Str,
            optional: false,
        }],
    )
    .expect("valid timer tick type")
}

#[test]
fn named_host_data_type_validation_rejects_invalid_shapes() {
    let duplicate_field = NamedDataType::object(
        "timer.Tick",
        vec![
            TypeField {
                name: "fired_at".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
            TypeField {
                name: "fired_at".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect_err("duplicate fields should be rejected");
    assert!(matches!(
        duplicate_field,
        NamedDataTypeError::DuplicateField { .. }
    ));

    let nested_ref = NamedDataType::object(
        "timer.Tick",
        vec![TypeField {
            name: "nested".into(),
            ty: TypeExpr::Ref("Other.Type".into()),
            optional: false,
        }],
    )
    .expect_err("nested refs should be rejected");
    assert!(matches!(nested_ref, NamedDataTypeError::NestedRef { .. }));

    let duplicate_enum = NamedDataType::object(
        "timer.Tick",
        vec![TypeField {
            name: "kind".into(),
            ty: TypeExpr::Enum(vec!["Red".into(), "Red".into()]),
            optional: false,
        }],
    )
    .expect_err("duplicate enum values should be rejected");
    assert!(matches!(
        duplicate_enum,
        NamedDataTypeError::DuplicateEnumValue { .. }
    ));

    let simple_name =
        NamedDataType::object("Tick", vec![]).expect_err("host data type names must be qualified");
    assert!(matches!(
        simple_name,
        NamedDataTypeError::InvalidName { .. }
    ));
}

#[test]
fn resource_catalog_rejects_conflicting_named_host_data_type_definitions() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_named_data_type(timer_tick_type_with_field("fired_at"))
        .expect("first definition");
    let err = catalog
        .add_named_data_type(timer_tick_type_with_field("delivered_at"))
        .expect_err("same host type name with different shape should be rejected");

    assert!(matches!(
        err,
        LashlangHostCatalogError::ConflictingNamedDataType { .. }
    ));
}

fn resources() -> LashlangHostCatalog {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "read_file",
            "read_file",
            TypeExpr::Object(vec![TypeField {
                name: "path".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
            TypeExpr::Str,
        )
        .expect("host catalog operation must not conflict");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    for (operation, input_ty) in [
        ("accept_str", TypeExpr::Str),
        ("accept_int", TypeExpr::Int),
        ("accept_float", TypeExpr::Float),
        (
            "accept_mode",
            TypeExpr::Enum(vec!["default".into(), "careful".into()]),
        ),
    ] {
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                operation,
                operation,
                input_ty,
                TypeExpr::Null,
            )
            .expect("host catalog operation must not conflict");
    }
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "accept_config",
            "accept_config",
            TypeExpr::Object(vec![TypeField {
                name: "mode".into(),
                ty: TypeExpr::Enum(vec!["default".into()]),
                optional: false,
            }]),
            TypeExpr::Null,
        )
        .expect("host catalog operation must not conflict");
    // The process control surface is a set of leaf tools now (FIG-2999), so a
    // fixture that starts or cancels a process calls them like any
    // other module operation.
    for operation in ["start", "cancel"] {
        catalog
            .add_module_operation(
                ["processes"],
                "Processes",
                operation,
                operation,
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
    }
    // The FIG-2997 lift fixture: a leaf tool whose `program` slot is typed
    // `Process` through the `x-lash` keyword (FIG-2993), the way real process
    // controls declare a target. The lift is type-directed on exactly this
    // contract shape.
    catalog
        .add_module_operation_contract(
            ["crew"],
            "Crew",
            "run",
            "crew.run",
            &crate::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "program": { "x-lash": { "kind": "process_unknown" } },
                        "inputs": { "type": "object" }
                    },
                    "required": ["program", "inputs"]
                }),
                serde_json::json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        )
        .expect("process-slot fixture operation");
    catalog
}
