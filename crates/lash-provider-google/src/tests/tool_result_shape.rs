//! FIG-3515: one tool call is answered by exactly one function response,
//! with the result's image inside it on Gemini 3 and as ordered user parts
//! after it on dialects without multimodal function responses.
use super::*;
use lash_core::GoogleDialect;
use lash_core::facade_support::ModelToolReturnPart;

const PNG: &[u8] = &[1, 2, 3, 4];

fn contents(dialect: GoogleDialect) -> Vec<Value> {
    let mut req = request(None);
    req.model_capability.google_dialect = dialect;
    req.messages = vec![
        LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::ToolCall {
                call_id: "call_1".into(),
                tool_name: "shot".into(),
                input_json: "{}".into(),
                replay: None,
            }],
        ),
        LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::ToolResult {
                call_id: "call_1".into(),
                tool_name: Some("shot".into()),
                content: vec![
                    ModelToolReturnPart::text("[\"before\","),
                    ModelToolReturnPart::Attachment(AttachmentSource::inline(
                        lash_core::MediaType::parse("image/png").unwrap(),
                        PNG.to_vec(),
                    )),
                    ModelToolReturnPart::text(",\"after\"]"),
                ],
            }],
        ),
    ];
    GoogleOAuthProvider::validate_attachments(&req).expect("png is supported");
    GoogleOAuthProvider::new(
        "access",
        "refresh",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .build_contents_with_attachment_parts(&req, &[])
    .expect("contents")
}

fn inline_png() -> Value {
    json!({"inlineData": {
        "mimeType": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(PNG),
    }})
}

#[test]
fn gemini3_text_image_text_result_is_one_multimodal_function_response() {
    let contents = contents(GoogleDialect::Gemini3);
    assert_eq!(
        contents[1],
        json!({
            "role": "user",
            "parts": [{
                "functionResponse": {
                    "id": "call_1",
                    "name": "shot",
                    "response": {"output": "[\"before\",\n[Attachment 1]\n,\"after\"]"},
                    "parts": [inline_png()],
                }
            }],
        })
    );
}

#[test]
fn legacy_text_image_text_result_is_one_function_response_then_the_image() {
    let contents = contents(GoogleDialect::Legacy);
    assert_eq!(
        contents[1],
        json!({
            "role": "user",
            "parts": [
                {
                    "functionResponse": {
                        "id": "call_1",
                        "name": "shot",
                        "response": {"output": "[\"before\",\n[Attachment 1]\n,\"after\"]"},
                    }
                },
                {"text": "Attachments from tool result call_1:"},
                inline_png(),
            ],
        })
    );
}
