//! The `send()`-to-completion latency gate (FIG-3843).
//!
//! Hosts no longer run turns inline: a host calls `send()`, Restate drives
//! the session, and `SendHandle::outcome()` follows live replay and durable
//! state to the answer. This module measures that path end to end on a live
//! `restate-server` and gates the added overhead — host-visible completion
//! minus the deterministic fast provider's own time — at p50 < 50 ms and
//! p99 < 250 ms on the same-process fast fixture.
//!
//! Every sample records the five spans the ticket names: request to durable
//! acceptance, acceptance to drive claim, drive claim to first visible
//! delta, drive claim to root settlement, and root settlement to
//! host-visible completion. Failures land in the same ledger. The
//! `poll` and `grace` cases isolate the send follower's two tail
//! behaviours — the 25 ms..1 s polling backoff and the 5 s live-report
//! grace — by fixing the drive-attach wake the follower waits on.

mod provider;
mod restate;
mod runner;
mod work_engine;
mod worker;

pub(crate) use provider::LatencyProviderKind;
use runner::{CaseSpec, Topology};
use work_engine::AwaitDriveMode;

/// The gate's budget, fixed before measurement (FIG-3843): same-process,
/// no-tool, deterministic fast-provider fixture.
pub const GATE_OVERHEAD_P50_MS: f64 = 50.0;
pub const GATE_OVERHEAD_P99_MS: f64 = 250.0;
/// The ticket's minimum population on the gated fixture.
pub const GATE_MIN_SAMPLES: usize = 10_000;
/// The case the budget binds.
pub const GATE_CASE: &str = "fast";

/// The default case table. `fast` is the gate; every other case is measured
/// and reported but not gated.
pub(crate) fn default_cases(fast_samples: usize, lanes: usize) -> Vec<CaseSpec> {
    vec![
        CaseSpec {
            name: GATE_CASE,
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::Text,
            await_drive: AwaitDriveMode::Real,
            samples: fast_samples,
            // The gated fixture is the *idle* path: pinned at 16 lanes so the
            // measured latency is the send path itself, not server-side
            // queueing (64 lanes saturates this server's commit rate and
            // multiplies every phase). `busy` below owns the loaded variant.
            lanes: 16.min(lanes),
            busy: false,
        },
        CaseSpec {
            name: "stream",
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::Stream,
            await_drive: AwaitDriveMode::Real,
            samples: 500,
            lanes,
            busy: false,
        },
        CaseSpec {
            name: "tool",
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::Tool,
            await_drive: AwaitDriveMode::Real,
            samples: 500,
            lanes,
            busy: false,
        },
        CaseSpec {
            name: "failure",
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::Fail,
            await_drive: AwaitDriveMode::Real,
            samples: 200,
            lanes,
            busy: false,
        },
        CaseSpec {
            name: "busy",
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::Text,
            await_drive: AwaitDriveMode::Real,
            samples: 300,
            lanes,
            busy: true,
        },
        CaseSpec {
            name: "provider-http",
            topology: Topology::SameProcess,
            provider: LatencyProviderKind::OpenAiCompat,
            await_drive: AwaitDriveMode::Real,
            samples: 200,
            lanes,
            busy: false,
        },
        CaseSpec {
            name: "cross-worker",
            topology: Topology::CrossWorker,
            provider: LatencyProviderKind::Text,
            await_drive: AwaitDriveMode::Real,
            samples: 500,
            lanes,
            busy: false,
        },
        // The follower's polling regime isolated: the drive-attach wake is
        // answered immediately, so the store poll alone carries settlement
        // detection and the 25 ms..1 s backoff bounds the settle→complete
        // tail. Runs cross-worker so no live replay or mailbox shortcuts it.
        CaseSpec {
            name: "poll",
            topology: Topology::CrossWorker,
            provider: LatencyProviderKind::Text,
            await_drive: AwaitDriveMode::Answered,
            samples: 300,
            lanes,
            busy: false,
        },
        // The 5 s live-report grace isolated: the drive-attach wake never
        // resolves, so a settled root with no live report waits the grace
        // before the durable answer. Runs cross-worker for the same reason.
        CaseSpec {
            name: "grace",
            topology: Topology::CrossWorker,
            provider: LatencyProviderKind::Text,
            await_drive: AwaitDriveMode::Pending,
            samples: 16,
            lanes: 16,
            busy: false,
        },
    ]
}

/// One `lash-perf latency` run.
pub struct LatencyRun {
    /// Where the JSON report lands.
    pub out: std::path::PathBuf,
    /// Where the raw per-sample ledger lands; defaults to `<out>.samples.json`.
    pub samples_out: Option<std::path::PathBuf>,
    /// Directory the run's SQLite store sets and the cross-worker's live in.
    pub store_dir: std::path::PathBuf,
    /// Which cases to run; empty means the full table.
    pub cases: Vec<String>,
    /// Sample count for the gated `fast` case.
    pub fast_samples: usize,
    /// Concurrent session lanes per case.
    pub lanes: usize,
    /// When set, scale every non-gated case's sample count down for dev runs.
    pub scale_down: bool,
}

/// The `lash-perf latency-worker` side: the second process a cross-worker
/// case drives.
pub struct LatencyWorkerArgs {
    /// The store directory the host opened this worker for.
    pub store_dir: std::path::PathBuf,
    /// Touched once the endpoint is bound and registered; the host polls it.
    pub ready_file: std::path::PathBuf,
    /// Loopback address the Restate endpoint binds.
    pub endpoint_bind: std::net::SocketAddr,
}

/// Run the gate: measure every selected case, write the JSON report and the
/// raw sample ledger, print the human summary, and exit non-zero when the
/// gated case is over budget.
pub async fn run(run: LatencyRun) -> anyhow::Result<i32> {
    let mut specs = default_cases(run.fast_samples, run.lanes);
    let known: std::collections::BTreeSet<&'static str> =
        specs.iter().map(|spec| spec.name).collect();
    for name in &run.cases {
        anyhow::ensure!(
            known.contains(name.as_str()),
            "unknown latency case `{name}` (known: {})",
            known.iter().copied().collect::<Vec<_>>().join(", ")
        );
    }
    if !run.cases.is_empty() {
        specs.retain(|spec| run.cases.iter().any(|name| name == spec.name));
    }
    if run.scale_down {
        for spec in &mut specs {
            spec.samples = spec.samples.min(64);
            spec.lanes = spec.lanes.min(8);
        }
    }

    let env = runner::LatencyEnv::open(&run.store_dir).await?;
    let mut reports = Vec::new();
    let mut samples = Vec::new();
    for spec in &specs {
        let (report, case_samples) = runner::run_case(spec, &env).await?;
        reports.push(report);
        samples.extend(case_samples);
    }
    let report = runner::build_report(env.describe(), reports);
    if let Some(parent) = run.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&run.out, serde_json::to_string_pretty(&report)? + "\n")?;
    let samples_path = run.samples_out.clone().unwrap_or_else(|| {
        run.out.with_file_name(format!(
            "{}.samples.json",
            run.out
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "latency".to_string())
        ))
    });
    std::fs::write(
        &samples_path,
        serde_json::to_string_pretty(&samples)? + "\n",
    )?;
    print_summary(&report);
    Ok(if report.verdict.pass { 0 } else { 2 })
}

/// The human-readable gate summary on stdout.
fn print_summary(report: &runner::LatencyReport) {
    println!("send-to-completion latency gate (FIG-3843)");
    for case in &report.cases {
        println!(
            "\ncase {} [{} / {:?} provider] — {} samples in {:.1}s — statuses {:?}",
            case.name,
            match case.topology {
                runner::Topology::SameProcess => "same-process",
                runner::Topology::CrossWorker => "cross-worker",
            },
            case.provider,
            case.samples,
            case.wall_seconds,
            case.statuses,
        );
        let row = |label: &str, summary: &Option<runner::LatencySummary>| {
            if let Some(summary) = summary {
                println!(
                    "  {:<30} n={:<6} p50={:<8.1} p90={:<8.1} p99={:<8.1} max={:<8.1}",
                    label, summary.n, summary.p50, summary.p90, summary.p99, summary.max,
                );
            }
        };
        let phases = &case.phases_ms;
        row("request→accept ms", &phases.request_to_accept);
        row("accept→drive claim ms", &phases.accept_to_drive_claim);
        row("claim→first delta ms", &phases.drive_claim_to_first_delta);
        row("claim→root settled ms", &phases.drive_claim_to_root_settled);
        row("settled→completion ms", &phases.root_settled_to_completion);
        row("send→completion ms", &phases.send_to_completion);
        row("provider ms", &phases.provider);
        row("overhead ms", &phases.overhead);
        row("poll-detect ms", &phases.poll_detect);
    }
    println!();
    if report.verdict.pass {
        println!("latency gate: PASS");
    } else {
        println!("latency gate: FAIL");
        for violation in &report.verdict.violations {
            println!("  - {violation}");
        }
    }
}

/// The worker half of a cross-worker case: serve lash's Restate services
/// over the shared store directory until the host kills the process.
pub async fn run_worker(args: LatencyWorkerArgs) -> anyhow::Result<()> {
    worker::run(args).await
}
