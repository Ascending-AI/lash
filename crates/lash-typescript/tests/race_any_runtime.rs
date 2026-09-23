//! `Promise.race` / `Promise.any` at the ability boundary (ADR 0099 §10, §11).
//!
//! The VM's half of the contract, stated against a scripted host: what one
//! aggregate asks the host for — its consumer mode, its unique leaves, the
//! immediate-prefix boundary — and how it reads each arm of the reply. The
//! product host's half is exercised end to end by the aggregate oracle.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-expect-in-tests only exempts #[test] functions, and the scripted host around them is test code too"
)]

use lashlang::{
    AbilityOp, AbilityResult, AggregateConsumer, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ResourceOperationBatchLeaf, ResourceOperationBatchResult,
    ResourceOperationResult, State, Value,
};
use std::sync::Mutex;

/// What one aggregate asked for.
#[derive(Debug, PartialEq)]
struct Asked {
    consumer: AggregateConsumer,
    leaves: Vec<&'static str>,
    settled_value_after: Option<usize>,
}

/// Answers every aggregate with a reply the case scripts, and records what
/// each one asked for.
struct ScriptedHost {
    reply: Box<dyn Fn(usize) -> ResourceOperationBatchResult + Send + Sync>,
    asked: Mutex<Vec<Asked>>,
    /// Answer every aggregate on the host-control channel instead, the way
    /// the product host answers a settlement read that failed on store I/O.
    /// A flag rather than a second host type: `lashlang::execute` is generic
    /// over its host, and a second instantiation doubles this binary's
    /// compile.
    host_control_failure: Option<&'static str>,
}

impl ScriptedHost {
    fn new(reply: impl Fn(usize) -> ResourceOperationBatchResult + Send + Sync + 'static) -> Self {
        Self {
            reply: Box::new(reply),
            asked: Mutex::new(Vec::new()),
            host_control_failure: None,
        }
    }

    fn failing_on_host_control(message: &'static str) -> Self {
        Self {
            host_control_failure: Some(message),
            ..Self::new(|_| unreachable!("a host-control failure answers no aggregate"))
        }
    }

    fn asked(&self) -> Vec<Asked> {
        std::mem::take(&mut *self.asked.lock().expect("asked lock"))
    }
}

impl ExecutionHost for ScriptedHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(_) if self.host_control_failure.is_some() => Err(
                ExecutionHostError::new(self.host_control_failure.unwrap_or_default()),
            ),
            AbilityOp::ResourceOperationBatch(batch) => {
                self.asked.lock().expect("asked lock").push(Asked {
                    consumer: batch.consumer,
                    leaves: batch
                        .leaves
                        .iter()
                        .map(|leaf| match leaf {
                            ResourceOperationBatchLeaf::Operation(_) => "tool",
                            ResourceOperationBatchLeaf::Timer(_) => "timer",
                        })
                        .collect(),
                    settled_value_after: batch.settled_value_after,
                });
                Ok(AbilityResult::ResourceOperationBatch((self.reply)(
                    batch.leaves.len(),
                )))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

fn run(source: &str, host: &ScriptedHost) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let compiled = lash_typescript::compile(source).unwrap_or_else(|error| panic!("{error}"));
    futures::executor::block_on(lashlang::execute(&compiled, &mut State::new(), host))
}

fn finished(outcome: ExecutionOutcome) -> serde_json::Value {
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish, got {outcome:?}");
    };
    json(&value)
}

/// The finished value as JSON, for the plain shapes these cases finish with.
fn json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null | Value::Undefined => serde_json::Value::Null,
        Value::Bool(value) => serde_json::json!(value),
        Value::Number(value) => serde_json::json!(value),
        Value::String(value) => serde_json::json!(value.as_str()),
        Value::List(values) | Value::Tuple(values) => {
            serde_json::Value::Array(values.iter().map(json).collect())
        }
        Value::Record(record) => serde_json::Value::Object(
            record
                .iter()
                .map(|(key, value)| (key.to_string(), json(value)))
                .collect(),
        ),
        other => panic!("unexpected finished value {other:?}"),
    }
}

fn value(json: serde_json::Value) -> ResourceOperationResult {
    ResourceOperationResult::Value(lashlang::from_json(json))
}

fn rejection(message: &str) -> ResourceOperationResult {
    ResourceOperationResult::Error(ExecutionHostError::new(message))
}

#[test]
fn a_race_asks_for_its_first_settlement_and_resolves_with_the_selected_leaf() {
    let host = ScriptedHost::new(|_| ResourceOperationBatchResult::Selected {
        leaf: 1,
        result: value(serde_json::json!({ "id": "b" })),
    });
    let outcome = run(
        "finish(await Promise.race([web.fetch({ id: 'a' }), web.fetch({ id: 'b' })]));",
        &host,
    )
    .expect("the race resolves");
    assert_eq!(finished(outcome), serde_json::json!({ "id": "b" }));
    assert_eq!(
        host.asked(),
        vec![Asked {
            consumer: AggregateConsumer::Race,
            leaves: vec!["tool", "tool"],
            settled_value_after: None,
        }]
    );
}

#[test]
fn a_selected_rejection_rejects_the_race() {
    let host = ScriptedHost::new(|_| ResourceOperationBatchResult::Selected {
        leaf: 0,
        result: rejection("first to settle"),
    });
    let outcome = run(
        "try { await Promise.race([web.fetch({}), web.fetch({})]); finish('resolved'); } catch (e) { finish(e.message); }",
        &host,
    )
    .expect("the cell catches the rejection");
    assert!(
        finished(outcome)
            .as_str()
            .is_some_and(|message| message.contains("first to settle"))
    );
}

/// §10 L5 and §11 clause 3: a plain operand answers ahead of any dispatched
/// settlement, but the pending leaves are still sent to the host, with the
/// boundary naming how many leaves precede the plain value in source order.
#[test]
fn a_plain_operand_decides_after_its_pending_siblings_are_admitted() {
    for (operands, boundary) in [("7, web.fetch({})", 0), ("web.fetch({}), 7", 1)] {
        let host = ScriptedHost::new(|_| ResourceOperationBatchResult::SettledValue);
        let outcome = run(&format!("finish(await Promise.race([{operands}]));"), &host)
            .expect("the plain value decides");
        assert_eq!(finished(outcome), serde_json::json!(7.0), "{operands}");
        assert_eq!(
            host.asked(),
            vec![Asked {
                consumer: AggregateConsumer::Race,
                leaves: vec!["tool"],
                settled_value_after: Some(boundary),
            }],
            "{operands}: the pending leaf is admitted before the value answers"
        );
    }
}

#[test]
fn a_race_of_plain_values_needs_no_host() {
    let host = ScriptedHost::new(|_| unreachable!("no pending operand, no host call"));
    let outcome = run("finish(await Promise.race([1, 2]));", &host).expect("resolves");
    assert_eq!(finished(outcome), serde_json::json!(1.0));
    assert!(host.asked().is_empty());
}

/// §11 clause 5: `Promise.race([])` never settles; the host ends the cell with
/// the typed terminal, which no `catch` can intercept.
#[test]
fn racing_nothing_ends_the_cell_uncaught() {
    let host = ScriptedHost::new(|_| unreachable!("an empty race opens nothing"));
    let error = run(
        "try { await Promise.race([]); finish('resolved'); } catch (e) { finish('caught'); }",
        &host,
    )
    .expect_err("the unsettled await ends the cell");
    assert!(
        matches!(
            error,
            lashlang::RuntimeError::AggregateAwaitUnsettled { .. }
        ),
        "{error:?}"
    );
    assert!(host.asked().is_empty());
}

/// §10 L2, L4 and §11 clauses 1 and 8: one execution per unique handle, one
/// error per input position, in input order.
#[test]
fn an_exhausted_any_rejects_with_input_ordered_errors_for_every_position() {
    let host = ScriptedHost::new(|leaves| {
        assert_eq!(leaves, 2, "a handle written twice is one leaf");
        ResourceOperationBatchResult::ExhaustedRejections(vec![
            ExecutionHostError::new("rejected p"),
            ExecutionHostError::new("rejected q"),
        ])
    });
    let outcome = run(
        "const p = web.fetch({ id: 'p' });
try {
  await Promise.any([p, web.fetch({ id: 'q' }), p]);
  finish('resolved');
} catch (e) {
  finish({ name: e.name, messages: e.errors.map((error) => error.message) });
}",
        &host,
    )
    .expect("the cell catches the AggregateError");
    let value = finished(outcome);
    assert_eq!(value["name"], serde_json::json!("AggregateError"));
    let messages = value["messages"]
        .as_array()
        .expect("errors")
        .iter()
        .map(|message| message.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3);
    for (message, id) in messages.iter().zip(["p", "q", "p"]) {
        assert!(
            message.contains(&format!("rejected {id}")),
            "{message} is {id}'s rejection"
        );
    }
    assert_eq!(host.asked()[0].consumer, AggregateConsumer::Any);
}

/// §10 L3: an aggregate's `Err` is the host-control channel, never a leaf
/// rejection. The cell's `catch` must not see it — a caught infrastructure
/// failure would commit a fallback value a redrive answers differently.
#[test]
fn a_host_control_failure_ends_the_cell_uncaught() {
    for aggregate in [
        "Promise.any",
        "Promise.race",
        "Promise.all",
        "Promise.allSettled",
    ] {
        let error = run(
            &format!(
                "try {{ await {aggregate}([web.fetch({{}}), web.fetch({{}})]); finish('resolved'); }} \
                 catch (e) {{ finish('caught'); }}"
            ),
            &ScriptedHost::failing_on_host_control("settlement read failed: disk I/O error"),
        )
        .expect_err("a host-control failure ends the cell");
        assert!(
            error.to_string().contains("disk I/O error"),
            "{aggregate}: the terminal names the host failure: {error}"
        );
    }
}

#[test]
fn any_of_nothing_rejects_with_an_empty_aggregate_error() {
    let host = ScriptedHost::new(|_| unreachable!("an empty any opens nothing"));
    let outcome = run(
        "try { await Promise.any([]); finish('resolved'); } catch (e) { finish({ name: e.name, count: e.errors.length }); }",
        &host,
    )
    .expect("catchable");
    assert_eq!(
        finished(outcome),
        serde_json::json!({ "name": "AggregateError", "count": 0.0 })
    );
}

/// §11 clause 4: an unawaited `sleep(ms)` is a timer leaf of the aggregate
/// that awaits it, and its fulfilment value is `undefined`.
#[test]
fn a_timer_leaf_rides_the_aggregate_and_fulfils_with_undefined() {
    let host = ScriptedHost::new(|_| ResourceOperationBatchResult::Selected {
        leaf: 1,
        result: ResourceOperationResult::Value(Value::Undefined),
    });
    let outcome = run(
        "const winner = await Promise.race([web.fetch({}), sleep(5)]); finish(winner === undefined);",
        &host,
    )
    .expect("the timer wins");
    assert_eq!(finished(outcome), serde_json::json!(true));
    assert_eq!(
        host.asked(),
        vec![Asked {
            consumer: AggregateConsumer::Race,
            leaves: vec!["tool", "timer"],
            settled_value_after: None,
        }]
    );
}

#[test]
fn a_timer_nothing_awaits_fails_at_cell_end_like_a_pending_tool() {
    let host = ScriptedHost::new(|_| unreachable!("nothing is awaited"));
    let error = run("sleep(5); finish(1);", &host).expect_err("an abandoned timer");
    assert!(
        matches!(error, lashlang::RuntimeError::PendingTool { .. }),
        "{error:?}"
    );
}

/// `Promise.all` asks for its first rejection and resumes with it; the host
/// answers without waiting for the other leaves (ADR 0062 deviation 15 is
/// retired). `allSettled` asks for every result.
#[test]
fn promise_all_and_all_settled_ask_for_their_own_consumer_modes() {
    let host = ScriptedHost::new(|_| ResourceOperationBatchResult::Selected {
        leaf: 1,
        result: rejection("first consumed"),
    });
    let outcome = run(
        "try { await Promise.all([web.fetch({}), web.fetch({})]); finish('resolved'); } catch (e) { finish(e.message); }",
        &host,
    )
    .expect("catchable");
    assert!(
        finished(outcome)
            .as_str()
            .is_some_and(|message| message.contains("first consumed"))
    );
    assert_eq!(host.asked()[0].consumer, AggregateConsumer::All);

    let host = ScriptedHost::new(|leaves| {
        ResourceOperationBatchResult::AllResults(
            (0..leaves).map(|_| value(serde_json::json!(1))).collect(),
        )
    });
    run(
        "finish(await Promise.allSettled([web.fetch({}), web.fetch({})]));",
        &host,
    )
    .expect("settles");
    assert_eq!(host.asked()[0].consumer, AggregateConsumer::AllSettled);
}
