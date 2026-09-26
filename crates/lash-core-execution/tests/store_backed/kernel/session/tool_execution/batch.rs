mod tests {
    use crate::SessionId;
    use crate::session::*;
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

    async fn granted_call_context(
        observer: Arc<dyn crate::engine::ObservationSink>,
    ) -> (crate::RuntimeExecutionContext<'static>, Arc<AtomicUsize>) {
        let executions = Arc::new(AtomicUsize::default());
        let orchestrating =
            crate::facade_support::OrchestratingToolDef::new(Arc::new(OrchestrationProbe {
                executions: Arc::clone(&executions),
            }));
        let registry =
            crate::tool_registry_from_registrations(Vec::new(), Vec::new(), vec![orchestrating])
                .expect("orchestration probe registry");
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session("granted-call-session")
            .expect("plugin session");
        let backend = crate::support::memory_backend().await;
        let attachment_store = Arc::new(crate::SessionAttachmentStore::ephemeral(
            crate::Backend::attachment_store(&backend),
        ));
        let host = Arc::new(crate::testing::MockSessionManager::default());
        let controller = crate::support::scoped_controller(
            &backend,
            crate::AdmittedScope::turn(
                SessionId::from("granted-call-session"),
                crate::TurnId::from("test-turn"),
            ),
        );
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
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: SessionId::from("granted-call-session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            observer,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        };
        let process_env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            backend.process_env_store();
        let dispatch = Arc::new(dispatch);
        let effect_host: Arc<dyn crate::EffectHost> = backend.effect_host();
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
        if let Some(guard) = wiring {
            context = context.with_live_opener_guard(Arc::new(guard));
        }
        (context, executions)
    }

    fn granted_call() -> crate::ToolExecutionGrant {
        crate::ToolExecutionGrant::from_definition(granted_tool_definition())
    }

    #[tokio::test]
    async fn scalar_granted_call_never_orchestrates() {
        let (context, orchestration_executions) =
            granted_call_context(crate::engine::NullObservationSink::arc()).await;

        let reply = context
            .call_command_tool(
                &crate::CommandReplayKey::new("scalar-granted"),
                ToolInvocation::new(
                    "scalar-granted",
                    crate::ToolId::from("tool:granted_orchestration_probe"),
                    serde_json::json!({}),
                )
                .with_execution_grant(granted_call()),
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
        let (context, orchestration_executions) =
            granted_call_context(crate::engine::NullObservationSink::arc()).await;

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
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        let trace_sink = Arc::new(ToolLifecycleTraceSink::default());
        let erased_trace_sink: Arc<dyn lash_trace::TraceSink> = trace_sink.clone();
        let tracing = crate::session::RuntimeExecutionTracing::new(
            erased_trace_sink,
            lash_trace::TraceContext::default(),
            lash_trace::TraceContext::default(),
        );
        let context = batch_failure_context(
            Arc::new(BatchFailureEffectController),
            crate::testing::ChannelObservationSink::new(None, Some(turn_tx)),
        )
        .with_tracing(Some(tracing));

        context
            .call_tool_batch(vec![
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
            ])
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
        stream_rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<crate::SessionStreamEvent>>,
        turn_rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<crate::TurnActivity>>,
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
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (turn_tx, turn_rx) = tokio::sync::mpsc::unbounded_channel();
        let (context, _) = granted_call_context(crate::testing::ChannelObservationSink::new(
            Some(event_tx),
            Some(turn_tx),
        ))
        .await;
        let sink = Arc::new(StartEventTranscriptSink {
            stream_rx: Mutex::new(event_rx),
            turn_rx: Mutex::new(turn_rx),
            lines: Mutex::new(Vec::new()),
        });
        let trace_sink: Arc<dyn lash_trace::TraceSink> = sink.clone();
        let tracing = crate::session::RuntimeExecutionTracing::new(
            trace_sink,
            lash_trace::TraceContext::default(),
            lash_trace::TraceContext::default(),
        );
        let context = context.with_tracing(Some(tracing));

        crate::emit_tool_call_started(
            &context,
            "test:start-order",
            "start-order",
            "granted_orchestration_probe",
            serde_json::json!({ "probe": true }),
            crate::TurnActivityId::new("tool:start-order"),
        );

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

    impl crate::AwaitEventResolver for BatchFailureEffectController {
        /// A test double that mints keys under no durable authority.
        fn await_event_authority_binding_id(&self) -> Option<String> {
            None
        }
    }

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
            _cancel: crate::runtime::TurnCancelWait,
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
        observer: Arc<dyn crate::engine::ObservationSink>,
    ) -> crate::RuntimeExecutionContext<'static> {
        let provider: Arc<dyn crate::ToolProvider> = Arc::new(BatchFailureTools);
        let plugins =
            crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
                "batch_failure_tools",
                crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
            ))])
            .build_session("session")
            .expect("plugin session");
        let tools = plugins.tools();
        let tool_catalog = plugins
            .resolved_tool_catalog(&SessionId::from("session"))
            .expect("tool catalog");
        let attachment_store: Arc<crate::SessionAttachmentStore> =
            Arc::new(crate::SessionAttachmentStore::unavailable());
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
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            observer,
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
            Arc::new(crate::testing::UnavailableProcessExecutionEnvStore),
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
    }

    #[tokio::test]
    async fn failed_group_formation_returns_empty_settlement_order() {
        let context = batch_failure_context(
            Arc::new(BatchFailureEffectController),
            crate::engine::NullObservationSink::arc(),
        );
        let replies = context
            .call_tool_batch(vec![ToolInvocation::new(
                "call",
                crate::ToolId::from("tool:batch_failure"),
                serde_json::json!({}),
            )])
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
