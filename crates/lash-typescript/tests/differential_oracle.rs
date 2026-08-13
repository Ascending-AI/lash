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
        let expected: String = serde_json::from_str(expected_json).expect("expected JSON");

        if *disposition == "reject" {
            let error = lash_typescript::compile(&format!("finish({expression});"))
                .expect_err("registered unsupported expression must reject");
            assert_eq!(error.code.as_str(), *diagnostic, "expression: {expression}");
            continue;
        }
        assert_eq!(*disposition, "accept", "unknown disposition");
        assert!(diagnostic.is_empty(), "accepted rows have no diagnostic");
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
