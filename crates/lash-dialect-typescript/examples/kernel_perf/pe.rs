//! The partial-evaluation spike's modes (FIG-5863): the same programs over a
//! plain library and over one whose hot helpers have residual variants.
//!
//! - `pe-stats`: the variants made, their guards, folds and residual size.
//! - `pe-check [program...]`: both libraries in lockstep at slices 1, 2, 3,
//!   7, 97 and unbounded, comparing every step, meter, park, end and
//!   exported state.
//! - `pe-park`: a park inside a callback that a residual `ts.array.map` frame
//!   called; each export there is resumed over the plain library.
//! - `pe-paired [program...]`: paired timings, min of 3 rounds.
//! - `pe-kmicro`: each hot helper's cost per call, paired.
//! - `pe-hits [program...]`: which variants the programs enter.
//!
//! `PE_FUNCTIONS` (comma-separated; `all` for every helper) picks the
//! helpers; `PE_INPUTS` the inputs per variant.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "spike harness: it reads its arguments and environment and reports on stdout"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{Datum, EffectName, FunctionRegistry, Name, Signature, Unit};
use lash_kernel_vm::{
    Bindings, KernelMachine, Machine, PeOptions, PeSelection, PreparedLibrary, Program, Start,
    Step, Target,
};

use super::{BOUNDS, Console, deliver_all, effects, programs, registry};

type ParkedRun = <KernelMachine as Machine>::Parked;

/// The spike's three helpers. Since FIG-5873 `ts.json.stringify` is a
/// wrapper and its serialize closure lives in `ts.json.stringify_generic`.
const HOT: [&str; 4] = [
    "ts.get",
    "ts.array.map",
    "ts.json.stringify",
    "ts.json.stringify_generic",
];

struct Setup {
    registry: Arc<FunctionRegistry>,
    plain: PreparedLibrary,
    special: PreparedLibrary,
    options: PeOptions,
    library: NamedLibrary,
    effects: BTreeMap<EffectName, Signature>,
    controls: BTreeMap<EffectName, BTreeSet<EffectControl>>,
    tool_roots: BTreeSet<Name>,
    pe_time: Duration,
}

fn options(count: bool) -> PeOptions {
    let functions = std::env::var("PE_FUNCTIONS").unwrap_or_else(|_| HOT.join(","));
    let functions = if functions == "all" {
        Vec::new()
    } else {
        functions.split(',').map(str::to_string).collect()
    };
    let inputs = std::env::var("PE_INPUTS")
        .ok()
        .map_or(2, |inputs| inputs.parse().expect("a count of inputs"));
    PeOptions {
        functions,
        inputs_per_variant: inputs,
        count,
        ..PeOptions::default()
    }
}

impl Setup {
    fn new(count: bool) -> Self {
        let registry = registry();
        let plain = PreparedLibrary::new(Arc::clone(&registry));
        let mut options = options(count);
        // `PE_PROFILE=1` chooses variants by what the gate programs'
        // activations pass: a profiling run over every candidate first.
        if std::env::var("PE_PROFILE").is_ok_and(|on| on == "1") {
            options.selection = profile(&registry, &options);
        }
        let began = Instant::now();
        let special = PreparedLibrary::specialized(Arc::clone(&registry), &options);
        let pe_time = began.elapsed();
        let library = NamedLibrary::from_registry(&registry).expect("unique library names");
        let controls = BTreeMap::from([(
            EffectName::new("finish").expect("a tool's name"),
            BTreeSet::from([EffectControl::Finish]),
        )]);
        let tool_roots: BTreeSet<Name> = ["processes", "jobs"].into_iter().map(Name::new).collect();
        Self {
            registry,
            plain,
            special,
            options,
            library,
            effects: effects(),
            controls,
            tool_roots,
            pe_time,
        }
    }

    fn new_over(
        registry: Arc<FunctionRegistry>,
        plain: PreparedLibrary,
        special: PreparedLibrary,
    ) -> Self {
        let library = NamedLibrary::from_registry(&registry).expect("unique library names");
        let controls = BTreeMap::from([(
            EffectName::new("finish").expect("a tool's name"),
            BTreeSet::from([EffectControl::Finish]),
        )]);
        let tool_roots: BTreeSet<Name> = ["processes", "jobs"].into_iter().map(Name::new).collect();
        Self {
            registry,
            plain,
            special,
            options: PeOptions::default(),
            library,
            effects: effects(),
            controls,
            tool_roots,
            pe_time: Duration::ZERO,
        }
    }

    /// The program lowered once, over each library.
    fn programs(&self, source: &str) -> Option<(Program, Program)> {
        let bindings = BTreeSet::new();
        let functions = BTreeMap::new();
        let environment = Environment {
            library: &self.library,
            effects: &self.effects,
            tool_roots: &self.tool_roots,
            controls: &self.controls,
            bindings: &bindings,
            functions: &functions,
        };
        let lowered = match lash_dialect_typescript::lower(source, &environment) {
            Ok(lowered) => lowered,
            Err(diagnostic) => {
                eprintln!("refused: {diagnostic:?}");
                return None;
            }
        };
        let document = Arc::new(lowered.document);
        Some((
            Program {
                document: Arc::clone(&document),
                library: self.plain.clone(),
            },
            Program {
                document,
                library: self.special.clone(),
            },
        ))
    }

    fn name_of(&self, unit: &Unit) -> String {
        match unit {
            Unit::Library(function) => self
                .registry
                .get(function)
                .map_or_else(|| function.to_string(), |f| f.definition.name.to_string()),
            Unit::Main => "main".to_string(),
            Unit::Function(name) => name.to_string(),
        }
    }
}

/// Runs every gate program once over a library that counts, for every
/// candidate variant, the activations whose inputs pass its guards.
fn profile(registry: &Arc<FunctionRegistry>, options: &PeOptions) -> PeSelection {
    let profiling = PeOptions {
        selection: PeSelection::Profiling,
        ..options.clone()
    };
    let library = PreparedLibrary::specialized(Arc::clone(registry), &profiling);
    let mut setup = Setup::new_over(Arc::clone(registry), library.clone(), library.clone());
    setup.options = profiling;
    for (_, source) in selected(&[]) {
        if let Some((program, _)) = setup.programs(&source) {
            drive(&mut start(&program), u64::MAX);
        }
    }
    let counts = library
        .pe_hits()
        .into_iter()
        .filter(|(_, _, hits)| *hits > 0)
        .map(|(code, guards, hits)| ((code, guards), hits))
        .collect();
    PeSelection::Profiled(counts)
}

fn start(program: &Program) -> KernelMachine {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    KernelMachine::start(program.clone(), BOUNDS, start).unwrap_or_else(|error| panic!("{error}"))
}

/// What a run shows: its end, the value it finished with and its meters.
#[derive(Debug, PartialEq)]
struct Observed {
    end: String,
    finished: Option<Datum>,
    charged: u64,
    memory: u64,
    printed: usize,
}

fn drive(machine: &mut KernelMachine, slice: u64) -> Observed {
    let mut console = Console::default();
    let mut finished = None;
    let end = loop {
        match machine
            .run(&mut console, slice)
            .unwrap_or_else(|error| panic!("{error}"))
        {
            Step::Slice => {}
            Step::Parked(park) => {
                if let Some(value) = deliver_all(machine, park.requests) {
                    finished = Some(value);
                }
            }
            Step::Ended(end) => break end,
        }
    };
    let meters = machine.meters();
    Observed {
        end: format!("{end:?}"),
        finished,
        charged: meters.charged,
        memory: meters.memory,
        printed: console.printed,
    }
}

pub(super) fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let setup = Setup::new(mode == "pe-hits" || mode == "pe-park");
    match mode.as_str() {
        "pe-stats" => stats(&setup),
        "pe-check" => check(&setup, &rest),
        "pe-park" => park(&setup),
        "pe-paired" => paired(&setup, &rest),
        "pe-kmicro" => kmicro(&setup, &rest),
        "pe-hits" => hits(&setup, &rest),
        "pe-bounds" => bounds(&setup, &rest),
        "pe-json" => json(&setup, &rest),
        "pe-census" => census(),
        other => panic!("unknown mode {other}"),
    }
}

fn selected(rest: &[String]) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = programs()
        .into_iter()
        .map(|(name, source)| (name.to_string(), source))
        .collect();
    all.extend(
        micro_programs()
            .into_iter()
            .map(|(name, source)| (name.to_string(), source)),
    );
    all.into_iter()
        .filter(|(name, _)| rest.is_empty() || rest.iter().any(|wanted| wanted == name))
        .collect()
}

fn stats(setup: &Setup) {
    let report = setup.special.pe_report();
    let variants: usize = report.iter().map(|code| code.variants.len()).sum();
    let generic: u64 = report.iter().map(|code| code.generic_nodes).sum();
    let residual: u64 = report
        .iter()
        .flat_map(|code| &code.variants)
        .map(|variant| variant.residual_nodes)
        .sum();
    println!(
        "options\t{:?}\nspecialized_codes\t{}\tvariants\t{variants}\tgeneric_nodes_of_specialized\t{generic}\tresidual_nodes\t{residual}\tprepare_with_pe_ms\t{:.3}",
        setup.options,
        report.len(),
        setup.pe_time.as_secs_f64() * 1e3,
    );
    println!("code\tgeneric_nodes\tguards\tscore\tfolds\tresidual_nodes\tcode_id");
    for code in report {
        for variant in &code.variants {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                code.name,
                code.generic_nodes,
                variant.guards,
                variant.score,
                variant.folds,
                variant.residual_nodes,
                variant.code
            );
        }
    }
}

/// Runs both libraries in lockstep with the same slice, comparing every
/// step, the meters after it and the state each exports there. Gives
/// the number of exported states compared and of steps.
fn lockstep(name: &str, plain: &Program, special: &Program, slice: u64) -> (usize, u64) {
    let mut console = Console::default();
    let mut twin_console = Console::default();
    let mut generic = start(plain);
    let mut machine = start(special);
    let mut compared = 0;
    let mut steps = 0u64;
    loop {
        let twin = generic.run(&mut twin_console, slice).expect("a step");
        let step = machine.run(&mut console, slice).expect("a step");
        steps += 1;
        match (generic.export(), machine.export()) {
            (Ok(a), Ok(b)) => {
                if a != b {
                    panic!(
                        "{name} slice {slice} step {steps}: the exported states differ\n{:#?}\n{:#?}",
                        a.tasks, b.tasks
                    );
                }
                compared += 1;
            }
            (Err(_), Err(_)) => {}
            (a, b) => panic!("{name} slice {slice} step {steps}: export {a:?} vs {b:?}"),
        }
        let (meters, twin_meters) = (machine.meters(), generic.meters());
        assert_eq!(
            (meters.charged, meters.memory, meters.live_tasks),
            (
                twin_meters.charged,
                twin_meters.memory,
                twin_meters.live_tasks
            ),
            "{name} slice {slice} step {steps}: the meters"
        );
        match (twin, step) {
            (Step::Slice, Step::Slice) => {}
            (Step::Parked(twin_park), Step::Parked(park)) => {
                assert_eq!(
                    format!("{twin_park:?}"),
                    format!("{park:?}"),
                    "{name} slice {slice} step {steps}: the parks"
                );
                let a = deliver_all(&mut generic, twin_park.requests);
                let b = deliver_all(&mut machine, park.requests);
                assert_eq!(a, b, "{name}: what finished");
            }
            (Step::Ended(twin_end), Step::Ended(end)) => {
                assert_eq!(
                    format!("{twin_end:?}"),
                    format!("{end:?}"),
                    "{name} slice {slice}: the ends"
                );
                assert!(
                    format!("{end:?}").starts_with("Finished"),
                    "{name}: {end:?}"
                );
                assert_eq!(console.printed, twin_console.printed, "{name}: prints");
                return (compared, steps);
            }
            (twin, step) => panic!("{name} slice {slice} step {steps}: {twin:?} vs {step:?}"),
        }
    }
}

fn check(setup: &Setup, rest: &[String]) {
    let mut checked = 0;
    let mut exports = 0;
    let mut steps = 0;
    for (name, source) in selected(rest) {
        let Some((plain, special)) = setup.programs(&source) else {
            println!("{name}\trefused");
            continue;
        };
        // A program that holds a large heap copies it at every export.
        let slices: &[u64] = if name == "rows_print_3000" {
            &[97, u64::MAX]
        } else {
            &[1, 2, 3, 7, 97, u64::MAX]
        };
        let mut row = Vec::new();
        for &slice in slices {
            let (compared, stepped) = lockstep(&name, &plain, &special, slice);
            exports += compared;
            steps += stepped;
            checked += 1;
            row.push(format!("{slice}:{compared}"));
        }
        println!("{name}\tsame\t{}", row.join(" "));
    }
    println!("checked\t{checked}\tsteps\t{steps}\texports_compared\t{exports}");
}

const MAP_PARK: &str = r#"
const source: number[] = [];
for (let n = 0; n < 12; n++) { source.push(n); }
let calls = 0;
const doubled = source.map((item: number) => { calls = calls + 1; return item + item + calls; });
await finish({ doubled: doubled, calls: calls, last: doubled[doubled.length - 1] });
"#;

/// Whether a parked run's first task is inside a callback that a
/// `ts.array.map` frame called: the map's frame below, document code on
/// top.
fn inside_map_callback(setup: &Setup, parked: &ParkedRun) -> bool {
    let Some(task) = parked.tasks.first() else {
        return false;
    };
    let units: Vec<String> = task
        .calls
        .iter()
        .map(|call| setup.name_of(&call.call.statement.unit))
        .collect();
    units.len() >= 2
        && units[..units.len() - 1]
            .iter()
            .any(|unit| unit == "ts.array.map")
        && units.last().is_some_and(|unit| unit == "main")
}

fn park(setup: &Setup) {
    let (plain, special) = setup.programs(MAP_PARK).expect("the program lowers");
    let straight = drive(&mut start(&plain), u64::MAX);
    assert!(straight.end.starts_with("Finished"), "{}", straight.end);
    let mut console = Console::default();
    let mut generic = start(&plain);
    let mut machine = start(&special);
    let mut parks_inside = 0;
    let mut resumed_runs = 0;
    let mut exports = 0;
    loop {
        // One unit of charge at a time on both: each step a safe point.
        let step = machine.run(&mut console, 1).expect("a step");
        let twin = generic.run(&mut console, 1).expect("a step");
        match (&step, &twin) {
            (Step::Slice, Step::Slice) => {}
            (Step::Parked(park), Step::Parked(twin_park)) => {
                deliver_all(&mut machine, park.requests.clone());
                deliver_all(&mut generic, twin_park.requests.clone());
                continue;
            }
            (Step::Ended(_), Step::Ended(_)) => break,
            other => panic!("the libraries step apart: {other:?}"),
        }
        let parked = machine.export().expect("an export");
        let twin_parked = generic.export().expect("an export");
        assert_eq!(
            parked, twin_parked,
            "a residual frame saves what the generic one saves"
        );
        exports += 1;
        if !inside_map_callback(setup, &parked) {
            continue;
        }
        assert!(
            machine.residual_frames() > 0,
            "a residual ts.array.map frame sits under the callback"
        );
        assert_eq!(generic.residual_frames(), 0);
        parks_inside += 1;
        // Resume this state over the plain library, on a machine built
        // afresh.
        let mut resumed =
            KernelMachine::import(plain.clone(), BOUNDS, parked).expect("the state imports");
        let resumed = drive(&mut resumed, u64::MAX);
        assert_eq!(
            resumed.end, straight.end,
            "a resumed run ends as the straight run"
        );
        assert_eq!(resumed.finished, straight.finished, "the same result");
        assert_eq!(resumed.charged, straight.charged, "the same charge");
        resumed_runs += 1;
    }
    assert!(parks_inside > 0, "the run parked inside the callback");
    println!(
        "park\texports_compared\t{exports}\tinside_callback_under_residual\t{parks_inside}\tresumed_over_plain_library\t{resumed_runs}\tcharged\t{}\tresult\t{:?}",
        straight.charged, straight.finished
    );
}

fn micro_programs() -> Vec<(&'static str, String)> {
    vec![
        (
            "micro_get",
            r#"
const o = { alpha: 1, beta: 2, gamma: 3 };
const keys: any[] = ["alpha", "beta", "gamma"];
let total = 0;
for (let i = 0; i < 300; i++) { const key = keys[i % 3]; total = total + o[key]; }
await finish(total);
"#
            .to_string(),
        ),
        (
            "micro_map",
            r#"
const source: number[] = [];
for (let n = 0; n < 300; n++) { source.push(n); }
const out = source.map((item: number) => item);
await finish(out.length);
"#
            .to_string(),
        ),
        (
            "micro_stringify",
            r#"
const img = { type: "image", id: "img-1", media_type: "image/png", label: "chart.png", size: 1234, width: 640, height: 480 };
let total = 0;
for (let i = 0; i < 20; i++) { const s = JSON.stringify(img); total = total + 1; }
await finish(total);
"#
            .to_string(),
        ),
    ]
}

/// The run time of `iterations` runs, start excluded.
fn time(program: &Program, iterations: u32) -> Duration {
    let mut running = Duration::ZERO;
    for _ in 0..iterations {
        let mut machine = start(program);
        let ran = Instant::now();
        drive(&mut machine, u64::MAX);
        running += ran.elapsed();
    }
    running
}

/// Paired timings: plain then specialized, three rounds, the minimum of
/// each. Gives ns per run for each and the charge, asserted equal.
fn pair(plain: &Program, special: &Program, budget: f64) -> (f64, f64, u64) {
    let once = Instant::now();
    let observed = drive(&mut start(plain), u64::MAX);
    let iterations = ((budget / once.elapsed().as_secs_f64().max(1e-6)) as u32).clamp(5, 20_000);
    assert_eq!(
        observed,
        drive(&mut start(special), u64::MAX),
        "the libraries run the program the same way"
    );
    time(plain, iterations.min(50));
    time(special, iterations.min(50));
    // `PE_ROUNDS` rounds (3 by default), alternating which library goes
    // first; the minimum of each.
    let rounds = std::env::var("PE_ROUNDS")
        .ok()
        .map_or(3, |rounds| rounds.parse().expect("a count of rounds"));
    let mut best = (f64::MAX, f64::MAX);
    for round in 0..rounds {
        let once =
            |program: &Program| time(program, iterations).as_nanos() as f64 / f64::from(iterations);
        let (a, b) = if round % 2 == 0 {
            let a = once(plain);
            (a, once(special))
        } else {
            let b = once(special);
            (once(plain), b)
        };
        best.0 = best.0.min(a);
        best.1 = best.1.min(b);
    }
    (best.0, best.1, observed.charged)
}

/// Paired timings of two programs that give the same result, alternate
/// rounds, the minimum of each.
fn pair_runs(first: &Program, second: &Program, budget: f64) -> (f64, f64, u64) {
    let once = Instant::now();
    let observed = drive(&mut start(first), u64::MAX);
    let iterations = ((budget / once.elapsed().as_secs_f64().max(1e-6)) as u32).clamp(5, 20_000);
    time(first, iterations.min(50));
    time(second, iterations.min(50));
    let rounds = std::env::var("PE_ROUNDS")
        .ok()
        .map_or(3, |rounds| rounds.parse().expect("a count of rounds"));
    let mut best = (f64::MAX, f64::MAX);
    for round in 0..rounds {
        let once =
            |program: &Program| time(program, iterations).as_nanos() as f64 / f64::from(iterations);
        let (a, b) = if round % 2 == 0 {
            let a = once(first);
            (a, once(second))
        } else {
            let b = once(second);
            (once(first), b)
        };
        best.0 = best.0.min(a);
        best.1 = best.1.min(b);
    }
    (best.0, best.1, observed.charged)
}

fn paired(setup: &Setup, rest: &[String]) {
    let budget = std::env::var("PE_BUDGET")
        .ok()
        .map_or(0.4, |budget| budget.parse().expect("seconds"));
    println!("program\tplain_ns\tspecialized_ns\tspecialized/plain\tcharged");
    let mut logs = Vec::new();
    for (name, source) in selected(rest) {
        let Some((plain, special)) = setup.programs(&source) else {
            println!("{name}\trefused");
            continue;
        };
        let (a, b, charged) = pair(&plain, &special, budget);
        let ratio = b / a;
        logs.push(ratio.ln());
        println!("{name}\t{a:.1}\t{b:.1}\t{ratio:.4}\t{charged}");
    }
    let geomean = (logs.iter().sum::<f64>() / logs.len().max(1) as f64).exp();
    println!("geomean\t\t\t{geomean:.4}\t{} programs", logs.len());
}

/// A kernel-text document over the registry, using `uses`.
fn kernel_programs(setup: &Setup, uses: &[&str], main: &str) -> (Program, Program) {
    let mut text = String::from("kernel 1\nnumbers by_spelling\n");
    for name in uses {
        let id = setup
            .registry
            .iter()
            .find(|(_, function)| function.definition.name.to_string() == *name)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no function {name}"));
        text.push_str(&format!("use {name} = @{id}\n"));
    }
    text.push_str(main);
    let document = Arc::new(
        lash_kernel_doc::parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}")),
    );
    (
        Program {
            document: Arc::clone(&document),
            library: setup.plain.clone(),
        },
        Program {
            document,
            library: setup.special.clone(),
        },
    )
}

/// Each hot helper's cost per call, called straight from kernel code:
/// each document calls one helper `N` times and its twin runs the same
/// loop without the call.
fn kmicro(setup: &Setup, rest: &[String]) {
    let uses = [
        "ts.get",
        "ts.array.map",
        "ts.json.stringify",
        "ts.json.stringify_generic",
        "num.add",
        "num.lt",
        "list.len",
    ];
    let rows: [(&str, f64, String, String); 5] = [
        (
            "ts.get (plain record, text key)",
            400.0,
            "main {\n let o = {alpha: 1.0, beta: 2.0, gamma: 3.0}\n let i = 0\n while num.lt(i, 400) {\n  let v = invoke ts.get(o, \"beta\")\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
            "main {\n let o = {alpha: 1.0, beta: 2.0, gamma: 3.0}\n let i = 0\n while num.lt(i, 400) {\n  let v = o.beta\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
        ),
        (
            "ts.get (list, number key)",
            400.0,
            "main {\n let o = [1.0, 2.0, 3.0]\n let i = 0\n while num.lt(i, 400) {\n  let v = invoke ts.get(o, 1.0)\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
            "main {\n let o = [1.0, 2.0, 3.0]\n let i = 0\n while num.lt(i, 400) {\n  let v = o[1]\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
        ),
        (
            "ts.array.map (list-direct, per element)",
            400.0,
            "main {\n let xs = []\n let i = 0\n while num.lt(i, 400) {\n  set xs[list.len(xs)] = i\n  set i = num.add(i, 1)\n }\n let f = fn(this, args) { return args[0] }\n let args = [f]\n let out = invoke ts.array.map(xs, args)\n return list.len(out)\n}\n".to_string(),
            "main {\n let xs = []\n let i = 0\n while num.lt(i, 400) {\n  set xs[list.len(xs)] = i\n  set i = num.add(i, 1)\n }\n let f = fn(this, args) { return args[0] }\n let args = [f]\n let out = xs\n return list.len(out)\n}\n".to_string(),
        ),
        (
            "ts.json.stringify_generic serialize closure (7-field record)",
            20.0,
            "main {\n let o = {type: \"image\", id: \"img-1\", media_type: \"image/png\", label: \"chart.png\", size: 1234.0, width: 640.0, height: 480.0}\n let i = 0\n while num.lt(i, 20) {\n  let args = [o]\n  let s = invoke ts.json.stringify_generic(null, args)\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
            "main {\n let o = {type: \"image\", id: \"img-1\", media_type: \"image/png\", label: \"chart.png\", size: 1234.0, width: 640.0, height: 480.0}\n let i = 0\n while num.lt(i, 20) {\n  let args = [o]\n  let s = o\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
        ),
        (
            "ts.json.stringify wrapper, plain path (7-field record)",
            20.0,
            "main {\n let o = {type: \"image\", id: \"img-1\", media_type: \"image/png\", label: \"chart.png\", size: 1234.0, width: 640.0, height: 480.0}\n let i = 0\n while num.lt(i, 20) {\n  let args = [o]\n  let s = invoke ts.json.stringify(null, args)\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
            "main {\n let o = {type: \"image\", id: \"img-1\", media_type: \"image/png\", label: \"chart.png\", size: 1234.0, width: 640.0, height: 480.0}\n let i = 0\n while num.lt(i, 20) {\n  let args = [o]\n  let s = o\n  set i = num.add(i, 1)\n }\n return i\n}\n".to_string(),
        ),
    ];
    println!(
        "helper\tcalls\tplain_ns_per_call\tspecialized_ns_per_call\tspecialized/plain\tcharge_per_call\tprogram_plain_ns\tprogram_specialized_ns\tbase_plain_ns\tbase_specialized_ns"
    );
    // `pe-kmicro loop <row> <plain|special>` runs one row for twenty
    // seconds, for a profiler.
    if rest.first().is_some_and(|word| word == "loop") {
        let row: usize = rest[1].parse().expect("a row");
        let (plain, special) = kernel_programs(setup, &uses, &rows[row].2);
        let program = if rest[2] == "special" { special } else { plain };
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(20) {
            drive(&mut start(&program), u64::MAX);
        }
        return;
    }
    for (label, calls, text, base) in rows {
        let (plain, special) = kernel_programs(setup, &uses, &text);
        let (base_plain, base_special) = kernel_programs(setup, &uses, &base);
        let observed = drive(&mut start(&plain), u64::MAX);
        assert!(
            observed.end.starts_with("Finished"),
            "{label}: {}",
            observed.end
        );
        let base_observed = drive(&mut start(&base_plain), u64::MAX);
        let (a, b, charged) = pair(&plain, &special, 0.6);
        let (base_a, base_b, _) = pair(&base_plain, &base_special, 0.6);
        let per_a = (a - base_a) / calls;
        let per_b = (b - base_b) / calls;
        println!(
            "{label}\t{calls}\t{per_a:.1}\t{per_b:.1}\t{:.4}\t{:.1}\t{a:.1}\t{b:.1}\t{base_a:.1}\t{base_b:.1}",
            per_b / per_a,
            (charged - base_observed.charged) as f64 / calls,
        );
    }
}

const JSON_FLAT: &str = r#"
const rows = [];
for (let i = 0; i < 40; i++) { rows.push({ id: `row-${i}`, index: i, score: i / 4, active: i % 2 === 0, label: "alpha beta", note: null }); }
let total = 0;
for (const row of rows) { total = total + JSON.stringify(row).length; }
await finish({ total: total, first: JSON.stringify(rows[0]) });
"#;

const JSON_NESTED: &str = r#"
const rows = [];
for (let i = 0; i < 40; i++) { rows.push({ id: `row-${i}`, index: i, tags: ["alpha", "beta", "gamma"], meta: { source: "bench", rank: i, ok: true }, scores: [i, i / 2, -i] }); }
let total = 0;
for (const row of rows) { total = total + JSON.stringify(row).length; }
await finish({ total: total, first: JSON.stringify(rows[0]) });
"#;

const JSON_REJECTED: &str = r#"
const rows = [];
for (let i = 0; i < 40; i++) { rows.push({ id: `row-${i}`, index: i, skipped: undefined, tags: ["alpha", "beta", "gamma"], meta: { source: "bench", rank: i, ok: true } }); }
let total = 0;
for (const row of rows) { total = total + JSON.stringify(row).length; }
await finish({ total: total, first: JSON.stringify(rows[0]) });
"#;

/// The compile-time plain-data case: each program lowered with the
/// proof off (every `JSON.stringify` runs the wrapper and its guard) and
/// on (a proven call is the native pass alone), over the plain library,
/// paired. Both give the same result; the charge drops by the guards'.
fn json(setup: &Setup, rest: &[String]) {
    let mut chosen: Vec<(String, String)> = vec![
        ("json_flat".to_string(), JSON_FLAT.to_string()),
        ("json_nested".to_string(), JSON_NESTED.to_string()),
        ("json_rejected".to_string(), JSON_REJECTED.to_string()),
    ];
    chosen.extend(selected(&[
        "image_host_environment".to_string(),
        "language_host_environment".to_string(),
        "micro_stringify".to_string(),
    ]));
    chosen.retain(|(name, _)| rest.is_empty() || rest.contains(name));
    println!(
        "program\tstringify_calls\tproven_calls\tguarded_ns\tproven_ns\tproven/guarded\tguarded_charge\tproven_charge"
    );
    for (name, source) in chosen {
        let (calls, proven, _) =
            lash_dialect_typescript::plain_json_census(&source, &BTreeSet::new())
                .expect("the program parses");
        lash_dialect_typescript::set_plain_json_proof(false);
        let guarded = setup.programs(&source).expect("the program lowers").0;
        lash_dialect_typescript::set_plain_json_proof(true);
        let direct = setup.programs(&source).expect("the program lowers").0;
        let a = drive(&mut start(&guarded), u64::MAX);
        let b = drive(&mut start(&direct), u64::MAX);
        // The guard allocates its own work lists, so the objects made
        // after it are numbered differently: compare with object ids
        // renumbered by first appearance.
        assert_eq!(
            (renumbered(&a.end), &a.finished, a.printed),
            (renumbered(&b.end), &b.finished, b.printed),
            "{name}: the proven lowering gives what the guarded one gives\n{a:?}\n{b:?}"
        );
        let (guarded_ns, direct_ns, _) = pair_runs(&guarded, &direct, 0.6);
        println!(
            "{name}\t{calls}\t{proven}\t{guarded_ns:.1}\t{direct_ns:.1}\t{:.4}\t{}\t{}",
            direct_ns / guarded_ns,
            a.charged,
            b.charged
        );
    }
}

/// `text` with each `ObjectId(n)` renumbered by first appearance.
fn renumbered(text: &str) -> String {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("ObjectId(") {
        let (before, after) = rest.split_at(at + "ObjectId(".len());
        out.push_str(before);
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let next = seen.len();
        let number = *seen.entry(digits.clone()).or_insert(next);
        out.push_str(&number.to_string());
        rest = &after[digits.len()..];
    }
    out.push_str(rest);
    out
}

/// Reads a JSON list of TypeScript sources on stdin and counts their
/// `JSON.stringify` calls and those the plain-data proof covers. Prints
/// counts only.
fn census() {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).expect("stdin");
    let sources: Vec<String> = serde_json::from_str(&input).expect("a JSON list of sources");
    let (mut parsed, mut failed, mut with_calls, mut with_proven, mut calls, mut proven) =
        (0, 0, 0, 0, 0, 0);
    let mut forms: BTreeMap<String, usize> = BTreeMap::new();
    for source in &sources {
        match lash_dialect_typescript::plain_json_census(source, &BTreeSet::new()) {
            Ok((c, p, seen)) => {
                for form in seen {
                    *forms.entry(form).or_default() += 1;
                }
                parsed += 1;
                calls += c;
                proven += p;
                with_calls += usize::from(c > 0);
                with_proven += usize::from(p > 0);
            }
            Err(_) => failed += 1,
        }
    }
    println!(
        "sources\t{}\tparsed\t{parsed}\tunparsed\t{failed}\tcells_calling_stringify\t{with_calls}\tcells_with_a_proven_call\t{with_proven}\tstringify_calls\t{calls}\tproven_calls\t{proven}",
        sources.len()
    );
    let mut forms: Vec<_> = forms.into_iter().collect();
    forms.sort_by_key(|form| std::cmp::Reverse(form.1));
    for (form, count) in forms {
        println!("form\t{count}\t{form}");
    }
}

/// Runs `program` to its end under `bounds`.
fn bounded(program: &Program, bounds: lash_kernel_vm::Bounds) -> Observed {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    match KernelMachine::start(program.clone(), bounds, start) {
        Ok(mut machine) => drive(&mut machine, u64::MAX),
        // A bound the start passes is the same refusal for both.
        Err(error) => Observed {
            end: format!("start refused: {error}"),
            finished: None,
            charged: 0,
            memory: 0,
            printed: 0,
        },
    }
}

/// Charge and memory bounds swept across each program: every charge
/// bound up to 2,000 and about 200 more up to the run's charge, and
/// about 200 memory bounds up to four times its final memory. Both
/// libraries end the same way, with the same meters, at every one.
fn bounds(setup: &Setup, rest: &[String]) {
    let mut runs = 0u64;
    let mut crossed = 0u64;
    for (name, source) in selected(rest) {
        let Some((plain, special)) = setup.programs(&source) else {
            continue;
        };
        let whole = bounded(&plain, BOUNDS);
        let mut charges: BTreeSet<u64> = (0..=whole.charged.min(2000)).collect();
        let mut memories: BTreeSet<u64> = BTreeSet::new();
        for step in 0..=200u32 {
            let fraction = f64::from(step) / 200.0;
            charges.insert((whole.charged as f64).powf(fraction) as u64);
            memories.insert((4.0 * whole.memory.max(1) as f64).powf(fraction) as u64);
        }
        let mut program_crossed = 0;
        for charge in &charges {
            let bounds = lash_kernel_vm::Bounds {
                charge: *charge,
                ..BOUNDS
            };
            let (a, b) = (bounded(&plain, bounds), bounded(&special, bounds));
            assert_eq!(a, b, "{name}: charge bound {charge}");
            program_crossed += u64::from(!a.end.starts_with("Finished"));
            runs += 1;
        }
        for memory in &memories {
            let bounds = lash_kernel_vm::Bounds {
                memory: *memory,
                ..BOUNDS
            };
            let (a, b) = (bounded(&plain, bounds), bounded(&special, bounds));
            assert_eq!(a, b, "{name}: memory bound {memory}");
            program_crossed += u64::from(!a.end.starts_with("Finished"));
            runs += 1;
        }
        crossed += program_crossed;
        println!(
            "{name}\tsame\tcharge_bounds\t{}\tmemory_bounds\t{}\truns_ending_at_a_bound\t{program_crossed}",
            charges.len(),
            memories.len()
        );
    }
    let (crossings, refusals) = KernelMachine::folded_bound_events();
    println!(
        "bounded_runs\t{runs}\tending_at_a_bound\t{crossed}\tfolded_charge_crossings\t{crossings}\tfolded_pin_refusals\t{refusals}"
    );
}

fn hits(setup: &Setup, rest: &[String]) {
    for (name, source) in selected(rest) {
        let Some((_, special)) = setup.programs(&source) else {
            continue;
        };
        drive(&mut start(&special), u64::MAX);
        let _ = name;
    }
    println!("code\tguards\tactivations");
    for (code, guards, hits) in setup.special.pe_hits() {
        println!("{code}\t{guards}\t{hits}");
    }
}
