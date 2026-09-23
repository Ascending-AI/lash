//! The tool-batch surface of [`RuntimeExecutionContext`].
//!
//! One source-ordered batch is prepared, opened as a durable effect group of
//! tool children (ADR 0099 §3), and its settlements are returned in caller
//! order beside the order the group settled them in (§5). It lives beside the
//! rest of tool execution rather than inside it because it is the one tenant
//! with its own settlement rules — and because the two together outgrew the
//! file-size budget.

use super::*;

use super::group::PreparedToolChildLeaf;

impl RuntimeExecutionContext<'_> {
    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn tool_batch_invocation(&self, batch_id: &str) -> crate::RuntimeEffectInvocation {
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

        for (index, mut call) in calls.into_iter().enumerate() {
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
                replies[index] = Some(
                    ToolInvocationReply::from_output(completed.completed.output)
                        .with_record(completed.record),
                );
                settled_during_preparation.push(index);
                continue;
            };
            let pending = crate::sansio::PendingToolCall {
                call_id: call.id.clone(),
                tool_name: manifest.name.clone(),
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
                        manifest,
                    ));
                }
                ToolPreparationOutcome::Completed(outcome) => {
                    let completed = context
                        .complete_undispatched_tool_call(call.id, None, *outcome)
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
            // ADR 0099: the batch opens as a durable effect group of
            // `ToolInvocation` children and the consumer observes settlement
            // rank — durable final-commit order — rather than a source-ordered
            // launch vector (§5).
            let group_invocation = self.tool_batch_invocation(&batch_id);
            let batch = crate::PreparedToolBatch::new_with_grants(
                batch_id.clone(),
                prepared_entries
                    .iter()
                    .map(|(_, prepared, authorization, _, _)| {
                        (prepared.clone(), authorization.execution_grant().cloned())
                    })
                    .collect(),
            );
            let mut leaves = Vec::with_capacity(prepared_entries.len());
            for ((index, _, authorization, _, manifest), call) in
                prepared_entries.iter().zip(batch.calls)
            {
                let admission = match authorization {
                    ToolCallAuthorization::Granted(grant) => {
                        crate::runtime::effect::ToolChildAdmission::Granted {
                            grant: grant.clone(),
                        }
                    }
                    ToolCallAuthorization::Catalog(_) => {
                        crate::runtime::effect::ToolChildAdmission::Catalog {
                            manifest: Box::new(manifest.clone()),
                        }
                    }
                };
                leaves.push(PreparedToolChildLeaf {
                    input_index: *index,
                    call,
                    admission,
                });
            }
            let handle = match self
                .open_tool_child_group(group_invocation, &batch_id, &leaves)
                .await
            {
                Ok(handle) => handle,
                Err(error) => return fail_batch(error.to_string(), &mut replies),
            };
            let mut settled = match self
                .consume_all_tool_child_settlements(handle, &leaves)
                .await
            {
                Ok(settled) => settled,
                Err(error) => return fail_batch(error.to_string(), &mut replies),
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
                    .map(|position| leaves[*position].input_index),
            );
            for (position, leaf) in leaves.iter().enumerate() {
                let Some(completed) = settled.settled[position].take() else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use lash_sansio::sync::MutexExt as _;
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

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!("granted leaf")).into()
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
        let registry = crate::ToolRegistry::from_tool_registrations(
            Vec::new(),
            Vec::new(),
            vec![orchestrating],
        )
        .expect("orchestration probe registry");
        let plugins = crate::plugin::PluginHost::empty()
            .build_session("granted-call-session")
            .expect("plugin session");
        let attachment_store = Arc::new(crate::SessionAttachmentStore::in_memory());
        let host = Arc::new(crate::testing::MockSessionManager::default());
        let controller = Arc::new(crate::NativeRuntimeEffectController::default());
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
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::Shared {
                controller: controller.clone(),
                admitted: crate::AdmittedScope::turn(
                    SessionId::from("granted-call-session"),
                    crate::TurnId::from("test-turn"),
                ),
            },
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: SessionId::from("granted-call-session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            event_tx,
            turn_activity_tx: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        };
        let process_env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new());
        let dispatch = Arc::new(dispatch);
        let effect_host: Arc<dyn crate::EffectHost> = Arc::new(
            crate::runtime::NativeEffectHost::with_native_controller(controller),
        );
        let wiring =
            crate::testing::wire_test_tool_children(&dispatch, &process_env_store, &effect_host);
        let mut context = crate::RuntimeExecutionContext::new(
            SessionId::from("granted-call-session"),
            dispatch,
            process_env_store,
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );
        context = context.with_tool_child_host(effect_host);
        if let Some((guard, issuer)) = wiring {
            context = context.with_live_opener_guard(Arc::new(guard));
            if let Some(issuer) = issuer {
                context = context.with_tool_child_completion_issuer(issuer);
            }
        }
        (context, executions)
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
            .call_tool_batch(
                vec![
                    ToolInvocation::new(
                        "batch-granted",
                        crate::ToolId::from("tool:granted_orchestration_probe"),
                        serde_json::json!({}),
                    )
                    .with_execution_grant(granted_call()),
                ],
                crate::session::ToolGroupOccurrence::Opener(1),
            )
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

    #[derive(Default)]
    struct ToolLifecycleTraceSink {
        lifecycle: Mutex<Vec<(String, &'static str, Option<String>)>>,
    }

    impl lash_trace::TraceSink for ToolLifecycleTraceSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            let entry = match &record.event {
                lash_trace::TraceEvent::ToolCallStarted {
                    call_id: Some(call_id),
                    issuing_node_id,
                    ..
                } => Some((call_id.clone(), "started", issuing_node_id.clone())),
                lash_trace::TraceEvent::ToolCallCompleted {
                    call_id: Some(call_id),
                    issuing_node_id,
                    ..
                } => Some((call_id.clone(), "completed", issuing_node_id.clone())),
                _ => None,
            };
            if let Some(entry) = entry {
                self.lifecycle.lock_recover().push(entry);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn batch_failures_before_dispatch_emit_ordered_per_call_lifecycle_pairs() {
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::channel(8);
        let trace_sink = Arc::new(ToolLifecycleTraceSink::default());
        let erased_trace_sink: Arc<dyn lash_trace::TraceSink> = trace_sink.clone();
        let tracing = crate::session::execution_context::RuntimeExecutionTracing::new(
            erased_trace_sink,
            lash_trace::TraceContext::default(),
            lash_trace::TraceContext::default(),
        );
        let context = batch_failure_context(Arc::new(BatchFailureEffectController))
            .with_turn_event_sender(turn_tx)
            .with_tracing(Some(tracing));

        context
            .call_tool_batch(
                vec![
                    ToolInvocation::new(
                        "missing-call-a",
                        crate::ToolId::from("tool:missing-a"),
                        serde_json::json!({}),
                    )
                    .with_issuing_language_node_id("node-a"),
                    ToolInvocation::new(
                        "missing-call-b",
                        crate::ToolId::from("tool:missing-b"),
                        serde_json::json!({}),
                    )
                    .with_issuing_language_node_id("node-b"),
                    ToolInvocation::new(
                        "invalid-prepared",
                        crate::ToolId::from("tool:batch_failure"),
                        serde_json::Value::Null,
                    )
                    .with_issuing_language_node_id("node-invalid"),
                ],
                crate::session::ToolGroupOccurrence::Opener(1),
            )
            .await;

        // A call that settles before provider dispatch is still a complete
        // lifecycle attempt. Each call id therefore owns one ordered Started
        // then Completed pair; the failure path must never publish a bare
        // completion or borrow another call's correlation.
        for (call_id, node_id) in [
            ("missing-call-a", "node-a"),
            ("missing-call-b", "node-b"),
            ("invalid-prepared", "node-invalid"),
        ] {
            let started = turn_rx.recv().await.expect("tool start activity");
            let completed = turn_rx.recv().await.expect("tool completion activity");
            let correlation_id = crate::TurnActivityId::new(format!("tool:{call_id}"));
            assert_eq!(started.correlation_id, correlation_id);
            assert_eq!(completed.correlation_id, correlation_id);
            assert!(matches!(
                started.event,
                crate::TurnEvent::ToolCallStarted {
                    call_id: Some(ref observed),
                    ..
                } if observed == call_id
            ));
            assert!(matches!(
                completed.event,
                crate::TurnEvent::ToolCallCompleted {
                    call_id: Some(ref observed),
                    ..
                } if observed == call_id
            ));
            let trace_lifecycle = trace_sink
                .lifecycle
                .lock_recover()
                .iter()
                .filter_map(|(observed, event, issuing_node_id)| {
                    (observed == call_id).then_some((*event, issuing_node_id.clone()))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                trace_lifecycle,
                [
                    ("started", Some(node_id.to_string())),
                    ("completed", Some(node_id.to_string())),
                ],
                "exactly one ordered trace pair keyed by {call_id} and linked to {node_id}"
            );
        }
        assert!(
            turn_rx.try_recv().is_err(),
            "exactly one pair per failed call"
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
                    ..
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

    /// A controller with no group substrate: formation of the batch's group
    /// fails, and the batch surface must fail closed rather than report any
    /// settlement.
    struct BatchFailureEffectController;

    impl crate::AwaitEventResolver for BatchFailureEffectController {}

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for BatchFailureEffectController {
        async fn execute_effect(
            &self,
            envelope: crate::RuntimeEffectEnvelope,
            local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            // A call settled during preparation still journals its
            // presentation boundary (FIG-3420) through this controller; run
            // its executor rather than synthesizing a presentation here.
            local_executor.execute(envelope).await
        }

        async fn open_effect_group(
            &self,
            _group: crate::RuntimeEffectGroup,
        ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
            Err(crate::effect_groups_unsupported(
                "BatchFailureEffectController",
            ))
        }

        async fn await_next_settlement(
            &self,
            _handle: &mut crate::EffectGroupHandle,
            _cancel: crate::CancellationToken,
        ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
            Err(crate::effect_groups_unsupported(
                "BatchFailureEffectController",
            ))
        }

        async fn close_effect_group(
            &self,
            _handle: crate::EffectGroupHandle,
            _disposition: crate::LoserPolicy,
        ) -> Result<(), crate::RuntimeEffectControllerError> {
            Err(crate::effect_groups_unsupported(
                "BatchFailureEffectController",
            ))
        }

        async fn commit_group_child_final(
            &self,
            _commit: crate::runtime::effect::GroupChildFinalCommit,
        ) -> Result<
            crate::runtime::effect::EffectGroupChildCommitOutcome,
            crate::RuntimeEffectControllerError,
        > {
            Ok(crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped)
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

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!("not reached")).into()
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
            .resolved_tool_catalog(&SessionId::from("session"))
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
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(controller),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            event_tx,
            turn_activity_tx: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        };
        crate::RuntimeExecutionContext::new(
            SessionId::from("session"),
            Arc::new(dispatch),
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
    }

    #[tokio::test]
    async fn failed_group_formation_returns_empty_settlement_order() {
        let context = batch_failure_context(Arc::new(BatchFailureEffectController));
        let replies = context
            .call_tool_batch(
                vec![ToolInvocation::new(
                    "call",
                    crate::ToolId::from("tool:batch_failure"),
                    serde_json::json!({}),
                )],
                crate::session::ToolGroupOccurrence::Opener(1),
            )
            .await;

        assert_eq!(replies.replies.len(), 1, "one reply per input call");
        assert!(
            !replies.replies[0].output.is_success(),
            "a failed group formation must fail the reply"
        );
        assert!(
            replies.settlement_order.is_empty(),
            "a failed batch reports no settled calls"
        );
        let message = replies.replies[0].output.value_for_projection()["message"]
            .as_str()
            .expect("failure message")
            .to_string();
        assert!(message.starts_with("tool batch failed: "), "{message}");
    }
}
