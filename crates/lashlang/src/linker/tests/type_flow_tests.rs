use super::*;
use crate::ast::BinaryOp;

/// `await tools.accept_mode(<argument>)?`
fn accept_mode(argument: Expr) -> Program {
    builders::program(vec![builders::module_call(
        &["tools"],
        "accept_mode",
        vec![argument],
    )])
}

/// `process select(mode: <mode_ty>) { finish mode }`
/// `start select(mode: "nope")`
/// `process consume(<params>) { for item in <iterable> { await tools.accept_int(item)? } }`
fn consume_process(params: Vec<ProcessParam>, iterable: Expr) -> Program {
    builders::module(
        vec![builders::process(
            "consume",
            params,
            builders::block(vec![builders::for_in(
                "item",
                iterable,
                builders::block(vec![builders::module_call(
                    &["tools"],
                    "accept_int",
                    vec![builders::var("item")],
                )]),
            )]),
        )],
        Vec::new(),
    )
}

/// ```text
/// process mutate(flag: bool) {
///   value = { a: 0 }
///   if flag { value = { a: 0 } } else { value = { b: 0 } }
///   value.<field> = 1
/// }
/// ```
fn union_field_mutation(field: &str) -> Program {
    let record = |name: &str| builders::record(vec![(name, builders::num(0.0))]);
    builders::module(
        vec![builders::process(
            "mutate",
            vec![builders::param("flag", TypeExpr::Bool)],
            builders::block(vec![
                builders::assign("value", record("a")),
                builders::if_else(
                    builders::var("flag"),
                    builders::block(vec![builders::assign("value", record("a"))]),
                    builders::block(vec![builders::assign("value", record("b"))]),
                ),
                builders::assign_path(
                    "value",
                    vec![builders::field_step(field)],
                    builders::num(1.0),
                ),
            ]),
        )],
        Vec::new(),
    )
}

#[test]
fn expected_enum_slots_reject_wrong_literals_but_admit_members_and_broad_strings() {
    // await tools.accept_mode("nope")?
    let wrong = accept_mode(builders::string("nope"));
    assert!(matches!(
        LinkedModule::link(wrong, full_host_environment()),
        Err(LinkError::IncompatibleExpectedLiteral { .. })
    ));

    // await tools.accept_mode("default")?
    let member = accept_mode(builders::string("default"));
    LinkedModule::link(member, full_host_environment()).expect("enum member should link");

    // process forward(mode: str) { await tools.accept_mode(mode)? }
    let broad = builders::module(
        vec![builders::process(
            "forward",
            vec![builders::param("mode", TypeExpr::Str)],
            builders::block(vec![builders::module_call(
                &["tools"],
                "accept_mode",
                vec![builders::var("mode")],
            )]),
        )],
        Vec::new(),
    );
    LinkedModule::link(broad, full_host_environment())
        .expect("broad strings remain gradually consistent with enums");

    // await tools.accept_config({ mode: "nope" })?
    let nested = builders::program(vec![builders::module_call(
        &["tools"],
        "accept_config",
        vec![builders::record(vec![("mode", builders::string("nope"))])],
    )]);
    assert!(matches!(
        LinkedModule::link(nested, full_host_environment()),
        Err(LinkError::IncompatibleExpectedLiteral { .. })
    ));

    // process mutate(state: { mode: enum["default"] }) { state.mode = "nope" }
    let container = builders::module(
        vec![builders::process(
            "mutate",
            vec![builders::param(
                "state",
                TypeExpr::Object(vec![builders::type_field(
                    "mode",
                    TypeExpr::Enum(vec!["default".into()]),
                    false,
                )]),
            )],
            builders::block(vec![builders::assign_path(
                "state",
                vec![builders::field_step("mode")],
                builders::string("nope"),
            )]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(container, full_host_environment()),
        Err(LinkError::IncompatibleExpectedLiteral { .. })
    ));

    // process choose() -> enum["default"] { finish "nope" }
    let declared_return = builders::module(
        vec![builders::process_returning(
            "choose",
            Vec::new(),
            TypeExpr::Enum(vec!["default".into()]),
            builders::block(vec![builders::finish(builders::string("nope"))]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(declared_return, full_host_environment()),
        Err(LinkError::IncompatibleExpectedLiteral { .. })
    ));
}

#[test]
fn branch_assignments_join_to_a_union_instead_of_first_wins() {
    // process choose(flag: bool) {
    //   value = "initial"
    //   if flag { value = "text" } else { value = 1 }
    //   await tools.accept_str(value)?
    // }
    let program = builders::module(
        vec![builders::process(
            "choose",
            vec![builders::param("flag", TypeExpr::Bool)],
            builders::block(vec![
                builders::assign("value", builders::string("initial")),
                builders::if_else(
                    builders::var("flag"),
                    builders::block(vec![builders::assign("value", builders::string("text"))]),
                    builders::block(vec![builders::assign("value", builders::num(1.0))]),
                ),
                builders::module_call(&["tools"], "accept_str", vec![builders::var("value")]),
            ]),
        )],
        Vec::new(),
    );

    assert!(matches!(
        LinkedModule::link(program, full_host_environment()),
        Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual.contains('|')
    ));
}

#[test]
fn for_bindings_use_list_elements_and_unknown_iterables_remain_gradual() {
    // process consume() { for item in ["one", "two"] { await tools.accept_int(item)? } }
    let known = consume_process(
        Vec::new(),
        builders::list(vec![builders::string("one"), builders::string("two")]),
    );
    assert!(matches!(
        LinkedModule::link(known, full_host_environment()),
        Err(LinkError::IncompatibleOperationInput { .. })
    ));

    // process consume(items: any) { for item in items { await tools.accept_int(item)? } }
    let unknown = consume_process(
        vec![builders::param("items", TypeExpr::Any)],
        builders::var("items"),
    );
    LinkedModule::link(unknown, full_host_environment())
        .expect("unknown iterable elements should remain gradual");

    // process consume() { for item in "not a list" { seen = item } }
    let non_list = builders::module(
        vec![builders::process(
            "consume",
            Vec::new(),
            builders::block(vec![builders::for_in(
                "item",
                builders::string("not a list"),
                builders::block(vec![builders::assign("seen", builders::var("item"))]),
            )]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(non_list, full_host_environment()),
        Err(LinkError::IncompatibleIterationTarget { .. })
    ));
}

#[test]
fn field_assignments_update_the_tracked_object_field_type() {
    // state = { value: "text" }
    // state.value = 1
    // await tools.accept_str(state.value)?
    let program = builders::program(vec![
        builders::assign(
            "state",
            builders::record(vec![("value", builders::string("text"))]),
        ),
        builders::assign_path(
            "state",
            vec![builders::field_step("value")],
            builders::num(1.0),
        ),
        builders::module_call(
            &["tools"],
            "accept_str",
            vec![builders::field(builders::var("state"), "value")],
        ),
    ]);

    assert!(matches!(
        LinkedModule::link(program, full_host_environment()),
        Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual == "int"
    ));
}

#[test]
fn missing_known_object_fields_are_errors_but_open_shapes_stay_gradual() {
    // value = { present: 1 }
    // finish value.missing
    let known = builders::program(vec![
        builders::assign(
            "value",
            builders::record(vec![("present", builders::num(1.0))]),
        ),
        builders::finish(builders::field(builders::var("value"), "missing")),
    ]);
    assert!(matches!(
        LinkedModule::link(known, full_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "missing"
    ));

    // process inspect(map: dict, unknown: any) { finish [map.missing, unknown.missing] }
    let open = builders::module(
        vec![builders::process(
            "inspect",
            vec![
                builders::param("map", TypeExpr::Dict),
                builders::param("unknown", TypeExpr::Any),
            ],
            builders::block(vec![builders::finish(builders::list(vec![
                builders::field(builders::var("map"), "missing"),
                builders::field(builders::var("unknown"), "missing"),
            ]))]),
        )],
        Vec::new(),
    );
    LinkedModule::link(open, full_host_environment())
        .expect("dict and any field access should stay gradual");
}

#[test]
fn union_field_assignments_update_matching_members_and_reject_unknown_fields() {
    // process mutate(flag: bool) {
    //   value = { a: 0 }
    //   if flag { value = { a: 0 } } else { value = { b: 0 } }
    //   value.a = 1
    // }
    let matching = union_field_mutation("a");
    LinkedModule::link(matching, full_host_environment())
        .expect("a field present on one union member should remain assignable");

    // process mutate(flag: bool) {
    //   value = { a: 0 }
    //   if flag { value = { a: 0 } } else { value = { b: 0 } }
    //   value.c = 1
    // }
    let missing = union_field_mutation("c");
    assert!(matches!(
        LinkedModule::link(missing, full_host_environment()),
        Err(LinkError::UnknownObjectField { field, .. }) if field == "c"
    ));
}

#[test]
fn binary_operators_reject_known_category_errors_but_admit_unknown_maps() {
    // finish {} + 1
    let known = builders::program(vec![builders::finish(builders::binary(
        builders::record(Vec::new()),
        BinaryOp::Add,
        builders::num(1.0),
    ))]);
    assert!(matches!(
        LinkedModule::link(known, full_host_environment()),
        Err(LinkError::IncompatibleBinaryOperands { .. })
    ));

    // process combine(map: dict, unknown: any) {
    //   left = map + 1
    //   finish left + unknown
    // }
    let gradual = builders::module(
        vec![builders::process(
            "combine",
            vec![
                builders::param("map", TypeExpr::Dict),
                builders::param("unknown", TypeExpr::Any),
            ],
            builders::block(vec![
                builders::assign(
                    "left",
                    builders::binary(builders::var("map"), BinaryOp::Add, builders::num(1.0)),
                ),
                builders::finish(builders::binary(
                    builders::var("left"),
                    BinaryOp::Add,
                    builders::var("unknown"),
                )),
            ]),
        )],
        Vec::new(),
    );
    LinkedModule::link(gradual, full_host_environment())
        .expect("dict and any operands should stay gradual");
}

#[test]
fn equality_accepts_a_compatible_union_member_but_rejects_known_category_mismatches() {
    // process compare(flag: bool, number: int) {
    //   value = "initial"
    //   if flag { value = number } else { value = "text" }
    //   equal = value == "text"
    //   not_equal = "text" != value
    //   finish [equal, not_equal]
    // }
    let union = builders::module(
        vec![builders::process(
            "compare",
            vec![
                builders::param("flag", TypeExpr::Bool),
                builders::param("number", TypeExpr::Int),
            ],
            builders::block(vec![
                builders::assign("value", builders::string("initial")),
                builders::if_else(
                    builders::var("flag"),
                    builders::block(vec![builders::assign("value", builders::var("number"))]),
                    builders::block(vec![builders::assign("value", builders::string("text"))]),
                ),
                builders::assign(
                    "equal",
                    builders::binary(
                        builders::var("value"),
                        BinaryOp::Equal,
                        builders::string("text"),
                    ),
                ),
                builders::assign(
                    "not_equal",
                    builders::binary(
                        builders::string("text"),
                        BinaryOp::NotEqual,
                        builders::var("value"),
                    ),
                ),
                builders::finish(builders::list(vec![
                    builders::var("equal"),
                    builders::var("not_equal"),
                ])),
            ]),
        )],
        Vec::new(),
    );
    LinkedModule::link(union, full_host_environment())
        .expect("equality should accept a category-compatible union member");

    // finish {} == 1
    let incompatible = builders::program(vec![builders::finish(builders::binary(
        builders::record(Vec::new()),
        BinaryOp::Equal,
        builders::num(1.0),
    ))]);
    assert!(matches!(
        LinkedModule::link(incompatible, full_host_environment()),
        Err(LinkError::IncompatibleBinaryOperands { .. })
    ));
}

#[test]
fn loop_carried_mutation_is_widened_after_one_forward_pass() {
    // state = { value: "before" }
    // while true { state.value = 1 }
    // await tools.accept_float(state.value)?
    let program = builders::program(vec![
        builders::assign(
            "state",
            builders::record(vec![("value", builders::string("before"))]),
        ),
        builders::while_loop(
            builders::bool_lit(true),
            builders::block(vec![builders::assign_path(
                "state",
                vec![builders::field_step("value")],
                builders::num(1.0),
            )]),
        ),
        builders::module_call(
            &["tools"],
            "accept_float",
            vec![builders::field(builders::var("state"), "value")],
        ),
    ]);

    assert!(matches!(
        LinkedModule::link(program, full_host_environment()),
        Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual.contains("str") && actual.contains("int")
    ));
}
