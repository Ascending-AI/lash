//! FIG-6943 / PLAN-INDEP: prepared conversions retain the body's value,
//! completion charge, tape and bound stop, including every value kind.

use super::*;
use lash_kernel_doc::Float;
use lash_kernel_doc::PreparedNative;
use lash_kernel_doc::Value;
use lash_vocabulary::names::Name;
use std::sync::Arc;

fn observe<const L: u64>(program: &Program, charge_limit: u64) -> Observed {
    let bounds = Bounds {
        charge_limit,
        call_depth: 200,
        memory: 64 << 20,
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    };
    let session = crate::pages::Session::seeded([
        (
            Name::new("handle"),
            Datum::Handle(Handle {
                kind: "conversion".into(),
                id: "fixture".into(),
            }),
        ),
        (
            Name::new("instant"),
            Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: 0.into(),
            }),
        ),
    ]);
    let mut machine = Plan::<L>::start(
        program.clone(),
        bounds,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: session.root,
            owner: crate::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(session.source),
    )
    .unwrap();
    let tape = lash_kernel_vm::ChargeTape::new();
    machine.record_charges(tape.clone());
    let mut world = World::default();
    let end = loop {
        match machine.run(&mut world, u64::MAX).unwrap() {
            Step::Ended(end) => break end,
            Step::Slice => (),
            Step::Parked(_) => panic!("the conversion law has no effect or sleep"),
        }
    };
    let bound_in = match &end {
        lash_kernel_vm::End::Error(lash_kernel_vm::RunError::Bound(exceeded)) => {
            exceeded.function.as_ref().map(ToString::to_string)
        }
        _ => None,
    };
    Observed {
        end: crate::ExpectedEnd::from_end(end),
        bound_in,
        prints: world.prints,
        requests: Vec::new(),
        charged: machine.meters().charged.unwrap(),
        tape: tape
            .steps()
            .into_iter()
            .flat_map(|weight| match weight {
                lash_kernel_exec::Weight::Units(units) => {
                    vec![lash_kernel_exec::Weight::Units(1); units as usize]
                }
                atomic => vec![atomic],
            })
            .collect(),
    }
}

#[test]
fn prepared_typescript_conversions_agree_with_their_general_paths() {
    let runner = runner::Runner::new("typescript").unwrap();
    let case = runner::Case {
        id: "conversions".into(),
        dialect: "typescript".into(),
        source: "null;".into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
        panic!("fixture lowers")
    };
    let registry = base.library.registry();
    let function = |name| {
        registry
            .iter()
            .find(|(_, registered)| registered.name() == name)
            .unwrap()
    };
    for name in ["ts.number.format_value", "ts.to_boolean"] {
        let (_, registered) = function(name);
        assert!(
            registered
                .native()
                .and_then(|native| native.prepare())
                .is_some(),
            "{name} must provide a prepared conversion entry"
        );
    }
    // Where the generated scalar template answers, compare it directly.
    // Longer spellings and object kinds are compared through their bodies below.
    for (name, values) in [
        (
            "ts.number.format_value",
            vec![
                -0.0,
                0.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                1e21,
                1e-7,
                9007199254740991.0,
                9007199254740992.0,
                9007199254740994.0,
                f64::from_bits(1),
                f64::MIN_POSITIVE,
            ],
        ),
        (
            "ts.to_boolean",
            vec![-0.0, 0.0, f64::NAN, f64::INFINITY, -1.0, f64::from_bits(1)],
        ),
    ] {
        let (_, registered) = function(name);
        let prepared = registered.native().unwrap().prepare().unwrap();
        let template = registered.plan().unwrap().template.as_ref().unwrap();
        let natives: Vec<PreparedNative> = template
            .callees
            .iter()
            .map(|id| {
                registry
                    .get(id)
                    .unwrap()
                    .native()
                    .unwrap()
                    .prepare()
                    .unwrap()
            })
            .collect();
        for value in values {
            let args = [Value::Float(Float::new(value))];
            if let Some(expected) = template.entry.and_then(|entry| entry(&args, &natives)) {
                let actual =
                    (prepared.entry)(&args).expect("the template's inline answer is prepared");
                assert!(actual.same(&expected), "{name}({value})");
            }
        }
    }
    let (number, _) = function("ts.number.format_value");
    let (boolean, _) = function("ts.to_boolean");
    let named = |name| *function(name).0;
    let sources = [
        format!(
            r#"
main {{
  let values = [-0.0, 0.0, 1.0, -1.0, 12.0, -123.0, 99999999999999.0, -9999999999999.0, -99999999999999.0, nan, inf, -inf, 1e21, 1e20, 1e-7, 1e-6,
    9007199254740991.0, 9007199254740992.0, 9007199254740994.0,
    5e-324, -5e-324, 2.2250738585072014e-308, 1.7976931348623157e308,
    9007199254740993, -9223372036854775808, 18446744073709551616]
  for value in values {{ let converted = invoke @{number}(value) print converted }}
  return null
}}
"#
        ),
        format!(
            r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {{ return null }}"
main {{
  let spawned = spawn call child()
  let values = [null, absent, false, true, 0, 1, 18446744073709551616,
    -0.0, 0.0, nan, inf, -inf, 5e-324, "", "x", "a long text beyond inline storage",
    b"", instant, (), [], map{{}}, set{{}}, {{}}, fn() {{ return null }},
    @{error}("fixture", "", null), spawned, &child, handle, @{reference}(spawned), @{reference}([])]
  for value in values {{ let converted = invoke @{boolean}(value) print converted }}
  do join spawned
  return null
}}
"#,
            reference = named("ref"),
            error = named("error.new")
        ),
    ];
    for source in sources {
        let program = crate::machine::prepare_document(
            &(base.library.clone()),
            Arc::new(crate::document_fixture::authored(
                &format!("kernel 1\nnumbers by_spelling\n{source}"),
                registry,
            )),
            None,
        )
        .expect("prepare linked law document");
        let reference = Program {
            library: in_layout(&program.library, Layout(3)),
            ..program.clone()
        };
        let expected = observe::<3>(&reference, 50_000_000);
        assert!(
            matches!(expected.end, crate::ExpectedEnd::Finished(_)),
            "{expected:?}"
        );
        assert_eq!(observe::<0>(&program, 50_000_000), expected);
        let general = Program {
            library: in_layout(&program.library, Layout(1)),
            ..program.clone()
        };
        assert_eq!(observe::<1>(&general, 50_000_000), expected);
        // Every charge event boundary, plus its interior, retains the exact
        // prefix and refuses at the same helper in the body layout.
        let mut charged = 0;
        let mut bounds = std::collections::BTreeSet::from([0]);
        for event in &expected.tape {
            let units = match event {
                lash_kernel_exec::Weight::Units(units)
                | lash_kernel_exec::Weight::Atomic(units) => *units,
            };
            bounds.insert(charged);
            bounds.insert(charged + units / 2);
            charged += units;
            bounds.insert(charged);
        }
        for bound in bounds {
            assert_eq!(
                observe::<0>(&program, bound),
                observe::<3>(&reference, bound),
                "charge bound {bound}"
            );
        }
    }
}

/// PLAN-INDEP / FIG-7000: an Any-returning helper's text path may
/// compose the same prepared calls as a Text-returning helper. Its
/// completion formula still belongs to the outer helper.
#[test]
fn composed_text_addition_agrees_with_its_body_and_charge() {
    let runner = runner::Runner::new("typescript").unwrap();
    let case = runner::Case {
        id: "prefix-number".into(),
        dialect: "typescript".into(),
        source: "null;".into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
        panic!("fixture lowers")
    };
    let registry = base.library.registry();
    let (add, registered) = registry
        .iter()
        .find(|(_, registered)| registered.name() == "ts.add")
        .unwrap();
    let template = registered.plan().unwrap().template.as_ref().unwrap();
    let natives: Vec<_> = template
        .callees
        .iter()
        .map(|id| {
            registry
                .get(id)
                .unwrap()
                .native()
                .unwrap()
                .prepare()
                .unwrap()
        })
        .collect();
    let prefix = lash_kernel_doc::arena::ShortText::new("key_").unwrap();
    for number in [-0.0, 0.0, -17.0, 42.0] {
        let args = [Value::Text(prefix), Value::Float(Float::new(number))];
        let actual = template.entry.and_then(|entry| entry(&args, &natives));
        let expected = lash_kernel_doc::arena::ShortText::new(&format!("key_{}", number as i64))
            .map(Value::Text);
        assert!(
            actual
                .zip(expected)
                .is_some_and(|(actual, expected)| actual.same(&expected)),
            "composed path for {number}: {actual:?}"
        );
    }
    let source = format!(
        r#"kernel 1
numbers by_spelling
main {{
  for number in [-0.0, 0.0, -17.0, 42.0, 9007199254740991.0, 9999999999999.0, 1.5, nan, inf, -inf] {{
    let answer = invoke @{add}("key_", number)
    print answer
  }}
  return null
}}
"#
    );
    let program = crate::machine::prepare_document(
        &(base.library.clone()),
        Arc::new(crate::document_fixture::authored(&source, registry)),
        None,
    )
    .expect("prepare linked law document");
    let reference = Program {
        library: in_layout(&program.library, Layout(3)),
        ..program.clone()
    };
    let expected = observe::<3>(&reference, 50_000_000);
    assert!(
        matches!(expected.end, crate::ExpectedEnd::Finished(_)),
        "{expected:?}"
    );
    assert_eq!(observe::<0>(&program, 50_000_000), expected);
    let mut charged = 0;
    for event in &expected.tape {
        let units = match event {
            lash_kernel_exec::Weight::Units(units) | lash_kernel_exec::Weight::Atomic(units) => {
                *units
            }
        };
        for bound in [charged, charged + units / 2] {
            assert_eq!(
                observe::<0>(&program, bound),
                observe::<3>(&reference, bound),
                "bound {bound}"
            );
        }
        charged += units;
    }
    // Explicit conversions and interpolation keep two completion formulas
    // in their original order, including long answers and fractional inputs.
    for (dialect, source) in [
        (
            "typescript",
            r#"
const labels = [];
for (const number of [-0.0, 0.0, -17.0, 42.0, 9007199254740991.0, 9999999999999.0, 1.5]) {
  labels.push("é_" + String(number));
  labels.push(`key_${number}`);
}
const result = labels;
"#,
        ),
        (
            "python",
            r#"
labels = []
for number in [0, -17, 42, 9007199254740991, 9999999999999, 1.5]:
    labels.append("é_" + str(number))
    labels.append(f"key_{number}")
result = labels
"#,
        ),
    ] {
        let runner = runner::Runner::new(dialect).unwrap();
        let case = runner::Case {
            id: "prefix-number-forms".into(),
            dialect: dialect.into(),
            source: source.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("text construction lowers")
        };
        let reference = Program {
            library: in_layout(&program.library, Layout(3)),
            ..program.clone()
        };
        let expected = run::<3>(&case, &reference, &mut Projector::default());
        assert!(
            matches!(expected.end, crate::ExpectedEnd::Finished(_)),
            "{expected:?}"
        );
        assert_eq!(
            run::<0>(&case, &program, &mut Projector::default()),
            expected,
            "{dialect}"
        );
    }
}
