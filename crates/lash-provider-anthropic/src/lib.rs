#[cfg(test)]
mod attachment_tests;
mod config;
mod policy;
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

pub use config::{AnthropicProvider, AnthropicProviderFactory, DEFAULT_BASE_URL};
pub use lash_core::llm::transport::{ANTHROPIC_FILE_MIMES, ANTHROPIC_IMAGE_MIMES};

#[cfg(test)]
mod tests {
    use lash_sansio::sync::MutexExt;

    use crate::stream::StreamState;
    use crate::{AnthropicProvider, DEFAULT_BASE_URL};
    use lash_core::llm::types::{
        AttachmentSource, LlmContentBlock, LlmEventSender, LlmJsonSchema, LlmMessage,
        LlmOutputPart, LlmOutputSpec, LlmRequest, LlmRole, LlmStreamEvent, LlmTerminalReason,
        LlmToolChoice, LlmToolSpec, LlmUsage, NonNegativeFiniteF64, ProviderRouteIdentity,
    };
    use lash_core::provider::{
        CacheRetention, ModelCapability, Provider, ProviderOptions, ReasoningCapability,
        ReasoningEncoding, StreamTermination,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    #[derive(Debug)]
    struct StaticSseTransport(&'static str);

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
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: lash_llm_transport::LlmHttpBody::buffered(self.0),
            })
        }
    }

    #[derive(Debug)]
    struct MetadataSseTransport(&'static str);

    #[async_trait::async_trait]
    impl lash_llm_transport::LlmHttpTransport for MetadataSseTransport {
        async fn send(
            &self,
            _request: lash_llm_transport::LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
        {
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: vec![
                    ("content-type".to_string(), "text/event-stream".to_string()),
                    ("x-request-cost".to_string(), "0.02".to_string()),
                    ("set-cookie".to_string(), "secret".to_string()),
                ],
                body: lash_llm_transport::LlmHttpBody::buffered(self.0),
            })
        }
    }

    // Capability data mirrors what the host catalog supplies. Effort encoding
    // sends the resolved variant verbatim (adaptive thinking); budget encoding
    // maps each variant to a token budget and omits the wire thinking block for
    // any variant absent from the map (e.g. "none").
    fn effort_capability(efforts: &[&str]) -> ModelCapability {
        ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: efforts.iter().map(|e| e.to_string()).collect(),
                default_effort: None,
                aliases: BTreeMap::new(),
                encoding: ReasoningEncoding::Effort,
                disable: Some(lash_core::provider::ReasoningDisableEncoding::Native),
                mandatory: false,
            }),
            cache_control: None,
            stream_termination: None,
            sampling: lash_core::SamplingCapability::Configurable,
        }
    }

    fn budget_capability() -> ModelCapability {
        let budgets = BTreeMap::from([
            ("low".to_string(), 1_024u32),
            ("medium".to_string(), 4_096u32),
            ("high".to_string(), 12_288u32),
        ]);
        ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: ["low", "medium", "high"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                default_effort: None,
                aliases: BTreeMap::new(),
                encoding: ReasoningEncoding::Budget(budgets),
                disable: Some(lash_core::provider::ReasoningDisableEncoding::Omit),
                mandatory: false,
            }),
            cache_control: None,
            stream_termination: None,
            sampling: lash_core::SamplingCapability::Configurable,
        }
    }

    // `DEFAULT_BASE_URL` is part of the crate's public surface and exercised by
    // downstream hosts; reference it here so the re-export stays covered.
    const _: &str = DEFAULT_BASE_URL;

    fn request(messages: Vec<LlmMessage>) -> LlmRequest {
        LlmRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages,
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::<LlmToolSpec>::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: Default::default(),
            model_capability: ModelCapability::default(),
            scope: lash_core::LlmRequestScope::new(
                "session-1",
                "session-1:frame:test",
                "session-1:request:test",
            ),
            output_spec: None,
            stream_events: None,
            generation: lash_core::GenerationOptions::default(),
            provider_trace: None,
        }
    }

    #[test]
    fn foreign_google_reasoning_signature_is_not_forwarded_to_anthropic() {
        let provider = AnthropicProvider::new("key");
        let req = request(vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::Reasoning {
                text: "neutral summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("google-thought-signature".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        "google_oauth",
                        "https://cloudcode-pa.googleapis.com/v1internal",
                        "gemini-2.5-pro",
                    )),
                    ..Default::default()
                }),
            }],
        )]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "text", "text": "neutral summary"})
        );
    }

    #[test]
    fn raw_anthropic_builder_drops_unstamped_reasoning_replay() {
        let provider = AnthropicProvider::new("key");
        let req = request(vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::Reasoning {
                text: "portable summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("unstamped-signature".to_string()),
                    ..Default::default()
                }),
            }],
        )]);

        let body = provider.build_request_body(&req).expect("body");
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "text", "text": "portable summary"})
        );
        assert!(!body.to_string().contains("unstamped-signature"));
    }

    #[test]
    fn same_route_reasoning_replay_is_forwarded_to_anthropic() {
        let provider = AnthropicProvider::new("key");
        let req = request(vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::Reasoning {
                text: "native summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("native-anthropic-signature".to_string()),
                    origin: Some(provider.route_identity("claude-sonnet-4-6")),
                    ..Default::default()
                }),
            }],
        )]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "thinking",
                "thinking": "native summary",
                "signature": "native-anthropic-signature"
            })
        );
    }

    #[tokio::test]
    async fn streamed_reasoning_parts_are_stamped_at_the_anthropic_boundary() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"summary\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"native-signature\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.stream_events = Some(LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        }));
        let mut provider =
            AnthropicProvider::new("key").with_transport(Arc::new(StaticSseTransport(body)));
        let expected_route = provider.route_identity("claude-sonnet-4-6");

        let response = provider
            .complete(req)
            .await
            .expect("thinking stream completes");
        let replay = match &response.parts[0] {
            LlmOutputPart::Reasoning {
                replay: Some(replay),
                ..
            } => replay,
            other => panic!("expected reasoning replay, got {other:?}"),
        };
        assert_eq!(replay.origin.as_ref(), Some(&expected_route));
        assert!(events.lock_recover().iter().any(|event| {
            matches!(
                event,
                LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                    replay: Some(replay),
                    ..
                }) if replay.origin.as_ref() == Some(&expected_route)
            )
        }));
    }

    #[tokio::test]
    async fn response_metadata_capture_respects_shared_allowlists() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}},\"billing\":{\"cost\":1}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\",\"billing\":{\"cost\":2},\"private\":\"hidden\"}\n\n",
        );
        let mut provider = AnthropicProvider::new("key")
            .with_options(ProviderOptions {
                response_metadata_headers: vec!["X-Request-Cost".to_string()],
                response_metadata_body_paths: vec!["/billing/cost".to_string()],
                ..ProviderOptions::default()
            })
            .with_transport(Arc::new(MetadataSseTransport(body)));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.stream_events = Some(LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        }));

        let response = provider
            .complete(req)
            .await
            .expect("metadata fixture completes");

        assert_eq!(
            response.response_metadata["header:x-request-cost"],
            json!("0.02")
        );
        assert_eq!(response.response_metadata["body:/billing/cost"], json!(2));
        assert_eq!(
            response
                .execution_evidence
                .as_ref()
                .and_then(|evidence| evidence.provider_finish_reason.as_deref()),
            Some("end_turn")
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
                        == Some(&json!("0.02"))
                        && !evidence.response_metadata.contains_key("header:set-cookie")
            )
        }));
    }

    #[tokio::test]
    async fn anthropic_requires_message_stop_and_retains_partial_usage() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_partial_1\",\"model\":\"claude-sonnet-4-6-served\",\"usage\":{\"input_tokens\":8,\"cache_read_input_tokens\":2,\"output_tokens_details\":{\"thinking_tokens\":0}}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n"
        );
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        req.stream_events = Some(LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        }));
        let mut provider =
            AnthropicProvider::new("key").with_transport(Arc::new(StaticSseTransport(body)));

        let error = provider
            .complete(req)
            .await
            .expect_err("message_stop is required");

        assert_eq!(error.kind, lash_core::ProviderFailureKind::Stream);
        assert_eq!(
            error.code.as_deref(),
            Some("stream_ended_before_message_stop")
        );
        let partial = error.partial_response.as_deref().expect("partial response");
        assert_eq!(partial.full_text, "partial");
        assert_eq!(partial.usage.input_tokens, 8);
        assert_eq!(partial.usage.output_tokens, 3);
        assert!(partial.provider_usage.is_some());
        let evidence = partial
            .execution_evidence
            .as_ref()
            .expect("Anthropic partial response retains observed provider evidence");
        assert_eq!(
            evidence.provider_response_id.as_deref(),
            Some("msg_partial_1")
        );
        assert_eq!(
            evidence.served_model.as_deref(),
            Some("claude-sonnet-4-6-served")
        );
        assert_eq!(evidence.provider_finish_reason.as_deref(), Some("end_turn"));
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
    }

    #[tokio::test]
    async fn anthropic_stream_evidence_is_monotonic_and_rejects_identity_drift() {
        let monotonic_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stable\",\"model\":\"claude-stable\",\"usage\":{\"output_tokens_details\":{\"thinking_tokens\":7}}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens_details\":{\"thinking_tokens\":0}}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut monotonic = AnthropicProvider::new("key")
            .with_transport(Arc::new(StaticSseTransport(monotonic_body)));
        let response = monotonic
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("a cumulative trailing zero must not erase a positive count");
        assert_eq!(
            response
                .execution_evidence
                .expect("stream evidence")
                .reasoning_output_tokens,
            Some(7)
        );

        let drifting_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_first\",\"model\":\"claude-first\",\"usage\":{}}}\n\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_second\",\"model\":\"claude-second\",\"usage\":{}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut drifting = AnthropicProvider::new("key")
            .with_transport(Arc::new(StaticSseTransport(drifting_body)));
        let error = drifting
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect_err("one stream cannot change provider response identity");
        assert!(
            error.message.contains("served_model")
                || error.message.contains("provider_response_id"),
            "identity-drift error must name the conflicting field: {error:?}"
        );
    }

    #[tokio::test]
    async fn anthropic_mid_arguments_eof_keeps_partial_without_emitting_tool_call() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":8}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n"
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.stream_events = Some(LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        }));
        let mut provider =
            AnthropicProvider::new("key").with_transport(Arc::new(StaticSseTransport(body)));

        let error = provider
            .complete(req)
            .await
            .expect_err("EOF mid-arguments must fail without message_stop");
        let partial = error.partial_response.as_deref().expect("partial response");
        let input_json = partial.parts.iter().find_map(|part| match part {
            LlmOutputPart::ToolCall { input_json, .. } => Some(input_json.as_str()),
            _ => None,
        });

        assert_eq!(input_json, Some("{\"q\":"));
        assert!(
            events.lock_recover().iter().all(|event| !matches!(
                event,
                LlmStreamEvent::Part(LlmOutputPart::ToolCall { .. })
            ))
        );
    }

    #[test]
    fn duplicate_content_block_stop_is_deduped_on_abort_by_call_id() {
        let mut state = StreamState::default();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let sender = LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        });
        let wire = [
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 1 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "call_abort", "name": "lookup", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"x\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ];
        for event in wire {
            AnthropicProvider::process_sse_event(
                &event.to_string(),
                &mut state,
                Some(&sender),
                true,
            )
            .expect("anthropic SSE event parses");
        }

        let stream_events = events.lock_recover().clone();
        let emitted_tool_calls = stream_events
            .iter()
            .filter(|event| matches!(event, LlmStreamEvent::Part(LlmOutputPart::ToolCall { .. })))
            .count();
        let aborted = lash_core::testing::response_synthesized_from_aborted_stream(&stream_events);
        let aborted_tool_calls = aborted
            .parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count();
        let (finalized, _, _, _) = AnthropicProvider::finalize(state, "claude-test");
        let finalized_tool_calls = finalized
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
            .count();

        assert_eq!(
            emitted_tool_calls, 2,
            "fixture must exercise duplicate emission"
        );
        assert_eq!(
            finalized_tool_calls, 1,
            "finalization walks each block once"
        );
        assert_eq!(
            aborted_tool_calls, 1,
            "emitted_part_tool_calls={emitted_tool_calls} abort_synthesized_tool_calls={aborted_tool_calls} finalized_response_tool_calls={finalized_tool_calls}"
        );
    }

    #[tokio::test]
    async fn anthropic_accepts_message_stop_and_explicit_eof_tolerance() {
        let terminal_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut terminal_req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        terminal_req.stream_events = Some(LlmEventSender::new(|_| {}));
        let mut terminal = AnthropicProvider::new("key")
            .with_transport(Arc::new(StaticSseTransport(terminal_body)));
        assert_eq!(
            terminal
                .complete(terminal_req)
                .await
                .expect("terminal stream")
                .full_text,
            "done"
        );

        let eof_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"legacy\"}}\n\n"
        );
        let mut tolerant_req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        tolerant_req.stream_events = Some(LlmEventSender::new(|_| {}));
        tolerant_req.model_capability.stream_termination = Some(StreamTermination::EofTolerated);
        let mut tolerant =
            AnthropicProvider::new("key").with_transport(Arc::new(StaticSseTransport(eof_body)));
        assert_eq!(
            tolerant
                .complete(tolerant_req)
                .await
                .expect("tolerated EOF")
                .full_text,
            "legacy"
        );
    }

    #[test]
    fn usage_payload_maps_canonical_token_buckets() {
        let usage = AnthropicProvider::parse_usage(&json!({
            "input_tokens": 12,
            "output_tokens": 13,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 4,
            "output_tokens_details": {
                "thinking_tokens": 3
            }
        }));

        assert_eq!(
            usage,
            LlmUsage {
                input_tokens: 12,
                output_tokens: 13,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 4,
                reasoning_output_tokens: 3,
            }
        );
    }

    #[test]
    fn image_attachment_serializes_as_base64_image_block() {
        use base64::Engine;
        let provider = AnthropicProvider::new("key");
        let png_bytes = vec![0x89, 0x50, 0x4E, 0x47];
        let mut req = request(vec![LlmMessage::new(
            LlmRole::User,
            vec![
                LlmContentBlock::Text {
                    text: "look at this".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
                LlmContentBlock::Attachment { attachment_idx: 0 },
            ],
        )]);
        req.attachments = vec![AttachmentSource::inline(
            lash_core::MediaType::parse("image/png").unwrap(),
            png_bytes.clone(),
        )];

        let body = provider.build_request_body(&req).expect("body");

        let messages = body["messages"].as_array().expect("messages array");
        let user_msg = messages.last().expect("user message");
        let content = user_msg["content"].as_array().expect("content array");
        let image_block = content
            .iter()
            .find(|b| b["type"] == "image")
            .expect("image block");
        assert_eq!(image_block["source"]["type"], "base64");
        assert_eq!(image_block["source"]["media_type"], "image/png");
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        assert_eq!(image_block["source"]["data"], expected_b64);
    }

    #[test]
    fn external_pdf_serializes_as_document_url_block() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )]);
        req.attachments = vec![AttachmentSource::external_url(
            lash_core::MediaType::parse("application/pdf").unwrap(),
            "https://example.test/report.pdf",
        )];

        let body = provider.build_request_body(&req).expect("body");
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "document");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://example.test/report.pdf");
    }

    #[test]
    fn provider_file_uses_media_type_to_select_image_or_document_block() {
        let provider = AnthropicProvider::new("key");

        for (mime, expected_block_type, file_id) in [
            ("image/png", "image", "file-image"),
            ("application/pdf", "document", "file-document"),
        ] {
            let mut req = request(vec![LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
            )]);
            req.attachments = vec![AttachmentSource::provider_file(
                lash_core::ProviderFileScope::new("anthropic", "credential"),
                file_id,
                Some(lash_core::MediaType::parse(mime).unwrap()),
            )];

            let body = provider.build_request_body(&req).expect("body");
            let block = &body["messages"][0]["content"][0];
            assert_eq!(block["type"], expected_block_type);
            assert_eq!(block["source"], json!({"type": "file", "file_id": file_id}));
        }
    }

    #[test]
    fn provider_file_without_media_type_is_rejected_before_transport() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )]);
        req.attachments = vec![AttachmentSource::provider_file(
            lash_core::ProviderFileScope::new("anthropic", "credential"),
            "file-without-mime",
            None,
        )];

        let err = provider
            .build_request_body(&req)
            .expect_err("missing MIME should be rejected before transport");

        assert_eq!(err.kind, lash_core::ProviderFailureKind::Validation);
        assert_eq!(
            err.code.as_deref(),
            Some("provider_file_media_type_required")
        );
        assert_eq!(
            err.message,
            "Anthropic Messages requires the media type for provider file ids in order to choose the image/document modality; supply `media_type` on `ProviderFile`"
        );
    }

    #[test]
    fn structured_output_uses_native_output_config_format() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![
            LlmMessage::text(LlmRole::System, "system prompt"),
            LlmMessage::text(LlmRole::User, "extract"),
        ]);
        req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
            name: "extract_result".to_string(),
            strict: true,
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["answer"],
                "properties": {
                    "answer": { "type": "string" }
                }
            })
            .into(),
        }));

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(
            body["output_config"]["format"],
            json!({
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["answer"],
                    "properties": {
                        "answer": { "type": "string" }
                    }
                }
            })
        );
        let system_text = body["system"][0]["text"].as_str().unwrap_or_default();
        assert_eq!(system_text, "system prompt");
        assert!(!system_text.contains("Respond with a single JSON object"));
    }

    #[test]
    fn structured_output_preserves_adaptive_effort_config() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "extract")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        req.model_capability = effort_capability(&["low", "medium", "high"]);
        req.output_spec = Some(LlmOutputSpec::JsonObject);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["output_config"]["effort"], json!("medium"));
        assert_eq!(
            body["output_config"]["format"],
            json!({
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "additionalProperties": true,
                }
            })
        );
    }

    #[test]
    fn structured_output_strips_bedrock_unsafe_array_constraints() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "rank")]);
        req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
            name: "rank_result".to_string(),
            strict: true,
            schema: json!({
                "type": "object",
                "required": ["ranked"],
                "properties": {
                    "ranked": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 2,
                        "items": { "type": "string" }
                    }
                }
            })
            .into(),
        }));

        let body = provider.build_request_body(&req).expect("body");
        let ranked = &body["output_config"]["format"]["schema"]["properties"]["ranked"];

        assert!(ranked.get("minItems").is_none());
        assert!(ranked.get("maxItems").is_none());
        assert!(
            ranked["description"]
                .as_str()
                .is_some_and(|description| description.contains("maxItems=2"))
        );
    }

    #[test]
    fn tool_input_schema_uses_anthropic_projection() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "rank")]);
        req.tools = Arc::new(vec![LlmToolSpec {
            name: "rank".to_string(),
            description: "Rank".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": { "type": "string" }
                    }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        }]);

        let body = provider.build_request_body(&req).expect("body");
        let ids = &body["tools"][0]["input_schema"]["properties"]["ids"];

        assert!(ids.get("minItems").is_none());
        assert!(ids.get("maxItems").is_none());
    }

    #[test]
    fn pause_turn_maps_to_stop() {
        let state = StreamState {
            stop_reason: Some("pause_turn".to_string()),
            ..StreamState::default()
        };

        let (_, _, _, terminal_reason) = AnthropicProvider::finalize(state, "claude-test");

        assert_eq!(terminal_reason, LlmTerminalReason::Stop);
    }

    #[test]
    fn unknown_stop_reason_maps_to_provider_error() {
        let state = StreamState {
            stop_reason: Some("new_provider_reason".to_string()),
            ..StreamState::default()
        };

        let (_, _, _, terminal_reason) = AnthropicProvider::finalize(state, "claude-test");

        assert_eq!(terminal_reason, LlmTerminalReason::ProviderError);
    }

    #[test]
    fn stream_merges_raw_usage_sidecar_across_message_start_and_delta() {
        let mut state = StreamState::default();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&events);
        let sender = LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        });
        for event in [
            json!({
                "type": "message_start",
                "message": {"usage": {
                    "input_tokens": 25,
                    "cache_read_input_tokens": 5,
                    "cache_creation_input_tokens": 2,
                    "output_tokens": 1
                }}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 40}
            }),
        ] {
            AnthropicProvider::process_sse_event(
                &event.to_string(),
                &mut state,
                Some(&sender),
                true,
            )
            .expect("sse event");
        }

        // The raw sidecar overlays `message_delta`'s cumulative output count
        // onto `message_start`'s input/cache buckets instead of last-wins.
        assert_eq!(
            state.provider_usage,
            Some(json!({
                "input_tokens": 25,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 2,
                "output_tokens": 40
            }))
        );
        assert_eq!(state.usage.input_tokens, 25);
        assert_eq!(state.usage.output_tokens, 40);
        assert!(events.lock_recover().iter().any(|event| {
            matches!(
                event,
                LlmStreamEvent::Evidence(evidence)
                    if evidence.provider_usage == state.provider_usage
                        && evidence
                            .execution_evidence
                            .as_ref()
                            .and_then(|evidence| evidence.provider_finish_reason.as_deref())
                            == Some("end_turn")
            )
        }));
    }

    #[test]
    fn thinking_display_is_omitted_unless_provider_exposes_thinking() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "extract")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        req.model_capability = effort_capability(&["low", "medium", "high"]);

        let hidden = AnthropicProvider::new("key")
            .build_request_body(&req)
            .expect("body");
        assert_eq!(hidden["thinking"]["display"], "omitted");

        let exposed = AnthropicProvider::new("key")
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .build_request_body(&req)
            .expect("body");
        assert_eq!(exposed["thinking"]["display"], "summarized");
    }

    #[test]
    fn request_body_omits_temperature_without_explicit_temperature_option() {
        let provider = AnthropicProvider::new("key");
        let plain = provider
            .build_request_body(&request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .expect("plain body");
        assert!(plain.get("temperature").is_none());

        let mut thinking_req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        thinking_req.model_variant =
            lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        thinking_req.model_capability = effort_capability(&["low", "medium", "high"]);
        let thinking = provider
            .build_request_body(&thinking_req)
            .expect("thinking body");
        assert!(thinking.get("temperature").is_none());
    }

    #[test]
    fn requested_temperature_is_emitted_unless_thinking_pins_sampling() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.generation.temperature =
            Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature"));
        req.generation.seed = Some(7);

        let plain = provider.build_request_body(&req).expect("plain body");
        assert_eq!(plain["temperature"], json!(0.25));
        // Anthropic Messages has no seed field, so a requested seed is simply
        // not expressible on this wire.
        assert!(plain.get("seed").is_none());

        let mut thinking_req = req.clone();
        thinking_req.model_variant =
            lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        thinking_req.model_capability = effort_capability(&["low", "medium", "high"]);
        let thinking = provider
            .build_request_body(&thinking_req)
            .expect("thinking body");
        assert_eq!(thinking["thinking"]["type"], "adaptive");
        // Extended thinking pins sampling; Anthropic rejects a temperature
        // alongside it.
        assert!(thinking.get("temperature").is_none());
    }

    #[test]
    fn requested_temperature_is_omitted_for_a_model_that_pins_sampling() {
        // Models released after Claude Opus 4.6 answer any caller-set
        // temperature with HTTP 400, thinking or no thinking. The host says so
        // through the capability; the adapter never reads the model name.
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model = "claude-opus-4-7".to_string();
        req.generation.temperature =
            Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature"));
        req.model_capability.sampling = lash_core::SamplingCapability::Pinned;

        let body = provider.build_request_body(&req).expect("body");
        assert!(
            body.get("thinking").is_none(),
            "no thinking on this request"
        );
        assert!(body.get("temperature").is_none());

        // Omission stays silent so one session-wide temperature keeps working
        // across mixed models, but it is reported rather than invisible: the
        // host can see that its request was not honored.
        req.generation.seed = Some(11);
        let pinned_disposition = AnthropicProvider::generation_disposition(&req, &body);
        assert_eq!(
            pinned_disposition.temperature,
            lash_core::llm::types::GenerationOptionDisposition::OmittedSamplingPinned
        );
        assert_eq!(
            pinned_disposition.seed,
            lash_core::llm::types::GenerationOptionDisposition::OmittedUnsupported,
            "Anthropic Messages has no seed field"
        );
        assert!(!pinned_disposition.nothing_omitted());

        // The same request against a model that allows it still emits.
        req.model_capability.sampling = lash_core::SamplingCapability::Configurable;
        let configurable = provider.build_request_body(&req).expect("body");
        assert_eq!(configurable["temperature"], json!(0.25));
        assert_eq!(
            AnthropicProvider::generation_disposition(&req, &configurable).temperature,
            lash_core::llm::types::GenerationOptionDisposition::Applied
        );
    }

    #[test]
    fn effort_capability_emits_adaptive_thinking_with_verbatim_variant() {
        // Effort encoding + a resolved variant produces the adaptive wire shape
        // and sends the variant as the `output_config.effort` verbatim — the
        // provider does not clamp against the model name (opus-4.7 exposes
        // `xhigh`).
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model = "claude-opus-4-7".to_string();
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("xhigh".to_string());
        req.model_capability = effort_capability(&["low", "medium", "high", "xhigh"]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], json!("xhigh"));
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn budget_capability_maps_variant_to_budget_tokens() {
        // Budget encoding resolves the variant to its token budget and emits the
        // `enabled` thinking block; no `output_config.effort` is set.
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model = "claude-haiku-4".to_string();
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        req.model_capability = budget_capability();

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], json!(4_096));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn budget_capability_disabled_selection_uses_explicit_omit_encoding() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model = "claude-haiku-4".to_string();
        req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
        req.model_capability = budget_capability();

        let body = provider.build_request_body(&req).expect("body");

        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn adaptive_capability_disabled_selection_emits_native_disabled_thinking() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
        req.model_capability = effort_capability(&["low", "medium", "high"]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn no_reasoning_capability_emits_no_thinking() {
        // A variant with no reasoning capability never produces a thinking block.
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());

        let body = provider.build_request_body(&req).expect("body");

        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn no_variant_emits_no_thinking_even_with_capability() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        req.model_capability = effort_capability(&["low", "medium", "high"]);

        let body = provider.build_request_body(&req).expect("body");

        assert!(body.get("thinking").is_none());
    }

    // Header-capturing transport: records the outbound `anthropic-beta` header
    // and answers with a minimal end_turn stream so `complete` succeeds. Used to
    // assert the interleaved-thinking beta gates on the emitted thinking shape.
    #[derive(Debug)]
    struct HeaderCaptureTransport {
        beta: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl lash_llm_transport::LlmHttpTransport for HeaderCaptureTransport {
        async fn send(
            &self,
            request: lash_llm_transport::LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::llm::transport::LlmTransportError>
        {
            let beta = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, value)| value.clone());
            *self.beta.lock_recover() = beta;
            let stream = [
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ]
            .concat();
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: lash_llm_transport::LlmHttpBody::buffered(stream.into_bytes()),
            })
        }
    }

    fn captured_beta_for(req: LlmRequest) -> String {
        let beta = Arc::new(std::sync::Mutex::new(None));
        let mut provider =
            AnthropicProvider::new("key").with_transport(Arc::new(HeaderCaptureTransport {
                beta: Arc::clone(&beta),
            }));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(provider.complete(req))
            .expect("stream completes");
        let value = beta.lock_recover().clone();
        value.expect("anthropic-beta header sent")
    }

    #[test]
    fn interleaved_thinking_beta_gates_on_emitted_thinking_shape() {
        // Budget/enabled thinking opts into the interleaved beta; adaptive
        // thinking (built-in) and no thinking do not. Gating is on the wire
        // shape, never the model name.
        let mut budget = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        budget.model = "claude-haiku-4".to_string();
        budget.model_variant =
            lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        budget.model_capability = budget_capability();
        assert!(
            captured_beta_for(budget).contains(crate::policy::INTERLEAVED_THINKING_BETA),
            "budget thinking must request the interleaved beta"
        );

        let mut adaptive = request(vec![LlmMessage::text(LlmRole::User, "think")]);
        adaptive.model = "claude-opus-4-7".to_string();
        adaptive.model_variant =
            lash_core::provider::ReasoningSelection::Effort("high".to_string());
        adaptive.model_capability = effort_capability(&["low", "medium", "high", "xhigh"]);
        assert!(
            !captured_beta_for(adaptive).contains(crate::policy::INTERLEAVED_THINKING_BETA),
            "adaptive thinking must not request the interleaved beta"
        );

        let plain = request(vec![LlmMessage::text(LlmRole::User, "hi")]);
        assert!(
            !captured_beta_for(plain).contains(crate::policy::INTERLEAVED_THINKING_BETA),
            "a request without thinking must not request the interleaved beta"
        );
    }

    #[test]
    fn explicit_text_cache_breakpoint_beats_last_user_block() {
        let provider = AnthropicProvider::new("key");
        let req = request(vec![LlmMessage::new(
            LlmRole::User,
            vec![
                LlmContentBlock::Text {
                    text: "stable history".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                },
                LlmContentBlock::Text {
                    text: "dynamic current iteration".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
            ],
        )]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(
            body["messages"][0]["content"][1]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("__lash_cache_breakpoint")
                .is_none()
        );
        assert_eq!(
            AnthropicProvider::generation_disposition(&req, &body).cache,
            lash_core::GenerationOptionDisposition::Applied,
        );
    }

    #[test]
    fn cache_retention_none_removes_cache_control() {
        let provider = AnthropicProvider::new("key").with_options(ProviderOptions {
            cache_retention: CacheRetention::None,
            ..ProviderOptions::default()
        });
        let req = request(vec![
            LlmMessage::text(LlmRole::System, "stable system prompt"),
            LlmMessage::text(LlmRole::User, "dynamic tail"),
        ]);

        let body = provider.build_request_body(&req).expect("body");

        assert!(body["system"][0].get("cache_control").is_none());
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn cache_retention_long_emits_ttl() {
        let provider = AnthropicProvider::new("key").with_options(ProviderOptions {
            cache_retention: CacheRetention::Long,
            ..ProviderOptions::default()
        });
        let req = request(vec![
            LlmMessage::text(LlmRole::System, "stable system prompt"),
            LlmMessage::text(LlmRole::User, "dynamic tail"),
        ]);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
    }

    #[test]
    fn output_token_cap_maps_to_max_tokens() {
        let provider = AnthropicProvider::new("key").with_options(ProviderOptions {
            max_output_tokens: Some(9999),
            ..ProviderOptions::default()
        });
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.generation.output_token_cap = NonZeroUsize::new(2048);

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["max_tokens"], 2048);
        let provider_limited_body = provider
            .build_request_body(&request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .expect("body");
        assert_eq!(provider_limited_body["max_tokens"], 9999);
    }

    #[test]
    fn stop_sequences_reach_the_messages_request() {
        let provider = AnthropicProvider::new("key");
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.generation.stop_sequences = vec!["</lashlang>".to_string()];

        let body = provider.build_request_body(&req).expect("body");

        assert_eq!(body["stop_sequences"], json!(["</lashlang>"]));
        assert_eq!(
            AnthropicProvider::generation_disposition(&req, &body).stop_sequences,
            lash_core::GenerationOptionDisposition::Applied
        );
    }

    /// The cross-provider conformance law below is feature-gated. This crate's
    /// self dev-dependency keeps `testing` on for every test build, so a bare
    /// `cargo test -p lash-provider-anthropic` runs the law. If that wiring is
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

    /// Cross-provider response-normalization conformance. Anthropic is
    /// streaming-first (no non-streaming `parts_from_value`), so each scenario's
    /// `body` carries the SSE event sequence as a JSON array of strings, and all
    /// three accessors replay it through `process_sse_event` + `finalize`.
    #[cfg(feature = "testing")]
    mod conformance {
        use super::*;
        use lash_llm_transport::conformance::{
            CanonicalUsage as U, ProviderConformanceSpec, ProviderNormalizer, ProviderWire,
            ReplayItemExpectation, Scenario, StreamAssembly, provider_conformance,
            strong_replay_payload,
        };
        use serde_json::Value;

        struct AnthropicNormalizer;

        // Replay a `body` that encodes a JSON array of SSE event strings through
        // the streaming parser and finalize into normalized outputs.
        fn replay(body: &Value) -> (Vec<LlmOutputPart>, LlmUsage, LlmTerminalReason) {
            let mut state = StreamState::default();
            if let Some(events) = body.as_array() {
                for event in events {
                    let raw = event.as_str().expect("sse event is a string");
                    AnthropicProvider::process_sse_event(raw, &mut state, None, true)
                        .expect("anthropic sse event parses");
                }
            }
            let (parts, _text, usage, terminal) = AnthropicProvider::finalize(state, "claude-test");
            (parts, usage, terminal)
        }

        // Build the SSE event array for a plain single-text-block message with a
        // given stop_reason and message_start usage block.
        fn text_message(stop_reason: &str, text: &str, usage: Value) -> Value {
            json!([
                json!({ "type": "message_start", "message": { "usage": usage } }).to_string(),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }).to_string(),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }).to_string(),
                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason } }).to_string(),
            ])
        }

        fn tool_use_message() -> Value {
            json!([
                json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT } } }).to_string(),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "call_1", "name": "lookup", "input": {} } }).to_string(),
                // arguments deliberately split across two delta events
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":" } }).to_string(),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"x\"}" } }).to_string(),
                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }).to_string(),
            ])
        }

        impl ProviderNormalizer for AnthropicNormalizer {
            fn name(&self) -> &str {
                "anthropic"
            }

            fn conformance_spec(&self) -> ProviderConformanceSpec {
                ProviderConformanceSpec::with_unsupported(&[(
                    Scenario::ToolCallReplayRoundTrip,
                    "Anthropic tool_use blocks carry no opaque replay state; the dialect has \
                     nowhere to put one",
                )])
            }

            fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
                let wire = match scenario {
                    Scenario::PlainTextStop => ProviderWire::body(text_message(
                        "end_turn",
                        "hello",
                        json!({ "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT }),
                    )),
                    Scenario::OutputCapped => ProviderWire::body(text_message(
                        "max_tokens",
                        "trunc",
                        json!({ "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT }),
                    )),
                    Scenario::ContentFilter => ProviderWire::body(text_message(
                        "refusal",
                        "",
                        json!({ "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT }),
                    )),
                    Scenario::NonStreamingToolUse => ProviderWire::body(tool_use_message()),
                    Scenario::StreamingTextAssembly => {
                        ProviderWire::body(Value::Null).with_text_stream(
                            vec![
                                json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT } } }).to_string(),
                                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }).to_string(),
                                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "hello " } }).to_string(),
                                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "world" } }).to_string(),
                                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                                json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } }).to_string(),
                            ],
                            "hello world",
                        )
                    }
                    Scenario::StreamingToolArgumentMerge => {
                        let events = tool_use_message();
                        ProviderWire::body(events.clone()).with_tool_call_stream(
                            events
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|v| v.as_str().unwrap().to_string())
                                .collect(),
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::StreamingToolCallAbortEquivalence => {
                        ProviderWire::body(Value::Null).with_aborted_tool_call_stream(
                            vec![
                                json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT } } }).to_string(),
                                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "call_abort", "name": "lookup", "input": {} } }).to_string(),
                                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":" } }).to_string(),
                                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"x\"}" } }).to_string(),
                                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                            ],
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::UsageCacheHit => ProviderWire::body(text_message(
                        "end_turn",
                        "ok",
                        // Anthropic's input_tokens is net of cache; the suite's
                        // canonical input is the gross total, so split it.
                        json!({
                            "input_tokens": U::BASE_INPUT - U::CACHED_INPUT,
                            "output_tokens": U::BASE_OUTPUT,
                            "cache_read_input_tokens": U::CACHED_INPUT
                        }),
                    )),
                    Scenario::UsageReasoning => ProviderWire::body(text_message(
                        "end_turn",
                        "ok",
                        json!({
                            "input_tokens": U::BASE_INPUT,
                            "output_tokens": U::OUTPUT_WITH_REASONING,
                            "output_tokens_details": { "thinking_tokens": U::REASONING }
                        }),
                    )),
                    Scenario::ReasoningExtraction => ProviderWire::body(json!([
                        json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT } } }).to_string(),
                        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking" } }).to_string(),
                        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "thinking about it" } }).to_string(),
                        json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text" } }).to_string(),
                        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "answer" } }).to_string(),
                        json!({ "type": "content_block_stop", "index": 1 }).to_string(),
                        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } }).to_string(),
                    ]))
                    .with_reasoning_text("thinking about it"),
                    Scenario::ReasoningReplayRoundTrip => {
                        // Two signed thinking blocks: a provider that keeps the
                        // first signature and drops the second must fail.
                        let first = strong_replay_payload("anthropic/thinking-0");
                        let second = strong_replay_payload("anthropic/thinking-1");
                        let mut sse = vec![
                            json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT } } }).to_string(),
                        ];
                        for (index, signature) in [&first, &second].into_iter().enumerate() {
                            sse.push(json!({ "type": "content_block_start", "index": index, "content_block": { "type": "thinking" } }).to_string());
                            sse.push(json!({ "type": "content_block_delta", "index": index, "delta": { "type": "thinking_delta", "thinking": format!("thinking about it {index}") } }).to_string());
                            sse.push(json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": signature } }).to_string());
                            sse.push(json!({ "type": "content_block_stop", "index": index }).to_string());
                        }
                        sse.push(json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } }).to_string());
                        ProviderWire::body(Value::Null).with_reasoning_replay_round_trip(
                            sse,
                            vec![
                                ReplayItemExpectation::new(
                                    first.clone(),
                                    "/messages/0/content/0/signature",
                                    json!(first),
                                ),
                                ReplayItemExpectation::new(
                                    second.clone(),
                                    "/messages/0/content/1/signature",
                                    json!(second),
                                ),
                            ],
                        )
                    }
                    Scenario::ToolCallReplayRoundTrip => return None,
                    Scenario::StreamingUsageMerge => ProviderWire::body(Value::Null)
                        .with_usage_merge_stream(vec![
                            // input arrives in message_start
                            json!({ "type": "message_start", "message": { "usage": { "input_tokens": U::BASE_INPUT } } }).to_string(),
                            // output arrives later in message_delta; merge must keep input
                            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": U::BASE_OUTPUT } }).to_string(),
                        ]),
                };
                Some(wire)
            }

            fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
                replay(body).0
            }

            fn usage_from_wire(&self, body: &Value) -> LlmUsage {
                replay(body).1
            }

            fn terminal_from_wire(
                &self,
                body: &Value,
                _parts: &[LlmOutputPart],
            ) -> LlmTerminalReason {
                replay(body).2
            }

            fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
                let mut state = StreamState::default();
                let stream_events = Arc::new(std::sync::Mutex::new(Vec::new()));
                let event_sink = Arc::clone(&stream_events);
                let sender = LlmEventSender::new(move |event| {
                    event_sink.lock_recover().push(event);
                });
                for raw in sse_events {
                    AnthropicProvider::process_sse_event(raw, &mut state, Some(&sender), true)
                        .expect("anthropic sse event parses");
                }
                let (mut parts, _text, usage, _terminal) =
                    AnthropicProvider::finalize(state, "claude-test");
                if matches!(scenario, Scenario::ReasoningReplayRoundTrip) {
                    let route = AnthropicProvider::new("test").route_identity("claude-sonnet-4-6");
                    for part in &mut parts {
                        part.stamp_replay_origin(&route)
                            .expect("conformance output accepts its minting route");
                    }
                }
                let stream_events = stream_events.lock_recover().clone();
                StreamAssembly {
                    parts,
                    usage,
                    stream_events,
                }
            }

            fn build_next_request(&self, _scenario: Scenario, messages: Vec<LlmMessage>) -> Value {
                AnthropicProvider::new("test")
                    .build_request_body(&request(messages))
                    .expect("anthropic next request serializes")
            }
        }

        #[test]
        fn anthropic_satisfies_provider_conformance() {
            provider_conformance(&AnthropicNormalizer);
        }
    }
}
