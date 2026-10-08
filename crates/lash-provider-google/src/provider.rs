//! The [`Provider`] trait implementation: config serialization, lowering
//! (project resolution, attachment uploads, request build), sending an
//! admitted body, the request/stream executor, and project-id resolution.

use crate::support::*;
use std::sync::Arc;

/// How to read one response, fixed by the request before any I/O: whether
/// the stream must end with terminal evidence, and the recorded model's
/// request defaults, which say whether it surfaces thinking and which
/// response metadata it captures.
#[derive(Clone, Debug)]
pub(crate) struct ResponseReading {
    pub(crate) stream_termination: StreamTermination,
    pub(crate) defaults: lash_core::provider::LlmProfileRequestDefaults,
}

impl GoogleOAuthProvider {
    /// Whether a non-2xx body is the API's definite refusal of a file the
    /// request named: the error object's own status says the resource is
    /// missing, invalid or inaccessible, and its message names a missing or
    /// expired file.
    fn rejects_file_reference(status: u16, body: &Value) -> bool {
        // A streaming endpoint wraps its error object in an array.
        let Some(error) = body
            .get("error")
            .or_else(|| body.get(0).and_then(|first| first.get("error")))
        else {
            return false;
        };
        matches!(
            (status, error.get("status").and_then(Value::as_str)),
            (404, Some("NOT_FOUND"))
                | (400, Some("INVALID_ARGUMENT" | "FAILED_PRECONDITION"))
                | (403, Some("PERMISSION_DENIED"))
        ) && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(message_names_missing_file)
    }

    /// Send `request` as a body, for a test that scripts the response.
    #[cfg(test)]
    pub(crate) async fn execute_request(
        &self,
        access_token: &str,
        request: Value,
        stream_events: Option<lash_core::llm::types::LlmEventSender>,
        provider_trace: Option<lash_core::llm::types::LlmProviderTraceSender>,
        reading: ResponseReading,
        generation_disposition: Option<GenerationReceipt>,
    ) -> Result<LlmResponse, LlmTransportError> {
        let template = RecordedRequestTemplate::literal(
            self.route_identity_for_model("test"),
            stream_events.is_some(),
            generation_disposition,
            request.to_string(),
        )
        .map_err(template_error)?;
        let body = LiveRequestBody::fill(Arc::new(template), Vec::new()).map_err(template_error)?;
        self.execute_body(access_token, &body, stream_events, provider_trace, reading)
            .await
    }

    /// Send `admitted`'s bytes as they are, authenticated by `access_token`.
    pub(crate) async fn execute_body(
        &self,
        access_token: &str,
        admitted: &LiveRequestBody,
        stream_events: Option<lash_core::llm::types::LlmEventSender>,
        provider_trace: Option<lash_core::llm::types::LlmProviderTraceSender>,
        reading: ResponseReading,
    ) -> Result<LlmResponse, LlmTransportError> {
        let ResponseReading {
            stream_termination,
            defaults,
        } = reading;
        let expose_thinking = defaults.expose_thinking;
        let generation_disposition = admitted.generation();
        // The body decides whether the response streams.
        let stream_events = if admitted.stream() {
            stream_events.or_else(|| Some(lash_core::llm::types::LlmEventSender::new(|_| {})))
        } else {
            None
        };
        let request_body_bytes = admitted.wire().into_bytes();
        let request_body = Some(admitted.redacted());
        // The model the body names reads the response's parts.
        let origin_model = serde_json::from_str::<Value>(&admitted.redacted())
            .ok()
            .and_then(|body| body.get("model").and_then(Value::as_str).map(str::to_owned));
        let method = if stream_events.is_some() {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        emit_provider_request_trace(
            provider_trace.as_ref(),
            "google",
            method,
            admitted.redacted().as_bytes(),
        );
        let mut url = self.method_url(method);
        if stream_events.is_some() {
            url.push_str("?alt=sse");
        }
        let mut http_request = LlmHttpRequest::post(url.clone(), request_body_bytes)
            .with_header(
                "Authorization",
                lash_llm_transport::HttpHeaderValue::sensitive(format!("Bearer {access_token}")),
            )
            .with_header("Content-Type", "application/json")
            .with_body_for_error(request_body.clone().unwrap_or_default())
            .with_response_start_timeout_message("Cloud Code response start timed out");
        merge_extra_headers(&mut http_request.headers, &self.extra_headers, false)?;
        let timeouts = self.options.llm_timeouts();
        let stream_bounds = SseStreamBounds::new(timeouts.request_timeout, &self.options);
        let resp = self
            .transport
            .send(
                http_request,
                response_start_timeout(
                    timeouts.request_timeout,
                    timeouts.response_start_timeout,
                    stream_events.is_some(),
                ),
            )
            .await?;

        if !resp.is_success() {
            let status = resp.status;
            let headers = resp.headers;
            let body = read_http_body_text(
                resp.body,
                self.options.response_body_limit(),
                timeouts.request_timeout,
                "Cloud Code response body timed out",
            )
            .await?;
            return Err(http_error_envelope(
                format!("Cloud Code request failed with {}", status),
                status,
                headers,
                body,
                request_body,
            ));
        }
        let provider_request_id =
            first_header_value(&resp.headers, "x-request-id").map(str::to_string);
        let mut response_metadata =
            ResponseMetadataCapture::from_response(&defaults, &resp.headers);
        if let Some(tx) = &stream_events {
            tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                response_started: true,
                request_body: request_body.clone(),
                http_summary: Some(format!("HTTP POST {url} (stream)")),
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
        if stream_events.is_none() {
            let text = read_http_body_text(
                resp.body,
                self.options.response_body_limit(),
                timeouts.request_timeout,
                "Cloud Code response body timed out",
            )
            .await?;
            response_metadata.capture_body_text(&text);
            emit_provider_trace(provider_trace.as_ref(), "google", &text);
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid Cloud Code response JSON: {e}"))
                    .with_raw(text.clone())
                    .with_retry_verdict(TransportRetryVerdict::NotRetryable)
            })?;
            let parts = self.response_parts_from_value(&value, origin_model.as_deref());
            let provider_usage = value.get("usageMetadata").cloned();
            let usage = provider_usage
                .as_ref()
                .map(|meta| {
                    Self::usage_from_event(&json!({
                        "response": {
                            "usageMetadata": meta
                        }
                    }))
                })
                .unwrap_or_default();
            let terminal_reason = Self::terminal_reason_from_value(&value, &parts);
            let mut execution_evidence =
                provider_request_id.map(|provider_request_id| ExecutionEvidence {
                    provider_request_id: Some(provider_request_id),
                    ..Default::default()
                });
            ExecutionEvidence::merge_optional(
                &mut execution_evidence,
                Self::execution_evidence_from_value(&value),
            )
            .map_err(|error| {
                LlmTransportError::new(format!("Google response {error}"))
                    .with_kind(ProviderFailureKind::Stream)
                    .with_lash_code(TurnFailureCode::from_wire(error.code()))
            })?;
            return Ok(LlmResponse {
                parts,
                usage,
                terminal_reason,
                terminal_diagnostic: None,
                provider_usage,
                request_body,
                http_summary: Some(format!("HTTP POST {}", url)),
                execution_evidence,
                generation_disposition,
                response_metadata: response_metadata.into_metadata(),
                expose_thinking: Some(expose_thinking),
            });
        }

        let mut stream_state = GoogleStreamState::default();
        stream_state.expose_thinking = expose_thinking;
        stream_state.execution_evidence =
            provider_request_id.map(|provider_request_id| ExecutionEvidence {
                provider_request_id: Some(provider_request_id),
                ..Default::default()
            });
        let stream_result = drive_sse_response(
            resp.body,
            timeouts.chunk_timeout,
            stream_bounds,
            "Cloud Code stream chunk timed out",
            "Cloud Code request timed out",
            &mut response_metadata,
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "google", raw);
                let prev_usage = stream_state.usage.clone();
                let prev_execution_evidence = stream_state.execution_evidence.clone();
                let first_new_tool_call = stream_state.tool_call_parts.len();
                let deltas = stream_state.push_event(self, raw, origin_model.as_deref())?;
                if let Some(tx) = stream_events.as_ref()
                    && expose_thinking
                {
                    for event in deltas.reasoning_events {
                        tx.send(event);
                    }
                }
                if let Some(tx) = stream_events.as_ref() {
                    if stream_state.usage != prev_usage && stream_state.usage != LlmUsage::default()
                    {
                        tx.send(LlmStreamEvent::Usage(stream_state.usage.clone()));
                    }
                    if stream_state.provider_usage.is_some() {
                        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                            provider_usage: stream_state.provider_usage.clone(),
                            execution_evidence: (stream_state.execution_evidence
                                != prev_execution_evidence)
                                .then(|| stream_state.execution_evidence.clone())
                                .flatten(),
                            ..Default::default()
                        }));
                    } else if stream_state.execution_evidence != prev_execution_evidence {
                        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                            execution_evidence: stream_state.execution_evidence.clone(),
                            ..Default::default()
                        }));
                    }
                    for event in deltas.text_events {
                        tx.send(event);
                    }
                    for part in &stream_state.tool_call_parts[first_new_tool_call..] {
                        tx.send(LlmStreamEvent::Part(part.clone()));
                    }
                }
                Ok(())
            },
        )
        .await;

        if stream_result.is_ok()
            && let Some(tx) = stream_events.as_ref()
        {
            if expose_thinking {
                for event in stream_state.flush_open_reasoning_part() {
                    tx.send(event);
                }
            }
            if let Some(event) = stream_state.seal_text_block() {
                tx.send(event);
            }
        }

        let partial_response = || {
            let mut parts = stream_state.output_parts.clone();
            if parts
                .iter()
                .filter_map(|part| match part {
                    LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
                .is_empty()
                && !stream_state.full.is_empty()
            {
                parts.push(LlmOutputPart::Text {
                    text: stream_state.full.clone(),
                    response_meta: None,
                });
            }
            parts.extend(stream_state.tool_call_parts.clone());
            LlmResponse {
                parts,
                usage: stream_state.usage.clone(),
                terminal_reason: LlmTerminalReason::Unknown,
                terminal_diagnostic: None,
                provider_usage: stream_state.provider_usage.clone(),
                request_body: request_body.clone(),
                http_summary: Some(format!("HTTP POST {url} (stream)")),
                execution_evidence: stream_state.execution_evidence.clone(),
                generation_disposition,
                response_metadata: response_metadata.metadata(),
                expose_thinking: Some(stream_state.expose_thinking),
            }
        };
        if let Err(error) = stream_result {
            return Err(error.with_partial_response(partial_response()));
        }
        if stream_termination == StreamTermination::RequireTerminalEvidence
            && stream_state.finish_event.is_none()
        {
            return Err(
                LlmTransportError::new("Google stream ended without finishReason")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_partial_response(partial_response()),
            );
        }

        let mut parts = stream_state.output_parts;
        if parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .is_empty()
            && !stream_state.full.is_empty()
        {
            parts.push(LlmOutputPart::Text {
                text: stream_state.full.clone(),
                response_meta: None,
            });
        }
        parts.extend(stream_state.tool_call_parts);

        // Mirror the non-streaming path: derive the terminal reason from the
        // last `finishReason` observed across the SSE events. When no event
        // carried one, `terminal_reason_from_value` on a value without a
        // finishReason falls back to ToolUse/Stop from the assembled parts.
        let terminal_reason = Self::terminal_reason_from_value(
            stream_state.finish_event.as_ref().unwrap_or(&Value::Null),
            &parts,
        );

        Ok(LlmResponse {
            parts,
            usage: stream_state.usage,
            terminal_reason,
            terminal_diagnostic: None,
            provider_usage: stream_state.provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {}", url)),
            execution_evidence: stream_state.execution_evidence,
            generation_disposition,
            response_metadata: response_metadata.into_metadata(),
            expose_thinking: Some(stream_state.expose_thinking),
        })
    }

    async fn resolve_project_id(
        &self,
        access_token: &str,
    ) -> Result<Option<String>, LlmTransportError> {
        let metadata = json!({
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        });

        let req = json!({
            "cloudaicompanionProject": null,
            "metadata": metadata,
        });
        let request_body_bytes = serde_json::to_vec(&req).map_err(|err| {
            LlmTransportError::new(format!(
                "Failed to serialize Cloud Code loadCodeAssist body: {err}"
            ))
            .with_kind(lash_core::ProviderFailureKind::Validation)
        })?;
        let request_body = Some(String::from_utf8_lossy(&request_body_bytes).into_owned());
        let http_request =
            LlmHttpRequest::post(self.method_url("loadCodeAssist"), request_body_bytes)
                .with_header(
                    "Authorization",
                    lash_llm_transport::HttpHeaderValue::sensitive(format!(
                        "Bearer {access_token}"
                    )),
                )
                .with_header("Content-Type", "application/json")
                .with_body_for_error(request_body.clone().unwrap_or_default())
                .with_response_start_timeout_message(
                    "Cloud Code loadCodeAssist response start timed out",
                );
        let resp = self
            .transport
            .send(http_request, self.options.llm_timeouts().request_timeout)
            .await?;
        if !resp.is_success() {
            let status = resp.status;
            let headers = resp.headers;
            let body = read_http_body_text(
                resp.body,
                self.options.response_body_limit(),
                self.options.llm_timeouts().request_timeout,
                "Cloud Code loadCodeAssist body timed out",
            )
            .await?;
            return Err(http_error_envelope(
                format!("Cloud Code loadCodeAssist failed with {}", status),
                status,
                headers,
                body,
                request_body,
            ));
        }
        let text = read_http_body_text(
            resp.body,
            self.options.response_body_limit(),
            self.options.llm_timeouts().request_timeout,
            "Cloud Code loadCodeAssist body timed out",
        )
        .await?;
        let body: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Cloud Code loadCodeAssist JSON: {e}"))
                .with_raw(text.clone())
        })?;
        Ok(body
            .get("cloudaicompanionProject")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }

    /// Resolve the project and pin attachment slots without delivering bytes
    /// or uploading provider files.
    async fn lower_with_token(
        &mut self,
        req: &LlmRequest,
        lease: &TokenLease,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        if self.project_id.is_none() {
            self.project_id = match self.resolved_project_id.get() {
                Some(resolved) => Some(resolved.clone()),
                None => {
                    let resolved = self
                        .resolve_project_id(lease.token.secret().expose_secret())
                        .await?;
                    if let Some(resolved) = &resolved {
                        let _ = self.resolved_project_id.set(resolved.clone());
                    }
                    resolved
                }
            };
        }
        let contents = self.build_contents(req)?;
        let (request, receipt) =
            Self::build_request_tree(self, req, contents, self.project_id.as_deref())?;
        lower_attachment_json(
            |mime, position| self.attachment_accepts(req.model.wire_model(), mime, position),
            req,
            self.route_identity_for_model(req.model.wire_model()),
            (req.stream_events.is_some(), Some(receipt)),
            &request,
            crate::attachment_delivery::CODEC,
        )
    }

    /// Send with the current token and report the file slots the API
    /// definitely refused to the caller, which forgets the host store's
    /// derivatives and decides the retry.
    async fn send_with_token(
        &self,
        context: &ResponseContext,
        admitted: &LiveRequestBody,
        lease: &TokenLease,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = context.stream_events.clone();
        let provider_trace = context.provider_trace.clone();
        let reading = ResponseReading {
            stream_termination: context
                .model()
                .metadata()
                .capability
                .stream_termination
                .unwrap_or(self.stream_termination),
            defaults: context.model().metadata().request_defaults.clone(),
        };
        self.execute_body(
            lease.token.secret().expose_secret(),
            admitted,
            stream_events,
            provider_trace,
            reading,
        )
        .await
        .map_err(|err| reject_missing_provider_files(err, admitted, Self::rejects_file_reference))
    }
}

#[async_trait]
impl Provider for GoogleOAuthProvider {
    fn kind(&self) -> &'static str {
        "google_oauth"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.route_identity_for_model(model)
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
        // No credential: the host re-attaches its token source when it
        // rebuilds the provider.
        let mut map = serde_json::Map::new();
        if self.endpoint != CODE_ASSIST_ENDPOINT {
            map.insert(
                "endpoint".to_string(),
                serde_json::Value::String(self.endpoint.clone()),
            );
        }
        if self.api_version != CODE_ASSIST_API_VERSION {
            map.insert(
                "api_version".to_string(),
                serde_json::Value::String(self.api_version.clone()),
            );
        }
        if let Some(project_id) = &self.project_id {
            map.insert(
                "project_id".to_string(),
                serde_json::Value::String(project_id.clone()),
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
        self.route_identity_for_model(req.model.wire_model())
            .validate_endpoint()
            .map_err(|error| {
                LlmTransportError::new(error.to_string())
                    .with_kind(ProviderFailureKind::Validation)
                    .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
            })?;
        let req = self.reasoning_retention_safe_request(req)?.into_owned();
        validate_extra_headers(
            &self.extra_headers,
            &["authorization", "content-type"],
            false,
        )?;
        Self::build_request_with_receipt(self, &req, Vec::new(), None)?;
        // Every generation refusal lands before the token source is asked,
        // the project lookup and any attachment upload.
        Self::resolve_generation(&req)?;
        let route = self.route_identity_for_model(req.model.wire_model());
        let tokens = Arc::clone(&self.tokens);
        let mut lease = tokens.current(&route).await?;
        match self.lower_with_token(&req, &lease).await {
            Err(error) if rejected_before_output(&error) => {
                match tokens
                    .replace(&route, &lease, TokenRequestReason::Rejected)
                    .await?
                {
                    Some(fresh) => {
                        lease = fresh;
                        self.lower_with_token(&req, &lease).await
                    }
                    None => Err(error),
                }
            }
            other => other,
        }
    }

    async fn send(
        &mut self,
        body: &LiveRequestBody,
        context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        self.route_identity_for_model(context.model().wire_model())
            .validate_endpoint()
            .map_err(|error| {
                LlmTransportError::new(error.to_string())
                    .with_kind(ProviderFailureKind::Validation)
                    .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
            })?;
        validate_extra_headers(
            &self.extra_headers,
            &["authorization", "content-type"],
            false,
        )?;
        let route = self.route_identity_for_model(context.model().wire_model());
        let tokens = Arc::clone(&self.tokens);
        let mut lease = tokens.current(&route).await?;
        match self.send_with_token(&context, body, &lease).await {
            Err(error) if rejected_before_output(&error) => {
                match tokens
                    .replace(&route, &lease, TokenRequestReason::Rejected)
                    .await?
                {
                    // Resend the admitted body once with the fresh token.
                    Some(fresh) => {
                        lease = fresh;
                        self.send_with_token(&context, body, &lease).await
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

#[cfg(test)]
mod error_detail_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct ProjectResolutionTransport {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmHttpTransport for ProjectResolutionTransport {
        async fn send(
            &self,
            request: LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let body = match attempt {
                0 => {
                    assert!(request.url.ends_with(":loadCodeAssist"));
                    r#"{"cloudaicompanionProject":"resolved-project"}"#
                }
                _ => {
                    assert!(
                        request.url.ends_with(":generateContent"),
                        "only the first request resolves the project: request {attempt}"
                    );
                    r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}"#
                }
            };
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: lash_llm_transport::LlmHttpBody::buffered(body),
            })
        }
    }

    pub(super) fn completion_request() -> LlmRequest {
        LlmRequest {
            instructions: None,
            model: lash_sansio::llm_profile::LlmProfileConfig::new(
                lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                    lash_sansio::llm_profile::LlmProfileMetadata::builder(
                        "gemini-3.1-pro-preview".to_string(),
                    )
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
                ),
            )
            .with_reasoning(Default::default()),
            messages: vec![lash_core::llm::types::LlmMessage::text(
                LlmRole::User,
                "hello",
            )],

            tools: Arc::new(Vec::<lash_core::llm::types::LlmToolSpec>::new()),
            tool_choice: LlmToolChoice::Auto,
            attachment_acceptance: Default::default(),
            scope: lash_core::LlmRequestScope::new(
                "project-resolution",
                "project-resolution:frame",
                "project-resolution:request",
            ),
            output_spec: None,
            stream_events: None,
            generation: Default::default(),
            provider_trace: None,
        }
    }

    /// Every refusal lands before the project lookup, any upload and the
    /// generate call: the transport never sees a request.
    #[tokio::test]
    async fn refused_settings_never_reach_the_project_lookup_or_the_transport() {
        let mut pinned = completion_request();
        pinned.generation.temperature =
            Some(lash_core::NonNegativeFiniteF64::new(0.5).expect("finite"));
        pinned.model.metadata_mut().capability.sampling = lash_core::SamplingCapability::Pinned;
        let mut parallel = completion_request();
        parallel.generation.parallel_tool_calls = Some(true);
        let mut effort = completion_request();
        effort.model.reasoning = lash_core::provider::ReasoningSelection::Effort("high".into());
        let mut gemini3_off = completion_request();
        gemini3_off.model.metadata_mut().capability.google_dialect =
            lash_core::GoogleDialect::Gemini3;
        gemini3_off.model.metadata_mut().capability.reasoning =
            Some(lash_core::provider::ReasoningCapability {
                efforts: vec!["high".to_string()],
                disable: true,
                ..lash_core::provider::ReasoningCapability::default()
            });
        gemini3_off.model.reasoning = lash_core::provider::ReasoningSelection::Disabled;
        for (label, req, code) in [
            ("pinned", pinned, "lash:unsupported_generation_option"),
            ("parallel", parallel, "lash:unsupported_generation_option"),
            ("effort", effort, "lash:effort_not_configurable"),
            (
                "gemini3 off",
                gemini3_off,
                "lash:reasoning_encoding_unrepresentable",
            ),
        ] {
            let transport = Arc::new(ProjectResolutionTransport {
                calls: AtomicUsize::new(0),
            });
            let mut provider = GoogleOAuthProvider::new(std::sync::Arc::new(
                lash_core::provider::ProviderToken::new("access"),
            ))
            .with_transport(transport.clone());
            let error = provider
                .complete(
                    req,
                    &lash_core::provider::NoSlotDeliveries,
                    &lash_core::provider::LiveCallHorizon::fixture(),
                )
                .await
                .expect_err(label);
            assert_eq!(
                error.code.as_ref().map(ToString::to_string).as_deref(),
                Some(code),
                "{label}"
            );
            assert_eq!(transport.calls.load(Ordering::SeqCst), 0, "{label}");
            assert!(provider.project_id.is_none(), "{label} resolved a project");
        }
    }

    /// Lash runs every model call on a fresh copy of the turn's provider.
    /// A copy taken before the first call still reuses the project that call
    /// resolved, so the lookup runs once per provider, not once per call.
    #[tokio::test]
    async fn every_copy_of_a_provider_reuses_the_project_one_call_resolved() {
        let transport = Arc::new(ProjectResolutionTransport {
            calls: AtomicUsize::new(0),
        });
        let provider = GoogleOAuthProvider::new(std::sync::Arc::new(
            lash_core::provider::ProviderToken::new("access"),
        ))
        .with_transport(transport.clone());

        for _ in 0..2 {
            let mut call_copy = provider.clone();
            let response = call_copy
                .complete(
                    completion_request(),
                    &lash_core::provider::NoSlotDeliveries,
                    &lash_core::provider::LiveCallHorizon::fixture(),
                )
                .await
                .expect("credentialed completion succeeds");
            assert_eq!(response.full_text(), "done");
            assert_eq!(call_copy.project_id.as_deref(), Some("resolved-project"));
        }
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            3,
            "one project lookup and two completions"
        );
    }

    #[tokio::test]
    async fn unsupported_retention_is_refused_before_project_resolution() {
        let transport = Arc::new(ProjectResolutionTransport {
            calls: AtomicUsize::new(0),
        });
        let mut provider = GoogleOAuthProvider::new(std::sync::Arc::new(
            lash_core::provider::ProviderToken::new("access"),
        ))
        .with_transport(transport.clone());
        let mut request = completion_request();
        *request.model.metadata_mut().capability.reasoning_retention =
            lash_core::ReasoningRetentionPolicy {
                capability: Some(lash_core::ReasoningRetentionCapability::OpenAiContext {
                    supported: vec![lash_core::OpenAiReasoningContext::CurrentTurn],
                }),
                selection: lash_core::ReasoningRetentionSelection::OpenAiContext {
                    context: lash_core::OpenAiReasoningContext::CurrentTurn,
                },
            };

        let error = provider
            .complete(
                request,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("provider-native retention must be refused");

        assert_eq!(error.kind, ProviderFailureKind::Unsupported);
        assert_eq!(
            error.code.as_ref().map(|code| code.to_string()),
            Some("lash:unsupported_reasoning_retention".to_string())
        );
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            0,
            "retention refusal must precede project-resolution HTTP"
        );
    }
}

#[cfg(test)]
mod token_source_tests {
    use super::*;
    use lash_core::provider::{ProviderToken, TokenError, TokenRequest};
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;

    /// Rotates `token-1` to `token-2` when lash rejects `token-1`.
    #[derive(Debug, Default)]
    struct RotatingHost {
        rotated: Mutex<bool>,
        asks: Mutex<Vec<TokenRequestReason>>,
    }

    #[async_trait::async_trait]
    impl TokenSource for RotatingHost {
        async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
            self.asks.lock_recover().push(request.reason);
            let mut rotated = self.rotated.lock_recover();
            if request.reason == TokenRequestReason::Rejected {
                *rotated = true;
            }
            Ok(ProviderToken::new(if *rotated {
                "token-2"
            } else {
                "token-1"
            }))
        }
    }

    /// Answers 401 to `token-1` and completes for any other token.
    #[derive(Debug, Default)]
    struct RejectsFirstToken {
        authorizations: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl LlmHttpTransport for RejectsFirstToken {
        async fn send(
            &self,
            request: LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
            let authorization = first_header_value(&request.headers, "authorization")
                .unwrap_or_default()
                .to_string();
            let rejected = authorization == "Bearer token-1";
            self.authorizations.lock_recover().push(authorization);
            Ok(lash_llm_transport::LlmHttpResponse {
                status: if rejected { 401 } else { 200 },
                headers: Vec::new(),
                body: lash_llm_transport::LlmHttpBody::buffered(if rejected {
                    r#"{"error":{"code":401,"status":"UNAUTHENTICATED"}}"#
                } else {
                    r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}"#
                }),
            })
        }
    }

    #[tokio::test]
    async fn a_pre_output_401_on_send_asks_the_host_once_and_resends() {
        let host = Arc::new(RotatingHost::default());
        let transport = Arc::new(RejectsFirstToken::default());
        let mut provider = GoogleOAuthProvider::new(host.clone())
            .with_project_id(Some("project".to_string()))
            .with_transport(transport.clone());

        let response = provider
            .complete(
                super::error_detail_tests::completion_request(),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect("the resend with the fresh token completes");

        assert_eq!(response.full_text(), "done");
        assert_eq!(
            *transport.authorizations.lock_recover(),
            vec!["Bearer token-1", "Bearer token-2"]
        );
        assert_eq!(
            *host.asks.lock_recover(),
            vec![
                TokenRequestReason::Current,
                TokenRequestReason::Current,
                TokenRequestReason::Rejected,
            ],
            "lower asks once, send asks once and once more after the 401"
        );
    }
}
