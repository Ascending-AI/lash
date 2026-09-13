use super::tests::{CountingEffectController, runtime_test_tool};
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug)]
struct ProviderPartPersistenceProvider {
    tool_calling: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::Provider for ProviderPartPersistenceProvider {
    fn kind(&self) -> &'static str {
        "stub"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> lash_core::facade_support::ProviderOptions {
        lash_core::facade_support::ProviderOptions::default()
    }

    fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        _request: lash_core::LlmRequest,
    ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.tool_calling && call > 0 {
            return Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            });
        }

        let mut parts = vec![
            LlmOutputPart::Reasoning {
                text: "reasoning summary".to_string(),
                replay: Some(ProviderReasoningReplay {
                    item_id: Some("reasoning-1".to_string()),
                    encrypted_content: Some("opaque-reasoning".to_string()),
                    ..ProviderReasoningReplay::default()
                }),
            },
            LlmOutputPart::Text {
                text: "answer".to_string(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("message-1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    provider_payload: Some("opaque-text".to_string()),
                    ..ResponseTextMeta::default()
                }),
            },
        ];
        if self.tool_calling {
            parts.push(LlmOutputPart::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            });
        }
        Ok(LlmResponse {
            parts,
            ..LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
        Box::new(self.clone())
    }
}

struct LookupRuntimeTool;

#[async_trait::async_trait]
impl ToolProvider for LookupRuntimeTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![runtime_test_tool("lookup").manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "lookup").then(|| Arc::new(runtime_test_tool(name).contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(serde_json::json!("found"))
    }
}

async fn persisted_provider_parts(tool_calling: bool, session_id: &str) -> Vec<Part> {
    let provider = ProviderPartPersistenceProvider {
        tool_calling,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(provider)),
    );
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    let policy = lash_core::SessionPolicy {
        provider_id: "stub".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let scoped_controller = lash_core::ScopedEffectController::shared(
        Arc::new(CountingEffectController::default()),
        lash_core::ExecutionScope::turn(session_id, "turn-1"),
    )
    .expect("scoped controller");
    let factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
        Arc::new(StandardProtocolPluginFactory::new()),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "provider-part-persistence-tools",
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::new(LookupRuntimeTool)),
        )),
    ];
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id(session_id)
        .with_policy(policy)
        .with_runtime_host(host)
        .with_plugin_factories(factories)
        .build(),
    )
    .await
    .expect("runtime");
    let turn = runtime
        .stream_turn(
            lash_core::TurnInput::text("respond"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller,
            ),
        )
        .await
        .expect("turn");
    let read_view = turn.state.read_view().expect("turn read view");
    read_view
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .find(|message| {
            message
                .parts
                .iter()
                .any(|part| part.response_meta.is_some())
        })
        .expect("provider-bearing assistant message persisted")
        .parts
        .iter()
        .filter(|part| matches!(part.kind, PartKind::Reasoning | PartKind::Prose))
        .cloned()
        .collect()
}

#[tokio::test]
async fn final_and_tool_calling_responses_persist_identical_typed_provider_parts() {
    let final_parts = persisted_provider_parts(false, "final-provider-parts").await;
    let tool_calling_parts = persisted_provider_parts(true, "tool-calling-provider-parts").await;

    assert_eq!(
        final_parts.iter().map(|part| part.kind).collect::<Vec<_>>(),
        [PartKind::Reasoning, PartKind::Prose]
    );
    assert_eq!(
        tool_calling_parts
            .iter()
            .map(|part| part.kind)
            .collect::<Vec<_>>(),
        [PartKind::Reasoning, PartKind::Prose]
    );
    assert_eq!(
        final_parts[0].reasoning_meta,
        tool_calling_parts[0].reasoning_meta
    );
    assert_eq!(
        final_parts[1].response_meta,
        tool_calling_parts[1].response_meta
    );
}
