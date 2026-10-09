//! The real attempt loop and derivative cache, with a synthetic uploader.
use super::{Case, Meter, Receipt, facade};
use anyhow::{Result, ensure};
use lash_core::attachments::{
    ProviderFileCacheLimits, ProviderFileDelivery, ProviderFileUploader, UploadedProviderFile,
};
use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_core::llm::types::{
    LiveRequestBody, LlmRequest, LlmResponse, RecordedRequestTemplate, RequestSegment,
    ResponseContext,
};
use lash_core::provider::{Provider, ProviderComponents, ProviderHandle, ProviderOptions};
use lash_core::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentStore, AttachmentStoreError,
    AttachmentStorePersistence, StoredAttachment, StoredBlobRef,
};
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryForms, DeliveryLimits, DeliverySecret, ProviderAccepts,
    ProviderFileScope,
};
use lash_sansio::llm::capability::{
    AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
};
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn scope() -> ProviderFileScope {
    ProviderFileScope {
        provider: "boundary-slots".into(),
        endpoint: "https://synthetic.invalid".into(),
        credential_scope: "synthetic-account".into(),
    }
}
struct Upload {
    scope: ProviderFileScope,
    meter: Meter,
}
#[async_trait::async_trait]
impl ProviderFileUploader for Upload {
    fn scope(&self) -> &ProviderFileScope {
        &self.scope
    }
    async fn upload(
        &self,
        reference: &AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError> {
        let start = Instant::now();
        if bytes.len() as u64 != reference.byte_len {
            return Err(AttachmentStoreError::Contract("upload length".into()));
        }
        let index = self.meter.count("attachment.derivative.upload");
        self.meter.record("attachment.derivative.upload", 1, start);
        Ok(UploadedProviderFile {
            id: DeliverySecret::new(format!("synthetic-file-{index}")),
            valid_until_ms: None,
        })
    }
}
struct MeasuredStore {
    inner: Arc<dyn AttachmentStore>,
    meter: Meter,
    delivery: bool,
}
#[async_trait::async_trait]
impl AttachmentStore for MeasuredStore {
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
        limit: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        let start = Instant::now();
        let result = self.inner.get(id, limit).await;
        self.meter.record("attachment.resolution", 1, start);
        result
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
    async fn deliver(
        &self,
        reference: &AttachmentRef,
        accepts: &ProviderAccepts,
        limits: &DeliveryLimits,
    ) -> Result<Delivery, AttachmentStoreError> {
        let start = Instant::now();
        let result = self.inner.deliver(reference, accepts, limits).await;
        if self.delivery {
            if matches!(
                &result,
                Ok(Delivery::ProviderFile {
                    uploaded: false,
                    ..
                })
            ) {
                self.meter
                    .record("attachment.derivative.cache_hit", 1, start);
            }
            self.meter.record("attachment.delivery", 1, start);
        }
        result
    }
    async fn invalidate_delivery(
        &self,
        reference: &AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentStoreError> {
        let start = Instant::now();
        let result = self.inner.invalidate_delivery(reference, rejected).await;
        if self.delivery {
            self.meter
                .record("attachment.derivative.invalidate", 1, start);
        }
        result
    }
}

#[derive(Clone, Debug)]
struct WireProvider {
    options: ProviderOptions,
    meter: Meter,
    templates: Arc<Mutex<Vec<RecordedRequestTemplate>>>,
}
#[async_trait::async_trait]
impl Provider for WireProvider {
    fn kind(&self) -> &'static str {
        "boundary-slots"
    }
    fn route_identity(&self, model: &str) -> lash_core::llm::types::ProviderRouteIdentity {
        lash_core::llm::types::ProviderRouteIdentity::for_endpoint(
            self.kind(),
            "https://synthetic.invalid",
            model,
        )
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
        request: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        let start = Instant::now();
        let template = RecordedRequestTemplate::of_request(
            self.route_identity(request.model.wire_model()),
            request,
        )
        .map_err(lash_core::provider::attachment_wire::template_error)?;
        let mut segments = template.segments().to_vec();
        for segment in &mut segments {
            if let RequestSegment::Attachment { slot } = segment {
                slot.accepts = ProviderAccepts {
                    bytes: false,
                    url: false,
                    provider_file: Some(scope()),
                };
            }
        }
        let result = RecordedRequestTemplate::from_segments(
            template.route.clone(),
            template.stream(),
            template.generation,
            segments,
        )
        .map_err(lash_core::provider::attachment_wire::template_error);
        self.meter.record("attachment.template.lower", 1, start);
        result
    }
    async fn send(
        &mut self,
        body: &LiveRequestBody,
        _: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        let start = Instant::now();
        self.templates.lock_recover().push(body.template().clone());
        let attempt = self.meter.count("attachment.provider.send") % 3;
        self.meter.record("attachment.provider.send", 1, start);
        if attempt == 0 {
            let mut error = LlmTransportError::new("synthetic file rejected")
                .with_kind(ProviderFailureKind::Validation)
                .with_rejected_slots(body.provider_file_slots());
            error.http_status = Some(400);
            return Err(error);
        }
        if attempt == 1 {
            return Err(LlmTransportError::new("synthetic retry before response")
                .with_kind(ProviderFailureKind::Transport)
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
        }
        Ok(facade::response("attachment answered".into()))
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

pub(super) async fn run(operations: usize) -> Result<Receipt> {
    let meter = Meter::default();
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await?);
    let raw = facade::backend(stores)?;
    let observed = meter.clone();
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(raw)
        .map_attachment_store(move |inner| {
            let originals = Arc::new(MeasuredStore {
                inner,
                meter: observed.clone(),
                delivery: false,
            });
            let derivatives = Arc::new(ProviderFileDelivery::new(
                originals,
                vec![Arc::new(Upload {
                    scope: scope(),
                    meter: observed.clone(),
                })],
                ProviderFileCacheLimits::standard(),
            ));
            Arc::new(MeasuredStore {
                inner: derivatives,
                meter: observed,
                delivery: true,
            }) as Arc<dyn AttachmentStore>
        })
        .into_backend();
    let mut options = ProviderOptions::default();
    options.reliability.retry.enabled = true;
    options.reliability.retry.max_attempts = Some(3);
    options.reliability.retry.base_delay_ms = 1;
    options.reliability.retry.max_delay_ms = 1;
    options.reliability.retry.jitter_ms = 0;
    let templates = Arc::new(Mutex::new(Vec::new()));
    let provider = ProviderHandle::new(ProviderComponents::new(Box::new(WireProvider {
        options,
        meter: meter.clone(),
        templates: templates.clone(),
    })));
    let core = facade::build(backend, "slots", true, true, provider)?;
    let result = async {
        let catalogue = AttachmentCapabilitySnapshot {
            revision: "boundary".into(),
            acceptors: vec![AttachmentAcceptor {
                provider: "boundary-slots".into(),
                rules: vec![AttachmentAcceptanceRule {
                    positions: vec![AttachmentPosition::Message],
                    media_types: vec!["image/png".into()],
                    media_families: vec![],
                    forms: DeliveryForms {
                        bytes: true,
                        url: true,
                        provider_file: true,
                    },
                }],
            }],
        };
        let workload = crate::workload::Workload::smoke_v1()?;
        let generator = crate::workload::Generator::new(&workload, "boundary-slots")?;
        for n in 0..operations {
            let id = lash::SessionId::try_from(format!("slots-{n}"))?;
            core.session(id.clone())
                .create(lash::SessionCreation::root(
                    lash::plugins::SessionToolAccess::ambient(),
                    facade::spec()?.attachment_acceptance(Arc::new(catalogue.clone())),
                ))
                .await?;
            let session = core.session(id).open().await?;
            let reference = session
                .put_attachment(
                    generator.png(0, n as u64, "wire", 1024)?,
                    AttachmentCreateMeta::new("image/png".parse()?, None, Some(format!("{n}.png"))),
                )
                .await?;
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                session
                    .send(lash::TurnInput::text("look").with_attachment(reference))
                    .output(),
            )
            .await??;
            ensure!(
                matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
                "attachment turn failed: {:?}; calls={:?}; phases={:?}",
                output.result.outcome,
                output.result.llm_calls,
                meter.0.lock_recover()
            );
        }
        ensure!(
            meter.count("attachment.delivery") == operations * 3,
            "WIRE-SLOTS did not deliver each attempt"
        );
        ensure!(
            meter.count("attachment.template.lower") == operations,
            "retry lowered again"
        );
        ensure!(
            meter.count("attachment.resolution") == operations * 2,
            "resolution population changed"
        );
        ensure!(
            meter.count("attachment.derivative.invalidate") == operations,
            "rejection did not invalidate one derivative"
        );
        ensure!(
            meter.count("attachment.derivative.upload") == operations * 2,
            "derivative miss/rejection population changed"
        );
        ensure!(
            meter.count("attachment.derivative.cache_hit") == operations,
            "warm retry did not reuse derivative"
        );
        for calls in templates.lock_recover().as_chunks::<3>().0 {
            ensure!(
                calls[0] == calls[1] && calls[1] == calls[2],
                "retry changed admitted template"
            );
        }
        anyhow::Ok(())
    }
    .await;
    core.shutdown().await?;
    result?;
    Ok(Receipt::new(
        Case::WireSlots,
        "facade+provider-attempt",
        "sqlite-memory-product+synthetic-uploader",
        operations,
        &meter,
        serde_json::json!({"attempts": operations * 3, "uploads": operations * 2, "warm_derivatives": operations,
            "retry_causes": ["provider-file-rejected", "transport-before-response"]}),
    ))
}
