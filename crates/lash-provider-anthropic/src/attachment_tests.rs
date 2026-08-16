use base64::Engine;
use lash_core::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRequest, LlmRole};

use crate::{ANTHROPIC_FILE_MIMES, ANTHROPIC_IMAGE_MIMES, AnthropicProvider};

const ATTACHMENT_FIXTURE_BYTES: &[u8] = b"fig1417-attachment-fixture";

fn request_with_inline_attachment(mime: &str) -> LlmRequest {
    let mut request = LlmRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )],
        attachments: Vec::new(),
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
    request.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse(mime).expect("fixture MIME"),
        ATTACHMENT_FIXTURE_BYTES.to_vec(),
    )];
    request
}

#[test]
fn image_allowlist_serializes_every_mime_as_base64_image_block() {
    let provider = AnthropicProvider::new("key");

    for &mime in ANTHROPIC_IMAGE_MIMES {
        let body = provider
            .build_request_body(&request_with_inline_attachment(mime))
            .expect("allowlisted image MIME must serialize");
        let block = &body["messages"][0]["content"][0];

        assert_eq!(block["type"], "image", "MIME: {mime}");
        assert_eq!(block["source"]["type"], "base64", "MIME: {mime}");
        assert_eq!(block["source"]["media_type"], mime, "MIME: {mime}");
        assert_eq!(
            block["source"]["data"],
            base64::engine::general_purpose::STANDARD.encode(ATTACHMENT_FIXTURE_BYTES),
            "MIME: {mime}"
        );
    }
}

#[test]
fn file_allowlist_serializes_every_mime_as_base64_document_block() {
    let provider = AnthropicProvider::new("key");

    for &mime in ANTHROPIC_FILE_MIMES {
        let body = provider
            .build_request_body(&request_with_inline_attachment(mime))
            .expect("allowlisted file MIME must serialize");
        let block = &body["messages"][0]["content"][0];

        assert_eq!(block["type"], "document", "MIME: {mime}");
        assert_eq!(block["source"]["type"], "base64", "MIME: {mime}");
        assert_eq!(block["source"]["media_type"], mime, "MIME: {mime}");
        assert_eq!(
            block["source"]["data"],
            base64::engine::general_purpose::STANDARD.encode(ATTACHMENT_FIXTURE_BYTES),
            "MIME: {mime}"
        );
    }
}

#[test]
fn unsupported_image_mime_is_rejected_at_request_boundary() {
    let provider = AnthropicProvider::new("key");
    let err = provider
        .build_request_body(&request_with_inline_attachment("image/bmp"))
        .expect_err("bmp should be rejected before wire");

    assert_eq!(err.kind, lash_core::ProviderFailureKind::Validation);
    assert_eq!(
        err.code.as_deref(),
        Some("unsupported_attachment_capability")
    );
    assert!(err.message.contains("Anthropic"));
    assert!(err.message.contains("image/bmp"));
}
