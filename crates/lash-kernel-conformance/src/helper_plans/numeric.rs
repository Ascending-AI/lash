//! Numeric packet bodies and typed forwards obey PLAN-INDEP and K-CHG-003.

use super::*;

/// PLAN-INDEP / K-CHG-003: generated typed helper paths agree with their
/// bodies, including the completion tape, raises and depth refusals.
#[test]
fn generated_scalar_contracts_agree_with_their_bodies() {
    use lash_kernel_doc::{Float, Value};

    use std::sync::Arc;
    let samples = [
        "null",
        "absent",
        "false",
        "true",
        "0",
        "1",
        "-1",
        "9223372036854775807",
        "0.0",
        "-0.0",
        "1.5",
        "nan",
        "inf",
        "\"\"",
        "\"x\"",
    ];
    for (dialect, names) in [
        (
            "python",
            &[
                "py.add",
                "py.sub",
                "py.mul",
                "py.truediv",
                "py.floordiv",
                "py.mod",
                "py.neg",
                "py.pos",
                "py.truth",
                "py.equal",
                "py.same",
                "py.str",
            ][..],
        ),
        (
            "typescript",
            &[
                "ts.add",
                "ts.sub",
                "ts.mul",
                "ts.div",
                "ts.rem",
                "ts.neg",
                "ts.to_boolean",
                "ts.not",
                "ts.strict_equals",
                "ts.strict_not_equals",
                "ts.lt",
                "ts.le",
                "ts.gt",
                "ts.ge",
                "ts.number.format_value",
                "ts.to_string",
                "ts.to_number",
                "ts.to_numeric",
            ][..],
        ),
    ] {
        let runner = runner::Runner::new(dialect).unwrap();
        let case = runner::Case {
            id: dialect.into(),
            dialect: dialect.into(),
            source: if dialect == "python" { "None" } else { "null;" }.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
            panic!("template fixture lowers")
        };
        let registry = base.library.registry();
        let general = in_layout(&base.library, Layout(1));
        let bodies = in_layout(&base.library, Layout(3));
        for name in names {
            let (function, registered) = registry
                .iter()
                .find(|(_, registered)| registered.definition().name.as_str() == *name)
                .unwrap();
            let arity = registered.definition().signature.params.len();
            let domain = registered
                .plan()
                .and_then(|plan| plan.template.as_ref())
                .and_then(|template| {
                    let native = if dialect == "python" {
                        template.forwards.ints
                    } else {
                        template.forwards.floats
                    }?;
                    Some(template.callees[usize::from(native)])
                });
            let native = domain
                .and_then(|id| registry.get(&id))
                .and_then(|function| function.native())
                .or_else(|| registered.native())
                .and_then(|native| native.prepare());
            if let Some(native) = native {
                let args = if dialect == "python" {
                    vec![Value::Int(1); arity]
                } else {
                    vec![Value::Float(Float::new(1.0)); arity]
                };
                assert!(
                    (native.entry)(&args).is_some(),
                    "{name}'s canonical native must answer"
                );
            } else if registered.native().is_none() {
                // Exercise the general body independently of any scalar
                // forwarding path, including unary identity forwarding.
                let input = if matches!(*name, "ts.to_number" | "ts.to_numeric") {
                    "true"
                } else if dialect == "python" {
                    "1"
                } else {
                    "1.0"
                };
                let args = vec![input; arity];
                let source = format!(
                    "kernel 1\nnumbers by_spelling\n# local definition op @{function}\nmain {{ let value = invoke @{function}({}) return value }}",
                    args.join(", ")
                );
                let mut document = lash_kernel_doc::parse_document(&source).unwrap();
                document.library.insert(*function);
                let program = crate::linked::program(Arc::new(document), general.clone());
                let (_, census) = crate::projection::counted(|| {
                    run::<1>(&case, &program, &mut Projector::default())
                });
                assert!(
                    census.register_entries > 0,
                    "{name}'s non-forwarding body must enter the packet stream: {census:?}"
                );
            }
            // Every depth near the elided helper frames uses the same
            // refusal point, charge prefix and function name as the body.
            for call_depth in 0..=6 {
                let args = vec![if dialect == "python" { "1" } else { "1.0" }; arity];
                let source = format!(
                    "kernel 1\nnumbers by_spelling\n# local definition op @{function}\nmain {{ let value = invoke @{function}({}) return value }}",
                    args.join(", ")
                );
                // The helper is already in this immutable build image. Do
                // not copy its complete definition closure into every sample.
                let mut document = lash_kernel_doc::parse_document(&source).unwrap();
                document.library.insert(*function);
                let mut program = crate::linked::program(Arc::new(document), base.library.clone());
                let expected = run_at_depth::<0>(
                    &case,
                    &program,
                    &mut Projector::default(),
                    50_000_000,
                    call_depth,
                );
                program.library = bodies.clone();
                assert_eq!(
                    run_at_depth::<3>(
                        &case,
                        &program,
                        &mut Projector::default(),
                        50_000_000,
                        call_depth
                    ),
                    expected,
                    "{name} depth {call_depth}"
                );
            }
            let mut answered = 0;
            for index in 0..samples.len().pow(arity as u32) {
                let mut selected = index;
                let args: Vec<_> = (0..arity)
                    .map(|_| {
                        let value = samples[selected % samples.len()];
                        selected /= samples.len();
                        value
                    })
                    .collect();
                let source = format!(
                    "kernel 1\nnumbers by_spelling\n# local definition op @{function}\nmain {{ let value = invoke @{function}({}) return value }}",
                    args.join(", ")
                );
                // The helper is already in this immutable build image. Do
                // not copy its complete definition closure into every sample.
                let mut document = lash_kernel_doc::parse_document(&source).unwrap();
                document.library.insert(*function);
                let mut program = crate::linked::program(Arc::new(document), base.library.clone());
                let case = runner::Case {
                    id: format!("{name}({})", args.join(", ")),
                    ..case.clone()
                };
                let expected = run::<0>(&case, &program, &mut Projector::default());
                program.library = general.clone();
                assert_eq!(
                    run::<1>(&case, &program, &mut Projector::default()),
                    expected,
                    "{} general",
                    case.id
                );
                program.library = bodies.clone();
                assert_eq!(
                    run::<3>(&case, &program, &mut Projector::default()),
                    expected,
                    "{} body",
                    case.id
                );
                answered += 1;
            }
            println!("{name}: {answered} contract/body cases");
        }
    }
}

/// FIG-6959 / PLAN-INDEP: Python's integer floor operators forward to the
/// native kernels, and every declined pair (zero and word overflow) keeps
/// the body's value or Python exception and completion charge. Floats keep
/// the packet body, including negative zero and mixed numeric kinds.
#[test]
fn python_floor_forwards_agree_with_their_bodies_on_edge_pairs() {
    use lash_kernel_doc::scalar::ScalarOp;

    use std::sync::Arc;
    let runner = runner::Runner::new("python").unwrap();
    let case = runner::Case {
        id: "python-floor".into(),
        dialect: "python".into(),
        source: "None".into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
        panic!("the numeric fixture lowers")
    };
    let registry = base.library.registry();
    let header = ["lt", "list.len"]
        .into_iter()
        .map(|name| {
            let (id, _) = registry
                .iter()
                .find(|(_, entry)| entry.definition().name.as_str() == name)
                .unwrap();
            format!("# local definition {name} @{id}\n")
        })
        .collect::<String>();
    let general = in_layout(&base.library, Layout(1));
    let bodies = in_layout(&base.library, Layout(3));
    let pairs = [
        ("0", "0"),
        ("1", "0"),
        ("-1", "0"),
        ("7", "3"),
        ("-7", "3"),
        ("7", "-3"),
        ("-7", "-3"),
        ("6", "-3"),
        ("-6", "3"),
        ("0", "-1"),
        ("-9223372036854775808", "-1"),
        ("-9223372036854775808", "3"),
        ("-9223372036854775808", "9223372036854775807"),
        ("9223372036854775807", "-9223372036854775808"),
        ("9223372036854775807", "-3"),
        ("-1", "-9223372036854775808"),
        ("9223372036854775808", "-3"),
        ("-7.5", "3.0"),
        ("7.5", "-3.0"),
        ("-0.0", "3.0"),
        ("1.0", "0.0"),
        ("7", "-3.0"),
        ("-7.5", "3"),
    ];
    for (name, op) in [
        ("py.mod", ScalarOp::RemFloor),
        ("py.floordiv", ScalarOp::FloorDiv),
    ] {
        let (function, registered) = registry
            .iter()
            .find(|(_, registered)| registered.definition().name.as_str() == name)
            .unwrap();
        let template = registered.plan().unwrap().template.as_ref().unwrap();
        assert_eq!(
            template.forwards.floats, None,
            "{name}: floats keep the packet body"
        );
        let native = template.callees
            [usize::from(template.forwards.ints.expect("the integer arm forwards"))];
        let pair = registry
            .get(&native)
            .unwrap()
            .native()
            .unwrap()
            .prepare()
            .unwrap()
            .pair
            .unwrap();
        assert_eq!(pair.ints, Some(op));
        assert_eq!(pair.floats, None);
        assert!(lash_kernel_doc::scalar::ints(op, 1, 0).is_none());
        for (a, b) in pairs {
            for body in [
                format!(
                    "main {{ let i = 0 let v = 0 while lt(i, 1) {{ set v = invoke @{function}({a}, {b}) set i = 1 }} return v }}"
                ),
                format!(
                    "main {{ let out = [] for a in [{a}] {{ let v = invoke @{function}(a, {b}) set out[list.len(out)] = v }} return out }}"
                ),
                format!(
                    "definition \"function calculate(a: Any, b: Any) -> Any\\nkernel 1\\nmetering statements\\nimport 0 operation host.echo(x: Any) -> Any\\nbody {{ let v = invoke @{function}(a, b) return v }}\"\n main {{ let v = call calculate({a}, {b}) return v }}"
                ),
            ] {
                let source = format!(
                    "kernel 1\nnumbers by_spelling\n# local definition op @{function}\n{}\n{body}",
                    header
                );
                let document = Arc::new(crate::document_fixture::authored(
                    &source,
                    base.library.registry(),
                ));
                let program = crate::machine::prepare_document(&base.library, document, None)
                    .expect("prepare numeric witness");
                let expected = run::<0>(&case, &program, &mut Projector::default());
                assert_eq!(
                    run::<1>(
                        &case,
                        &Program {
                            library: general.clone(),
                            ..program.clone()
                        },
                        &mut Projector::default()
                    ),
                    expected,
                    "{name}({a}, {b}) general"
                );
                assert_eq!(
                    run::<3>(
                        &case,
                        &Program {
                            library: bodies.clone(),
                            ..program.clone()
                        },
                        &mut Projector::default()
                    ),
                    expected,
                    "{name}({a}, {b}) body"
                );
            }
        }
    }
}
