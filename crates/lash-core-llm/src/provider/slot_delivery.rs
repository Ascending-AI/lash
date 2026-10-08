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
use lash_sansio::llm::types::AttachmentSlot;
use lash_sansio::{AttachmentId, AttachmentRef};

use crate::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_sansio::session_model::TurnFailureCode;

/// What fills the attachment slots of one attempt.
#[async_trait::async_trait]
pub trait SlotDeliveries: Send + Sync {
    /// Deliver every slot of one attempt under one fresh request budget
    /// (`AttachmentReadPolicy`). Returns one delivery per slot, in slot order;
    /// slots with equal `(reference.id, effective accepts)` share one `Arc`
    /// (one backend call, bytes retained once). Effective accepts =
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
