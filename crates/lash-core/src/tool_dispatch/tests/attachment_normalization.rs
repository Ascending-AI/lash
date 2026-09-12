use super::*;
use crate::AttachmentStore as _;
use crate::SessionStoreFactory as _;

const FIRST_BYTES: &[u8] = b"authorized attachment";
const DENIED_BYTES: &[u8] = b"denied attachment";

fn inline_attachment(bytes: &[u8]) -> crate::AttachmentSource {
    crate::AttachmentSource::inline(
        crate::MediaType::parse("text/plain").expect("literal media type"),
        bytes.to_vec(),
    )
}

fn attachment_output(sources: impl IntoIterator<Item = crate::AttachmentSource>) -> ToolOutcome {
    ToolOutcome::from_output(crate::ToolCallOutput::success_tool_value(
        crate::ToolValue::Array(
            sources
                .into_iter()
                .map(crate::ToolValue::Attachment)
                .collect(),
        ),
    ))
}

fn attachment_call_output(
    sources: impl IntoIterator<Item = crate::AttachmentSource>,
) -> crate::ToolCallOutput {
    attachment_output(sources)
        .into_done_output()
        .expect("attachment probe returns a completed output")
}

fn before_attachment_hook(bytes: &'static [u8]) -> crate::plugin::BeforeToolCallHook {
    Arc::new(move |_context| {
        Box::pin(async move {
            Ok(vec![crate::BeforeToolCallPluginDirective::from(
                crate::ShortCircuitToolDirective {
                    output: attachment_call_output([inline_attachment(bytes)]),
                },
            )])
        })
    })
}

fn after_attachment_hook(bytes: &'static [u8]) -> crate::plugin::AfterToolCallHook {
    Arc::new(move |_context| {
        Box::pin(async move {
            Ok(vec![crate::AfterToolCallPluginDirective::from(
                crate::ShortCircuitToolDirective {
                    output: attachment_call_output([inline_attachment(bytes)]),
                },
            )])
        })
    })
}

#[derive(Clone)]
struct AttachmentProbeTools {
    definition: crate::ToolDefinition,
    sources: Vec<crate::AttachmentSource>,
}

#[async_trait::async_trait]
impl ToolProvider for AttachmentProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        attachment_output(self.sources.clone())
    }

    async fn execute_internal(&self, _call: crate::InternalProcessToolCall<'_>) -> ToolOutcome {
        attachment_output(self.sources.clone())
    }
}

struct AttachmentProbeOrchestrator {
    definition: crate::ToolDefinition,
    sources: Vec<crate::AttachmentSource>,
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for AttachmentProbeOrchestrator {
    fn manifest(&self) -> crate::ToolManifest {
        self.definition.manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(self.definition.contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        _context: &crate::facade_support::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        attachment_output(self.sources.clone())
    }
}

struct DenySecondInlinePolicy {
    authorized: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl crate::AttachmentSourcePolicy for DenySecondInlinePolicy {
    fn authorize(
        &self,
        producer: &crate::AttachmentProducer,
        source: &crate::AttachmentSource,
    ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
        let crate::AttachmentSource::Inline { bytes, .. } = source else {
            return Ok(());
        };
        self.authorized.lock_recover().push(bytes.clone());
        if bytes == DENIED_BYTES {
            return Err(crate::test_support::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "literal second source is denied".to_string(),
            });
        }
        Ok(())
    }
}

async fn durable_attachment_context(
    plugins: Arc<PluginSession>,
) -> (
    ToolDispatchContext<'static>,
    Arc<dyn crate::RuntimePersistence>,
    Arc<crate::InMemoryAttachmentStore>,
) {
    let factory = crate::InMemorySessionStoreFactory::new();
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
    let backend = Arc::new(crate::InMemoryAttachmentStore::new());
    let attachment_store = Arc::new(crate::SessionAttachmentStore::new(
        Arc::clone(&backend) as Arc<dyn crate::AttachmentStore>,
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &persistence,
        ))),
        request.session_id,
    ));
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.attachment_store = attachment_store;
    (context, persistence, backend)
}

fn deny_probe_attachment(
    context: &mut ToolDispatchContext<'_>,
) -> Arc<std::sync::Mutex<Vec<Vec<u8>>>> {
    let authorized = Arc::new(std::sync::Mutex::new(Vec::new()));
    context.attachment_source_policy = Arc::new(DenySecondInlinePolicy {
        authorized: Arc::clone(&authorized),
    });
    authorized
}

async fn assert_policy_denial_left_no_attachment_state(
    outcome: &ToolDispatchOutcome,
    persistence: &Arc<dyn crate::RuntimePersistence>,
    backend: &Arc<crate::InMemoryAttachmentStore>,
    authorized: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) {
    let crate::ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
        panic!("the denied hook attachment must replace the result with failure");
    };
    assert_eq!(failure.code, "attachment_source_policy_denied");
    assert_eq!(
        *authorized.lock_recover(),
        vec![DENIED_BYTES.to_vec()],
        "the final hook output must pass through attachment policy"
    );
    assert!(
        persistence.list_uncommitted(u64::MAX).unwrap().is_empty(),
        "authorization rejection must leave no write-ahead manifest intent"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "authorization rejection must leave no physical blob"
    );
}

fn prepared(definition: &crate::ToolDefinition, call_id: &str) -> crate::PreparedToolCall {
    crate::PreparedToolCall::from_parts(
        call_id,
        definition.id().clone(),
        definition.name(),
        json!({}),
        None,
        serde_json::Value::Null,
    )
}

fn assert_single_stored_attachment(output: &crate::ToolCallOutput) {
    let attachments = output.attachments();
    assert_eq!(attachments.len(), 1, "probe returns exactly one attachment");
    assert!(
        matches!(
            attachments.first(),
            Some(crate::AttachmentSource::Stored { .. })
        ),
        "the durable record must contain a stored reference, not inline bytes: {attachments:?}"
    );
}

#[tokio::test]
async fn denied_second_source_records_no_manifest_intent_for_the_first() {
    let definition = named_beta_tool("atomic_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: vec![
            inline_attachment(FIRST_BYTES),
            inline_attachment(DENIED_BYTES),
        ],
    });
    let (mut context, persistence, backend) =
        durable_attachment_context(test_plugins(provider)).await;
    let authorized = Arc::new(std::sync::Mutex::new(Vec::new()));
    context.attachment_source_policy = Arc::new(DenySecondInlinePolicy {
        authorized: Arc::clone(&authorized),
    });
    assert!(
        persistence.list_uncommitted(u64::MAX).unwrap().is_empty(),
        "precondition: the manifest starts empty"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "precondition: the blob store starts empty"
    );
    let outcome = dispatch_tool_call(
        &context,
        definition.name().to_string(),
        json!({ "value": "valid" }),
    )
    .await;

    let crate::ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("the denied second attachment must fail the recorded call");
    };
    assert_eq!(failure.code, "attachment_source_policy_denied");
    assert_eq!(
        *authorized.lock_recover(),
        vec![FIRST_BYTES.to_vec(), DENIED_BYTES.to_vec()],
        "precondition: policy reaches and denies the second source"
    );
    assert!(
        persistence.list_uncommitted(u64::MAX).unwrap().is_empty(),
        "authorization rejection must leave no write-ahead manifest intent"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "authorization rejection must leave no physical blob"
    );
}

#[tokio::test]
async fn before_tool_attachment_replacement_is_normalized_before_leaf_recording() {
    let definition = named_beta_tool("before_hook_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: Vec::new(),
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "before_hook_attachment_probe",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_before_tool_call(before_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(plugins).await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(persistence.list_uncommitted(u64::MAX).unwrap().is_empty());
    assert!(backend.list().await.unwrap().is_empty());

    let outcome = dispatch_tool_call(
        &context,
        definition.name().to_string(),
        json!({ "value": "valid" }),
    )
    .await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
}

#[tokio::test]
async fn after_tool_attachment_replacement_is_normalized_before_leaf_recording() {
    let definition = named_beta_tool("after_hook_leaf_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: Vec::new(),
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "after_hook_leaf_attachment_probe",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_after_tool_call(after_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(plugins).await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(persistence.list_uncommitted(u64::MAX).unwrap().is_empty());
    assert!(backend.list().await.unwrap().is_empty());

    let outcome = dispatch_tool_call(
        &context,
        definition.name().to_string(),
        json!({ "value": "valid" }),
    )
    .await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
}

#[tokio::test]
async fn after_tool_attachment_replacement_is_normalized_before_orchestrating_recording() {
    let definition = named_beta_tool("after_hook_orchestrating_attachment_probe");
    let orchestrator =
        crate::facade_support::OrchestratingToolDef::new(Arc::new(AttachmentProbeOrchestrator {
            definition: definition.clone(),
            sources: Vec::new(),
        }));
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "after_hook_orchestrating_attachment_probe",
        crate::PluginSpec::new()
            .with_orchestrating_tool(orchestrator)
            .with_after_tool_call(after_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(plugins).await;
    context.tool_registry = Some(context.plugins.tool_registry());
    let authorized = deny_probe_attachment(&mut context);
    assert!(persistence.list_uncommitted(u64::MAX).unwrap().is_empty());
    assert!(backend.list().await.unwrap().is_empty());
    let call = prepared(&definition, "after-hook-orchestrating-call");
    let tool_context = tool_context_for_prepared(&context, &call);

    let outcome = execute_orchestrating_tool(&context, call, tool_context).await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
}

#[tokio::test]
async fn after_tool_attachment_replacement_is_normalized_before_internal_recording() {
    let definition = named_beta_tool("after_hook_internal_attachment_probe")
        .with_activation(crate::ToolActivation::Internal);
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: Vec::new(),
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "after_hook_internal_attachment_probe",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_after_tool_call(after_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(plugins).await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(persistence.list_uncommitted(u64::MAX).unwrap().is_empty());
    assert!(backend.list().await.unwrap().is_empty());
    let call = prepared(&definition, "after-hook-internal-call");
    let tool_context = tool_context_for_prepared(&context, &call);

    let outcome = execute_internal_process_tool(&context, call, tool_context).await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
}

#[tokio::test]
async fn orchestrating_tool_output_is_normalized_under_process_ownership() {
    let definition = named_beta_tool("orchestrating_attachment_probe");
    let orchestrator =
        crate::facade_support::OrchestratingToolDef::new(Arc::new(AttachmentProbeOrchestrator {
            definition: definition.clone(),
            sources: vec![inline_attachment(FIRST_BYTES)],
        }));
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "orchestrating_attachment_probe",
        crate::PluginSpec::new().with_orchestrating_tool(orchestrator),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, _) = durable_attachment_context(plugins).await;
    context.tool_registry = Some(context.plugins.tool_registry());
    let _owner = context
        .attachment_store
        .bind_process_scoped("orchestrating-process");
    assert!(
        persistence.list_uncommitted(u64::MAX).unwrap().is_empty(),
        "precondition: the process manifest starts empty"
    );
    let call = prepared(&definition, "orchestrating-attachment-call");
    let tool_context = tool_context_for_prepared(&context, &call);

    let outcome = execute_orchestrating_tool(&context, call, tool_context).await;

    assert_single_stored_attachment(&outcome.record.output);
    let entries = persistence.list_uncommitted(u64::MAX).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "normalization records one manifest intent"
    );
    assert_eq!(
        entries[0].owner_kind,
        Some(crate::AttachmentOwnerKind::Process)
    );
    assert_eq!(
        entries[0].owner_id.as_deref(),
        Some("orchestrating-process")
    );
}

#[tokio::test]
async fn internal_process_tool_output_is_normalized_under_process_ownership() {
    let definition = named_beta_tool("internal_attachment_probe")
        .with_activation(crate::ToolActivation::Internal);
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: vec![inline_attachment(FIRST_BYTES)],
    });
    let (context, persistence, _) = durable_attachment_context(test_plugins(provider)).await;
    let _owner = context
        .attachment_store
        .bind_process_scoped("internal-process");
    assert!(
        persistence.list_uncommitted(u64::MAX).unwrap().is_empty(),
        "precondition: the process manifest starts empty"
    );
    let call = prepared(&definition, "internal-attachment-call");
    let tool_context = tool_context_for_prepared(&context, &call);

    let outcome = execute_internal_process_tool(&context, call, tool_context).await;

    assert_single_stored_attachment(&outcome.record.output);
    let entries = persistence.list_uncommitted(u64::MAX).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "normalization records one manifest intent"
    );
    assert_eq!(
        entries[0].owner_kind,
        Some(crate::AttachmentOwnerKind::Process)
    );
    assert_eq!(entries[0].owner_id.as_deref(), Some("internal-process"));
}
