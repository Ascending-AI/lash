mod tests {
    use crate::ProcessId;
    use crate::RuntimeExecutionContext;
    use crate::SessionId;
    use crate::runtime::RuntimeEffectControllerHandle;
    use crate::session::ToolInvocationReply;
    use crate::support::prelude::*;
    use crate::tool_dispatch::ToolDispatchContext;
    use crate::{
        PreparedToolCall, ToolCall, ToolDefinition, ToolOutcome, ToolPrepareCall, ToolProvider,
    };
    use crate::{ProcessInput, ProcessRegistration};
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

    const SEED: u64 = 0x5_f720;

    async fn await_external_process_attachment(
        source: crate::AttachmentSource,
    ) -> (
        ToolInvocationReply,
        Arc<dyn crate::RuntimePersistence>,
        Arc<dyn crate::AttachmentStore>,
        Arc<DenyProcessAwaitAttachments>,
    ) {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session("root")
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::memory_store_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let process = registry
            .register_process(ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register external process");
        registry
            .add_observer(
                &SessionId::from("session"),
                &process.id,
                crate::ProcessObserverBy::host("process-await-attachment-test"),
            )
            .await
            .expect("observe external process");
        registry
            .complete_process(
                &process.id,
                crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                        source.clone(),
                    )),
                ),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete external process with attachment");

        let factory = backend.session_store_factory();
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("session"),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };
        let persistence = factory
            .create_store(&request)
            .await
            .expect("create real in-memory manifest store");
        let attachment_backend = backend.attachment_store();
        let attachment_store = Arc::new(crate::SessionAttachmentStore::new(
            Arc::clone(&attachment_backend),
            Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
                &persistence,
            ))),
            request.session_id.clone(),
        ));
        let policy = Arc::new(DenyProcessAwaitAttachments::default());
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host,
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: request.session_id.clone(),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&attachment_store),
            attachment_source_policy: Arc::clone(&policy) as Arc<dyn crate::AttachmentSourcePolicy>,
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            request.session_id,
            dispatch,
            backend.process_env_store(),
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );
        assert!(
            persistence
                .list_uncommitted(u64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(attachment_backend.list().await.unwrap().is_empty());
        let handle = RuntimeExecutionContext::process_handle_json(&process.id.clone());
        let reply =
            crate::await_process_handle(&context, "await-external-attachment".to_string(), handle)
                .await;
        (reply, persistence, attachment_backend, policy)
    }

    async fn assert_external_process_attachment_denied(source: crate::AttachmentSource) {
        let (reply, persistence, attachment_backend, policy) =
            await_external_process_attachment(source.clone()).await;
        let record = reply.record.expect("external process await is recorded");
        assert_eq!(record.call_id.as_deref(), Some("await-external-attachment"));
        assert_eq!(record.tool, "await_process");
        let crate::ToolCallOutcome::Failure(failure) = record.output.outcome else {
            panic!("denied external process attachment must replace the recorded result");
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
        assert!(
            persistence
                .list_uncommitted(u64::MAX)
                .await
                .unwrap()
                .is_empty()
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

    #[tokio::test]
    async fn external_process_referenced_attachment_is_denied_before_await_recording() {
        assert_external_process_attachment_denied(crate::AttachmentSource::external_url(
            crate::MediaType::parse("image/png").unwrap(),
            "https://example.invalid/process.png",
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
            Ok(PreparedToolCall::from_parts(
                call.pending.call_id,
                call.tool_id,
                call.pending.tool_name,
                call.pending.args,
                call.pending.replay,
                serde_json::json!({ "prepared": true }),
            ))
        }

        async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(serde_json::json!({
                "payload": call.context.prepared_payload().clone(),
            }))
            .into()
        }
    }

    #[tokio::test]
    async fn process_handle_start_registers_prepared_tool_call() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::clone(&prepares),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session("root")
            .expect("plugin session");
        let tools = Arc::clone(&provider);
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let env_store = backend.process_env_store();
        let parent = crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({"program": "parent"}),
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(
            "process-env:inherited",
        )));
        let context = RuntimeExecutionContext::new(
            SessionId::from("session"),
            dispatch,
            env_store,
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
        .with_execution_env_spec(crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::testing::standard_test_policy(),
        ))
        .with_process_execution(crate::ProcessId::fixture("parent"), &parent, None);

        let started = crate::start_tool_process(
            &context,
            "async-call-1".to_string(),
            "process_prepare".to_string(),
            serde_json::json!({ "input": "live" }),
        )
        .await;
        let handle_json = started.output.value_for_projection();
        let crate::ToolCallOutcome::Success(_handle) = started.output.outcome else {
            panic!("expected process handle output");
        };
        // The handle is the one record: an opaque id naming the process the
        // registrar minted, beside that id.
        let process_id = crate::ProcessId::parse(
            handle_json["process_id"]
                .as_str()
                .expect("the handle names its process"),
        )
        .expect("a minted process id");
        assert_eq!(
            handle_json,
            serde_json::json!({
                "__handle__": "lash",
                "id": lash_sansio::handle::HandleId::process(&process_id).as_str(),
                "process_id": process_id,
            })
        );
        assert_eq!(
            lash_sansio::handle::parse_handle_json(&handle_json)
                .as_ref()
                .and_then(lash_sansio::handle::HandleId::target),
            Some(lash_sansio::handle::HandleTarget::Process {
                process_id: process_id.clone(),
            })
        );
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        let record = registry
            .get_process(&process_id)
            .await
            .expect("read process")
            .expect("registered process");
        registry
            .remove_observer(
                &SessionId::from("session"),
                &process_id,
                crate::ProcessObserverBy::host("remove-test-observer"),
            )
            .await
            .expect("remove persisted observer");
        assert!(
            !registry
                .is_observer(&SessionId::from("session"), &process_id)
                .await
                .expect("check removed observer"),
            "tool-started process must rely on its persisted observer edge"
        );
        let ProcessInput::ToolCall { call } = record.input.as_ref() else {
            panic!("expected prepared tool call process input");
        };
        assert_eq!(call.tool_name, "process_prepare");
        assert_eq!(call.args, serde_json::json!({ "input": "live" }));
        assert_eq!(
            call.prepared_payload,
            serde_json::json!({ "prepared": true })
        );

        let awaited =
            crate::await_process_handle(&context, "await-async-call-1".to_string(), handle_json)
                .await;

        assert!(!awaited.output.is_success());
        let await_error = awaited.output.value_for_projection().to_string();
        assert!(
            await_error.contains(&format!(
                "process handle `{process_id}` is not live or visible in this session"
            )),
            "revoked tool-started handle must return the typed visibility miss: {await_error}"
        );
        let record = awaited.record.expect("await record");
        assert_eq!(record.call_id.as_deref(), Some("await-async-call-1"));
        assert_eq!(record.tool, "await_process");
    }

    #[tokio::test]
    async fn process_handle_signal_appends_event_from_foreground() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session("root")
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let target_process = registry
            .register_process(
                ProcessRegistration::new(
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                    crate::ProcessLifecyclePolicy::new(
                        crate::ParentScope::Host,
                        crate::OnParentEnd::Abandon,
                    ),
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register target process");
        registry
            .add_observer(
                &SessionId::from("session"),
                &target_process.id,
                crate::ProcessObserverBy::host("foreground-signal-test"),
            )
            .await
            .expect("observe target process");
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            SessionId::from("session"),
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );

        let handle = lash_sansio::handle::handle_record_json(
            &lash_sansio::handle::HandleId::process(&target_process.id),
        );
        let signalled = crate::signal_process_handle(
            &context,
            "signal-1".to_string(),
            handle,
            "ready".to_string(),
            json!({ "kind": "ping" }),
        )
        .await;

        assert!(
            signalled.output.is_success(),
            "{:?}",
            signalled.output.value_for_projection()
        );
        let record = signalled.record.expect("signal record");
        assert_eq!(record.call_id.as_deref(), Some("signal-1"));
        assert_eq!(record.tool, "signal_process");
        let events = registry
            .full_event_window(&target_process.id, 0)
            .await
            .expect("list events");
        assert!(
            events.iter().any(|event| event.event_type == "signal.ready"
                && event.payload.get("kind") == Some(&json!("ping"))),
            "expected appended signal.ready event, got {events:?}"
        );
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
            .build_session("root")
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            SessionId::from("session"),
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
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
            let awaited =
                crate::await_process_handle(&context, format!("await-{shape}"), handle.clone())
                    .await;
            let signalled = crate::signal_process_handle(
                &context,
                format!("signal-{shape}"),
                handle.clone(),
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
            let cancelled =
                crate::cancel_process_handle(&context, format!("cancel-{shape}"), handle.clone())
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
                    record.call_id.as_deref(),
                    Some(format!("{operation}-{shape}").as_str()),
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
            .build_session("root")
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let backend = crate::support::memory_store_set().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let hidden_process = registry
            .register_process(
                ProcessRegistration::new(
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                    crate::ProcessLifecyclePolicy::new(
                        crate::ParentScope::Host,
                        crate::OnParentEnd::Abandon,
                    ),
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register hidden process");
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            SessionId::from("session"),
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );
        let handle = lash_sansio::handle::handle_record_json(
            &lash_sansio::handle::HandleId::process(&hidden_process.id),
        );

        let awaited = crate::await_process_handle(
            &context,
            "await-hidden-process".to_string(),
            handle.clone(),
        )
        .await;
        let signalled = crate::signal_process_handle(
            &context,
            "signal-hidden-process".to_string(),
            handle.clone(),
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        let cancelled = crate::cancel_process_handle(
            &context,
            "cancel-hidden-process".to_string(),
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
            awaited
                .record
                .as_ref()
                .and_then(|record| record.call_id.as_deref()),
            Some("await-hidden-process")
        );
        assert_eq!(
            cancelled
                .record
                .as_ref()
                .and_then(|record| record.call_id.as_deref()),
            Some("cancel-hidden-process")
        );

        let mut local_ids = BTreeMap::new();
        for label in ["local-signal", "local-cancel", "local-await"] {
            let mut registration = ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            );
            if label == "local-signal" {
                registration = registration.with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
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
                crate::ProcessCompletionAuthority::external_owner(),
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
            "signal-local".to_string(),
            local_handle(&local_ids["local-signal"]),
            "ready".to_string(),
            serde_json::Value::Null,
        )
        .await;
        let local_cancel = crate::cancel_process_handle(
            &context,
            "cancel-local".to_string(),
            local_handle(&local_ids["local-cancel"]),
        )
        .await;
        let local_await = crate::await_process_handle(
            &context,
            "await-local".to_string(),
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
                crate::ProcessCompletionAuthority::external_owner(),
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
            "signal-terminal-process".to_string(),
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
            "await-pruned-process".to_string(),
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
            "cancel-pruned-process".to_string(),
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
            "signal-pruned-process".to_string(),
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

    /// FIG-3117: a start realized from a recorded tool intent must grant the
    /// run possession of the child it started.
    ///
    /// The child a `processes.start` declaration starts carries no observer
    /// edge, so possession is the only authority that can reach it — exactly
    /// the authority the in-session start path takes when the registry row
    /// lands. Realization happens in tool dispatch, which holds no runtime
    /// execution context, so the realized handle has to grant possession where
    /// it arrives back: the settled attempt. The precondition is asserted
    /// first, so a fixture that happened to observe the child could not pass
    /// this test by accident.
    ///
    /// `started_process_ids()` is the possession set a scripted-program segment
    /// handover carries and `restore_started_process_ids` reinstalls, so the
    /// same grant is what survives a segment boundary.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_realized_start_intent_grants_the_run_possession_of_its_child() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session("root")
            .expect("plugin session");
        let tool_catalog = Arc::new(catalog_for(&provider));
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default()
                .with_process_registry(Arc::clone(&registry)),
        );
        let started = registry
            .register_process(ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register the child a start declaration realizes");
        let child = started.id.clone();
        registry
            .complete_process(
                &child,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                    "child done"
                ))),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the started child");
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation(
                "test-runtime-effect-controller",
            ))
            .await
            .expect("open the presentation handler");
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            // The completion presents through the journaled boundary, so the
            // context runs on the open handler's lent controller.
            effect_controller: RuntimeEffectControllerHandle::borrowed(handler.scoped()),
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
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            SessionId::from("session"),
            dispatch,
            backend.process_env_store(),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );
        let realized_handle = RuntimeExecutionContext::process_handle_json(&started.id.clone());

        assert!(
            !context.started_process_ids().contains(&child),
            "the run must not possess the child before the start is realized"
        );
        let before = crate::await_process_handle(
            &context,
            "await-before-realization".to_string(),
            realized_handle.clone(),
        )
        .await;
        assert!(
            !before.output.is_success()
                && before
                    .output
                    .value_for_projection()
                    .to_string()
                    .contains("is not live or visible in this session"),
            "precondition: an unpossessed, unobserved child is refused, got {:?}",
            before.output.value_for_projection()
        );

        let identity = crate::ToolIntentIdentity {
            session_id: SessionId::from("session"),
            execution_scope_id: "session".to_string(),
            tool_call_id: "start-child".to_string(),
            intent_index: 0,
            replay_key: child.to_string(),
            minting_emission_replay_key: None,
        };
        context
            .complete_tool_call(
                "start-child".to_string(),
                None,
                crate::tool_dispatch::ToolDispatchOutcome {
                    record: crate::ToolCallRecord {
                        call_id: Some("start-child".to_string()),
                        tool: "start_process".to_string(),
                        args: json!({}),
                        output: crate::ToolCallOutput::success(realized_handle.clone()),
                    },
                    attempts: Vec::new(),
                    intents: crate::ToolIntents::default(),
                    intent_outcomes: vec![crate::ToolIntentExecutionOutcome::Executed {
                        identity,
                        kind: crate::ToolIntentKind::StartProcess,
                        result: realized_handle.clone(),
                    }],
                    captures: Vec::new(),
                    triggers: Vec::new(),
                },
                "test:start-child",
                3,
            )
            .await
            .expect("the start call presents");

        assert!(
            context.started_process_ids().contains(&child),
            "a realized start intent grants the run possession the segment handover carries"
        );
        let awaited = crate::await_process_handle(
            &context,
            "await-after-realization".to_string(),
            realized_handle,
        )
        .await;
        assert!(
            awaited.output.is_success(),
            "the run must resolve the handle its own realized start answered: {:?}",
            awaited.output.value_for_projection()
        );
        drop(context);
        handler
            .close()
            .await
            .expect("close the presentation handler");
    }
}
