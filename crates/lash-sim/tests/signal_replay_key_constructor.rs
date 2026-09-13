//! Gate: nothing may hand-spell the process signal replay key.
//!
//! FIG-2876. The key was written out as `process:{id}:signal.{name}:{tail}` at
//! eight sites — two in the session-manager control seam, one in the facade's
//! tool-intent ingress, two in core's in-tree mock registry, one in the session
//! execution context, one in the await-effect suffix, and one in a runbook
//! worker binary *outside* the workspace crates. A change to the format in core
//! left the other seven minting a different durable dedupe key: double
//! realization of one recorded intent, and not a compile error. Every site now
//! calls `lash_core::runtime::process_signal_wait_key` (or its `await` sibling),
//! and this test fails if the literal comes back.
//!
//! The scan also caught four sites the ticket's inventory predates — conformance
//! fixtures that interpolate the process id around a hard-coded signal name —
//! because a fixture re-spelling the surrounding format drifts exactly the same
//! way the eight did.
//!
//! Shape: a source-scanning lint test with a documented exemption inventory,
//! modelled on `process_lifecycle_vocabulary.rs` (FIG-2844). It needs no CI
//! wiring because it runs with the workspace suite. It scans the whole
//! repository, not just `crates/`, because the runbook binary is exactly the
//! copy that could escape the workspace unnoticed.

use std::path::{Path, PathBuf};

/// Directories under the workspace root whose Rust sources are scanned.
const SCANNED_ROOTS: &[&str] = &["crates", "runbooks", "examples"];

/// The one file that may spell the format: the constructor and its `await`
/// sibling, which share the prefix by construction.
const EXEMPT_FILES: &[&str] = &["crates/lash-core/src/runtime/process/events.rs"];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/lash-sim")
        .to_path_buf()
}

/// Test sources may pin the spelling: that is how the key's byte identity is
/// proved, and a pinned expectation is the opposite of a second minting site.
fn is_test_path(relative: &str) -> bool {
    relative.split('/').any(|segment| {
        segment == "tests"
            || segment == "testing"
            || segment == "benches"
            || segment.ends_with("_tests.rs")
            || segment == "tests.rs"
    })
}

fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
}

/// Drop whitespace so a reformatted or re-spaced literal cannot hide.
fn normalise(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A line mints the key if it interpolates a process id into the `process:`
/// namespace *and* carries the `:signal.` segment — whether or not the signal
/// name itself is a hole, because a fixture that hard-codes `signal.ready` still
/// re-spells the surrounding format. A fully fixed spelling such as
/// `process:1:signal.ready:1` interpolates nothing and is a pinned expectation,
/// not a minting site.
fn mints_signal_key(line: &str) -> bool {
    let normalised = normalise(line);
    normalised.contains("process:{") && normalised.contains(":signal.")
}

fn scan_file(path: &Path, relative: &str, failures: &mut Vec<String>) {
    let source = std::fs::read_to_string(path).expect("read scanned source");
    for (number, line) in source.lines().enumerate() {
        if is_comment(line) || !mints_signal_key(line) {
            continue;
        }
        failures.push(format!("{relative}:{}: {}", number + 1, line.trim()));
    }
}

fn scan_dir(root: &Path, workspace: &Path, failures: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut paths = entries
        .map(|entry| entry.expect("directory entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name == "target" || name == "node_modules" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, workspace, failures);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let relative = path
            .strip_prefix(workspace)
            .expect("scanned path under the workspace")
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&relative) || EXEMPT_FILES.contains(&relative.as_str()) {
            continue;
        }
        scan_file(&path, &relative, failures);
    }
}

fn scan_workspace() -> Vec<String> {
    let workspace = workspace_root();
    let mut failures = Vec::new();
    for root in SCANNED_ROOTS {
        scan_dir(&workspace.join(root), &workspace, &mut failures);
    }
    failures
}

#[test]
fn the_signal_replay_key_format_appears_once() {
    let failures = scan_workspace();
    assert!(
        failures.is_empty(),
        "the process signal replay key must be built by \
         lash_core::runtime::process_signal_wait_key, not retyped:\n{}",
        failures.join("\n")
    );
}

/// The gate would be worthless if it could not see a reintroduced literal.
#[test]
fn the_gate_rejects_a_reintroduced_literal() {
    assert!(mints_signal_key(
        r#"    let key = format!("process:{process_id}:signal.{signal_name}:{signal_id}");"#
    ));
    assert!(mints_signal_key(
        r#"                "process:{}:signal.{}:{}","#
    ));
    assert!(
        mints_signal_key(r#"format!("process:{id}:signal.{name}:await:{ordinal}")"#),
        "the await variant is the same prefix and must also route through core"
    );
    assert!(
        !mints_signal_key(r#"    Ok(format!("signal.{signal_name}"))"#),
        "the bare event type is a different key and has its own constructor"
    );
    assert!(
        mints_signal_key(r#"    key: format!("process:{process_id}:signal.ready:1"),"#),
        "a hard-coded signal name still re-spells the surrounding format"
    );
    assert!(
        !mints_signal_key(r#"    key: "process:1:signal.ready:1".to_string(),"#),
        "a fully fixed spelling interpolates nothing and is a pinned expectation"
    );
}

/// The scan must actually reach the runbook binary that motivated the ticket;
/// an empty or crates-only walk would pass vacuously.
#[test]
fn the_scan_reaches_the_runbook_worker_outside_the_workspace_crates() {
    let worker = workspace_root().join("runbooks/restate-postgres-workers/src/bin/worker.rs");
    assert!(
        worker.is_file(),
        "the runbook worker moved; re-point this gate's coverage proof"
    );
    let source = std::fs::read_to_string(&worker).expect("read the runbook worker");
    assert!(
        source.contains("process_signal_wait_key"),
        "the runbook worker must build its replay key from the core constructor"
    );
}
