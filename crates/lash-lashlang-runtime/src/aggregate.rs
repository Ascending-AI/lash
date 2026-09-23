//! The one mapping from a Lashlang aggregate to the runtime's durable group
//! and back (ADR 0099 §10, §11; FIG-3397), shared by both bridges.
//!
//! A bridge resolves each of its leaves in its own way — a TypeScript runtime
//! value journaled in place, a trigger operation, a leaf refused before
//! dispatch, a tool call, a timer — and hands the result here as a
//! [`BridgeAggregateLeaf`]. This module forms one
//! [`ToolAggregateRequest`](lash_core::session::ToolAggregateRequest), runs it
//! under the VM's consumer mode, and turns the answer into the VM's reply
//! algebra. A bridge's already-resolved leaves are the immediate prefix
//! (§10 L5); host-control failures come back as `Err`, never as a leaf
//! rejection (L3).

use lashlang::{ExecutionHostError, ResourceOperationBatchResult, ResourceOperationResult, Value};

/// One unique leaf of an aggregate, as a bridge resolved it.
pub enum BridgeAggregateLeaf {
    /// Settled by the bridge before the aggregate formed.
    Settled(Result<Value, ExecutionHostError>),
    /// A tool call to admit as a group child.
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
    consumer: lashlang::AggregateConsumer,
    settled_value_after: Option<usize>,
    site: u64,
    occurrence: u64,
    leaves: Vec<BridgeAggregateLeaf>,
    mut tool_value: impl FnMut(
        usize,
        lash_core::session::ToolInvocationReply,
    ) -> Result<Value, ExecutionHostError>,
) -> Result<ResourceOperationBatchResult, ExecutionHostError> {
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
            site,
            occurrence: lash_core::session::ToolGroupOccurrence::Opener(occurrence),
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
                results.push(ResourceOperationResult::from_result(result_of(
                    leaf, reply,
                )?));
            }
            Ok(ResourceOperationBatchResult::AllResults(results))
        }
        lash_core::session::ToolAggregateOutcome::Selected { leaf, reply } => {
            Ok(ResourceOperationBatchResult::Selected {
                leaf,
                result: ResourceOperationResult::from_result(result_of(leaf, reply)?),
            })
        }
        lash_core::session::ToolAggregateOutcome::SettledValue => {
            Ok(ResourceOperationBatchResult::SettledValue)
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
            Ok(ResourceOperationBatchResult::ExhaustedRejections(errors))
        }
        lash_core::session::ToolAggregateOutcome::HostControl(message) => {
            Err(ExecutionHostError::new(message))
        }
    }
}

/// The host-level message for a VM terminal that is a host lifetime contract
/// rather than a program error: an await nothing can settle ends the
/// execution with the typed [`lash_core::RuntimeErrorCode::AggregateAwaitUnsettled`]
/// (ADR 0099 §11 clause 5). `None` for every other failure.
pub fn host_lifetime_failure_message(error: &lashlang::RuntimeError) -> Option<String> {
    matches!(
        error,
        lashlang::RuntimeError::AggregateAwaitUnsettled { .. }
    )
    .then(|| {
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AggregateAwaitUnsettled,
            error.to_string(),
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
