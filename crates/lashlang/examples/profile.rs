mod bench_support;

use bench_support::{
    BenchHost, Scenario, benchmark_program, linked_benchmark_program, projected_bindings,
    seeded_state_for,
};
use lashlang::{
    ExecutionEnvironment, ExecutionOutcome, ExecutionScratch, ProfileReport, compile_linked,
    execute,
};
use std::env;

fn main() {
    let mut args = env::args().skip(1);
    if matches!(args.next().as_deref(), Some("--list-scenarios")) {
        for scenario in Scenario::ALL {
            println!("{scenario}");
        }
        return;
    }
    let mut args = env::args().skip(1);
    let first = args.next();
    let second = args.next();

    let (scenarios, iterations) = match (first.as_deref(), second.as_deref()) {
        (Some("all"), maybe_iterations) => (
            Scenario::ALL.to_vec(),
            maybe_iterations
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(10_000),
        ),
        (Some(value), maybe_iterations) if Scenario::parse(value).is_some() => (
            vec![Scenario::parse(value).expect("scenario was checked")],
            maybe_iterations
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(10_000),
        ),
        (Some(iterations), None) => (
            vec![Scenario::Baseline],
            iterations.parse::<usize>().ok().unwrap_or(10_000),
        ),
        _ => (vec![Scenario::Baseline], 10_000),
    };

    let host = BenchHost;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut profile = ProfileReport::default();
    let mut scratch = ExecutionScratch::new();
    let mut program_bytes = 0usize;

    for scenario in &scenarios {
        let source = benchmark_program(*scenario);
        program_bytes += source.len();
        let linked = linked_benchmark_program(*scenario, source.as_str());
        let compiled = compile_linked(&linked);

        for _ in 0..iterations {
            let mut state = seeded_state_for(*scenario);
            let env = ExecutionEnvironment::new(&host)
                .profiled()
                .with_scratch(std::mem::take(&mut scratch))
                .with_projected_bindings(projected_bindings(*scenario));
            let outcome = rt
                .block_on(execute(&compiled, &mut state, &env))
                .expect("profiled execution");
            scratch = env.take_recycled_scratch().unwrap_or_default();
            let run_profile = env.take_profile().expect("profile should be recorded");
            profile.merge(&run_profile);
            let ExecutionOutcome::Finished(_) = outcome else {
                panic!("benchmark program must finish");
            };
        }
    }

    println!("lashlang profile");
    if scenarios.len() == 1 {
        println!("scenario: {}", scenarios[0]);
    } else {
        println!(
            "scenario: all ({})",
            scenarios
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("iterations: {iterations}");
    println!("program_bytes: {program_bytes}");
    let compile_stats = profile.compile_stats();
    println!(
        "compile_type_literals_total: {}",
        compile_stats.type_literals_total
    );
    println!(
        "compile_type_literals_const_folded: {}",
        compile_stats.type_literals_const_folded
    );
    println!(
        "compile_type_literals_dynamic: {}",
        compile_stats.type_literals_dynamic
    );
    println!("compile_type_ref_sites: {}", compile_stats.type_ref_sites);
    println!();
    println!("instruction_hotspots:");
    for stat in profile.instruction_stats().iter().take(12) {
        println!(
            "{:<16} count={:<10} total_ms={:<10.3} avg_ns={}",
            stat.name,
            stat.count,
            stat.total_ns as f64 / 1_000_000.0,
            stat.avg_ns()
        );
    }
    println!();
    println!("builtin_hotspots:");
    for stat in profile.builtin_stats().iter().take(12) {
        println!(
            "{:<16} count={:<10} total_ms={:<10.3} avg_ns={}",
            stat.name,
            stat.count,
            stat.total_ns as f64 / 1_000_000.0,
            stat.avg_ns()
        );
    }
}
