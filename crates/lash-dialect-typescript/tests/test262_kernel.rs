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

// Test tooling may read an explicit local selection; the default covers the record.
#[allow(clippy::disallowed_methods)]
fn selection_filter() -> Vec<String> {
    std::env::var("TEST262_KERNEL_FILTER")
        .ok()
        .map(|value| value.split(',').map(str::to_owned).collect())
        .unwrap_or_default()
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
/// excluded by an exact registered dialect deviation.
fn run_selection(shard: usize) {
    let recorded = runner::recorded_classes();
    let filter = selection_filter();
    for prefix in &filter {
        assert!(
            recorded.keys().any(|path| path.starts_with(prefix)),
            "no recorded case matches {prefix}"
        );
    }
    // A stable partition of the complete record. Concurrent libtest laws keep
    // expensive families from monopolizing the single test action's deadline.
    let paths: Vec<_> = recorded
        .keys()
        .enumerate()
        .filter(|(index, path)| {
            index % 40 == shard
                && (filter.is_empty() || filter.iter().any(|prefix| path.starts_with(prefix)))
        })
        .map(|(_, path)| path.clone())
        .collect();
    assert!(!paths.is_empty(), "the selection is empty");
    let executor = runner::executor();
    let observations: Vec<(String, Observed)> = paths
        .iter()
        .map(|path| {
            assert!(
                recorded.contains_key(path),
                "{path} has no recorded outcome"
            );
            println!("case-start\t{path}");
            let observed = runner::run(path, executor.as_ref());
            let qualifier = match &observed {
                Observed::Pass => "-",
                Observed::Refused(code, _) | Observed::Harness(code) => code,
                Observed::Diverged(_) => "kernel",
            };
            println!("outcome\t{path}\t{}\t{qualifier}", observed.class());
            if observed != Observed::Pass {
                println!("detail\t{path}\t{observed:?}");
            }
            (path.clone(), observed)
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
        "{} recorded passes did not pass on the kernel; register an exact kernel-rule deviation or fix the implementation",
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

/// Every recorded case belongs to exactly one of these full-selection laws.
mod selection {
    macro_rules! shards {
        ($($name:ident: $index:literal),* $(,)?) => {$ (
            #[test]
            fn $name() { super::run_selection($index); }
        )*};
    }
    shards! {
        shard_00: 0,
        shard_01: 1,
        shard_02: 2,
        shard_03: 3,
        shard_04: 4,
        shard_05: 5,
        shard_06: 6,
        shard_07: 7,
        shard_08: 8,
        shard_09: 9,
        shard_10: 10,
        shard_11: 11,
        shard_12: 12,
        shard_13: 13,
        shard_14: 14,
        shard_15: 15,
        shard_16: 16,
        shard_17: 17,
        shard_18: 18,
        shard_19: 19,
        shard_20: 20,
        shard_21: 21,
        shard_22: 22,
        shard_23: 23,
        shard_24: 24,
        shard_25: 25,
        shard_26: 26,
        shard_27: 27,
        shard_28: 28,
        shard_29: 29,
        shard_30: 30,
        shard_31: 31,
        shard_32: 32,
        shard_33: 33,
        shard_34: 34,
        shard_35: 35,
        shard_36: 36,
        shard_37: 37,
        shard_38: 38,
        shard_39: 39,
    }
}
