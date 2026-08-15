use std::collections::BTreeMap;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const EXPECTATIONS: &str = include_str!("differential/expectations.tsv");

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

        if *disposition == "reject" {
            let error = lash_typescript::compile(&format!("finish({expression});"))
                .expect_err("registered unsupported expression must reject");
            assert_eq!(error.code.as_str(), *diagnostic, "expression: {expression}");
            continue;
        }
        if *disposition == "runtime-reject" {
            let program = lash_typescript::compile(&format!("finish({expression});"))
                .expect("runtime-only deviation must compile");
            let error =
                futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                    .expect_err("registered runtime deviation must reject");
            assert!(
                error.to_string().contains(diagnostic),
                "expression: {expression}; error: {error}"
            );
            continue;
        }
        assert_eq!(*disposition, "accept", "unknown disposition");
        assert_eq!(*diagnostic, "-", "accepted rows have no diagnostic");
        let expected: String = serde_json::from_str(expected_json).expect("expected JSON");
        let source = format!("finish(`${{{expression}}}`);");
        let program = lash_typescript::compile(&source)
            .unwrap_or_else(|error| panic!("compile `{expression}`: {error}"));
        let outcome =
            futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                .unwrap_or_else(|error| panic!("execute `{expression}`: {error}"));
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::String(expected.into())),
            "expression: {expression}"
        );
    }

    assert_eq!(lane_counts.get("opus"), Some(&163));
    assert_eq!(lane_counts.get("sol"), Some(&124));
    assert!(
        lane_counts.get("findings").copied().unwrap_or_default() >= 10,
        "every fixed semantic finding needs an oracle row"
    );
}

/// The register quotes the corpus's size, and a quoted number decays.
///
/// It had already decayed once — the register claimed 310 rows and 237 distinct
/// expressions while the table held 345 and 272 — which is the same failure as
/// the hand-maintained method inventory: a count restated in prose beside the
/// thing it counts, with nothing making them agree. This asserts the register's
/// two numbers against the table itself.
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
    let claim = register
        .split("The Node differential table carries ")
        .nth(1)
        .expect("the register states the table's size");
    let claimed_total = claim
        .split(' ')
        .next()
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the register states a row count");
    let claimed_distinct = claim
        .split("of which ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the register states a distinct-expression count");

    assert_eq!(
        claimed_total, total,
        "the register claims {claimed_total} rows; the table has {total}"
    );
    assert_eq!(
        claimed_distinct, distinct,
        "the register claims {claimed_distinct} distinct expressions; the table has {distinct}"
    );
}
