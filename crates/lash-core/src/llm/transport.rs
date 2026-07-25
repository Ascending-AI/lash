//! Transport-level failure types and attachment capability diagnostics shared
//! by provider adapters.

pub use lash_http_transport::retry_after_from_headers;
pub use lash_sansio::llm::types::ProviderFailureKind;
pub type ProviderFailure = lash_http_transport::HttpTransportError;
pub type LlmTransportError = lash_http_transport::HttpTransportError;

use lash_sansio::llm::types::AttachmentSource;

pub const OPENAI_IMAGE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
// OpenAI's current `input_file` table explicitly lists every MIME below:
// https://developers.openai.com/api/docs/guides/file-inputs#full-list-of-accepted-file-types
pub const OPENAI_FILE_MIMES: &[&str] = &[
    "application/pdf",
    "application/json",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-powerpoint",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "text/csv",
    "text/html",
    "text/markdown",
    "text/plain",
];

pub const ANTHROPIC_IMAGE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
pub const ANTHROPIC_FILE_MIMES: &[&str] = &["application/pdf"];

pub const GOOGLE_IMAGE_MIMES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/heic",
    "image/heif",
];
pub const GOOGLE_FILE_MIMES: &[&str] = &["application/pdf"];
pub const GOOGLE_MEDIA_FAMILIES: &[&str] = &["audio", "text", "video"];

/// Derive the cross-provider attachment diagnostic from adapter capability
/// lists instead of maintaining a second MIME table at each call site.
pub fn known_attachment_acceptors(source: &AttachmentSource) -> Vec<&'static str> {
    if let AttachmentSource::ProviderFile { provider_scope, .. } = source {
        return match provider_scope.provider.to_ascii_lowercase().as_str() {
            "openai" => vec!["OpenAI Responses"],
            "anthropic" => vec!["Anthropic Messages"],
            "google" | "google_oauth" | "gemini" => vec!["Google Gemini"],
            _ => Vec::new(),
        };
    }

    let media_type = source.media_type().expect("MIME-bearing source");
    let mime = media_type.as_str();
    let borrowed_url = matches!(source, AttachmentSource::ExternalUrl { .. });
    let mut providers = Vec::new();
    if OPENAI_IMAGE_MIMES.contains(&mime) || OPENAI_FILE_MIMES.contains(&mime) {
        providers.push("OpenAI Responses");
    }
    if OPENAI_IMAGE_MIMES.contains(&mime) {
        providers.push("OpenAI Chat Completions");
    }
    if ANTHROPIC_IMAGE_MIMES.contains(&mime) || ANTHROPIC_FILE_MIMES.contains(&mime) {
        providers.push("Anthropic Messages");
    }
    if !borrowed_url
        && (GOOGLE_IMAGE_MIMES.contains(&mime)
            || GOOGLE_MEDIA_FAMILIES.contains(&media_type.family())
            || GOOGLE_FILE_MIMES.contains(&mime))
    {
        providers.push("Google Gemini");
    }
    providers
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::MediaType;
    use lash_sansio::llm::types::ProviderFileScope;

    fn inline(mime: &str) -> AttachmentSource {
        AttachmentSource::inline(MediaType::parse(mime).unwrap(), vec![1])
    }

    #[test]
    fn attachment_acceptors_are_derived_from_provider_capabilities() {
        assert_eq!(
            known_attachment_acceptors(&inline("image/gif")),
            vec![
                "OpenAI Responses",
                "OpenAI Chat Completions",
                "Anthropic Messages"
            ]
        );
        assert_eq!(
            known_attachment_acceptors(&inline("application/pdf")),
            vec!["OpenAI Responses", "Anthropic Messages", "Google Gemini"]
        );
        assert_eq!(
            known_attachment_acceptors(&inline("audio/mpeg")),
            vec!["Google Gemini"]
        );
    }

    #[test]
    fn openai_file_capabilities_match_the_current_input_file_documentation() {
        assert_eq!(
            OPENAI_FILE_MIMES,
            [
                "application/pdf",
                "application/json",
                "application/msword",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "application/vnd.ms-excel",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "application/vnd.ms-powerpoint",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "text/csv",
                "text/html",
                "text/markdown",
                "text/plain",
            ]
        );
    }

    #[test]
    fn attachment_acceptors_account_for_source_restrictions_and_file_scope() {
        let borrowed_text =
            AttachmentSource::external_url(MediaType::parse("text/plain").unwrap(), "https://x");
        assert_eq!(
            known_attachment_acceptors(&borrowed_text),
            vec!["OpenAI Responses"]
        );

        let google_file = AttachmentSource::provider_file(
            ProviderFileScope::new("gemini", "credential"),
            "file-id",
            None,
        );
        assert_eq!(
            known_attachment_acceptors(&google_file),
            vec!["Google Gemini"]
        );
    }
}
