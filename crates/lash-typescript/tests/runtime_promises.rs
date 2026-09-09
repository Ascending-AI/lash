use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ResourceOperationBatchResult, ResourceOperationResult, State, Value,
};

struct Host;
impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => Ok(AbilityResult::Value(call.args[0].clone())),
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .into_iter()
                        .map(|call| ResourceOperationResult::Value(call.args[0].clone()))
                        .collect(),
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected operation")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let compiled = lash_typescript::compile(source).expect(source);
    futures::executor::block_on(lashlang::execute(&compiled, &mut State::new(), &Host))
}

#[test]
fn promise_aggregates_accept_runtime_arrays_and_mixed_values() {
    for method in ["all", "allSettled"] {
        for (setup, expression, expected) in [
            (
                "const ids = [1,2];",
                "ids.map(id => web.fetch({id}))",
                serde_json::json!([{"id":1},{"id":2}]),
            ),
            (
                "const ids = [1,2];",
                "ids.map(async id => (await web.fetch({id})).id)",
                serde_json::json!([1, 2]),
            ),
            (
                "const ids = [1,2]; const ps = ids.map(id => web.fetch({id}));",
                "ps",
                serde_json::json!([{"id":1},{"id":2}]),
            ),
            (
                "function computed() { return 7; }",
                "[web.fetch({id:1}),42,computed()]",
                serde_json::json!([{"id":1},42,7]),
            ),
        ] {
            let source = format!("{setup} finish(await Promise.{method}({expression}));");
            let expected = if method == "allSettled" {
                serde_json::Value::Array(
                    expected
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| serde_json::json!({"status":"fulfilled","value":value}))
                        .collect(),
                )
            } else {
                expected
            };
            assert_eq!(
                execute(&source).unwrap(),
                ExecutionOutcome::Finished(lashlang::from_json(expected)),
                "{source}"
            );
        }
    }
}

#[test]
fn abandoned_and_settled_handles_are_loud_errors() {
    for source in [
        "web.fetch({id:1});",
        "const p = web.fetch({id:1}); finish(42);",
        "await 42;",
    ] {
        assert!(
            matches!(
                execute(source),
                Err(lashlang::RuntimeError::PendingTool { .. })
            ),
            "{source}"
        );
    }
}
