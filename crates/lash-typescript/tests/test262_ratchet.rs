//! Test262 ratchet bookkeeping (FIG-4761): the complete selection listing
//! equals the record's keys. Corpus partitions check each observed verdict
//! in `test262_full.rs`; this binary never executes the corpus.
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
#[allow(dead_code, reason = "bookkeeping does not read test metadata")]
mod metadata;
#[path = "test262/support/runner.rs"]
#[allow(
    dead_code,
    reason = "bookkeeping only reads the selection and recorded outcomes"
)]
mod runner;

#[test]
fn selection_listing_matches_the_ratchet() {
    assert!(
        std::env::var_os("TEST262_BLESS").is_none(),
        "TEST262_BLESS requires test262_full's bless_full_selection (see README.md)"
    );
    let recorded = runner::recorded_outcomes();
    let selected = runner::vendored_tests();
    let recorded_paths = recorded
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let missing = selected.difference(&recorded_paths).collect::<Vec<_>>();
    let stale = recorded_paths.difference(&selected).collect::<Vec<_>>();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "the ratchet must cover the complete selection exactly; missing: {missing:?}; stale: {stale:?}"
    );
    eprintln!("{}", runner::summary(&recorded));
    eprintln!("{}", runner::tally_lines(&recorded));
}
