use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ResourceOperation, ResourceOperationBatchResult, ResourceOperationResult, State, Value,
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Echoes each call's first argument; the `fail` operation rejects.
fn echo_or_fail(call: ResourceOperation) -> Result<Value, ExecutionHostError> {
    if call.operation == "fail" {
        return Err(ExecutionHostError::new("tool failed"));
    }
    Ok(call.args[0].clone())
}

struct Host;
impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => echo_or_fail(call).map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .into_iter()
                        .map(|call| ResourceOperationResult::from_result(echo_or_fail(call)))
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
        "await (async () => { web.fetch({id:1}); })();",
        "await Promise.all([1].map(async id => { web.fetch({id}); return id; }));",
        "await Promise.all(42);",
        "const p = web.fetch({id:1}); await p; await p;",
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

/// A `Promise.all` operand written inside another tool call's argument lowers
/// at the top await depth: its leaves are pending handles the batch settles
/// and unwraps, so a leaf rejection rejects the aggregate instead of arriving
/// as an `{ok: false}` envelope the outer call would forward as a value.
#[test]
fn nested_aggregates_unwrap_their_leaves_and_propagate_rejections() {
    assert_eq!(
        execute("finish(await web.echo({ v: await Promise.all([web.fetch({id:1}), 2]) }));")
            .unwrap(),
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({"v": [{"id":1}, 2]})))
    );
    let error = execute("finish(await web.echo({ v: await Promise.all([web.fail({id:1})]) }));")
        .expect_err("a rejected nested aggregate must reject the program");
    assert!(
        matches!(
            error,
            lashlang::RuntimeError::UnwrappedModuleOperationFailed { .. }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("tool failed"), "{error}");
    assert_eq!(
        execute(
            "try { await web.echo({ v: await Promise.all([web.fail({id:1})]) }); finish('missed'); } catch (e) { finish('caught'); }"
        )
        .unwrap(),
        ExecutionOutcome::Finished(Value::String("caught".into()))
    );
}

/// Only a handle this execution minted reaches a request slot: a hand-written
/// handle record is refused with the pending-tool code and names its problem,
/// and the live handle it tried to alias stays awaitable.
#[test]
fn a_hand_written_handle_record_is_refused_and_steals_nothing() {
    let error = execute(
        "const p = web.fetch({id:1}); const forged = { __handle__: 'tool', id: 0 }; finish(await forged);",
    )
    .expect_err("a forged handle must not settle a live request");
    let lashlang::RuntimeError::PendingTool { problem } = &error else {
        panic!("expected the typed pending-tool refusal: {error}");
    };
    assert!(
        problem.contains("not minted by this execution"),
        "{problem}"
    );
    assert_eq!(
        execute(
            "const p = web.fetch({id:1}); const forged = { __handle__: 'tool', id: 0 }; try { await forged; } catch (e) {} finish(await p);"
        )
        .unwrap(),
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({"id":1})))
    );
}

/// The repair text names what was actually awaited: a plain value is not a
/// settled handle, and a handle awaited twice is settled, not foreign.
#[test]
fn await_refusals_name_what_was_awaited() {
    let plain = execute("await 42; finish(1);").expect_err("a number is not awaitable");
    let lashlang::RuntimeError::PendingTool { problem } = &plain else {
        panic!("{plain}");
    };
    assert!(problem.contains("plain number value"), "{problem}");
    assert!(!problem.contains("already"), "{problem}");
    let twice = execute("const p = web.fetch({id:1}); await p; await p; finish(1);")
        .expect_err("a handle settles once");
    let lashlang::RuntimeError::PendingTool { problem } = &twice else {
        panic!("{twice}");
    };
    assert!(problem.contains("already awaited"), "{problem}");
}

#[derive(Default)]
struct CountingHost {
    dispatched: AtomicUsize,
}

impl ExecutionHost for CountingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) | AbilityOp::ResourceOperationBatch(_) => {
                self.dispatched.fetch_add(1, Ordering::SeqCst);
                Host.perform(op).await
            }
            other => Host.perform(other).await,
        }
    }
}

/// A pending handle inside a tool's arguments is refused before dispatch, so
/// the host never runs the call with a handle record where a value belongs.
#[test]
fn a_pending_handle_passed_as_a_tool_argument_is_refused_before_dispatch() {
    for source in [
        "const p = web.fetch({id:1}); finish(await web.echo({ inner: p }));",
        "const p = web.fetch({id:1}); finish(await web.echo([p]));",
        "const p = web.fetch({id:1}); finish(await Promise.all([web.echo({ inner: p })]));",
    ] {
        let host = CountingHost::default();
        let compiled = lash_typescript::compile(source).expect(source);
        let error =
            futures::executor::block_on(lashlang::execute(&compiled, &mut State::new(), &host))
                .expect_err(source);
        let lashlang::RuntimeError::PendingTool { problem } = &error else {
            panic!("{source}: {error}");
        };
        assert!(
            problem.contains("passed as a tool argument"),
            "{source}: {problem}"
        );
        assert_eq!(
            host.dispatched.load(Ordering::SeqCst),
            0,
            "{source}: nothing may reach the host"
        );
    }
}
