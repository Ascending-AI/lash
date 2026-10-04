//! The tool-batch surface of [`RuntimeExecutionContext`].
//!
//! One source-ordered batch is admitted and consumed by the logical owner's
//! Run. Its replies remain in caller order beside the durable settlement
//! order used by settlement-selecting consumers.

use super::*;

use super::group::{ToolAggregateConsumer, tool_call_limit_failure};

impl RuntimeExecutionContext<'_> {
    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    pub(super) fn tool_batch_invocation(&self, batch_id: &str) -> crate::RuntimeEffectInvocation {
        let suffix = format!("tool-batch:{batch_id}");
        if let Some(parent) = self.parent_invocation.as_ref() {
            let parent_effect_id = parent.effect_id().unwrap_or("effect");
            return crate::runtime::causal::child_effect_invocation(
                self.dispatch.effect_controller.execution_scope(),
                parent,
                format!("{parent_effect_id}:{suffix}"),
                suffix,
            );
        }
        let replay_key = format!("{}:{suffix}", self.execution_scope_id());
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                self.dispatch.effect_controller.execution_scope().clone(),
                replay_key,
            )
            .expect("tool batch carries an admitted effect scope"),
            self.effect_attribution(),
            suffix,
        )
    }

    /// Executes one Run aggregate and returns replies in source order beside
    /// their durable settlement order. Preparation and checks are recorded in
    /// admission; no caller-side preparation or child dispatch precedes it.
    pub async fn call_tool_batch(&self, calls: Vec<ToolInvocation>) -> ToolBatchReplies {
        if calls.is_empty() {
            return ToolBatchReplies::default();
        }
        let batch_id = deterministic_tool_invocation_batch_id(&calls);
        let call_count = calls.len();
        let invocation = self.tool_batch_invocation(&batch_id);
        let request = ToolAggregateRequest {
            leaves: calls.into_iter().map(ToolAggregateLeaf::Tool).collect(),
            consumer: ToolAggregateConsumer::AllSettled,
            settled_value_after: None,
            command: crate::CommandReplayKey::new(invocation.effect_replay_key()),
        };
        let poll = match self.admit_tool_run_aggregate(request).await {
            Ok(cursor) => {
                self.await_tool_run_aggregate(&cursor, ToolAggregateConsumer::AllSettled)
                    .await
            }
            Err(error) => Err(error),
        };
        let (replies, settlement_order) = match poll {
            Ok(ToolRunAggregatePoll::Ready {
                outcome: ToolAggregateOutcome::AllResults(replies),
                settlement_order,
            }) => (replies, settlement_order),
            Err(error) => {
                // Keep resource refusal model-visible. A live infrastructure
                // fault also aborts the enclosing cell rather than committing
                // a diagnostic as a tool-produced result (FIG-3528).
                if let Some(exceeded) = error.tool_call_limit_exceeded() {
                    let refused = ToolInvocationReply::from_output(ToolCallOutput::failure(
                        tool_call_limit_failure(exceeded),
                    ));
                    return ToolBatchReplies {
                        replies: vec![refused; call_count],
                        settlement_order: Vec::new(),
                    };
                }
                if !error.journaled {
                    self.record_nested_effect_error(error.clone());
                }
                return failed_batch(error.to_string(), call_count);
            }
            Ok(_) => {
                return failed_batch(
                    "an awaited allSettled tool Run did not return every source slot".into(),
                    call_count,
                );
            }
        };
        if let Err(reason) = validate_batch_settlement_order(&settlement_order, call_count) {
            return failed_batch(reason, call_count);
        }
        if replies.len() != call_count {
            return failed_batch(
                "a tool Run returned the wrong source count".into(),
                call_count,
            );
        }
        let replies = replies
            .into_iter()
            .map(|reply| match reply {
                Some(ToolAggregateLeafReply::Tool(reply)) => Ok(*reply),
                _ => Err("a tool Run left a source call unfilled"),
            })
            .collect::<Result<Vec<_>, _>>();
        match replies {
            Ok(replies) => ToolBatchReplies {
                replies,
                settlement_order,
            },
            Err(reason) => failed_batch(reason.into(), call_count),
        }
    }
}

/// Infrastructure/shape failure carries no settlement evidence.
fn failed_batch(reason: String, call_count: usize) -> ToolBatchReplies {
    let reply =
        ToolInvocationReply::error(serde_json::json!(format!("tool batch failed: {reason}")));
    ToolBatchReplies {
        replies: vec![reply; call_count],
        settlement_order: Vec::new(),
    }
}
