use super::*;
use crate::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
    LinkedModule, NamedDataType, TypeField, canonical_program_ir, parse,
};

fn host_catalog() -> LashlangHostCatalog {
    let mut catalog = LashlangHostCatalog::new();
    catalog.add_module_operation(
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
    );
    catalog.add_module_operation(
        ["tools"],
        "Tools",
        "echo",
        "echo",
        TypeExpr::Any,
        TypeExpr::Any,
    );
    crate::add_trigger_resource_operations(&mut catalog);
    catalog
        .add_trigger_source_constructor(
            ["timer", "Schedule"],
            TypeExpr::Object(vec![
                TypeField {
                    name: "expr".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "tz".into(),
                    ty: TypeExpr::Str,
                    optional: true,
                },
            ]),
            NamedDataType::object(
                "timer.Tick",
                vec![TypeField {
                    name: "fired_at".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid tick type"),
        )
        .expect("valid trigger source");
    catalog
}

fn host_environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(host_catalog(), LashlangAbilities::all())
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

fn assert_linked_source_round_trip(source: &str) -> LinkedModule {
    let surface = host_environment();
    let linked = LinkedModule::link(parse(source).expect("parse original"), &surface)
        .expect("link original");
    let rendered = linked
        .artifact
        .canonical_source()
        .expect("render canonical source");
    let reparsed = LinkedModule::link(parse(&rendered).expect("parse rendered"), &surface)
        .unwrap_or_else(|err| panic!("rendered source failed to link:\n{rendered}\n{err}"));
    assert_eq!(reparsed.module_ref, linked.module_ref, "{rendered}");
    assert_eq!(
        reparsed.host_requirements_ref, linked.host_requirements_ref,
        "{rendered}"
    );
    assert_eq!(
        reparsed.artifact.exports, linked.artifact.exports,
        "{rendered}"
    );
    assert_eq!(
        reparsed.artifact.canonical_ir, linked.artifact.canonical_ir,
        "{rendered}"
    );
    linked
}

#[test]
fn canonical_source_round_trips_complex_linked_module() {
    let source = r#"
    @label(title: "Handle tick", description: "Read file and return data")
    process handle_tick(tick: timer.Tick, tool: Tools) signals { stop: str } -> { ok: bool } {
      @label(title: "Read")
      text = await tool.read_file({ path: tick.fired_at })?
      if text == "" {
        finish { ok: false }
      } else if text == "stop" {
        fail "stopped"
      } else {
        signal_run(start handle_tick(tick: tick, tool: tool), "stop", text)
        finish { ok: true }
      }
    }

    process summarize(tick: timer.Tick) -> str {
      values = [1, 2, 3]
      total = 0
      for value in values {
        if value == 2 {
          continue
        }
        total = total + value
      }
      while total < 5 {
        total = total + 1
      }
      finish to_string(total)
    }

    source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
    handle = await triggers.register({
      source: source,
      target: summarize,
      inputs: { tick: trigger.event },
      name: "daily"
    })?
    finish { handle: handle, source: source }
    "#;

    assert_linked_source_round_trip(source);
}

#[test]
fn canonical_source_round_trips_precedence_and_literals() {
    let source = r#"
    type Payload = { "odd-key": enum["yes", "no"], maybe: str | null, list: list[int]? }

    process inspect(payload: Payload) -> any {
      value = ((1 + 2) * 3) >= 9 and not false
      picked = value ? payload."odd-key" : "fallback\nvalue"
      finish { picked: picked, type: Type { value: str | null } }
    }

    result = await tools.echo({ value: { nested: [null, true, false, 3.5] } })?
    finish result
    "#;

    assert_linked_source_round_trip(source);
}

#[test]
fn canonical_process_source_returns_focused_definition() {
    let linked = assert_linked_source_round_trip(
        r#"
        @label(title: "Worker")
        process worker(tick: timer.Tick) -> str {
          finish tick.fired_at
        }

        source = timer.Schedule({ expr: "0 8 * * *" })
        finish source
        "#,
    );
    let process_ref = linked.artifact.process_ref("worker").expect("process ref");
    let source = linked
        .artifact
        .canonical_process_source(process_ref)
        .expect("render process")
        .expect("process source");
    assert_eq!(
        source,
        r#"@label(title: "Worker")
process worker(tick: timer.Tick) -> str {
  finish tick.fired_at
}
"#
    );

    let parsed = parse(&source).expect("parse process source");
    assert_eq!(parsed.declarations.len(), 1);
    assert_eq!(
        parsed.process("worker"),
        linked.artifact.canonical_ir.process("worker")
    );
}

#[test]
fn canonical_process_source_by_name_returns_none_for_missing_process() {
    let linked = assert_linked_source_round_trip("process worker() { finish true }\nfinish true");
    assert!(
        linked
            .artifact
            .canonical_process_source_by_name("missing")
            .expect("render missing")
            .is_none()
    );
}

#[test]
fn canonical_program_source_handles_unlinked_programs_without_requirements() {
    let program = parse(
        r#"
        process scan(path: str) {
          finish path
        }

        answer = (1 + 2) * 3
        pair = 1, 2
        singleton = (answer,)
        empty = ()
        finish { pair: pair, singleton: singleton, empty: empty, scalar: (answer) }
        "#,
    )
    .expect("parse");
    let rendered = canonical_program_source(&program).expect("render source");
    assert!(rendered.contains("pair = (1, 2)"), "{rendered}");
    assert!(rendered.contains("singleton = (answer,)"), "{rendered}");
    assert!(rendered.contains("empty = ()"), "{rendered}");
    assert!(rendered.contains("scalar: answer"), "{rendered}");
    let reparsed = parse(&rendered).expect("parse rendered");
    assert_eq!(
        canonical_program_ir(reparsed),
        canonical_program_ir(program)
    );
}

#[test]
fn host_descriptor_constructor_requires_requirements_context() {
    let linked = assert_linked_source_round_trip(
        r#"source = timer.Schedule({ expr: "0 8 * * *" })
finish source"#,
    );
    let err = canonical_program_source(&linked.artifact.canonical_ir)
        .expect_err("linked constructor needs requirements");
    assert!(matches!(
        err,
        CanonicalSourceError::UnknownHostDescriptorConstructor { type_name }
            if type_name == "timer.Schedule"
    ));
}

#[test]
fn ambiguous_host_descriptor_constructor_is_rejected() {
    let mut catalog = host_catalog();
    catalog.add_value_constructor(
        ["timer", "DuplicateSchedule"],
        TypeExpr::Object(vec![]),
        TypeExpr::Ref("timer.Schedule".into()),
    );
    let requirements = HostRequirements {
        resources: catalog,
        globals: Default::default(),
        abilities: LashlangAbilities::default(),
        language_features: LashlangLanguageFeatures::default(),
    };
    let program = Program::block(vec![Expr::Finish(Box::new(
        Expr::HostDescriptorConstructor {
            type_name: "timer.Schedule".into(),
            input: Box::new(Expr::Record(vec![(
                "expr".into(),
                Expr::String("0 8 * * *".into()),
            )])),
        },
    ))]);
    let err = canonical_program_source_with_requirements(&program, &requirements)
        .expect_err("ambiguous constructor should fail");
    assert!(matches!(
        err,
        CanonicalSourceError::AmbiguousHostDescriptorConstructor { type_name, paths }
            if type_name == "timer.Schedule"
                && paths == vec!["timer.DuplicateSchedule".to_string(), "timer.Schedule".to_string()]
    ));
}

#[test]
fn unsupported_numbers_are_rejected() {
    let program = Program::block(vec![Expr::Finish(Box::new(Expr::Number(-1.0)))]);
    let err =
        canonical_program_source(&program).expect_err("negative raw number is not sourceable");
    assert!(matches!(
        err,
        CanonicalSourceError::UnsupportedNumber { value } if value == "-1"
    ));
}

#[test]
fn non_sourceable_type_shapes_are_rejected() {
    for (ty, expected_kind) in [
        (TypeExpr::Enum(Vec::new()), "empty enum"),
        (
            TypeExpr::Process {
                input: Box::new(TypeExpr::Object(vec![])),
                output: Box::new(TypeExpr::Any),
                input_count: 2,
            },
            "multi-input process",
        ),
        (TypeExpr::Union(vec![TypeExpr::Str]), "single-variant union"),
    ] {
        let program = Program::block(vec![Expr::Finish(Box::new(Expr::TypeLiteral(Box::new(
            ty,
        ))))]);
        let err = canonical_program_source(&program).expect_err("type is not sourceable");
        assert!(matches!(
            err,
            CanonicalSourceError::NonSourceableType { kind } if kind == expected_kind
        ));
    }
}
