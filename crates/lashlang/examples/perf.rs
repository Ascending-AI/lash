mod bench_support;

use bench_support::{
    BenchHost, Scenario, benchmark_host_environment, benchmark_program, linked_benchmark_program,
    projected_bindings, seeded_state_for,
};
use lashlang::{
    CompiledProcessCache, CompiledProgramCache, ExecutionEnvironment, ExecutionOutcome,
    ExecutionScratch, InMemoryLashlangArtifactStore, LashlangArtifactStore, LinkedModule,
    LinkedProgramCache, ProjectedBindings, Snapshot, State, compile_linked, execute, prewarm,
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
    ArtifactRoundtrip,
    CompiledProcessCache,
    CompiledProgramCache,
    LinkedProgramCache,
    PhaseBreakdown,
}

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
            Mode::ArtifactRoundtrip => 10_000,
            Mode::CompiledProgramCache | Mode::LinkedProgramCache => 25_000,
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

fn run_perf(rt: &tokio::runtime::Runtime, mode: Mode, scenario: Scenario, iterations: usize) {
    let source = benchmark_program(scenario);
    let projected = projected_bindings(scenario);
    let host = BenchHost;
    let mut scratch = ExecutionScratch::new();
    let mut process_cache_stats = None;
    let mut program_cache_stats = None;
    let mut linked_cache_stats = None;
    let mut artifact_bytes = None;
    let mut phase_breakdown = None;

    reset_alloc_counters();
    let mut started = Instant::now();
    match mode {
        Mode::OneShot => {
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let mut scratch = ExecutionScratch::new();
                let linked = linked_benchmark_program(std::hint::black_box(source.as_str()));
                let compiled = compile_linked(&linked);
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
                let linked = linked_benchmark_program(std::hint::black_box(source.as_str()));
                let compiled = compile_linked(&linked);
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::LinkArtifact => {
            for _ in 0..iterations {
                let linked = linked_benchmark_program(std::hint::black_box(source.as_str()));
                std::hint::black_box((&linked.module_ref, &linked.host_requirements_ref));
            }
        }
        Mode::CompiledExecute => {
            let linked = linked_benchmark_program(source.as_str());
            let compiled = compile_linked(&linked);
            for _ in 0..iterations {
                let mut state = seeded_state_for(scenario);
                let outcome =
                    execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
                expect_finished(outcome);
            }
        }
        Mode::Snapshot => {
            let linked = linked_benchmark_program(source.as_str());
            let compiled = compile_linked(&linked);
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
        Mode::ArtifactRoundtrip => {
            let linked = linked_benchmark_program(source.as_str());
            artifact_bytes = Some(
                linked
                    .artifact
                    .to_store_bytes()
                    .expect("artifact should encode")
                    .len(),
            );
            let store = InMemoryLashlangArtifactStore::new();
            for _ in 0..iterations {
                rt.block_on(store.put_module_artifact(&linked.artifact))
                    .expect("artifact store put should succeed");
                let artifact = rt
                    .block_on(store.get_module_artifact(&linked.module_ref))
                    .expect("artifact store get should succeed")
                    .expect("artifact should exist");
                std::hint::black_box(artifact);
            }
        }
        Mode::CompiledProcessCache => {
            let linked = linked_benchmark_program(source.as_str());
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
                        &linked.host_requirements_ref,
                    )
                    .expect("process cache compile should succeed");
                std::hint::black_box(compiled.compile_stats());
            }
            process_cache_stats = Some(cache.stats());
        }
        Mode::CompiledProgramCache => {
            let mut cache = CompiledProgramCache::new();
            for _ in 0..iterations {
                let compiled = cache
                    .get_or_compile(std::hint::black_box(source.as_str()))
                    .expect("program cache compile should succeed");
                std::hint::black_box(compiled.compile_stats());
            }
            program_cache_stats = Some(cache.stats());
        }
        Mode::LinkedProgramCache => {
            let mut cache = LinkedProgramCache::new();
            let surface = benchmark_host_environment();
            for _ in 0..iterations {
                let compiled = cache
                    .get_or_compile(std::hint::black_box(source.as_str()), surface)
                    .expect("linked program cache compile should succeed");
                std::hint::black_box(compiled.compiled_program().compile_stats());
            }
            linked_cache_stats = Some(cache.stats());
        }
        Mode::PhaseBreakdown => {
            phase_breakdown = Some(run_phase_breakdown(
                rt,
                scenario,
                source.as_str(),
                iterations,
            ));
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
    println!("program_bytes: {}", source.len());
    if let Some(bytes) = artifact_bytes {
        println!("artifact_bytes: {bytes}");
    }
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
    if let Some(stats) = program_cache_stats {
        println!("program_cache_hits: {}", stats.hits);
        println!("program_cache_misses: {}", stats.misses);
        println!("program_cache_evictions: {}", stats.evictions);
        println!("program_cache_entries: {}", stats.entries);
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

fn run_phase_breakdown(
    rt: &tokio::runtime::Runtime,
    scenario: Scenario,
    source: &str,
    iterations: usize,
) -> Vec<PhaseBreakdownMetric> {
    let parsed = lashlang::parse(source).expect("benchmark program should parse");
    let linked = LinkedModule::link(parsed.clone(), benchmark_host_environment())
        .expect("benchmark program should link");
    let compiled = compile_linked(&linked);
    let projected = projected_bindings(scenario);
    let host = BenchHost;
    let mut scratch = ExecutionScratch::new();

    let parse = measure_phase("parse", iterations, || {
        let parsed =
            lashlang::parse(std::hint::black_box(source)).expect("benchmark program should parse");
        std::hint::black_box(parsed);
    });
    let link = measure_phase("link", iterations, || {
        let linked = LinkedModule::link(
            std::hint::black_box(parsed.clone()),
            benchmark_host_environment(),
        )
        .expect("benchmark program should link");
        std::hint::black_box(linked.module_ref);
    });
    let compile = measure_phase("compile", iterations, || {
        let compiled = compile_linked(std::hint::black_box(&linked));
        std::hint::black_box(compiled.compile_stats());
    });
    let execute = measure_phase("execute", iterations, || {
        let mut state = seeded_state_for(scenario);
        let outcome = execute_benchmark(rt, &compiled, &mut state, &host, &mut scratch, &projected);
        expect_finished(outcome);
    });

    vec![parse, link, compile, execute]
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
        "artifact_roundtrip" => Mode::ArtifactRoundtrip,
        "compiled_process_cache" => Mode::CompiledProcessCache,
        "compiled_program_cache" => Mode::CompiledProgramCache,
        "linked_program_cache" => Mode::LinkedProgramCache,
        "phase_breakdown" => Mode::PhaseBreakdown,
        other => panic!(
            "unknown mode `{other}`; expected one_shot, prewarmed_one_shot, link_artifact, compiled_execute, snapshot, artifact_roundtrip, compiled_process_cache, compiled_program_cache, linked_program_cache, or phase_breakdown"
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
