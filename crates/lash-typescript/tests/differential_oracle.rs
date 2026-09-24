// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const README: &str = include_str!("../README.md");

/// The shard name of every `expectations/<shard>.tsv` and its text, sorted by
/// shard. Reading the directory is what lets a new `findings/<shard>.txt`
/// join the table without an edit to any shared file.
fn expectation_shards() -> Vec<(String, String)> {
    shard_files("expectations", ".tsv")
}

/// The shard name of every `findings/<shard>.txt` and its lines, sorted by
/// shard.
fn finding_shards() -> BTreeMap<String, Vec<String>> {
    shard_files("findings", ".txt")
        .into_iter()
        .map(|(shard, text)| (shard, text.lines().map(str::to_owned).collect()))
        .collect()
}

fn shard_files(directory: &str, extension: &str) -> Vec<(String, String)> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/differential")
        .join(directory);
    let mut names = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .expect("a shard entry")
                .file_name()
                .into_string()
                .expect("UTF-8 shard name")
        })
        .filter(|name| name.ends_with(extension))
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let shard = name.strip_suffix(extension).expect("filtered").to_owned();
            let text = std::fs::read_to_string(directory.join(&name))
                .unwrap_or_else(|error| panic!("read {name}: {error}"));
            (shard, text)
        })
        .collect()
}

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected ability in differential oracle",
            )),
        }
    }
}

#[test]
fn committed_node_expectations_match_the_accepted_dialect() {
    let mut shard_counts = BTreeMap::<String, usize>::new();
    let mut failures = Vec::new();
    for (shard, table) in expectation_shards() {
        for (line_number, line) in table.lines().enumerate().skip(1) {
            let columns = line.split('\t').collect::<Vec<_>>();
            assert_eq!(
                columns.len(),
                6,
                "malformed oracle row {shard} {}",
                line_number + 1
            );
            let [
                lane,
                index,
                disposition,
                expression_json,
                expected_json,
                diagnostic,
            ] = columns.as_slice()
            else {
                unreachable!()
            };
            assert_eq!(*lane, shard, "a shard's rows carry its name");
            *shard_counts.entry(shard.clone()).or_default() += 1;
            let expression: String =
                serde_json::from_str(expression_json).expect("expression JSON");
            if let Err(failure) = check_row(&expression, disposition, expected_json, diagnostic) {
                failures.push(format!("differential:{lane}:{index}: {failure}"));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        shard_counts.get("findings").copied().unwrap_or_default() >= 10,
        "every fixed semantic finding needs an oracle row"
    );
}

/// Links `source` the way a cell is admitted: against a host, with the
/// linker's checks. The oracle executes a row through the VM either way; a row
/// the linker refuses is recorded as such (`accept-unlinked`), so no row claims
/// a cell accepts what a cell refuses.
fn link_cell(source: &str) -> Result<(), String> {
    lash_typescript::link(source, &lashlang::testing::harness::test_environment())
        .map(drop)
        .map_err(|error| error.code.as_str().to_string())
}

/// One row's verdict; every row is checked so a failure lists them all.
fn check_row(
    expression: &str,
    disposition: &str,
    expected_json: &str,
    diagnostic: &str,
) -> Result<(), String> {
    if disposition == "reject" {
        let error = lash_typescript::testing::compile(&format!("finish({expression});"))
            .err()
            .ok_or_else(|| format!("`{expression}`: registered unsupported expression compiled"))?;
        return (error.code.as_str() == diagnostic)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "`{expression}`: rejects with {}, not {diagnostic}",
                    error.code.as_str()
                )
            });
    }
    if disposition == "runtime-reject" {
        let program = lash_typescript::testing::compile(&format!("finish({expression});"))
            .map_err(|error| {
                format!("`{expression}`: runtime-only deviation must compile: {error}")
            })?;
        let error =
            futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                .err()
                .ok_or_else(|| format!("`{expression}`: registered runtime deviation ran"))?;
        return error
            .to_string()
            .contains(diagnostic)
            .then_some(())
            .ok_or_else(|| format!("`{expression}`: {error} does not name {diagnostic}"));
    }
    if disposition == "open-defect" {
        // A reported defect: the row pins that the dialect still disagrees
        // with Node, so the fix fails it until the row is promoted.
        let expected: String = serde_json::from_str(expected_json).expect("expected JSON");
        let source = format!("finish(`${{{expression}}}`);");
        let answer = lash_typescript::testing::compile(&source)
            .ok()
            .and_then(|program| {
                futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                    .ok()
            });
        if !README.contains(&format!("`{diagnostic}`")) {
            return Err(format!(
                "`{expression}`: open defect `{diagnostic}` is not in the README's list"
            ));
        }
        return (answer != Some(ExecutionOutcome::Finished(Value::String(expected.into()))))
            .then_some(())
            .ok_or_else(|| {
                format!("`{expression}`: open defect `{diagnostic}` now matches Node; promote it")
            });
    }
    let source = format!("finish(`${{{expression}}}`);");
    match (disposition, diagnostic, link_cell(&source)) {
        ("accept", "-", Ok(())) => {}
        ("accept-unlinked", code, Err(refused)) if refused == code => {}
        ("accept", "-", Err(refused)) => {
            return Err(format!(
                "`{expression}`: a cell refuses it at link with {refused}; record it as accept-unlinked"
            ));
        }
        ("accept-unlinked", code, linked) => {
            return Err(format!(
                "`{expression}`: accept-unlinked names {code}, but linking answers {linked:?}"
            ));
        }
        _ => {
            return Err(format!(
                "`{expression}`: unknown disposition {disposition} {diagnostic}"
            ));
        }
    }
    let expected: String = serde_json::from_str(expected_json).expect("expected JSON");
    let program = lash_typescript::testing::compile(&source)
        .map_err(|error| format!("compile `{expression}`: {error}"))?;
    let outcome =
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
            .map_err(|error| format!("execute `{expression}`: {error}"))?;
    (outcome == ExecutionOutcome::Finished(Value::String(expected.clone().into())))
        .then_some(())
        .ok_or_else(|| format!("`{expression}`: {outcome:?}, Node answers {expected:?}"))
}

/// The table is the generator's output, row for row, so regeneration stays a
/// byte-identical step (ADR 0062).
///
/// Nothing in CI runs Node, so what is checkable is the part of the table the
/// generator does not compute: each row's expression is its shard file's line
/// at that index, and each row's disposition and diagnostic are the ones
/// `dispositions.tsv` names for its expression. Both had drifted: FIG-3166
/// hand-edited seven rows to `runtime-reject` and rewrote one expression, so
/// the next regeneration silently reverted them.
#[test]
fn committed_table_is_what_the_generator_writes() {
    let lanes = finding_shards();
    let dispositions = include_str!("differential/dispositions.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let [expression, disposition, diagnostic] = line.split('\t').collect::<Vec<_>>()[..]
            else {
                panic!("malformed disposition row: {line}")
            };
            let expression: String =
                serde_json::from_str(expression).expect("disposition expression JSON");
            (expression, (disposition, diagnostic))
        })
        .collect::<BTreeMap<_, _>>();
    let mut used = std::collections::BTreeSet::new();
    let mut rows = BTreeMap::<String, usize>::new();
    for (shard, table) in expectation_shards() {
        let lines = lanes
            .get(&shard)
            .unwrap_or_else(|| panic!("expectations/{shard}.tsv has no findings/{shard}.txt"));
        for line in table.lines().skip(1) {
            let columns = line.split('\t').collect::<Vec<_>>();
            let [lane, index, disposition, expression, _, diagnostic] = columns[..] else {
                panic!("malformed oracle row: {line}")
            };
            assert_eq!(*lane, shard, "a shard's rows carry its name");
            let expression: String = serde_json::from_str(expression).expect("expression JSON");
            let index = index.parse::<usize>().expect("row index");
            assert_eq!(
                lines.get(index - 1).map(String::as_str),
                Some(expression.as_str()),
                "{lane} row {index} is not its shard file's line"
            );
            *rows.entry(shard.clone()).or_default() += 1;
            let expected = dispositions
                .get(&expression)
                .copied()
                .unwrap_or(("accept", "-"));
            assert_eq!(
                (disposition, diagnostic),
                expected,
                "{lane} row {index}: `{expression}`"
            );
            used.insert(expression);
        }
    }
    for (lane, lines) in &lanes {
        assert_eq!(
            rows.get(lane),
            Some(&lines.len()),
            "{lane}: findings/{lane}.txt has no matching expectations/{lane}.tsv rows"
        );
    }
    let unused = dispositions
        .keys()
        .filter(|expression| !used.contains(*expression))
        .collect::<Vec<_>>();
    assert!(unused.is_empty(), "dispositions no row uses: {unused:?}");
}
