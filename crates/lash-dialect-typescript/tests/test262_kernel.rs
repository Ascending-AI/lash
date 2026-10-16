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

const SELECTION_PARTITIONS: usize = 160;
const PRINTING_PARTITIONS: usize = 8;

fn partition_paths(
    recorded: &BTreeMap<String, String>,
    shard: usize,
    partitions: usize,
) -> impl Iterator<Item = &String> {
    assert!(shard < partitions);
    recorded
        .keys()
        .enumerate()
        .filter(move |(index, _)| index % partitions == shard)
        .map(|(_, path)| path)
}

/// No execution law owns more than a 160th of the corpus (rounded up),
/// and the declared execution and printer laws each cover it exactly once.
#[test]
fn partitions_cover_the_corpus_once_in_bounded_laws() {
    let recorded = runner::recorded_classes();
    assert!(!recorded.is_empty());
    for (declared, count) in [
        (selection::PARTITIONS, SELECTION_PARTITIONS),
        (printing::PARTITIONS, PRINTING_PARTITIONS),
    ] {
        assert_eq!(declared, (0..count).collect::<Vec<_>>());
        let mut seen = std::collections::BTreeSet::new();
        for &shard in declared {
            let paths: Vec<_> = partition_paths(&recorded, shard, count).collect();
            assert!(!paths.is_empty(), "partition {shard}/{count} is empty");
            if count == SELECTION_PARTITIONS {
                assert!(
                    paths.len() <= recorded.len().div_ceil(160),
                    "execution partition {shard} owns {} cases; limit is {}",
                    paths.len(),
                    recorded.len().div_ceil(160),
                );
            }
            for path in paths {
                assert!(seen.insert(path), "{path} occurs in multiple partitions");
            }
        }
        assert_eq!(seen, recorded.keys().collect());
    }
    println!("{} cases covered once by each family", recorded.len());
}

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
    let paths: Vec<_> = partition_paths(&recorded, shard, SELECTION_PARTITIONS)
        .filter(|path| filter.is_empty() || filter.iter().any(|prefix| path.starts_with(prefix)))
        .cloned()
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

/// K-DIALECT-001 over a disjoint part of the complete recorded corpus.
/// Keep the printer work spread across test actions, just like execution,
/// so one full-corpus law cannot monopolize the remote action's deadline.
fn run_printing_selection(shard: usize) {
    let recorded = runner::recorded_classes();
    let paths: Vec<_> = partition_paths(&recorded, shard, PRINTING_PARTITIONS).collect();
    assert!(!paths.is_empty(), "the printer selection is empty");
    let accepted = paths
        .iter()
        .filter(|path| runner::printing_relowers(path))
        .count();
    assert!(accepted > 0, "no Test262 program reached the printer law");
    println!(
        "printer law: shard={shard} considered={} lowered={accepted} refused_or_missing_harness={}",
        paths.len(),
        paths.len() - accepted
    );
}

/// Every recorded case reaches exactly one K-DIALECT-001 printer law.
mod printing {
    macro_rules! shards {
        ($($name:ident: $index:literal),* $(,)?) => {
            pub(super) const PARTITIONS: &[usize] = &[$($index),*];
            $(
            #[test]
            fn $name() { super::run_printing_selection($index); }
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
    }
}

/// Every recorded case belongs to exactly one of these full-selection laws.
mod selection {
    macro_rules! shards {
        ($($name:ident: $index:literal),* $(,)?) => {
            pub(super) const PARTITIONS: &[usize] = &[$($index),*];
            $(
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
        shard_40: 40,
        shard_41: 41,
        shard_42: 42,
        shard_43: 43,
        shard_44: 44,
        shard_45: 45,
        shard_46: 46,
        shard_47: 47,
        shard_48: 48,
        shard_49: 49,
        shard_50: 50,
        shard_51: 51,
        shard_52: 52,
        shard_53: 53,
        shard_54: 54,
        shard_55: 55,
        shard_56: 56,
        shard_57: 57,
        shard_58: 58,
        shard_59: 59,
        shard_60: 60,
        shard_61: 61,
        shard_62: 62,
        shard_63: 63,
        shard_64: 64,
        shard_65: 65,
        shard_66: 66,
        shard_67: 67,
        shard_68: 68,
        shard_69: 69,
        shard_70: 70,
        shard_71: 71,
        shard_72: 72,
        shard_73: 73,
        shard_74: 74,
        shard_75: 75,
        shard_76: 76,
        shard_77: 77,
        shard_78: 78,
        shard_79: 79,
        shard_80: 80,
        shard_81: 81,
        shard_82: 82,
        shard_83: 83,
        shard_84: 84,
        shard_85: 85,
        shard_86: 86,
        shard_87: 87,
        shard_88: 88,
        shard_89: 89,
        shard_90: 90,
        shard_91: 91,
        shard_92: 92,
        shard_93: 93,
        shard_94: 94,
        shard_95: 95,
        shard_96: 96,
        shard_97: 97,
        shard_98: 98,
        shard_99: 99,
        shard_100: 100,
        shard_101: 101,
        shard_102: 102,
        shard_103: 103,
        shard_104: 104,
        shard_105: 105,
        shard_106: 106,
        shard_107: 107,
        shard_108: 108,
        shard_109: 109,
        shard_110: 110,
        shard_111: 111,
        shard_112: 112,
        shard_113: 113,
        shard_114: 114,
        shard_115: 115,
        shard_116: 116,
        shard_117: 117,
        shard_118: 118,
        shard_119: 119,
        shard_120: 120,
        shard_121: 121,
        shard_122: 122,
        shard_123: 123,
        shard_124: 124,
        shard_125: 125,
        shard_126: 126,
        shard_127: 127,
        shard_128: 128,
        shard_129: 129,
        shard_130: 130,
        shard_131: 131,
        shard_132: 132,
        shard_133: 133,
        shard_134: 134,
        shard_135: 135,
        shard_136: 136,
        shard_137: 137,
        shard_138: 138,
        shard_139: 139,
        shard_140: 140,
        shard_141: 141,
        shard_142: 142,
        shard_143: 143,
        shard_144: 144,
        shard_145: 145,
        shard_146: 146,
        shard_147: 147,
        shard_148: 148,
        shard_149: 149,
        shard_150: 150,
        shard_151: 151,
        shard_152: 152,
        shard_153: 153,
        shard_154: 154,
        shard_155: 155,
        shard_156: 156,
        shard_157: 157,
        shard_158: 158,
        shard_159: 159,
    }
}
