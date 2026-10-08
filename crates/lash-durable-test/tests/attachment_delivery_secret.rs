//! DELIVERY-SECRET, the core's half (ADR 0135 §4, FIG-5445).
//!
//! A host store double delivers every attachment as a URL carrying a
//! recognizable signature. The provider receives that URL on the live wire
//! and nowhere else: after a turn whose call succeeds and a turn whose call
//! fails with a provider error that echoes the wire (scrubbed by the
//! provider, as every adapter must), the signature is in no durable row of
//! the store file (admission chunks, journals, history, observations), in
//! no trace record, and in neither turn's output or errors.

// Test code.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, Mutex};

use lash_core::provider::{Provider, ProviderComponents, ProviderHandle, ProviderOptions};
use lash_core::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentStore, AttachmentStoreError,
    AttachmentStorePersistence, LlmRequest, LlmResponse, StoredAttachment, StoredBlobRef,
};
use lash_core_execution::StoreSet;
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryForms, DeliveryLimits, DeliverySecret, ProviderAccepts,
};
use lash_sansio::llm::capability::{
    AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
};
use lash_sansio::llm::types::{LiveRequestBody, ProviderRouteIdentity};
use lash_sansio::sync::MutexExt as _;

/// What every signature this law's store mints starts with.
const SIGNATURE: &str = "lash-delivery-secret-signature";
/// The provider kind the host catalogue accepts images for.
const PROVIDER: &str = "secret-echo";

/// The host's backend, delivering every attachment as a URL signed for
/// that one delivery; every other method is the SQLite store's.
struct SigningStore {
    inner: Arc<dyn AttachmentStore>,
    signed: Mutex<u64>,
}

#[async_trait::async_trait]
impl AttachmentStore for SigningStore {
    fn persistence(&self) -> AttachmentStorePersistence {
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
        id: &AttachmentId,
        max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id, max_bytes).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }

    async fn deliver(
        &self,
        reference: &AttachmentRef,
        accepts: &ProviderAccepts,
        limits: &DeliveryLimits,
    ) -> Result<Delivery, AttachmentStoreError> {
        if !accepts.url {
            return self.inner.deliver(reference, accepts, limits).await;
        }
        let signed = {
            let mut signed = self.signed.lock_recover();
            *signed += 1;
            *signed
        };
        Ok(Delivery::Url {
            url: DeliverySecret::new(format!(
                "https://bucket.store.test/{}?X-Signature={SIGNATURE}-{signed}",
                reference.id
            )),
            valid_until_ms: Some(limits.valid_through_ms),
        })
    }
}

/// The scenario's provider: it records every live wire it is sent, answers
/// the first call, and fails every later one with an upstream error that
/// echoes the wire, scrubbed as an adapter scrubs provider text.
#[derive(Clone, Debug, Default)]
struct SecretEcho {
    options: ProviderOptions,
    wires: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Provider for SecretEcho {
    fn kind(&self) -> &'static str {
        PROVIDER
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(PROVIDER, "https://secret-echo.test/v1", model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn send(
        &mut self,
        _request: LlmRequest,
        body: &LiveRequestBody,
    ) -> Result<LlmResponse, lash_core::llm::transport::LlmTransportError> {
        let wire = body.wire();
        let first = {
            let mut wires = self.wires.lock_recover();
            wires.push(wire.clone());
            wires.len() == 1
        };
        if first {
            return Ok(LlmResponse {
                parts: vec![lash_core::LlmOutputPart::Text {
                    text: "seen".to_owned(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            });
        }
        let echoed = body.scrub(&format!("upstream rejected the request {wire}"));
        Err(lash_core::llm::transport::LlmTransportError::new(echoed)
            .with_kind(lash_core::ProviderFailureKind::Validation)
            .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::Forbidden)
            .with_request_body(body.redacted()))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// The host catalogue: the provider takes PNG images in messages by URL only.
fn url_only_images() -> AttachmentCapabilitySnapshot {
    AttachmentCapabilitySnapshot {
        revision: "delivery-secret".to_owned(),
        acceptors: vec![AttachmentAcceptor {
            provider: PROVIDER.to_owned(),
            rules: vec![AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message],
                media_types: vec!["image/png".to_owned()],
                media_families: Vec::new(),
                forms: DeliveryForms {
                    bytes: false,
                    url: true,
                    provider_file: false,
                },
            }],
        }],
    }
}

/// Every byte file the store set keeps under `dir`, read whole.
fn stored_bytes(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    std::fs::read_dir(dir)
        .expect("the store directory reads")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            (
                entry.path().display().to_string(),
                std::fs::read(entry.path()).expect("a store file reads"),
            )
        })
        .collect()
}

fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A delivered URL reaches the provider's live wire and no record, trace,
/// error or output, after a successful call and after a failing one whose
/// provider error echoed it.
#[tokio::test]
async fn a_delivered_url_reaches_only_the_live_wire() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let sqlite = lash_sqlite_store::SqliteStoreSet::open(dir.path().join("lash.db"))
        .await
        .expect("a file store set opens");
    let stores: Arc<dyn StoreSet> = Arc::new(sqlite);
    let backend =
        lash_core::testing::runtime_helpers::LayeredBackend::over(served::backend(stores))
            .map_attachment_store(|inner| {
                Arc::new(SigningStore {
                    inner,
                    signed: Mutex::new(0),
                }) as Arc<dyn AttachmentStore>
            })
            .into_backend();
    let provider = SecretEcho::default();
    let wires = Arc::clone(&provider.wires);
    let trace_path = dir.path().join("trace.jsonl");
    let core = lash::LashCore::standard_builder(backend.clone())
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .trace_jsonl_path(trace_path.clone())
        .serve_test_llm_profile(
            ProviderHandle::new(ProviderComponents::new(Box::new(provider))),
            served::metadata(),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "delivery-secret-deployment",
            "delivery-secret-boot",
        ))
        .expect("the core builds");
    let session_id = lash::SessionId::try_from("delivery-secret".to_owned()).expect("an id");
    let session = core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            served::spec(4).attachment_acceptance(Arc::new(url_only_images())),
        ))
        .await
        .expect("the session is created");
    // The host puts before it sends, under the session's upload holder.
    let uploads = lash_core::facade_support::RuntimeAttachmentStore::new(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session(session_id),
    );
    let image = uploads
        .put(
            b"a picture worth a signature".to_vec(),
            AttachmentCreateMeta::new(
                lash_core::MediaType::parse("image/png").expect("a media type"),
                None,
                Some("signed.png".to_owned()),
            ),
        )
        .await
        .expect("the host puts the image");

    let mut outputs = Vec::new();
    for text in ["look at the picture", "look again"] {
        let output = tokio::time::timeout(
            served::WATCHDOG,
            session
                .send(lash::TurnInput::text(text).with_attachment(image.clone()))
                .output(),
        )
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("the turn answers");
        outputs.push(format!("{output:?}"));
    }
    core.shutdown().await.expect("the core shuts down");

    let wires = wires.lock_recover().clone();
    assert_eq!(wires.len(), 2, "both turns sent their call: {wires:?}");
    for wire in &wires {
        assert!(
            wire.contains(SIGNATURE),
            "the live wire carries the delivered URL: {wire}"
        );
    }
    assert!(
        outputs[1].contains("upstream rejected the request"),
        "the second turn failed with the provider's error: {}",
        outputs[1]
    );
    for (turn, output) in outputs.iter().enumerate() {
        assert!(
            !output.contains(SIGNATURE),
            "turn {turn}'s output or errors carry the delivered URL: {output}"
        );
    }
    let traced = std::fs::read(&trace_path).unwrap_or_default();
    assert!(!traced.is_empty(), "the turns were traced");
    assert!(
        !holds(&traced, SIGNATURE),
        "a trace record carries the delivered URL"
    );
    for (file, bytes) in stored_bytes(dir.path()) {
        if file.ends_with("trace.jsonl") {
            continue;
        }
        assert!(
            !holds(&bytes, SIGNATURE),
            "the store file {file} holds the delivered URL"
        );
    }
}
