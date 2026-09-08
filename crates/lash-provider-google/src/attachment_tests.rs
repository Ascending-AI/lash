use base64::Engine;
use lash_core::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRequest, LlmRole};

use crate::GoogleOAuthProvider;

const ATTACHMENT_FIXTURE_BYTES: &[u8] = b"fig1417-attachment-fixture";

fn request_with_inline_attachment(mime: &str) -> (LlmRequest, AttachmentSource) {
    let attachment = AttachmentSource::inline(
        lash_core::MediaType::parse(mime).expect("fixture MIME"),
        ATTACHMENT_FIXTURE_BYTES.to_vec(),
    );
    let request = LlmRequest {
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                source: Box::new(attachment.clone()),
            }],
        )],
        resolved_stored: Default::default(),
        tools: Default::default(),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: crate::attachment_test_capability(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    };
    (request, attachment)
}

fn assert_inline_data(mime: &str) {
    let (request, attachment) = request_with_inline_attachment(mime);
    GoogleOAuthProvider::validate_attachments(&request)
        .expect("allowlisted attachment MIME must validate");
    let part = GoogleOAuthProvider::inline_attachment_part(&request, &attachment);
    let contents = GoogleOAuthProvider::for_test()
        .build_contents_with_attachment_parts(&request, &[(attachment, part)]);
    let wire_part = &contents[0]["parts"][0];

    assert_eq!(wire_part["inlineData"]["mimeType"], mime, "MIME: {mime}");
    assert_eq!(
        wire_part["inlineData"]["data"],
        base64::engine::general_purpose::STANDARD.encode(ATTACHMENT_FIXTURE_BYTES),
        "MIME: {mime}"
    );
}

#[test]
fn image_allowlist_serializes_every_mime_as_inline_data() {
    for mime in [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/heic",
        "image/heif",
    ] {
        assert_inline_data(mime);
    }
}

#[test]
fn file_allowlist_serializes_every_mime_as_inline_data() {
    let mime = "application/pdf";
    assert_inline_data(mime);
}

#[test]
fn test_host_rejects_gif_attachment_at_request_boundary() {
    let (request, _) = request_with_inline_attachment("image/gif");
    let err = GoogleOAuthProvider::validate_attachments(&request)
        .expect_err("gif should be rejected for Gemini");

    assert_eq!(err.kind, lash_core::ProviderFailureKind::Validation);
    assert_eq!(
        err.code.as_deref(),
        Some("unsupported_attachment_capability")
    );
    assert!(err.message.contains("Google Gemini"));
    assert!(err.message.contains("image/gif"));
}

#[test]
fn host_declared_url_acceptance_drives_file_data_encoding() {
    let (mut request, _) = request_with_inline_attachment("image/png");
    let source = AttachmentSource::external_url(
        lash_core::MediaType::parse("image/png").unwrap(),
        "https://example.test/host-declared.png",
    );
    request.messages[0].blocks = std::sync::Arc::new(vec![LlmContentBlock::Attachment {
        source: Box::new(source.clone()),
    }]);
    GoogleOAuthProvider::validate_attachments(&request)
        .expect_err("the original test host does not admit URLs");
    let snapshot = std::sync::Arc::make_mut(&mut request.model_capability.attachment_acceptance);
    snapshot.revision = "test-host-url-revision".into();
    snapshot
        .acceptors
        .push(lash_core::provider::AttachmentAcceptor {
            provider: "Google Gemini".into(),
            rules: vec![lash_core::provider::AttachmentAcceptanceRule::Mime {
                source: lash_core::provider::AttachmentMimeSource::ExternalUrl,
                media_types: vec!["image/png".into()],
                media_families: Vec::new(),
            }],
        });
    GoogleOAuthProvider::validate_attachments(&request)
        .expect("host declaration admits the URL source");
    let part = GoogleOAuthProvider::inline_attachment_part(&request, &source);
    assert_eq!(
        part,
        serde_json::json!({"fileData": {"mimeType": "image/png", "fileUri": "https://example.test/host-declared.png"}})
    );
}
