// Compare the in-process TypeScript VM with the workloads in
// https://github.com/pydantic/monty/blob/main/scripts/startup_performance.py.
// The 10,000-session loop follows https://gist.github.com/samuelcolvin/f48e60a983264c5b4d6132978f59be1e.
// This measures VM states, not security isolation or subprocess startup.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::expect_used)]

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};
use std::collections::BTreeSet;
use std::env;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SIMPLE: &str = "finish(1 + 1);";
const EXPECTED_REPORT: &str = "3 orders, total 21.6, biggest B2";

// Translated from Monty's AGENT_BLOCKS. Lash drops function bindings at cell
// boundaries, so block 2 evaluates the helper immediately and blocks 3 and 5
// use local callbacks. The orders, arithmetic, JSON, and report are the same.
const AGENT_BLOCKS: [&str; 10] = [
    "const orders = [{sku: 'A1', qty: 2, price: 3.5}, {sku: 'B2', qty: 1, price: 12.0}, {sku: 'C3', qty: 5, price: 1.0}];",
    "const lineTotal = (o: {qty: number, price: number}): number => o.qty * o.price; const firstLineTotal = lineTotal(orders[0]);",
    "const totals = [firstLineTotal, ...orders.slice(1).map(o => o.qty * o.price)];",
    "const grand = totals.reduce((sum, value) => sum + value, 0);",
    "const biggest = orders.reduce((best, o) => o.qty * o.price > best.qty * best.price ? o : best, orders[0]).sku;",
    "const summary = JSON.stringify({grand, biggest});",
    "const discount = grand > 10 ? 0.1 : 0;",
    "const net = Math.round(grand * (1 - discount) * 100) / 100;",
    "const report = `${orders.length} orders, total ${net}, biggest ${biggest}`;",
    "console.log(report);",
];

#[derive(Default)]
struct Host(Mutex<Option<String>>);

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(Value::String(value)) => {
                *self.0.lock().expect("print lock") = Some(value.to_string());
                Ok(AbilityResult::Unit)
            }
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability: {other:?}"
            ))),
        }
    }
}

fn compile_cell(source: &str, state: &State) -> lashlang::CompiledProgram {
    let globals: BTreeSet<String> = state.binding_names().map(str::to_owned).collect();
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("parse `{source}`: {error}"));
    let spans = program.spans.clone();
    let artifact = lashlang::ModuleArtifact::from_program(program)
        .unwrap_or_else(|error| panic!("artifact `{source}`: {error}"));
    lashlang::compile(&artifact, lashlang::Entry::Main, Some(&spans))
        .unwrap_or_else(|error| panic!("compile `{source}`: {error}"))
}

fn run_cell(source: &str, state: &mut State, host: &Host) -> ExecutionOutcome {
    let compiled = compile_cell(source, state);
    futures::executor::block_on(lashlang::execute(&compiled, state, host))
        .unwrap_or_else(|error| panic!("execute `{source}`: {error}"))
}

fn simple_session(host: &Host) {
    let outcome = run_cell(SIMPLE, &mut State::new(), host);
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(2.0)));
}

fn agent_session(host: &Host) {
    *host.0.lock().expect("print lock") = None;
    let mut state = State::new();
    for source in AGENT_BLOCKS {
        let _ = run_cell(source, &mut state, host);
    }
    assert_eq!(
        host.0.lock().expect("print lock").as_deref(),
        Some(EXPECTED_REPORT)
    );
}

fn median_ms(samples: &mut [Duration]) -> f64 {
    samples.sort();
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        (samples[middle - 1].as_secs_f64() + samples[middle].as_secs_f64()) / 2.0
    } else {
        samples[middle].as_secs_f64()
    };
    median * 1000.0
}

fn option(args: &[String], name: &str, default: usize) -> usize {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].parse().unwrap_or_else(|_| panic!("invalid {name}")))
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let iterations = option(&args, "--iterations", 10_000);
    let rounds = option(&args, "--rounds", 20);
    let idle_ms = option(&args, "--idle-ms", 1_000);
    assert!(iterations > 0 && rounds > 0);
    let host = Host::default();

    simple_session(&host);
    agent_session(&host);

    let mut fresh = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        std::thread::sleep(Duration::from_millis(idle_ms as u64));
        let started = Instant::now();
        simple_session(&host);
        fresh.push(started.elapsed());
    }

    let mut agent = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        agent_session(&host);
        agent.push(started.elapsed());
    }

    let started = Instant::now();
    for _ in 0..iterations {
        simple_session(&host);
    }
    let batch_ms = started.elapsed().as_secs_f64() * 1000.0;

    println!("Lash TypeScript VM, in-process; parse + compile + execute per cell");
    println!(
        "fresh state + 1+1: {:.3} ms median of {rounds}, idle {idle_ms} ms",
        median_ms(&mut fresh)
    );
    println!(
        "10 feeds in one session: {:.3} ms median of {rounds}",
        median_ms(&mut agent)
    );
    println!(
        "{iterations} fresh states + 1+1: {batch_ms:.3} ms ({:.3} ms/script)",
        batch_ms / iterations as f64
    );
}
