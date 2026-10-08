//! The [`Provider`] trait implementation: config serialization plus the
//! `complete` request/stream driver.

use crate::config::DEFAULT_BASE_URL;
use crate::policy::{
    ANTHROPIC_VERSION, CONTEXT_MANAGEMENT_BETA, FINE_GRAINED_BETA, INTERLEAVED_THINKING_BETA,
    OAUTH_API_BETA,
};
use crate::stream::StreamState;
use crate::support::*;

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> &'static str {
        "anthropic"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(
            self.kind(),
            self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
            model,
        )
    }

    fn attachment_accepts(
        &self,
        _model: &str,
        mime: &lash_sansio::MediaType,
        _position: AttachmentPosition,
    ) -> ProviderAccepts {
        self.accepts_attachment(mime)
    }
    fn attachment_file_scope(&self) -> Option<ProviderFileScope> {
        self.file_scope()
    }
    fn encode_slot(
        &self,
        slot: &AttachmentSlot,
        delivery: &Delivery,
    ) -> Result<TransientJson, LlmTransportError> {
        self.encode_attachment(slot, delivery)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if self.auth_scheme != crate::AnthropicAuthScheme::default() {
            map.insert(
                "auth_scheme".to_string(),
                Value::String(
                    match self.auth_scheme {
                        crate::AnthropicAuthScheme::ApiKey => "api_key",
                        crate::AnthropicAuthScheme::Bearer => "bearer",
                    }
                    .to_string(),
                ),
            );
        }
        if let Some(base_url) = &self.base_url {
            map.insert(
                "base_url".to_string(),
                serde_json::Value::String(base_url.clone()),
            );
        }
        if self.stream_termination != StreamTermination::default() {
            map.insert(
                "stream_termination".to_string(),
                serde_json::to_value(self.stream_termination).unwrap_or(Value::Null),
            );
        }
        serialize_options_tail(&mut map, &self.options);
        serde_json::Value::Object(map)
    }

    async fn lower(
        &mut self,
        req: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        self.validate_route_and_headers(req.model.wire_model())?;
        let (body, receipt) = self.build_request(req)?;
        lower_attachment_json(
            |mime, position| self.attachment_accepts(req.model.wire_model(), mime, position),
            req,
            self.route_identity(req.model.wire_model()),
            (true, Some(receipt)),
            &body,
            crate::attachment_delivery::CODEC,
            &[
                "/messages/*/content/*/source",
                "/messages/*/content/*/content/*/source",
            ],
        )
    }

    async fn send(
        &mut self,
        admitted: &LiveRequestBody,
        mut context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        let minting_route = self.validate_route_and_headers(context.model().wire_model())?;
        if let Some(downstream) = context.stream_events.take() {
            let stream_route = minting_route.clone();
            context.stream_events = Some(LlmEventSender::new(move |mut event| {
                if let LlmStreamEvent::Part(part) = &mut event {
                    let _ = part.stamp_replay_origin(&stream_route);
                }
                downstream.send(event);
            }));
        }
        let tokens = Arc::clone(&self.tokens);
        let mut lease = tokens.current(&minting_route).await?;
        match self
            .send_attempt(&context, admitted, &minting_route, &lease.token)
            .await
        {
            Err(error) if rejected_before_output(&error) => {
                match tokens
                    .replace(&minting_route, &lease, TokenRequestReason::Rejected)
                    .await?
                {
                    // Resend the admitted body once with the fresh token.
                    Some(fresh) => {
                        lease = fresh;
                        self.send_attempt(&context, admitted, &minting_route, &lease.token)
                            .await
                    }
                    None => Err(error),
                }
            }
            other => other,
        }
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

impl AnthropicProvider {
    /// One attempt of `send`, authenticated by `token`.
    async fn send_attempt(
        &self,
        context: &ResponseContext,
        admitted: &LiveRequestBody,
        minting_route: &ProviderRouteIdentity,
        token: &ProviderToken,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = context.stream_events.clone();
        let provider_trace = context.provider_trace.clone();
        let timeouts = self.options.llm_timeouts();
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let body: Value = serde_json::from_str(&admitted.redacted()).map_err(|err| {
            LlmTransportError::new(format!("The Anthropic request body does not decode: {err}"))
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::AdmittedRequestUnavailable)
                .with_retry_verdict(TransportRetryVerdict::Forbidden)
        })?;
        let generation_disposition = admitted.generation();
        let request_body_bytes = admitted.wire().into_bytes();
        emit_provider_request_trace(
            provider_trace.as_ref(),
            "anthropic",
            "messages",
            admitted.redacted().as_bytes(),
        );
        let request_body = Some(admitted.redacted());
        // `fine-grained-tool-streaming-2025-05-14` streams partial JSON so we
        // can surface tool arguments incrementally. Interleaved thinking is
        // built-in on adaptive thinking; the beta is only needed for the
        // budget-encoded (`"type": "enabled"`) thinking block, so we gate on
        // the thinking shape actually emitted rather than the model name.
        let mut betas = vec![FINE_GRAINED_BETA.to_string()];
        if self.auth_scheme == crate::AnthropicAuthScheme::Bearer {
            betas.push(OAUTH_API_BETA.to_string());
        }
        let budget_thinking = body
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");
        if budget_thinking {
            betas.push(INTERLEAVED_THINKING_BETA.to_string());
        }
        if body.get("context_management").is_some() {
            betas.push(CONTEXT_MANAGEMENT_BETA.to_string());
        }

        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let mut request = LlmHttpRequest::post(url.clone(), request_body_bytes)
            .with_header("anthropic-version", ANTHROPIC_VERSION)
            .with_header("anthropic-beta", betas.join(","))
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "text/event-stream")
            .with_body_for_error(request_body.clone().unwrap_or_default())
            .with_response_start_timeout_message("Anthropic response start timed out");
        let (name, value) = match self.auth_scheme {
            crate::AnthropicAuthScheme::ApiKey => {
                ("x-api-key", token.secret().expose_secret().to_string())
            }
            crate::AnthropicAuthScheme::Bearer => (
                "Authorization",
                format!("Bearer {}", token.secret().expose_secret()),
            ),
        };
        request = request.with_header(name, lash_llm_transport::HttpHeaderValue::sensitive(value));
        merge_extra_headers(&mut request.headers, &self.extra_headers, true)?;
        let stream_bounds = SseStreamBounds::new(timeouts.request_timeout, &self.options);

        let resp = self
            .transport
            .send(
                request,
                response_start_timeout(
                    timeouts.request_timeout,
                    timeouts.response_start_timeout,
                    true,
                ),
            )
            .await?;

        let status = resp.status;
        if !resp.is_success() {
            let mut headers = resp.headers;
            if first_header_value(&headers, "x-request-id").is_none()
                && let Some(request_id) =
                    first_header_value(&headers, "request-id").map(str::to_string)
            {
                headers.push(("x-request-id".to_string(), request_id));
            }
            let text = read_http_body_text(
                resp.body,
                self.options.response_body_limit(),
                timeouts.request_timeout,
                "Anthropic response body timed out",
            )
            .await?;
            return Err(http_error_envelope(
                format!("Anthropic request failed with {}", status),
                status,
                headers,
                text,
                request_body.clone(),
            ));
        }
        let provider_request_id = first_header_value(&resp.headers, "request-id")
            .or_else(|| first_header_value(&resp.headers, "x-request-id"))
            .map(str::to_string);
        let mut response_metadata = ResponseMetadataCapture::from_response(
            &context.model().metadata().request_defaults,
            &resp.headers,
        );
        if let Some(tx) = &stream_events {
            tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                response_started: true,
                request_body: request_body.clone(),
                http_summary: Some(format!(
                    "HTTP POST {}/v1/messages (stream)",
                    base_url.trim_end_matches('/')
                )),
                execution_evidence: provider_request_id.clone().map(|provider_request_id| {
                    ExecutionEvidence {
                        provider_request_id: Some(provider_request_id),
                        ..Default::default()
                    }
                }),
                generation_disposition,
                response_metadata: response_metadata.metadata(),
                ..Default::default()
            }));
        }
        let mut state = StreamState {
            execution_evidence: provider_request_id.map(|provider_request_id| ExecutionEvidence {
                provider_request_id: Some(provider_request_id),
                ..Default::default()
            }),
            expose_thinking: context.model().metadata().request_defaults.expose_thinking,
            ..StreamState::default()
        };
        let expose_thinking = context.model().metadata().request_defaults.expose_thinking;
        let stream_result = drive_sse_response(
            resp.body,
            timeouts.chunk_timeout,
            stream_bounds,
            "Anthropic stream chunk timed out",
            "Anthropic request timed out",
            &mut response_metadata,
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "anthropic", raw);
                Self::process_sse_event(raw, &mut state, stream_events.as_ref(), expose_thinking)
            },
        )
        .await;

        let stream_termination = context
            .model()
            .metadata()
            .capability
            .stream_termination
            .unwrap_or(self.stream_termination);
        if let Err(error) = stream_result {
            let mut partial = Self::partial_response(
                state.clone(),
                request_body.clone(),
                &url,
                generation_disposition,
                context.model().wire_model(),
            );
            partial.response_metadata = response_metadata.into_metadata();
            partial
                .stamp_replay_origin(minting_route)
                .map_err(replay_origin_conflict_error)?;
            return Err(error.with_partial_response(partial));
        }
        // A stream that ended before `message_stop` completes when the route
        // tolerates EOF and it produced output; one that produced none
        // failed, whatever the route tolerates.
        if !state.message_stopped
            && (stream_termination == StreamTermination::RequireTerminalEvidence
                || state.blocks.is_empty())
        {
            let mut partial = Self::partial_response(
                state,
                request_body,
                &url,
                generation_disposition,
                context.model().wire_model(),
            );
            partial.response_metadata = response_metadata.into_metadata();
            partial
                .stamp_replay_origin(minting_route)
                .map_err(replay_origin_conflict_error)?;
            return Err(
                LlmTransportError::new("Anthropic stream ended before message_stop")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_lash_code(TurnFailureCode::StreamEndedBeforeMessageStop)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_partial_response(partial),
            );
        }

        let provider_usage = state.provider_usage.take();
        let execution_evidence = state.execution_evidence.clone();
        let expose_thinking = state.expose_thinking;
        let (parts, usage, terminal_reason) = Self::finalize(state, context.model().wire_model());
        let mut response = LlmResponse {
            parts,
            usage,
            terminal_reason,
            terminal_diagnostic: None,
            provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {} (stream)", url)),
            execution_evidence,
            generation_disposition,
            response_metadata: response_metadata.into_metadata(),
            expose_thinking: Some(expose_thinking),
        };
        response
            .stamp_replay_origin(minting_route)
            .map_err(replay_origin_conflict_error)?;
        Ok(response)
    }
}

fn replay_origin_conflict_error(
    conflict: lash_core::llm::types::ProviderReplayOriginConflict,
) -> LlmTransportError {
    LlmTransportError::new(conflict.to_string())
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(TurnFailureCode::ProviderReplayOriginConflict)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

impl AnthropicProvider {
    /// The route serving `model`, once its endpoint and the host's extra
    /// headers are valid.
    fn validate_route_and_headers(
        &self,
        model: &str,
    ) -> Result<ProviderRouteIdentity, LlmTransportError> {
        validate_extra_headers(
            &self.extra_headers,
            &[
                "x-api-key",
                "authorization",
                "anthropic-version",
                "anthropic-beta",
                "content-type",
                "accept",
            ],
            true,
        )?;
        let route = self.route_identity(model);
        route.validate_endpoint().map_err(|error| {
            LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
        })?;
        Ok(route)
    }

    fn partial_response(
        mut state: StreamState,
        request_body: Option<String>,
        url: &str,
        generation_disposition: Option<GenerationReceipt>,
        origin_model: &str,
    ) -> LlmResponse {
        let provider_usage = state.provider_usage.take();
        let execution_evidence = state.execution_evidence.clone();
        let expose_thinking = state.expose_thinking;
        let (parts, usage, _) = Self::finalize(state, origin_model);
        LlmResponse {
            parts,
            usage,
            terminal_reason: LlmTerminalReason::Unknown,
            terminal_diagnostic: None,
            provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {url} (stream)")),
            execution_evidence,
            generation_disposition,
            response_metadata: Default::default(),
            expose_thinking: Some(expose_thinking),
        }
    }
}
