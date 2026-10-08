//! REF-ONLY-BOUNDARY and the identity half of ATTACHMENT-IDENTITY.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::{
    AttachmentCreateMeta, AttachmentRef, AttachmentStore, AttachmentStoreError,
    ToolDefinitionBindingExt as _, ToolProvider,
};
use lash_core_execution::StoreSet;
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryForms, DeliveryLimits, DeliverySecret, ProviderAccepts,
};
use lash_sansio::llm::capability::{
    AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
};
use lash_sansio::sync::MutexExt as _;

const MODEL: &str = "attachment-ref-model";
const BLOB: &[u8] = b"immutable attachment content";
/// The tool's own content: one delivery serves every slot of one content
/// id, so the tool result is a second delivery only when its bytes differ.
const TOOL_BLOB: &[u8] = b"immutable tool attachment content";

fn meta(label: &str) -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        "image/png".parse().unwrap(),
        Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some(label.to_owned()),
    )
}

fn catalogue() -> AttachmentCapabilitySnapshot {
    AttachmentCapabilitySnapshot {
        revision: "ref-law".into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "test".into(),
            rules: vec![AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
                media_types: vec!["image/png".into()],
                media_families: vec![],
                forms: DeliveryForms {
                    bytes: true,
                    url: true,
                    provider_file: false,
                },
            }],
        }],
    }
}

/// An immutable origin double: the URL's path names the very blob get reads.
struct DeliveringStore {
    inner: Arc<dyn AttachmentStore>,
    url: bool,
    deliveries: AtomicUsize,
}
#[async_trait::async_trait]
impl AttachmentStore for DeliveringStore {
    fn persistence(&self) -> lash_core::AttachmentStorePersistence {
        self.inner.persistence()
    }
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }
    async fn get(
        &self,
        id: &lash_core::AttachmentId,
        max_bytes: u64,
    ) -> Result<lash_core::StoredAttachment, AttachmentStoreError> {
        self.inner.get(id, max_bytes).await
    }
    async fn deliver(
        &self,
        reference: &AttachmentRef,
        accepts: &ProviderAccepts,
        limits: &DeliveryLimits,
    ) -> Result<Delivery, AttachmentStoreError> {
        let stored = self.get(&reference.id, limits.max_bytes).await?;
        assert_eq!(stored.bytes.len() as u64, reference.byte_len);
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        if self.url {
            assert!(accepts.url);
            Ok(Delivery::Url {
                url: DeliverySecret::new(format!("https://immutable.test/{}", reference.id)),
                valid_until_ms: None,
            })
        } else {
            assert!(accepts.bytes);
            Ok(Delivery::Bytes(stored.bytes))
        }
    }
    async fn delete(&self, id: &lash_core::AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }
    async fn list(&self) -> Result<Vec<lash_core::StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
    async fn head(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<Option<lash_core::StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

struct LawStores {
    inner: Arc<dyn StoreSet>,
    attachments: Arc<DeliveringStore>,
}
impl StoreSet for LawStores {
    fn durable_store(&self) -> Arc<dyn lash_durable::DurableStore> {
        self.inner.durable_store()
    }
    fn node_wakes(&self) -> Option<Arc<dyn lash_durable::NodeWakes>> {
        self.inner.node_wakes()
    }
    fn binding_identity(&self) -> &lash_core_execution::StoreBindingId {
        self.inner.binding_identity()
    }
    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }
    fn session_store_factory(&self) -> Arc<dyn lash_core::DeploymentStore> {
        self.inner.session_store_factory()
    }
    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::store::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }
    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.inner.process_registry()
    }
    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }
    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }
    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }
    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        self.inner.definition_store()
    }
    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.attachments.clone()
    }
    fn module_artifacts(&self) -> Arc<dyn lash_core::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }
    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }
    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }
    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}

struct NestedMedia;
fn definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:nested_media",
        "nested_media",
        "Return retained media in an MCP content block",
        serde_json::json!({"type":"object"}),
        serde_json::json!({}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], "nested_media"))
}
#[async_trait::async_trait]
impl ToolProvider for NestedMedia {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "nested_media").then(|| Arc::new(definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        assert!(
            call.context
                .attachments()
                .put(vec![0; 1025], meta("refused.png"))
                .await
                .is_err()
        );
        let reference = match call
            .context
            .attachments()
            .put(TOOL_BLOB.to_vec(), meta("tool.png"))
            .await
        {
            Ok(reference) => reference,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        assert_eq!(
            call.context.attachments().read(&reference).await.unwrap(),
            TOOL_BLOB
        );
        let value = lash_core::ToolValue::Object(std::collections::BTreeMap::from([(
            "content".into(),
            lash_core::ToolValue::Array(vec![lash_core::ToolValue::Object(
                std::collections::BTreeMap::from([
                    ("type".into(), "image".into()),
                    (
                        "attachment".into(),
                        lash_core::ToolValue::Attachment(reference),
                    ),
                ]),
            )]),
        )]));
        lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(value))
            .into()
    }
}

struct Sent {
    history: Vec<AttachmentRef>,
    requests: Vec<Vec<AttachmentRef>>,
    deliveries: usize,
}
async fn send(url: bool, tools: bool) -> Sent {
    let sqlite: Arc<dyn StoreSet> =
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let attachments = Arc::new(DeliveringStore {
        inner: sqlite.attachment_store(),
        url,
        deliveries: AtomicUsize::new(0),
    });
    let stores: Arc<dyn StoreSet> = Arc::new(LawStores {
        inner: sqlite,
        attachments: attachments.clone(),
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .complete({
            let requests = requests.clone();
            let calls = calls.clone();
            move |request| {
                requests
                    .lock_recover()
                    .push(request.attachments().cloned().collect());
                let invoke = tools && calls.fetch_add(1, Ordering::SeqCst) == 0;
                async move {
                    Ok(lash_core::llm::types::LlmResponse {
                        parts: vec![if invoke {
                            lash_core::LlmOutputPart::ToolCall {
                                call_id: "call-media".into(),
                                tool_name: "nested_media".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }
                        } else {
                            lash_core::LlmOutputPart::Text {
                                text: "noted".into(),
                                response_meta: None,
                            }
                        }],
                        ..Default::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .unwrap();
    let core = lash::LashCore::standard_builder(backend)
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder(MODEL)
                .context_window_tokens(100_000)
                .build()
                .unwrap(),
        )
        .tools(Arc::new(NestedMedia))
        .max_attachment_bytes(Some(1024))
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "ref-law", "node",
        ))
        .unwrap();
    let id = lash::SessionId::from("ref-law");
    let spec = lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(4),
    )
    .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
    .attachment_acceptance(Arc::new(catalogue()));
    core.session(id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec,
        ))
        .await
        .unwrap();
    let session = core.session(id).open().await.unwrap();
    let reference = session
        .put_attachment(BLOB.to_vec(), meta("input.png"))
        .await
        .unwrap();
    let failed = session
        .put_attachment(vec![0; 1025], meta("too-large.png"))
        .await;
    assert!(
        failed.is_err(),
        "a refused put cannot yield an attachment-bearing input"
    );
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(lash::TurnInput::text("read the media").with_attachment(reference.clone()))
            .id(lash::TurnId::from("ref-law-turn"))
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.is_success(), "{output:?}");
    let history = output
        .result
        .state
        .read_view()
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .flat_map(lash_core::Part::attachments)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(history.first(), Some(&reference));
    core.shutdown().await.unwrap();
    let requests = requests.lock_recover().clone();
    Sent {
        history,
        requests,
        deliveries: attachments.deliveries.load(Ordering::SeqCst),
    }
}

fn identity(references: &[AttachmentRef]) -> lash_core::AppendRequestIdentity {
    let message = lash_core::PluginMessage {
        id: Some("same-message".into()),
        role: lash_core::MessageRole::User,
        origin: None,
        parts: references
            .iter()
            .enumerate()
            .map(|(i, reference)| {
                lash_core::Part::attachment_part(
                    format!("part-{i}"),
                    String::new(),
                    Some(lash::messages::PartAttachment {
                        reference: reference.clone(),
                    }),
                )
            })
            .collect(),
    };
    lash_core::RuntimeTurnCommitStamp::append_session_nodes(
        lash_core::OperationId::turn("ref-law", "ref-law-turn", "append"),
        None,
        &[lash_core::SessionAppendNode::message(message)],
    )
    .unwrap()
    .append_request_identity
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ref_commits_one_identity_however_it_is_delivered() {
    let bytes = send(false, false).await;
    let url = send(true, false).await;
    assert!(
        bytes.deliveries > 0 && url.deliveries > 0,
        "both stores delivered on the model send"
    );
    assert_eq!(bytes.history, url.history);
    let original = identity(&bytes.history);
    assert_eq!(original, identity(&url.history));
    let reference = &bytes.history[0];
    for changed in [
        AttachmentRef {
            media_type: "image/jpeg".parse().unwrap(),
            ..reference.clone()
        },
        AttachmentRef {
            byte_len: reference.byte_len + 1,
            ..reference.clone()
        },
        AttachmentRef {
            label: Some("changed.png".into()),
            ..reference.clone()
        },
        AttachmentRef {
            type_metadata: None,
            ..reference.clone()
        },
        AttachmentRef {
            id: lash_core::AttachmentId::parse("f".repeat(64)).unwrap(),
            ..reference.clone()
        },
    ] {
        assert_ne!(original, identity(&[changed]));
    }
    let other = AttachmentRef {
        label: Some("other occurrence".into()),
        ..reference.clone()
    };
    assert_ne!(
        identity(&[reference.clone(), other.clone()]),
        identity(&[other, reference.clone()])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_and_tool_attachments_reach_history_as_refs() {
    let sent = send(false, true).await;
    assert_eq!(
        sent.history.len(),
        2,
        "the input and nested MCP media survive in typed history"
    );
    assert_eq!(sent.history[1].label.as_deref(), Some("tool.png"));
    assert!(
        sent.requests.iter().any(|refs| refs == &sent.history),
        "the tool result reaches the next model send"
    );
    assert!(
        sent.deliveries >= 3,
        "the model sends delivered both input and tool-result refs"
    );
}
