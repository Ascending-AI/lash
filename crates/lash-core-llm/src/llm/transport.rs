//! Transport-level failure types and attachment capability diagnostics shared
//! by provider adapters.

pub use lash_http_transport::{
    HttpFailureContext, LlmTransportError, TransportRetryVerdict, retry_after_from_headers,
};
pub use lash_sansio::llm::types::ProviderFailureKind;
pub use lash_sansio::session_model::TurnFailureCode;

use lash_sansio::AttachmentRef;
use lash_sansio::llm::attachment_delivery::AttachmentPosition;

pub fn known_attachment_acceptors<'a>(
    snapshot: &'a crate::provider::AttachmentCapabilitySnapshot,
    reference: &AttachmentRef,
    position: AttachmentPosition,
) -> Vec<&'a str> {
    snapshot.acceptors(&reference.media_type, position)
}
pub fn unsupported_attachment_capability(
    provider: &str,
    reference: &AttachmentRef,
    position: AttachmentPosition,
    accepted_by: &[&str],
) -> LlmTransportError {
    let accepted = if accepted_by.is_empty() {
        "none".to_owned()
    } else {
        accepted_by.join(", ")
    };
    LlmTransportError::new(format!("{provider} cannot encode attachment `{}` MIME `{}` at {position:?}; accepting providers: {accepted}", reference.id, reference.media_type))
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(TurnFailureCode::UnsupportedAttachmentCapability)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}
