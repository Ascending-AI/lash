//! Helper schedules obey PLAN-INDEP through the kernel-owned equality seam.
//! Layouts 0 and 1 answer closed helpers by their generated plans and the
//! rest by their natives; layout 3 runs every body (FIG-6678).
#![allow(
    clippy::unwrap_used,
    reason = "malformed fixtures and failed machine operations are law failures"
)]

use crate::projection::{Plan, Projector};
use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Timestamp};
use lash_kernel_vm::kernel_internals::{Layout, prepare_with_layout};
use lash_kernel_vm::{Bounds, CellStart, Host, Machine, Outcome, Program, Request, Step, Target};

#[allow(
    dead_code,
    reason = "the plan law shares lowering, not the differential's standalone runner"
)]
#[path = "../../lash-dialect-typescript/shared/helper_differential/runner.rs"]
pub(crate) mod runner;

pub(crate) struct World {
    pub(crate) bits: u64,
    pub(crate) stride: u64,
    pub(crate) nanoseconds: i64,
    pub(crate) prints: Vec<Datum>,
}
impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(self.nanoseconds),
        }
    }
    fn random(&mut self) -> u64 {
        let bits = self.bits;
        self.bits = bits.wrapping_add(self.stride);
        bits
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".into(),
            message: "no projection".into(),
            data: Datum::Null,
        })
    }
    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

#[derive(Debug, PartialEq)]
struct Observed {
    end: crate::ExpectedEnd,
    /// The function a bound the run ended at names (`K-LIB-006`).
    bound_in: Option<String>,
    prints: Vec<Datum>,
    requests: Vec<Request>,
    charged: u64,
    tape: Vec<lash_kernel_exec::Weight>,
}

fn run<const L: u64>(
    case: &runner::Case,
    program: &Program,
    projector: &mut Projector<'_>,
) -> Observed {
    run_bounded::<L>(case, program, projector, 50_000_000)
}

fn run_bounded<const L: u64>(
    case: &runner::Case,
    program: &Program,
    projector: &mut Projector<'_>,
    charge: u64,
) -> Observed {
    run_with_memory::<L>(
        case,
        program,
        projector,
        charge,
        lash_kernel_vm::MemoryLimit::Recount,
    )
}

fn run_at_depth<const L: u64>(
    case: &runner::Case,
    program: &Program,
    projector: &mut Projector<'_>,
    charge: u64,
    call_depth: u32,
) -> Observed {
    run_with_options::<L>(
        case,
        program,
        projector,
        charge,
        call_depth,
        lash_kernel_vm::MemoryLimit::Recount,
    )
}

fn run_with_memory<const L: u64>(
    case: &runner::Case,
    program: &Program,
    projector: &mut Projector<'_>,
    charge: u64,
    memory_limit: lash_kernel_vm::MemoryLimit,
) -> Observed {
    run_with_options::<L>(case, program, projector, charge, 200, memory_limit)
}

fn run_with_options<const L: u64>(
    case: &runner::Case,
    program: &Program,
    projector: &mut Projector<'_>,
    charge: u64,
    call_depth: u32,
    memory_limit: lash_kernel_vm::MemoryLimit,
) -> Observed {
    let bounds = Bounds {
        memory_limit,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: charge,
        memory: 64 << 20,
        call_depth,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
        durability: lash_kernel_vm::Durability::Durable,
    };
    let source = projector
        .imports
        .and_then(|images| images.first())
        .map(|image| image.source.clone())
        .unwrap_or_default();
    let mut machine = Plan::<L>::start(
        program.clone(),
        bounds,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: crate::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    )
    .unwrap();
    let tape = lash_kernel_vm::ChargeTape::new();
    machine.record_charges(tape.clone());
    let mut world = World {
        bits: case.bits,
        stride: case.stride,
        nanoseconds: case.nanoseconds,
        prints: Vec::new(),
    };
    let mut requests = Vec::new();
    let end = loop {
        match machine.run(&mut world, u64::MAX).unwrap() {
            Step::Slice => panic!("an unbounded helper run yielded"),
            Step::Parked(park) => {
                projector
                    .at(
                        &mut machine,
                        program,
                        bounds,
                        &source,
                        crate::projection::admissions(&park.requests),
                    )
                    .unwrap();
                machine.record_charges(tape.clone());
                for request in park.requests {
                    let (wait, outcome) = match &request {
                        Request::Effect(effect) => {
                            assert_eq!(effect.effect.as_str(), "observe");
                            (effect.wait, Outcome::Completed(Datum::Null))
                        }
                        Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
                    };
                    requests.push(request);
                    machine.deliver(wait, outcome).unwrap();
                }
            }
            Step::Ended(end) => {
                projector.end(&end, &source).unwrap();
                break end;
            }
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
        requests,
        charged: machine.meters().charged.expect("exact charge"),
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

/// Recursion through helpers (`%`, `-`, `+`, `==`) to depths around the
/// call-depth bound of [`run`]: the deepest are refused, at the same call
/// and naming the same function under every plan (`K-BND-002`).
fn depth_cases(dialect: &str) -> Vec<runner::Case> {
    (190..=200)
        .map(|depth| runner::Case {
            id: format!("{dialect}/depth-{depth}"),
            dialect: dialect.into(),
            source: match dialect {
                "python" => format!(
                    "def f(n):\n    if n == 0:\n        return 0\n    return f(n - 1) + n % 3\nawait observe(f({depth}))\n"
                ),
                _ => format!(
                    "function f(n) {{ if (n === 0) {{ return 0; }} return f(n - 1) + (n % 3); }}\nawait observe(f({depth}));\n"
                ),
            },
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        })
        .collect()
}

fn compare(dialect: &str, shard: Option<usize>) {
    let mut cases: Vec<runner::Case> = serde_json::from_str(include_str!(
        "../../lash-dialect-typescript/tests/helper_differential/cases.json"
    ))
    .unwrap();
    cases.extend(depth_cases(dialect));
    let runner = runner::Runner::new(dialect).unwrap();
    let mut body = None;
    let mut general = None;
    let mut compared = 0;
    for (index, case) in cases
        .iter()
        .filter(|case| case.dialect == dialect)
        .enumerate()
    {
        if shard.is_some_and(|shard| index % 16 != shard) {
            continue;
        }
        println!("{dialect}: checking {}", case.id);
        let runner::Lowered::Program(mut program) = runner.lower(case).unwrap() else {
            continue;
        };
        let mut reference = Projector::default();
        let expected = run::<0>(case, &program, &mut reference);
        reference.check_memory().unwrap();
        let prepared = general.get_or_insert_with(|| {
            prepare_with_layout(program.library.registry().clone(), Layout(1))
        });
        program.library = prepared.clone();
        let mut warm = Projector {
            layout: Layout(1),
            ..Projector::default()
        };
        assert_eq!(
            run::<1>(case, &program, &mut warm),
            expected,
            "{} general native",
            case.id
        );
        assert_eq!(
            warm.states, reference.states,
            "{} native checkpoints",
            case.id
        );
        assert_eq!(
            crate::projection::ids(&warm.states),
            crate::projection::ids(&reference.states),
            "{} native ids",
            case.id
        );
        assert_eq!(
            warm.terminal, reference.terminal,
            "{} native carry",
            case.id
        );
        let prepared = body.get_or_insert_with(|| {
            prepare_with_layout(program.library.registry().clone(), Layout(3))
        });
        program.library = prepared.clone();
        let mut cold = Projector {
            imports: Some(&reference.images),
            cold: true,
            layout: Layout(3),
            ..Projector::default()
        };
        assert_eq!(
            run::<3>(case, &program, &mut cold),
            expected,
            "{} cold fallback",
            case.id
        );
        cold.check_memory().unwrap();
        assert_eq!(
            cold.states, reference.states,
            "{} fallback checkpoints",
            case.id
        );
        assert_eq!(
            crate::projection::ids(&cold.states),
            crate::projection::ids(&reference.states),
            "{} fallback ids",
            case.id
        );
        assert_eq!(
            cold.terminal, reference.terminal,
            "{} fallback carry",
            case.id
        );
        compared += 1;
        if compared % 100 == 0 {
            println!("{dialect}: compared={compared}, last={}", case.id);
        }
    }
    assert!(compared > 0);
    println!("{dialect}: helper schedule plan comparisons={compared}");
}

/// K-CHG-003/007 and PLAN-INDEP: shard 0/16 of the unchanged TS corpus.
#[test]
fn typescript_00_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(0));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 1/16 of the unchanged TS corpus.
#[test]
fn typescript_01_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(1));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 2/16 of the unchanged TS corpus.
#[test]
fn typescript_02_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(2));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 3/16 of the unchanged TS corpus.
#[test]
fn typescript_03_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(3));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 4/16 of the unchanged TS corpus.
#[test]
fn typescript_04_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(4));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 5/16 of the unchanged TS corpus.
#[test]
fn typescript_05_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(5));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 6/16 of the unchanged TS corpus.
#[test]
fn typescript_06_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(6));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 7/16 of the unchanged TS corpus.
#[test]
fn typescript_07_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(7));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 8/16 of the unchanged TS corpus.
#[test]
fn typescript_08_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(8));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 9/16 of the unchanged TS corpus.
#[test]
fn typescript_09_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(9));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 10/16 of the unchanged TS corpus.
#[test]
fn typescript_10_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(10));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 11/16 of the unchanged TS corpus.
#[test]
fn typescript_11_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(11));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 12/16 of the unchanged TS corpus.
#[test]
fn typescript_12_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(12));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 13/16 of the unchanged TS corpus.
#[test]
fn typescript_13_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(13));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 14/16 of the unchanged TS corpus.
#[test]
fn typescript_14_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(14));
}

/// K-CHG-003/007 and PLAN-INDEP: shard 15/16 of the unchanged TS corpus.
#[test]
fn typescript_15_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(15));
}

/// K-CHG-003/007 and PLAN-INDEP: the same schedule contract for Python.
#[test]
fn python_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("python", None);
}

/// K-CHG-007: empty repetition still performs one declared test per round.
/// Its cost depends on the repetition count even though both lists are empty.
#[test]
fn empty_python_sequence_repetition_charges_its_declared_tests() {
    let runner = runner::Runner::new("python").unwrap();
    let charge = |count| {
        let case = runner::Case {
            id: "empty-repeat".into(),
            dialect: "python".into(),
            source: format!("await observe([] * {count})"),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        runner.run(&case).unwrap().charged.expect("exact charge")
    };
    assert_eq!(charge(9) - charge(5), 2 * (9 - 5));
}

#[rustfmt::skip]
#[path = "helper_plan_control_generated.rs"]
mod control;

fn control_program(source: &str) -> Program {
    use lash_kernel_dialect::{NamedLibrary, define_functions};
    use std::sync::Arc;
    let mut registry = crate::tests::corpus_registry();
    let mut names = NamedLibrary::from_registry(&registry).unwrap();
    for definition in define_functions(
        include_str!("../../lash-ext-helper-compiler/tests/control.kernel"),
        &mut names,
    )
    .unwrap()
    {
        let native = (definition.name.as_str() == "law.declining").then(|| {
            std::sync::Arc::new(Declining) as std::sync::Arc<dyn lash_kernel_doc::NativeFunction>
        });
        registry.register(definition, native).unwrap();
    }
    assert_eq!(
        registry.install_plans(control::PLANS).unwrap(),
        control::PLANS.len(),
        "the compiled control fixture matches its admitted bodies"
    );
    let mut source = format!("kernel 1\nnumbers by_spelling\n{source}\n");
    for (function, registered) in registry.iter() {
        source.push_str(&format!(
            "use {} = @{function}\n",
            registered.definition().name
        ));
    }
    Program {
        release: crate::pages::RELEASE,
        document: Arc::new(lash_kernel_doc::parse_document(&source).unwrap()),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::new(registry)),
    }
}

fn compare_control(source: &str, charge: u64) -> Observed {
    let program = control_program(source);
    let case = runner::Case {
        id: "control".into(),
        dialect: "kernel".into(),
        source: source.into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let expected = run_bounded::<3>(&case, &program, &mut Projector::default(), charge);
    assert_eq!(
        run_bounded::<0>(&case, &program, &mut Projector::default(), charge),
        expected
    );
    assert_eq!(
        run_bounded::<1>(&case, &program, &mut Projector::default(), charge),
        expected
    );
    expected
}

/// K-FORM-014..017: catch sees only guest raises, cleanup runs in order,
/// and its departure replaces the return or raise it interrupted.
#[test]
fn generated_handlers_preserve_typed_raises_departures_and_cleanup_order() {
    for (mode, answer, raised) in [
        (0, 20, Some("guest")),
        (1, 10, None),
        (2, 20, Some("division_by_zero")),
        (3, 40, None),
    ] {
        let observed = compare_control(
            &format!(
                "main {{ let log = [] let answer = invoke law.handlers({mode}, log) return (answer, log) }}"
            ),
            1_000_000,
        );
        let mut log = vec![Datum::Int(1.into()), Datum::Int(2.into())];
        if let Some(kind) = raised {
            log.push(Datum::Text(kind.into()));
        }
        log.extend([Datum::Int(99.into()), Datum::Int(100.into())]);
        assert_eq!(
            observed.end,
            crate::ExpectedEnd::Finished(Datum::Tuple(vec![
                Datum::Int(answer.into()),
                Datum::List(log)
            ]))
        );
    }
    let halted = compare_control("main { let value = invoke law.halt() return value }", 31);
    assert!(matches!(halted.end, crate::ExpectedEnd::Bound { .. }));
}

/// K-FORM-004/K-FN-005/K-CHG-007: closures share the original cells and
/// execute later as charged ordinary code, with their logical depth unit.
#[test]
fn generated_closures_share_cells_and_keep_ordinary_charge_and_depth() {
    let source = "main { let pair = invoke law.factory(5) let increment = pair.increment let peek = pair.peek let first = apply increment(2) let second = apply increment(3) let state = apply peek() return (first, second, state) }";
    let observed = compare_control(source, 1_000_000);
    assert_eq!(
        observed.end,
        crate::ExpectedEnd::Finished(Datum::Tuple(vec![
            Datum::Int(7.into()),
            Datum::Int(10.into()),
            Datum::Tuple(vec![Datum::Int(6.into()), Datum::Int(10.into())])
        ]))
    );
    // Each ordinary closure's charged trace and each refusal point survive
    // the plan/body choice, including the formula completion boundary.
    for charge in 0..=observed.charged {
        compare_control(source, charge);
    }
    for depth in 190..=200 {
        compare_control(
            &format!(
                "fn descend(n, f) {{ if eq(n, 0) {{ let value = apply f() return value }} let next = num.sub(n, 1) let value = call descend(next, f) return value }} main {{ let pair = invoke law.factory(5) let peek = pair.peek let value = call descend({depth}, peek) return value }}"
            ),
            1_000_000,
        );
    }
}

struct Declining;
impl lash_kernel_doc::NativeFunction for Declining {
    fn call(
        &self,
        _: lash_kernel_doc::NativeCall<'_, '_>,
    ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
        Err(lash_kernel_doc::NativeError::Decline)
    }
}

/// K-LIB-006: the native is offered once, then its body's mutations are
/// performed once by the generated callee plan under the same admission.
#[test]
fn generated_callees_run_a_declined_body_once() {
    let observed = compare_control(
        "main { let log = [] let answer = invoke law.decline_caller(log) return answer }",
        1_000_000,
    );
    let log = Datum::List(vec![Datum::Int(1.into())]);
    assert_eq!(
        observed.end,
        crate::ExpectedEnd::Finished(Datum::Tuple(vec![log.clone(), log]))
    );
}

/// K-URI-WORK (FIG-6720): one completion price bounds the synchronous
/// delegated calls and heap bookkeeping, including work before a URI raises.
/// The decoder traverses percent-delimited chunks: its 64 units per input
/// unit cover calls, holding changes and reservations; 32 cover fixed work.
#[test]
fn uri_decode_completion_price_bounds_delegated_work_on_return_and_raise() {
    use lash_kernel_vm::perf_witness::{self, Profiling as _};
    use std::sync::Arc;

    let runner = runner::Runner::new("typescript").unwrap();
    let case = runner::Case {
        id: "uri-work".into(),
        dialect: "typescript".into(),
        source: "decodeURI('');".into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
        panic!("the URI operation lowers");
    };
    for name in ["ts.uri.decodeURI", "ts.uri.decodeURIComponent"] {
        let function = base
            .library
            .registry()
            .iter()
            .find_map(|(id, registered)| {
                (registered.definition().name.as_str() == name).then_some(*id)
            })
            .unwrap();
        for repeats in [0, 1, 16, 256] {
            let prefix = "%E0%A0%80".repeat(repeats);
            for (suffix, raised) in [
                ("", false),
                ("literal😀%2f", false),
                ("%", true),
                ("%+1", true),
                ("%ED%A0%80", true),
            ] {
                let input = format!("{prefix}{suffix}");
                let source = format!(
                    "kernel 1\nnumbers by_spelling\nuse decode = @{function}\nmain {{ let answer = invoke decode(null, [{input:?}]) return answer }}"
                );
                let program = Program {
                    document: Arc::new(lash_kernel_doc::parse_document(&source).unwrap()),
                    ..base.clone()
                };
                let mut machine = crate::pages::start(
                    program,
                    Bounds {
                        memory_limit: lash_kernel_vm::MemoryLimit::Recount,
                        charge: lash_kernel_vm::Charge::Exact,
                        charge_limit: 50_000_000,
                        memory: 64 << 20,
                        call_depth: 200,
                        live_tasks: 100,
                        requests_per_park: 100,
                        join_members: 100,
                        durability: lash_kernel_vm::Durability::Durable,
                    },
                    CellStart {
                        target: Target::Main,
                        args: Vec::new(),
                        session: None,
                        owner: crate::pages::OWNER,
                        modes: Default::default(),
                        host: Vec::new(),
                    },
                )
                .unwrap();
                let mut world = World {
                    bits: 0,
                    stride: 0,
                    nanoseconds: 0,
                    prints: Vec::new(),
                };
                machine.count_work();
                perf_witness::enable_work();
                let step = machine.run(&mut world, u64::MAX).unwrap();
                let fixed = perf_witness::take_work().unwrap();
                perf_witness::disable_work();
                let Step::Ended(end) = step else {
                    panic!("a synchronous decoder ends");
                };
                if raised {
                    assert!(
                        matches!(
                            end,
                            lash_kernel_vm::End::Error(lash_kernel_vm::RunError::Uncaught(_))
                        ),
                        "{end:?}"
                    );
                } else {
                    assert!(matches!(end, lash_kernel_vm::End::Finished(_)), "{end:?}");
                }
                let work = machine.work().unwrap();
                // A body owns its completion charge in the code row; a
                // native or generated plan owns it in the native row. Match
                // the exact helper, excluding any closures inside its body.
                let price: u64 = work
                    .natives
                    .iter()
                    .filter(|native| native.symbol.function == function)
                    .map(|native| native.counts.self_charge.expect("exact charge"))
                    .chain(
                        work.codes
                            .iter()
                            .filter(|code| {
                                code.symbol.function == Some(function)
                                    && code.symbol.site.path.is_empty()
                            })
                            .map(|code| code.counts.self_charge.expect("exact charge")),
                    )
                    .sum();
                let calls: u64 = work.natives.iter().map(|native| native.counts.calls).sum();
                let admitted_work =
                    calls + fixed.heap.accounting_changes + fixed.heap.accounting_reservations;
                assert!(
                    admitted_work <= price,
                    "{name}, repeats={repeats}, suffix={suffix:?}: {admitted_work} calls, holding changes and reservations exceed completion price {price}"
                );
            }
        }
    }
}

/// FIG-6732: compiled helper plans retain results and errors under Off, in
/// both dialects, using the differential corpus rather than a second set of cases.
#[test]
fn charge_off_preserves_the_helper_corpus() {
    let cases: Vec<runner::Case> = serde_json::from_str(include_str!(
        "../../lash-dialect-typescript/tests/helper_differential/cases.json"
    ))
    .unwrap();
    let mut executed = 0;
    for dialect in ["typescript", "python"] {
        let exact = runner::Runner::new(dialect).unwrap();
        let off = runner::Runner::new_with_charge(dialect, lash_kernel_vm::Charge::Off).unwrap();
        for case in cases.iter().filter(|case| case.dialect == dialect) {
            let expected = exact.run(case).unwrap();
            let actual = off.run(case).unwrap();
            assert_eq!(actual.result, expected.result, "{}", case.id);
            assert_eq!(actual.helpers, expected.helpers, "{}", case.id);
            assert_eq!(actual.charged, None, "{}", case.id);
            executed += 1;
        }
    }
    println!("charge-off helper corpus: executed={executed}");
    assert!(executed > 0);
}

/// PLAN-INDEP / K-CHG-003: generated typed helper paths agree with their
/// bodies, including the completion tape, raises and depth refusals.
#[test]
fn generated_scalar_templates_agree_with_their_bodies() {
    use lash_kernel_doc::{Float, PreparedNative, Value, parse_document};
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
        let general = prepare_with_layout(Arc::clone(registry), Layout(1));
        let bodies = prepare_with_layout(Arc::clone(registry), Layout(3));
        for name in names {
            let (function, registered) = registry
                .iter()
                .find(|(_, registered)| registered.definition().name.as_str() == *name)
                .unwrap();
            let template = registered
                .plan()
                .unwrap()
                .template
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no generated scalar path"));
            let natives: Vec<PreparedNative> = template
                .callees
                .iter()
                .map(|function| {
                    registry
                        .get(function)
                        .unwrap()
                        .native()
                        .unwrap()
                        .prepare()
                        .unwrap()
                })
                .collect();
            let arity = registered.definition().signature.params.len();
            let args = if dialect == "python" {
                vec![Value::Int(1); arity]
            } else {
                vec![Value::Float(Float::new(1.0)); arity]
            };
            assert!(
                (template.entry)(&args, &natives).is_some(),
                "{name}'s numeric path must be exercised"
            );
            // Every depth near the elided helper frames uses the same
            // refusal point, charge prefix and function name as the body.
            for call_depth in 0..=template.depth + 2 {
                let args = vec![if dialect == "python" { "1" } else { "1.0" }; arity];
                let source = format!(
                    "kernel 1\nnumbers by_spelling\nuse op = @{function}\nmain {{ let value = invoke op({}) return value }}",
                    args.join(", ")
                );
                let mut program = Program {
                    release: base.release,
                    document: Arc::new(parse_document(&source).unwrap()),
                    library: base.library.clone(),
                };
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
                    "kernel 1\nnumbers by_spelling\nuse op = @{function}\nmain {{ let value = invoke op({}) return value }}",
                    args.join(", ")
                );
                let mut program = Program {
                    release: base.release,
                    document: Arc::new(parse_document(&source).unwrap()),
                    library: base.library.clone(),
                };
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
            println!("{name}: {answered} template/body cases");
        }
    }
}

/// FIG-6731: generated helpers retain results, charge, effects and parks
/// with memory Off, and never create a reservation scope.
fn memory_off_helper_plans_keep_the_corpus_without_memory_scopes(shard: usize) {
    let cases: Vec<runner::Case> = serde_json::from_str(include_str!(
        "../../lash-dialect-typescript/tests/helper_differential/cases.json"
    ))
    .unwrap();
    let mut compared = 0;
    for dialect in ["typescript", "python"] {
        let runner = runner::Runner::new(dialect).unwrap();
        for (index, case) in cases
            .iter()
            .filter(|case| case.dialect == dialect)
            .enumerate()
        {
            if index % 16 != shard {
                continue;
            }
            println!("memory-off helper shard={shard}: {}", case.id);
            let runner::Lowered::Program(program) = runner.lower(case).unwrap() else {
                continue;
            };
            let expected = run::<0>(case, &program, &mut Projector::default());
            if matches!(&expected.end, crate::ExpectedEnd::Bound { name, .. } if name == "memory") {
                continue;
            }
            lash_kernel_vm::perf_witness::enable_work();
            let observed = run_with_memory::<0>(
                case,
                &program,
                &mut Projector::default(),
                50_000_000,
                lash_kernel_vm::MemoryLimit::Off,
            );
            let work = lash_kernel_vm::perf_witness::take_work().unwrap();
            lash_kernel_vm::perf_witness::disable_work();
            assert_eq!(observed, expected, "{}", case.id);
            assert_eq!(
                (
                    work.memory_scopes,
                    work.heap.arena_memory_prices,
                    work.heap.accounting_reservations,
                    work.heap.accounting_changes,
                    work.heap.accounting_additions,
                    work.heap.refinement_attempts
                ),
                (0, 0, 0, 0, 0, 0),
                "{}",
                case.id
            );
            compared += 1;
        }
    }
    assert!(compared > 0);
    println!("memory-off helpers: shard={shard} compared={compared}");
}

/// FIG-6731: shard 0/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_00_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(0);
}

/// FIG-6731: shard 1/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_01_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(1);
}

/// FIG-6731: shard 2/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_02_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(2);
}

/// FIG-6731: shard 3/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_03_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(3);
}

/// FIG-6731: shard 4/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_04_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(4);
}

/// FIG-6731: shard 5/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_05_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(5);
}

/// FIG-6731: shard 6/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_06_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(6);
}

/// FIG-6731: shard 7/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_07_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(7);
}

/// FIG-6731: shard 8/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_08_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(8);
}

/// FIG-6731: shard 9/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_09_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(9);
}

/// FIG-6731: shard 10/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_10_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(10);
}

/// FIG-6731: shard 11/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_11_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(11);
}

/// FIG-6731: shard 12/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_12_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(12);
}

/// FIG-6731: shard 13/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_13_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(13);
}

/// FIG-6731: shard 14/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_14_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(14);
}

/// FIG-6731: shard 15/16 of the unchanged helper corpus with memory Off.
#[test]
fn memory_off_15_helper_plans_keep_the_corpus_without_memory_scopes() {
    memory_off_helper_plans_keep_the_corpus_without_memory_scopes(15);
}

/// FIG-6754's actual dialect path, including nested and statement-root invokes.
pub(crate) fn python_region_programs() -> Vec<(runner::Case, Program)> {
    let runner = runner::Runner::new("python").unwrap();
    let sources = [
        ("branch", "def branch():\n    result = 0\n    i = 0\n    while i < 14:\n        if i % 2 == 0:\n            result = result + i\n        else:\n            result = result - i\n        i = i + 1\n    return result\nresult = branch()\n".to_owned()),
        ("modulo signs and zero", "def modulo(left, right):\n    i = 0\n    while i < 4:\n        left = left % right\n        right = right + 1\n        i = i + 1\n    return left\nresult = modulo(-7, -2)\n".to_owned()),
        ("float modulo", "def modulo(left, right):\n    i = 0\n    while i < 4:\n        left = left % right\n        i = i + 1\n    return left\nresult = modulo(7.5, -2.0)\n".to_owned()),
        ("kind decline", "def modulo(left, right):\n    i = 0\n    while i < 4:\n        left = left % right\n        i = i + 1\n    return left\nresult = modulo(7, \"x\")\n".to_owned()),
    ];
    sources
        .into_iter()
        .map(|(name, source)| {
            let case = runner::Case {
                id: format!("Python invoke region: {name}"),
                dialect: "python".into(),
                source,
                bits: 0,
                stride: 0,
                nanoseconds: 0,
            };
            let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
                panic!("{} lowers", case.id)
            };
            (case, program)
        })
        .collect()
}

/// FIG-6755: the cold `sum1m` and `branch1m` shapes of
/// `scripts/perf/kernel/embedded`, shortened, at a cell's top level in
/// both dialects: their variables are session bindings, which regions
/// hold as slots between boundaries.
pub(crate) fn top_level_region_programs() -> Vec<(runner::Case, Program)> {
    let sum_ts =
        "let result = 0;\nlet i = 0;\nwhile (i < 12) { result = result + i; i = i + 1; }\n";
    let branch_ts = "let result = 0;\nlet i = 0;\nwhile (i < 12) {\n  if (i % 2 === 0) { result = result + i; } else { result = result - i; }\n  i = i + 1;\n}\n";
    let sum_py = "result = 0\ni = 0\nwhile i < 12:\n    result = result + i\n    i = i + 1\n";
    let branch_py = "result = 0\ni = 0\nwhile i < 12:\n    if i % 2 == 0:\n        result = result + i\n    else:\n        result = result - i\n    i = i + 1\n";
    [
        ("typescript", "sum", sum_ts),
        ("typescript", "branch", branch_ts),
        ("python", "sum", sum_py),
        ("python", "branch", branch_py),
    ]
    .into_iter()
    .map(|(dialect, name, source)| {
        let runner = runner::Runner::new(dialect).unwrap();
        let case = runner::Case {
            id: format!("top-level {dialect} region: {name}"),
            dialect: dialect.into(),
            source: source.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("{} lowers", case.id)
        };
        (case, program)
    })
    .collect()
}

/// FIG-6754: lowered Python branch and modulo loops reach the generated
/// templates and form regions over unshared frame slots.
#[test]
fn lowered_python_modulo_branch_loops_form_regions() {
    use lash_kernel_vm::kernel_internals::loop_regions;
    for (case, program) in python_region_programs() {
        assert!(loop_regions(&program).unwrap() > 0, "{}", case.source);
        let expected = run::<0>(&case, &program, &mut Projector::default());
        let mut reference = program.clone();
        reference.library = prepare_with_layout(
            program.library.registry().clone(),
            Layout(0).without_regions(),
        );
        assert_eq!(
            run::<0>(&case, &reference, &mut Projector::default()),
            expected,
            "{}",
            case.id
        );
    }
}
