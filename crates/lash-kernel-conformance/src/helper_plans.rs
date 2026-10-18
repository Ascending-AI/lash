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
    let bounds = Bounds {
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: charge,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
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
                        charge: lash_kernel_vm::Charge::Exact,
                        charge_limit: 50_000_000,
                        memory: 64 << 20,
                        call_depth: 200,
                        live_tasks: 100,
                        requests_per_park: 100,
                        join_members: 100,
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
