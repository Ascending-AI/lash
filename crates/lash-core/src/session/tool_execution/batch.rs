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
            for (_, child) in indexed_tools {
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
        parent_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        intent_drain_slot: Option<crate::tool_dispatch::IntentDrainGuard>,
    ) -> CoordinatedToolLaunch {
        let authorization = match child.execution_grant {
            Some(grant) => ToolCallAuthorization::Granted(grant),
            None => ToolCallAuthorization::Catalog(child.call.tool_id.clone()),
        };
        let call_id = child.call.call_id.clone();
        let tool_name = child.call.tool_name.clone();
        let args = child.call.args.clone();
        let replay = child.call.replay.clone();
        let activity_id = tool_activity_id(&call_id);
        self.emit_tool_call_started(&call_id, &tool_name, args.clone(), activity_id.clone())
            .await;

        if authorization.allows_orchestration()
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
            let completed = self.complete_tool_call(call_id, replay, outcome).await;
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
            authorization.execution_grant(),
        );
        let intent_trace_hook = child_execution_trace_hook.clone();
        let trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook> =
            child_execution_trace_hook
                .map(|hook| std::iter::once((call_id.clone(), hook)).collect())
                .unwrap_or_default();
        let execution_grant = authorization.into_execution_grant();
        let coordinated = coordinate_tool_invocation(
            self.dispatch.as_ref(),
            child.call.clone(),
            execution_grant,
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
        let completed = self.complete_tool_call(call_id, replay, outcome).await;
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

        for (index, mut call) in calls.into_iter().enumerate() {
            let authorization = ToolCallAuthorization::from_invocation(&mut call);
            let Some(tool_name) = authorization.tool_name(self.dispatch.as_ref()) else {
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
                };
                let completed = self.complete_tool_call(call.id, None, outcome).await;
                replies[index] = Some(
                    ToolInvocationReply::from_output(completed.completed.output)
                        .with_record(completed.record),
                );
                settled_during_preparation.push(index);
                continue;
            };
            let pending = crate::sansio::PendingToolCall {
                call_id: call.id.clone(),
                tool_name,
                args: call.args,
                replay: None,
            };
            let preparation = authorization
                .prepare(self.dispatch.as_ref(), pending, call.id.clone())
                .await;
            match preparation {
                ToolPreparationOutcome::Prepared(prepared) => {
                    prepared_entries.push((
                        index,
                        *prepared,
                        authorization,
                        call.child_execution_trace_hook,
                    ));
                }
                ToolPreparationOutcome::Completed(outcome) => {
                    let completed = self.complete_tool_call(call.id, None, *outcome).await;
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
                    .map(|(_, prepared, authorization, _)| {
                        (prepared.clone(), authorization.execution_grant().cloned())
                    })
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
                                call_id.clone(),
                                prepared.replay.clone(),
                                dispatch_outcome,
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
    use lash_sansio::sync::MutexExt as _;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn granted_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:granted_orchestration_probe",
            "granted_orchestration_probe",
            "Proves granted calls stay in the leaf lane",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "string" }),
        )
    }

    struct GrantedLeafTool;

    #[async_trait::async_trait]
    impl crate::ToolProvider for GrantedLeafTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![granted_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "granted_orchestration_probe")
                .then(|| Arc::new(granted_tool_definition().contract()))
        }

        async fn prepare_granted_tool_call(
            &self,
            _grant: &crate::ToolExecutionGrant,
            call: crate::ToolPrepareCall<'_>,
        ) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
            Ok(crate::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
            crate::ToolOutcome::ok(serde_json::json!("catalog leaf"))
        }

        async fn execute_granted(
            &self,
            _grant: &crate::ToolExecutionGrant,
            _args: &serde_json::Value,
            _context: &crate::AttemptContext<'_>,
        ) -> crate::ToolOutcome {
            crate::ToolOutcome::ok(serde_json::json!("granted leaf"))
        }
    }

    struct OrchestrationProbe {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::facade_support::OrchestratingToolImplementation for OrchestrationProbe {
        fn manifest(&self) -> crate::ToolManifest {
            granted_tool_definition().manifest()
        }

        fn contract(&self) -> Arc<crate::ToolContract> {
            Arc::new(granted_tool_definition().contract())
        }

        async fn execute(
            &self,
            _args: &serde_json::Value,
            _context: &crate::facade_support::OrchestrationContext<'_>,
        ) -> crate::ToolOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            crate::ToolOutcome::ok(serde_json::json!("orchestrated"))
        }
    }

    fn granted_call_context(
        event_tx: tokio::sync::mpsc::Sender<crate::SessionStreamEvent>,
    ) -> (crate::RuntimeExecutionContext<'static>, Arc<AtomicUsize>) {
        let executions = Arc::new(AtomicUsize::default());
        let orchestrating =
            crate::facade_support::OrchestratingToolDef::new(Arc::new(OrchestrationProbe {
                executions: Arc::clone(&executions),
            }));
        let registry = crate::ToolRegistry::from_tool_registrations_with_hidden_tools(
            Vec::new(),
            vec![orchestrating],
            BTreeSet::new(),
        )
        .expect("orchestration probe registry");
        let plugins = crate::plugin::PluginHost::empty()
            .build_session("granted-call-session")
            .expect("plugin session");
        let attachment_store = Arc::new(crate::SessionAttachmentStore::in_memory());
        let host = Arc::new(crate::testing::MockSessionManager::default());
        let dispatch = crate::tool_dispatch::ToolDispatchContext {
            plugins,
            tools: Arc::new(GrantedLeafTool),
            tool_registry: Some(Arc::new(registry)),
            tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
                granted_tool_definition(),
            ])),
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host,
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::NativeRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "granted-call-session".to_string(),
            agent_frame_id: crate::FrameNodeId::default(),
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
        (
            crate::RuntimeExecutionContext::new(
                "granted-call-session".to_string(),
                Arc::new(dispatch),
                Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
                attachment_store,
                Arc::new(crate::ChronologicalProjection::default()),
                None,
                crate::TurnContext::default(),
            ),
            executions,
        )
    }

    fn granted_call() -> crate::ToolExecutionGrant {
        crate::ToolExecutionGrant::from_definition(granted_tool_definition())
    }

    #[tokio::test]
    async fn scalar_granted_call_never_orchestrates() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let (context, orchestration_executions) = granted_call_context(event_tx);

        let reply = context
            .call_tool_with_execution_grant(
                "scalar-granted".to_string(),
                granted_call(),
                serde_json::json!({}),
                0,
            )
            .await;

        assert_eq!(
            orchestration_executions.load(Ordering::SeqCst),
            0,
            "grant authority cannot enter the orchestration lane"
        );
        assert_eq!(
            reply.output.value_for_projection(),
            serde_json::json!("granted leaf")
        );
    }

    #[tokio::test]
    async fn batch_granted_call_never_orchestrates() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let (context, orchestration_executions) = granted_call_context(event_tx);

        let replies = context
            .call_tool_batch(vec![
                ToolInvocation::new(
                    "batch-granted",
                    crate::ToolId::from("tool:granted_orchestration_probe"),
                    serde_json::json!({}),
                )
                .with_execution_grant(granted_call()),
            ])
            .await;

        assert_eq!(
            orchestration_executions.load(Ordering::SeqCst),
            0,
            "grant authority cannot enter the batch child's orchestration lane"
        );
        assert_eq!(
            replies.replies[0].output.value_for_projection(),
            serde_json::json!("granted leaf")
        );
    }

    #[tokio::test]
    async fn batch_with_unresolvable_tool_ids_emits_per_call_completion_correlations() {
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::channel(8);
        let context = batch_failure_context(Arc::new(BatchFailureEffectController::new(
            BatchFailureResponse::EffectDecodeError,
        )))
        .with_turn_event_sender(turn_tx);

        context
            .call_tool_batch(vec![
                ToolInvocation::new(
                    "missing-call-a",
                    crate::ToolId::from("tool:missing-a"),
                    serde_json::json!({}),
                ),
                ToolInvocation::new(
                    "missing-call-b",
                    crate::ToolId::from("tool:missing-b"),
                    serde_json::json!({}),
                ),
                ToolInvocation::new(
                    "invalid-prepared",
                    crate::ToolId::from("tool:batch_failure"),
                    serde_json::Value::Null,
                ),
            ])
            .await;

        let first = turn_rx.recv().await.expect("first completion activity");
        let second = turn_rx.recv().await.expect("second completion activity");
        let third = turn_rx.recv().await.expect("third completion activity");
        assert_ne!(first.correlation_id, second.correlation_id);
        assert_ne!(second.correlation_id, third.correlation_id);
        assert_ne!(first.correlation_id, third.correlation_id);
        assert!(matches!(
            first.event,
            crate::TurnEvent::ToolCallCompleted {
                call_id: Some(ref call_id),
                ..
            } if call_id == "missing-call-a"
        ));
        assert!(matches!(
            second.event,
            crate::TurnEvent::ToolCallCompleted {
                call_id: Some(ref call_id),
                ..
            } if call_id == "missing-call-b"
        ));
        assert_eq!(
            first.correlation_id,
            crate::TurnActivityId::new("tool:missing-call-a")
        );
        assert_eq!(
            second.correlation_id,
            crate::TurnActivityId::new("tool:missing-call-b")
        );
        assert!(matches!(
            third.event,
            crate::TurnEvent::ToolCallCompleted {
                call_id: Some(ref call_id),
                ..
            } if call_id == "invalid-prepared"
        ));
        assert_eq!(
            third.correlation_id,
            crate::TurnActivityId::new("tool:invalid-prepared")
        );
    }

    struct StartEventTranscriptSink {
        stream_rx: Mutex<tokio::sync::mpsc::Receiver<crate::SessionStreamEvent>>,
        turn_rx: Mutex<tokio::sync::mpsc::Receiver<crate::TurnActivity>>,
        lines: Mutex<Vec<&'static str>>,
    }

    impl lash_trace::TraceSink for StartEventTranscriptSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            let stream_event = self
                .stream_rx
                .lock_recover()
                .try_recv()
                .expect("stream start must be queued before the trace start");
            assert!(matches!(
                stream_event,
                crate::SessionStreamEvent::ToolCallStart {
                    call_id: Some(ref call_id),
                    ref name,
                    ref args,
                } if call_id == "start-order"
                    && name == "granted_orchestration_probe"
                    && args == &serde_json::json!({ "probe": true })
            ));
            assert!(matches!(
                self.turn_rx.lock_recover().try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            assert!(matches!(
                record.event,
                lash_trace::TraceEvent::ToolCallStarted {
                    call_id: Some(ref call_id),
                    ref name,
                    ref args,
                } if call_id == "start-order"
                    && name == "granted_orchestration_probe"
                    && args == &serde_json::json!({ "probe": true })
            ));
            self.lines
                .lock_recover()
                .extend(["stream ToolCallStart", "trace ToolCallStarted"]);
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_event_transcript_preserves_stream_trace_activity_order() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let (turn_tx, turn_rx) = tokio::sync::mpsc::channel(1);
        let (context, _) = granted_call_context(event_tx);
        let sink = Arc::new(StartEventTranscriptSink {
            stream_rx: Mutex::new(event_rx),
            turn_rx: Mutex::new(turn_rx),
            lines: Mutex::new(Vec::new()),
        });
        let trace_sink: Arc<dyn lash_trace::TraceSink> = sink.clone();
        let tracing = crate::session::execution_context::RuntimeExecutionTracing::new(
            trace_sink,
            lash_trace::TraceContext::default(),
            lash_trace::TraceContext::default(),
        );
        let context = context
            .with_tracing(Some(tracing))
            .with_turn_event_sender(turn_tx);

        context
            .emit_tool_call_started(
                "start-order",
                "granted_orchestration_probe",
                serde_json::json!({ "probe": true }),
                crate::TurnActivityId::new("tool:start-order"),
            )
            .await;

        let activity = sink
            .turn_rx
            .lock_recover()
            .try_recv()
            .expect("turn activity follows the trace start");
        assert!(matches!(
            activity.event,
            crate::TurnEvent::ToolCallStarted {
                call_id: Some(ref call_id),
                ref name,
                ref args,
                graph_key: None,
                parent_call_id: None,
            } if call_id == "start-order"
                && name == "granted_orchestration_probe"
                && args == &serde_json::json!({ "probe": true })
        ));
        sink.lines.lock_recover().push("activity ToolCallStarted");
        let transcript = sink.lines.lock_recover().join("\n");

        insta::assert_snapshot!(transcript, @r#"
        stream ToolCallStart
        trace ToolCallStarted
        activity ToolCallStarted
        "#);
    }

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
        .build_session("session")
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
            agent_frame_id: crate::FrameNodeId::default(),
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
