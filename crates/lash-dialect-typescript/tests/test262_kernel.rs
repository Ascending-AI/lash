//! Test262 on the kernel: the vendored selection lowered by this crate and
//! run on a kernel machine, counted by directory against main's record.

#![expect(
    clippy::expect_used,
    reason = "test target: the helpers around the test are test code too"
)]

#[path = "test262_kernel/ingest.rs"]
mod ingest;
#[path = "test262_kernel/metadata.rs"]
mod metadata;
#[path = "test262_kernel/runner.rs"]
mod runner;

use std::collections::BTreeMap;

use runner::Observed;

/// The tests to run: the stratified sample, or every recorded test whose
/// path starts with `TEST262_KERNEL_FILTER` (`test/built-ins/Array/`).
fn selection(recorded: &BTreeMap<String, String>) -> Vec<String> {
    // Test tooling reads its selection from the environment.
    #[expect(
        clippy::disallowed_methods,
        reason = "test tooling reads its selection from the environment"
    )]
    let filter = std::env::var("TEST262_KERNEL_FILTER").ok();
    match filter {
        Some(prefix) => recorded
            .keys()
            .filter(|path| path.starts_with(&prefix))
            .cloned()
            .collect(),
        None => runner::sample_paths(),
    }
}

/// Every selected test has one observation, and the report counts them by
/// directory. With no machine installed a program that lowers is counted
/// `not-run`; nothing is counted `pass` that did not run to its expected
/// end.
#[test]
fn the_selection_runs_on_the_kernel_and_is_counted_by_directory() {
    let recorded = runner::recorded_classes();
    let paths = selection(&recorded);
    assert!(!paths.is_empty(), "the selection is empty");
    let executor = runner::executor();
    let observations: Vec<(String, Observed)> = paths
        .iter()
        .map(|path| {
            assert!(
                recorded.contains_key(path),
                "{path} has no recorded outcome"
            );
            (path.clone(), runner::run(path, executor.as_deref()))
        })
        .collect();
    let report = runner::report(&observations, &recorded);
    println!("{report}");
    let counted: usize = report
        .lines()
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(|line| {
            line.split('\t')
                .skip(1)
                .take(5)
                .map(|count| count.parse::<usize>().expect("a count"))
                .sum::<usize>()
        })
        .sum();
    assert_eq!(counted, paths.len(), "{report}");
    if executor.is_none() {
        // Only a parse-negative test passes without running.
        for (path, observed) in &observations {
            if *observed == Observed::Pass {
                let meta = metadata::read_metadata(&ingest::data_path(path)).expect("metadata");
                assert!(meta.negative.is_some(), "{path} passed without running");
            }
        }
    }
}
