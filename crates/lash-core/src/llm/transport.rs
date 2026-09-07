//! Transport-level failure types and attachment capability diagnostics shared
//! by provider adapters.

pub use lash_http_transport::{TransportRetryVerdict, retry_after_from_headers};
pub use lash_sansio::llm::types::ProviderFailureKind;
pub type ProviderFailure = lash_http_transport::HttpTransportError;
pub type LlmTransportError = lash_http_transport::HttpTransportError;

use lash_sansio::llm::types::AttachmentSource;

pub fn known_attachment_acceptors<'a>(
    snapshot: &'a crate::provider::AttachmentCapabilitySnapshot,
    source: &AttachmentSource,
) -> Vec<&'a str> {
    snapshot.acceptors(source)
}

pub fn unsupported_attachment_capability(
    provider: &str,
    source: &AttachmentSource,
    accepted_by: &[&str],
) -> LlmTransportError {
    let accepted = if accepted_by.is_empty() {
        "none".to_string()
    } else {
        accepted_by.join(", ")
    };
    let message = match source {
        AttachmentSource::ProviderFile {
            provider_scope,
            media_type,
            ..
        } => match media_type {
            Some(media_type) => format!(
                "{provider} cannot materialize attachment MIME `{media_type}` from source `provider_file` scoped to provider `{}`; providers accepting this source: {accepted}",
                provider_scope.provider
            ),
            None => format!(
                "{provider} cannot materialize attachment source `provider_file` scoped to provider `{}`; the source carries no caller MIME; providers accepting this source: {accepted}",
                provider_scope.provider
            ),
        },
        source => {
            let media_type = source
                .media_type()
                .expect("non-provider-file attachment sources carry a MIME");
            format!(
                "{provider} cannot materialize attachment MIME `{media_type}` from source `{}`; providers accepting this MIME/source: {accepted}",
                source_kind(source)
            )
        }
    };
    ProviderFailure::new(message)
        .with_kind(ProviderFailureKind::Validation)
        .with_code("unsupported_attachment_capability")
}

pub fn source_kind(source: &AttachmentSource) -> &'static str {
    match source {
        AttachmentSource::Inline { .. } => "inline",
        AttachmentSource::Stored { .. } => "stored",
        AttachmentSource::ExternalUrl { .. } => "external_url",
        AttachmentSource::ProviderFile { .. } => "provider_file",
    }
}
