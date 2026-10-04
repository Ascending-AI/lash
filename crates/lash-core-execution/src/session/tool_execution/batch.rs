//! The tool-batch surface of [`RuntimeExecutionContext`].
//!
//! One source-ordered batch is admitted and consumed by the logical owner's
//! Run. Its replies remain in caller order beside the durable settlement
//! order used by settlement-selecting consumers.

use super::*;

use super::group::{PreparedToolChildLeaf, ToolAggregateConsumer, tool_call_limit_failure};

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

/// Infrastructure/shape failure carries no settlement evidence.
fn failed_batch(reason: String, call_count: usize) -> ToolBatchReplies {
    let reply =
        ToolInvocationReply::error(serde_json::json!(format!("tool batch failed: {reason}")));
    ToolBatchReplies {
        replies: vec![reply; call_count],
        settlement_order: Vec::new(),
    }
}
