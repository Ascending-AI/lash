#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-expect-in-tests only exempts #[test] functions, and the probe helpers around them in this target are test code too"
)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lash_sim::runner::FIXED_AGENT_PRODUCT_CONTRACTS;
use lash_sim::stack_policy::{PRODUCT_STACK_BUDGET_BYTES, SIM_HARNESS_STACK_LIMIT_BYTES};

/// Liveness fence for one probe child, not a performance budget: a warm probe
/// replays its contract and exits in tens of milliseconds, so this bound is
/// three orders of magnitude of headroom. Its job is to make a child that
/// stops making progress fail *this* contract's case with the child's own
/// output, instead of blocking the parent in `wait` until the Bazel test
/// action's own timeout kills the whole suite with no test line emitted
/// (FIG-3124).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// The fast path costs at most one of these; the slow path costs a wakeup
/// every interval until the fence above fires.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// One `#[test]` per fixed Agent product contract.
///
/// The probes used to share a single case that looped over
/// `FIXED_AGENT_PRODUCT_CONTRACTS`, which made every child's runtime, and every
/// child's failure, indistinguishable from the others: a single stuck child
/// took the whole suite to the action timeout and libtest reported nothing
/// about any of the nine. A case per contract keeps the coverage identical and
/// makes the report name the contract.
macro_rules! product_stack_probe_cases {
    ($($case:ident => $contract:literal,)+) => {
        /// The contracts the cases below cover, in declaration order;
        /// `stack_policy_probe_cases_cover_every_fixed_agent_product_contract`
        /// pins this against the product list so a new contract cannot land
        /// without its own case.
        const PROBED_CONTRACTS: &[&str] = &[$($contract,)+];

        $(
            #[test]
            fn $case() {
                assert_product_stack_probe_passes($contract);
            }
        )+
    };
}

product_stack_probe_cases! {
    stack_policy_probe_agent_foreground_tool_call_round_trip_passes_at_2_mib =>
        "agent.foreground_tool_call_round_trip",
    stack_policy_probe_agent_started_process_tool_call_graph_passes_at_2_mib =>
        "agent.started_process_tool_call_graph",
    stack_policy_probe_agent_durable_input_suspension_resolution_passes_at_2_mib =>
        "agent.durable_input_suspension_resolution",
    stack_policy_probe_agent_started_process_subagent_spawn_passes_at_2_mib =>
        "agent.started_process_subagent_spawn",
    stack_policy_probe_agent_nested_process_start_await_passes_at_2_mib =>
        "agent.nested_process_start_await",
    stack_policy_probe_agent_session_turn_process_child_passes_at_2_mib =>
        "agent.session_turn_process_child",
    stack_policy_probe_agent_failed_child_preserves_failure_graph_passes_at_2_mib =>
        "agent.failed_child_preserves_failure_graph",
    stack_policy_probe_agent_parallel_spawn_and_join_passes_at_2_mib =>
        "agent.parallel_spawn_and_join",
    stack_policy_probe_agent_tuple_values_finish_as_json_arrays_passes_at_2_mib =>
        "agent.tuple_values_finish_as_json_arrays",
}

#[test]
fn stack_policy_probe_cases_cover_every_fixed_agent_product_contract() {
    assert_eq!(
        PROBED_CONTRACTS, FIXED_AGENT_PRODUCT_CONTRACTS,
        "every fixed Agent product contract needs its own product stack probe case",
    );
}

#[test]
// Architecture lint: lexical escape-hatch guard. The product-stack probe cases
// above are the behavioral half of this policy.
fn lint_stack_policy_rejects_raw_stack_literals_and_global_stack_escape_hatches() {
    assert_eq!(PRODUCT_STACK_BUDGET_BYTES, 2 * 1024 * 1024);
    assert_eq!(SIM_HARNESS_STACK_LIMIT_BYTES, 8 * 1024 * 1024);

    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let checked_files = ["main.rs", "lib.rs", "stack_policy.rs"];
    let mut checked_paths: Vec<PathBuf> = checked_files
        .iter()
        .map(|file| src_dir.join(file))
        .collect();
    let runner_dir = src_dir.join("runner");
    let runner_entries = std::fs::read_dir(&runner_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", runner_dir.display()));
    for entry in runner_entries {
        let path = entry.expect("runner dir entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            checked_paths.push(path);
        }
    }
    let mut stack_size_lines = Vec::new();

    for path in checked_paths {
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        assert!(
            !body.contains("RUST_MIN_STACK"),
            "{} must not use global stack escape hatches",
            path.display()
        );

        for (line_index, line) in body.lines().enumerate() {
            if line.contains(".stack_size(") {
                stack_size_lines.push(format!("{}:{}:{line}", path.display(), line_index + 1));
            }
        }
    }

    assert!(
        matches!(stack_size_lines.as_slice(), [line] if line.starts_with(&src_dir.join("stack_policy.rs").display().to_string())
            && line.ends_with(".stack_size(stack_bytes)")),
        "all lash-sim thread stacks must flow through the named stack policy helper; found {stack_size_lines:?}",
    );
    assert_eq!(
        stack_size_lines.len(),
        1,
        "all lash-sim thread stacks must flow through the named stack policy helper",
    );
}

fn assert_product_stack_probe_passes(contract: &str) {
    let binary = lash_sim_binary();
    let stack_bytes = PRODUCT_STACK_BUDGET_BYTES.to_string();

    let mut child = Command::new(&binary)
        .args([
            "stack-probe",
            "agent-contract",
            "--contract",
            contract,
            "--stack-bytes",
            &stack_bytes,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {}: {err}", binary.display()));

    // Drain both pipes on their own threads: a child that fills a pipe while
    // the parent is polling would otherwise block on the write and be reported
    // as the hang this fence exists to catch.
    let stdout = drain(child.stdout.take().expect("probe child stdout is piped"));
    let stderr = drain(child.stderr.take().expect("probe child stderr is piped"));

    let status = wait_until(&mut child, PROBE_TIMEOUT);
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = collect(stdout);
    let stderr = collect(stderr);

    let Some(status) = status else {
        panic!(
            "product stack probe for `{contract}` did not exit within {PROBE_TIMEOUT:?} at {stack_bytes} bytes; the child was killed\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
    };

    assert!(
        status.success(),
        "product stack probe for `{contract}` failed at {stack_bytes} bytes\nstatus: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

/// `Some(status)` when the child exited before the deadline, `None` when it is
/// still running at the deadline. `std::process::Child` has no timed wait, and
/// `Command::output` waits forever, which is the defect this replaces.
fn wait_until(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .expect("poll the product stack probe child")
        {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => return None,
            None => std::thread::sleep(PROBE_POLL_INTERVAL),
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // A read error here is the child's pipe closing under a kill; the
        // bytes already read are still the evidence we want to print.
        let _ = pipe.read_to_end(&mut buffer);
        String::from_utf8_lossy(&buffer).into_owned()
    })
}

fn collect(reader: JoinHandle<String>) -> String {
    reader
        .join()
        .unwrap_or_else(|_| "<probe output reader panicked>".to_string())
}

fn lash_sim_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_lash-sim") {
        return PathBuf::from(path);
    }

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../target/debug/lash-sim");
    path
}
