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
    tool_call_limit_failure,
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

    /// Prepares one caller-order tool call for group admission: resolves its
    /// manifest and runs the tool's preparation. A call refused or completed
    /// during preparation has already settled — it belongs to the immediate
    /// prefix ahead of every dispatched settlement (ADR 0099 §10 L5).
    ///
    /// `batch_id` plus `index` and the call id are the call's observation-lane
    /// material while it has no invocation of its own (ADR 0105 §1).
    ///
    /// `refused` is this call's answer from a refused round: a refused round
    /// prepares none of its calls, and each answers its typed refusal.
    pub(super) async fn prepare_tool_leaf(
        &self,
        batch_id: &str,
        index: usize,
        mut call: ToolInvocation,
        refused: Option<crate::ToolAdmissionRefusal>,
    ) -> ToolLeafPreparation {
        let leaf_started = self.dispatch.clock.now();
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let context = call
            .issuing_language_node_id
            .clone()
            .map(|node_id| self.clone().with_issuing_language_node_id(node_id))
            .unwrap_or_else(|| self.clone());
        let call_key = format!("{batch_id}:{index}:{}", call.id);
        let authorization = ToolCallAuthorization::from_invocation(&mut call);
        let admitted = match (
            refused,
            authorization.resolve_manifest(self.dispatch.as_ref()),
        ) {
            (None, Some(manifest)) => Ok(manifest),
            (Some(refused), manifest) => Err(crate::tool_dispatch::admission_failure(
                manifest
                    .as_ref()
                    .map_or(call.tool_id.as_str(), |manifest| manifest.name.as_str()),
                refused,
            )),
            (None, None) => Err(ToolFailure::runtime(
                ToolFailureClass::Unavailable,
                "tool_unavailable",
                format!("Tool id `{}` is unavailable in this session", call.tool_id),
            )),
        };
        let manifest = match admitted {
            Ok(manifest) => manifest,
            Err(failure) => {
                let outcome = ToolDispatchOutcome {
                    record: ToolCallRecord {
                        call_id: call.id.clone(),
                        provider_call_id: None,
                        tool: call.tool_id.to_string(),
                        args: call.args,
                        output: ToolCallOutput::failure(failure),
                    },
                    attempts: Vec::new(),
                    intents: crate::ToolIntents::default(),
                    intent_outcomes: Vec::new(),
                    captures: Vec::new(),
                    triggers: Vec::new(),
                };
                if let Err(error) = context
                    .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
                    .await
                {
                    context.record_nested_effect_error(error);
                }
                // The call never ran; the Completed observation reports the
                // preparation window it actually spent in.
                let completed = context
                    .complete_language_tool_call(
                        AdmittedCallIdentity(call.id, authorization.tool_id().clone()),
                        None,
                        outcome,
                        true,
                        &call_key,
                        context
                            .dispatch
                            .clock
                            .now()
                            .saturating_duration_since(leaf_started)
                            .as_millis() as u64,
                    )
                    .await;
                return ToolLeafPreparation::Completed(Box::new(
                    ToolInvocationReply::from_output(completed.completed.output)
                        .with_record(completed.record),
                ));
            }
        };
        let pending = crate::sansio::PendingToolCall {
            call_id: call.id.clone(),
            provider_call_id: None,
            tool_name: manifest.name.clone(),
            args: call.args,
            replay: None,
        };
        // The leaf's own key namespaces the prepare's observation lanes —
        // `observation_keyed` qualifies it under this dispatch's base, which
        // for a batch opened under a parent effect is that effect's invocation
        // (ADR 0105 §1).
        let keyed_dispatch = self.dispatch.observation_keyed(&call_key);
        match authorization.prepare(&keyed_dispatch, pending).await {
            ToolPreparationOutcome::Prepared(prepared) => {
                ToolLeafPreparation::Prepared(Box::new(PreparedToolLeafEntry {
                    index,
                    prepared: *prepared,
                    authorization,
                    manifest,
                }))
            }
            ToolPreparationOutcome::Completed(outcome) => {
                if let Err(error) = context
                    .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
                    .await
                {
                    context.record_nested_effect_error(error);
                }
                let completed = context
                    .complete_language_tool_call(
                        AdmittedCallIdentity(call.id, authorization.tool_id().clone()),
                        None,
                        *outcome,
                        true,
                        &call_key,
                        context
                            .dispatch
                            .clock
                            .now()
                            .saturating_duration_since(leaf_started)
                            .as_millis() as u64,
                    )
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
        batch_id: &crate::BatchId,
        entries: Vec<PreparedToolLeafEntry>,
    ) -> Result<Vec<PreparedToolChildLeaf>, crate::PluginError> {
        let batch = crate::PreparedToolBatch::new_with_grants(
            batch_id.clone(),
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
                    // A recorded binding is the catalog admission the
                    // journaled call had, under its recorded manifest.
                    ToolCallAuthorization::Catalog(_) | ToolCallAuthorization::Recorded(_) => {
                        crate::runtime::effect::ToolChildAdmission::Catalog {
                            owner: self
                                .dispatch
                                .plugins
                                .tool_execution_owner(&entry.manifest.id, None)?,
                            manifest: Box::new(entry.manifest),
                        }
                    }
                };
                Ok(PreparedToolChildLeaf {
                    input_index: entry.index,
                    call,
                    admission,
                })
            })
            .collect()
    }

    /// Executes a source-ordered tool batch for code-executor implementors and returns replies in
    /// the same order even though individual calls may run concurrently.
    ///
    /// The batch opens as a durable effect group of `ToolInvocation` children
    /// (ADR 0099 §3); replies stay input-ordered, but `settlement_order` is the
    /// group's durable final-commit order, not source order (§5).
    pub async fn call_tool_batch(&self, calls: Vec<ToolInvocation>) -> ToolBatchReplies {
        if calls.is_empty() {
            return ToolBatchReplies::default();
        }

        let batch_id = deterministic_tool_invocation_batch_id(&calls);
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

        let refused = self.admit_tool_round(&calls).err();
        for (index, call) in calls.into_iter().enumerate() {
            let refused = refused.as_ref().map(|refused| refused.refusal_for(index));
            match self
                .prepare_tool_leaf(&batch_id, index, call, refused)
                .await
            {
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
            let prepared_leaves = match self.tool_child_leaves(&batch_id, prepared_entries) {
                Ok(leaves) => leaves,
                Err(error) => {
                    let error = crate::RuntimeEffectControllerError::from(error);
                    self.record_nested_effect_error(error.clone());
                    return fail_batch(error.to_string(), &mut replies);
                }
            };
            let leaves = prepared_leaves
                .into_iter()
                .map(|leaf| PreparedGroupChild::Tool(Box::new(leaf)))
                .collect::<Vec<_>>();
            let consumer = ToolAggregateConsumer::AllSettled;
            let group_key = self.tool_child_group_key(&batch_id);
            let handle = match self
                .open_tool_child_group(
                    group_invocation,
                    group_key.clone(),
                    &batch_id,
                    &leaves,
                    consumer.wake(),
                    crate::GroupReopen::RetainedShape,
                )
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
                    // A batch the session's recorded `max_tool_calls` refuses
                    // is the program's failure (FIG-4546): each call it had
                    // not already settled answers the typed refusal, and the
                    // enclosing cell goes on to read it.
                    if let Some(exceeded) = error.tool_call_limit_exceeded() {
                        let refused = ToolInvocationReply::from_output(ToolCallOutput::failure(
                            tool_call_limit_failure(exceeded),
                        ));
                        return ToolBatchReplies {
                            replies: replies
                                .iter_mut()
                                .map(|reply| reply.take().unwrap_or_else(|| refused.clone()))
                                .collect(),
                            settlement_order: Vec::new(),
                        };
                    }
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
            // A cancelled consumer answers with its consumed prefix; every
            // other reply is its member's durable final (ADR 0116 §2.6).
            if settled.cancelled
                && let Err(error) = self
                    .present_cancelled_tool_group(&group_key, &leaves, &mut settled)
                    .await
            {
                if !error.journaled {
                    self.record_nested_effect_error(error.clone());
                }
                return fail_batch(error.to_string(), &mut replies);
            }
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

impl RuntimeExecutionContext<'_> {
    /// Admits a round of calls together, before any prepares (K1): every
    /// call's admitted manifest — its recorded binding's, its grant's, or the
    /// catalog's — must carry a valid, supported declaration.
    pub(super) fn admit_tool_round(
        &self,
        calls: &[ToolInvocation],
    ) -> Result<(), crate::tool_dispatch::ToolRoundRefusal> {
        let manifests: Vec<_> = calls
            .iter()
            .map(
                |call| match (&call.recorded_binding, &call.execution_grant) {
                    (Some(binding), _) => Some(binding.manifest.clone()),
                    (None, Some(grant)) => Some(grant.manifest().clone()),
                    (None, None) => crate::tool_dispatch::resolve_callable_manifest_by_id(
                        self.dispatch.as_ref(),
                        &call.tool_id,
                    ),
                },
            )
            .collect();
        crate::tool_dispatch::admit_tool_round(manifests.iter().map(Option::as_ref))
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
