//! Kernel execution measurements consumed by `scripts/perf/kernel/measure.py`.
//!
//! The programs of the retired VM's benchmark (`crates/lash-vm/benches/
//! benchmark.rs` at `515a596747`, the last main commit before the cutover),
//! written as the TypeScript a cell author would write and lowered through
//! this dialect. Each is timed on the kernel machine alone: lowering,
//! registry assembly and the library's preparation happen once, outside the
//! timed loop, as the old `compiled_execute` mode compiled once. A scripted host answers every
//! tool call at once.
//!
//! `kernel_perf <most iterations> [program or file.ts/file.py...]` runs each program
//! for about two seconds and prints one tab-separated row: the manifest's
//! function count, the time per run with and without `start`, the run's
//! charge, and where it went. The split comes from a second run that stops
//! after every unit of charge and reads the pending statement's site from the
//! sealed records (sealing changes no charge, `K-MACH-007`): the charge and
//! the diagnostic wall time of each charge-sliced step go to TypeScript helper bodies, kernel library bodies
//! or the document's own code. A helper's body is charged as it runs and
//! its formula is not (`K-CHG-007`), so a step is a statement of the body
//! that does the work;
//! the body of a library function with a native implementation is covered
//! by its formula and runs within one step. `KERNEL_PERF_SPLIT=0` skips the
//! second run, for a profiler. Split shares describe this altered dispatch
//! geometry, perturbed by sealing changed pages between steps; they are neither exact
//! inclusive CPU shares nor shares of the ordinary unsplit run.
//! Raw iteration times and allocator call counts
//! accompany every row, with executable and source hashes and build identity.
//! Counts exclude machine destruction and receipt construction. `KERNEL_PERF_FIXED=1`
//! uses the exact cap so maintained comparisons have fixed workload geometry.
//! Means remain descriptive; shared-host timing is advisory.
//! `KERNEL_PERF_OBSERVE=work` or `=context` times each run with the
//! machine's work counters or its published guest context on.
//!
//! `kernel_perf work <dir> [program...]` runs each program once with the
//! machine counting its work and writes `<dir>/kernel-work.json`: per guest
//! function and native, calls, operations, self and inclusive charge, native
//! calls and heap insertions. It refuses a program whose self charges do
//! not add up to its charge or whose observed run differs from a plain one.
//!
//! `kernel_perf guest <dir> [program...]` runs each program over and over
//! with the machine publishing its guest context, while a thread here reads
//! it at jittered intervals of about 10 ms, until at least
//! `KERNEL_PERF_GUEST_SAMPLES` (1000) samples of a running machine are
//! taken or `KERNEL_PERF_GUEST_SECONDS` (60) pass. It writes
//! `<dir>/guest.folded`, Inferno's folded stacks rooted at the program's
//! name, `<dir>/guest-symbols.tsv`, each frame's self and inclusive samples,
//! `<dir>/guest-statements.tsv`, interpreted samples by statement, and
//! `<dir>/guest-completeness.tsv`, every tick's outcome. Samples measure
//! wall residency, not CPU time.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

#[derive(Serialize)]
struct Sample {
    iteration: u32,
    ns_start_and_run: u128,
    ns_run: u128,
    allocations_start_and_run: usize,
    allocations_run: usize,
    reallocations_start_and_run: usize,
    reallocations_run: usize,
    charged: u64,
}

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Timestamp, Unit,
};
use lash_kernel_vm::observe::{GuestContext, GuestProfile, GuestSymbols, WorkReport};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Step, Target,
};
use serde_json::{Value as Json, json};
#[expect(
    dead_code,
    reason = "the benchmark shares the page fixture's start and source, not its session seeding helpers"
)]
#[path = "../src/tests/pages.rs"]
mod pages;

#[path = "../shared/gate_six.rs"]
mod gate_six;

#[path = "../shared/gate_six_registry.rs"]
mod gate_six_registry;

use gate_six::{answer, effects, programs};
use gate_six_registry::registry;

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
    start_on(program, &pages::Source::default())
}
fn start_on(program: &Program, source: &pages::Source) -> KernelMachine {
    let session = pages::Session::default();
    let start = CellStart {
        owner: pages::OWNER,
        target: Target::Main,
        args: Vec::new(),
        session: None,
        modes: lash_kernel_vm::EffectModes::recorded(),
        host: Vec::new(),
    };
    KernelMachine::start(
        program.clone(),
        BOUNDS,
        start,
        Box::new(source.clone()),
        session.census(BOUNDS.memory),
    )
    .unwrap_or_else(|error| panic!("{error}"))
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
    /// Calls to `run(1)`, including parks and the terminal call.
    run_calls: u64,
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
    let source = pages::Source::default();
    let mut machine = start_on(program, &source);
    let mut console = Console::default();
    let mut split = Split::default();
    let mut charged = 0;
    loop {
        let unit = pending_unit(&mut machine, &source);
        let began = Instant::now();
        let step = machine
            .run(&mut console, 1)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let took = began.elapsed();
        split.run_calls += 1;
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
fn pending_unit(machine: &mut KernelMachine, source: &pages::Source) -> Option<Unit> {
    use lash_kernel_state::paged::{EditEpoch, FrameRecord, PagedHeap, TaskRecord, tree::Tree};
    let sealed = machine.seal(pages::BUDGET).ok()??;
    let root = sealed.root;
    let heap = PagedHeap::open_header(root.heap, Box::new(source.clone()), EditEpoch(1)).ok()?;
    let ready = Tree::<lash_kernel_doc::TaskId>::of_sequence(&root.ready);
    let (_, next) = ready.rank_get(0, &heap.access).ok()??;
    let tasks = Tree::<TaskRecord>::of_keyed(&root.tasks);
    let task = tasks
        .get(
            &lash_kernel_state::paged::directory::ordinal_key(next.0),
            &heap.access,
        )
        .ok()??;
    let (_, frame): (_, FrameRecord) = task
        .frames
        .rank_get(task.frames.len().checked_sub(1)?, &heap.access)
        .ok()??;
    Some(frame.statement.resolve(&heap.access).ok()?.unit.clone())
}

/// What one invocation does.
enum Mode {
    /// Times each program, and splits its charge and time.
    Time,
    /// Counts each program's work into `kernel-work.json`.
    Work(PathBuf),
    /// Samples each program's guest context into folded stacks.
    Guest(PathBuf),
}

/// What `KERNEL_PERF_OBSERVE` turns on in the timed runs.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Observe {
    Nothing,
    Work,
    Context,
}

/// Starts a run with what `observe` asks for on.
fn start_observed(
    program: &Program,
    observe: Observe,
    context: &Arc<GuestContext>,
) -> KernelMachine {
    let mut machine = start(program);
    match observe {
        Observe::Nothing => {}
        Observe::Work => machine.count_work(),
        Observe::Context => machine.publish_context(Arc::clone(context)),
    }
    machine
}

#[expect(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "benchmark entry point: it reads its arguments, its program files and its switches, and reports on stdout"
)]
fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "--list-programs") {
        args.next();
        if args.next().is_some() {
            return Err("--list-programs takes no selection".to_string());
        }
        for (name, _) in programs() {
            println!("{name}");
        }
        return Ok(());
    }
    let mode = match args.peek().map(String::as_str) {
        Some(mode @ ("work" | "guest")) => {
            let work = mode == "work";
            args.next();
            let dir = PathBuf::from(args.next().ok_or("an output directory")?);
            std::fs::create_dir_all(&dir).map_err(|error| format!("{dir:?}: {error}"))?;
            if work {
                Mode::Work(dir)
            } else {
                Mode::Guest(dir)
            }
        }
        _ => Mode::Time,
    };
    let cap: u32 = match mode {
        Mode::Time => args
            .next()
            .map_or(Ok(2000), |count| count.parse())
            .map_err(|error| format!("invalid iteration count: {error}"))?,
        Mode::Work(_) | Mode::Guest(_) => 1,
    };
    if cap == 0 {
        return Err("iteration count must be positive".to_string());
    }
    let observe = match std::env::var("KERNEL_PERF_OBSERVE").as_deref() {
        Ok("work") => Observe::Work,
        Ok("context") => Observe::Context,
        Ok("") | Err(_) => Observe::Nothing,
        Ok(other) => {
            return Err(format!(
                "KERNEL_PERF_OBSERVE is work or context, not {other}"
            ));
        }
    };
    let context = GuestContext::new();
    let mut work = Vec::new();
    let mut guest = Guest::default();
    // A name selects a program; a path to a `.ts` or `.py` file adds one, named by
    // its path.
    let (files, only): (Vec<String>, BTreeSet<String>) = {
        let (files, names): (Vec<String>, Vec<String>) =
            args.partition(|arg| arg.ends_with(".ts") || arg.ends_with(".py"));
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
    for name in &only {
        if !programs.iter().any(|(selected, _)| selected == name) {
            return Err(format!("unknown kernel program: {name}"));
        }
    }
    if programs.is_empty() {
        return Err("empty kernel selection".to_string());
    }
    let artifact = format!(
        "{:x}",
        Sha256::digest(
            std::fs::read(std::env::current_exe().map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?
        )
    );
    let observed_split = !std::env::var("KERNEL_PERF_SPLIT").is_ok_and(|split| split == "0");
    let fixed = std::env::var("KERNEL_PERF_FIXED").is_ok_and(|fixed| fixed == "1");
    let registry = registry();
    let mut registry = (*registry).clone();
    let mut names = NamedLibrary::from_registry(&registry).expect("unique library names");
    for definition in
        lash_dialect_python::define_helpers(&mut names).unwrap_or_else(|error| panic!("{error}"))
    {
        registry
            .register(definition, None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    let registry = Arc::new(registry);
    // Prepared once, as a worker prepares its registry: every library body
    // is compiled here; each live document's preparation is reused at start.
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
    if matches!(mode, Mode::Time) {
        println!(
            "program\tfunctions\titerations\tns_start_and_run\tns_run\tcharged\trun_calls\thelper_charge\tlibrary_body_charge\tprogram_charge\ttop_helpers\tsplit_helper_wall_share\tsplit_slowest_helpers\tsplit_library_body_wall_share\tsplit_program_wall_share\tresult\treceipt"
        );
    }
    for (name, source) in &programs {
        let name = name.as_str();
        let lowered = match if name.ends_with(".py") {
            lash_dialect_python::lower(source, &environment)
                .map(|lowered| lowered.document)
                .map_err(|error| format!("{error:?}"))
        } else {
            lash_dialect_typescript::lower(source, &environment)
                .map(|lowered| lowered.document)
                .map_err(|error| format!("{error:?}"))
        } {
            Ok(lowered) => lowered,
            Err(diagnostic) => {
                return Err(format!("{name}: refused: {diagnostic:?}"));
            }
        };
        let functions = lowered.manifest.functions.len();
        let program = Program {
            release: pages::RELEASE,
            document: Arc::new(lowered),
            library: prepared.clone(),
        };
        match &mode {
            Mode::Time => {}
            Mode::Work(_) => {
                work.push(count(name, &program));
                continue;
            }
            Mode::Guest(_) => {
                guest.sample(name, &program);
                continue;
            }
        }
        let first = Instant::now();
        let (charged, result) = to_end(&mut start(&program), name);
        // About two seconds a program, and at most the requested count.
        let once = first.elapsed().as_secs_f64().max(1e-6);
        let iterations = if fixed {
            cap
        } else {
            ((2.0 / once) as u32).clamp(cap.min(3), cap)
        };
        let warmups = iterations.min(20);
        for _ in 0..warmups {
            to_end(&mut start_observed(&program, observe, &context), name);
        }
        let mut samples = Vec::with_capacity(iterations as usize);
        for iteration in 0..iterations {
            let whole_counts = stats_alloc::Region::new(ALLOCATOR);
            let began = Instant::now();
            let mut machine = start_observed(&program, observe, &context);
            let run_counts = stats_alloc::Region::new(ALLOCATOR);
            let ran = Instant::now();
            let (actual_charge, actual_result) = to_end(&mut machine, name);
            let ns_run = ran.elapsed().as_nanos();
            let counts_run = run_counts.change();
            let ns_start_and_run = began.elapsed().as_nanos();
            let counts_whole = whole_counts.change();
            assert_eq!(actual_charge, charged, "{name}: iteration charge");
            assert_eq!(actual_result, result, "{name}: iteration result");
            samples.push(Sample {
                iteration,
                ns_start_and_run,
                ns_run,
                allocations_start_and_run: counts_whole.allocations,
                allocations_run: counts_run.allocations,
                reallocations_start_and_run: counts_whole.reallocations,
                reallocations_run: counts_run.reallocations,
                charged: actual_charge,
            });
        }
        let mean = |get: fn(&Sample) -> u128| {
            samples.iter().map(get).sum::<u128>() as f64 / f64::from(iterations)
        };
        // Canonical Datum JSON: recursively sorted object keys, compact UTF-8.
        // Datum's ordered collections and typed numeric spellings are preserved.
        // Hashing and serialization happen after every measured window.
        let mut canonical_result =
            serde_json::to_value(&result).map_err(|error| error.to_string())?;
        canonical_result.sort_all_objects();
        let result_sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&canonical_result).map_err(|error| error.to_string())?
            )
        );
        let receipt = serde_json::to_string(&serde_json::json!({
            "program": name,
            "artifact_sha256": artifact,
            "workload_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
            "result_sha256": result_sha256,
            "observe": observe,
            "compiler": lash_dialect_typescript::BUILD_IDENTITY.0,
            "build_profile": lash_dialect_typescript::BUILD_IDENTITY.1,
            "debug_assertions": cfg!(debug_assertions),
            "allocator": "system+stats_alloc",
            "split": observed_split,
            "fixed_iterations": fixed,
            "cap": cap,
            "iterations": iterations,
            "warmups": warmups,
            "samples": samples,
        }))
        .map_err(|error| error.to_string())?;
        if !observed_split {
            println!(
                "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}\t\t\t\t\t\t\t\t\t\t{result:?}\t{receipt}",
                mean(|sample| sample.ns_start_and_run),
                mean(|sample| sample.ns_run),
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
            "{name}\t{functions}\t{iterations}\t{:.1}\t{:.1}\t{charged}\t{}\t{}\t{}\t{}\t{top}\t{helper_time_share:.6}\t{slow}\t{library_time_share:.6}\t{program_time_share:.6}\t{result:?}\t{receipt}",
            mean(|sample| sample.ns_start_and_run),
            mean(|sample| sample.ns_run),
            split.run_calls,
            split.helpers,
            split.library,
            split.program,
        );
    }
    match &mode {
        Mode::Time => {}
        Mode::Work(dir) => {
            let path = dir.join("kernel-work.json");
            let report = json!({
                "instrument": "kernel-work",
                "quantity": "exact counts of one run per program: charge in kernel charge units, operations are statements plus expressions and action operands evaluated",
                "programs": work,
            });
            let text = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
            std::fs::write(&path, text).map_err(|error| format!("{path:?}: {error}"))?;
            println!("{}", path.display());
        }
        Mode::Guest(dir) => guest.write(dir)?,
    }
    Ok(())
}

/// Runs one program once with its work counted, and checks the counts
/// against the run and against a plain run.
fn count(name: &str, program: &Program) -> Json {
    let plain = to_end(&mut start(program), name);
    let mut machine = start(program);
    machine.count_work();
    let counted = to_end(&mut machine, name);
    assert_eq!(counted, plain, "{name}: counting changed the run");
    let Some(work) = machine.work() else {
        panic!("{name}: the machine counted nothing")
    };
    assert_eq!(
        work.charged, plain.0,
        "{name}: the counts saw another charge"
    );
    assert_eq!(work.residual(), 0, "{name}: the self charges do not add up");
    assert_eq!(
        work.insertion_residual(),
        0,
        "{name}: the insertions do not add up"
    );
    assert_eq!(
        work.unattributed_charge, 0,
        "{name}: charge with no function"
    );
    work_json(name, &work, &machine.guest_symbols())
}

fn work_json(name: &str, work: &WorkReport, symbols: &GuestSymbols) -> Json {
    let native_name = |token: u32| {
        symbols
            .lib(token)
            .map_or_else(|| format!("#{token}"), |lib| lib.name.clone())
    };
    let calls = |calls: &[(u32, u64)]| -> Json {
        calls
            .iter()
            .map(|(token, count)| json!({ "native": native_name(*token), "calls": count }))
            .collect()
    };
    let mut codes: Vec<_> = work.codes.iter().collect();
    codes.sort_by(|a, b| {
        b.counts
            .inclusive_charge
            .cmp(&a.counts.inclusive_charge)
            .then(a.symbol.token.cmp(&b.symbol.token))
    });
    let mut natives: Vec<_> = work.natives.iter().collect();
    natives.sort_by(|a, b| {
        b.counts
            .self_charge
            .cmp(&a.counts.self_charge)
            .then(b.counts.calls.cmp(&a.counts.calls))
            .then(a.symbol.token.cmp(&b.symbol.token))
    });
    json!({
        "program": name,
        "charged": work.charged,
        "self_charge_residual": i64::try_from(work.residual()).unwrap_or(i64::MAX),
        "unattributed_charge": work.unattributed_charge,
        "heap_insertions": work.heap_insertions,
        "heap_insertion_residual": i64::try_from(work.insertion_residual()).unwrap_or(i64::MAX),
        "unattributed_heap_insertions": work.unattributed_insertions,
        "unattributed_native_calls": calls(&work.unattributed_native_calls),
        "functions": codes.iter().map(|code| {
            let counts = &code.counts;
            json!({
                "name": code.symbol.name,
                "kind": code.symbol.kind.as_str(),
                "function": code.symbol.function.map(|function| function.to_string()),
                "site": code.symbol.site.to_string(),
                "calls": counts.calls,
                "operations": counts.statements + counts.expressions,
                "statements": counts.statements,
                "expressions": counts.expressions,
                "uncharged_operations": counts.uncharged_operations,
                "self_charge": counts.self_charge,
                "inclusive_charge": counts.inclusive_charge,
                "heap_insertions": counts.heap_insertions,
                "native_calls": calls(&code.native_calls),
            })
        }).collect::<Vec<_>>(),
        "natives": natives.iter().map(|native| {
            let counts = &native.counts;
            json!({
                "name": native.symbol.name,
                "kind": native.symbol.kind.as_str(),
                "function": native.symbol.function.to_string(),
                "calls": counts.calls,
                "fast_calls": counts.fast_calls,
                "zero_charge_calls": counts.zero_charge_calls,
                "retries": counts.retries,
                "guard_work": counts.guard_work,
                "formula_evaluations": counts.formula_evaluations,
                "self_charge": counts.self_charge,
                "heap_insertions": counts.heap_insertions,
            })
        }).collect::<Vec<_>>(),
    })
}

/// A small xorshift: the sampler's jitter, seeded the same every time.
struct Jitter(u64);

impl Jitter {
    /// An interval uniform in half to one and a half times `mean`.
    fn next(&mut self, mean: Duration) -> Duration {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
        mean.mul_f64(0.5 + unit)
    }
}

/// The sampler: reads `context` at jittered intervals about `mean` apart
/// until `stop`, counting the deadlines it passed while late as missed.
fn sampler(
    context: Arc<GuestContext>,
    mean: Duration,
    stop: Arc<AtomicBool>,
    accepted: Arc<AtomicU64>,
) -> GuestProfile {
    let mut jitter = Jitter(0x9e37_79b9_7f4a_7c15);
    let mut profile = GuestProfile::new();
    let mut deadline = Instant::now() + jitter.next(mean);
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
        let woke = Instant::now();
        let mut next = deadline + jitter.next(mean);
        let mut missed = 0;
        while next <= woke {
            missed += 1;
            next += jitter.next(mean);
        }
        profile.tick(&context);
        profile.missed(missed);
        accepted.store(profile.completeness().accepted, Ordering::Relaxed);
        deadline = next;
    }
    profile
}

/// Every program's guest profile.
#[derive(Default)]
struct Guest {
    profiles: Vec<(String, GuestProfile, GuestSymbols, Duration, u64)>,
}

impl Guest {
    #[expect(
        clippy::disallowed_methods,
        reason = "benchmark switches: the sample count and time budget"
    )]
    fn sample(&mut self, name: &str, program: &Program) {
        let target: u64 = std::env::var("KERNEL_PERF_GUEST_SAMPLES")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(1000);
        let budget = Duration::from_secs(
            std::env::var("KERNEL_PERF_GUEST_SECONDS")
                .ok()
                .and_then(|seconds| seconds.parse().ok())
                .unwrap_or(60),
        );
        let context = GuestContext::new();
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicU64::new(0));
        let thread = {
            let (context, stop, accepted) = (
                Arc::clone(&context),
                Arc::clone(&stop),
                Arc::clone(&accepted),
            );
            std::thread::spawn(move || sampler(context, Duration::from_millis(10), stop, accepted))
        };
        let symbols = start(program).guest_symbols();
        let began = Instant::now();
        let mut runs = 0u64;
        while accepted.load(Ordering::Relaxed) < target && began.elapsed() < budget {
            let mut machine = start(program);
            machine.publish_context(Arc::clone(&context));
            to_end(&mut machine, name);
            runs += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let profile = thread
            .join()
            .unwrap_or_else(|_| panic!("{name}: the sampler panicked"));
        let counts = profile.completeness();
        assert!(
            counts.adds_up(),
            "{name}: the ticks do not add up: {counts:?}"
        );
        self.profiles
            .push((name.to_string(), profile, symbols, began.elapsed(), runs));
    }

    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        reason = "benchmark report: it writes its files where its argument says and names them"
    )]
    fn write(&self, dir: &Path) -> Result<(), String> {
        let mut folded = String::new();
        let mut symbols_tsv = String::from(
            "program\tframe\tkind\tself_samples\tinclusive_samples\tself_share\tinclusive_share\tfunction\tsite\n",
        );
        let mut statements_tsv = String::from("program\tcode\tstatement_site\tsamples\n");
        let mut completeness_tsv = String::from(
            "program\tseconds\truns\tticks\taccepted\tinactive\tparked\tlost_to_races\tmissed\tunknown_phase\traces\ttruncated\tunresolved\tinterpret\tnative\tformula\tcollect\tadds_up\tpopulation\n",
        );
        for (name, profile, symbols, took, runs) in &self.profiles {
            folded.push_str(&profile.folded(Some(name), symbols));
            let counts = profile.completeness();
            let accepted = counts.accepted as f64;
            for time in profile
                .symbol_times(symbols)
                .into_iter()
                .filter(|_| counts.accepted > 0)
            {
                let (kind, function, site) = describe(&time.frame, symbols);
                let _ = writeln!(
                    symbols_tsv,
                    "{name}\t{}\t{kind}\t{}\t{}\t{:.4}\t{:.4}\t{function}\t{site}",
                    time.frame,
                    time.self_samples,
                    time.inclusive_samples,
                    time.self_samples as f64 / accepted,
                    time.inclusive_samples as f64 / accepted,
                );
            }
            for (code, site, samples) in profile.statement_times(symbols) {
                let _ = writeln!(statements_tsv, "{name}\t{code}\t{site}\t{samples}");
            }
            let _ = writeln!(
                completeness_tsv,
                "{name}\t{:.3}\t{runs}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                took.as_secs_f64(),
                counts.ticks,
                counts.accepted,
                counts.inactive,
                counts.parked,
                counts.lost_to_races,
                counts.missed,
                counts.unknown_phase,
                counts.races,
                counts.truncated,
                profile.unresolved(symbols),
                counts.interpret,
                counts.native,
                counts.formula,
                counts.collect,
                counts.adds_up(),
                if counts.accepted == 0 {
                    "insufficient"
                } else {
                    "diagnostic_wall_residency"
                },
            );
        }
        for (file, text) in [
            ("guest.folded", folded),
            ("guest-symbols.tsv", symbols_tsv),
            ("guest-statements.tsv", statements_tsv),
            ("guest-completeness.tsv", completeness_tsv),
        ] {
            let path = dir.join(file);
            std::fs::write(&path, text).unwrap_or_else(|error| panic!("{path:?}: {error}"));
            println!("{}", path.display());
        }
        if self
            .profiles
            .iter()
            .any(|(_, profile, _, _, _)| profile.completeness().accepted == 0)
        {
            return Err(
                "insufficient guest population: zero accepted samples; raw diagnostics retained"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// A folded frame's kind, function identity and site.
fn describe(frame: &str, symbols: &GuestSymbols) -> (String, String, String) {
    let lib = |name: &str| symbols.libs.iter().find(|lib| lib.name == name);
    if let Some(name) = frame
        .strip_prefix("native:")
        .or_else(|| frame.strip_prefix("formula:"))
    {
        let kind = if frame.starts_with("native:") {
            "native"
        } else {
            "formula"
        };
        return match lib(name) {
            Some(lib) => (kind.to_string(), lib.function.to_string(), "-".to_string()),
            None => (kind.to_string(), "-".to_string(), "-".to_string()),
        };
    }
    match symbols.codes.iter().find(|code| code.name == frame) {
        Some(code) => (
            code.kind.as_str().to_string(),
            code.function
                .map_or_else(|| "-".to_string(), |function| function.to_string()),
            code.site.to_string(),
        ),
        None => ("marker".to_string(), "-".to_string(), "-".to_string()),
    }
}
