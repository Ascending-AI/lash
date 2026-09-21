use super::*;

/// `Type { declared: str }`
fn declared_str_witness() -> Expr {
    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
        "declared",
        TypeExpr::Str,
        false,
    )]))
}

/// `{ task: "inspect", output: <witness> }`
fn inspect_request(witness: Expr) -> Expr {
    builders::record(vec![
        ("task", builders::string("inspect")),
        ("output", witness),
    ])
}

/// `result = (await <module>.<spawn|query>({ task: "inspect", output: <witness> }))?`
/// `result.<field>`
fn spawn_and_read(module: &str, witness: Expr, field: &str) -> Program {
    let operation = if module == "agents" { "spawn" } else { "query" };
    builders::program(vec![
        builders::assign(
            "result",
            builders::module_call(&[module], operation, vec![inspect_request(witness)]),
        ),
        builders::field(builders::var("result"), field),
    ])
}

/// `process ask(llm: Llm) -> <return_ty> { finish (await llm.query({ task: "plain text" }))? }`
fn plain_text_query(return_ty: TypeExpr) -> Program {
    builders::module(
        vec![builders::process_returning(
            "ask",
            vec![builders::param("llm", TypeExpr::Ref("Llm".into()))],
            return_ty,
            builders::block(vec![builders::finish(builders::unwrap(
                builders::await_expr(builders::receiver_call(
                    builders::var("llm"),
                    "query",
                    vec![builders::record(vec![(
                        "task",
                        builders::string("plain text"),
                    )])],
                )),
            ))]),
        )],
        Vec::new(),
    )
}

/// `result = (await static_tool.run({}))?`
/// `result.<field>`
fn static_tool_run(field: &str) -> Program {
    builders::program(vec![
        builders::assign(
            "result",
            builders::module_call(&["static_tool"], "run", vec![builders::record(Vec::new())]),
        ),
        builders::field(builders::var("result"), field),
    ])
}

fn typed_output_host_environment() -> LashlangHostEnvironment {
    let input_schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "task": { "type": "string" },
            "output": {}
        },
        "required": ["task"]
    });
    let mut resources = LashlangHostCatalog::new();
    for (module, authority, default_schema) in [
        ("agents", "Agents", None),
        ("llm", "Llm", Some(serde_json::json!({ "type": "string" }))),
    ] {
        resources
            .add_module_operation_contract(
                [module],
                authority,
                if module == "agents" { "spawn" } else { "query" },
                format!("{module}_typed_output"),
                &crate::OperationContract::from_input_field(
                    input_schema.clone(),
                    "output",
                    default_schema,
                ),
            )
            .expect("host catalog operation must not conflict");
    }
    resources
        .add_module_operation(
            ["static_tool"],
            "StaticTool",
            "run",
            "static_run",
            TypeExpr::Any,
            TypeExpr::Object(vec![TypeField {
                name: "declared".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
        )
        .expect("host catalog operation must not conflict");
    LashlangHostEnvironment::new(resources, LashlangAbilities::all())
}

#[test]
fn closed_type_literals_type_outputs_in_lowering_and_validation() {
    // result = (await agents.spawn({ task: "inspect", output: Type { declared: str } }))?
    // result.declared
    let direct = spawn_and_read("agents", declared_str_witness(), "declared");
    LinkedModule::link(direct, typed_output_host_environment())
        .expect("declared output field should link");

    // result = (await agents.spawn({ task: "inspect", output: Type { declared: str } }))?
    // result.undeclared
    let missing_in_lowering = spawn_and_read("agents", declared_str_witness(), "undeclared");
    assert!(matches!(
        LinkedModule::link(missing_in_lowering, typed_output_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
    ));

    // process ask(llm: Llm) {
    //   result = (await llm.query({ task: "inspect", output: Type { declared: str } }))?
    //   finish result.undeclared
    // }
    let missing_in_validation = builders::module(
        vec![builders::process(
            "ask",
            vec![builders::param("llm", TypeExpr::Ref("Llm".into()))],
            builders::block(vec![
                builders::assign(
                    "result",
                    builders::unwrap(builders::await_expr(builders::receiver_call(
                        builders::var("llm"),
                        "query",
                        vec![inspect_request(declared_str_witness())],
                    ))),
                ),
                builders::finish(builders::field(builders::var("result"), "undeclared")),
            ]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(missing_in_validation, typed_output_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
    ));
}

#[test]
fn literal_type_defensively_degrades_type_literals_to_any() {
    assert_eq!(
        literal_type(&Expr::TypeLiteral(Box::new(TypeExpr::Str))),
        TypeExpr::Any
    );
}

#[test]
fn declared_aliases_make_nested_type_literal_witnesses_closed() {
    // type Inner = { value: str }
    // result = (await agents.spawn({ task: "inspect", output: Type { nested: Inner } }))?
    // result.nested.value
    let program = builders::module(
        vec![builders::type_decl(
            "Inner",
            TypeExpr::Object(vec![builders::type_field("value", TypeExpr::Str, false)]),
        )],
        vec![
            builders::assign(
                "result",
                builders::module_call(
                    &["agents"],
                    "spawn",
                    vec![inspect_request(builders::type_literal(TypeExpr::Object(
                        vec![builders::type_field(
                            "nested",
                            TypeExpr::Ref("Inner".into()),
                            false,
                        )],
                    )))],
                ),
            ),
            builders::field(builders::field(builders::var("result"), "nested"), "value"),
        ],
    );
    LinkedModule::link(program, typed_output_host_environment())
        .expect("declared aliases should close a schema witness");
}

#[test]
fn record_shorthand_types_outputs_and_rejects_missing_fields() {
    // result = (await agents.spawn({
    //   task: "inspect",
    //   output: { declared: "str", count: "int", tags: "list[str]" }
    // }))?
    // values = [result.declared, result.count, result.tags]
    let declared = builders::program(vec![
        builders::assign(
            "result",
            builders::module_call(
                &["agents"],
                "spawn",
                vec![inspect_request(builders::record(vec![
                    ("declared", builders::string("str")),
                    ("count", builders::string("int")),
                    ("tags", builders::string("list[str]")),
                ]))],
            ),
        ),
        builders::assign(
            "values",
            builders::list(vec![
                builders::field(builders::var("result"), "declared"),
                builders::field(builders::var("result"), "count"),
                builders::field(builders::var("result"), "tags"),
            ]),
        ),
    ]);
    LinkedModule::link(declared, typed_output_host_environment())
        .expect("record shorthand fields should link");

    // result = (await llm.query({ task: "inspect", output: { declared: "str" } }))?
    // result.undeclared
    let missing = spawn_and_read(
        "llm",
        builders::record(vec![("declared", builders::string("str"))]),
        "undeclared",
    );
    assert!(matches!(
        LinkedModule::link(missing, typed_output_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
    ));
}

#[test]
fn dynamic_and_stored_schema_witnesses_stay_any() {
    // shape = Type { declared: str }
    // result = (await agents.spawn({ task: "inspect", output: shape }))?
    // result.undeclared
    let stored = builders::program(vec![
        builders::assign("shape", declared_str_witness()),
        builders::assign(
            "result",
            builders::module_call(
                &["agents"],
                "spawn",
                vec![inspect_request(builders::var("shape"))],
            ),
        ),
        builders::field(builders::var("result"), "undeclared"),
    ]);
    // inner = Type { value: str }
    // result = (await agents.spawn({ task: "inspect", output: Type { nested: inner } }))?
    // result.undeclared
    let nested = builders::program(vec![
        builders::assign(
            "inner",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "value",
                TypeExpr::Str,
                false,
            )])),
        ),
        builders::assign(
            "result",
            builders::module_call(
                &["agents"],
                "spawn",
                vec![inspect_request(builders::type_literal(TypeExpr::Object(
                    vec![builders::type_field(
                        "nested",
                        TypeExpr::Ref("inner".into()),
                        false,
                    )],
                )))],
            ),
        ),
        builders::field(builders::var("result"), "undeclared"),
    ]);
    for program in [stored, nested] {
        LinkedModule::link(program, typed_output_host_environment())
            .expect("dynamic schema witnesses must stay gradual");
    }
}

#[test]
fn missing_witness_uses_default_schema_and_static_tools_are_unchanged() {
    // process ask(llm: Llm) -> str { finish (await llm.query({ task: "plain text" }))? }
    let default_matches = plain_text_query(TypeExpr::Str);
    LinkedModule::link(default_matches, typed_output_host_environment())
        .expect("llm query should default to str");

    // process ask(llm: Llm) -> int { finish (await llm.query({ task: "plain text" }))? }
    let default_mismatch = plain_text_query(TypeExpr::Int);
    assert!(matches!(
        LinkedModule::link(default_mismatch, typed_output_host_environment()),
        Err(LinkError::IncompatibleProcessReturn { expected, actual, .. })
            if expected == "int" && actual == "str"
    ));

    // result = (await static_tool.run({}))?
    // result.declared
    let static_output = static_tool_run("declared");
    LinkedModule::link(static_output, typed_output_host_environment())
        .expect("static P2 output type should be preserved");

    // result = (await static_tool.run({}))?
    // result.undeclared
    let static_missing = static_tool_run("undeclared");
    assert!(matches!(
        LinkedModule::link(static_missing, typed_output_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
    ));
}

#[test]
fn shaping_builtins_link_valid_shapes_and_reject_every_known_wrong_shape() {
    let ranked = |rank: f64| builders::record(vec![("rank", builders::num(rank))]);
    let one_two = || builders::list(vec![builders::num(1.0), builders::num(2.0)]);
    let valid = builders::program(vec![builders::finish(builders::record(vec![
        (
            "sorted",
            builders::builtin(
                "sort",
                vec![builders::list(vec![builders::num(2.0), builders::num(1.0)])],
            ),
        ),
        (
            "sorted_by",
            builders::builtin(
                "sort_by",
                vec![
                    builders::list(vec![ranked(2.0), ranked(1.0)]),
                    builders::string("rank"),
                ],
            ),
        ),
        ("total", builders::builtin("sum", vec![one_two()])),
        ("least", builders::builtin("min", vec![one_two()])),
        ("greatest", builders::builtin("max", vec![one_two()])),
        (
            "rewritten",
            builders::builtin(
                "replace",
                vec![
                    builders::string("aba"),
                    builders::string("a"),
                    builders::string("x"),
                ],
            ),
        ),
        (
            "lower",
            builders::builtin("lower", vec![builders::string("ABC")]),
        ),
        (
            "upper",
            builders::builtin("upper", vec![builders::string("abc")]),
        ),
        (
            "unique",
            builders::builtin(
                "unique",
                vec![builders::list(vec![builders::num(1.0), builders::num(1.0)])],
            ),
        ),
        ("reversed", builders::builtin("reverse", vec![one_two()])),
    ]))]);
    LinkedModule::link(valid, full_host_environment())
        .expect("valid shaping builtin types should link");

    let one_and_two = || builders::list(vec![builders::num(1.0), builders::string("two")]);
    let value_record = || builders::record(vec![("value", builders::num(1.0))]);
    let invalid = [
        ("sort", r#"finish sort([1, "two"])"#, vec![one_and_two()]),
        (
            "sort_by",
            r#"finish sort_by([1], "rank")"#,
            vec![
                builders::list(vec![builders::num(1.0)]),
                builders::string("rank"),
            ],
        ),
        ("sum", r#"finish sum([1, "two"])"#, vec![one_and_two()]),
        (
            "min",
            "finish min([[1], [2]])",
            vec![builders::list(vec![
                builders::list(vec![builders::num(1.0)]),
                builders::list(vec![builders::num(2.0)]),
            ])],
        ),
        ("max", "finish max({ value: 1 })", vec![value_record()]),
        (
            "replace",
            r#"finish replace("a", "a", 1)"#,
            vec![
                builders::string("a"),
                builders::string("a"),
                builders::num(1.0),
            ],
        ),
        ("lower", "finish lower(1)", vec![builders::num(1.0)]),
        (
            "upper",
            "finish upper(false)",
            vec![builders::bool_lit(false)],
        ),
        ("unique", "finish unique(1)", vec![builders::num(1.0)]),
        (
            "reverse",
            "finish reverse({ value: 1 })",
            vec![value_record()],
        ),
    ];
    for (builtin, source, args) in invalid {
        let program = builders::program(vec![builders::finish(builders::builtin(builtin, args))]);
        assert!(
            matches!(
                LinkedModule::link(program, full_host_environment()),
                Err(LinkError::IncompatibleBuiltinOperands { builtin: actual, .. })
                    if actual == builtin
            ),
            "expected `{builtin}` typing failure for {source}"
        );
    }

    // process shape(items: dict) { finish reverse(items) }
    let dict_items = builders::module(
        vec![builders::process(
            "shape",
            vec![builders::param("items", TypeExpr::Dict)],
            builders::block(vec![builders::finish(builders::builtin(
                "reverse",
                vec![builders::var("items")],
            ))]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(dict_items, full_host_environment()),
        Err(LinkError::IncompatibleBuiltinOperands { builtin, .. }) if builtin == "reverse"
    ));
}
