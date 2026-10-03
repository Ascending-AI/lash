mod tests {
    use crate::SessionId;
    use crate::plugin::PluginSessionRequest;
    use crate::session::*;
    use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
    use lash_sansio::sync::MutexExt as _;
    use std::sync::Arc;
    use std::sync::Mutex;

    const SEED: u64 = 0x5_f730;

    fn granted_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:granted_leaf_probe",
            "granted_leaf_probe",
            "Proves granted calls run the granted leaf",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
    }

    struct GrantedLeafTool;

    #[async_trait::async_trait]
    impl crate::ToolProvider for GrantedLeafTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![granted_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "granted_leaf_probe").then(|| Arc::new(granted_tool_definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!("granted leaf")).into()
        }
    }

    async fn granted_call_context_over<'run>(
        backend: &crate::Backend,
        scoped: crate::ScopedEffectController<'run>,
        observer: Arc<dyn crate::engine::ObservationSink>,
        tools: Arc<dyn crate::ToolProvider>,
    ) -> crate::RuntimeExecutionContext<'run> {
        let plugins =
            crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
                crate::plugin::PluginDeclaration::initial("granted_tools"),
                crate::plugin::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
            ))])
            .build_session(PluginSessionRequest::creation(
                "granted-call-session",
                Default::default(),
            ))
            .expect("plugin session");
        let attachment_store = Arc::new(crate::RuntimeAttachmentStore::ephemeral(
            backend.attachment_store(),
        ));
        let host = Arc::new(crate::testing::MockSessionManager::default());
        let dispatch = crate::tool_dispatch::ToolDispatchContext {
            plugins,
            tools,
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
                granted_tool_definition(),
            ])),
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host,
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: scoped,
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: SessionId::from("granted-call-session"),
                agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            },
            observer,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
            tool_receipts: None,
        };
        let process_env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            backend.process_env_store();
        let dispatch = Arc::new(dispatch);
        let effect_host: Arc<dyn crate::EffectHost> = backend.effect_host();
        let wiring =
            crate::testing::wire_test_tool_children(&dispatch, &process_env_store, &effect_host);
        let mut context = crate::RuntimeExecutionContext::new(
            dispatch,
            process_env_store,
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            crate::TurnContext::default(),
            crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
        );
        context = context.with_tool_child_host(effect_host);
        if let Some(guard) = wiring {
            context = context.with_live_opener_guard(Arc::new(guard));
        }
        context
    }

    fn granted_call() -> crate::ToolExecutionGrant {
        crate::ToolExecutionGrant::from_definition(
            crate::plugin::PluginRevision::new(
                "granted_tools",
                crate::plugin::BehaviorRevision::ONE,
            ),
            granted_tool_definition(),
        )
    }

    /// The granted leaf, counting every body that runs.
    struct CountingLeafTool {
        executions: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for CountingLeafTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            GrantedLeafTool.tool_manifests()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            GrantedLeafTool.resolve_contract(name)
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            self.executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            GrantedLeafTool.execute(call).await
        }
    }

    /// K1: a round is admitted whole. One member declared isolated, which no
    /// process implementation runs, refuses every member of its batch before
    /// any prepares or starts: no body runs, each member answers the typed
    /// admission refusal, and the plain sibling names the refused member.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_refused_member_starts_no_member_of_its_batch() {
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let handler = double
            .open_handler(crate::AdmittedScope::turn(
                SessionId::from("granted-call-session"),
                crate::TurnId::from("refused-round-turn"),
            ))
            .await
            .expect("open the refused-round handler");
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let context = granted_call_context_over(
            &backend,
            handler.scoped(),
            crate::engine::NullObservationSink::arc(),
            Arc::new(CountingLeafTool {
                executions: Arc::clone(&executions),
            }),
        )
        .await;
        let isolated = crate::ToolExecutionGrant::from_definition(
            crate::plugin::PluginRevision::new(
                "granted_tools",
                crate::plugin::BehaviorRevision::ONE,
            ),
            granted_tool_definition().with_declaration(crate::ToolDeclaration {
                isolated: true,
                ..crate::ToolDeclaration::default()
            }),
        );

        let replies = context
            .call_tool_batch(vec![
                ToolInvocation::new(
                    lash_core_execution::ToolCallId::fixture("round-plain"),
                    crate::ToolId::from("tool:granted_leaf_probe"),
                    serde_json::json!({}),
                )
                .with_execution_grant(granted_call()),
                ToolInvocation::new(
                    lash_core_execution::ToolCallId::fixture("round-isolated"),
                    crate::ToolId::from("tool:granted_leaf_probe"),
                    serde_json::json!({}),
                )
                .with_execution_grant(isolated),
            ])
            .await;

        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no member of a refused round runs its body"
        );
        let refusals: Vec<_> = replies
            .replies
            .iter()
            .map(|reply| match &reply.output.outcome {
                crate::ToolCallOutcome::Failure(failure) => {
                    assert_eq!(failure.code, crate::ToolAdmissionRefusal::CODE);
                    failure.cause.as_deref().cloned()
                }
                other => panic!("a refused member answers its refusal, not {other:?}"),
            })
            .collect();
        assert_eq!(
            refusals,
            vec![
                Some(crate::ToolFailureCause::Admission {
                    refusal: crate::ToolAdmissionRefusal::Sibling { member: 1 },
                }),
                Some(crate::ToolFailureCause::Admission {
                    refusal: crate::ToolAdmissionRefusal::UnsupportedIsolation,
                }),
            ]
        );
        assert_eq!(
            replies.settlement_order,
            vec![0, 1],
            "a refused round settles in source order before any dispatch"
        );
        drop(context);
        handler
            .close()
            .await
            .expect("close the refused-round handler");
    }

    #[derive(Default)]
    struct ToolLifecycleTraceSink {
        lifecycle: Mutex<Vec<(String, &'static str, Option<String>)>>,
        records: Mutex<Vec<lash_trace::TraceRecord>>,
    }

    impl lash_trace::TraceSink for ToolLifecycleTraceSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            let entry = match &record.event {
                lash_trace::TraceEvent::ToolCallStarted {
                    call_id,
                    issuing_node_id,
                    ..
                } => Some((call_id.to_string(), "started", issuing_node_id.clone())),
                lash_trace::TraceEvent::ToolCallCompleted {
                    call_id,
                    issuing_node_id,
                    ..
                } => Some((call_id.to_string(), "completed", issuing_node_id.clone())),
                _ => None,
            };
            if let Some(entry) = entry {
                self.lifecycle.lock_recover().push(entry);
                self.records.lock_recover().push(record.clone());
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn batch_failures_before_dispatch_emit_ordered_per_call_lifecycle_pairs() {
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        let trace_sink = Arc::new(ToolLifecycleTraceSink::default());
        let erased_trace_sink: Arc<dyn lash_trace::TraceSink> = trace_sink.clone();
        let runtime = crate::trace::TraceRuntime::default().with_trace_sink(erased_trace_sink);
        let stores = crate::support::sqlite_memory_store_set().await;
        let factory = stores.session_store_factory();
        crate::SessionCatalogStore::admit_session(
            factory.as_ref(),
            &crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from("session"),
                relation: crate::SessionRelation::Root,
                config: crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
                .into(),
                head: crate::SessionCreationHead::Config,
            },
        )
        .await
        .expect("admit the receipt owner");
        let tool_receipts: Arc<dyn crate::RuntimeStore> = factory;
        let scope = lash_trace::DurableTraceScope {
            scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                session_id: SessionId::from("session"),
                turn_id: crate::TurnId::fixture("refused-turn"),
            }),
            cause: lash_trace::TraceCause::Root,
            anchor: lash_trace::TraceAnchor::Untraced,
            started_at_ms: 1,
        };
        let tracing = crate::session::RuntimeExecutionTracing::new(
            runtime.clone(),
            Some(scope.clone()),
            lash_trace::TraceContext::default(),
        );
        let observer = crate::testing::ChannelObservationSink::new(None, Some(turn_tx));
        let context = batch_failure_context(
            Arc::new(BatchFailureEffectController),
            observer.clone(),
            Some(tool_receipts.clone()),
        )
        .with_tracing(Some(tracing.clone()))
        .with_trace_standing(runtime.unreplayed(Some(scope.clone())));
        let calls = vec![
            ToolInvocation::new(
                lash_core_execution::ToolCallId::fixture("missing-call-a"),
                crate::ToolId::from("tool:missing-a"),
                serde_json::json!({}),
            )
            .with_issuing_language_node_id("node-a"),
            ToolInvocation::new(
                lash_core_execution::ToolCallId::fixture("missing-call-b"),
                crate::ToolId::from("tool:missing-b"),
                serde_json::json!({}),
            )
            .with_issuing_language_node_id("node-b"),
            ToolInvocation::new(
                lash_core_execution::ToolCallId::fixture("invalid-prepared"),
                crate::ToolId::from("tool:batch_failure"),
                serde_json::Value::Null,
            )
            .with_issuing_language_node_id("node-invalid"),
        ];
        let replies = context.call_tool_batch(calls.clone()).await;
        assert!(
            replies
                .replies
                .iter()
                .all(|reply| !reply.output.is_success())
        );
        assert!(!context.has_nested_effect_error());

        // A call that settles before provider dispatch is still a complete
        // recorded lifecycle. Each call id therefore owns one ordered Started
        // then Completed pair; the failure path must never publish a bare
        // completion or borrow another call's correlation.
        for (call_id, node_id) in [
            ("missing-call-a", "node-a"),
            ("missing-call-b", "node-b"),
            ("invalid-prepared", "node-invalid"),
        ] {
            let call_id = lash_core_execution::ToolCallId::fixture(call_id);
            let started = turn_rx.recv().await.expect("tool start activity");
            let completed = turn_rx.recv().await.expect("tool completion activity");
            let correlation_id = crate::TurnActivityId::new(format!("tool:{call_id}"));
            assert_eq!(started.correlation_id, correlation_id);
            assert_eq!(completed.correlation_id, correlation_id);
            assert!(matches!(
                started.event,
                crate::TurnEvent::ToolCallStarted {
                    call_id: ref observed,
                    ..
                } if *observed == call_id
            ));
            assert!(matches!(
                completed.event,
                crate::TurnEvent::ToolCallCompleted {
                    call_id: ref observed,
                    ..
                } if *observed == call_id
            ));
            let trace_lifecycle = trace_sink
                .lifecycle
                .lock_recover()
                .iter()
                .filter_map(|(observed, event, issuing_node_id)| {
                    (*observed == call_id.to_string()).then_some((*event, issuing_node_id.clone()))
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
        let first = serde_json::to_value(&*trace_sink.records.lock_recover()).expect("first pairs");
        drop(context);
        let replay = batch_failure_context(
            Arc::new(BatchFailureEffectController),
            observer,
            Some(tool_receipts),
        )
        .with_tracing(Some(tracing))
        .with_trace_standing(runtime.unreplayed(Some(scope)));
        let replies = replay.call_tool_batch(calls).await;
        assert!(
            replies
                .replies
                .iter()
                .all(|reply| !reply.output.is_success())
        );
        assert!(!replay.has_nested_effect_error());
        assert_eq!(trace_sink.records.lock_recover().len(), 6);
        assert_eq!(
            serde_json::to_value(&*trace_sink.records.lock_recover()).expect("retained pairs"),
            first,
            "re-executing preparation through SQL retains the same identities and times"
        );
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
        .expect("valid declared tool schemas")
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
        tool_receipts: Option<Arc<dyn crate::RuntimeStore>>,
    ) -> crate::RuntimeExecutionContext<'static> {
        let provider: Arc<dyn crate::ToolProvider> = Arc::new(BatchFailureTools);
        let plugins =
            crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
                lash_core_execution::plugin::PluginDeclaration::initial("batch_failure_tools"),
                crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
            ))])
            .build_session(PluginSessionRequest::creation(
                "session",
                Default::default(),
            ))
            .expect("plugin session");
        let tools = plugins.tools();
        let tool_catalog = plugins.resolved_tool_catalog().expect("tool catalog");
        let attachment_store: Arc<crate::RuntimeAttachmentStore> =
            Arc::new(crate::RuntimeAttachmentStore::unavailable());
        let dispatch = crate::tool_dispatch::ToolDispatchContext {
            tool_receipts,
            plugins,
            tools,
            tool_registry: None,
            tool_catalog,
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::runtime::ScopedEffectController::shared(
                controller,
                crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
            )
            .expect("valid test runtime scope"),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: SessionId::from("session"),
                agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            },
            observer,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        };
        crate::RuntimeExecutionContext::new(
            Arc::new(dispatch),
            Arc::new(crate::testing::UnavailableProcessExecutionEnvStore),
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            crate::TurnContext::default(),
            crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
        )
    }

    #[tokio::test]
    async fn failed_group_formation_returns_empty_settlement_order() {
        let context = batch_failure_context(
            Arc::new(BatchFailureEffectController),
            crate::engine::NullObservationSink::arc(),
            None,
        );
        let replies = context
            .call_tool_batch(vec![ToolInvocation::new(
                lash_core_execution::ToolCallId::fixture("call"),
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
