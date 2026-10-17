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
mod runner;

struct World {
    bits: u64,
    stride: u64,
    nanoseconds: i64,
    prints: Vec<Datum>,
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
    let bounds = Bounds {
        charge: 50_000_000,
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
        charged: machine.meters().charged,
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

fn compare(dialect: &str, parity: Option<usize>) {
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
        if parity.is_some_and(|parity| index % 2 != parity) {
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

/// K-CHG-003/007 and PLAN-INDEP: the TypeScript helper corpus preserves
/// values, errors, effects, charge and canonical checkpoints across plans.
#[test]
fn typescript_even_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(0));
}

/// K-CHG-003/007 and PLAN-INDEP: the other half of the same corpus.
#[test]
fn typescript_odd_helper_schedules_survive_native_fallback_and_cold_resume() {
    compare("typescript", Some(1));
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
        runner.run(&case).unwrap().charged
    };
    assert_eq!(charge(9) - charge(5), 2 * (9 - 5));
}
