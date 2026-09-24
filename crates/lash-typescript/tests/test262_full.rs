//! Test262 conformance, full selection (FIG-3646): every vendored test keeps
//! its recorded outcome. `TEST262_BLESS=1 kiln run` rewrites the record from
//! the run instead (see README.md).
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]

#[path = "test262/support/ingest.rs"]
#[allow(
    dead_code,
    reason = "the corpus laws read the harness bindings; the runner does not"
)]
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
    let paths = runner::vendored_tests().into_iter().collect::<Vec<_>>();
    let observed = runner::run_all(&paths);
    if runner::bless(&paths, &observed, &recorded) {
        eprintln!("blessed outcomes.tsv from the run");
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
