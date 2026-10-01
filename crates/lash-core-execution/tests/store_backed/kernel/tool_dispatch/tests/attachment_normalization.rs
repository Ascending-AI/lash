use super::*;
use crate::plugin::PluginSessionRequest;

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
    let backend = crate::support::sqlite_memory_store_backend().await;
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
        .expect("create the manifest store");
    let persistence: Arc<dyn crate::RuntimeStore> = factory.clone();
    let backend: Arc<dyn crate::AttachmentStore> = backend.attachment_store();
    let attachment_store = Arc::new(crate::RuntimeAttachmentStore::new(
        Arc::clone(&backend),
        Arc::new(crate::attachments::PersistenceReferrersAdapter(Arc::clone(
            &persistence,
        ))),
        crate::RuntimeOwner::Session(request.session_id),
    ));
    let mut context = exact_dispatch_context_with_plugins(ports, plugins).await;
    context.attachment_store = attachment_store;
    (context, persistence, backend)
}

/// Whether neither probe digest has a referrer: no pending write and no
/// edge was recorded for either.
async fn no_attachment_referrers(persistence: &dyn crate::RuntimeStore) -> bool {
    for bytes in [FIRST_BYTES, DENIED_BYTES] {
        let referrers = persistence
            .attachment_referrers(&crate::attachments::content_id(bytes))
            .await
            .unwrap();
        if !referrers.is_empty() {
            return false;
        }
    }
    true
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
        no_attachment_referrers(persistence.as_ref()).await,
        "authorization rejection must leave no pending write"
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
        no_attachment_referrers(persistence.as_ref()).await,
        "precondition: no digest has a referrer"
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
        no_attachment_referrers(persistence.as_ref()).await,
        "authorization rejection must leave no pending write"
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
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(no_attachment_referrers(persistence.as_ref()).await);
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
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(no_attachment_referrers(persistence.as_ref()).await);
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
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
    let (mut context, persistence, backend) = durable_attachment_context(
        crate::support::double_dispatch_ports(&double, &handler),
        plugins,
    )
    .await;
    let authorized = deny_probe_attachment(&mut context);
    assert!(
        no_attachment_referrers(persistence.as_ref()).await,
        "precondition: no deferred completion digest has a referrer"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "precondition: the deferred completion blob store starts empty"
    );
    let attachment_store = Arc::clone(&context.attachment_store);
    let execution = crate::RuntimeExecutionContext::new(
        Arc::new(context),
        crate::support::sqlite_memory_store_set()
            .await
            .process_env_store(),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
        crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
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

struct CountedAttachmentTools {
    inner: AttachmentProbeTools,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for CountedAttachmentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.inner.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(call).await
    }
}

struct TransientPublicationManifest {
    fail_completion: bool,
    failures: AtomicUsize,
}

impl TransientPublicationManifest {
    fn failure(&self) -> crate::StoreError {
        self.failures.fetch_add(1, Ordering::SeqCst);
        crate::StoreError::StorageFailure {
            backend: "publication-probe",
            message: "manifest unavailable".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl crate::AttachmentReferrers for TransientPublicationManifest {
    async fn begin_attachment_write(
        &self,
        write: &crate::AttachmentWrite,
    ) -> Result<crate::AttachmentWriteFence, crate::StoreError> {
        if !self.fail_completion {
            return Err(self.failure());
        }
        crate::AttachmentReferrers::begin_attachment_write(
            &crate::attachments::NoopAttachmentReferrers,
            write,
        )
        .await
    }

    async fn complete_attachment_write(
        &self,
        _write: &crate::AttachmentWrite,
        _permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        Err(self.failure())
    }

    async fn abort_attachment_write(
        &self,
        _write: &crate::AttachmentWrite,
        _permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::StoreError> {
        panic!("publication failure must not start a rollback retry")
    }

    async fn acquire_attachment_refs(
        &self,
        _claim: &crate::ReferrerClaim,
        _ids: &[crate::AttachmentId],
    ) -> Result<(), crate::StoreError> {
        panic!("unexpected acquire_attachment_refs")
    }

    async fn forget_attachment_ref(
        &self,
        _referrer: &crate::ArtifactReferrer,
        _id: &crate::AttachmentId,
    ) -> Result<(), crate::StoreError> {
        panic!("unexpected forget_attachment_ref")
    }

    async fn end_attachment_referrer(
        &self,
        _referrer: &crate::ArtifactReferrer,
    ) -> Result<(), crate::StoreError> {
        panic!("unexpected end_attachment_referrer")
    }

    async fn session_referrer_state(
        &self,
        _session: &SessionId,
    ) -> Result<crate::SessionReferrerState, crate::StoreError> {
        panic!("unexpected session_referrer_state")
    }

    async fn attachment_referrers(
        &self,
        _id: &crate::AttachmentId,
    ) -> Result<Vec<crate::ArtifactReferrer>, crate::StoreError> {
        panic!("unexpected attachment_referrers")
    }
}

#[tokio::test]
async fn transient_manifest_publication_failure_never_reexecutes_the_tool() {
    for fail_completion in [false, true] {
        let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let definition = named_beta_tool("publication_failure_probe")
            .with_retry_policy(ToolRetryPolicy::safe(3, 0, 0));
        let provider: Arc<dyn ToolProvider> = Arc::new(CountedAttachmentTools {
            inner: AttachmentProbeTools {
                definition: definition.clone(),
                sources: vec![inline_attachment(FIRST_BYTES)],
            },
            calls: Arc::clone(&calls),
        });
        let mut context = exact_dispatch_context_with_plugins(
            crate::support::double_dispatch_ports(&double, &handler),
            test_plugins(provider),
        )
        .await;
        let backend = crate::support::sqlite_memory_store_backend().await;
        let manifest = Arc::new(TransientPublicationManifest {
            fail_completion,
            failures: AtomicUsize::new(0),
        });
        context.attachment_store = Arc::new(crate::RuntimeAttachmentStore::new(
            backend.attachment_store(),
            manifest.clone(),
            crate::RuntimeOwner::Session(SessionId::from("session")),
        ));
        let outcome = dispatch_tool_call(
            &context,
            definition.name().to_string(),
            json!({ "value": "valid" }),
        )
        .await;
        let crate::ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
            panic!("manifest failure must fail the completed tool result");
        };
        assert_eq!(failure.code, "attachment_store_failed");
        assert_eq!(failure.retry, ToolRetryStatus::Never);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an executed tool must not be replayed"
        );
        assert_eq!(
            manifest.failures.load(Ordering::SeqCst),
            1,
            "no publication retry loop"
        );
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(
            backend.attachment_store().list().await.unwrap().len(),
            usize::from(fail_completion)
        );
        drop(context);
        handler.close().await.expect("close the dispatch handler");
    }
}
