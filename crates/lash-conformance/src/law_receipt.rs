//! Execution receipts for registered conformance laws (FIG-3429, item 8).
//!
//! A law is *registered* by a catalogue row in `macros.rs`; it is *executed*
//! only when the generated test reaches the end of its body. The difference is
//! the hole this file exists to close: a fixture that self-skips — a store
//! suite whose database URL is absent, the FIG-3414 shape — returns from the
//! generated test before the receipt call is reached, so "the test is green"
//! and "the law ran" were never the same fact until now.
//!
//! The mechanism is deliberately the smallest thing that survives every test
//! driver: each generated test appends one line — `law<TAB>label` — to a
//! receipts file. The destination is picked
//! per process:
//!
//! * `LASH_LAW_RECEIPTS` — the explicit path the Cargo/nextest CI steps set.
//! * `TEST_UNDECLARED_OUTPUTS_DIR` — Bazel's own seam: a test that writes
//!   `law-receipts.txt` there has it collected under
//!   `bazel-testlogs/<target>/test.outputs/`, which is how a trusted
//!   `bazel test` produces the same record with no extra plumbing.
//! * Neither set — no-op. Local `cargo test` runs cost nothing.
//!
//! The census in `scripts/check_law_execution_receipts.py` turns registration
//! and receipts into the two-sided check: every registered law a claiming job
//! ran must appear in its receipts, and a receipt naming no registered law is
//! a bug in the receipt itself.

use std::io::Write as _;
use std::path::PathBuf;

/// The file Bazel collects when a test writes into its undeclared-outputs dir.
const BAZEL_RECEIPT_NAME: &str = "law-receipts.txt";

/// Records that `law` executed under fixture `label`.
///
/// Called by the generated test *after* the law body returns, so a fixture
/// that skips — an early `return` on a missing service — leaves no record,
/// which is exactly the registered-but-skipped state the census fails on.
/// Cheap enough to run unconditionally: two environment reads and, when no
/// receipt destination is configured, nothing else.
pub fn record(law: &'static str, label: &'static str) {
    let Some(path) = receipt_path() else {
        return;
    };
    let line = format!("{law}\t{label}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // A lost line degrades the census to a false failure, never a false
        // pass — so a failed write is swallowed rather than reported.
        let _ = file.write_all(line.as_bytes());
    }
}

fn receipt_path() -> Option<PathBuf> {
    receipt_path_from(|name| std::env::var(name).ok())
}

fn receipt_path_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(path) = get("LASH_LAW_RECEIPTS") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = get("TEST_UNDECLARED_OUTPUTS_DIR") {
        return Some(PathBuf::from(dir).join(BAZEL_RECEIPT_NAME));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no destination configured the receipt is a no-op: the mechanism
    /// must never make an ordinary `cargo test` slower or flakier.
    #[test]
    fn an_unconfigured_receipt_writes_nothing() {
        assert_eq!(receipt_path_from(|_| None), None);
    }

    #[test]
    fn the_explicit_path_wins_over_the_bazel_fallback() {
        let resolved = receipt_path_from(|name| match name {
            "LASH_LAW_RECEIPTS" => Some("explicit.txt".into()),
            "TEST_UNDECLARED_OUTPUTS_DIR" => Some("undeclared".into()),
            _ => None,
        });
        assert_eq!(resolved, Some(PathBuf::from("explicit.txt")));
    }

    #[test]
    fn the_bazel_fallback_writes_one_named_file_in_undeclared_outputs() {
        let resolved = receipt_path_from(|name| match name {
            "TEST_UNDECLARED_OUTPUTS_DIR" => Some("undeclared".into()),
            _ => None,
        });
        assert_eq!(
            resolved,
            Some(PathBuf::from("undeclared").join(BAZEL_RECEIPT_NAME))
        );
    }
}
