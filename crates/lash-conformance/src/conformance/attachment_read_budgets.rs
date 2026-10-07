use crate::*;
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole, LlmToolChoice};

fn request(sources: Vec<AttachmentSource>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "fixture",
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                )
                .with_capability(Default::default())
                .with_extra_body(Default::default())
                .with_request_defaults(Default::default()),
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
        scope: lash_core::llm::types::LlmRequestScope::new("session", "frame", "call"),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}
#[expect(clippy::unwrap_used, reason = "fixture MIME is valid")]
fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}
/// Every attachment backend bounds actual bytes before returning a retained blob.
#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_materialization_read_budgets(backend: Arc<dyn AttachmentStore>) {
    let first = backend
        .put(vec![1; 4], meta())
        .await
        .expect("put legal blob");
    let second = backend
        .put(vec![2; 4], meta())
        .await
        .expect("put second legal blob");
    let mut large = backend
        .put(vec![3; 5], meta())
        .await
        .expect("put foreign-runtime blob");
    large.byte_len = 0;
    let store = RuntimeAttachmentStore::ephemeral(Arc::clone(&backend)).with_read_policy(
        AttachmentReadPolicy {
            max_blob_bytes: 4,
            max_request_bytes: 8192,
        },
    );
    assert!(matches!(
        backend.get(&large.id, 4).await,
        Err(AttachmentStoreError::ReadLimitExceeded { .. })
    ));
    assert_eq!(
        backend
            .get(&first.id, 4)
            .await
            .expect("exact limit")
            .bytes
            .len(),
        4
    );
    assert!(
        resolve_llm_request_attachments(request(vec![AttachmentSource::stored(large)]), &store)
            .await
            .is_err()
    );
    let bounded = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 2548,
    });
    assert!(
        resolve_llm_request_attachments(
            request(vec![
                AttachmentSource::stored(first.clone()),
                AttachmentSource::stored(second)
            ]),
            &bounded
        )
        .await
        .is_err()
    );
    let per_occurrence = 4 * 8 + 1024 + 24 * 9;
    let exact = store.reconfigured_read_policy(AttachmentReadPolicy {
        max_blob_bytes: 4,
        max_request_bytes: 4 + 2 * per_occurrence,
    });
    let resolved = resolve_llm_request_attachments(
        request(vec![
            AttachmentSource::stored(first.clone()),
            AttachmentSource::stored(first),
        ]),
        &exact,
    )
    .await
    .expect("deduplicated retained bytes fit exactly");
    assert_eq!(resolved.resolved_stored.len(), 1);
}
