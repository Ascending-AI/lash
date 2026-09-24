// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const EXPECTATIONS: &str = include_str!("differential/expectations.tsv");
const README: &str = include_str!("../README.md");

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
    let mut lane_counts = BTreeMap::<&str, usize>::new();
    let mut failures = Vec::new();
    for (line_number, line) in EXPECTATIONS.lines().enumerate().skip(1) {
        let columns = line.split('\t').collect::<Vec<_>>();
        assert_eq!(columns.len(), 6, "malformed oracle row {}", line_number + 1);
        let [
            lane,
            _index,
            disposition,
            expression_json,
            expected_json,
            diagnostic,
        ] = columns.as_slice()
        else {
            unreachable!()
        };
        *lane_counts.entry(lane).or_default() += 1;
        let expression: String = serde_json::from_str(expression_json).expect("expression JSON");
        if let Err(failure) = check_row(&expression, disposition, expected_json, diagnostic) {
            failures.push(failure);
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(lane_counts.get("opus"), Some(&163));
    assert_eq!(lane_counts.get("sol"), Some(&124));
    assert!(
        lane_counts.get("findings").copied().unwrap_or_default() >= 10,
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

/// The register quotes the corpus's size, and a quoted number decays.
///
/// It had already decayed twice. First the register claimed 310 rows and 237
/// distinct expressions while the table held 345 and 272; then a check that
/// pinned only the paragraph's *first* two numbers let the second pair drift to
/// 448 and 521, numbers matching nothing at all. Pinning a subset is what let
/// the rest rot, so this reads every number in the paragraph and requires the
/// whole sequence — total, distinct, distinct, total — to be the table's own.
#[test]
fn committed_row_counts_match_the_register() {
    let table = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/differential/expectations.tsv"
    ))
    .expect("the expectation table is readable");
    let rows = table.lines().skip(1).filter(|line| !line.is_empty());
    let total = rows.clone().count();
    let distinct = rows
        .filter_map(|line| line.split('\t').nth(3))
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    let register = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("the register is readable");
    let paragraph = register
        .split("The Node differential table carries ")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("the register states the table's size");
    let claimed = paragraph
        .split(|character: char| !character.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .map(|token| token.parse::<usize>().expect("a register count parses"))
        .collect::<Vec<_>>();

    assert_eq!(
        claimed,
        vec![total, distinct, distinct, total],
        "the register's counts are {claimed:?}; the table has {total} rows and \
         {distinct} distinct expressions"
    );
}

/// The table is the generator's output, row for row, so regeneration stays a
/// byte-identical step (ADR 0062).
///
/// Nothing in CI runs Node, so what is checkable is the part of the table the
/// generator does not compute: each row's expression is its lane file's line
/// at that index, and each row's disposition and diagnostic are the ones
/// `dispositions.tsv` names for its expression. Both had drifted: FIG-3166
/// hand-edited seven rows to `runtime-reject` and rewrote one expression, so
/// the next regeneration silently reverted them.
#[test]
fn committed_table_is_what_the_generator_writes() {
    let lanes = [
        ("opus", include_str!("differential/opus-expressions.txt")),
        ("sol", include_str!("differential/sol-expressions.txt")),
        (
            "findings",
            include_str!("differential/findings-expressions.txt"),
        ),
    ]
    .into_iter()
    .map(|(lane, text)| (lane, text.lines().collect::<Vec<_>>()))
    .collect::<BTreeMap<_, _>>();
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
    let mut rows = BTreeMap::<&str, usize>::new();
    for line in EXPECTATIONS.lines().skip(1) {
        let columns = line.split('\t').collect::<Vec<_>>();
        let [lane, index, disposition, expression, _, diagnostic] = columns[..] else {
            panic!("malformed oracle row: {line}")
        };
        let expression: String = serde_json::from_str(expression).expect("expression JSON");
        let index = index.parse::<usize>().expect("row index");
        assert_eq!(
            lanes[lane].get(index - 1).copied(),
            Some(expression.as_str()),
            "{lane} row {index} is not its lane file's line"
        );
        *rows.entry(lane).or_default() += 1;
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
    for (lane, lines) in &lanes {
        assert_eq!(rows.get(lane), Some(&lines.len()), "{lane} rows");
    }
    let unused = dispositions
        .keys()
        .filter(|expression| !used.contains(*expression))
        .collect::<Vec<_>>();
    assert!(unused.is_empty(), "dispositions no row uses: {unused:?}");
}
