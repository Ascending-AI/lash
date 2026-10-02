//! Test262 conformance, whole-selection ratchet (FIG-3646): every vendored
//! test keeps its recorded outcome in one case, so the documented
//! `TEST262_BLESS=1` recipe can rewrite the record from the run it makes
//! (see README.md). `test262_full.rs` carries the same per-test assertion as
//! shardable corpus partitions.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

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
