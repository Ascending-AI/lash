use super::materialization::{encoding_allowance, materialization_cost};
use super::*;
use crate::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole, LlmToolChoice};
use crate::{AttachmentSource, MediaType};

fn request(sources: Vec<AttachmentSource>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("fixture")
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            sources
                .into_iter()
                .map(|source| LlmContentBlock::Attachment {
                    source: Box::new(source),
                })
                .collect(),
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(vec![]),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance: Default::default(),
        scope: crate::llm::types::LlmRequestScope::new("session", "frame", "call"),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}
fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}
fn policy(max_request_bytes: u64) -> AttachmentReadPolicy {
    AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes,
    }
}

#[tokio::test]
async fn repeated_ids_still_charge_provider_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FileAttachmentStore::new(dir.path()));
    let reference = backend.put(vec![1; 4], meta()).await.unwrap();
    let store = RuntimeAttachmentStore::ephemeral(backend).with_read_policy(policy(2500));
    let result = resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(reference.clone()),
            AttachmentSource::stored(reference),
        ]),
        &store,
    )
    .await;
    assert!(
        result.is_err(),
        "each provider occurrence allocates its own encoding"
    );
}

#[tokio::test]
async fn pre_resolved_bytes_cannot_bypass_current_policy() {
    let mut input = request(vec![]);
    input
        .resolved_stored
        .insert(content_id(&[1; 5]), vec![1; 5]);
    let store = RuntimeAttachmentStore::unavailable().with_read_policy(policy(8192));
    assert!(matches!(
        resolve_llm_request_attachments(input, &store).await,
        Err(AttachmentStoreError::ReadLimitExceeded {
            byte_len: 5,
            max_bytes: 4
        })
    ));
}

#[tokio::test]
async fn inline_bytes_and_encoding_share_the_request_budget() {
    let store = RuntimeAttachmentStore::unavailable().with_read_policy(policy(2548));
    let input = request(vec![
        AttachmentSource::inline(meta().media_type, vec![1; 4]),
        AttachmentSource::inline(meta().media_type, vec![2; 4]),
    ]);
    assert!(matches!(
        resolve_llm_request_attachments(input, &store).await,
        Err(AttachmentStoreError::RequestBudgetExceeded { max_bytes: 2548 })
    ));
}

struct ReadProbe {
    inner: FileAttachmentStore,
    reads: std::sync::atomic::AtomicUsize,
    limit: std::sync::atomic::AtomicU64,
}
#[async_trait::async_trait]
impl AttachmentStore for ReadProbe {
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
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.limit
            .store(max_bytes, std::sync::atomic::Ordering::SeqCst);
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
}

#[tokio::test]
async fn empty_occurrences_refuse_before_backend_work() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(ReadProbe {
        inner: FileAttachmentStore::new(dir.path()),
        reads: Default::default(),
        limit: Default::default(),
    });
    let reference = backend.put(vec![], meta()).await.unwrap();
    let store = RuntimeAttachmentStore::ephemeral(backend.clone()).with_read_policy(policy(2100));
    assert!(
        resolve_llm_request_attachments(
            request(vec![
                AttachmentSource::stored(reference.clone()),
                AttachmentSource::stored(reference)
            ]),
            &store
        )
        .await
        .is_err()
    );
    assert_eq!(backend.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn expansion_overflow_is_refused() {
    assert!(encoding_allowance(u64::MAX, 0).is_none());
    assert!(materialization_cost(4, u64::MAX, 0).is_none());
}

#[tokio::test]
async fn retained_capacity_is_charged_even_for_short_content() {
    let mut bytes = Vec::with_capacity(8193);
    bytes.push(1);
    let mut input = request(vec![]);
    input.resolved_stored.insert(content_id(&bytes), bytes);
    let store = RuntimeAttachmentStore::unavailable().with_read_policy(policy(8192));
    assert!(matches!(
        resolve_llm_request_attachments(input, &store).await,
        Err(AttachmentStoreError::RequestBudgetExceeded { max_bytes: 8192 })
    ));
}
