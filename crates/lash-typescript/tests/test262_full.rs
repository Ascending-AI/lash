//! Test262 conformance, full selection (FIG-3646): every vendored test keeps
//! its recorded outcome. The corpus runs as [`runner::CORPUS_PARTITIONS`]
//! disjoint cases, so the Buck2 shards spread them across lanes (FIG-4733).
//! The whole-selection ratchet and the `TEST262_BLESS` recipe live in
//! `test262_ratchet.rs`.
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

use std::collections::BTreeSet;

/// Dev-time focused row runner: `TEST262_ONLY=a.js,b.js` prints each observed
/// outcome instead of running the full selection.
#[test]
fn focused_rows() {
    let Ok(filter) = std::env::var("TEST262_ONLY") else {
        return;
    };
    for relative in filter.split(',') {
        eprintln!("{relative}\n  -> {}", runner::run(relative));
    }
}

/// Corpus partition `index` of the selection (the `LASH_QUICK` subset when
/// the knob is set): the partition's tests run and compare against the
/// record. The partitions are disjoint and their union is the selection, so
/// the target checks every vendored test while the Buck2 shards spread the
/// cases. Re-recording stays with the `test262_ratchet` target: a bless must
/// see the whole selection in one run.
fn run_corpus_partition(index: usize) {
    assert!(
        std::env::var_os("TEST262_BLESS").is_none(),
        "TEST262_BLESS rewrites the whole record; run the test262_ratchet target (see README.md)"
    );
    let recorded = runner::recorded_outcomes();
    let vendored = runner::vendored_tests().into_iter().collect::<Vec<_>>();
    let selection = match runner::quick_selection(&vendored) {
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
    let paths = runner::corpus_partition(&selection, index);
    eprintln!(
        "corpus partition {index}: running {} of {} selected tests",
        paths.len(),
        selection.len()
    );
    let observed = runner::run_all(&paths);
    let mismatches = runner::compare(&paths, &observed, &recorded);
    assert!(
        mismatches.is_empty(),
        "corpus partition {index}: {} Test262 outcomes changed; a new pass must be promoted, a \
         new failure fixed or owned, a changed refusal re-recorded (see README.md):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The partitions are disjoint, nonempty and their union is the whole
/// selection — the property the sharding relies on to keep coverage identical
/// to the old single-case full run.
#[test]
fn corpus_partitions_cover_the_selection() {
    let vendored = runner::vendored_tests().into_iter().collect::<Vec<_>>();
    let mut covered = BTreeSet::new();
    for index in 0..runner::CORPUS_PARTITIONS {
        let partition = runner::corpus_partition(&vendored, index);
        assert!(!partition.is_empty(), "corpus partition {index} is empty");
        for path in partition {
            assert!(covered.insert(path), "a test is in two corpus partitions");
        }
    }
    assert_eq!(covered.len(), vendored.len());
}

/// One corpus case per partition so the Buck2 shards can spread them; the
/// literal count must match `runner::CORPUS_PARTITIONS`.
macro_rules! corpus_partition_case {
    ($name:ident, $index:literal) => {
        #[test]
        fn $name() {
            run_corpus_partition($index);
        }
    };
}

const _: () = assert!(runner::CORPUS_PARTITIONS == 32);

corpus_partition_case!(corpus_partition_00, 0);
corpus_partition_case!(corpus_partition_01, 1);
corpus_partition_case!(corpus_partition_02, 2);
corpus_partition_case!(corpus_partition_03, 3);
corpus_partition_case!(corpus_partition_04, 4);
corpus_partition_case!(corpus_partition_05, 5);
corpus_partition_case!(corpus_partition_06, 6);
corpus_partition_case!(corpus_partition_07, 7);
corpus_partition_case!(corpus_partition_08, 8);
corpus_partition_case!(corpus_partition_09, 9);
corpus_partition_case!(corpus_partition_10, 10);
corpus_partition_case!(corpus_partition_11, 11);
corpus_partition_case!(corpus_partition_12, 12);
corpus_partition_case!(corpus_partition_13, 13);
corpus_partition_case!(corpus_partition_14, 14);
corpus_partition_case!(corpus_partition_15, 15);
corpus_partition_case!(corpus_partition_16, 16);
corpus_partition_case!(corpus_partition_17, 17);
corpus_partition_case!(corpus_partition_18, 18);
corpus_partition_case!(corpus_partition_19, 19);
corpus_partition_case!(corpus_partition_20, 20);
corpus_partition_case!(corpus_partition_21, 21);
corpus_partition_case!(corpus_partition_22, 22);
corpus_partition_case!(corpus_partition_23, 23);
corpus_partition_case!(corpus_partition_24, 24);
corpus_partition_case!(corpus_partition_25, 25);
corpus_partition_case!(corpus_partition_26, 26);
corpus_partition_case!(corpus_partition_27, 27);
corpus_partition_case!(corpus_partition_28, 28);
corpus_partition_case!(corpus_partition_29, 29);
corpus_partition_case!(corpus_partition_30, 30);
corpus_partition_case!(corpus_partition_31, 31);

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
