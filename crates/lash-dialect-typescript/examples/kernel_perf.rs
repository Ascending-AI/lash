//! The kernel's speed gate (kernel workflow spec §11, gate 6).
//!
//! The programs of the retired VM's benchmark (`crates/lash-vm/benches/
//! benchmark.rs` at `515a596747`, the last main commit before the cutover),
//! written as the TypeScript a cell author would write and lowered through
//! this dialect. Each is timed on the kernel machine alone: lowering,
//! registry assembly and the library's preparation happen once, outside the
//! timed loop, as the old `compiled_execute` mode compiled once. A scripted host answers every
//! tool call at once.
//!
//! `kernel_perf <most iterations> [program or file.ts...]` runs each program
//! for about two seconds and prints one tab-separated row: the manifest's
//! function count, the time per run with and without `start`, the run's
//! charge, and where it went. The split comes from a second run that stops
//! after every unit of charge and reads the pending statement's site from the
//! exported state (exporting changes no charge, `K-MACH-007`): the charge and
//! the time of each step go to TypeScript helper bodies, kernel library bodies
//! or the document's own code. A helper's body is charged as it runs and
//! its formula is not (`K-CHG-007`), so a step is a statement of the body
//! that does the work;
//! the body of a library function with a native implementation is covered
//! by its formula and runs within one step. `KERNEL_PERF_SPLIT=0` skips the
//! second run, for a profiler.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Timestamp, Unit,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Start, Step, Target,
};

#[path = "../shared/gate_six.rs"]
mod gate_six;

use gate_six::{answer, effects, programs};

const BOUNDS: Bounds = Bounds {
    charge: 1_000_000_000,
    memory: 1 << 30,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console {
    printed: usize,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "the benchmark host has no projection".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, _value: &Datum) {
        self.printed += 1;
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Answers every request of a park, in the order asked.
fn deliver_all(machine: &mut KernelMachine, requests: Vec<Request>) -> Option<Datum> {
    let mut finished = None;
    for request in requests {
        let (wait, outcome) = match request {
            Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
            Request::Effect(effect) => {
                let argument = effect.args.into_iter().next().unwrap_or(Datum::Null);
                if effect.effect.as_str() == "finish" {
                    finished = Some(argument.clone());
                }
                (effect.wait, answer(effect.effect.as_str(), argument))
            }
        };
        machine
            .deliver(wait, outcome)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    finished
}

fn start(program: &Program) -> KernelMachine {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    KernelMachine::start(program.clone(), BOUNDS, start).unwrap_or_else(|error| panic!("{error}"))
}

/// Runs a started machine to its end. Gives the charge and the value the
/// program finished with.
fn to_end(machine: &mut KernelMachine, name: &str) -> (u64, Datum) {
    let mut console = Console::default();
    let mut finished = Datum::Null;
    loop {
        match machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            Step::Parked(park) => {
                if let Some(value) = deliver_all(machine, park.requests) {
                    finished = value;
                }
            }
            Step::Ended(End::Finished(_)) => return (machine.meters().charged, finished),
            Step::Ended(other) => panic!("{name} ended {other:?}"),
            Step::Slice => unreachable!("the slice is unbounded"),
        }
    }
}

/// Where the charge of one run went.
#[derive(Default)]
struct Split {
    /// Steps of one unit of charge.
    steps: u64,
    helpers: u64,
    library: u64,
    program: u64,
    by_helper: BTreeMap<String, u64>,
    /// Time inside `run`, by where the step ran: helper bodies, library
    /// bodies and document code. Stepping and exporting slow a run down, so only the shares
    /// mean anything.
    helper_time: std::time::Duration,
    library_time: std::time::Duration,
    program_time: std::time::Duration,
    helper_time_by: BTreeMap<String, std::time::Duration>,
}

/// Runs the program one unit of charge at a time, attributing each step's
/// charge to the body its ready task's pending statement is in.
fn split(program: &Program, registry: &FunctionRegistry, name: &str) -> Split {
    let mut machine = start(program);
    let mut console = Console::default();
    let mut split = Split::default();
    let mut charged = 0;
    loop {
        let unit = pending_unit(&mut machine);
        let began = Instant::now();
        let step = machine
            .run(&mut console, 1)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let took = began.elapsed();
        split.steps += 1;
        let now = machine.meters().charged;
        let spent = now - charged;
        charged = now;
        match unit {
            Some(Unit::Library(function)) => {
                let label = registry
                    .get(&function)
                    .map_or_else(|| function.to_string(), |f| f.definition.name.to_string());
                if label.starts_with("ts.") {
                    split.helpers += spent;
                    split.helper_time += took;
                    *split.helper_time_by.entry(label.clone()).or_default() += took;
                    *split.by_helper.entry(label).or_default() += spent;
                } else {
                    split.library += spent;
                    split.library_time += took;
                }
            }
            _ => {
                split.program += spent;
                split.program_time += took;
            }
        }
        match step {
            Step::Slice => {}
            Step::Parked(park) => {
                deliver_all(&mut machine, park.requests);
            }
            Step::Ended(End::Finished(_)) => return split,
            Step::Ended(other) => panic!("{name} ended {other:?}"),
        }
    }
}

/// The site unit of the statement the next ready task runs next.
fn pending_unit(machine: &mut KernelMachine) -> Option<Unit> {
    let parked = machine.export().ok()?;
    let ready = parked.run.ready.first()?;
    let task = usize::try_from(ready.0).ok()?;
    let call = parked.tasks.get(task)?.calls.last()?;
    Some(call.call.statement.unit.clone())
}

#[expect(
    clippy::expect_used,
    reason = "benchmark setup: the shipped library and helpers register by construction"
)]
fn registry() -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();
    lash_kernel_lib::register_numbers(&mut registry).expect("numeric library registration");
    lash_kernel_lib::register_text_json(&mut registry).expect("text library registration");
    lash_kernel_vm::register_machine_functions(&mut registry)
        .expect("machine library registration");
    lash_kernel_lib::register_collections(&mut registry).expect("collection library registration");
    lash_ext_regex_ecma::register(
        &mut registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
    )
    .expect("regex extension registration");
    lash_ext_date_ecma::register(&mut registry).expect("date extension registration");
    lash_ext_url_whatwg::register(&mut registry).expect("URL extension registration");
    let mut library = NamedLibrary::from_registry(&registry).expect("unique kernel names");
    for definition in lash_dialect_typescript::define_helpers(&mut library)
        .unwrap_or_else(|error| panic!("{error}"))
    {
        registry
            .register(definition, None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    Arc::new(registry)
}

#[expect(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "benchmark entry point: it reads its arguments, its program files and its one switch, and reports on stdout"
)]
fn main() {
    let mut args = std::env::args().skip(1);
    let cap: u32 = args
        .next()
        .map_or(2000, |count| count.parse().expect("an iteration count"));
    // A name selects a program; a path to a `.ts` file adds one, named by
    // its path.
    let (files, only): (Vec<String>, BTreeSet<String>) = {
        let (files, names): (Vec<String>, Vec<String>) = args.partition(|arg| arg.ends_with(".ts"));
        (files, names.into_iter().collect())
    };
    let mut programs: Vec<(String, String)> = programs()
        .into_iter()
        .filter(|(name, _)| only.is_empty() && files.is_empty() || only.contains(*name))
        .map(|(name, source)| (name.to_string(), source))
        .collect();
    for file in files {
        let source =
            std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("{file}: {error}"));
        programs.push((file, source));
    }
    let registry = registry();
    // Prepared once, as a worker prepares its registry: every library body
    // is compiled here, and a run's start compiles only its document.
    let prepared = PreparedLibrary::new(Arc::clone(&registry));
    let library = NamedLibrary::from_registry(&registry).expect("unique library names");
    let effects = effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").expect("a tool's name"),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let tool_roots: BTreeSet<Name> = ["processes", "jobs"].into_iter().map(Name::new).collect();
    let environment = Environment {
        library: &library,
        effects: &effects,
        tool_roots: &tool_roots,
        controls: &controls,
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    println!(
        "program\tfunctions\titerations\tns_start_and_run\tns_run\tcharged\tsteps\thelper_charge\tlibrary_body_charge\tprogram_charge\ttop_helpers\thelper_time_share\tslowest_helpers\tlibrary_body_time_share\tprogram_time_share\tresult"
    );
    for (name, source) in &programs {
        let name = name.as_str();
        let lowered = match lash_dialect_typescript::lower(source, &environment) {
            Ok(lowered) => lowered,
            Err(diagnostic) => {
                println!("{name}\trefused\t{diagnostic:?}");
                continue;
            }
        };
        let functions = lowered.document.manifest.functions.len();
        let program = Program {
            document: Arc::new(lowered.document),
            library: prepared.clone(),
        };
        let first = Instant::now();
        let (charged, result) = to_end(&mut start(&program), name);
        // About two seconds a program, and at most the requested count.
        let once = first.elapsed().as_secs_f64().max(1e-6);
        let iterations = ((2.0 / once) as u32).clamp(3, cap);
        for _ in 0..iterations.min(20) {
            to_end(&mut start(&program), name);
        }
        let mut running = std::time::Duration::ZERO;
        let began = Instant::now();
        for _ in 0..iterations {
            let mut machine = start(&program);
            let ran = Instant::now();
            to_end(&mut machine, name);
            running += ran.elapsed();
        }
        let whole = began.elapsed();
        let per = |total: std::time::Duration| total.as_nanos() as f64 / f64::from(iterations);
        if std::env::var("KERNEL_PERF_SPLIT").is_ok_and(|split| split == "0") {
            println!(
                "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}",
                per(whole),
                per(running),
            );
            continue;
        }
        let split = split(&program, &registry, name);
        assert_eq!(
            split.helpers + split.library + split.program,
            charged,
            "{name}: a stepped run charges what a whole run charges"
        );
        let mut top: Vec<_> = split.by_helper.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1));
        let top = top
            .iter()
            .take(5)
            .map(|(helper, charge)| format!("{helper}={charge}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut slow: Vec<_> = split.helper_time_by.iter().collect();
        slow.sort_by(|a, b| b.1.cmp(a.1));
        let slow = slow
            .iter()
            .take(5)
            .map(|(helper, took)| format!("{helper}={}us", took.as_micros()))
            .collect::<Vec<_>>()
            .join(",");
        let stepped = split.helper_time + split.library_time + split.program_time;
        let helper_time_share = split.helper_time.as_secs_f64() / stepped.as_secs_f64().max(1e-9);
        let library_time_share = split.library_time.as_secs_f64() / stepped.as_secs_f64().max(1e-9);
        let program_time_share = split.program_time.as_secs_f64() / stepped.as_secs_f64().max(1e-9);
        println!(
            "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}\t{}\t{}\t{}\t{}\t{top}\t{helper_time_share:.6}\t{slow}\t{library_time_share:.6}\t{program_time_share:.6}\t{result:?}",
            per(whole),
            per(running),
            split.steps,
            split.helpers,
            split.library,
            split.program,
        );
    }
}
