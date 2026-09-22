//! Gate: store backends may not retype a process or wake-delivery lifecycle
//! literal into a query.
//!
//! FIG-2844. The live-process predicate used to be hand-spelled as
//! `status IN ('running', 'waiting')` at 35 sites, with retention as its
//! `NOT IN` inverse and the wake-delivery labels spelled out again. Adding a
//! [`ProcessStatus`] variant compiled clean and silently dropped rows from
//! worklist queries while making live rows prunable. Both backends now build
//! those predicates from the exported vocabulary
//! (`lash_core::store_backend_support::live_process_status_predicate_sql` and
//! friends); this test fails if a raw literal comes back.
//!
//! Shape: a source-scanning lint test, like
//! `lash-core/tests/integration_boundary.rs`, with a documented exemption
//! inventory, like `scripts/check-substrate-boundary.sh`. It needs no CI wiring
//! because it runs with the workspace suite.

use lash_core::{ProcessStatus, WakeDeliveryState};
use std::path::{Path, PathBuf};

/// Roots whose statements must build their lifecycle filters from the enums.
const SCANNED_ROOTS: &[&str] = &[
    "lash-core/src",
    "lash-sqlite-store/src",
    "lash-postgres-store/src",
];

/// Files that legitimately spell the vocabulary, each for a reason that is not
/// a query predicate.
///
/// - The two schema files declare the durable `CHECK` vocabulary and its
///   partial indexes. That surface is owned by FIG-2811 and is deliberately out
///   of scope here: `schema_congruence` matches these declarations as literal
///   text, so generating them would move the gate rather than satisfy it.
/// - `required_constraints.rs` is the registry of those same `CHECK`
///   declarations, compared against the schema files.
/// - `schema/migrations.rs` carries that same DDL as the versioned migration
///   arms' executable statements — the `113 -> 114` arm's `ADD CONSTRAINT`
///   text is the `schema.sql` declaration verbatim, not a query predicate.
const EXEMPT_FILES: &[&str] = &[
    "lash-sqlite-store/src/schema.rs",
    "lash-core/src/store_backend_support/required_constraints.rs",
    "lash-postgres-store/src/postgres/schema/migrations.rs",
];

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn workspace_crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf()
}

/// Production statements may not.
fn is_test_path(relative: &str) -> bool {
    relative.split('/').any(|segment| {
        segment == "tests"
            || segment == "testing"
            || segment == "test_support.rs"
            || segment.ends_with("_tests.rs")
    })
}

/// Strip line comments and doc comments: prose that names the predicate is
/// documentation, not a query.
fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
}

/// Lowercase the line and drop every space, so a retyped predicate cannot hide
/// behind casing or spacing: `status in ('running','waiting')` and
/// `status='running'` normalise to the same shapes as the canonical spelling.
fn normalise(line: &str) -> String {
    line.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every `<column> <op> '<label>` shape a retyped predicate takes, matched
/// against the normalised line. The labels are themselves lowercase, so a
/// case-folded comparison loses nothing.
fn violations_in(line: &str, column: &str, labels: &[String]) -> Vec<String> {
    let normalised = normalise(line);
    let column = normalise(column);
    let mut found = Vec::new();
    for (operator, spelling) in [
        ("in('", " IN ('"),
        ("notin('", " NOT IN ('"),
        ("='", " = '"),
        ("<>'", " <> '"),
    ] {
        let needle = format!("{column}{operator}");
        let mut search_from = 0;
        while let Some(offset) = normalised[search_from..].find(&needle) {
            let start = search_from + offset + needle.len();
            if let Some(label) = labels
                .iter()
                .find(|label| normalised[start..].starts_with(label.as_str()))
            {
                found.push(format!("{column}{spelling}{label}'"));
            }
            search_from = start;
        }
    }
    found
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn scan_file(
    path: &Path,
    relative: &str,
    status_labels: &[String],
    state_labels: &[String],
) -> Vec<String> {
    let source = std::fs::read_to_string(path).expect("read scanned source");
    let mut failures = Vec::new();
    for (number, line) in source.lines().enumerate() {
        if is_comment(line) {
            continue;
        }
        let mut hits = violations_in(line, "status", status_labels);
        hits.extend(violations_in(line, "state", state_labels));
        for hit in hits {
            failures.push(format!("{relative}:{}: `{hit}`", number + 1));
        }
    }
    failures
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn scan_dir(
    root: &Path,
    crates_dir: &Path,
    status_labels: &[String],
    state_labels: &[String],
    failures: &mut Vec<String>,
) {
    let entries = std::fs::read_dir(root).expect("read scanned directory");
    let mut paths = entries
        .map(|entry| entry.expect("directory entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            scan_dir(&path, crates_dir, status_labels, state_labels, failures);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let relative = path
            .strip_prefix(crates_dir)
            .expect("scanned path under crates")
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&relative) || EXEMPT_FILES.contains(&relative.as_str()) {
            continue;
        }
        failures.extend(scan_file(&path, &relative, status_labels, state_labels));
    }
}

fn scan_workspace() -> Vec<String> {
    let crates_dir = workspace_crates_dir();
    let status_labels = ProcessStatus::ALL
        .iter()
        .map(|status| status.label().to_string())
        .collect::<Vec<_>>();
    let state_labels = WakeDeliveryState::ALL
        .iter()
        .map(|state| state.as_str().to_string())
        .collect::<Vec<_>>();
    let mut failures = Vec::new();
    for root in SCANNED_ROOTS {
        scan_dir(
            &crates_dir.join(root),
            &crates_dir,
            &status_labels,
            &state_labels,
            &mut failures,
        );
    }
    failures
}

#[test]
fn store_statements_never_retype_a_lifecycle_literal() {
    let failures = scan_workspace();
    assert!(
        failures.is_empty(),
        "lifecycle literals must be generated from ProcessStatus / WakeDeliveryState \
         (lash_core::store_backend_support), not retyped into a query:\n{}",
        failures.join("\n")
    );
}

/// The gate would be worthless if it could not see a reintroduced literal.
#[test]
fn the_gate_rejects_a_reintroduced_literal() {
    let status_labels = ProcessStatus::ALL
        .iter()
        .map(|status| status.label().to_string())
        .collect::<Vec<_>>();
    let state_labels = WakeDeliveryState::ALL
        .iter()
        .map(|state| state.as_str().to_string())
        .collect::<Vec<_>>();

    for witness in [
        "         WHERE status IN ('running', 'waiting')",
        "         WHERE status NOT IN ('running', 'waiting')",
        "     WHERE processes.status IN ('running', 'waiting')",
        "                 AND delivery.state IN ('pending', 'enqueuing')",
        "             SET state = 'discarded', discard_reason = 'target_gone'",
        "                 WHERE earlier.state <> 'enqueued'",
        // Lowercase keywords and squeezed spacing are the same predicate.
        "         WHERE status in ('running','waiting') OR status='running'",
        "         where PROCESSES.STATUS Not In ('running', 'waiting')",
        "                 AND delivery.state<>'enqueued'",
    ] {
        let mut hits = violations_in(witness, "status", &status_labels);
        hits.extend(violations_in(witness, "state", &state_labels));
        assert!(!hits.is_empty(), "gate missed a raw literal: {witness}");
    }

    // Generated call sites and unrelated vocabularies stay clean.
    for benign in [
        "        live = live_process_status(\"status\"),",
        "           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))",
        "               AND ($1::TEXT[] IS NULL OR status = ANY($1))",
        "                     SET state = ?3,",
        "        let terminal = \"state IN ('completed', 'cancelled')\";",
    ] {
        let mut hits = violations_in(benign, "status", &status_labels);
        hits.extend(violations_in(benign, "state", &state_labels));
        assert!(
            hits.is_empty(),
            "gate false-positived on: {benign} -> {hits:?}"
        );
    }

    // Comment prose describing the predicate is documentation, not a query.
    assert!(is_comment(
        "/// `status IN ('running', 'waiting')` selects live rows"
    ));
}
