//! Test262 conformance, full selection (FIG-3646): every vendored test keeps
//! its recorded outcome. `TEST262_BLESS=1 kiln run` rewrites the record from
//! the run instead (see README.md).
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]

#[path = "test262/support/ingest.rs"]
#[allow(dead_code, reason = "not every ingest helper is used in this shard")]
mod ingest;
#[path = "test262/support/metadata.rs"]
#[allow(
    dead_code,
    reason = "the full run reads the metadata the runner needs, not the census fields"
)]
mod metadata;
#[path = "test262/support/runner.rs"]
#[allow(
    dead_code,
    reason = "the full run uses the runner, not the census helpers"
)]
mod runner;

use std::collections::BTreeSet;

#[test]
fn full_selection_matches_the_ratchet() {
    let recorded = runner::recorded_outcomes();
    let vendored = runner::vendored_tests().into_iter().collect::<Vec<_>>();
    let paths = match runner::quick_selection(&vendored) {
        Some(subset) => {
            eprintln!(
                "LASH_QUICK: running {} of {} selected tests",
                subset.len(),
                vendored.len()
            );
            subset
        }
        None => vendored,
    };
    let observed = runner::run_all(&paths);
    if runner::bless(&paths, &observed, &recorded) {
        eprintln!("blessed the outcomes shards from the run");
        return;
    }
    let mismatches = runner::compare(&paths, &observed, &recorded);
    eprintln!("{}", runner::summary(&recorded));
    eprintln!("{}", runner::tally_lines(&recorded));
    assert!(
        mismatches.is_empty(),
        "{} Test262 outcomes changed; a new pass must be promoted, a new failure \
         fixed or owned, a changed refusal re-recorded (see README.md):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The `LASH_QUICK` subset is deterministic: the same inputs draw the same
/// tests regardless of input order, each stratum contributes a tenth, and an
/// include keeps the named shard whole.
#[test]
fn quick_subset_is_a_stable_shard_aware_sample() {
    let mut paths = Vec::new();
    for index in 0..40 {
        paths.push(format!("test/language/alpha/case-{index:02}.js"));
        paths.push(format!("test/built-ins/Array/case-{index:02}.js"));
    }
    paths.sort();

    let subset = runner::quick_subset(&paths, &BTreeSet::new());
    assert_eq!(
        (
            subset
                .iter()
                .filter(|path| path.starts_with("test/language/"))
                .count(),
            subset
                .iter()
                .filter(|path| path.starts_with("test/built-ins/"))
                .count(),
        ),
        (4, 4)
    );
    let mut reordered = paths.clone();
    reordered.reverse();
    assert_eq!(subset, runner::quick_subset(&reordered, &BTreeSet::new()));

    let included = runner::quick_subset(&paths, &BTreeSet::from(["language".to_owned()]));
    assert_eq!(
        included
            .iter()
            .filter(|path| path.starts_with("test/language/"))
            .count(),
        40,
        "a shard include keeps that shard whole"
    );
    assert_eq!(
        runner::quick_subset(&paths, &BTreeSet::from(["*".to_owned()])).len(),
        paths.len(),
        "`*` keeps the whole selection"
    );
    let exact = runner::quick_subset(
        &paths,
        &BTreeSet::from(["test/language/alpha/case-07.js".to_owned()]),
    );
    assert!(exact.contains("test/language/alpha/case-07.js"));
}
