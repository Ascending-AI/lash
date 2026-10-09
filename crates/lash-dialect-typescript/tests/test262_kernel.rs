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

const REGISTER: &str = include_str!("../deviations.md");

/// The tests the deviation register excludes, each with the code of the
/// row that is its grounds: the rows of its "Test262 exclusions" table.
fn registered_exclusions() -> BTreeMap<String, String> {
    REGISTER
        .split("## Test262 exclusions")
        .nth(1)
        .expect("the register lists the Test262 exclusions")
        .lines()
        .filter(|line| line.starts_with("| `test/"))
        .map(|line| {
            let (test, code) = line
                .trim_matches('|')
                .split_once(" | ")
                .expect("a row is a test and a code");
            (
                test.trim().trim_matches('`').to_string(),
                code.trim().to_string(),
            )
        })
        .collect()
}

/// Every selected test has one observation, and the report counts them by
/// directory. Every recorded pass must run to its expected end or be
/// explicitly refused in the outcome record and census, or excluded by a
/// registered dialect deviation.
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
            (path.clone(), runner::run(path, executor.as_ref()))
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
                .take(4)
                .map(|count| count.parse::<usize>().expect("a count"))
                .sum::<usize>()
        })
        .sum();
    assert_eq!(counted, paths.len(), "{report}");
    let excluded = registered_exclusions();
    for (path, code) in &excluded {
        assert_eq!(
            recorded.get(path).map(String::as_str),
            Some("pass"),
            "the register excludes {path}, which the record does not mark as passing"
        );
        assert!(
            REGISTER.contains(&format!("\n| {code} | ")),
            "the register excludes {path} under {code}, which is no row's code"
        );
    }
    let regressed: Vec<_> = observations
        .iter()
        .filter(|(path, observed)| {
            recorded.get(path).is_some_and(|class| class == "pass")
                && *observed != Observed::Pass
                && !excluded.contains_key(path)
        })
        .collect();
    for (path, observed) in &regressed {
        println!("regression\t{path}\t{observed:?}");
    }
    assert!(
        regressed.is_empty(),
        "{} recorded passes did not pass on the kernel; register a real dialect refusal in the census and outcome record, or fix the implementation",
        regressed.len()
    );
}

/// K-DIALECT-001 over every recorded Test262 program, rather than a sample.
#[test]
fn every_lowered_test262_program_prints_to_the_same_kernel_behavior() {
    let recorded = runner::recorded_classes();
    let accepted = recorded
        .keys()
        .filter(|path| runner::printing_relowers(path))
        .count();
    assert!(accepted > 0, "no Test262 program reached the printer law");
    println!(
        "printer law: considered={} lowered={accepted} refused_or_missing_harness={}",
        recorded.len(),
        recorded.len() - accepted
    );
}
