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
    lashlang::LashlangHostEnvironment::new(
        catalog,
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_triggers(),
    )
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
        const remember = defineProcess({{
          name: "remember", signals: {{}},
          run: async ({params}) => {{ return true; }}
        }});
        const schedule = timer.Schedule({{ expr: "0 8 * * *" }});
        finish(await registerTrigger({{
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
/// Retiring the global for `registerTrigger` alone would strand these three.
#[test]
fn register_update_and_revive_share_the_arrow() {
    for (operation, extra) in [
        ("register", ""),
        ("update", ", expected_revision: 1"),
        ("revive", ", expected_revision: 1"),
    ] {
        let source = format!(
            r#"
            const remember = defineProcess({{
              name: "remember", signals: {{}},
              run: async (tick: unknown) => {{ return true; }}
            }});
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
        const remember = defineProcess({
          name: "remember", signals: {},
          run: async (tick: unknown, label: unknown) => { return true; }
        });
        const schedule = timer.Schedule({ expr: "0 8 * * *" });
        finish(await registerTrigger({
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
