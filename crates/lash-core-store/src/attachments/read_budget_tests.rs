use super::materialization::{encoding_allowance, materialization_cost};
use super::*;
use crate::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole, LlmToolChoice};
use crate::{AttachmentSource, MediaType};

fn request(sources: Vec<AttachmentSource>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "fixture".into(),
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
        model_variant: Default::default(),
        model_capability: Default::default(),
        attachment_acceptance: Default::default(),
        extra_body: Default::default(),
        request_defaults: Default::default(),
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
async fn falsely_small_metadata_refuses_actual_oversized_blob() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FileAttachmentStore::new(dir.path()));
    let mut reference = backend.put(vec![1; 5], meta()).await.unwrap();
    reference.byte_len = 0;
    let store = RuntimeAttachmentStore::ephemeral(backend).with_read_policy(policy(8192));
    let result =
        resolve_llm_request_attachments(request(vec![AttachmentSource::stored(reference)]), &store)
            .await;
    assert!(
        result.is_err(),
        "actual content must be bounded independently of reference metadata"
    );
}

#[tokio::test]
async fn legal_blobs_cross_aggregate_materialization_budget() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FileAttachmentStore::new(dir.path()));
    let first = backend.put(vec![1; 4], meta()).await.unwrap();
    let second = backend.put(vec![2; 4], meta()).await.unwrap();
    let store = RuntimeAttachmentStore::ephemeral(backend).with_read_policy(policy(2548));
    let result = resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(first),
            AttachmentSource::stored(second),
        ]),
        &store,
    )
    .await;
    assert!(
        result.is_err(),
        "legal individual blobs must not bypass the aggregate budget"
    );
}

#[tokio::test]
async fn repeated_ids_charge_retained_bytes_once() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FileAttachmentStore::new(dir.path()));
    let reference = backend.put(vec![1; 4], meta()).await.unwrap();
    // Four encoding copies, 1024 envelope bytes, and escaped MIME copies.
    let per_occurrence = 4 * 8 + 1024 + 24 * 9;
    let store =
        RuntimeAttachmentStore::ephemeral(backend).with_read_policy(policy(4 + 2 * per_occurrence));
    let result = resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(reference.clone()),
            AttachmentSource::stored(reference),
        ]),
        &store,
    )
    .await
    .unwrap();
    assert_eq!(result.resolved_stored.len(), 1);
    assert_eq!(
        result.resolved_stored.values().map(Vec::len).sum::<usize>(),
        4
    );
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
async fn repeated_ids_read_once_and_propagate_remaining_expansion_budget() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(ReadProbe {
        inner: FileAttachmentStore::new(dir.path()),
        reads: Default::default(),
        limit: Default::default(),
    });
    let reference = backend.put(vec![1; 4], meta()).await.unwrap();
    let per_occurrence = 4 * 8 + 1024 + 24 * 9;
    let store =
        RuntimeAttachmentStore::ephemeral(backend.clone()).with_read_policy(AttachmentReadPolicy {
            max_blob_bytes: 100,
            max_request_bytes: 4 + 2 * per_occurrence,
        });
    resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(reference.clone()),
            AttachmentSource::stored(reference),
        ]),
        &store,
    )
    .await
    .unwrap();
    assert_eq!(backend.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(backend.limit.load(std::sync::atomic::Ordering::SeqCst), 4);
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
