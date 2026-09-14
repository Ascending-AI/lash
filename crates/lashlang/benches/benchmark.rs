#![expect(clippy::expect_used, clippy::unwrap_used, reason = "FIG-2784 pass 2")]

#[path = "../examples/bench_support/mod.rs"]
mod bench_support;

use bench_support::builders as b;
use bench_support::{
    BenchHost, Scenario, benchmark_host_environment, benchmark_main, benchmark_program,
    linked_benchmark_program, projected_bindings, seeded_state_for,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lashlang::{
    ExecutionEnvironment, ExecutionOutcome, Expr, LinkedModule, Program, Snapshot, State, Value,
    compile_linked, execute, prewarm,
};
use std::hint::black_box;
use std::time::Duration;

fn lashlang_benchmarks(c: &mut Criterion) {
    let host = BenchHost;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut group = c.benchmark_group("lashlang");
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(60);

    for scenario in Scenario::ALL {
        benchmark_one_shot_modes(&mut group, &rt, &host, *scenario);
    }

    group.finish();
}

fn benchmark_one_shot_modes(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    rt: &tokio::runtime::Runtime,
    host: &BenchHost,
    scenario: Scenario,
) {
    let linked = linked_benchmark_program(scenario);
    let compiled = compile_linked(&linked);
    let projected = projected_bindings(scenario);

    group.bench_function(BenchmarkId::new("one_shot", scenario), |b| {
        b.iter(|| {
            let mut state = seeded_state_for(scenario);
            let linked = linked_benchmark_program(black_box(scenario));
            let compiled = compile_linked(&linked);
            let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
            let outcome = rt
                .block_on(execute(&compiled, &mut state, &env))
                .expect("benchmark execution");
            black_box(expect_finished(outcome));
        });
    });

    group.bench_function(BenchmarkId::new("prewarmed_one_shot", scenario), |b| {
        prewarm();
        b.iter(|| {
            let mut state = seeded_state_for(scenario);
            let linked = linked_benchmark_program(black_box(scenario));
            let compiled = compile_linked(&linked);
            let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
            let outcome = rt
                .block_on(execute(&compiled, &mut state, &env))
                .expect("benchmark execution");
            black_box(expect_finished(outcome));
        });
    });

    group.bench_function(BenchmarkId::new("compiled_execute", scenario), |b| {
        b.iter(|| {
            let mut state = seeded_state_for(scenario);
            let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
            let outcome = rt
                .block_on(execute(black_box(&compiled), &mut state, &env))
                .expect("benchmark execution");
            black_box(expect_finished(outcome));
        });
    });

    group.bench_function(BenchmarkId::new("snapshot", scenario), |b| {
        b.iter(|| {
            let mut state = seeded_state_for(scenario);
            let snapshot = state.snapshot();
            let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
            let decoded = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
            state = State::from_snapshot(decoded);
            let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
            let outcome = rt
                .block_on(execute(black_box(&compiled), &mut state, &env))
                .expect("benchmark execution");
            black_box(expect_finished(outcome));
        });
    });
}

fn lashlang_m9_benchmarks(c: &mut Criterion) {
    let host = BenchHost;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let scenario = Scenario::ToolControlHostEnvironment;
    let projected = projected_bindings(scenario);
    let production = benchmark_program(scenario);
    let live_state = m9_live_state_program();

    let mut group = c.benchmark_group("lashlang_m9/vm_execute");
    group.measurement_time(Duration::from_secs(1));
    group.sample_size(10);

    for (mode, program) in [
        ("production_rlm", production),
        ("production_rlm_live_state", live_state),
    ] {
        let linked = LinkedModule::link(program, benchmark_host_environment())
            .expect("M9 benchmark program should link");
        let compiled = compile_linked(&linked);
        group.bench_function(BenchmarkId::new("vm_attribution", mode), |b| {
            b.iter(|| {
                let mut state = seeded_state_for(scenario);
                let env =
                    ExecutionEnvironment::new(&host).with_projected_bindings(projected.clone());
                let outcome = rt
                    .block_on(execute(black_box(&compiled), &mut state, &env))
                    .expect("M9 benchmark execution");
                black_box(expect_finished(outcome));
            });
        });
    }

    group.finish();
}

/// The production scenario with eight live scalars and four compound values
/// bound ahead of its first process start, so the VM's attribution work is
/// measured against a wider live set.
fn m9_live_state_program() -> Program {
    let mut live_state: Vec<Expr> = (0..8)
        .map(|index| b::assign(&format!("live_scalar_{index}"), b::num(f64::from(index))))
        .collect();
    for pair in 0..4 {
        live_state.push(b::assign(
            &format!("live_compound_{pair}"),
            b::record(vec![
                ("value", b::var(&format!("live_scalar_{}", pair * 2))),
                (
                    "next",
                    b::record(vec![(
                        "value",
                        b::var(&format!("live_scalar_{}", pair * 2 + 1)),
                    )]),
                ),
            ]),
        ));
    }
    live_state.push(b::assign(
        "live_stack",
        b::list(
            (0..4)
                .map(|pair| b::var(&format!("live_compound_{pair}")))
                .collect(),
        ),
    ));

    let scenario = Scenario::ToolControlHostEnvironment;
    live_state.extend(benchmark_main(scenario));
    let mut program = benchmark_program(scenario);
    program.main = b::block(live_state);
    program
}

fn expect_finished(outcome: ExecutionOutcome) -> Value {
    match outcome {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => panic!("benchmark program must finish"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

criterion_group!(benches, lashlang_benchmarks, lashlang_m9_benchmarks);
criterion_main!(benches);
