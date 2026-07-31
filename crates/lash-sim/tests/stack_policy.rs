use std::path::{Path, PathBuf};
use std::process::Command;

use lash_sim::runner::FIXED_AGENT_PRODUCT_CONTRACTS;
use lash_sim::stack_policy::{PRODUCT_STACK_BUDGET_BYTES, SIM_HARNESS_STACK_LIMIT_BYTES};

#[test]
fn stack_policy_agent_contract_product_probes_pass_at_2_mib() {
    let binary = lash_sim_binary();
    let stack_bytes = PRODUCT_STACK_BUDGET_BYTES.to_string();

    for contract in FIXED_AGENT_PRODUCT_CONTRACTS {
        let output = Command::new(&binary)
            .args([
                "stack-probe",
                "agent-contract",
                "--contract",
                contract,
                "--stack-bytes",
                &stack_bytes,
            ])
            .output()
            .unwrap_or_else(|err| panic!("failed to run {}: {err}", binary.display()));

        assert!(
            output.status.success(),
            "product stack probe for `{contract}` failed at {stack_bytes} bytes\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
// Architecture lint: lexical escape-hatch guard. The preceding product-stack
// probe is the behavioral half of this policy.
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

fn lash_sim_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_lash-sim") {
        return PathBuf::from(path);
    }

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../target/debug/lash-sim");
    path
}
