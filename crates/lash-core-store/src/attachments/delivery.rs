//! One attempt's bounded, validated slot deliveries.
use super::{AttachmentStoreError, RuntimeAttachmentStore, validate_attachment_bytes};
use lash_core_llm::provider::{AttachmentDeliveryError, SlotDeliveries};
use lash_sansio::AttachmentRef;
use lash_sansio::llm::attachment_delivery::{
    Delivery, DeliveryContext, DeliveryLimits, ProviderAccepts,
};
use lash_sansio::llm::types::AttachmentSlot;
use std::sync::Arc;

pub(super) fn encoding_allowance(byte_len: u64) -> Option<u64> {
    byte_len.checked_add(2)?.checked_div(3)?.checked_mul(16)
}
pub(super) fn delivery_cost(capacity: u64, byte_len: u64, occurrences: u64) -> Option<u64> {
    capacity.checked_add(encoding_allowance(byte_len)?.checked_mul(occurrences)?)
}

struct Group<'a> {
    reference: &'a AttachmentRef,
    accepts: ProviderAccepts,
    indices: Vec<usize>,
}

#[async_trait::async_trait]
impl SlotDeliveries for RuntimeAttachmentStore {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        let policy = self.read_policy();
        let refusal = || AttachmentDeliveryError::LimitExceeded {
            max_bytes: policy.max_request_bytes,
        };
        let mut remaining = policy.max_request_bytes;
        let mut groups: Vec<Group<'_>> = Vec::new();
        // Reserve all envelopes before making the first infrastructure call.
        for (index, slot) in slots.iter().enumerate() {
            let reference = &slot.reference;
            let metadata = (reference.media_type.as_str().len() as u64)
                .checked_add(
                    reference
                        .label
                        .as_ref()
                        .map_or(0, |label| label.len() as u64),
                )
                .ok_or_else(refusal)?;
            let overhead = metadata
                .checked_mul(24)
                .and_then(|n| n.checked_add(1024))
                .ok_or_else(refusal)?;
            remaining = remaining.checked_sub(overhead).ok_or_else(refusal)?;
            let accepts = slot
                .accepts
                .narrowed_to_live_scope(ctx.live_file_scope.as_ref());
            if accepts.is_empty() {
                return Err(AttachmentDeliveryError::Unsupported {
                    id: reference.id.clone(),
                });
            }
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.reference.id == reference.id && group.accepts == accepts)
            {
                if group.reference.byte_len != reference.byte_len {
                    return Err(AttachmentDeliveryError::ContentMismatch {
                        id: reference.id.clone(),
                    });
                }
                group.indices.push(index);
            } else {
                groups.push(Group {
                    reference,
                    accepts,
                    indices: vec![index],
                });
            }
        }
        let mut result = vec![None; slots.len()];
        for group in groups {
            let occurrences = group.indices.len() as u64;
            let mut low = 0;
            let mut high = policy.max_blob_bytes.min(remaining);
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if delivery_cost(middle, middle, occurrences).is_some_and(|cost| cost <= remaining)
                {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            let limits = DeliveryLimits {
                max_bytes: low,
                valid_through_ms: ctx.valid_through_ms,
            };
            let delivery = self
                .backend()
                .deliver(group.reference, &group.accepts, &limits)
                .await
                .map_err(|error| match error {
                    AttachmentStoreError::ReadLimitExceeded { .. }
                        if low < policy.max_blob_bytes =>
                    {
                        refusal()
                    }
                    other => delivery_error(other),
                })?;
            if !group.accepts.allows(&delivery) {
                return Err(AttachmentDeliveryError::Refused {
                    id: group.reference.id.clone(),
                });
            }
            let cost = match &delivery {
                Delivery::Bytes(bytes) => {
                    validate_attachment_bytes(group.reference, bytes, bytes.capacity() as u64, low)
                        .map_err(delivery_error)?;
                    delivery_cost(bytes.capacity() as u64, bytes.len() as u64, occurrences)
                        .ok_or_else(refusal)?
                }
                Delivery::Url {
                    url,
                    valid_until_ms,
                } => {
                    check_horizon(group.reference, *valid_until_ms, ctx)?;
                    (url.len() as u64)
                        .checked_mul(24)
                        .and_then(|n| n.checked_mul(occurrences))
                        .ok_or_else(refusal)?
                }
                Delivery::ProviderFile {
                    id, valid_until_ms, ..
                } => {
                    check_horizon(group.reference, *valid_until_ms, ctx)?;
                    // Reserve the bounded upload scratch even on cache hits. The wrapper
                    // reads with min(max_bytes, byte_len), so its capacity fits this charge.
                    if group.reference.byte_len > low {
                        return Err(AttachmentDeliveryError::LimitExceeded { max_bytes: low });
                    }
                    delivery_cost(
                        group.reference.byte_len,
                        group.reference.byte_len,
                        occurrences,
                    )
                    .and_then(|n| {
                        (id.len() as u64)
                            .checked_mul(24)?
                            .checked_mul(occurrences)?
                            .checked_add(n)
                    })
                    .ok_or_else(refusal)?
                }
            };
            remaining = remaining.checked_sub(cost).ok_or_else(refusal)?;
            let delivery = Arc::new(delivery);
            for index in group.indices {
                result[index] = Some(Arc::clone(&delivery));
            }
        }
        result
            .into_iter()
            .map(|delivery| delivery.ok_or_else(refusal))
            .collect()
    }
    async fn invalidate(
        &self,
        reference: &AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        self.backend()
            .invalidate_delivery(reference, rejected)
            .await
            .map_err(delivery_error)
    }
}
fn check_horizon(
    reference: &AttachmentRef,
    valid_until_ms: Option<u64>,
    ctx: &DeliveryContext,
) -> Result<(), AttachmentDeliveryError> {
    if valid_until_ms.is_some_and(|expiry| expiry < ctx.valid_through_ms) {
        Err(AttachmentDeliveryError::Refused {
            id: reference.id.clone(),
        })
    } else {
        Ok(())
    }
}
fn delivery_error(error: AttachmentStoreError) -> AttachmentDeliveryError {
    match error {
        AttachmentStoreError::DeliveryUnsupported { id, .. } => {
            AttachmentDeliveryError::Unsupported { id }
        }
        AttachmentStoreError::NotFound(id) => AttachmentDeliveryError::Missing { id },
        AttachmentStoreError::ContentMismatch { id, .. } => {
            AttachmentDeliveryError::ContentMismatch { id }
        }
        AttachmentStoreError::ReadLimitExceeded { max_bytes, .. }
        | AttachmentStoreError::RequestBudgetExceeded { max_bytes } => {
            AttachmentDeliveryError::LimitExceeded { max_bytes }
        }
        other => {
            let retryable = other.is_retryable();
            let message = match other {
                AttachmentStoreError::Backend {
                    operation, class, ..
                } => format!("backend {operation} ({class})"),
                AttachmentStoreError::ReferrersOperationFailed { operation, .. } => {
                    format!("referrers {operation} failed")
                }
                AttachmentStoreError::Io { .. } => "store I/O failed".into(),
                _ => "store operation failed".into(),
            };
            AttachmentDeliveryError::Unavailable { retryable, message }
        }
    }
}
