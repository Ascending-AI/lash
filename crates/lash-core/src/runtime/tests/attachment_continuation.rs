use super::*;
use lash_sansio::sync::MutexExt;

const UNSUPPORTED_BYTES: &[u8] = b"native workspace badge binary bytes";

struct AttachmentResultTool {
    media_type: &'static str,
    bytes: &'static [u8],
    label: &'static str,
}

fn attachment_result_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:attachment_result",
        "attachment_result",
        "Return one stored attachment.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for AttachmentResultTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![attachment_result_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "attachment_result")
            .then(|| Arc::new(attachment_result_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        let attachment_ref = call
            .context
            .attachments()
            .put(
                self.bytes.to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse(self.media_type).expect("test MIME"),
                    None,
                    Some(self.label.to_string()),
                ),
            )
            .await
            .expect("store tool attachment");
        crate::ToolOutcome::from_output(crate::ToolCallOutput::success_tool_value(
            crate::ToolValue::Attachment(crate::AttachmentSource::stored(attachment_ref)),
        ))
    }
}

fn attachment_provider(requests: Arc<Mutex<Vec<crate::llm::types::LlmRequest>>>) -> TestProvider {
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let call_index = Arc::clone(&call_index);
            async move {
                let index = call_index.fetch_add(1, Ordering::SeqCst);
                requests.lock_recover().push(request.clone());
                if index == 0 {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "attachment-result-call".to_string(),
                            tool_name: "attachment_result".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    });
                }
                if let Some(source) = request.attachments.iter().find(|source| {
                    crate::llm::transport::known_attachment_acceptors(source).is_empty()
                }) {
                    return Err(crate::llm::transport::unsupported_attachment_capability(
                        "OpenAI Chat Completions",
                        source,
                        &[],
                    ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: format!("completed provider call {index}"),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
}

fn request_text(request: &crate::llm::types::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            crate::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            crate::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                Some(content.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn unsupported_committed_tool_attachment_degrades_and_session_remains_continuable() {
    let trace_path = std::env::temp_dir().join(format!(
        "lash-attachment-degradation-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = attachment_provider(Arc::clone(&requests));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(AttachmentResultTool {
            media_type: "application/octet-stream",
            bytes: UNSUPPORTED_BYTES,
            label: "workspace_badge.bin",
        }),
        provider,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let artifact_turn = runtime
        .run_turn_assembled(
            TurnInput::text("fetch the workspace badge"),
            CancellationToken::new(),
            named_turn_scope("root", "unsupported-attachment-turn"),
        )
        .await
        .expect("artifact turn assembles");
    assert_eq!(artifact_turn.tool_calls.len(), 1);
    assert!(artifact_turn.tool_calls[0].output.is_success());

    let text_turn = runtime
        .run_turn_assembled(
            TurnInput::text("answer this text-only follow-up"),
            CancellationToken::new(),
            named_turn_scope("root", "text-after-unsupported-attachment"),
        )
        .await
        .expect("text-only continuation assembles");
    assert!(
        matches!(artifact_turn.outcome, TurnOutcome::Finished(_))
            && matches!(text_turn.outcome, TurnOutcome::Finished(_)),
        "a successful unmaterializable attachment and its historical replay must remain continuable: artifact_outcome={:?}, artifact_errors={:?}, text_outcome={:?}, text_errors={:?}",
        artifact_turn.outcome,
        artifact_turn
            .errors
            .iter()
            .map(|issue| (&issue.code, &issue.message))
            .collect::<Vec<_>>(),
        text_turn.outcome,
        text_turn
            .errors
            .iter()
            .map(|issue| (&issue.code, &issue.message))
            .collect::<Vec<_>>()
    );

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(
            request.attachments.is_empty(),
            "unmaterializable attachments must be omitted from provider requests"
        );
        let text = request_text(request);
        assert!(text.contains("attachment_unavailable"), "{text}");
        assert!(text.contains("workspace_badge.bin"), "{text}");
        assert!(text.contains("application/octet-stream"), "{text}");
        assert!(
            text.contains("no_provider_accepts_mime_and_source"),
            "{text}"
        );
    }

    let trace = std::fs::read_to_string(&trace_path).expect("read degradation trace");
    let degradation = trace
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace record"))
        .find(|record| record["type"] == "attachment_degraded")
        .expect("typed attachment degradation trace");
    assert_eq!(degradation["media_type"], "application/octet-stream");
    assert_eq!(degradation["label"], "workspace_badge.bin");
    assert_eq!(degradation["reason"], "no_provider_accepts_mime_and_source");

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn accepted_tool_attachment_round_trips_without_degradation() {
    const IMAGE_BYTES: &[u8] = b"accepted-image-bytes";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = attachment_provider(Arc::clone(&requests));
    let mut runtime = runtime_with_plugins_and_tools(
        Vec::new(),
        Arc::new(AttachmentResultTool {
            media_type: "image/png",
            bytes: IMAGE_BYTES,
            label: "accepted.png",
        }),
        provider,
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("fetch the accepted image"),
            CancellationToken::new(),
            named_turn_scope("root", "accepted-attachment-turn"),
        )
        .await
        .expect("accepted attachment turn");
    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let replay = &requests[1];
    assert_eq!(replay.attachments.len(), 1);
    let source = &replay.attachments[0];
    let attachment_ref = source.stored_ref().expect("stored accepted attachment");
    assert_eq!(attachment_ref.media_type.as_str(), "image/png");
    assert_eq!(attachment_ref.label.as_deref(), Some("accepted.png"));
    assert_eq!(replay.attachment_bytes(source), Some(IMAGE_BYTES));
    assert!(!request_text(replay).contains("attachment_unavailable"));
}
