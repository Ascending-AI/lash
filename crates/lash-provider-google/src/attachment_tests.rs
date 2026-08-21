use base64::Engine;
use lash_core::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRequest, LlmRole};

use crate::{GOOGLE_FILE_MIMES, GOOGLE_IMAGE_MIMES, GoogleOAuthProvider};

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
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )],
        attachments: vec![attachment.clone()],
        resolved_stored: Default::default(),
        tools: Default::default(),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
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
        .build_contents_with_attachment_parts(&request, std::slice::from_ref(&part));
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
    for &mime in GOOGLE_IMAGE_MIMES {
        assert_inline_data(mime);
    }
}

#[test]
fn file_allowlist_serializes_every_mime_as_inline_data() {
    for &mime in GOOGLE_FILE_MIMES {
        assert_inline_data(mime);
    }
}

#[test]
fn google_rejects_gif_attachment_at_request_boundary() {
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
