// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

mod bench_support;

use bench_support::{
    BenchHost, Scenario, benchmark_host_environment, benchmark_main, benchmark_program,
    linked_benchmark_program, projected_bindings, seeded_state_for,
};
use lashlang::{
    CompiledProcessCache, ExecutionEnvironment, ExecutionOutcome, ExecutionScratch, LinkedModule,
    LinkedProgramCache, ProjectedBindings, Snapshot, State, execute, prewarm,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static DEALLOCATIONS: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

#[expect(
    unsafe_code,
    reason = "the allocation-accounting harness installs a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        record_dealloc(layout.size() as u64);
    }

    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, old_layout, new_size) };
        if ptr.is_null() {
            return ptr;
        }
        let old_size = old_layout.size() as u64;
        let new_size = new_size as u64;
        if new_size > old_size {
            record_alloc(new_size - old_size);
        } else {
            record_dealloc(old_size - new_size);
        }
        ptr
    }
}

fn record_alloc(bytes: u64) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let mut peak = PEAK_LIVE_BYTES.load(Ordering::Relaxed);
    while live > peak {
        match PEAK_LIVE_BYTES.compare_exchange_weak(
            peak,
            live,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => peak = next,
        }
    }
}

fn record_dealloc(bytes: u64) {
    let _ = LIVE_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        Some(live.saturating_sub(bytes))
    });
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    OneShot,
    PrewarmedOneShot,
    LinkArtifact,
    CompiledExecute,
    Snapshot,
    CompiledProcessCache,
    LinkedProgramCache,
    PhaseBreakdown,
}

#[expect(
    clippy::expect_used,
    reason = "benchmark entry point: the fixed-configuration tokio runtime is built per the constants above"
)]
fn main() {
    let mut args = env::args().skip(1);
    if matches!(args.next().as_deref(), Some("--list-scenarios")) {
        for scenario in Scenario::ALL {
            println!("{scenario}");
        }
        return;
    }
    let mut args = env::args().skip(1);
    let mode = args
        .next()
        .as_deref()
        .map(parse_mode)
        .unwrap_or(Mode::OneShot);
    let scenario_arg = args.next();
    let iterations = args
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(match mode {
            Mode::OneShot | Mode::PrewarmedOneShot => 25_000,
            Mode::CompiledExecute | Mode::Snapshot | Mode::CompiledProcessCache => 100_000,
            Mode::LinkArtifact => 25_000,
            Mode::LinkedProgramCache => 25_000,
            Mode::PhaseBreakdown => 10_000,
        });

    let scenarios = parse_scenarios(scenario_arg.as_deref());
    for (index, scenario) in scenarios.iter().copied().enumerate() {
        if index > 0 {
            println!();
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        run_perf(&rt, mode, scenario, iterations);
    }
}

#[expect(
    clippy::expect_used,
    reason = "each mode's step is a checked benchmark fact: snapshot round trip, process export and cache miss compile, per each message"
)]
fn run_perf(rt: &tokio::runtime::Runtime, mode: Mode, scenario: Scenario, iterations: usize) {
    let cache_key = scenario.to_string();
    let projected = projected_bindings(scenario);
    let host = BenchHost;
    let mut scratch = ExecutionScratch::new();
    let mut process_cache_stats = None;
    let mut linked_cache_stats = None;
    let mut phase_breakdown = None;

    reset_alloc_counters();
    let mut started = Instant::now();
    match mode {
        Mode::OneShot => {
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let mut scratch = ExecutionScratch::new();
                let linked = linked_benchmark_program(std::hint::black_box(scenario));
                let compiled = lashlang::compile(
                    &linked.artifact,
                    lashlang::Entry::Main,
                    Some(linked.spans()),
                )
                .expect("a module main entry compiles");
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::PrewarmedOneShot => {
            prewarm();
            reset_alloc_counters();
            started = Instant::now();
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let mut scratch = ExecutionScratch::new();
                let linked = linked_benchmark_program(std::hint::black_box(scenario));
                let compiled = lashlang::compile(
                    &linked.artifact,
                    lashlang::Entry::Main,
                    Some(linked.spans()),
                )
                .expect("a module main entry compiles");
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::LinkArtifact => {
            for _ in 0..iterations {
                let linked = linked_benchmark_program(std::hint::black_box(scenario));
                std::hint::black_box((
                    &linked.artifact.module_ref(),
                    &linked.artifact.host_requirements_ref(),
                ));
            }
        }
        Mode::CompiledExecute => {
            let linked = linked_benchmark_program(scenario);
            let compiled = lashlang::compile(
                &linked.artifact,
                lashlang::Entry::Main,
                Some(linked.spans()),
            )
            .expect("a module main entry compiles");
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::Snapshot => {
            let linked = linked_benchmark_program(scenario);
            let compiled = lashlang::compile(
                &linked.artifact,
                lashlang::Entry::Main,
                Some(linked.spans()),
            )
            .expect("a module main entry compiles");
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let snapshot = state.snapshot();
                let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
                let decoded = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
                state = State::from_snapshot(decoded);
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::CompiledProcessCache => {
            let linked = linked_benchmark_program(scenario);
            let process_ref = linked
                .artifact
                .process_ref("echo")
                .expect("benchmark module should export echo process")
                .clone();
            let mut cache = CompiledProcessCache::new();
            for _ in 0..iterations {
                let compiled = cache
                    .get_or_compile(
                        &linked.artifact,
                        &process_ref,
                        linked.artifact.host_requirements_ref(),
                    )
                    .expect("process cache compile should succeed");
                std::hint::black_box(compiled.compile_stats());
            }
            process_cache_stats = Some(cache.stats());
        }
        Mode::LinkedProgramCache => {
            let mut cache = LinkedProgramCache::new();
            let surface = benchmark_host_environment();
            for _ in 0..iterations {
                let key = std::hint::black_box(cache_key.as_str());
                let compiled = match cache.cached_linked_program(key, surface) {
                    Some(compiled) => compiled,
                    None => cache
                        .get_or_compile_ast(key, benchmark_program(scenario), surface)
                        .expect("linked program cache compile should succeed"),
                };
                std::hint::black_box(compiled.compiled_program().compile_stats());
            }
            linked_cache_stats = Some(cache.stats());
        }
        Mode::PhaseBreakdown => {
            phase_breakdown = Some(run_phase_breakdown(rt, scenario, iterations));
        }
    }
    let elapsed = started.elapsed();
    let allocs = alloc_snapshot();
    let phase_totals = phase_breakdown.as_ref().map(|phases| {
        phases.iter().fold(
            PhaseBreakdownMetric::zero("phase_total"),
            |mut total, phase| {
                total.ns_per_iter += phase.ns_per_iter;
                total.allocations_per_iter += phase.allocations_per_iter;
                total.allocated_bytes_per_iter += phase.allocated_bytes_per_iter;
                total
            },
        )
    });
    let allocations_per_iter = phase_totals
        .as_ref()
        .map(|total| total.allocations_per_iter)
        .unwrap_or(allocs.allocations as f64 / iterations as f64);
    let allocated_bytes_per_iter = phase_totals
        .as_ref()
        .map(|total| total.allocated_bytes_per_iter)
        .unwrap_or(allocs.allocated_bytes as f64 / iterations as f64);
    let allocations = allocations_per_iter * iterations as f64;
    let allocated_bytes = allocated_bytes_per_iter * iterations as f64;

    println!("lashlang perf");
    println!("mode: {mode:?}");
    println!("scenario: {scenario}");
    println!("iterations: {iterations}");
    println!("program_expressions: {}", benchmark_main(scenario).len());
    println!("elapsed_ms: {:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!(
        "ns_per_iter: {:.1}",
        elapsed.as_nanos() as f64 / iterations as f64
    );
    println!("allocations: {:.0}", allocations);
    println!("deallocations: {}", allocs.deallocations);
    println!("allocated_bytes: {:.0}", allocated_bytes);
    println!("allocations_per_iter: {:.3}", allocations_per_iter);
    println!("allocated_bytes_per_iter: {:.1}", allocated_bytes_per_iter);
    println!("peak_live_bytes: {}", allocs.peak_live_bytes);
    if let Some(stats) = process_cache_stats {
        println!("process_cache_hits: {}", stats.hits);
        println!("process_cache_misses: {}", stats.misses);
        println!("process_cache_evictions: {}", stats.evictions);
        println!("process_cache_entries: {}", stats.entries);
    }
    if let Some(stats) = linked_cache_stats {
        println!("linked_cache_hits: {}", stats.hits);
        println!("linked_cache_misses: {}", stats.misses);
        println!("linked_cache_evictions: {}", stats.evictions);
        println!("linked_cache_entries: {}", stats.entries);
    }
    if let Some(phases) = phase_breakdown {
        if let Some(total) = phase_totals {
            println!("{}_ns_per_iter: {:.1}", total.name, total.ns_per_iter);
            println!(
                "{}_allocations_per_iter: {:.3}",
                total.name, total.allocations_per_iter
            );
            println!(
                "{}_allocated_bytes_per_iter: {:.1}",
                total.name, total.allocated_bytes_per_iter
            );
        }
        for phase in phases {
            println!("{}_ns_per_iter: {:.1}", phase.name, phase.ns_per_iter);
            println!(
                "{}_allocations_per_iter: {:.3}",
                phase.name, phase.allocations_per_iter
            );
            println!(
                "{}_allocated_bytes_per_iter: {:.1}",
                phase.name, phase.allocated_bytes_per_iter
            );
        }
    }
}

struct PhaseBreakdownMetric {
    name: &'static str,
    ns_per_iter: f64,
    allocations_per_iter: f64,
    allocated_bytes_per_iter: f64,
}

impl PhaseBreakdownMetric {
    fn zero(name: &'static str) -> Self {
        Self {
            name,
            ns_per_iter: 0.0,
            allocations_per_iter: 0.0,
            allocated_bytes_per_iter: 0.0,
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "the benchmark module links against the fixture host environment twice for the phase breakdown"
)]
fn run_phase_breakdown(
    rt: &tokio::runtime::Runtime,
    scenario: Scenario,
    iterations: usize,
) -> Vec<PhaseBreakdownMetric> {
    let parsed = benchmark_program(scenario);
    let linked = LinkedModule::link(parsed.clone(), benchmark_host_environment())
        .expect("benchmark program should link");
    let compiled = lashlang::compile(
        &linked.artifact,
        lashlang::Entry::Main,
        Some(linked.spans()),
    )
    .expect("a module main entry compiles");
    let projected = projected_bindings(scenario);
    let host = BenchHost;
    let mut scratch = ExecutionScratch::new();

    let build = measure_phase("build", iterations, || {
        let built = benchmark_program(std::hint::black_box(scenario));
        std::hint::black_box(built);
    });
    let link = measure_phase("link", iterations, || {
        let linked = LinkedModule::link(
            std::hint::black_box(parsed.clone()),
            benchmark_host_environment(),
        )
        .expect("benchmark program should link");
        std::hint::black_box(linked.artifact.module_ref());
    });
    let compile = measure_phase("compile", iterations, || {
        let compiled = lashlang::compile(
            &linked.artifact,
            lashlang::Entry::Main,
            Some(linked.spans()),
        )
        .expect("a module main entry compiles");
        std::hint::black_box(compiled.compile_stats());
    });
    let execute = measure_phase("execute", iterations, || {
        let mut state = seeded_state_for(scenario);
        let outcome = execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
        expect_finished(outcome);
    });

    vec![build, link, compile, execute]
}

fn measure_phase(
    name: &'static str,
    iterations: usize,
    mut run: impl FnMut(),
) -> PhaseBreakdownMetric {
    reset_alloc_counters();
    let started = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let elapsed = started.elapsed();
    let allocs = alloc_snapshot();
    PhaseBreakdownMetric {
        name,
        ns_per_iter: elapsed.as_nanos() as f64 / iterations as f64,
        allocations_per_iter: allocs.allocations as f64 / iterations as f64,
        allocated_bytes_per_iter: allocs.allocated_bytes as f64 / iterations as f64,
    }
}

#[expect(
    clippy::expect_used,
    reason = "the benchmark's canonical fixture executes to completion, per the execute below"
)]
fn execute_benchmark(
    rt: &tokio::runtime::Runtime,
    compiled: &lashlang::CompiledProgram,
    state: &mut State,
    host: &BenchHost,
    scratch: &mut ExecutionScratch,
    projected: &ProjectedBindings,
) -> ExecutionOutcome {
    let env = ExecutionEnvironment::new(host)
        .with_scratch(std::mem::take(scratch))
        .with_projected_bindings(projected.clone());
    let outcome = rt
        .block_on(execute(compiled, state, &env))
        .expect("benchmark execution should succeed");
    *scratch = env.take_recycled_scratch().unwrap_or_default();
    outcome
}

fn parse_scenarios(value: Option<&str>) -> Vec<Scenario> {
    match value {
        Some("all") => Scenario::ALL.to_vec(),
        Some(value) => vec![Scenario::parse(value).unwrap_or_else(|| {
            panic!(
                "unknown scenario `{value}`; expected {}",
                Scenario::expected_values()
            )
        })],
        None => vec![Scenario::Baseline],
    }
}

fn parse_mode(value: &str) -> Mode {
    match value {
        "one_shot" => Mode::OneShot,
        "prewarmed_one_shot" => Mode::PrewarmedOneShot,
        "link_artifact" => Mode::LinkArtifact,
        "compiled_execute" => Mode::CompiledExecute,
        "snapshot" => Mode::Snapshot,
        "compiled_process_cache" => Mode::CompiledProcessCache,
        "linked_program_cache" => Mode::LinkedProgramCache,
        "phase_breakdown" => Mode::PhaseBreakdown,
        other => panic!(
            "unknown mode `{other}`; expected one_shot, prewarmed_one_shot, link_artifact, compiled_execute, snapshot, compiled_process_cache, linked_program_cache, or phase_breakdown"
        ),
    }
}

fn reset_alloc_counters() {
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    LIVE_BYTES.store(0, Ordering::Relaxed);
    PEAK_LIVE_BYTES.store(0, Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
}

fn alloc_snapshot() -> AllocSnapshot {
    AllocSnapshot {
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        peak_live_bytes: PEAK_LIVE_BYTES.load(Ordering::Relaxed),
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        deallocations: DEALLOCATIONS.load(Ordering::Relaxed),
    }
}

struct AllocSnapshot {
    allocated_bytes: u64,
    peak_live_bytes: u64,
    allocations: u64,
    deallocations: u64,
}

fn expect_finished(outcome: ExecutionOutcome) {
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("benchmark program must finish");
    };
    std::hint::black_box(value);
}
