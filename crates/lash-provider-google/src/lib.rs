#[cfg(test)]
mod attachment_tests;
mod config;
#[cfg(all(test, feature = "testing"))]
mod conformance_route;
#[cfg(test)]
mod execution_evidence_tests;
pub mod oauth;
mod provider;
#[cfg(test)]
mod provider_trace_tests;
#[cfg(test)]
mod replay_provenance_tests;
mod request;
mod stream;
mod support;
#[cfg(feature = "testing")]
pub mod testing;
mod upload;

pub use config::{GoogleOAuthProvider, GoogleOAuthProviderFactory};
pub use lash_core::llm::transport::{GOOGLE_FILE_MIMES, GOOGLE_IMAGE_MIMES, GOOGLE_MEDIA_FAMILIES};

#[cfg(test)]
mod tests {
    use lash_sansio::sync::MutexExt;

    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use super::GoogleOAuthProvider;
    use base64::Engine;
    use lash_core::llm::types::{
        AttachmentSource, LlmContentBlock, LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest,
        LlmRole, LlmStreamEvent, LlmTerminalReason, LlmToolChoice, LlmToolSpec, LlmUsage,
        ProviderRouteIdentity, ResponseTextMeta,
    };
    use lash_core::provider::{
        ModelCapability, ProviderOptions, ReasoningCapability, ReasoningEncoding, StreamTermination,
    };
    use lash_core::{Message, MessageRole, Part};
    use serde_json::{Value, json};

    #[derive(Debug)]
    struct StaticSseTransport {
        body: String,
        headers: Vec<(String, String)>,
    }

    impl StaticSseTransport {
        fn new(body: impl Into<String>) -> Self {
            Self {
                body: body.into(),
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            }
        }

        fn with_headers(body: impl Into<String>, headers: Vec<(String, String)>) -> Self {
            Self {
                body: body.into(),
                headers,
            }
        }
    }

    #[async_trait::async_trait]
    impl lash_llm_transport::LlmHttpTransport for StaticSseTransport {
        async fn send(
            &self,
            _request: lash_llm_transport::LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
        {
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: self.headers.clone(),
                body: lash_llm_transport::LlmHttpBody::buffered(self.body.clone()),
            })
        }
    }

    fn request_with_capability(
        model_variant: Option<&str>,
        model_capability: ModelCapability,
    ) -> LlmRequest {
        LlmRequest {
            model: "gemini-3.1-pro-preview".to_string(),
            messages: vec![LlmMessage::text(LlmRole::User, "hello")],
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::<LlmToolSpec>::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: model_variant
                .map(|effort| lash_core::provider::ReasoningSelection::Effort(effort.to_string()))
                .unwrap_or_default(),
            model_capability,
            scope: lash_core::LlmRequestScope::new(
                "session-1",
                "session-1:frame:test",
                "session-1:request:test",
            ),
            output_spec: None,
            stream_events: None::<LlmEventSender>,
            generation: lash_core::GenerationOptions::default(),
            provider_trace: None,
        }
    }

    fn request(model_variant: Option<&str>) -> LlmRequest {
        request_with_capability(model_variant, ModelCapability::default())
    }

    #[tokio::test]
    async fn response_metadata_capture_respects_shared_allowlists() {
        let body = "data: {\"response\":{\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"done\"}]} }],\"billing\":{\"cost\":2},\"private\":\"hidden\"}}\n\n";
        let provider = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_options(ProviderOptions {
                response_metadata_headers: vec!["X-Request-Cost".to_string()],
                response_metadata_body_paths: vec!["/response/billing/cost".to_string()],
                ..ProviderOptions::default()
            })
            .with_transport(Arc::new(StaticSseTransport::with_headers(
                body,
                vec![
                    ("content-type".to_string(), "text/event-stream".to_string()),
                    ("x-request-cost".to_string(), "0.03".to_string()),
                    ("set-cookie".to_string(), "secret".to_string()),
                ],
            )));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let response = provider
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect("metadata fixture completes");

        assert_eq!(
            response.response_metadata["header:x-request-cost"],
            json!("0.03")
        );
        assert_eq!(
            response.response_metadata["body:/response/billing/cost"],
            json!(2)
        );
        assert!(!response.response_metadata.contains_key("header:set-cookie"));
        assert!(
            !response
                .response_metadata
                .values()
                .any(|value| value == "hidden")
        );
        assert!(events.lock_recover().iter().any(|event| {
            matches!(
                event,
                LlmStreamEvent::Evidence(evidence)
                    if evidence.response_metadata.get("header:x-request-cost")
                        == Some(&json!("0.03"))
                        && !evidence.response_metadata.contains_key("header:set-cookie")
            )
        }));
    }

    #[tokio::test]
    async fn google_default_tolerates_eof_but_strict_policy_retains_partial_usage() {
        let body = "data: {\"response\":{\"responseId\":\"google-partial-1\",\"modelVersion\":\"gemini-partial-served\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"legacy\"},{\"functionCall\":{\"id\":\"call-1\",\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":6,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":0}}}\n\n";
        let wire_request = json!({ "model": "gemini-test" });
        let tolerant = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_transport(Arc::new(StaticSseTransport::new(body)));
        let response = tolerant
            .execute_request(
                "access",
                wire_request.clone(),
                Some(LlmEventSender::new(|_| {})),
                None,
                StreamTermination::EofTolerated,
                None,
            )
            .await
            .expect("Google default permits clean EOF");
        assert_eq!(response.full_text, "legacy");

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let strict = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_transport(Arc::new(StaticSseTransport::new(body)));
        let error = strict
            .execute_request(
                "access",
                wire_request,
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect_err("strict Google route requires finishReason");
        assert_eq!(
            error.code.as_deref(),
            Some("stream_ended_before_finish_reason")
        );
        let partial = error.partial_response.as_deref().expect("partial response");
        assert_eq!(partial.full_text, "legacy");
        assert_eq!(partial.usage.input_tokens, 6);
        assert_eq!(partial.usage.output_tokens, 2);
        assert!(partial.provider_usage.is_some());
        let evidence = partial
            .execution_evidence
            .as_ref()
            .expect("Google partial response retains observed provider evidence");
        assert_eq!(
            evidence.provider_response_id.as_deref(),
            Some("google-partial-1")
        );
        assert_eq!(
            evidence.served_model.as_deref(),
            Some("gemini-partial-served")
        );
        assert_eq!(evidence.provider_finish_reason, None);
        assert_eq!(evidence.reasoning_output_tokens, Some(0));
        assert!(
            partial
                .parts
                .iter()
                .any(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
        );
        assert!(
            events
                .lock_recover()
                .iter()
                .any(|event| matches!(event, LlmStreamEvent::Part(LlmOutputPart::ToolCall { .. })))
        );
        assert!(events.lock_recover().iter().any(|event| {
            matches!(
                event,
                LlmStreamEvent::Evidence(evidence)
                    if evidence.provider_usage == partial.provider_usage
            )
        }));
    }

    #[tokio::test]
    async fn mixed_text_and_tool_chunk_preserves_completed_response_order_on_abort() {
        let body = "data: {\"response\":{\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"before tool\"},{\"functionCall\":{\"id\":\"call-1\",\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}]}}]}}\n\n";
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let provider = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_transport(Arc::new(StaticSseTransport::new(body)));
        let completed = provider
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect("mixed text/tool stream completes");
        let stream_events = events.lock_recover().clone();
        let aborted = lash_core::testing::response_synthesized_from_aborted_stream(&stream_events);

        assert_eq!(
            aborted.parts, completed.parts,
            "the abort accumulator must retain the provider's text-before-tool ordering; events were {stream_events:?}"
        );
    }

    #[tokio::test]
    async fn google_strict_policy_accepts_finish_reason() {
        let body = "data: {\"response\":{\"responseId\":\"google-response-1\",\"modelVersion\":\"gemini-3.1-pro-served\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"done\"}]}}],\"usageMetadata\":{\"promptTokenCount\":6,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":0}}}\n\n";
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let provider = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_transport(Arc::new(StaticSseTransport::new(body)));
        let response = provider
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect("finishReason is terminal evidence");
        assert_eq!(response.full_text, "done");
        assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);
        let evidence = response
            .execution_evidence
            .as_ref()
            .expect("Google success carries provider-reported execution evidence");
        assert_eq!(
            evidence.provider_response_id.as_deref(),
            Some("google-response-1")
        );
        assert_eq!(
            evidence.served_model.as_deref(),
            Some("gemini-3.1-pro-served")
        );
        assert_eq!(evidence.provider_finish_reason.as_deref(), Some("STOP"));
        assert_eq!(evidence.reasoning_output_tokens, Some(0));
        assert!(events.lock_recover().iter().any(|event| {
            matches!(
                event,
                LlmStreamEvent::Evidence(stream)
                    if stream.execution_evidence.as_ref() == Some(evidence)
            )
        }));
    }

    const REASONING_SIGNATURE_1: &str = "U0lHLTE=";
    const REASONING_SIGNATURE_2: &str = "U0lHLTI=";

    fn streaming_reasoning_events() -> Vec<Value> {
        vec![
            json!({"response":{"candidates":[{"content":{"parts":[{
                "text": "plan é",
                "thought": true,
                "thoughtSignature": REASONING_SIGNATURE_1
            }]}}]}}),
            json!({"response":{"candidates":[{"content":{"parts":[{
                "text": "carefully",
                "thought": true,
                "thoughtSignature": REASONING_SIGNATURE_2
            }]}}]}}),
            json!({"response":{"candidates":[{
                "content":{"parts":[{"text":"answer"}]},
                "finishReason":"STOP"
            }]}}),
        ]
    }

    fn sse_body(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    fn batch_response_from_stream_events(events: &[Value]) -> Value {
        let mut parts = Vec::new();
        let mut finish_reason = None;
        for event in events {
            let Some(candidate) = event
                .pointer("/response/candidates/0")
                .and_then(Value::as_object)
            else {
                continue;
            };
            if let Some(event_parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
            {
                parts.extend(event_parts.iter().cloned());
            }
            if let Some(reason) = candidate.get("finishReason") {
                finish_reason = Some(reason.clone());
            }
        }
        let mut candidate = json!({"content": {"parts": parts}});
        if let Some(reason) = finish_reason {
            candidate["finishReason"] = reason;
        }
        json!({"candidates": [candidate]})
    }

    fn next_request_from_response_parts(parts: &[LlmOutputPart]) -> Value {
        let assistant_id = "google-regression.assistant";
        let mut durable_parts = Vec::new();
        for part in parts {
            match part {
                LlmOutputPart::Text {
                    text,
                    response_meta,
                } => durable_parts.push(Part::prose(
                    format!("{assistant_id}.p{}", durable_parts.len()),
                    text.clone(),
                    response_meta.clone(),
                )),
                LlmOutputPart::Reasoning { text, replay } => durable_parts.push(Part::reasoning(
                    format!("{assistant_id}.p{}", durable_parts.len()),
                    text.clone(),
                    replay.clone(),
                )),
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => durable_parts.push(Part::tool_call(
                    format!("{assistant_id}.p{}", durable_parts.len()),
                    input_json.clone(),
                    call_id.clone(),
                    tool_name.clone(),
                    replay.clone(),
                )),
            }
        }
        let history = vec![
            Message {
                id: assistant_id.to_string(),
                role: MessageRole::Assistant,
                parts: Arc::new(durable_parts),
                origin: None,
            },
            Message {
                id: "google-regression.user".to_string(),
                role: MessageRole::User,
                parts: Arc::new(vec![Part::text(
                    "google-regression.user.p0".to_string(),
                    "continue".to_string(),
                    None,
                )]),
                origin: None,
            },
        ];
        let durable_json = serde_json::to_string(&history).expect("history serializes");
        let durable_history: Vec<Message> =
            serde_json::from_str(&durable_json).expect("history deserializes");
        let mut req = request(None);
        req.model = "gemini-test".to_string();
        req.messages = lash_core::session_model::render_prompt(&durable_history).messages;
        let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
        GoogleOAuthProvider::build_request(
            &GoogleOAuthProvider::new("access", "refresh", 0),
            &req,
            contents,
            None,
        )
    }

    async fn streaming_reasoning_response(
        wire_events: &[Value],
        expose_thinking: bool,
    ) -> (lash_core::llm::types::LlmResponse, Vec<LlmStreamEvent>) {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let provider = GoogleOAuthProvider::new("access", "refresh", 0)
            .with_options(ProviderOptions {
                expose_thinking,
                ..ProviderOptions::default()
            })
            .with_transport(Arc::new(StaticSseTransport::new(sse_body(wire_events))));
        let response = provider
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                Some(LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                })),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect("streaming reasoning response");
        let events = events.lock_recover().clone();
        (response, events)
    }

    #[tokio::test]
    async fn google_streaming_reasoning_preserves_signature_and_gates_deltas() {
        let wire_events = streaming_reasoning_events();
        let (exposed, exposed_events) = streaming_reasoning_response(&wire_events, true).await;
        assert_eq!(exposed.full_text, "answer");
        let exposed_deltas = exposed_events
            .iter()
            .filter_map(|event| match event {
                LlmStreamEvent::ReasoningDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(exposed_deltas, ["plan é", "carefully"]);
        let reasoning = exposed
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Reasoning { text, replay } => Some((text, replay)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning.len(), 2);
        assert_eq!(reasoning[0].0, "plan é");
        assert_eq!(
            reasoning[0]
                .1
                .as_ref()
                .and_then(|meta| meta.signature.as_deref()),
            Some(REASONING_SIGNATURE_1)
        );
        assert_eq!(
            reasoning[0]
                .1
                .as_ref()
                .and_then(|meta| meta.origin.as_ref()),
            Some(&GoogleOAuthProvider::route_identity_for_model(
                "gemini-test"
            ))
        );
        assert_eq!(reasoning[1].0, "carefully");
        assert_eq!(
            reasoning[1]
                .1
                .as_ref()
                .and_then(|meta| meta.signature.as_deref()),
            Some(REASONING_SIGNATURE_2)
        );
        let replayed = next_request_from_response_parts(&exposed.parts);
        assert_eq!(
            replayed.pointer("/request/contents/0/parts/0/thoughtSignature"),
            Some(&json!(REASONING_SIGNATURE_1))
        );
        assert_eq!(
            replayed.pointer("/request/contents/0/parts/1/thoughtSignature"),
            Some(&json!(REASONING_SIGNATURE_2))
        );

        let (hidden, hidden_events) = streaming_reasoning_response(&wire_events, false).await;
        assert_eq!(hidden.parts, exposed.parts);
        assert!(!hidden.full_text.contains("plan é"));
        assert!(!hidden.full_text.contains("carefully"));
        assert!(
            hidden_events
                .iter()
                .all(|event| !matches!(event, LlmStreamEvent::ReasoningDelta(_)))
        );
        for events in [&exposed_events, &hidden_events] {
            assert!(events.iter().all(|event| {
                !matches!(
                    event,
                    LlmStreamEvent::Delta(text)
                        if text.contains("plan é") || text.contains("carefully")
                )
            }));
        }
    }

    #[tokio::test]
    async fn google_streaming_reasoning_matches_non_streaming_parts() {
        let wire_events = streaming_reasoning_events();
        let (streaming, _) = streaming_reasoning_response(&wire_events, true).await;
        let batch_value = batch_response_from_stream_events(&wire_events);
        let non_streaming =
            GoogleOAuthProvider::response_parts_from_value(&batch_value, Some("gemini-test"));
        let non_streaming_terminal =
            GoogleOAuthProvider::terminal_reason_from_value(&batch_value, &non_streaming);

        assert_eq!(streaming.parts, non_streaming);
        assert_eq!(streaming.terminal_reason, non_streaming_terminal);
    }

    #[tokio::test]
    async fn google_streaming_signed_then_unsigned_reasoning_retains_signature() {
        let wire_events = vec![
            json!({"response":{"candidates":[{"content":{"parts":[{
                "text":"signed unsigned",
                "thought":true,
                "thoughtSignature":REASONING_SIGNATURE_1
            }]}}]}}),
            // A shorter cumulative snapshot is not new text and must not erase
            // the signature already attached to this reasoning run.
            json!({"response":{"candidates":[{"content":{"parts":[{
                "text":"signed",
                "thought":true
            }]}}]}}),
            json!({"response":{"candidates":[{
                "content":{"parts":[{"text":" tail","thought":true}]},
                "finishReason":"STOP"
            }]}}),
        ];
        let (response, _) = streaming_reasoning_response(&wire_events, true).await;
        assert!(matches!(
            response.parts.as_slice(),
            [LlmOutputPart::Reasoning {
                text,
                replay: Some(replay),
            }] if text == "signed unsigned tail"
                && replay.signature.as_deref() == Some(REASONING_SIGNATURE_1)
        ));
    }

    #[tokio::test]
    async fn google_streaming_same_event_thought_parts_stay_distinct_and_precede_text() {
        let wire_events = vec![json!({"response":{"candidates":[{
            "content":{"parts":[
                {"text":"first", "thought":true, "thoughtSignature":REASONING_SIGNATURE_1},
                {"text":"second", "thought":true, "thoughtSignature":REASONING_SIGNATURE_2},
                {"text":"answer"}
            ]},
            "finishReason":"STOP"
        }]}})];
        let (response, events) = streaming_reasoning_response(&wire_events, true).await;
        let reasoning = response
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
            .collect::<Vec<_>>();
        assert_eq!(reasoning.len(), 2);
        let visible = events
            .iter()
            .filter_map(|event| match event {
                LlmStreamEvent::ReasoningDelta(text) => Some(("reasoning", text.as_str())),
                LlmStreamEvent::Delta(text) => Some(("text", text.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            [
                ("reasoning", "first"),
                ("reasoning", "second"),
                ("text", "answer")
            ]
        );
    }

    #[tokio::test]
    async fn google_streaming_reasoning_does_not_merge_across_an_intervening_tool_call() {
        let wire_events = vec![
            json!({"response":{"candidates":[{"content":{"parts":[{
                "text":"before",
                "thought":true,
                "thoughtSignature":REASONING_SIGNATURE_1
            }]}}]}}),
            json!({"response":{"candidates":[{"content":{"parts":[{
                "functionCall":{"id":"call-1","name":"lookup","args":{"q":"x"}}
            }]}}]}}),
            json!({"response":{"candidates":[{
                "content":{"parts":[{
                    "text":"after",
                    "thought":true,
                    "thoughtSignature":REASONING_SIGNATURE_2
                }]},
                "finishReason":"STOP"
            }]}}),
        ];
        let (response, _) = streaming_reasoning_response(&wire_events, true).await;
        let signatures = response
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Reasoning {
                    replay: Some(replay),
                    ..
                } => replay.signature.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(signatures, [REASONING_SIGNATURE_1, REASONING_SIGNATURE_2]);
    }

    fn effort_capability(efforts: &[&str]) -> ModelCapability {
        ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: efforts.iter().copied().map(str::to_string).collect(),
                default_effort: None,
                aliases: Default::default(),
                encoding: ReasoningEncoding::Effort,
                disable: None,
                mandatory: false,
            }),
            cache_control: None,
            stream_termination: None,
            sampling: lash_core::SamplingCapability::Configurable,
        }
    }

    fn budget_capability(entries: &[(&str, u32)]) -> ModelCapability {
        ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: entries
                    .iter()
                    .map(|(effort, _)| (*effort).to_string())
                    .collect(),
                default_effort: None,
                aliases: Default::default(),
                encoding: ReasoningEncoding::Budget(
                    entries
                        .iter()
                        .map(|(effort, tokens)| ((*effort).to_string(), *tokens))
                        .collect(),
                ),
                disable: Some(lash_core::provider::ReasoningDisableEncoding::Budget(0)),
                mandatory: false,
            }),
            cache_control: None,
            stream_termination: None,
            sampling: lash_core::SamplingCapability::Configurable,
        }
    }

    #[test]
    fn usage_payload_maps_canonical_token_buckets() {
        let usage = GoogleOAuthProvider::usage_from_event(&json!({
            "response": {
                "usageMetadata": {
                    "promptTokenCount": 21,
                    "cachedContentTokenCount": 5,
                    "candidatesTokenCount": 10,
                    "thoughtsTokenCount": 3
                }
            }
        }));

        assert_eq!(
            usage,
            LlmUsage {
                input_tokens: 16,
                output_tokens: 13,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 3,
            }
        );
    }

    #[test]
    fn google_image_attachment_serializes_as_inline_data_part() {
        let png_bytes = vec![0x89, 0x50, 0x4E, 0x47];
        let attachment = AttachmentSource::inline(
            lash_core::MediaType::parse("image/png").unwrap(),
            png_bytes.clone(),
        );
        let req = request(None);

        let part = GoogleOAuthProvider::inline_attachment_part(&req, &attachment);

        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        assert_eq!(part["inlineData"]["mimeType"], "image/png");
        assert_eq!(part["inlineData"]["data"], expected_b64);
    }

    #[test]
    fn google_audio_attachment_serializes_as_inline_data_part() {
        let bytes = vec![0x49, 0x44, 0x33];
        let attachment = AttachmentSource::inline(
            lash_core::MediaType::parse("audio/mpeg").unwrap(),
            bytes.clone(),
        );
        let mut req = request(None);
        req.attachments = vec![attachment.clone()];

        GoogleOAuthProvider::validate_attachments(&req).expect("audio is supported");
        let part = GoogleOAuthProvider::inline_attachment_part(&req, &attachment);

        assert_eq!(part["inlineData"]["mimeType"], "audio/mpeg");
        assert_eq!(
            part["inlineData"]["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
    }

    #[test]
    fn google_provider_file_ignores_optional_media_type_hint() {
        for media_type in [
            None,
            Some(lash_core::MediaType::parse("image/png").unwrap()),
        ] {
            let attachment = AttachmentSource::provider_file(
                lash_core::ProviderFileScope::new("google_oauth", "credential"),
                "files/123",
                media_type,
            );
            let mut req = request(None);
            req.attachments = vec![attachment.clone()];

            GoogleOAuthProvider::validate_attachments(&req).expect("provider file is supported");
            assert_eq!(
                GoogleOAuthProvider::inline_attachment_part(&req, &attachment),
                json!({"fileData": {"fileUri": "files/123"}})
            );
        }
    }

    #[test]
    fn google_accepts_webp_attachment_through_validation() {
        let mut req = request(None);
        req.attachments = vec![AttachmentSource::inline(
            lash_core::MediaType::parse("image/webp").unwrap(),
            vec![0],
        )];

        GoogleOAuthProvider::validate_attachments(&req).expect("webp is supported");
    }

    #[test]
    fn google_unknown_finish_reason_maps_to_provider_error() {
        let terminal_reason = GoogleOAuthProvider::terminal_reason_from_value(
            &json!({"candidates":[{"finishReason":"NEW_REASON"}]}),
            &[],
        );

        assert_eq!(terminal_reason, LlmTerminalReason::ProviderError);
    }

    #[test]
    fn google_image_safety_finish_reason_maps_to_content_filter() {
        let terminal_reason = GoogleOAuthProvider::terminal_reason_from_value(
            &json!({"candidates":[{"finishReason":"IMAGE_SAFETY"}]}),
            &[],
        );

        assert_eq!(terminal_reason, LlmTerminalReason::ContentFilter);
    }

    #[test]
    fn streaming_captures_finish_reason_instead_of_hardcoding_stop() {
        // Regression: the streaming finalizer used to hardcode terminal_reason
        // = Stop, mislabeling MAX_TOKENS / tool-call / safety turns. Drive the
        // SSE events through process_sse_event and confirm the captured
        // finishReason maps through terminal_reason_from_value (here MAX_TOKENS
        // -> OutputLimit), exactly like the non-streaming path.
        let mut full = String::new();
        let mut usage = LlmUsage::default();
        let mut tool_calls: Vec<LlmOutputPart> = Vec::new();
        let mut finish_event: Option<serde_json::Value> = None;
        for raw in [
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
            r#"{"candidates":[{"finishReason":"MAX_TOKENS"}]}"#,
        ] {
            GoogleOAuthProvider::process_sse_event(
                raw,
                &mut full,
                &mut Vec::new(),
                &mut usage,
                Some(&mut tool_calls),
                &mut finish_event,
            )
            .expect("sse event");
        }
        assert!(
            finish_event.is_some(),
            "finishReason event must be captured"
        );
        let terminal_reason = GoogleOAuthProvider::terminal_reason_from_value(
            finish_event.as_ref().unwrap_or(&serde_json::Value::Null),
            &[],
        );
        assert_eq!(terminal_reason, LlmTerminalReason::OutputLimit);
    }

    #[test]
    fn streaming_captures_raw_usage_metadata_sidecar() {
        let mut full = String::new();
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage: Option<Value> = None;
        let mut execution_evidence = None;
        let mut finish_event: Option<Value> = None;
        let meta = json!({"promptTokenCount": 6, "candidatesTokenCount": 4});
        for raw in [
            json!({"response":{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}}).to_string(),
            json!({"response":{"usageMetadata": meta}}).to_string(),
            // A trailing empty usage block must not clobber the captured raw
            // sidecar, mirroring the normalized-usage non-zero guard.
            json!({"response":{"usageMetadata": {}}}).to_string(),
        ] {
            GoogleOAuthProvider::process_sse_event_with_text_parts(
                &raw,
                crate::support::SseTextPartSink {
                    full: &mut full,
                    text_deltas: &mut text_deltas,
                    reasoning_deltas: &mut reasoning_deltas,
                    usage: &mut usage,
                    provider_usage: &mut provider_usage,
                    execution_evidence: &mut execution_evidence,
                    tool_call_parts: None,
                    output_parts: None,
                    finish_event: &mut finish_event,
                },
                None,
            )
            .expect("sse event");
        }
        assert_eq!(provider_usage, Some(meta));
        assert_eq!(usage.input_tokens, 6);
        assert_eq!(usage.output_tokens, 4);
    }

    #[test]
    fn streaming_populates_reasoning_deltas_without_an_output_part_sink() {
        let mut full = String::new();
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage = None;
        let mut execution_evidence = None;
        let mut finish_event = None;
        GoogleOAuthProvider::process_sse_event_with_text_parts(
            &json!({"response":{"candidates":[{"content":{"parts":[{
                "text":"thought",
                "thought":true
            }]}}]}})
            .to_string(),
            crate::support::SseTextPartSink {
                full: &mut full,
                text_deltas: &mut text_deltas,
                reasoning_deltas: &mut reasoning_deltas,
                usage: &mut usage,
                provider_usage: &mut provider_usage,
                execution_evidence: &mut execution_evidence,
                tool_call_parts: None,
                output_parts: None,
                finish_event: &mut finish_event,
            },
            None,
        )
        .expect("reasoning event parses");

        assert_eq!(reasoning_deltas, ["thought"]);
    }

    #[test]
    fn thinking_config_uses_effort_encoding_for_thinking_level() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let body = GoogleOAuthProvider::build_request(
            &provider,
            &request_with_capability(
                Some("medium"),
                effort_capability(&["low", "medium", "high"]),
            ),
            Vec::new(),
            None,
        );

        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert!(
            body["request"]["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none()
        );
    }

    #[test]
    fn thinking_config_uses_budget_encoding_for_variant_budget() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let body = GoogleOAuthProvider::build_request(
            &provider,
            &request_with_capability(
                Some("high"),
                budget_capability(&[("high", 16_000), ("max", 24_576)]),
            ),
            Vec::new(),
            None,
        );

        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            16_000
        );
        assert!(
            body["request"]["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none()
        );
    }

    #[test]
    fn disabled_budget_model_emits_zero_thinking_budget() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let mut req = request_with_capability(
            None,
            budget_capability(&[("high", 16_000), ("max", 24_576)]),
        );
        req.model_variant = lash_core::provider::ReasoningSelection::Disabled;

        let body = GoogleOAuthProvider::build_request(&provider, &req, Vec::new(), None);

        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 0 })
        );
    }

    #[test]
    fn thinking_config_is_omitted_without_capability() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let body = GoogleOAuthProvider::build_request(
            &provider,
            &request_with_capability(Some("medium"), ModelCapability::default()),
            Vec::new(),
            None,
        );

        assert!(
            body["request"]["generationConfig"]
                .get("thinkingConfig")
                .is_none()
        );
    }

    #[test]
    fn thinking_config_omits_thoughts_unless_provider_exposes_thinking() {
        let hidden_provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let hidden = GoogleOAuthProvider::build_request(
            &hidden_provider,
            &request_with_capability(
                Some("medium"),
                effort_capability(&["low", "medium", "high"]),
            ),
            Vec::new(),
            None,
        );
        assert_eq!(
            hidden["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert!(
            hidden["request"]["generationConfig"]["thinkingConfig"]
                .get("includeThoughts")
                .is_none()
        );

        let exposed_provider =
            GoogleOAuthProvider::new("access", "refresh", 0).with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            });
        let exposed = GoogleOAuthProvider::build_request(
            &exposed_provider,
            &request_with_capability(
                Some("medium"),
                effort_capability(&["low", "medium", "high"]),
            ),
            Vec::new(),
            None,
        );
        assert_eq!(
            exposed["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn output_token_cap_maps_to_max_output_tokens() {
        let provider =
            GoogleOAuthProvider::new("access", "refresh", 0).with_options(ProviderOptions {
                max_output_tokens: Some(9999),
                ..ProviderOptions::default()
            });

        let mut req = request(None);
        req.generation.output_token_cap = NonZeroUsize::new(4096);
        let body = GoogleOAuthProvider::build_request(&provider, &req, Vec::new(), None);

        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 4096);
        let provider_limited =
            GoogleOAuthProvider::build_request(&provider, &request(None), Vec::new(), None);
        assert_eq!(
            provider_limited["request"]["generationConfig"]["maxOutputTokens"],
            9999
        );
    }

    #[test]
    fn stop_sequences_reach_the_generation_config() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let mut req = request(None);
        req.generation.stop_sequences = vec!["</lashlang>".to_string()];

        let body = GoogleOAuthProvider::build_request(&provider, &req, Vec::new(), None);

        assert_eq!(
            body["request"]["generationConfig"]["stopSequences"],
            json!(["</lashlang>"])
        );
        assert_eq!(
            GoogleOAuthProvider::generation_disposition(&req).stop_sequences,
            lash_core::GenerationOptionDisposition::Applied
        );
    }

    #[test]
    fn caller_sampling_controls_reach_the_generation_config() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);

        let defaulted = GoogleOAuthProvider::build_request(&provider, &request(None), vec![], None);
        assert_eq!(defaulted["request"]["generationConfig"]["temperature"], 0);
        // A seed is emitted only when one was asked for.
        assert!(
            defaulted["request"]["generationConfig"]
                .get("seed")
                .is_none()
        );

        let mut req = request(None);
        req.generation.temperature =
            Some(lash_core::NonNegativeFiniteF64::new(0.8).expect("finite temperature"));
        req.generation.seed = Some(11);
        let body = GoogleOAuthProvider::build_request(&provider, &req, vec![], None);
        assert_eq!(body["request"]["generationConfig"]["temperature"], 0.8);
        assert_eq!(body["request"]["generationConfig"]["seed"], 11);
    }

    #[test]
    fn cache_breakpoint_is_reported_as_dropped() {
        let mut req = request(None);
        req.messages = vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Text {
                text: "stable history".into(),
                response_meta: None,
                cache_breakpoint: true,
            }],
        )];

        let disposition = GoogleOAuthProvider::generation_disposition(&req);
        assert_eq!(
            disposition.cache,
            lash_core::GenerationOptionDisposition::OmittedUnsupported
        );
        assert!(!disposition.nothing_omitted());
    }

    #[test]
    fn google_text_thought_signature_is_stored_and_replayed_for_same_origin() {
        let signature = base64::engine::general_purpose::STANDARD.encode("sig");
        let value = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "text": "hello",
                        "thoughtSignature": signature
                    }]
                }
            }]
        });
        let parts =
            GoogleOAuthProvider::response_parts_from_value(&value, Some("gemini-3.1-pro-preview"));
        let meta = match &parts[0] {
            LlmOutputPart::Text {
                response_meta: Some(meta),
                ..
            } => meta,
            other => panic!("expected text metadata, got {other:?}"),
        };
        assert_eq!(meta.provider_payload.as_deref(), Some(signature.as_str()));
        assert_eq!(
            meta.origin.as_ref(),
            Some(&GoogleOAuthProvider::route_identity_for_model(
                "gemini-3.1-pro-preview"
            ))
        );

        let mut req = request(None);
        req.messages = vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::Text {
                text: "hello".into(),
                response_meta: Some(meta.clone()),
                cache_breakpoint: false,
            }],
        )];
        let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
        assert_eq!(contents[0]["parts"][0]["thoughtSignature"], signature);
    }

    #[test]
    fn google_function_call_thought_signature_round_trips_from_streaming_and_batch() {
        let signature = "ZnVuY3Rpb24tY2FsbC1zaWduYXR1cmU=";
        let function_part = json!({
            "functionCall": {
                "id": "call-1",
                "name": "lookup",
                "args": {"q": "x"}
            },
            "thoughtSignature": signature
        });
        let streaming_event = json!({"response":{"candidates":[{
            "content":{"parts":[function_part.clone()]},
            "finishReason":"STOP"
        }]}});
        let mut full = String::new();
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage = None;
        let mut execution_evidence = None;
        let mut output_parts = Vec::new();
        let mut streaming_parts = Vec::new();
        let mut finish_event = None;
        GoogleOAuthProvider::process_sse_event_with_text_parts(
            &streaming_event.to_string(),
            crate::support::SseTextPartSink {
                full: &mut full,
                text_deltas: &mut text_deltas,
                reasoning_deltas: &mut reasoning_deltas,
                usage: &mut usage,
                provider_usage: &mut provider_usage,
                execution_evidence: &mut execution_evidence,
                tool_call_parts: Some(&mut streaming_parts),
                output_parts: Some(&mut output_parts),
                finish_event: &mut finish_event,
            },
            Some("gemini-test"),
        )
        .expect("streaming function call parses");
        let batch_parts = GoogleOAuthProvider::response_parts_from_value(
            &json!({"candidates":[{
                "content":{"parts":[function_part]},
                "finishReason":"STOP"
            }]}),
            Some("gemini-test"),
        );

        for (path, parts) in [("streaming", streaming_parts), ("batch", batch_parts)] {
            assert!(
                matches!(
                    parts.as_slice(),
                    [LlmOutputPart::ToolCall {
                        replay: Some(replay),
                        ..
                    }] if replay.opaque.as_deref() == Some(signature)
                        && replay.origin.as_ref()
                            == Some(&GoogleOAuthProvider::route_identity_for_model("gemini-test"))
                ),
                "{path} parser must retain the functionCall thoughtSignature"
            );
            let request = next_request_from_response_parts(&parts);
            assert_eq!(
                request.pointer("/request/contents/0/parts/0/thoughtSignature"),
                Some(&json!(signature)),
                "{path} replay projection must restore the functionCall thoughtSignature"
            );
        }
    }

    #[test]
    fn google_text_thought_signature_replay_rejects_invalid_or_cross_origin_metadata() {
        let valid = base64::engine::general_purpose::STANDARD.encode("sig");
        for meta in [
            ResponseTextMeta {
                provider_payload: Some("not base64!".to_string()),
                origin: Some(GoogleOAuthProvider::route_identity_for_model(
                    "gemini-3.1-pro-preview",
                )),
                ..ResponseTextMeta::default()
            },
            ResponseTextMeta {
                provider_payload: Some(valid.clone()),
                origin: Some(ProviderRouteIdentity::new(
                    "other_provider",
                    "other-route",
                    "gemini-3.1-pro-preview",
                )),
                ..ResponseTextMeta::default()
            },
            ResponseTextMeta {
                provider_payload: Some(valid.clone()),
                origin: Some(GoogleOAuthProvider::route_identity_for_model(
                    "gemini-2.5-pro",
                )),
                ..ResponseTextMeta::default()
            },
        ] {
            let mut req = request(None);
            req.messages = vec![LlmMessage::new(
                LlmRole::Assistant,
                vec![LlmContentBlock::Text {
                    text: "hello".into(),
                    response_meta: Some(meta),
                    cache_breakpoint: false,
                }],
            )];
            let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
            assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
        }
    }

    #[test]
    fn google_claude_on_vertex_tool_parameters_strip_json_schema_meta_declarations() {
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let mut claude_on_vertex = request(None);
        claude_on_vertex.model = "claude-sonnet-4-6".to_string();
        claude_on_vertex.tools = Arc::new(vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$id": "tool.schema.json",
                "$defs": { "unused": { "type": "string" } },
                "definitions": { "old": { "type": "string" } },
                "type": "object",
                "properties": {
                    "nested": {
                        "$id": "nested",
                        "$defs": { "x": { "type": "string" } },
                        "type": "object"
                    }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        }]);
        let claude_on_vertex_body =
            GoogleOAuthProvider::build_request(&provider, &claude_on_vertex, Vec::new(), None);
        let parameters =
            &claude_on_vertex_body["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(parameters.get("$schema").is_none());
        assert!(parameters.get("$id").is_none());
        assert!(parameters.get("$defs").is_none());
        assert!(parameters.get("definitions").is_none());
        assert!(parameters["properties"]["nested"].get("$id").is_none());
        assert!(
            claude_on_vertex_body["request"]["tools"][0]["functionDeclarations"][0]
                .get("parametersJsonSchema")
                .is_none()
        );

        let mut gemini = claude_on_vertex;
        gemini.model = "gemini-3.1-pro-preview".to_string();
        let gemini_body = GoogleOAuthProvider::build_request(&provider, &gemini, Vec::new(), None);
        assert!(
            gemini_body["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]
                .get("$schema")
                .is_some()
        );
    }

    /// The cross-provider conformance law below is feature-gated. This crate's
    /// self dev-dependency keeps `testing` on for every test build, so a bare
    /// `cargo test -p lash-provider-google` runs the law. If that wiring is
    /// ever dropped, this sentinel makes the bare run fail loudly instead of
    /// reporting green over a law it never compiled.
    #[cfg(not(feature = "testing"))]
    #[test]
    fn conformance_law_must_not_be_compiled_out() {
        panic!(
            "the cross-provider conformance law was compiled out: this crate's `testing` feature \
             is off for the test build, which the self dev-dependency in Cargo.toml is supposed \
             to guarantee"
        );
    }

    /// Cross-provider response-normalization conformance. Wraps this crate's
    /// (private) Gemini parsers in a `ProviderNormalizer`. Gemini materializes
    /// non-streaming function calls, but it does not expose the streaming
    /// chunk-merge scenarios in the same shape as SSE-first providers.
    #[cfg(feature = "testing")]
    mod conformance {
        use super::*;
        use lash_llm_transport::conformance::{
            CanonicalUsage as U, ProviderConformanceSpec, ProviderNormalizer, ProviderWire,
            Scenario, StreamAssembly, provider_conformance, strong_replay_payload,
        };

        struct GoogleNormalizer;

        impl ProviderNormalizer for GoogleNormalizer {
            fn name(&self) -> &str {
                "google-gemini"
            }

            fn conformance_spec(&self) -> ProviderConformanceSpec {
                ProviderConformanceSpec::with_unsupported(&[
                    (
                        Scenario::StreamingToolArgumentMerge,
                        "Gemini streams complete functionCall objects, not argument deltas",
                    ),
                    (
                        Scenario::StreamingUsageMerge,
                        "Gemini usage events replace aggregate usage instead of incremental SSE deltas",
                    ),
                ])
            }

            fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
                let wire = match scenario {
                    Scenario::PlainTextStop => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [{ "text": "hello" }] },
                            "finishReason": "STOP"
                        }],
                        "usageMetadata": {
                            "promptTokenCount": U::BASE_INPUT,
                            "candidatesTokenCount": U::BASE_OUTPUT
                        }
                    })),
                    Scenario::OutputCapped => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [{ "text": "trunc" }] },
                            "finishReason": "MAX_TOKENS"
                        }]
                    })),
                    Scenario::ContentFilter => ProviderWire::body(json!({
                        "candidates": [{ "content": { "parts": [] }, "finishReason": "SAFETY" }]
                    })),
                    Scenario::NonStreamingToolUse => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [{
                                "functionCall": {
                                    "id": "call_1",
                                    "name": "lookup",
                                    "args": { "q": "x" }
                                }
                            }] }
                        }]
                    })),
                    Scenario::StreamingTextAssembly => {
                        ProviderWire::body(json!({})).with_text_stream(
                            vec![
                                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"hello "}]}}]}}"#.to_string(),
                                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"world"}]},"finishReason":"STOP"}]}}"#.to_string(),
                            ],
                            "hello world",
                        )
                    }
                    Scenario::StreamingToolArgumentMerge => return None,
                    Scenario::StreamingToolCallAbortEquivalence => {
                        ProviderWire::body(json!({})).with_aborted_tool_call_stream(
                            vec![json!({
                                "response": { "candidates": [{
                                    "content": { "parts": [{
                                        "functionCall": {
                                            "id": "call_abort",
                                            "name": "lookup",
                                            "args": { "q": "x" }
                                        }
                                    }] }
                                }] }
                            })
                            .to_string()],
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::UsageCacheHit => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [{ "text": "ok" }] },
                            "finishReason": "STOP"
                        }],
                        "usageMetadata": {
                            "promptTokenCount": U::BASE_INPUT,
                            "candidatesTokenCount": U::BASE_OUTPUT,
                            "cachedContentTokenCount": U::CACHED_INPUT
                        }
                    })),
                    Scenario::UsageReasoning => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [{ "text": "ok" }] },
                            "finishReason": "STOP"
                        }],
                        "usageMetadata": {
                            "promptTokenCount": U::BASE_INPUT,
                            "candidatesTokenCount": U::BASE_OUTPUT,
                            "thoughtsTokenCount": U::REASONING
                        }
                    })),
                    Scenario::ReasoningExtraction => ProviderWire::body(json!({
                        "candidates": [{
                            "content": { "parts": [
                                { "text": "thinking about it", "thought": true },
                                { "text": "answer" }
                            ] },
                            "finishReason": "STOP"
                        }]
                    }))
                    .with_reasoning_text("thinking about it"),
                    Scenario::ReasoningReplayRoundTrip => {
                        let signature = strong_replay_payload("google-gemini");
                        ProviderWire::body(json!({})).with_reasoning_replay_round_trip(
                            vec![
                                json!({
                                    "response": { "candidates": [{
                                        "content": { "parts": [{
                                            "text": "thinking ",
                                            "thought": true
                                        }] }
                                    }] }
                                })
                                .to_string(),
                                json!({
                                    "response": { "candidates": [{
                                        "content": { "parts": [{
                                            "text": "carefully",
                                            "thought": true,
                                            "thoughtSignature": signature
                                        }] },
                                        "finishReason": "STOP"
                                    }] }
                                })
                                .to_string(),
                            ],
                            signature,
                            "/request/contents/0/parts/0/thoughtSignature",
                        )
                    }
                    Scenario::StreamingUsageMerge => return None,
                };
                Some(wire)
            }

            fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
                GoogleOAuthProvider::response_parts_from_value(body, None)
            }

            fn usage_from_wire(&self, body: &Value) -> LlmUsage {
                // `usage_from_event` reads `event.response.usageMetadata`; the
                // unwrapped body carries `usageMetadata` at the top level, so
                // re-wrap it exactly as the non-streaming path does.
                let meta = body.get("usageMetadata").cloned().unwrap_or(Value::Null);
                GoogleOAuthProvider::usage_from_event(&json!({
                    "response": { "usageMetadata": meta }
                }))
            }

            fn terminal_from_wire(
                &self,
                body: &Value,
                parts: &[LlmOutputPart],
            ) -> LlmTerminalReason {
                GoogleOAuthProvider::terminal_reason_from_value(body, parts)
            }

            fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
                let mut full = String::new();
                let mut text_deltas = Vec::new();
                let mut reasoning_deltas = Vec::new();
                let mut usage = LlmUsage::default();
                let mut provider_usage = None;
                let mut execution_evidence = None;
                let mut output_parts = Vec::new();
                let mut tool_calls = Vec::new();
                let mut finish_event = None;
                let stream_events = Arc::new(std::sync::Mutex::new(Vec::new()));
                let event_sink = Arc::clone(&stream_events);
                let sender = LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                });
                for raw in sse_events {
                    let first_new_tool_call = tool_calls.len();
                    GoogleOAuthProvider::process_sse_event_with_text_parts(
                        raw,
                        crate::support::SseTextPartSink {
                            full: &mut full,
                            text_deltas: &mut text_deltas,
                            reasoning_deltas: &mut reasoning_deltas,
                            usage: &mut usage,
                            provider_usage: &mut provider_usage,
                            execution_evidence: &mut execution_evidence,
                            tool_call_parts: Some(&mut tool_calls),
                            output_parts: Some(&mut output_parts),
                            finish_event: &mut finish_event,
                        },
                        None,
                    )
                    .expect("google sse event parses");
                    for part in &tool_calls[first_new_tool_call..] {
                        sender.send(LlmStreamEvent::Part(part.clone()));
                    }
                }
                let mut parts = output_parts;
                parts.extend(tool_calls);
                if matches!(scenario, Scenario::ReasoningReplayRoundTrip) {
                    crate::conformance_route::stamp_google_replay_origin(&mut parts);
                }
                StreamAssembly {
                    parts,
                    usage,
                    stream_events: stream_events.lock_recover().clone(),
                }
            }

            fn build_next_request(&self, messages: Vec<LlmMessage>) -> Value {
                let mut req = request(None);
                req.messages = messages;
                let provider = GoogleOAuthProvider::new("access", "refresh", 0);
                let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
                GoogleOAuthProvider::build_request(&provider, &req, contents, None)
            }
        }

        #[test]
        fn google_satisfies_provider_conformance() {
            provider_conformance(&GoogleNormalizer);
        }
    }
}
