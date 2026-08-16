use super::*;

const FIXTURE_BYTES: &[u8] = b"fig1417-attachment-fixture";

fn request_with_inline_attachment(mime: &str) -> LlmRequest {
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
    )]);
    req.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse(mime).expect("fixture MIME"),
        FIXTURE_BYTES.to_vec(),
    )];
    req
}

fn fixture_data_url(mime: &str) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(FIXTURE_BYTES)
    )
}

#[test]
fn responses_image_allowlist_serializes_every_mime_as_input_image() {
    let provider = OpenAiProvider::new("key");

    for &mime in OPENAI_IMAGE_MIMES {
        let body = provider
            .build_responses_request_body(&request_with_inline_attachment(mime), false)
            .expect("allowlisted image MIME must serialize");
        let part = &body["input"][0]["content"][0];

        assert_eq!(part["type"], "input_image", "MIME: {mime}");
        assert_eq!(part["image_url"], fixture_data_url(mime), "MIME: {mime}");
    }
}

#[test]
fn responses_file_allowlist_serializes_every_mime_as_input_file() {
    let provider = OpenAiProvider::new("key");

    for &mime in OPENAI_FILE_MIMES {
        let body = provider
            .build_responses_request_body(&request_with_inline_attachment(mime), false)
            .expect("allowlisted file MIME must serialize");
        let part = &body["input"][0]["content"][0];

        assert_eq!(part["type"], "input_file", "MIME: {mime}");
        assert_eq!(part["file_data"], fixture_data_url(mime), "MIME: {mime}");
    }
}

#[test]
fn chat_image_allowlist_serializes_every_mime_as_image_url() {
    let provider = openrouter_provider();

    for &mime in OPENAI_IMAGE_MIMES {
        let body = provider
            .build_chat_request_body(&request_with_inline_attachment(mime), false)
            .expect("allowlisted image MIME must serialize");
        let part = &body["messages"][0]["content"][0];

        assert_eq!(part["type"], "image_url", "MIME: {mime}");
        assert_eq!(
            part["image_url"]["url"],
            fixture_data_url(mime),
            "MIME: {mime}"
        );
    }
}

#[test]
fn responses_pdf_url_serializes_as_input_file_url() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
    )]);
    req.attachments = vec![AttachmentSource::external_url(
        lash_core::MediaType::parse("application/pdf").unwrap(),
        "https://example.test/report.pdf",
    )];

    let body = provider.build_responses_request_body(&req, false).unwrap();
    assert_eq!(body["input"][0]["content"][0]["type"], "input_file");
    assert_eq!(
        body["input"][0]["content"][0]["file_url"],
        "https://example.test/report.pdf"
    );
}

#[test]
fn responses_provider_file_ignores_optional_media_type_hint() {
    let provider = OpenAiProvider::new("key");

    for media_type in [
        None,
        Some(lash_core::MediaType::parse("image/png").unwrap()),
    ] {
        let mut req = request(vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )]);
        req.attachments = vec![AttachmentSource::provider_file(
            lash_core::ProviderFileScope::new("openai", "credential"),
            "file-123",
            media_type,
        )];

        let body = provider.build_responses_request_body(&req, false).unwrap();
        assert_eq!(
            body["input"][0]["content"][0],
            json!({"type": "input_file", "file_id": "file-123"})
        );
    }
}
