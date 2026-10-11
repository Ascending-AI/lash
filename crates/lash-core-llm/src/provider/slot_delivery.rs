//! The seam an admitted call's attempt fills its attachment slots through
//! (ADR 0135 §3, §4; ADR 0133 §6, WIRE-SLOTS).
//!
//! The attempt loop of [`ProviderHandle`](super::ProviderHandle) hands every
//! slot of the call's recorded template to a [`SlotDeliveries`] once per
//! attempt, inside the attempt's deadline and cancellation, and encodes each
//! returned delivery through the slot's pinned codec. A delivery lives only
//! in that attempt: nothing here is serialized, recorded or traced. The
//! runtime attachment store implements the seam; a provider double with no
//! store passes [`NoSlotDeliveries`].

use std::sync::Arc;

use lash_sansio::llm::attachment_delivery::{Delivery, DeliveryContext};
use lash_sansio::llm::types::{AttachmentSlot, LiveRequestBody, RecordedRequestTemplate};
use lash_sansio::{AttachmentId, AttachmentRef};

use super::Provider;
use crate::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_sansio::session_model::TurnFailureCode;

/// What fills the attachment slots of one attempt.
#[async_trait::async_trait]
pub trait SlotDeliveries: Send + Sync {
    /// Deliver every slot of one attempt under one fresh request budget
    /// (`AttachmentReadPolicy`). Returns one delivery per slot, in slot order;
    /// slots with equal `(reference.id, reference.media_type, effective
    /// accepts)` share one `Arc` (one backend call, bytes retained once). Effective accepts =
    /// `slot.accepts.narrowed_to_live_scope(ctx.live_file_scope.as_ref())`.
    ///
    /// For each delivery the impl checks, before returning it: the form is
    /// allowed by the effective accepts (scope by `==`); a Bytes delivery has
    /// `len == byte_len`, `capacity <= limits.max_bytes` and
    /// `content_id(bytes) == id`; a Url/ProviderFile has `valid_until_ms`
    /// absent or `>= ctx.valid_through_ms`. A failed check is
    /// [`AttachmentDeliveryError::Refused`] (a host-store defect) or
    /// [`AttachmentDeliveryError::ContentMismatch`].
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError>;

    /// Forget a delivery the provider definitely rejected (an expired or
    /// missing file id), so the next attempt delivers afresh. Compare-and-invalidate: a newer cached delivery stays.
    async fn invalidate(
        &self,
        reference: &AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError>;
}

/// The seam of a host with no attachment store: it refuses every slot as
/// [`AttachmentDeliveryError::Unsupported`]. A template with no slot never
/// asks it.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSlotDeliveries;

#[async_trait::async_trait]
impl SlotDeliveries for NoSlotDeliveries {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        _ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        match slots.first() {
            Some(slot) => Err(AttachmentDeliveryError::Unsupported {
                id: slot.reference.id.clone(),
            }),
            None => Ok(Vec::new()),
        }
    }

    async fn invalidate(
        &self,
        _reference: &AttachmentRef,
        _rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        Ok(())
    }
}

/// Why an attempt's slots could not be delivered. Every variant is unsent:
/// nothing reached the provider and nothing was charged.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentDeliveryError {
    #[error("attachment `{id}` cannot be delivered in any accepted form")]
    Unsupported { id: AttachmentId },
    #[error("attachment `{id}` is missing")]
    Missing { id: AttachmentId },
    #[error("attachment `{id}` does not match its reference")]
    ContentMismatch { id: AttachmentId },
    #[error("attachment delivery exceeds the {max_bytes}-byte limit")]
    LimitExceeded { max_bytes: u64 },
    #[error("the attachment store returned a delivery outside the slot's acceptance")]
    Refused { id: AttachmentId },
    /// A backend, referrers or upload fault. `message` is pre-redacted.
    #[error("attachment delivery is unavailable: {message}")]
    Unavailable { retryable: bool, message: String },
}

impl AttachmentDeliveryError {
    /// The code a call this error settles carries (ADR 0135 §4).
    pub fn failure_code(&self) -> TurnFailureCode {
        match self {
            Self::Unsupported { .. } => TurnFailureCode::UnsupportedAttachmentCapability,
            Self::Missing { .. }
            | Self::ContentMismatch { .. }
            | Self::Refused { .. }
            | Self::LimitExceeded { .. } => TurnFailureCode::AttachmentResolutionFailed,
            Self::Unavailable { .. } => TurnFailureCode::AttachmentDeliveryUnavailable,
        }
    }

    /// Whether the attempt may be made again: only a transient fault, and
    /// then within the call's pinned deadline and its provider's attempts.
    pub fn retry_verdict(&self) -> TransportRetryVerdict {
        match self {
            Self::Unsupported { .. } | Self::LimitExceeded { .. } => {
                TransportRetryVerdict::Forbidden
            }
            Self::Missing { .. } | Self::ContentMismatch { .. } | Self::Refused { .. } => {
                TransportRetryVerdict::NotRetryable
            }
            Self::Unavailable {
                retryable: true, ..
            } => TransportRetryVerdict::RetryableTransient,
            Self::Unavailable {
                retryable: false, ..
            } => TransportRetryVerdict::NotRetryable,
        }
    }

    /// The attempt failure this error is: unsent, typed by
    /// [`Self::failure_code`] and [`Self::retry_verdict`]. Its text names
    /// refs only, never a delivered value.
    pub fn into_transport_error(self) -> LlmTransportError {
        let kind = match &self {
            Self::Unsupported { .. } | Self::LimitExceeded { .. } => {
                ProviderFailureKind::Validation
            }
            Self::Unavailable {
                retryable: true, ..
            } => ProviderFailureKind::Transport,
            Self::Missing { .. }
            | Self::ContentMismatch { .. }
            | Self::Refused { .. }
            | Self::Unavailable { .. } => ProviderFailureKind::Unknown,
        };
        let code = self.failure_code();
        let verdict = self.retry_verdict();
        LlmTransportError::new(self.to_string())
            .with_kind(kind)
            .with_lash_code(code)
            .with_retry_verdict(verdict)
    }
}

/// The one fill of an attempt, for [`ProviderHandle`](super::ProviderHandle)'s
/// attempt loop and [`Provider::complete`] alike. Fill `template`'s slots:
/// deliver every slot through
/// `deliveries` (none for a template with no slot), check each delivery is a
/// form its slot's acceptance allows, encode each through the slot's pinned
/// codec, and fill the literals. The deliveries are returned with the live
/// body so a rejected one can be forgotten; neither is ever recorded.
pub(super) async fn fill_slots(
    provider: &(impl Provider + ?Sized),
    template: &Arc<RecordedRequestTemplate>,
    deliveries: &dyn SlotDeliveries,
    valid_through_ms: u64,
) -> Result<(LiveRequestBody, Vec<Arc<Delivery>>), LlmTransportError> {
    let slots: Vec<&AttachmentSlot> = template.slots().collect();
    let live_file_scope = provider.attachment_file_scope();
    let delivered = if slots.is_empty() {
        Vec::new()
    } else {
        let ctx = DeliveryContext {
            valid_through_ms,
            live_file_scope: live_file_scope.clone(),
        };
        deliveries
            .deliver(&slots, &ctx)
            .await
            .map_err(AttachmentDeliveryError::into_transport_error)?
    };
    if delivered.len() != slots.len() {
        return Err(AttachmentDeliveryError::Unavailable {
            retryable: false,
            message: format!(
                "the attachment store answered {} deliveries for {} slots",
                delivered.len(),
                slots.len()
            ),
        }
        .into_transport_error());
    }
    let mut values = Vec::with_capacity(slots.len());
    for (slot, delivery) in slots.iter().zip(&delivered) {
        if !slot
            .accepts
            .narrowed_to_live_scope(live_file_scope.as_ref())
            .allows(delivery)
        {
            return Err(AttachmentDeliveryError::Refused {
                id: slot.reference.id.clone(),
            }
            .into_transport_error());
        }
        values.push(provider.encode_slot(slot, delivery)?);
    }
    let live = LiveRequestBody::fill(Arc::clone(template), values).map_err(|error| {
        LlmTransportError::new(format!(
            "the admitted request template cannot be filled: {error}"
        ))
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(TurnFailureCode::AdmittedRequestUnavailable)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
    })?;
    Ok((live, delivered))
}

/// Forget each delivery the provider rejected, by slot index. A failure to
/// forget is logged by ref only and does not change the attempt's outcome:
/// the next attempt's delivery is checked again either way.
pub(super) async fn invalidate_rejected(
    template: &RecordedRequestTemplate,
    deliveries: &dyn SlotDeliveries,
    delivered: &[Arc<Delivery>],
    rejected: &[usize],
) {
    let slots: Vec<&AttachmentSlot> = template.slots().collect();
    for &index in rejected {
        let (Some(slot), Some(delivery)) = (slots.get(index), delivered.get(index)) else {
            continue;
        };
        if let Err(error) = deliveries.invalidate(&slot.reference, delivery).await {
            tracing::warn!(
                target: "lash_core::provider::reliability",
                attachment_id = %slot.reference.id,
                error = %error,
                "a rejected attachment delivery could not be forgotten"
            );
        }
    }
}

/// Read limits, independent of put admission and retained-history policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentReadPolicy {
    pub max_blob_bytes: u64,
    /// Unique retained buffer capacity plus provider encoding allowance per occurrence.
    /// This bounds attachment payload work, not the allocator or the full prompt.
    pub max_request_bytes: u64,
}
impl AttachmentReadPolicy {
    /// The read budgets the standard attachment policy states: 32 MiB per
    /// blob and 128 MiB per request. No measurement backs either value.
    pub const STANDARD: Self = Self {
        max_blob_bytes: 32 * 1024 * 1024,
        max_request_bytes: 128 * 1024 * 1024,
    };
}

/// Immutable bytes for calls outside persistence. Construction checks every
/// content address and length, retaining one buffer per content id. Each
/// delivery reserves the same envelope and inline encoding costs as the host
/// store under the explicitly supplied read policy; context supplies the live
/// scope and validity horizon, and carries no byte budget.
#[derive(Clone, Debug)]
pub struct InlineDeliveries {
    bytes: std::collections::BTreeMap<AttachmentId, Arc<Delivery>>,
    policy: AttachmentReadPolicy,
}
impl InlineDeliveries {
    pub fn from_bytes(
        values: impl IntoIterator<Item = (AttachmentRef, Vec<u8>)>,
        policy: AttachmentReadPolicy,
    ) -> Result<Self, AttachmentDeliveryError> {
        let mut bytes = std::collections::BTreeMap::new();
        for (reference, value) in values {
            check_bytes(&reference, &value)?;
            bytes
                .entry(reference.id)
                .or_insert_with(|| Arc::new(Delivery::Bytes(value)));
        }
        Ok(Self { bytes, policy })
    }
}
#[async_trait::async_trait]
impl SlotDeliveries for InlineDeliveries {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        let values = slots
            .iter()
            .map(|slot| {
                self.bytes.get(&slot.reference.id).cloned().ok_or_else(|| {
                    AttachmentDeliveryError::Missing {
                        id: slot.reference.id.clone(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_deliveries(slots, &values, ctx, self.policy)?;
        Ok(values)
    }
    async fn invalidate(
        &self,
        _: &AttachmentRef,
        _: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        Ok(())
    }
}

pub(super) fn check_bytes(
    reference: &AttachmentRef,
    bytes: &[u8],
) -> Result<(), AttachmentDeliveryError> {
    if bytes.len() as u64 != reference.byte_len
        || lash_sansio::attachment::content_id(bytes) != reference.id
    {
        return Err(AttachmentDeliveryError::ContentMismatch {
            id: reference.id.clone(),
        });
    }
    Ok(())
}

pub(super) fn validate_deliveries(
    slots: &[&AttachmentSlot],
    values: &[Arc<Delivery>],
    ctx: &DeliveryContext,
    policy: AttachmentReadPolicy,
) -> Result<(), AttachmentDeliveryError> {
    let refuse = || AttachmentDeliveryError::LimitExceeded {
        max_bytes: policy.max_request_bytes,
    };
    let mut remaining = policy.max_request_bytes;
    let mut seen = std::collections::HashSet::new();
    // Reserve every occurrence's envelope before inspecting payloads.
    for slot in slots {
        let metadata = (slot.reference.media_type.as_str().len() as u64)
            .checked_add(
                slot.reference
                    .label
                    .as_ref()
                    .map_or(0, |label| label.len() as u64),
            )
            .ok_or_else(refuse)?;
        let overhead = metadata
            .checked_mul(24)
            .and_then(|n| n.checked_add(1024))
            .ok_or_else(refuse)?;
        remaining = remaining.checked_sub(overhead).ok_or_else(refuse)?;
    }
    for (slot, value) in slots.iter().zip(values) {
        let reference = &slot.reference;
        if !slot
            .accepts
            .narrowed_to_live_scope(ctx.live_file_scope.as_ref())
            .allows(value)
        {
            return Err(AttachmentDeliveryError::Unsupported {
                id: reference.id.clone(),
            });
        }
        let unique = seen.insert(Arc::as_ptr(value));
        let cost = match value.as_ref() {
            Delivery::Bytes(bytes) => {
                check_bytes(reference, bytes)?;
                if bytes.capacity() as u64 > policy.max_blob_bytes {
                    return Err(AttachmentDeliveryError::LimitExceeded {
                        max_bytes: policy.max_blob_bytes,
                    });
                }
                reference
                    .byte_len
                    .checked_add(2)
                    .and_then(|n| n.checked_div(3))
                    .and_then(|n| n.checked_mul(16))
                    .and_then(|n| n.checked_add(if unique { bytes.capacity() as u64 } else { 0 }))
                    .ok_or_else(refuse)?
            }
            Delivery::Url {
                url,
                valid_until_ms,
            } => {
                check_horizon(reference, *valid_until_ms, ctx)?;
                (url.len() as u64).checked_mul(24).ok_or_else(refuse)?
            }
            Delivery::ProviderFile {
                id,
                valid_until_ms,
                uploaded,
                ..
            } => {
                check_horizon(reference, *valid_until_ms, ctx)?;
                let scratch = if unique && *uploaded {
                    reference.byte_len
                } else {
                    0
                };
                if scratch > policy.max_blob_bytes {
                    return Err(AttachmentDeliveryError::LimitExceeded {
                        max_bytes: policy.max_blob_bytes,
                    });
                }
                (id.len() as u64)
                    .checked_mul(24)
                    .and_then(|n| n.checked_add(scratch))
                    .ok_or_else(refuse)?
            }
        };
        remaining = remaining.checked_sub(cost).ok_or_else(refuse)?;
    }
    Ok(())
}
fn check_horizon(
    reference: &AttachmentRef,
    expiry: Option<u64>,
    ctx: &DeliveryContext,
) -> Result<(), AttachmentDeliveryError> {
    if expiry.is_some_and(|expiry| expiry < ctx.valid_through_ms) {
        Err(AttachmentDeliveryError::Refused {
            id: reference.id.clone(),
        })
    } else {
        Ok(())
    }
}

/// Deliveries recovered from one canonical attempt. They serve only the
/// admitted references and positions, checking both the admitted acceptance
/// and the forwarding slot's effective acceptance. They preserve each
/// delivered value's expiry and file scope. Secrets have no serialized representation.
#[derive(Clone, Debug)]
pub struct CanonicalDeliveries {
    pub(super) slots: Vec<AttachmentSlot>,
    pub(super) values: Vec<Arc<Delivery>>,
    pub(super) policy: AttachmentReadPolicy,
}
#[async_trait::async_trait]
impl SlotDeliveries for CanonicalDeliveries {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        if slots.len() != self.slots.len() {
            return Err(AttachmentDeliveryError::Unavailable {
                retryable: false,
                message: "canonical slot count differs".into(),
            });
        }
        for ((slot, admitted), delivery) in slots.iter().zip(&self.slots).zip(&self.values) {
            if slot.reference != admitted.reference
                || slot.position != admitted.position
                || !admitted
                    .accepts
                    .narrowed_to_live_scope(ctx.live_file_scope.as_ref())
                    .allows(delivery)
            {
                return Err(AttachmentDeliveryError::Refused {
                    id: slot.reference.id.clone(),
                });
            }
        }
        validate_deliveries(slots, &self.values, ctx, self.policy)?;
        Ok(self.values.clone())
    }
    async fn invalidate(
        &self,
        _: &AttachmentRef,
        _: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        Ok(())
    }
}
