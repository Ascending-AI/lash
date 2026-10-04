//! The one mapping from a Lashlang aggregate to the logical Run
//! and back (ADR 0099 §10, §11; FIG-3397), shared by both bridges.
//!
//! A bridge resolves each of its leaves in its own way — a language runtime
//! value journaled in place, a trigger operation, a leaf refused before
//! dispatch, a tool call, a timer — and hands the result here as a
//! [`BridgeAggregateLeaf`]. This module forms one
//! [`ToolAggregateRequest`](lash_core::session::ToolAggregateRequest), runs it
//! under the VM's consumer mode, and turns the answer into the VM's reply
//! algebra. A bridge's already-resolved leaves are the immediate prefix
//! (§10 L5); host-control failures come back as `Err`, never as a leaf
//! rejection (L3).

use lashlang::{
    ExecutionHostError, ResourceOperationBatchOutcome, ResourceOperationOutcome, Value,
};

/// One unique leaf of an aggregate, as a bridge resolved it.
pub enum BridgeAggregateLeaf {
    /// Settled by the bridge before the aggregate formed.
    Settled(Result<Value, ExecutionHostError>),
    /// A tool call to admit in the logical Run.
    Tool(lash_core::session::ToolInvocation),
    /// A timer from an unawaited `sleep(ms)`.
    Timer { duration_ms: u64 },
}

/// Runs one aggregate through `ctx` and answers the VM.
///
/// `tool_value` turns one settled tool reply into its Lashlang value, with
/// whatever bookkeeping the bridge keeps per reply; it is called once for
/// every reply the answer carries, in leaf order, and never for a loser.
pub async fn settle_bridge_aggregate(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    command: &lash_core::CommandReplayKey,
    consumer: lashlang::AggregateConsumer,
    settled_value_after: Option<usize>,
    leaves: Vec<BridgeAggregateLeaf>,
    mut tool_value: impl FnMut(
        usize,
        lash_core::session::ToolInvocationReply,
    ) -> Result<Value, ExecutionHostError>,
) -> Result<ResourceOperationBatchOutcome, ExecutionHostError> {
    let mut settled = Vec::with_capacity(leaves.len());
    let mut request_leaves = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        match leaf {
            BridgeAggregateLeaf::Settled(result) => {
                request_leaves.push(lash_core::session::ToolAggregateLeaf::Settled {
                    fulfilled: result.is_ok(),
                });
                settled.push(Some(result));
            }
            BridgeAggregateLeaf::Tool(call) => {
                request_leaves.push(lash_core::session::ToolAggregateLeaf::Tool(call));
                settled.push(None);
            }
            BridgeAggregateLeaf::Timer { duration_ms } => {
                request_leaves.push(lash_core::session::ToolAggregateLeaf::Timer { duration_ms });
                settled.push(None);
            }
        }
    }
    let outcome = ctx
        .call_tool_aggregate(lash_core::session::ToolAggregateRequest {
            leaves: request_leaves,
            consumer: match consumer {
                lashlang::AggregateConsumer::AllSettled => {
                    lash_core::session::ToolAggregateConsumer::AllSettled
                }
                lashlang::AggregateConsumer::All => lash_core::session::ToolAggregateConsumer::All,
                lashlang::AggregateConsumer::Race => {
                    lash_core::session::ToolAggregateConsumer::Race
                }
                lashlang::AggregateConsumer::Any => lash_core::session::ToolAggregateConsumer::Any,
            },
            settled_value_after,
            command: command.clone(),
        })
        .await;
    let mut result_of = |leaf: usize,
                         reply: Option<lash_core::session::ToolAggregateLeafReply>|
     -> Result<Result<Value, ExecutionHostError>, ExecutionHostError> {
        if let Some(result) = settled.get_mut(leaf).and_then(Option::take) {
            return Ok(result);
        }
        match reply {
            Some(lash_core::session::ToolAggregateLeafReply::Tool(reply)) => {
                Ok(tool_value(leaf, *reply))
            }
            Some(lash_core::session::ToolAggregateLeafReply::Timer) => Ok(Ok(Value::Undefined)),
            None => Err(ExecutionHostError::new(format!(
                "aggregate leaf {leaf} was answered without a settlement"
            ))),
        }
    };
    match outcome {
        lash_core::session::ToolAggregateOutcome::AllResults(replies) => {
            let mut results = Vec::with_capacity(replies.len());
            for (leaf, reply) in replies.into_iter().enumerate() {
                results.push(ResourceOperationOutcome::from_result(result_of(
                    leaf, reply,
                )?));
            }
            Ok(ResourceOperationBatchOutcome::AllResults(results))
        }
        lash_core::session::ToolAggregateOutcome::Selected { leaf, reply } => {
            Ok(ResourceOperationBatchOutcome::Selected {
                leaf,
                result: ResourceOperationOutcome::from_result(result_of(leaf, reply)?),
            })
        }
        lash_core::session::ToolAggregateOutcome::SettledValue => {
            Ok(ResourceOperationBatchOutcome::SettledValue)
        }
        lash_core::session::ToolAggregateOutcome::ExhaustedRejections(replies) => {
            let mut errors = Vec::with_capacity(replies.len());
            for (leaf, reply) in replies.into_iter().enumerate() {
                match result_of(leaf, reply)? {
                    Err(error) => errors.push(error),
                    Ok(_) => {
                        return Err(ExecutionHostError::new(format!(
                            "aggregate leaf {leaf} fulfilled, yet the aggregate reported every \
                             leaf rejected"
                        )));
                    }
                }
            }
            Ok(ResourceOperationBatchOutcome::ExhaustedRejections(errors))
        }
        lash_core::session::ToolAggregateOutcome::HostControl(message) => {
            Err(ExecutionHostError::new(message))
        }
        lash_core::session::ToolAggregateOutcome::ToolCallLimitExceeded(exceeded) => {
            Err(ExecutionHostError::from_tool_failure(
                &tool_call_limit_failure(exceeded),
                command.to_string(),
            ))
        }
    }
}

/// The tool failure a `max_tool_calls` refusal carries: typed by its class
/// and code, never retried, and worded by the refusal so the limit is named
/// in what the model reads (FIG-4546).
pub fn tool_call_limit_failure(
    exceeded: lash_core::ToolCallLimitExceeded,
) -> lash_core::ToolFailure {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::ResourceLimit,
        lash_core::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    );
    failure.raw = Some(lash_core::ToolValue::untrusted_json(
        serde_json::json!({ "tool_call_limit": exceeded }),
    ));
    failure
}

/// Whether a VM terminal is the session's `max_tool_calls` refusing an
/// aggregate: the program's failure, carried on the aggregate's error
/// channel with the refusal's typed class and code.
pub fn is_tool_call_limit_failure(error: &lashlang::RuntimeError) -> bool {
    matches!(
        error,
        lashlang::RuntimeError::AggregateHostControl { source }
            if source.tool_failure_code() == Some(lash_core::ToolCallLimitExceeded::CODE)
    )
}

/// The message for the VM terminal that ends an execution awaiting an
/// aggregate nothing can settle: the typed
/// [`lash_core::RuntimeErrorCode::AggregateAwaitUnsettled`], the cause, and
/// the guard the empty aggregate needs. The host ends the execution
/// uncatchably (ADR 0099 §11 clause 5), but the defect is the program's and
/// the feedback says so (FIG-4547). `None` for every other failure.
pub fn host_lifetime_failure_message(error: &lashlang::RuntimeError) -> Option<String> {
    matches!(
        error,
        lashlang::RuntimeError::AggregateAwaitUnsettled { .. }
    )
    .then(|| {
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AggregateAwaitUnsettled,
            format!("{error}; guard the empty case before awaiting it"),
        )
        .to_string()
    })
}

/// A timer leaf's duration in whole milliseconds: the same numbers and
/// duration strings `await sleep(ms)` accepts. A timer leaf is always relative
/// — its start point is its admission (ADR 0099 §11 clause 4).
pub fn timer_duration_ms(sleep: &lashlang::Sleep) -> Result<u64, ExecutionHostError> {
    match crate::bridge::process_sleep(sleep.kind, &sleep.value)? {
        lash_core::SleepSpec::For { duration_ms } => Ok(duration_ms),
        lash_core::SleepSpec::Until { .. } => Err(ExecutionHostError::new(
            "an aggregate timer is relative to its admission and cannot name a deadline",
        )),
    }
}
