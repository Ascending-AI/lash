use super::*;

const SEED: u64 = 0x5_2d2b;

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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        attachment_output(self.sources.clone()).into()
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

async fn durable_attachment_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    plugins: Arc<PluginSession>,
) -> (
    ToolDispatchContext<'h>,
    Arc<dyn crate::RuntimeStore>,
    Arc<dyn crate::AttachmentStore>,
) {
    let backend = crate::support::memory_store_backend().await;
    let factory = backend.session_store_factory();
    let request = crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("session"),
        relation: crate::SessionRelation::Root,
        config: crate::SessionPolicy::new(crate::TurnBudget::Unbounded).into(),
        head: crate::SessionCreationHead::CommittedByCreator,
    };
    crate::SessionCatalogStore::admit_session(factory.as_ref(), &request)
        .await
        .expect("create the manifest store");
    let persistence: Arc<dyn crate::RuntimeStore> = factory.clone();
    let backend: Arc<dyn crate::AttachmentStore> = backend.attachment_store();
    let attachment_store = Arc::new(crate::SessionAttachmentStore::new(
        Arc::clone(&backend),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &persistence,
        ))),
        request.session_id,
    ));
    let mut context = exact_dispatch_context_with_plugins(ports, plugins).await;
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
    persistence: &Arc<dyn crate::RuntimeStore>,
    backend: &Arc<dyn crate::AttachmentStore>,
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
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "authorization rejection must leave no write-ahead manifest intent"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "authorization rejection must leave no physical blob"
    );
}

#[tokio::test]
async fn denied_second_source_records_no_manifest_intent_for_the_first() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let definition = named_beta_tool("atomic_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: vec![
            inline_attachment(FIRST_BYTES),
            inline_attachment(DENIED_BYTES),
        ],
    });
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        test_plugins(provider),
    )
    .await;
    let authorized = Arc::new(std::sync::Mutex::new(Vec::new()));
    context.attachment_source_policy = Arc::new(DenySecondInlinePolicy {
        authorized: Arc::clone(&authorized),
    });
    assert!(
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty(),
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
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "authorization rejection must leave no write-ahead manifest intent"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "authorization rejection must leave no physical blob"
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn before_tool_attachment_replacement_is_normalized_before_leaf_recording() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let definition = named_beta_tool("before_hook_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: Vec::new(),
    });
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        "before_hook_attachment_probe",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_before_tool_call(before_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(backend.list().await.unwrap().is_empty());

    let outcome = dispatch_tool_call(
        &context,
        definition.name().to_string(),
        json!({ "value": "valid" }),
    )
    .await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn after_tool_attachment_replacement_is_normalized_before_leaf_recording() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let definition = named_beta_tool("after_hook_leaf_attachment_probe");
    let provider: Arc<dyn ToolProvider> = Arc::new(AttachmentProbeTools {
        definition: definition.clone(),
        sources: Vec::new(),
    });
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        "after_hook_leaf_attachment_probe",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_after_tool_call(after_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(backend.list().await.unwrap().is_empty());

    let outcome = dispatch_tool_call(
        &context,
        definition.name().to_string(),
        json!({ "value": "valid" }),
    )
    .await;

    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn deferred_completion_after_hook_attachment_is_normalized_before_recording() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        "deferred_completion_attachment_probe",
        crate::PluginSpec::new().with_after_tool_call(after_attachment_hook(DENIED_BYTES)),
    ))])
    .build_session("root")
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(
        persistence
            .list_uncommitted(u64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "precondition: the deferred completion manifest starts empty"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "precondition: the deferred completion blob store starts empty"
    );
    let attachment_store = Arc::clone(&context.attachment_store);
    let execution = crate::RuntimeExecutionContext::new(
        SessionId::from("session"),
        Arc::new(context),
        crate::support::memory_store_set().await.process_env_store(),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
    );

    let outcome = execution
        .pending_completion_dispatch_outcome(
            &crate::tool_dispatch::ToolCallIds {
                call_id: crate::ToolCallId::fixture("deferred-attachment-call"),
                provider_call_id: None,
            },
            "test:deferred-attachment-call",
            "deferred_attachment_probe".to_string(),
            json!({ "value": "valid" }),
            crate::Resolution::Ok(json!({ "completed": true })),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;

    assert_eq!(
        outcome.attempts.len(),
        1,
        "precondition: this is the deferred-completion attempt-recording exit"
    );
    assert_eq!(outcome.attempts[0].ordinal, 1);
    assert_eq!(
        outcome.record.call_id,
        crate::ToolCallId::fixture("deferred-attachment-call"),
        "the deferred completion is recorded under the parked call's id"
    );
    assert_policy_denial_left_no_attachment_state(&outcome, &persistence, &backend, &authorized)
        .await;
    drop(execution);
    handler.close().await.expect("close the dispatch handler");
}
