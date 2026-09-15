//! The `inputs` arrow: the fired trigger event is a parameter, not a global.
//!
//! `trigger.event` was bound nowhere in a TypeScript program — the one place
//! the dialect stopped being TypeScript. The event is now the parameter of an
//! `inputs` arrow, and the three ways a model reaches for the old shape are
//! named diagnostics rather than binding errors (GitHub #1350, FIG-2986).
//!
//! Byte-identity between the arrow and the record it replaces is proved where
//! the artifact, hash, identity, bytecode and registration payload are all
//! observable at once, in
//! `lash-internal-protocol-rlm`'s `trigger_inputs_arrow_reproduces_the_retired_record_form`.

use lash_typescript::DiagnosticCode;

fn environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    lashlang::add_trigger_resource_operations(&mut catalog)
        .expect("trigger resource operations are unique");
    catalog
        .add_trigger_source_constructor(
            ["timer", "Schedule"],
            lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                name: "expr".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }]),
            lashlang::NamedDataType::object(
                "timer.Tick",
                vec![lashlang::TypeField {
                    name: "fired_at".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid timer tick type"),
        )
        .expect("timer trigger source");
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:processes/start",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": true,
                    "properties": { "definition": { "x-lash": { "kind": "process_unknown" } } },
                    "required": ["definition"]
                }),
                serde_json::json!({ "x-lash": { "kind": "handle", "payload": {} } }),
            ),
        )
        .expect("process start operation");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
}

/// A program whose target takes `params`, registering with `inputs`.
fn program(params: &str, inputs: &str) -> String {
    let inputs = if inputs.is_empty() {
        String::new()
    } else {
        format!("  inputs: {inputs},\n")
    };
    format!(
        r#"
        const remember = async ({params}) => {{ return true; }};
        const schedule = timer.Schedule({{ expr: "0 8 * * *" }});
        finish(await triggers.register({{
          source: schedule,
          target: remember,
        {inputs}  subscription_key: "remembered-key"
        }}));
        "#
    )
}

fn reject(source: &str) -> lash_typescript::Diagnostic {
    lash_typescript::link(source, &environment()).expect_err("this program must not link")
}

fn accept(source: &str) {
    lash_typescript::link(source, &environment()).expect("this program must link");
}

#[test]
fn the_arrow_template_binds_the_event_and_freezes_every_other_input() {
    accept(&program(
        "tick: unknown, label: unknown",
        "(event) => ({ tick: event, label: \"daily\" })",
    ));
}

/// The same contract on every operation that takes a registration record.
/// Retiring the global for `triggers.register` alone would strand these three.
#[test]
fn register_update_and_revive_share_the_arrow() {
    for (operation, extra) in [
        ("register", ""),
        ("update", ", expected_revision: 1"),
        ("revive", ", expected_revision: 1"),
    ] {
        let source = format!(
            r#"
            const remember = async (tick: unknown) => {{ return true; }};
            const schedule = timer.Schedule({{ expr: "0 8 * * *" }});
            finish(await triggers.{operation}({{
              source: schedule,
              target: remember,
              inputs: (event) => ({{ tick: event }}),
              subscription_key: "remembered-key"{extra}
            }}));
            "#
        );
        lash_typescript::link(&source, &environment())
            .unwrap_or_else(|error| panic!("triggers.{operation}: {error}"));
    }
}

#[test]
fn omitted_inputs_defaults_to_a_one_parameter_target() {
    accept(&program("tick: unknown", ""));
}

#[test]
fn a_zero_parameter_target_is_told_to_take_an_event_parameter() {
    let error = reject(&program("", ""));
    assert_eq!(error.code, DiagnosticCode::LinkError, "{error}");
    assert!(
        error.message.contains("takes no parameters")
            && error
                .message
                .contains("give the process an event parameter"),
        "{error}"
    );
}

#[test]
fn a_multi_parameter_target_cannot_omit_inputs() {
    let error = reject(&program("tick: unknown, label: unknown", ""));
    assert_eq!(error.code, DiagnosticCode::LinkError, "{error}");
    assert!(
        error.message.contains("takes 2 parameters")
            && error.message.contains("map every parameter explicitly"),
        "{error}"
    );
}

/// The reported defect: a model builds the source, then reads `.event` off it.
/// The descriptor is opaque and one source can feed many registrations, so the
/// answer is not a better property — it is the arrow.
#[test]
fn reading_event_off_the_source_descriptor_names_the_arrow() {
    let error = reject(&program("tick: unknown", "{ tick: schedule.event }"));
    assert_eq!(
        error.code,
        DiagnosticCode::TriggerSourceEventAccess,
        "{error}"
    );
    assert_eq!(
        error.message,
        "a trigger source descriptor is opaque and has no `event` property: one source can feed many registrations, so the fired event does not belong to it"
    );
    assert_eq!(
        error.suggestions,
        vec![
            "pass the fired event as the `inputs` arrow's parameter: `inputs: (event) => ({ tick: event })`, where `tick` is the target's parameter name; omit `inputs` entirely when the target takes exactly one parameter".to_string()
        ]
    );
    assert!(error.is_dialect_refusal(), "{error}");
}

#[test]
fn the_retired_global_names_the_arrow() {
    let error = reject(&program("tick: unknown", "{ tick: trigger.event }"));
    assert_eq!(error.code, DiagnosticCode::TriggerEventRemoved, "{error}");
    assert_eq!(
        error.message,
        "`trigger` is bound nowhere in a TypeScript program and `trigger.event` is no longer part of the dialect"
    );
    assert!(error.is_dialect_refusal(), "{error}");
}

/// The retired spelling is refused wherever it is written, not only inside a
/// registration: a cell that reaches for it anywhere gets the same answer.
#[test]
fn the_retired_global_is_refused_outside_a_registration() {
    let error = lash_typescript::link("finish(trigger.event);", &environment())
        .expect_err("the global is gone");
    assert_eq!(error.code, DiagnosticCode::TriggerEventRemoved, "{error}");
}

/// Scoped to the magic unbound spelling. A program that binds `trigger` reads
/// its own value, here as anywhere else.
///
/// Reading `.event` off such a binding is the one residue: the shared linker
/// still reserves the `trigger.event` *path shape* for the Lashlang surface,
/// which is a Lashlang rule and not this dialect's diagnostic. The assertion
/// below is that the retired-global diagnostic does not fire, not that the
/// shared reservation is gone; it goes when the Lashlang surface does.
#[test]
fn a_bound_local_named_trigger_is_unaffected() {
    lash_typescript::link(
        "const trigger = { id: 7 }; finish(trigger.id);",
        &environment(),
    )
    .expect("a bound `trigger` is an ordinary object");
    let error = lash_typescript::link(
        "const trigger = { event: 7 }; finish(trigger.event);",
        &environment(),
    )
    .expect_err("the shared linker still owns this path shape");
    assert_ne!(error.code, DiagnosticCode::TriggerEventRemoved, "{error}");
}

/// The shape is judged before the values lower. `inputs: { event }` has an
/// unbound `event` inside it; reporting that binding would send a model
/// hunting for a declaration instead of changing the shape.
#[test]
fn an_object_valued_inputs_reports_its_shape_not_an_unbound_name() {
    let error = reject(&program("tick: unknown", "{ tick }"));
    assert_eq!(
        error.code,
        DiagnosticCode::TriggerInputsLiteralRequired,
        "{error}"
    );
    assert_eq!(
        error.message,
        "`inputs` must be an arrow literal that names the fired event"
    );
    assert!(error.is_dialect_refusal(), "{error}");
}

#[test]
fn the_template_rejects_every_shape_that_is_not_an_erasable_arrow() {
    let cases = [
        ("a block body", "(event) => { return { tick: event }; }"),
        ("an async arrow", "async (event) => ({ tick: event })"),
        ("no parameter", "() => ({ tick: 1 })"),
        ("two parameters", "(event, extra) => ({ tick: event })"),
        (
            "a destructured parameter",
            "({ event }) => ({ tick: event })",
        ),
        ("a defaulted parameter", "(event = 1) => ({ tick: event })"),
        ("a non-object body", "(event) => event"),
        ("a computed key", "(event) => ({ [\"tick\"]: event })"),
        ("a spread", "(event) => ({ ...event })"),
        ("a duplicate key", "(event) => ({ tick: event, tick: 1 })"),
        ("a projection", "(event) => ({ tick: event.fired_at })"),
        ("a nested use", "(event) => ({ tick: { inner: event } })"),
        ("a call", "(event) => ({ tick: String(event) })"),
        ("a capture", "(event) => ({ tick: () => event })"),
    ];
    for (label, inputs) in cases {
        let error = reject(&program("tick: unknown", inputs));
        assert_eq!(
            error.code,
            DiagnosticCode::TriggerInputsLiteralRequired,
            "{label}: {error}"
        );
        assert!(error.is_dialect_refusal(), "{label}: {error}");
        assert!(!error.suggestions.is_empty(), "{label}: {error}");
    }
}

/// The arrow is a template, so its parameter is not a binding: a value that
/// happens to mention the parameter name is refused rather than silently
/// reading an outer value of the same name.
#[test]
fn a_fixed_value_is_an_ordinary_expression_in_the_enclosing_scope() {
    accept(&program(
        "tick: unknown, label: unknown",
        "(event) => ({ tick: event, label: \"a\" + \"b\" })",
    ));
    let error = reject(
        r#"
        const event = "outer";
        const remember = async (tick: unknown, label: unknown) => { return true; };
        const schedule = timer.Schedule({ expr: "0 8 * * *" });
        finish(await triggers.register({
          source: schedule,
          target: remember,
          inputs: (event) => ({ tick: event, label: event }),
          subscription_key: "remembered-key"
        }));
        "#,
    );
    assert_eq!(
        error.code,
        DiagnosticCode::TriggerInputsLiteralRequired,
        "{error}"
    );
}

/// A process is a value, so a target reaches the registration through any
/// expression that evaluates to one.
///
/// The retired `ProcessTargetStaticRequired` rule existed because `start` and
/// the registration took a *binding name* rather than a value; with the forms
/// gone, an alias, a record field and a call all resolve to the same lifted
/// declaration, and nothing is left to spell wrong.
#[test]
fn a_trigger_target_is_any_expression_that_names_a_process() {
    for target in ["remember", "alias"] {
        let source = format!(
            r#"
            const remember = async (tick: unknown) => {{ return true; }};
            const alias = remember;
            const schedule = timer.Schedule({{ expr: "0 8 * * *" }});
            finish(await triggers.register({{
              source: schedule,
              target: {target},
              inputs: (event) => ({{ tick: event }})
            }}));
            "#
        );
        lash_typescript::link(&source, &environment())
            .unwrap_or_else(|error| panic!("{target}: {error}"));
    }
    accept(&program("tick: unknown", "(event) => ({ tick: event })"));
}

/// FIG-3059: a process body could not register a trigger aimed at another
/// process. The target had to be a literal top-level `defineProcess` binding,
/// and reading that binding from inside the body was a capture, which the
/// process body refuses — the two rules were jointly unsatisfiable.
///
/// A target now lowers to the process it names rather than to a read of the
/// binding that holds it, so there is nothing to capture.
#[test]
fn a_process_can_register_a_trigger_aimed_at_another_process() {
    accept(
        r#"
        const remember = async (tick: unknown) => { return true; };
        const owner = async () => {
          const schedule = timer.Schedule({ expr: "0 8 * * *" });
          await triggers.register({
            source: schedule,
            target: remember,
            inputs: (event) => ({ tick: event }),
            subscription_key: "remembered-key"
          });
          return true;
        };
        finish(await processes.start({ definition: owner }));
        "#,
    );
}

/// Source order is the registration order: two registrations in one process
/// body reach the program in the order they are written.
#[test]
fn registrations_in_a_process_body_keep_their_source_order() {
    accept(
        r#"
        const first = async (tick: unknown) => { return true; };
        const second = async (tick: unknown) => { return true; };
        const owner = async () => {
          const schedule = timer.Schedule({ expr: "0 8 * * *" });
          await triggers.register({
            source: schedule, target: first,
            inputs: (event) => ({ tick: event }),
            subscription_key: "first-key"
          });
          await triggers.register({
            source: schedule, target: second,
            inputs: (event) => ({ tick: event }),
            subscription_key: "second-key"
          });
          return true;
        };
        finish(await processes.start({ definition: owner }));
        "#,
    );
}

/// The declared type of a `run` parameter is the process's durable input
/// shape, so the linker has to carry it rather than drop it: it is what a
/// registration is checked against before any foreground effect runs
/// (FIG-3071).
fn process_params(source: &str) -> Vec<(String, lashlang::TypeExpr)> {
    let linked = lash_typescript::link(source, &environment()).expect("this program must link");
    linked
        .artifact
        .canonical_ir
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) => Some(process.params.clone()),
            _ => None,
        })
        .expect("the program declares a process")
        .into_iter()
        .map(|param| (param.name.to_string(), param.ty))
        .collect()
}

#[test]
fn a_declared_parameter_type_reaches_the_process_signature() {
    assert_eq!(
        process_params(&program(
            "tick: timer.Tick, label: string, retries: number, ok: boolean",
            "(event) => ({ tick: event, label: \"daily\", retries: 1, ok: true })",
        )),
        vec![
            (
                "tick".to_string(),
                lashlang::TypeExpr::Ref("timer.Tick".into())
            ),
            ("label".to_string(), lashlang::TypeExpr::Str),
            ("retries".to_string(), lashlang::TypeExpr::Float),
            ("ok".to_string(), lashlang::TypeExpr::Bool),
        ]
    );
}

/// An object literal, an array and a union of string literals are the three
/// composite shapes the runtime's type language carries; the enumeration is
/// one type there rather than a union of single-value types.
#[test]
fn composite_declared_parameter_types_lower_to_their_runtime_shapes() {
    assert_eq!(
        process_params(&program(
            "tick: timer.Tick, record: { id: string; note?: string }, tags: string[], mode: \"fast\" | \"slow\"",
            "(event) => ({ tick: event, record: { id: \"a\" }, tags: [\"a\"], mode: \"fast\" })",
        )),
        vec![
            (
                "tick".to_string(),
                lashlang::TypeExpr::Ref("timer.Tick".into())
            ),
            (
                "record".to_string(),
                lashlang::TypeExpr::Object(vec![
                    lashlang::TypeField {
                        name: "id".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    },
                    lashlang::TypeField {
                        name: "note".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: true,
                    },
                ])
            ),
            (
                "tags".to_string(),
                lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::Str))
            ),
            (
                "mode".to_string(),
                lashlang::TypeExpr::Enum(vec!["fast".into(), "slow".into()])
            ),
        ]
    );
}

/// Existing programs must not move: an unannotated parameter, and one written
/// `unknown`, both stay gradual, so their canonical IR and module ref are the
/// bytes they already were.
#[test]
fn an_unannotated_parameter_stays_gradual() {
    assert_eq!(
        process_params(&program(
            "tick, label: unknown",
            "(event) => ({ tick: event, label: \"daily\" })",
        )),
        vec![
            ("tick".to_string(), lashlang::TypeExpr::Any),
            ("label".to_string(), lashlang::TypeExpr::Any),
        ]
    );
}

/// Widening an undeclarable type to `Any` would let a mismatched registration
/// through silently, so it is refused — naming the process, the parameter and
/// the construct, at the annotation.
#[test]
fn a_parameter_type_the_runtime_cannot_carry_is_refused() {
    let diagnostic = reject(&program(
        "tick: [string, number]",
        "(event) => ({ tick: event })",
    ));
    assert_eq!(diagnostic.code, DiagnosticCode::ProcessParamTypeUnsupported);
    assert_eq!(
        diagnostic.message,
        "process `remember` parameter `tick` declares a tuple type, which has no durable type"
    );
    assert!(diagnostic.span.is_some(), "{diagnostic:?}");
}
