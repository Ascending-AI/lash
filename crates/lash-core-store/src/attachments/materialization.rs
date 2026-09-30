use super::{AttachmentStoreError, RuntimeAttachmentStore};
use crate::AttachmentId;

/// Four base64-sized copies cover encoding scratch, a provider value, its
/// serialized body and a retained request-body copy. The envelope allowance
/// covers per-occurrence JSON punctuation; MIME and labels allow JSON escaping.
pub(super) fn encoding_allowance(byte_len: u64, overhead: u64) -> Option<u64> {
    byte_len
        .checked_add(2)?
        .checked_div(3)?
        .checked_mul(16)?
        .checked_add(overhead)
}

pub(super) fn materialization_cost(byte_len: u64, occurrences: u64, overhead: u64) -> Option<u64> {
    byte_len
        .checked_add(encoding_allowance(byte_len, 0)?.checked_mul(occurrences)?)?
        .checked_add(overhead)
}

pub async fn resolve_llm_request_attachments(
    mut request: crate::llm::types::LlmRequest,
    store: &RuntimeAttachmentStore,
) -> Result<crate::llm::types::LlmRequest, AttachmentStoreError> {
    let policy = store.read_policy();
    let refusal = || AttachmentStoreError::RequestBudgetExceeded {
        max_bytes: policy.max_request_bytes,
    };
    let mut remaining = policy.max_request_bytes;
    let mut stored_plan = std::collections::HashMap::<AttachmentId, u64>::new();
    for source in request.attachments() {
        let metadata_bytes = source
            .media_type()
            .map_or(0, |mime| mime.as_str().len() as u64)
            .checked_add(
                source
                    .stored_ref()
                    .and_then(|r| r.label.as_ref())
                    .map_or(0, |s| s.len() as u64),
            )
            .ok_or_else(refusal)?;
        let overhead = metadata_bytes
            .checked_mul(24)
            .and_then(|n| n.checked_add(1024))
            .ok_or_else(refusal)?;
        match source {
            crate::AttachmentSource::Stored { attachment_ref } => {
                let entry = stored_plan.entry(attachment_ref.id.clone()).or_default();
                *entry = entry.checked_add(1).ok_or_else(refusal)?;
                remaining = remaining.checked_sub(overhead).ok_or_else(refusal)?;
            }
            crate::AttachmentSource::Inline { bytes, .. } => {
                let len = bytes.len() as u64;
                if len > policy.max_blob_bytes {
                    return Err(AttachmentStoreError::ReadLimitExceeded {
                        byte_len: len,
                        max_bytes: policy.max_blob_bytes,
                    });
                }
                remaining = remaining
                    .checked_sub(
                        materialization_cost(len, 1, overhead)
                            .and_then(|cost| cost.checked_add(bytes.capacity() as u64 - len))
                            .ok_or_else(refusal)?,
                    )
                    .ok_or_else(refusal)?;
            }
            crate::AttachmentSource::ExternalUrl { url, .. } => {
                let cost = (url.len() as u64)
                    .checked_mul(24)
                    .and_then(|n| n.checked_add(overhead))
                    .ok_or_else(refusal)?;
                remaining = remaining.checked_sub(cost).ok_or_else(refusal)?;
            }
            crate::AttachmentSource::ProviderFile { id, .. } => {
                let cost = (id.len() as u64)
                    .checked_mul(24)
                    .and_then(|n| n.checked_add(overhead))
                    .ok_or_else(refusal)?;
                remaining = remaining.checked_sub(cost).ok_or_else(refusal)?;
            }
        }
    }
    // Caller-supplied cache entries are retained allocations too, even if no
    // source references them. Never trust a prior resolver's policy.
    for (id, bytes) in &request.resolved_stored {
        let len = bytes.len() as u64;
        if len > policy.max_blob_bytes {
            return Err(AttachmentStoreError::ReadLimitExceeded {
                byte_len: len,
                max_bytes: policy.max_blob_bytes,
            });
        }
        let occurrences = stored_plan.remove(id).unwrap_or_default();
        remaining = remaining
            .checked_sub(
                materialization_cost(len, occurrences, 0)
                    .and_then(|cost| cost.checked_add(bytes.capacity() as u64 - len))
                    .ok_or_else(refusal)?,
            )
            .ok_or_else(refusal)?;
    }
    for (id, occurrences) in stored_plan {
        // Invert the cost before reading: every streamed byte must fit both
        // the per-blob limit and the remaining budget including expansion.
        let mut low = 0;
        let mut high = policy.max_blob_bytes.min(remaining);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if materialization_cost(middle, occurrences, 0).is_some_and(|cost| cost <= remaining) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        let stored = store
            .backend
            .get(&id, low)
            .await
            .map_err(|error| match error {
                AttachmentStoreError::ReadLimitExceeded { .. } if low < policy.max_blob_bytes => {
                    refusal()
                }
                other => other,
            })?;
        let len = stored.bytes.len() as u64;
        if len > low || stored.bytes.capacity() as u64 > low {
            return Err(AttachmentStoreError::ReadLimitExceeded {
                byte_len: len,
                max_bytes: low,
            });
        }
        remaining = remaining
            .checked_sub(
                materialization_cost(len, occurrences, 0)
                    .and_then(|cost| cost.checked_add(stored.bytes.capacity() as u64 - len))
                    .ok_or_else(refusal)?,
            )
            .ok_or_else(refusal)?;
        request.resolved_stored.insert(id, stored.bytes);
    }
    Ok(request)
}
