//! DELIVERY-SECRET (ADR 0135 §4): admissions and journals hold templates,
//! never deliveries. A URL and a provider file are filled live on every
//! attempt, including after the node dies before recording its answer.

// Test code.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};

use lash_core::provider::{Provider, ProviderComponents, ProviderHandle, ProviderOptions};
use lash_core::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentStore, AttachmentStoreError,
    AttachmentStorePersistence, LlmResponse, StoredAttachment, StoredBlobRef,
};
use lash_core_execution::StoreSet;
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryForms, DeliveryLimits, DeliverySecret, ProviderAccepts,
    ProviderFileScope,
};
use lash_sansio::llm::capability::{
    AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
};
use lash_sansio::llm::types::{
    LiveRequestBody, ProviderRouteIdentity, RecordedRequestTemplate, RequestSegment,
    ResponseContext,
};
use lash_sansio::sync::MutexExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What every signature this law's store mints starts with.
const SIGNATURE: &str = "lash-delivery-secret-signature";
/// The provider kind the host catalogue accepts images for.
const PROVIDER: &str = "delivery-recorder";

/// The host's backend, minting a URL or provider-file id for each delivery;
/// every other method is the SQLite store's.
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
        if !accepts.url && accepts.provider_file.is_none() {
            return self.inner.deliver(reference, accepts, limits).await;
        }
        let signed = {
            let mut signed = self.signed.lock_recover();
            *signed += 1;
            *signed
        };
        if reference.label.as_deref() == Some("file.png") {
            let scope = accepts.provider_file.clone().expect("a file slot");
            return Ok(Delivery::ProviderFile {
                scope,
                id: DeliverySecret::new(format!("file-{SIGNATURE}-{signed}")),
                valid_until_ms: Some(limits.valid_through_ms),
                uploaded: false,
            });
        }
        Ok(Delivery::Url {
            url: DeliverySecret::new(format!(
                "https://bucket.store.test/{}?X-Signature={SIGNATURE}-{signed}",
                reference.id
            )),
            valid_until_ms: Some(limits.valid_through_ms),
        })
    }
}

fn scope() -> ProviderFileScope {
    ProviderFileScope {
        provider: PROVIDER.into(),
        endpoint: "https://delivery-recorder.test/v1".into(),
        credential_scope: "law-account".into(),
    }
}

/// Records only live wires in the host double. Its first attempt can stay
/// pending until the simulated node is killed; the next owner answers it.
#[derive(Clone, Debug, Default)]
struct WireRecorder {
    options: ProviderOptions,
    wires: Arc<Mutex<Vec<(String, RecordedRequestTemplate)>>>,
    lowered: Arc<AtomicUsize>,
    crash: bool,
}

#[async_trait::async_trait]
impl Provider for WireRecorder {
    fn kind(&self) -> &'static str {
        PROVIDER
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(PROVIDER, "https://delivery-recorder.test/v1", model)
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
    fn attachment_file_scope(&self) -> Option<ProviderFileScope> {
        Some(scope())
    }
    async fn lower(
        &mut self,
        request: &lash_core::LlmRequest,
    ) -> Result<RecordedRequestTemplate, lash_core::llm::transport::LlmTransportError> {
        self.lowered.fetch_add(1, Ordering::SeqCst);
        let template = RecordedRequestTemplate::of_request(
            self.route_identity(request.model.wire_model()),
            request,
        )
        .map_err(lash_core::provider::attachment_wire::template_error)?;
        let mut segments = template.segments().to_vec();
        for segment in &mut segments {
            if let RequestSegment::Attachment { slot } = segment
                && slot.reference.label.as_deref() == Some("file.png")
            {
                slot.accepts = ProviderAccepts {
                    bytes: false,
                    url: false,
                    provider_file: Some(scope()),
                };
            }
        }
        RecordedRequestTemplate::from_segments(
            template.route.clone(),
            template.stream,
            template.generation,
            segments,
        )
        .map_err(lash_core::provider::attachment_wire::template_error)
    }
    async fn send(
        &mut self,
        body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, lash_core::llm::transport::LlmTransportError> {
        let first = {
            let mut wires = self.wires.lock_recover();
            wires.push((body.wire(), body.template().clone()));
            wires.len() == 1
        };
        if self.crash && first {
            std::future::pending::<()>().await;
        }
        Ok(LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "seen".into(),
                response_meta: None,
            }],
            request_body: Some(body.redacted()),
            ..LlmResponse::default()
        })
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// The host permits URL and provider-file deliveries for PNG images.
fn catalogue() -> AttachmentCapabilitySnapshot {
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
                    provider_file: true,
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

/// DELIVERY-SECRET: only templates are durable, before and after a send;
/// a takeover fills both slots afresh without lowering the admitted call.
#[tokio::test]
async fn delivered_values_stay_out_of_records_across_crash_and_resend() {
    for crash in [false, true] {
        let dir = tempfile::tempdir().expect("a store directory");
        let clock = SimClock::new();
        let sqlite = sim::file(dir.path().join("lash.db"), Arc::clone(&clock)).await;
        let database = Arc::new(sqlite.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(sqlite);
        let backend =
            lash_core::testing::runtime_helpers::LayeredBackend::over(sim::backend(stores))
                .map_attachment_store(|inner| {
                    Arc::new(SigningStore {
                        inner,
                        signed: Mutex::new(0),
                    }) as Arc<dyn AttachmentStore>
                })
                .into_backend();
        let provider = WireRecorder {
            crash,
            ..Default::default()
        };
        let wires = Arc::clone(&provider.wires);
        let lowered = Arc::clone(&provider.lowered);
        let core = lash::LashCore::standard_builder(backend.clone())
            .serve_sessions(false)
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .serve_test_llm_profile(
                ProviderHandle::new(ProviderComponents::new(Box::new(provider))),
                served::metadata(),
            )
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "delivery-secret-deployment",
                "delivery-secret-boot",
            ))
            .expect("the core builds");
        let nodes = SimNodes::new(
            database,
            Arc::clone(&clock),
            Script::new(),
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: backend.formats().decodes(),
                max_active: 4,
            },
            lash::testing::node_activation(&core, Arc::new(Tripwire::default()))
                .expect("the core activation")
                .1,
        );
        let session_id = lash::SessionId::try_from("delivery-secret".to_owned()).expect("an id");
        let session = core
            .session(session_id.clone())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                served::spec(4).attachment_acceptance(Arc::new(catalogue())),
            ))
            .await
            .expect("the session is created");
        let uploads = lash_core::facade_support::RuntimeAttachmentStore::new(
            backend.attachment_store(),
            backend.attachment_referrers(),
            lash_core::RuntimeOwner::Session(session_id),
        );
        let mut input = lash::TurnInput::text("look at both pictures");
        for (label, bytes) in [
            ("signed.png", b"a signed picture".to_vec()),
            ("file.png", b"an uploaded picture".to_vec()),
        ] {
            let reference = uploads
                .put(
                    bytes,
                    AttachmentCreateMeta::new(
                        "image/png".parse().expect("a media type"),
                        None,
                        Some(label.into()),
                    ),
                )
                .await
                .expect("host put");
            input = input.with_attachment(reference);
        }
        // send() admits mail; only the production engine activation drives it.
        let handle = session.send(input).await.expect("input accepted");
        nodes.start("a");
        while wires.lock_recover().is_empty() {
            assert!(clock.logical_ms() < 120_000, "the first attempt never sent");
            nodes.step().await;
        }
        // Admission, chunk blobs and phase rows are already durable here.
        assert_no_delivery_in_store(dir.path());
        if crash {
            nodes.kill("a");
            nodes.start("b");
        }
        let actor = lash_durable::ActorKey::session("delivery-secret").expect("an actor");
        loop {
            nodes.quiesce().await;
            if matches!(nodes.database().actor(&actor).await, Ok(Some(row)) if row.state == lash_durable::ActorState::Idle)
            {
                break;
            }
            assert!(clock.logical_ms() < 240_000, "the turn never settled");
            nodes.step().await;
        }
        let output = handle.output().await.expect("the settled output");
        assert!(output.is_success(), "the turn: {output:?}");
        let wires = wires.lock_recover().clone();
        assert_eq!(wires.len(), if crash { 2 } else { 1 });
        assert_eq!(
            lowered.load(Ordering::SeqCst),
            1,
            "takeover reads the admission"
        );
        for (wire, template) in &wires {
            assert!(
                wire.contains(&format!("file-{SIGNATURE}")),
                "the file reaches the live wire"
            );
            assert!(
                wire.contains(&format!("X-Signature={SIGNATURE}")),
                "the URL reaches the live wire"
            );
            assert_eq!(template.slots().count(), 2);
            assert!(!serde_json::to_string(template).unwrap().contains(SIGNATURE));
        }
        if crash {
            assert_eq!(
                wires[0].1, wires[1].1,
                "the admission pins literals and slots"
            );
            assert_ne!(wires[0].0, wires[1].0, "the resend fills fresh values");
            for first in [format!("{SIGNATURE}-1"), format!("{SIGNATURE}-2")] {
                assert!(!wires[1].0.contains(&first), "the resend reused {first}");
            }
        }
        assert_no_delivery_in_store(dir.path());
        nodes.kill("a");
        nodes.kill("b");
        core.shutdown().await.expect("the core shuts down");
    }
}

fn assert_no_delivery_in_store(dir: &std::path::Path) {
    for (file, bytes) in stored_bytes(dir) {
        assert!(
            !holds(&bytes, SIGNATURE),
            "the store file {file} holds a delivered value"
        );
    }
}
