mod tests {
    use crate::ProcessId;
    use crate::RuntimeExecutionContext;
    use crate::SessionId;
    use crate::plugin::PluginSessionRequest;

    use crate::session::ToolInvocationReply;

    use crate::tool_dispatch::ToolDispatchContext;
    use crate::{
        PreparedToolCall, ToolCall, ToolDefinition, ToolOutcome, ToolPrepareCall, ToolProvider,
    };
    use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
    use lash_sansio::sync::MutexExt as _;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn catalog_for<T: ToolProvider + ?Sized>(provider: &Arc<T>) -> crate::ToolCatalog {
        let manifests = provider.tool_manifests();
        let contracts = manifests
            .iter()
            .filter_map(|manifest| {
                provider
                    .resolve_contract(&manifest.name)
                    .map(|contract| (manifest.id.clone(), contract))
            })
            .collect();
        crate::ToolCatalog::from_tools(manifests, contracts)
            .expect("test provider exposes complete resident definitions")
    }

    struct PrepareRecordingTool {
        prepares: Arc<AtomicUsize>,
    }

    #[derive(Default)]
    struct DenyProcessAwaitAttachments {
        authorized: std::sync::Mutex<Vec<(crate::AttachmentProducer, crate::AttachmentSource)>>,
    }

    impl crate::AttachmentSourcePolicy for DenyProcessAwaitAttachments {
        fn authorize(
            &self,
            producer: &crate::AttachmentProducer,
            source: &crate::AttachmentSource,
        ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
            self.authorized
                .lock_recover()
                .push((producer.clone(), source.clone()));
            Err(crate::test_support::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "process-await test denies every attachment source".to_string(),
            })
        }
    }

    async fn await_external_process_attachment(
        source: crate::AttachmentSource,
    ) -> (
        ToolInvocationReply,
        Arc<dyn crate::RuntimeStore>,
        Arc<dyn crate::AttachmentStore>,
        Arc<DenyProcessAwaitAttachments>,
    ) {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let process = registry
            .register_process(crate::testing::held_engine_registration(
                serde_json::Value::Null,
                crate::ProcessProvenance::host(),
                crate::Lifetime::Detached,
            ))
            .await
            .expect("register held process");
        registry
            .add_observer(
                &SessionId::from("session"),
                &process.id,
                crate::ProcessObserverBy::host("process-await-attachment-test"),
            )
            .await
            .expect("observe held process");
        registry
            .complete_process(
                &process.id,
                crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                        source.clone(),
                    )),
                ),
                crate::ProcessCompletionAuthority::workflow_key(&process.id),
            )
            .await
            .expect("complete held process with attachment");

        let factory = backend.session_store_factory();
        let request = crate::SessionStoreCreateRequest {
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
        };
        crate::SessionCatalogStore::admit_session(factory.as_ref(), &request)
            .await
            .expect("create real in-memory manifest store");
        let persistence: Arc<dyn crate::RuntimeStore> = factory.clone();
        let attachment_backend = backend.attachment_store();
        let attachment_store = Arc::new(crate::RuntimeAttachmentStore::new(
            Arc::clone(&attachment_backend),
            Arc::new(crate::attachments::PersistenceReferrersAdapter(Arc::clone(
                &persistence,
            ))),
            crate::RuntimeOwner::Session(request.session_id.clone()),
        ));
        let policy = Arc::new(DenyProcessAwaitAttachments::default());
        let dispatch = Arc::new(ToolDispatchContext {
            tool_receipts: None,
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host,
            trigger_router: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::ActorContext::unavailable()
                .scoped(crate::AdmittedScope::runtime_operation(
                    "test-runtime-effect-controller",
                ))
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
                session_id: request.session_id.clone(),
                agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            },
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::clone(&policy) as Arc<dyn crate::AttachmentSourcePolicy>,
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        });
        let context = RuntimeExecutionContext::new(
            dispatch,
            backend.process_env_store(),
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
        assert!(attachment_backend.list().await.unwrap().is_empty());
        let handle = RuntimeExecutionContext::process_handle_json(&process.id.clone());
        let reply = crate::await_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("await-external-attachment"),
            handle,
        )
        .await;
        (reply, persistence, attachment_backend, policy)
    }

    async fn assert_external_process_attachment_denied(source: crate::AttachmentSource) {
        let (reply, _persistence, attachment_backend, policy) =
            await_external_process_attachment(source.clone()).await;
        let record = reply.record.expect("process await is recorded");
        assert_eq!(
            record.call_id,
            lash_core_execution::ToolCallId::fixture("await-external-attachment")
        );
        assert_eq!(record.tool, "await_process");
        let crate::ToolCallOutcome::Failure(failure) = record.output.outcome else {
            panic!("denied process attachment must replace the recorded result");
        };
        assert_eq!(failure.code, "attachment_source_policy_denied");
        assert_eq!(
            *policy.authorized.lock_recover(),
            vec![(
                crate::AttachmentProducer::Tool {
                    tool_name: "await_process".to_string(),
                },
                source,
            )],
            "the completed process attachment must be authorized as await_process output"
        );
        assert!(attachment_backend.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn external_process_inline_attachment_is_denied_before_await_recording() {
        assert_external_process_attachment_denied(crate::AttachmentSource::inline(
            crate::MediaType::parse("text/plain").unwrap(),
            b"external completion".to_vec(),
        ))
        .await;
    }

    fn process_tool_definition() -> ToolDefinition {
        ToolDefinition::raw(
            "tool:process_prepare",
            "process_prepare",
            "Records preparation before background registration.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
    }

    #[async_trait::async_trait]
    impl ToolProvider for PrepareRecordingTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![process_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "process_prepare").then(|| Arc::new(process_tool_definition().contract()))
        }

        async fn prepare_tool_call(
            &self,
            call: ToolPrepareCall<'_>,
        ) -> Result<PreparedToolCall, ToolOutcome> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedToolCall {
                call_id: call.pending.call_id.clone(),
                provider_call_id: None,
                tool_id: call.tool_id,
                tool_name: call.pending.tool_name,
                args: call.pending.args,
                replay: call.pending.replay,
                prepared_payload: serde_json::json!({ "prepared": true }),
            })
        }

        async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(serde_json::json!({
                "payload": call.context.prepared_payload().clone(),
            }))
            .into()
        }
    }

    /// An unreadable handle is refused by one rule, and the refusal is recorded.
    ///
    /// `process_handle_operations_share_one_authority_rule` covers the second
    /// gate, `authorize_handle`. The first gate — `parse_process_handle` — is
    /// three separate early returns, one per operation, and nothing drove any
    /// of them. Two things ride on that arm and neither is implied by "the call
    /// fails": the reply must carry a `ToolCallRecord` under the operation's
    /// own tool name and the caller's `call_id`, because a refused handle is
    /// turn history a later turn reads and replays, not a dropped call; and the
    /// three operations must agree on the message, because they share one
    /// parser and a caller cannot be told a handle is unreadable by `await` and
    /// readable by `cancel`.
    ///
    /// The three malformed shapes are kept distinguishable on purpose: they are
    /// the parser's three branches (`ProcessId::from_handle_json`), and a
    /// parser that collapsed them would still refuse every one of them.
    #[tokio::test]
    async fn an_unreadable_process_handle_is_refused_and_recorded_by_every_handle_operation() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::sqlite_memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let dispatch = Arc::new(ToolDispatchContext {
            tool_receipts: None,
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::ActorContext::unavailable()
                .scoped(crate::AdmittedScope::runtime_operation(
                    "test-runtime-effect-controller",
                ))
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        });
        let context = RuntimeExecutionContext::new(
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::RuntimeAttachmentStore::unavailable()),
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

        let unreadable: [(&str, serde_json::Value); 3] = [
            ("not a handle record", json!({ "id": "process-7" })),
            (
                "a handle that names no process",
                lash_sansio::handle::handle_record_json(&lash_sansio::handle::HandleId::tool(7, 1)),
            ),
            (
                "a process handle whose id no registrar minted",
                json!({ "__handle__": "lash", "id": "p.process-7" }),
            ),
        ];

        let mut messages = BTreeMap::new();
        for (shape, handle) in unreadable {
            let awaited = crate::await_process_handle(
                &context,
                lash_core_execution::ToolCallId::fixture(&format!("await-{shape}")),
                handle.clone(),
            )
            .await;
            let signalled = crate::signal_process_handle(
                &context,
                lash_core_execution::ToolCallId::fixture(&format!("signal-{shape}")),
                handle.clone(),
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
            let cancelled = crate::cancel_process_handle(
                &context,
                lash_core_execution::ToolCallId::fixture(&format!("cancel-{shape}")),
                handle.clone(),
            )
            .await;

            let parse_refusal = awaited.output.value_for_projection();
            for (operation, reply) in [
                ("await", &awaited),
                ("signal", &signalled),
                ("cancel", &cancelled),
            ] {
                assert!(
                    !reply.output.is_success(),
                    "{operation} operated on {shape}: {:?}",
                    reply.output.value_for_projection()
                );
                assert_eq!(
                    reply.output.value_for_projection(),
                    parse_refusal,
                    "{operation} must refuse {shape} with the shared parser message"
                );
                let record = reply
                    .record
                    .as_ref()
                    .unwrap_or_else(|| panic!("{operation} must record its refusal of {shape}"));
                assert_eq!(
                    record.call_id,
                    lash_core_execution::ToolCallId::fixture(&format!("{operation}-{shape}")),
                    "{operation} must record the caller's call id for {shape}"
                );
                assert_eq!(
                    record.tool,
                    format!("{operation}_process"),
                    "{operation} must record its refusal under its own tool name"
                );
                assert_eq!(
                    record.args.get("handle"),
                    Some(&handle),
                    "{operation} must record the handle it could not read"
                );
            }
            assert!(
                parse_refusal.to_string().contains("Invalid process handle"),
                "{shape} must render the parser's refusal: {parse_refusal}"
            );
            messages.insert(parse_refusal.to_string(), shape);
        }
        assert_eq!(
            messages.len(),
            3,
            "each unreadable shape must stay distinguishable: {messages:?}"
        );
    }

    #[tokio::test]
    async fn process_handle_operations_share_one_authority_rule() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::sqlite_memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let hidden_process = registry
            .register_process(
                crate::testing::held_engine_registration(
                    serde_json::Value::Null,
                    crate::ProcessProvenance::host(),
                    crate::Lifetime::Detached,
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::JsonSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register hidden process");
        let dispatch = Arc::new(ToolDispatchContext {
            tool_receipts: None,
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::ActorContext::unavailable()
                .scoped(crate::AdmittedScope::runtime_operation(
                    "test-runtime-effect-controller",
                ))
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        });
        let context = RuntimeExecutionContext::new(
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::RuntimeAttachmentStore::unavailable()),
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
        let handle = lash_sansio::handle::handle_record_json(
            &lash_sansio::handle::HandleId::process(&hidden_process.id),
        );

        let awaited = crate::await_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("await-hidden-process"),
            handle.clone(),
        )
        .await;
        let signalled = crate::signal_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("signal-hidden-process"),
            handle.clone(),
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        let cancelled = crate::cancel_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("cancel-hidden-process"),
            handle.clone(),
        )
        .await;

        let visibility_miss = awaited.output.value_for_projection();
        for (operation, reply) in [
            ("await", &awaited),
            ("signal", &signalled),
            ("cancel", &cancelled),
        ] {
            assert!(
                !reply.output.is_success(),
                "{operation} unexpectedly operated an unpossessed, unobserved handle"
            );
            let error = reply.output.value_for_projection();
            assert_eq!(
                error, visibility_miss,
                "{operation} must return the exact same typed visibility miss"
            );
            assert!(
                error.to_string().contains(&format!(
                    "process handle `{}` is not live or visible in this session",
                    hidden_process.id
                )),
                "{operation} must return the shared typed visibility miss: {error}"
            );
        }
        assert_eq!(
            awaited.record.as_ref().map(|record| record.call_id.clone()),
            Some(lash_core_execution::ToolCallId::fixture(
                "await-hidden-process"
            )),
        );
        assert_eq!(
            cancelled
                .record
                .as_ref()
                .map(|record| record.call_id.clone()),
            Some(lash_core_execution::ToolCallId::fixture(
                "cancel-hidden-process"
            )),
        );

        let mut local_ids = BTreeMap::new();
        for label in ["local-signal", "local-cancel", "local-await"] {
            let mut registration = crate::testing::held_engine_registration(
                serde_json::Value::Null,
                crate::ProcessProvenance::host(),
                crate::Lifetime::Detached,
            );
            if label == "local-signal" {
                registration = registration.with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::JsonSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]);
            }
            let record = registry
                .register_process(registration)
                .await
                .expect("register run-local process without observer edge");
            crate::record_started_process(&context, &record.id);
            local_ids.insert(label, record.id);
        }
        registry
            .complete_process(
                &local_ids["local-await"],
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                    "local done"
                ))),
                crate::ProcessCompletionAuthority::workflow_key(&local_ids["local-await"]),
            )
            .await
            .expect("complete run-local await process");
        let local_handle = |process_id: &ProcessId| {
            lash_sansio::handle::handle_record_json(&lash_sansio::handle::HandleId::process(
                process_id,
            ))
        };
        let local_signal = crate::signal_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("signal-local"),
            local_handle(&local_ids["local-signal"]),
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        let local_cancel = crate::cancel_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("cancel-local"),
            local_handle(&local_ids["local-cancel"]),
        )
        .await;
        let local_await = crate::await_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("await-local"),
            local_handle(&local_ids["local-await"]),
        )
        .await;
        for (operation, reply) in [
            ("await", &local_await),
            ("signal", &local_signal),
            ("cancel", &local_cancel),
        ] {
            assert!(
                reply.output.is_success(),
                "{operation} must accept run-local possession without an observer edge: {:?}",
                reply.output.value_for_projection()
            );
        }
        let local_prune = registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune terminal run-local fixtures");
        assert_eq!(local_prune.pruned_processes, 1);

        registry
            .complete_process(
                &hidden_process.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                    "done"
                ))),
                crate::ProcessCompletionAuthority::workflow_key(&hidden_process.id),
            )
            .await
            .expect("complete observed process");
        registry
            .add_observer(
                &SessionId::from("session"),
                &hidden_process.id,
                crate::ProcessObserverBy::host("observe-hidden-process"),
            )
            .await
            .expect("observe process");
        let retained = registry
            .get_process(&hidden_process.id)
            .await
            .expect("read observed process")
            .expect("observed process remains retained");
        let retained_bytes =
            serde_json::to_vec(&retained).expect("serialize retained terminal process");
        let terminal_signal = crate::signal_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("signal-terminal-process"),
            handle.clone(),
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        assert!(!terminal_signal.output.is_success());
        assert!(
            terminal_signal
                .output
                .value_for_projection()
                .to_string()
                .contains("already terminal"),
            "signaling a retained terminal process must return the typed terminal error"
        );
        let after_rejected_signal = registry
            .get_process(&hidden_process.id)
            .await
            .expect("read terminal process after rejected signal")
            .expect("terminal process remains retained");
        assert_eq!(
            serde_json::to_vec(&after_rejected_signal)
                .expect("serialize terminal process after rejected signal"),
            retained_bytes,
            "a rejected terminal signal must leave prune eligibility byte-stable"
        );

        let prune = registry
            .prune_terminal_processes(
                retained.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune observed process");
        assert_eq!(prune.pruned_processes, 1);
        assert!(matches!(
            registry.get_process(&hidden_process.id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
        let pruned_await = crate::await_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("await-pruned-process"),
            handle.clone(),
        )
        .await;
        assert!(
            pruned_await.output.is_success(),
            "a pruned await must be information in turn history: {:?}",
            pruned_await.output.value_for_projection()
        );
        let rendered = pruned_await.output.value_for_projection().to_string();
        assert!(
            rendered.contains("process_no_longer_retained"),
            "pruned await must render the typed information code: {rendered}"
        );
        let history_record = pruned_await
            .record
            .expect("record pruned await in turn history");
        assert!(
            history_record.output.is_success(),
            "turn history must retain pruned await as information, not tool failure"
        );

        let pruned_cancel = crate::cancel_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("cancel-pruned-process"),
            handle.clone(),
        )
        .await;
        assert!(!pruned_cancel.output.is_success());
        assert!(
            pruned_cancel
                .output
                .value_for_projection()
                .to_string()
                .contains("outcome is no longer retained")
        );
        let pruned_signal = crate::signal_process_handle(
            &context,
            lash_core_execution::ToolCallId::fixture("signal-pruned-process"),
            handle,
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        assert!(!pruned_signal.output.is_success());
        assert!(
            pruned_signal
                .output
                .value_for_projection()
                .to_string()
                .contains("outcome is no longer retained")
        );
    }
}
