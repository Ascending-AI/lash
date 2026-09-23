//! The tool-batch surface of [`RuntimeExecutionContext`].
//!
//! One source-ordered batch is prepared, opened as a durable effect group of
//! tool children (ADR 0099 §3), and its settlements are returned in caller
//! order beside the order the group settled them in (§5). It lives beside the
//! rest of tool execution rather than inside it because it is the one tenant
//! with its own settlement rules — and because the two together outgrew the
//! file-size budget.

use super::*;

use super::group::{
    GroupChildSettled, PreparedGroupChild, PreparedToolChildLeaf, ToolAggregateConsumer,
};

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
                self.dispatch.effect_controller.scoped().execution_scope(),
                parent,
                format!("{parent_effect_id}:{suffix}"),
                suffix,
            );
        }
        let replay_key = format!("{}:{suffix}", self.execution_scope_id());
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                self.dispatch
                    .effect_controller
                    .scoped()
                    .execution_scope()
                    .clone(),
                replay_key,
            )
            .expect("tool batch carries an admitted effect scope"),
            self.effect_attribution(),
            suffix,
        )
    }

    /// Prepares one caller-order tool call for group admission: resolves its
    /// manifest and runs the tool's preparation. A call refused or completed
    /// during preparation has already settled — it belongs to the immediate
    /// prefix ahead of every dispatched settlement (ADR 0099 §10 L5).
    pub(super) async fn prepare_tool_leaf(
        &self,
        index: usize,
        mut call: ToolInvocation,
    ) -> ToolLeafPreparation {
        let context = call
            .issuing_language_node_id
            .clone()
            .map(|node_id| self.clone().with_issuing_language_node_id(node_id))
            .unwrap_or_else(|| self.clone());
        let authorization = ToolCallAuthorization::from_invocation(&mut call);
        let Some(manifest) = authorization.resolve_manifest(self.dispatch.as_ref()) else {
            let outcome = ToolDispatchOutcome {
                record: ToolCallRecord {
                    call_id: Some(call.id.clone()),
                    tool: call.tool_id.to_string(),
                    args: call.args,
                    output: ToolCallOutput::failure(ToolFailure::runtime(
                        ToolFailureClass::Unavailable,
                        "tool_unavailable",
                        format!("Tool id `{}` is unavailable in this session", call.tool_id),
                    )),
                    duration_ms: 0,
                },
                attempts: Vec::new(),
                intents: crate::ToolIntents::default(),
                intent_outcomes: Vec::new(),
                captures: Vec::new(),
                triggers: Vec::new(),
            };
            let completed = context
                .complete_undispatched_tool_call(call.id, None, outcome)
                .await;
            return ToolLeafPreparation::Completed(Box::new(
                ToolInvocationReply::from_output(completed.completed.output)
                    .with_record(completed.record),
            ));
        };
        let pending = crate::sansio::PendingToolCall {
            call_id: call.id.clone(),
            tool_name: manifest.name.clone(),
            args: call.args,
            replay: None,
        };
        match authorization
            .prepare(self.dispatch.as_ref(), pending, call.id.clone())
            .await
        {
            ToolPreparationOutcome::Prepared(prepared) => {
                ToolLeafPreparation::Prepared(Box::new(PreparedToolLeafEntry {
                    index,
                    prepared: *prepared,
                    authorization,
                    manifest,
                }))
            }
            ToolPreparationOutcome::Completed(outcome) => {
                let completed = context
                    .complete_undispatched_tool_call(call.id, None, *outcome)
                    .await;
                ToolLeafPreparation::Completed(Box::new(
                    ToolInvocationReply::from_output(completed.completed.output)
                        .with_record(completed.record),
                ))
            }
        }
    }

    /// The retained group leaves for `entries`, with the byte-identical
    /// `child:{index}:{call_id}` replay suffixes the batch path minted
    /// (ADR 0099 §3) and each leaf's admission pinned.
    pub(super) fn tool_child_leaves(
        &self,
        batch_id: &str,
        entries: Vec<PreparedToolLeafEntry>,
    ) -> Vec<PreparedToolChildLeaf> {
        let batch = crate::PreparedToolBatch::new_with_grants(
            batch_id.to_string(),
            entries
                .iter()
                .map(|entry| {
                    (
                        entry.prepared.clone(),
                        entry.authorization.execution_grant().cloned(),
                    )
                })
                .collect(),
        );
        entries
            .into_iter()
            .zip(batch.calls)
            .map(|(entry, call)| {
                let admission = match entry.authorization {
                    ToolCallAuthorization::Granted(grant) => {
                        crate::runtime::effect::ToolChildAdmission::Granted { grant }
                    }
                    ToolCallAuthorization::Catalog(_) => {
                        crate::runtime::effect::ToolChildAdmission::Catalog {
                            manifest: Box::new(entry.manifest),
                        }
                    }
                };
                PreparedToolChildLeaf {
                    input_index: entry.index,
                    call,
                    admission,
                }
            })
            .collect()
    }

    /// Executes a source-ordered tool batch for code-executor implementors and returns replies in
    /// the same order even though individual calls may run concurrently.
    ///
    /// The batch opens as a durable effect group of `ToolInvocation` children
    /// (ADR 0099 §3); replies stay input-ordered, but `settlement_order` is the
    /// group's durable final-commit order, not source order (§5).
    pub async fn call_tool_batch(
        &self,
        calls: Vec<ToolInvocation>,
        occurrence: crate::session::ToolGroupOccurrence,
    ) -> ToolBatchReplies {
        if calls.is_empty() {
            return ToolBatchReplies::default();
        }

        let batch_id = deterministic_tool_invocation_batch_id(&calls, occurrence);
        let mut replies = vec![None; calls.len()];
        // A failed batch reports an empty settlement order by construction: downstream
        // settlement-selecting aggregates treat the order as evidence of what settled.
        // Replies already completed during preparation are preserved.
        let fail_batch =
            |reason: String, replies: &mut Vec<Option<ToolInvocationReply>>| -> ToolBatchReplies {
                let error = serde_json::json!(format!("tool batch failed: {reason}"));
                ToolBatchReplies {
                    replies: replies
                        .iter_mut()
                        .map(|reply| {
                            reply
                                .take()
                                .unwrap_or_else(|| ToolInvocationReply::error(error.clone()))
                        })
                        .collect(),
                    settlement_order: Vec::new(),
                }
            };
        let mut prepared_entries = Vec::new();
        // A call that finishes while being prepared has already settled by the
        // time the concurrent batch starts, so it leads the settlement order.
        let mut settled_during_preparation = Vec::new();

        for (index, call) in calls.into_iter().enumerate() {
            match self.prepare_tool_leaf(index, call).await {
                ToolLeafPreparation::Prepared(entry) => prepared_entries.push(*entry),
                ToolLeafPreparation::Completed(reply) => {
                    replies[index] = Some(*reply);
                    settled_during_preparation.push(index);
                }
            }
        }
        let mut settlement_order = settled_during_preparation;

        if !prepared_entries.is_empty() {
            // ADR 0099: the batch opens as a durable effect group of
            // `ToolInvocation` children and the consumer observes settlement
            // rank — durable final-commit order — rather than a source-ordered
            // launch vector (§5).
            let group_invocation = self.tool_batch_invocation(&batch_id);
            let leaves = self
                .tool_child_leaves(&batch_id, prepared_entries)
                .into_iter()
                .map(|leaf| PreparedGroupChild::Tool(Box::new(leaf)))
                .collect::<Vec<_>>();
            let consumer = ToolAggregateConsumer::AllSettled;
            let handle = match self
                .open_tool_child_group(group_invocation, &batch_id, &leaves, consumer.wake())
                .await
            {
                Ok(handle) => handle,
                // A live controller error here — the group row's claim
                // faulted, or the formation boundary refused — recorded
                // nothing durable. The replies keep this API's contract, but
                // the error is also recorded so the enclosing cell aborts and
                // the store diagnostic never commits as a tool result the
                // tools did not produce (FIG-3528). A journaled error is a
                // recorded `Failed` terminal replaying and stays on the reply
                // surface.
                Err(error) => {
                    if !error.journaled {
                        self.record_nested_effect_error(error.clone());
                    }
                    return fail_batch(error.to_string(), &mut replies);
                }
            };
            let mut settled = match self
                .consume_tool_child_group(handle, &leaves, consumer)
                .await
            {
                Ok(settled) => settled,
                // Same split as the open: a live fault while consuming or
                // incorporating settlements aborts the enclosing cell; a
                // journaled error — a child's recorded `Failed` terminal
                // surfacing through `settlement.outcome` — stays
                // model-visible (FIG-3528).
                Err(error) => {
                    if !error.journaled {
                        self.record_nested_effect_error(error.clone());
                    }
                    return fail_batch(error.to_string(), &mut replies);
                }
            };
            // The group reports settlement in child positions; the caller
            // counts in original call positions. Dropping an out-of-range
            // position and back-filling the gap would turn any malformed order
            // into a clean-looking input-order permutation, which is exactly
            // the rejection selection this field exists to prevent — the
            // defect would be repaired into invisibility instead of failing
            // closed.
            if let Err(reason) =
                validate_batch_settlement_order(&settled.settlement_positions, leaves.len())
            {
                return fail_batch(reason, &mut replies);
            }
            settlement_order.extend(
                settled
                    .settlement_positions
                    .iter()
                    .filter_map(|position| leaves[*position].tool())
                    .map(|leaf| leaf.input_index),
            );
            for (position, leaf) in leaves.iter().enumerate() {
                let (Some(leaf), Some(GroupChildSettled::Tool(completed))) =
                    (leaf.tool(), settled.settled[position].take())
                else {
                    return fail_batch(
                        format!("tool-child group left position {position} unfilled"),
                        &mut replies,
                    );
                };
                replies[leaf.input_index] = Some(
                    ToolInvocationReply::from_output(completed.completed.output)
                        .with_record(completed.record),
                );
            }
        }

        #[expect(
            clippy::expect_used,
            reason = "the loop above writes every index of `replies` exactly once before it is drained here"
        )]
        let replies = replies
            .into_iter()
            .map(|reply| reply.expect("every batch reply slot should be filled"))
            .collect::<Vec<_>>();
        ToolBatchReplies {
            replies,
            settlement_order,
        }
    }
}

/// One tool call ready for group admission.
pub(super) struct PreparedToolLeafEntry {
    /// The call's position in its caller's leaf order.
    pub(super) index: usize,
    pub(super) prepared: crate::PreparedToolCall,
    pub(super) authorization: ToolCallAuthorization,
    pub(super) manifest: crate::ToolManifest,
}

/// What preparing one tool call produced.
pub(super) enum ToolLeafPreparation {
    /// Ready for admission as a group child.
    Prepared(Box<PreparedToolLeafEntry>),
    /// Settled during preparation: part of the immediate prefix.
    Completed(Box<ToolInvocationReply>),
}
