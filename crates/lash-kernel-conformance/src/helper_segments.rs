//! Helpers that call back into guest code run as generated segments that
//! yield at each callback site with their body frame's coordinate
//! (FIG-6709, kernel end state §J). A run cut at any statement under plans
//! reopens under bodies, and the reverse, to the same checkpoint and the
//! same rest of the run.
#![allow(
    clippy::unwrap_used,
    reason = "malformed fixtures and failed machine operations are law failures"
)]

use crate::helper_plans::{World, runner};
use crate::projection::{Image, Plan, Projector};
use lash_kernel_doc::Datum;
use lash_kernel_vm::kernel_internals::conformance::checkpoint;
use lash_kernel_vm::kernel_internals::{Layout, in_layout};
use lash_kernel_vm::{
    Bounds, CellStart, Machine, Outcome, PreparedLibrary, Program, Request, Step, Target,
};

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 50_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

/// Guest callbacks of several statements through segment helpers: each
/// callback logs what it sees, so a callback run again shows in the result.
const TYPESCRIPT: &[(&str, &str)] = &[
    (
        "iterate",
        r#"const xs = [3, 1, 2];
const log = [];
const kept = xs.filter((x) => { log.push(x); return x > 1; });
const doubled = xs.map((x) => { const y = x * 2; log.push(y); return y; });
xs.forEach((x, i) => { const y = x * i; log.push(y); });
const sum = xs.reduce((acc, x) => { log.push(acc); return acc + x; }, 10);
await host.observe([kept, doubled, sum, log]);"#,
    ),
    (
        "search",
        r#"const xs = [2, 5, 1];
const log = [];
const at = xs.findIndex((x) => { log.push(x); return x === 5; });
const every = xs.every((x) => { log.push(-x); return x > 0; });
const some = xs.some((x) => { log.push(x * 10); return x > 4; });
const flat = xs.flatMap((x) => { log.push(x + 100); return [x, -x]; });
await host.observe([at, every, some, flat, log]);"#,
    ),
    (
        "compare",
        r#"const xs = [4, 1, 3, 2];
const log = [];
const sorted = xs.toSorted((a, b) => { log.push(a * 10 + b); return a - b; });
xs.sort((a, b) => { log.push(-(a * 10 + b)); return b - a; });
await host.observe([sorted, xs, log]);"#,
    ),
    (
        "nested",
        r#"const xs = [1, 2];
const log = [];
xs.forEach((x) => { const ys = [x, x + 1].filter((y) => { log.push(x * 10 + y); return y > 1; }); log.push(ys.length); });
await host.observe(log);"#,
    ),
    (
        "coerce",
        r#"const log = [];
const o = { valueOf() { log.push("valueOf"); return 5; } };
const scaled = o * 2;
const less = o < 7;
await host.observe([scaled, less, log]);"#,
    ),
    (
        "raise",
        r#"const xs = [1, 2, 3];
const log = [];
let caught = "none";
try {
  xs.forEach((x) => { log.push(x); if (x === 2) { throw new Error("two"); } });
} catch (e) {
  caught = e.message;
}
await host.observe([caught, log]);"#,
    ),
];

/// Key functions and conversions through Python segment helpers, some
/// standing in a `try` of their own.
const PYTHON: &[(&str, &str)] = &[
    (
        "sort",
        "def key(x):\n    print(\"key\", x)\n    return -x\nxs = [3, 1, 2]\nxs.sort(key=key)\nprint(xs)\nys = sorted(xs, key=lambda x: x % 3)\nawait host.observe([xs, ys])\n",
    ),
    (
        "raise",
        "def bad(x):\n    print(\"bad\", x)\n    if x == 1:\n        raise ValueError(\"one\")\n    return x\nxs = [3, 1, 2]\ntry:\n    xs.sort(key=bad)\nexcept ValueError as e:\n    print(\"caught\", str(e))\nawait host.observe(xs)\n",
    ),
    (
        "convert",
        "xs = [1, \"a\", [2]]\nprint(str(xs))\ntry:\n    xs.index(9)\nexcept ValueError as e:\n    print(str(e))\nn = int(\"42\") + int(\"-0x1f\", 0)\nf = float(\"2.5\")\ntry:\n    int(\"nope\")\nexcept ValueError as e:\n    print(\"bad\", str(e))\nawait host.observe([str(xs), n, f])\n",
    ),
];

/// What a run does from a cut on.
#[derive(Debug, PartialEq)]
struct Rest {
    end: crate::ExpectedEnd,
    prints: Vec<Datum>,
    requests: Vec<Request>,
    charged: u64,
}

/// A run cut after every statement: its images, how much it had printed
/// and requested at each and what it owed there, and what it did in all.
struct Cut {
    images: Vec<Image>,
    marks: Vec<(usize, usize, Vec<Request>)>,
    whole: Rest,
}

fn world() -> World {
    World::default()
}

/// Runs `machine` to its end statement by statement, answering each park
/// at once; `cut` sees each statement's end and each park with what it
/// owes.
fn finish<M: Machine>(
    machine: &mut M,
    world: &mut World,
    requests: &mut Vec<Request>,
    mut cut: impl FnMut(&mut M, &World, &[Request], &[Request]),
) -> crate::ExpectedEnd {
    loop {
        match machine.run(world, 0).unwrap() {
            Step::Slice => cut(machine, world, requests, &[]),
            Step::Parked(park) => {
                cut(machine, world, requests, &park.requests);
                for request in park.requests {
                    let (wait, outcome) = match &request {
                        Request::Effect(effect) => {
                            assert_eq!(
                                crate::linked::operation(&effect.use_).as_str(),
                                "host.observe"
                            );
                            (effect.wait, Outcome::Completed(Datum::Null))
                        }
                    };
                    requests.push(request);
                    machine.deliver(wait, outcome).unwrap();
                }
            }
            Step::Ended(end) => return crate::ExpectedEnd::from_end(end),
        }
    }
}

fn cut_all<const L: u64>(program: &Program) -> Cut {
    let source = crate::pages::Source::default();
    let mut machine = Plan::<L>::start(
        program.clone(),
        BOUNDS,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: crate::pages::OWNER,
            modes: runner::modes(program),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    )
    .unwrap();
    let mut projector = Projector {
        layout: Layout(L),
        library: Some(program.library.clone()),
        ..Projector::default()
    };
    let mut marks = Vec::new();
    let mut world = world();
    let mut requests = Vec::new();
    let end = finish(
        &mut machine,
        &mut world,
        &mut requests,
        |machine, world, requests, owed| {
            projector.at(machine, program, BOUNDS, &source).unwrap();
            marks.push((world.prints.len(), requests.len(), owed.to_vec()));
        },
    );
    projector.check_memory().unwrap();
    Cut {
        images: projector.images,
        marks,
        whole: Rest {
            end,
            prints: world.prints,
            requests,
            charged: machine.meters().charged.expect("exact charge"),
        },
    }
}

/// Opens every image of `cut` under layout `L` with `library`: each
/// reseals to its own checkpoint and runs on as the cut run did.
fn reopen_all<const L: u64>(name: &str, program: &Program, library: &PreparedLibrary, cut: &Cut) {
    let mut program = program.clone();
    program.library = library.clone();
    for (index, (image, (printed, requested, owed))) in
        cut.images.iter().zip(&cut.marks).enumerate()
    {
        let mut machine =
            crate::pages::open::<Plan<L>>(program.clone(), BOUNDS, &image.root, &image.source)
                .unwrap();
        let resealed = machine.seal(crate::pages::BUDGET).unwrap().unwrap();
        assert!(
            checkpoint(
                lash_kernel_vm::kernel_internals::sealed_root(&resealed),
                Box::new(image.source.clone())
            )
            .unwrap()
                == checkpoint(&image.root, Box::new(image.source.clone())).unwrap(),
            "{name}: cut {index} reopened under layout {L} to another checkpoint"
        );
        // A cut at a park still owes its delivery: the rest begins there.
        let mut world = world();
        let mut requests = Vec::new();
        for request in owed {
            let (wait, outcome) = match request {
                Request::Effect(effect) => (effect.wait, Outcome::Completed(Datum::Null)),
            };
            requests.push(request.clone());
            machine.deliver(wait, outcome).unwrap();
        }
        let end = finish(&mut machine, &mut world, &mut requests, |_, _, _, _| {});
        let rest = Rest {
            end,
            prints: world.prints,
            requests,
            charged: machine.meters().charged.expect("exact charge"),
        };
        let expected = Rest {
            end: cut.whole.end.clone(),
            prints: cut.whole.prints[*printed..].to_vec(),
            requests: cut.whole.requests[*requested..].to_vec(),
            charged: cut.whole.charged,
        };
        assert_eq!(rest, expected, "{name}: cut {index} under layout {L}");
    }
}

fn cross(dialect: &str, programs: &[(&str, &str)]) -> Vec<Rest> {
    let started = std::time::Instant::now();
    let runner = runner::Runner::new(dialect).unwrap();
    let mut libraries = None;
    let mut cuts = 0;
    let mut observed = Vec::new();
    for (name, source) in programs {
        let case = runner::Case {
            id: format!("{dialect}/{name}"),
            dialect: dialect.into(),
            source: (*source).into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("{} is refused", case.id);
        };
        let (plans, bodies) = libraries.get_or_insert_with(|| {
            (
                in_layout(&program.library, Layout(0)),
                in_layout(&program.library, Layout(3)),
            )
        });
        let mut planned = program.clone();
        planned.library = plans.clone();
        let mut bodied = program.clone();
        bodied.library = bodies.clone();
        let by_plan = cut_all::<0>(&planned);
        let by_body = cut_all::<3>(&bodied);
        assert_eq!(
            by_plan.whole, by_body.whole,
            "{}: plans and bodies",
            case.id
        );
        // Segments run natively: the helpers' own statements are no
        // statements of the run.
        assert!(
            by_plan.images.len() < by_body.images.len(),
            "{}: {} cuts under plans, {} under bodies",
            case.id,
            by_plan.images.len(),
            by_body.images.len()
        );
        reopen_all::<3>(&case.id, &program, bodies, &by_plan);
        reopen_all::<0>(&case.id, &program, plans, &by_body);
        println!(
            "{}: cuts plan={} body={} after {:?}",
            case.id,
            by_plan.images.len(),
            by_body.images.len(),
            started.elapsed()
        );
        cuts += by_plan.images.len() + by_body.images.len();
        observed.push(by_plan.whole);
    }
    assert!(cuts > 0);
    println!("{dialect}: segment cross-resumes={cuts}");
    observed
}

/// §J invariant 2: a run cut after every statement, inside callbacks too,
/// reopens under bodies when it ran by plans and under plans when it ran
/// by bodies, to the same checkpoint, then prints, requests, charges and
/// ends as the cut run did: no observed callback runs again.
#[test]
fn typescript_segments_cross_resume_after_every_statement() {
    cross("typescript", &TYPESCRIPT[..3]);
}

/// §J invariant 2 for nested callbacks, coercion and a raise out of a
/// callback.
#[test]
fn typescript_nested_and_raising_segments_cross_resume_after_every_statement() {
    cross("typescript", &TYPESCRIPT[3..]);
}

/// A nested callback keeps a session read when its lifted body also has a
/// standalone compilation with a lexical free slot (FIG-7109).
#[test]
fn nested_typescript_callback_keeps_its_lexical_capture_shape() {
    let observed = cross(
        "typescript",
        &[(
            "nested-capture",
            "const log = []; [1].forEach((x) => { [2].forEach((y) => { log.push(x + y); }); }); await host.observe(log);",
        )],
    );
    assert_eq!(observed[0].end, crate::ExpectedEnd::Finished(Datum::Null));
    assert_eq!(observed[0].requests.len(), 1);
    let Request::Effect(effect) = &observed[0].requests[0];
    assert_eq!(
        effect.args,
        vec![Datum::List(vec![Datum::Float(3.0.into())])]
    );
}

/// §J invariant 2 for Python's segment helpers.
#[test]
fn python_segments_cross_resume_after_every_statement() {
    cross("python", PYTHON);
}
