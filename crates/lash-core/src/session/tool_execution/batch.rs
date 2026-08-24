//! The tool-batch path of [`RuntimeExecutionContext`].
//!
//! One source-ordered batch is prepared, launched through the durable effect
//! boundary, and settled back into caller order. It lives beside the rest of
//! tool execution rather than inside it because it is the one tenant with its
//! own concurrency and settlement rules — and because the two together outgrew
//! the file-size budget.

use super::*;

impl RuntimeExecutionContext<'_> {
    fn tool_batch_invocation(&self, batch_id: &str) -> crate::RuntimeInvocation {
        let suffix = format!("tool-batch:{batch_id}");
        if let Some(parent) = self.parent_invocation.as_ref() {
            let parent_effect_id = parent.effect_id().unwrap_or("effect");
            return crate::runtime::causal::child_effect_invocation(
                parent,
                format!("{parent_effect_id}:{suffix}"),
                crate::RuntimeEffectKind::ToolBatch,
                suffix,
            );
        }
        let replay_key = format!("{}:{suffix}", self.execution_scope_id());
        crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new(self.session_id.clone()),
            suffix,
            crate::RuntimeEffectKind::ToolBatch,
            replay_key,
        )
    }

    pub(crate) async fn execute_prepared_tool_batch_launches(
        &self,
        batch: crate::PreparedToolBatch,
        parent_invocation: crate::RuntimeInvocation,
        child_trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook>,
    ) -> Result<crate::ToolBatchEffectOutcome, crate::RuntimeEffectControllerError> {
        let indexed_tools = batch.calls.into_iter().enumerate().collect::<Vec<_>>();
        let cancellation = self.cancellation_token.clone().unwrap_or_default();
        let tool_cancel = cancellation.child_token();
        let child_trace_hooks = std::sync::Arc::new(child_trace_hooks);
        if !self
            .dispatch
            .effect_controller
            .controller()
            .supports_concurrent_effects()
        {
            let mut launches = Vec::with_capacity(indexed_tools.len());
            let mut triggers = Vec::new();
            let mut context = self.clone().with_cancellation_token(tool_cancel.clone());
            for (index, child) in indexed_tools {
                if cancellation.is_cancelled() {
                    tool_cancel.cancel();
                    launches.push(cancelled_runtime_tool_call_launch(
                        child.call.call_id,
                        child.call.tool_name,
                        child.call.args,
                        child.call.replay,
                    ));
                    continue;
                }
                let child_execution_trace_hook =
                    child_trace_hooks.get(&child.call.call_id).cloned();
                let outcome = context
                    .execute_prepared_tool_batch_child(
                        child,
                        index,
                        parent_invocation.clone(),
                        child_execution_trace_hook,
                        None,
                    )
                    .await;
                launches.push(outcome.launch);
                triggers.extend(outcome.triggers);
                context = context.with_cancellation_token(tool_cancel.clone());
            }
            // This path runs the leaves one at a time, so they settle in the
            // order they were issued.
            let settlement_order = (0..launches.len()).collect();
            return Ok(crate::ToolBatchEffectOutcome {
                launches,
                triggers,
                settlement_order,
            });
        }
        let intent_drain_gate =
            std::sync::Arc::new(crate::tool_dispatch::BatchIntentDrainGate::default());
        let child_outcomes = schedule_tool_batch(indexed_tools, |(index, _)| *index, {
            let context = self.clone();
            let cancellation = cancellation.clone();
            let tool_cancel = tool_cancel.clone();
            let child_trace_hooks = std::sync::Arc::clone(&child_trace_hooks);
            let intent_drain_gate = std::sync::Arc::clone(&intent_drain_gate);
            move |(index, child)| {
                let context = context.clone().with_cancellation_token(tool_cancel.clone());
                let cancellation = cancellation.clone();
                let tool_cancel = tool_cancel.clone();
                let parent_invocation = parent_invocation.clone();
                let cancelled_tool = child.call.clone();
                let child_execution_trace_hook =
                    child_trace_hooks.get(&child.call.call_id).cloned();
                let (intent_drain_slot, mut final_result_committed) =
                    crate::tool_dispatch::IntentDrainGuard::new(
                        std::sync::Arc::clone(&intent_drain_gate),
                        index,
                    );
                async move {
                    let tool_call = context.execute_prepared_tool_batch_child(
                        child,
                        index,
                        parent_invocation,
                        child_execution_trace_hook,
                        Some(intent_drain_slot),
                    );
                    tokio::pin!(tool_call);
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            tool_cancel.cancel();
                            let grace = context
                                .dispatch
                                .clock
                                .sleep(std::time::Duration::from_millis(50));
                            tokio::pin!(grace);
                            if final_result_committed.is_committed() {
                                return tool_call.await;
                            }
                            tokio::select! {
                                biased;
                                outcome = &mut tool_call => outcome,
                                () = final_result_committed.committed() => tool_call.await,
                                // Abandoning the child here drops its drain
                                // guard along with its future, which discharges
                                // the drain slot; no hand-written release is
                                // needed to keep later siblings moving.
                                _ = &mut grace => {
                                    CoordinatedToolLaunch {
                                        launch: cancelled_runtime_tool_call_launch(
                                            cancelled_tool.call_id,
                                            cancelled_tool.tool_name,
                                            cancelled_tool.args,
                                            cancelled_tool.replay,
                                        ),
                                        triggers: Vec::new(),
                                    }
                                },
                            }
                        }
                        outcome = &mut tool_call => outcome,
                    }
                }
            }
        })
        .await;
        // A cancel-grace timeout above drops a child tool future that may have
        // parked this process run's execution permit
        // (`release_process_execution_permit_while`), and the run continues
        // afterwards. Reacquire the slot here — every child is finished, so
        // this cannot starve a sibling that is still parked on it — exactly as
        // the cancelled background-session-turn path does before it resumes.
        crate::runtime::ensure_process_execution_permit().await;

        let mut launches = Vec::with_capacity(child_outcomes.outcomes.len());
        let mut triggers = Vec::new();
        for outcome in child_outcomes.outcomes {
            launches.push(outcome.launch);
            triggers.extend(outcome.triggers);
        }
        Ok(crate::ToolBatchEffectOutcome {
            launches,
            triggers,
            settlement_order: child_outcomes.settlement_order,
        })
    }

    async fn execute_prepared_tool_batch_child(
        &self,
        child: crate::PreparedToolBatchCall,
        index: usize,
        parent_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        intent_drain_slot: Option<crate::tool_dispatch::IntentDrainGuard>,
    ) -> CoordinatedToolLaunch {
        let call_id = child.call.call_id.clone();
        let tool_name = child.call.tool_name.clone();
        let args = child.call.args.clone();
        let replay = child.call.replay.clone();
        let activity_id = TurnActivityId::new(format!("tool:{call_id}"));
        self.emit_tool_call_started(&call_id, &tool_name, args.clone(), activity_id.clone())
            .await;

        if child.execution_grant.is_none()
            && self.dispatch.is_orchestrating_tool(&child.call.tool_id)
        {
            let tool_context = crate::ToolContext::from_dispatch(Arc::clone(&self.dispatch))
                .prepared_call(&child.call)
                .cancellation_token(self.cancellation_token.clone())
                .runtime_execution_context(
                    self.clone()
                        .with_parent_invocation(parent_invocation.clone()),
                )
                .parent_invocation(Some(parent_invocation))
                .child_execution_trace_hook(child_execution_trace_hook)
                .build();
            let outcome = crate::tool_dispatch::execute_orchestrating_tool(
                self.dispatch.as_ref(),
                child.call,
                tool_context,
            )
            .await;
            // An orchestrating body declares no intents to drain, so its slot is
            // discharged as soon as the body returns. Dropping the guard here
            // rather than at the end of this function keeps the release point
            // where the former hand-written discharge stood.
            drop(intent_drain_slot);
            let completed = self
                .complete_tool_call(index, call_id, replay, outcome, activity_id)
                .await;
            return CoordinatedToolLaunch {
                launch: crate::runtime::ToolCallLaunch::Done {
                    result: Box::new(completed.completed),
                },
                // Orchestrating bodies emit trigger occurrences directly, so the
                // batch still owns their effect outcomes.
                triggers: self.dispatch.trigger_outcomes.drain(),
            };
        }

        let retry_policy = crate::tool_dispatch::resolve_retry_policy(
            self.dispatch.as_ref(),
            &child.call.tool_id,
            child.execution_grant.as_deref(),
        );
        let intent_trace_hook = child_execution_trace_hook.clone();
        let trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook> =
            child_execution_trace_hook
                .map(|hook| std::iter::once((call_id.clone(), hook)).collect())
                .unwrap_or_default();
        let coordinated = coordinate_tool_invocation(
            self.dispatch.as_ref(),
            child.call.clone(),
            child.execution_grant,
            retry_policy,
            ToolAttemptEffectIdentity::Batch {
                parent: parent_invocation.clone(),
                replay_suffix: child.replay_suffix.clone(),
            },
            self.cancellation_token.clone(),
            intent_drain_slot,
            intent_trace_hook,
            |completion_key| {
                crate::RuntimeEffectLocalExecutor::tool_batch(
                    self.clone(),
                    trace_hooks.clone(),
                    completion_key,
                )
            },
        )
        .await;
        let outcome = match coordinated.launch {
            ToolCallLaunch::Done(outcome) => *outcome,
            ToolCallLaunch::Pending(pending) => {
                self.await_pending_tool_dispatch_outcome_with_suffix(
                    &call_id,
                    Some(parent_invocation),
                    format!("{}:await", child.replay_suffix),
                    *pending,
                    self.cancellation_token.clone(),
                )
                .await
            }
        };
        let completed = self
            .complete_tool_call(index, call_id, replay, outcome, activity_id)
            .await;
        CoordinatedToolLaunch {
            launch: crate::runtime::ToolCallLaunch::Done {
                result: Box::new(completed.completed),
            },
            triggers: coordinated.triggers,
        }
    }

    /// Executes a source-ordered tool batch for code-executor implementors and returns replies in
    /// the same order even though individual calls may run concurrently.
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

        for (index, call) in calls.into_iter().enumerate() {
            let preparation = if let Some(grant) = call.execution_grant.as_deref().cloned() {
                let pending = crate::sansio::PendingToolCall {
                    call_id: call.id.clone(),
                    tool_name: grant.manifest().name.clone(),
                    args: call.args,
                    replay: None,
                };
                (
                    Some(grant.clone()),
                    prepare_granted_tool_call_with_context(
                        self.dispatch.as_ref(),
                        &grant,
                        pending,
                        Some(call.id.clone()),
                    )
                    .await,
                )
            } else {
                let Some(manifest) = crate::tool_dispatch::resolve_callable_manifest_by_id(
                    self.dispatch.as_ref(),
                    &call.tool_id,
                ) else {
                    let outcome = ToolDispatchOutcome {
                        record: ToolCallRecord {
                            call_id: Some(call.id.clone()),
                            tool: call.tool_id.to_string(),
                            args: call.args,
                            output: ToolCallOutput::failure(ToolFailure::runtime(
                                ToolFailureClass::Unavailable,
                                "tool_unavailable",
                                format!(
                                    "Tool id `{}` is unavailable in this session",
                                    call.tool_id
                                ),
                            )),
                            duration_ms: 0,
                        },
                        attempts: Vec::new(),
                        intents: crate::ToolIntents::default(),
                        intent_outcomes: Vec::new(),
                    };
                    let completed = self
                        .complete_tool_call(
                            index,
                            call.id,
                            None,
                            outcome,
                            TurnActivityId::new(format!("tool:{}", batch_id)),
                        )
                        .await;
                    replies[index] = Some(
                        ToolInvocationReply::from_output(completed.completed.output)
                            .with_record(completed.record),
                    );
                    settled_during_preparation.push(index);
                    continue;
                };

                let pending = crate::sansio::PendingToolCall {
                    call_id: call.id.clone(),
                    tool_name: manifest.name,
                    args: call.args,
                    replay: None,
                };
                (None, self.prepare_tool_call(pending).await)
            };
            let (execution_grant, preparation) = preparation;
            match preparation {
                ToolPreparationOutcome::Prepared(prepared) => {
                    prepared_entries.push((
                        index,
                        *prepared,
                        execution_grant,
                        call.child_execution_trace_hook,
                    ));
                }
                ToolPreparationOutcome::Completed(outcome) => {
                    let completed = self
                        .complete_tool_call(
                            index,
                            call.id,
                            None,
                            *outcome,
                            TurnActivityId::new(format!("tool:{}", batch_id)),
                        )
                        .await;
                    replies[index] = Some(
                        ToolInvocationReply::from_output(completed.completed.output)
                            .with_record(completed.record),
                    );
                    settled_during_preparation.push(index);
                }
            }
        }
        let mut settlement_order = settled_during_preparation;

        if !prepared_entries.is_empty() {
            let invocation = self.tool_batch_invocation(&batch_id);
            let batch = crate::PreparedToolBatch::new_with_grants(
                batch_id.clone(),
                prepared_entries
                    .iter()
                    .map(|(_, prepared, grant, _)| (prepared.clone(), grant.clone()))
                    .collect(),
            );
            let child_trace_hooks = prepared_entries
                .iter()
                .filter_map(|(_, prepared, _, hook)| {
                    hook.clone().map(|hook| (prepared.call_id.clone(), hook))
                })
                .collect();
            let envelope = crate::RuntimeEffectEnvelope::new(
                invocation.clone(),
                crate::RuntimeEffectCommand::ToolBatch { batch },
            );
            let local_executor = crate::RuntimeEffectLocalExecutor::tool_batch(
                self.clone(),
                child_trace_hooks,
                None,
            );
            let raw_outcome = self
                .dispatch
                .effect_controller
                .controller()
                .execute_effect(envelope, local_executor)
                .await;
            let mut outcome =
                match raw_outcome.and_then(crate::RuntimeEffectOutcome::into_tool_batch_effect) {
                    Ok(outcome) => outcome,
                    Err(err) => return fail_batch(err.to_string(), &mut replies),
                };
            // The batch effect drained its own trigger buffer into the recorded
            // outcome, so restoring it here is what makes an inner emission
            // survive both the local run and its replay: the enclosing effect
            // boundary drains this buffer in turn.
            self.restore_tool_trigger_outcomes(std::mem::take(&mut outcome.triggers));
            self.dispatch
                .recorded_intent_outcomes
                .record_launches(&outcome.launches);
            // The batch reports settlement in prepared-entry positions; the
            // caller counts in original call positions.
            let batch_call_indices = prepared_entries
                .iter()
                .map(|(index, _, _, _)| *index)
                .collect::<Vec<_>>();
            // Validate before translating. Dropping an out-of-range position
            // and back-filling the gap would turn any malformed order into a
            // clean-looking input-order permutation, which is exactly the
            // rejection selection this field exists to prevent — the defect
            // would be repaired into invisibility instead of failing closed.
            if let Err(reason) =
                validate_batch_settlement_order(&outcome.settlement_order, batch_call_indices.len())
            {
                return fail_batch(reason, &mut replies);
            }
            if outcome.launches.len() != prepared_entries.len() {
                let message = format!(
                    "returned {} launches for {} prepared calls",
                    outcome.launches.len(),
                    prepared_entries.len()
                );
                return fail_batch(message, &mut replies);
            }
            settlement_order.extend(
                outcome
                    .settlement_order
                    .iter()
                    .map(|position| batch_call_indices[*position]),
            );
            // This loop looks like it settles parked leaves in input order,
            // and a reviewer reading it alone would rightly call that a
            // defect: a deferred leaf that rejects first would not lead the
            // order. It does not, because a batch leaf never reaches here
            // parked. `execute_prepared_tool_batch_child` awaits its own
            // pending completion and always hands back `Done`, and it does
            // so inside the unordered scheduler, so a deferred leaf's true
            // completion time is what places it in `settlement_order`.
            // `ToolBatchEffectOutcome` is crate-private with that one
            // producer, so no host can supply parked launches either. The
            // `Pending` arm below is therefore unreachable today and is
            // kept only so the match stays total.
            //
            // `session::settlement_latency_tests` holds this down with two
            // real deferred tools whose completions race: the fast rejection
            // leads the order whichever position it was launched in.
            for ((index, prepared, _, _), launch) in
                prepared_entries.into_iter().zip(outcome.launches)
            {
                let call_id = prepared.call_id.clone();
                let reply = match launch {
                    crate::runtime::ToolCallLaunch::Done { result } => {
                        let result = *result;
                        let record = ToolCallRecord {
                            call_id: Some(result.call_id.clone()),
                            tool: result.tool_name.clone(),
                            args: result.args.clone(),
                            output: result.output.clone(),
                            duration_ms: result.duration_ms,
                        };
                        ToolInvocationReply::from_output(result.output).with_record(record)
                    }
                    crate::runtime::ToolCallLaunch::Pending {
                        key,
                        pending,
                        duration_ms,
                    } => {
                        let dispatch_outcome = self
                            .await_pending_tool_dispatch_outcome(
                                &call_id,
                                Some(invocation.clone()),
                                crate::tool_dispatch::PendingToolDispatchOutcome {
                                    tool_name: prepared.tool_name.clone(),
                                    args: prepared.args.clone(),
                                    key: *key,
                                    pending,
                                    duration_ms,
                                    attempts: Vec::new(),
                                },
                                self.cancellation_token.clone(),
                            )
                            .await;
                        let completed = self
                            .complete_tool_call(
                                index,
                                call_id.clone(),
                                prepared.replay.clone(),
                                dispatch_outcome,
                                TurnActivityId::new(format!("tool:{call_id}")),
                            )
                            .await;
                        ToolInvocationReply::from_output(completed.completed.output)
                            .with_record(completed.record)
                    }
                };
                replies[index] = Some(reply);
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum BatchFailureResponse {
        LaunchCountMismatch,
        MalformedSettlementOrder,
        EffectDecodeError,
    }

    struct BatchFailureEffectController {
        calls: AtomicUsize,
        response: BatchFailureResponse,
    }

    impl BatchFailureEffectController {
        fn new(response: BatchFailureResponse) -> Self {
            Self {
                calls: AtomicUsize::default(),
                response,
            }
        }
    }

    impl crate::AwaitEventResolver for BatchFailureEffectController {}

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for BatchFailureEffectController {
        async fn execute_effect(
            &self,
            envelope: crate::RuntimeEffectEnvelope,
            _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(matches!(
                envelope.command,
                crate::RuntimeEffectCommand::ToolBatch { .. }
            ));
            let settlement_order = match self.response {
                BatchFailureResponse::LaunchCountMismatch => vec![0],
                BatchFailureResponse::MalformedSettlementOrder => vec![0, 0],
                BatchFailureResponse::EffectDecodeError => {
                    return Err(crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode,
                        "effect decode failed",
                    ));
                }
            };
            Ok(crate::RuntimeEffectOutcome::ToolBatch {
                launches: Vec::new(),
                triggers: Vec::new(),
                settlement_order,
            })
        }
    }

    struct BatchFailureTools;

    fn batch_failure_tool() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:batch_failure",
            "batch_failure",
            "",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for BatchFailureTools {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![batch_failure_tool().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "batch_failure").then(|| Arc::new(batch_failure_tool().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
            crate::ToolOutcome::ok(serde_json::json!("not reached"))
        }
    }

    fn batch_failure_context(
        controller: Arc<BatchFailureEffectController>,
    ) -> crate::RuntimeExecutionContext<'static> {
        let provider: Arc<dyn crate::ToolProvider> = Arc::new(BatchFailureTools);
        let plugins = crate::plugin::PluginHost::new(vec![Arc::new(
            crate::plugin::StaticPluginFactory::new(
                "batch_failure_tools",
                crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
            ),
        )])
        .build_session("session", None)
        .expect("plugin session");
        let tools = plugins.tools();
        let tool_catalog = plugins
            .resolved_tool_catalog("session")
            .expect("tool catalog");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let attachment_store: Arc<crate::SessionAttachmentStore> =
            Arc::new(crate::SessionAttachmentStore::in_memory());
        let dispatch = crate::tool_dispatch::ToolDispatchContext {
            plugins,
            tools,
            tool_registry: None,
            tool_catalog,
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(controller),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        };
        crate::RuntimeExecutionContext::new(
            "session".to_string(),
            Arc::new(dispatch),
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
    }

    #[tokio::test]
    async fn launch_count_mismatch_returns_empty_settlement_order() {
        let controller = Arc::new(BatchFailureEffectController::new(
            BatchFailureResponse::LaunchCountMismatch,
        ));
        let context = batch_failure_context(Arc::clone(&controller));
        let replies = context
            .call_tool_batch(vec![ToolInvocation::new(
                "call",
                crate::ToolId::from("tool:batch_failure"),
                serde_json::json!({}),
            )])
            .await;

        assert_eq!(
            controller.calls.load(Ordering::SeqCst),
            1,
            "the effect controller must drive the launch-count-mismatch path"
        );
        assert_eq!(replies.replies.len(), 1, "one reply per input call");
        assert!(
            !replies.replies[0].output.is_success(),
            "mismatched launch count must fail the reply"
        );
        assert!(
            replies.settlement_order.is_empty(),
            "a failed batch reports no settled calls"
        );
        assert_eq!(
            replies.replies[0].output.value_for_projection()["message"],
            serde_json::json!("tool batch failed: returned 0 launches for 1 prepared calls")
        );
    }

    #[tokio::test]
    async fn malformed_settlement_order_returns_empty_settlement_order() {
        let context = batch_failure_context(Arc::new(BatchFailureEffectController::new(
            BatchFailureResponse::MalformedSettlementOrder,
        )));
        let replies = context
            .call_tool_batch(vec![ToolInvocation::new(
                "call",
                crate::ToolId::from("tool:batch_failure"),
                serde_json::json!({}),
            )])
            .await;

        assert!(!replies.replies[0].output.is_success());
        assert!(replies.settlement_order.is_empty());
        assert_eq!(
            replies.replies[0].output.value_for_projection()["message"],
            serde_json::json!(
                "tool batch failed: tool batch reported 2 settled positions for 1 launches"
            )
        );
    }

    #[tokio::test]
    async fn effect_decode_error_returns_empty_settlement_order() {
        let context = batch_failure_context(Arc::new(BatchFailureEffectController::new(
            BatchFailureResponse::EffectDecodeError,
        )));
        let replies = context
            .call_tool_batch(vec![ToolInvocation::new(
                "call",
                crate::ToolId::from("tool:batch_failure"),
                serde_json::json!({}),
            )])
            .await;

        assert!(!replies.replies[0].output.is_success());
        assert!(replies.settlement_order.is_empty());
        assert_eq!(
            replies.replies[0].output.value_for_projection()["message"],
            serde_json::json!(
                "tool batch failed: runtime_effect_envelope_canonical_decode: effect decode failed"
            )
        );
    }
}
