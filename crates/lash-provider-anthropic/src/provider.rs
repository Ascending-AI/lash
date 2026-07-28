//! The [`Provider`] trait implementation: config serialization plus the
//! `complete` request/stream driver.

use lash_llm_transport::util::extract_error_detail;

use crate::config::DEFAULT_BASE_URL;
use crate::policy::{ANTHROPIC_VERSION, FINE_GRAINED_BETA, INTERLEAVED_THINKING_BETA};
use crate::stream::StreamState;
use crate::support::*;

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> &'static str {
        "anthropic"
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "api_key".to_string(),
            serde_json::Value::String(self.api_key.clone()),
        );
        if let Some(base_url) = &self.base_url {
            map.insert(
                "base_url".to_string(),
                serde_json::Value::String(base_url.clone()),
            );
        }
        if self.stream_termination != StreamTermination::RequireTerminalEvidence {
            map.insert(
                "stream_termination".to_string(),
                serde_json::to_value(self.stream_termination).unwrap_or(Value::Null),
            );
        }
        serialize_options_tail(&mut map, &self.options);
        serde_json::Value::Object(map)
    }

    async fn complete(&mut self, req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let timeouts = self.options.llm_timeouts();
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let body = self.build_request_body(&req)?;
        let generation_disposition = Some(Self::generation_disposition(&req, &body));
        let request_body_bytes = serde_json::to_vec(&body).map_err(|err| {
            LlmTransportError::new(format!("Failed to serialize Anthropic body: {err}"))
                .with_kind(ProviderFailureKind::Validation)
        })?;
        emit_provider_request_trace(
            provider_trace.as_ref(),
            "anthropic",
            "messages",
            &request_body_bytes,
        );
        let request_body = Some(String::from_utf8_lossy(&request_body_bytes).into_owned());

        // `fine-grained-tool-streaming-2025-05-14` streams partial JSON so we
        // can surface tool arguments incrementally. Interleaved thinking is
        // built-in on adaptive thinking; the beta is only needed for the
        // budget-encoded (`"type": "enabled"`) thinking block, so we gate on
        // the thinking shape actually emitted rather than the model name.
        let mut betas = vec![FINE_GRAINED_BETA.to_string()];
        let budget_thinking = body
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");
        if budget_thinking {
            betas.push(INTERLEAVED_THINKING_BETA.to_string());
        }

        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let request = LlmHttpRequest::post(url.clone(), request_body_bytes)
            .with_header("x-api-key", self.api_key.clone())
            .with_header("anthropic-version", ANTHROPIC_VERSION)
            .with_header("anthropic-beta", betas.join(","))
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "text/event-stream")
            .with_body_for_error(request_body.clone().unwrap_or_default())
            .with_response_start_timeout_message("Anthropic response start timed out");

        let resp = self
            .transport
            .send(
                request,
                response_start_timeout(timeouts.request_timeout, timeouts.chunk_timeout, true),
            )
            .await?;

        let status = resp.status;
        if !resp.is_success() {
            let headers = resp.headers;
            let text = read_http_body_text(
                resp.body,
                timeouts.request_timeout,
                "Anthropic response body timed out",
            )
            .await
            .unwrap_or_default();
            let detail = extract_error_detail(&text);
            let message = if let Some(detail) = detail {
                format!("Anthropic request failed with {}: {}", status, detail,)
            } else {
                format!("Anthropic request failed with {}", status)
            };
            return Err(http_error_envelope(
                message,
                status,
                headers,
                text,
                request_body.clone(),
            ));
        }

        let mut state = StreamState::default();
        let expose_thinking = self.options.expose_thinking;
        let stream_result = drive_sse_response(
            resp.body,
            timeouts.chunk_timeout,
            "Anthropic stream chunk timed out",
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "anthropic", raw);
                Self::process_sse_event(raw, &mut state, stream_events.as_ref(), expose_thinking)
            },
        )
        .await;

        let stream_termination = req
            .model_capability
            .stream_termination
            .unwrap_or(self.stream_termination);
        if let Err(error) = stream_result {
            return Err(error.with_partial_response(Self::partial_response(
                state.clone(),
                request_body.clone(),
                &url,
                generation_disposition,
            )));
        }
        if stream_termination == StreamTermination::RequireTerminalEvidence
            && !state.message_stopped
        {
            return Err(
                LlmTransportError::new("Anthropic stream ended before message_stop")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_code("stream_ended_before_message_stop")
                    .retryable(true)
                    .with_partial_response(Self::partial_response(
                        state,
                        request_body,
                        &url,
                        generation_disposition,
                    )),
            );
        }

        let provider_usage = state.provider_usage.take();
        let (parts, full_text, usage, terminal_reason) = Self::finalize(state);
        Ok(LlmResponse {
            full_text,
            parts,
            usage,
            terminal_reason,
            terminal_diagnostic: None,
            provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {} (stream)", url)),
            execution_evidence: None,
            generation_disposition,
            response_metadata: Default::default(),
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

impl AnthropicProvider {
    /// Which of the caller's generation options this request carries.
    ///
    /// The assembled body is the evidence, so the record cannot drift from
    /// what was sent: Messages always carries a `max_tokens`, it has no seed
    /// field at all, and the one reason a requested temperature is missing is
    /// that this request pins sampling — extended thinking, or a model whose
    /// host-declared capability says so.
    pub(crate) fn generation_disposition(req: &LlmRequest, body: &Value) -> GenerationDisposition {
        GenerationDisposition {
            output_token_cap: GenerationOptionDisposition::applied(
                req.generation.output_token_cap.is_some(),
            ),
            temperature: if body.get("temperature").is_some() {
                GenerationOptionDisposition::Applied
            } else {
                GenerationOptionDisposition::sampling_pinned(req.generation.temperature.is_some())
            },
            seed: GenerationOptionDisposition::unsupported(req.generation.seed.is_some()),
        }
    }

    fn partial_response(
        mut state: StreamState,
        request_body: Option<String>,
        url: &str,
        generation_disposition: Option<GenerationDisposition>,
    ) -> LlmResponse {
        let provider_usage = state.provider_usage.take();
        let (parts, full_text, usage, _) = Self::finalize(state);
        LlmResponse {
            full_text,
            parts,
            usage,
            terminal_reason: LlmTerminalReason::Unknown,
            terminal_diagnostic: None,
            provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {url} (stream)")),
            execution_evidence: None,
            generation_disposition,
            response_metadata: Default::default(),
        }
    }
}
