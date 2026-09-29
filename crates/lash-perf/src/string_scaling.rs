//! TypeScript string loops measured after lowering, with only VM execution timed.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, bail};
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, State, Value,
};
use serde::{Deserialize, Serialize};

const SIZES: [usize; 3] = [1_000, 10_000, 100_000];
const TIMED_SAMPLES: usize = 3;
const BUDGETS_JSON: &str = include_str!("../../../scripts/perf_guard_budgets.json");
// The checked-in 1.5 limit allows 50% timing slack on a shared host: 100x
// more iterations may take at most 150x as long. Quadratic growth is far above it.

#[derive(Clone, Copy, Debug)]
enum Workload {
    Concat,
    CharCode,
    Uri,
}

impl Workload {
    const ALL: [Self; 3] = [Self::Concat, Self::CharCode, Self::Uri];

    fn name(self) -> &'static str {
        match self {
            Self::Concat => "concat",
            Self::CharCode => "char_code",
            Self::Uri => "uri",
        }
    }

    fn source(self, iterations: usize) -> String {
        match self {
            Self::Concat => format!(
                "let text = ''; for (let i = 0; i < {iterations}; i++) {{ text += 'x'; }} finish(text.length);"
            ),
            Self::CharCode => format!(
                "const input = 'Az9'; let sum = 0; for (let i = 0; i < {iterations}; i++) {{ \
                 const code = input.charCodeAt(i % input.length); \
                 sum += String.fromCharCode(code).charCodeAt(0); }} finish(sum);"
            ),
            // The varying surrogate pair follows the four-byte URI sweeps in test262.
            Self::Uri => format!(
                "let units = 0; for (let i = 0; i < {iterations}; i++) {{ \
                 const pair = String.fromCharCode(0xD800, 0xDC00 + (i % 64)); \
                 units += decodeURI(encodeURI(pair)).length; }} finish(units);"
            ),
        }
    }

    fn expected(self, iterations: usize) -> f64 {
        match self {
            Self::Concat => iterations as f64,
            Self::CharCode => (0..iterations).map(|i| f64::from(b"Az9"[i % 3])).sum(),
            Self::Uri => (iterations * 2) as f64,
        }
    }
}

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected string benchmark ability: {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Budgets {
    string_scaling: ScalingBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScalingBudget {
    max_normalized_time_ratio: f64,
}

#[derive(Debug, Serialize)]
struct Measurement {
    workload: &'static str,
    iterations: usize,
    cpu_duration_ms: f64,
    cpu_samples_ms: Vec<f64>,
    wall_samples_ms: Vec<f64>,
    cpu_ns_per_iteration: f64,
    // These are profiled VM opcode counts. Builtin work charged to the VM's
    // instruction budget is not represented by the opcode profiler.
    profiled_instructions: u64,
    instructions_per_cpu_second: f64,
}

#[derive(Debug, Serialize)]
struct ScalingResult {
    workload: &'static str,
    normalized_time_ratio: f64,
    max_normalized_time_ratio: f64,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    measurements: Vec<Measurement>,
    scaling: Vec<ScalingResult>,
}

fn compile(source: &str) -> anyhow::Result<lashlang::CompiledProgram> {
    let program = lash_typescript::parse_with_globals(source, &BTreeSet::new())
        .map_err(|error| anyhow::anyhow!("parse TypeScript: {error}"))?;
    let spans = program.spans.clone();
    let artifact = lashlang::ModuleArtifact::from_program(program)
        .map_err(|error| anyhow::anyhow!("create module artifact: {error}"))?;
    lashlang::compile(&artifact, lashlang::Entry::Main, Some(&spans))
        .map_err(|error| anyhow::anyhow!("compile TypeScript: {error}"))
}

async fn execute(
    compiled: &lashlang::CompiledProgram,
    workload: Workload,
    iterations: usize,
    expected: f64,
    profile: bool,
) -> anyhow::Result<u64> {
    let mut state = State::new();
    let host = Host;
    let env = if profile {
        ExecutionEnvironment::new(&host).profiled()
    } else {
        ExecutionEnvironment::new(&host)
    };
    let outcome = lashlang::execute(compiled, &mut state, &env).await?;
    match outcome {
        ExecutionOutcome::Finished(Value::Number(value)) if value == expected => {}
        other => bail!("{} at {iterations} returned {other:?}", workload.name()),
    }
    if profile {
        let report = env.take_profile().context("VM did not emit a profile")?;
        let count = report
            .instruction_stats()
            .iter()
            .map(|stat| stat.count)
            .sum();
        Ok(count)
    } else {
        Ok(0)
    }
}

async fn measure(workload: Workload, iterations: usize) -> anyhow::Result<Measurement> {
    let compiled = compile(&workload.source(iterations))?;
    let expected = workload.expected(iterations);
    // Profile separately so per-op clock reads do not inflate the timed run.
    let profiled_instructions = execute(&compiled, workload, iterations, expected, true).await?;
    if profiled_instructions == 0 {
        bail!(
            "{} at {iterations} profiled zero instructions",
            workload.name()
        );
    }
    // The thread CPU clock excludes time this shared runner deschedules us.
    // Keep wall samples to show host contention and use the least CPU time.
    let mut cpu_samples_ms = Vec::with_capacity(TIMED_SAMPLES);
    let mut wall_samples_ms = Vec::with_capacity(TIMED_SAMPLES);
    for _ in 0..TIMED_SAMPLES {
        let cpu_start = thread_cpu_time_ns()?;
        let start = Instant::now();
        execute(&compiled, workload, iterations, expected, false).await?;
        wall_samples_ms.push(start.elapsed().as_secs_f64() * 1_000.0);
        cpu_samples_ms.push((thread_cpu_time_ns()? - cpu_start) as f64 / 1_000_000.0);
    }
    let cpu_duration_ms = cpu_samples_ms.iter().copied().fold(f64::INFINITY, f64::min);
    let duration = cpu_duration_ms / 1_000.0;
    Ok(Measurement {
        workload: workload.name(),
        iterations,
        cpu_duration_ms,
        cpu_samples_ms,
        wall_samples_ms,
        cpu_ns_per_iteration: duration * 1_000_000_000.0 / iterations as f64,
        profiled_instructions,
        instructions_per_cpu_second: profiled_instructions as f64 / duration,
    })
}

#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "clock_gettime writes to this local timespec")]
fn thread_cpu_time_ns() -> anyhow::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid writable timespec for this call.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(u64::try_from(time.tv_sec)? * 1_000_000_000 + u64::try_from(time.tv_nsec)?)
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_time_ns() -> anyhow::Result<u64> {
    bail!("string scaling requires the Linux thread CPU clock")
}

fn normalized_time_ratio(small: &Measurement, large: &Measurement) -> f64 {
    (large.cpu_duration_ms / small.cpu_duration_ms)
        / (large.iterations as f64 / small.iterations as f64)
}

/// Run all three source workloads, emit a report, and fail on excessive growth.
pub async fn run(out: Option<&Path>) -> anyhow::Result<()> {
    let budget: Budgets = serde_json::from_str(BUDGETS_JSON)?;
    let limit = budget.string_scaling.max_normalized_time_ratio;
    if !limit.is_finite() || limit <= 1.0 {
        bail!("string scaling budget must be finite and greater than one");
    }
    let mut report = Report {
        measurements: Vec::new(),
        scaling: Vec::new(),
    };
    for workload in Workload::ALL {
        let first = report.measurements.len();
        for size in SIZES {
            let measurement = measure(workload, size).await?;
            println!(
                "{} {:>6} iterations: {:>10.1} CPU ns/iteration, {:>12.0} instructions/CPU s ({:>8} profiled instructions)",
                measurement.workload,
                size,
                measurement.cpu_ns_per_iteration,
                measurement.instructions_per_cpu_second,
                measurement.profiled_instructions,
            );
            report.measurements.push(measurement);
        }
        let small = &report.measurements[first];
        let large = report
            .measurements
            .last()
            .context("missing largest measurement")?;
        let ratio = normalized_time_ratio(small, large);
        let passed = ratio <= limit;
        println!(
            "{} scaling: {:.3}x normalized time (limit {:.3}x): {}",
            workload.name(),
            ratio,
            limit,
            if passed { "PASS" } else { "FAIL" }
        );
        report.scaling.push(ScalingResult {
            workload: workload.name(),
            normalized_time_ratio: ratio,
            max_normalized_time_ratio: limit,
            passed,
        });
    }
    if let Some(path) = out {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("write string scaling report to {}", path.display()))?;
    }
    if report.scaling.iter().any(|result| !result.passed) {
        bail!("string scaling budget failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quadratic_growth_trips_checked_in_budget() {
        let budget: Budgets = serde_json::from_str(BUDGETS_JSON).expect("checked-in budget");
        let measurement = |iterations, cpu_duration_ms| Measurement {
            workload: "synthetic",
            iterations,
            cpu_duration_ms,
            cpu_samples_ms: Vec::new(),
            wall_samples_ms: Vec::new(),
            cpu_ns_per_iteration: 0.0,
            profiled_instructions: 1,
            instructions_per_cpu_second: 1.0,
        };
        let small = measurement(1_000, 1.0);
        let linear = measurement(100_000, 100.0);
        let quadratic = measurement(100_000, 10_000.0);
        assert!(
            normalized_time_ratio(&small, &linear)
                <= budget.string_scaling.max_normalized_time_ratio
        );
        assert!(
            normalized_time_ratio(&small, &quadratic)
                > budget.string_scaling.max_normalized_time_ratio
        );
    }

    #[tokio::test]
    async fn sources_lower_and_execute_in_the_vm() {
        for workload in Workload::ALL {
            let compiled = compile(&workload.source(7)).expect("TypeScript lowers");
            assert!(
                execute(&compiled, workload, 7, workload.expected(7), true)
                    .await
                    .unwrap()
                    > 0
            );
        }
    }
}
