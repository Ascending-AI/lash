// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_sansio::sync::MutexExt;

const UNSUPPORTED_BYTES: &[u8] = b"native workspace badge binary bytes";

struct AttachmentResultTool {
    media_type: &'static str,
    bytes: &'static [u8],
    label: &'static str,
}

fn attachment_result_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:attachment_result",
        "attachment_result",
        "Return one stored attachment.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for AttachmentResultTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![attachment_result_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attachment_result")
            .then(|| Arc::new(attachment_result_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let attachment_ref = call
                .context
                .attachments()
                .put(
                    self.bytes.to_vec(),
                    lash_core::AttachmentCreateMeta::new(
                        lash_core::MediaType::parse(self.media_type).expect("test MIME"),
                        None,
                        Some(self.label.to_string()),
                    ),
                )
                .await
                .expect("store tool attachment");
            lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
                lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(
                    attachment_ref,
                )),
            ))
        })
        .await
        .into()
    }
}

fn attachment_provider(
    requests: Arc<Mutex<Vec<lash_core::llm::types::LlmRequest>>>,
) -> TestProvider {
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
                if let Some(source) = request.attachments().iter().find(|source| {
                    lash_core::llm::transport::known_attachment_acceptors(
                        &request.model_capability.attachment_acceptance,
                        source,
                    )
                    .is_empty()
                }) {
                    return Err(
                        lash_core::llm::transport::unsupported_attachment_capability(
                            "OpenAI Chat Completions",
                            source,
                            &[],
                        ),
                    );
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

fn request_text(request: &lash_core::llm::types::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            lash_core::llm::types::LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            lash_core::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                Some(lash_core::facade_support::tool_result_text(content).into_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn unsupported_committed_tool_attachment_degrades_and_session_remains_continuable() {
    let backend = memory_backend().await;
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
    let mut runtime = TestRuntime::new(&backend, provider)
        .plugins(Vec::new())
        .attachment_acceptance(
            lash_core::attachments::attachment_test_capability().attachment_acceptance,
        )
        .tools(Arc::new(AttachmentResultTool {
            media_type: "application/octet-stream",
            bytes: UNSUPPORTED_BYTES,
            label: "workspace_badge.bin",
        }))
        .host(test_host_config_with_trace_path(
            &backend,
            trace_path.clone(),
        ))
        .build()
        .await;

    let artifact_turn = runtime
        .run_turn_assembled(
            TurnInput::text("fetch the workspace badge"),
            CancellationToken::new(),
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("unsupported-attachment-turn"),
            ),
        )
        .await
        .expect("artifact turn assembles");
    assert_eq!(artifact_turn.tool_calls.len(), 1);
    assert!(artifact_turn.tool_calls[0].output.is_success());

    let text_turn = runtime
        .run_turn_assembled(
            TurnInput::text("answer this text-only follow-up"),
            CancellationToken::new(),
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("text-after-unsupported-attachment"),
            ),
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
            request.attachments().is_empty(),
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
    let degradation = lash_trace::parse_jsonl_records::<serde_json::Value>(&trace)
        .expect("trace records")
        .into_iter()
        .find(|record| record["type"] == "attachment_degraded")
        .expect("typed attachment degradation trace");
    assert_eq!(degradation["media_type"], "application/octet-stream");
    assert_eq!(degradation["label"], "workspace_badge.bin");
    assert_eq!(degradation["reason"], "no_provider_accepts_mime_and_source");

    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn accepted_tool_attachment_round_trips_without_degradation() {
    let backend = memory_backend().await;
    const IMAGE_BYTES: &[u8] = b"accepted-image-bytes";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = attachment_provider(Arc::clone(&requests));
    let mut runtime = TestRuntime::new(&backend, provider)
        .plugins(Vec::new())
        .attachment_acceptance(
            lash_core::attachments::attachment_test_capability().attachment_acceptance,
        )
        .tools(Arc::new(AttachmentResultTool {
            media_type: "image/png",
            bytes: IMAGE_BYTES,
            label: "accepted.png",
        }))
        .build()
        .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("fetch the accepted image"),
            CancellationToken::new(),
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("accepted-attachment-turn"),
            ),
        )
        .await
        .expect("accepted attachment turn");
    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let replay = &requests[1];
    assert_eq!(replay.attachments().len(), 1);
    let source = &replay.attachments()[0];
    let attachment_ref = source.stored_ref().expect("stored accepted attachment");
    assert_eq!(attachment_ref.media_type.as_str(), "image/png");
    assert_eq!(attachment_ref.label.as_deref(), Some("accepted.png"));
    assert_eq!(replay.attachment_bytes(source), Some(IMAGE_BYTES));
    assert!(!request_text(replay).contains("attachment_unavailable"));
}

/// Returns `["before", <stored image>, "after"]`: an array tool value that
/// embeds an attachment between two text fragments.
struct ArrayAttachmentTool;

fn array_attachment_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:array_attachment",
        "array_attachment",
        "Return an array embedding one stored image.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "array" }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ArrayAttachmentTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![array_attachment_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "array_attachment")
            .then(|| Arc::new(array_attachment_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let attachment_ref = call
                .context
                .attachments()
                .put(
                    b"array-image".to_vec(),
                    lash_core::AttachmentCreateMeta::new(
                        lash_core::MediaType::parse("image/png").expect("test MIME"),
                        None,
                        Some("array.png".to_string()),
                    ),
                )
                .await
                .expect("store tool attachment");
            lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
                lash_core::ToolValue::Array(vec![
                    lash_core::ToolValue::String("before".to_string()),
                    lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(
                        attachment_ref,
                    )),
                    lash_core::ToolValue::String("after".to_string()),
                ]),
            ))
        })
        .await
        .into()
    }
}

/// FIG-3515: a tool value embedding an attachment used to split into several
/// results for one call id, which the resume-safety check refuses; every
/// later prepared checkpoint was then skipped, and an Immediate cancel
/// committed the stale draft without the new turn's input or calls. In this
/// harness main commits turn 2's input anyway and fails on the duplicated
/// result, so the test pins the precondition (one resume-safe result per
/// call); `turn_boundary::tests::gates_advance_after_an_attachment_bearing_tool_result`
/// pins the gates themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachment_in_array_tool_value_then_immediate_cancel_loses_nothing() {
    let backend = memory_backend().await;
    const SESSION_ID: &str = "array-attachment-immediate-cancel";
    let (answering_tx, answering_rx) = tokio::sync::oneshot::channel::<()>();
    let answering_tx = Arc::new(Mutex::new(Some(answering_tx)));
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let call_index = Arc::clone(&call_index);
            let answering_tx = Arc::clone(&answering_tx);
            async move {
                let tool_call = |call_id: &str| LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: call_id.to_string(),
                        tool_name: "array_attachment".to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                };
                match call_index.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(tool_call("turn-one-call")),
                    1 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "turn one done".to_string(),
                            response_meta: None,
                        }],
                        ..LlmResponse::default()
                    }),
                    2 => Ok(tool_call("turn-two-call")),
                    _ => {
                        // Turn 2's call has executed; hold the model's answer
                        // open until the host cancels the turn.
                        if let Some(tx) = answering_tx.lock_recover().take() {
                            let _ = tx.send(());
                        }
                        std::future::pending::<Result<LlmResponse, _>>().await
                    }
                }
            }
        })
        .build();
    let store = unbound_recording_store(&backend).await;
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(&backend, provider)
        .plugins(Vec::new())
        .attachment_acceptance(
            lash_core::attachments::attachment_test_capability().attachment_acceptance,
        )
        .tools(Arc::new(ArrayAttachmentTool))
        .host(lash_core::facade_support::EmbeddedRuntimeHost::new(
            test_runtime_host_config(&backend),
        ))
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;

    let turn_one = runtime
        .run_turn_assembled(
            TurnInput::text("return the array"),
            CancellationToken::new(),
            host_admitted_scope(
                &runtime.host.core,
                lash_core::AdmittedScope::unpinned(
                    runtime.export_persistence_state().turn_scope("array-turn"),
                )
                .expect("turn scope"),
            ),
        )
        .await
        .expect("array turn assembles");
    assert!(
        matches!(turn_one.outcome, TurnOutcome::Finished(_)),
        "turn one: {:?} {:?}",
        turn_one.outcome,
        turn_one
            .errors
            .iter()
            .map(|issue| (&issue.code, &issue.message))
            .collect::<Vec<_>>()
    );
    let committed = runtime.read_view().expect("read view").messages().to_vec();
    assert!(
        lash_sansio::messages_are_prompt_resume_safe(&committed),
        "an attachment-bearing tool value commits a resume-safe transcript"
    );

    let turn_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&runtime.host.core.control.effect_host),
        SESSION_ID,
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    );
    let turn_id = "cancelled-turn";
    let persisted_state = runtime.export_persistence_state();
    let turn_scope = host_admitted_scope(
        &runtime.host.core,
        lash_core::AdmittedScope::unpinned(persisted_state.turn_scope(turn_id))
            .expect("turn scope"),
    );
    let turn_address =
        lash_core::facade_support::TurnAddress::new(&persisted_state.session_id, turn_id);
    let turn = lash_core::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("turn two input"),
                CancellationToken::new(),
                turn_scope,
            )
            .await
    });
    answering_rx
        .await
        .expect("the model is answering turn 2's executed call");
    turn_driver
        .request_cancel(
            lash_core::facade_support::TurnCancelRequest::new(
                turn_address,
                "cancel-after-call",
                Some("test-user".to_string()),
            )
            .with_reason("user stopped the turn"),
        )
        .await
        .expect("seal user cancellation");
    let turn_two = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("cancelled turn finishes")
        .expect("cancelled turn task")
        .expect("cancelled turn assembles");
    assert!(
        matches!(
            turn_two.outcome,
            TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence })
                if evidence.mode == lash_core::TurnCancelMode::Immediate
        ),
        "{:?}",
        turn_two.outcome
    );

    let reopened = lash_core::store::load_persisted_session_read_view(store.as_ref())
        .await
        .expect("reopen the cancelled session")
        .expect("durable session");
    let parts: Vec<_> = reopened
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .collect();
    assert!(
        parts
            .iter()
            .any(|part| part.kind() == lash_core::PartKind::Text
                && part.content() == "turn two input"),
        "turn 2's input is committed"
    );
    for kind in [
        lash_core::PartKind::ToolCall,
        lash_core::PartKind::ToolResult,
    ] {
        assert_eq!(
            parts
                .iter()
                .filter(|part| part.kind() == kind && part.tool_call_id() == Some("turn-two-call"))
                .count(),
            1,
            "turn 2's executed call commits exactly one {kind:?}"
        );
    }
    let turn_one_result = parts
        .iter()
        .find(|part| {
            part.kind() == lash_core::PartKind::ToolResult
                && part.tool_call_id() == Some("turn-one-call")
        })
        .expect("turn 1's result is committed");
    let blocks = turn_one_result
        .tool_result_content()
        .expect("tool result blocks");
    assert_eq!(blocks.len(), 3, "{blocks:?}");
    assert!(
        blocks[1].attachment().is_some(),
        "the image keeps its place"
    );
    assert!(lash_sansio::messages_are_prompt_resume_safe(
        reopened.messages()
    ));
}
