//! The [`Provider`] trait implementation: config serialization, the `complete`
//! orchestration (attachment prep, request build, inline-fallback retry), the
//! request/stream executor, and project-id resolution.

use crate::support::*;
use std::sync::Arc;

struct GoogleCredentialCallContext<'a> {
    provider: &'a mut GoogleOAuthProvider,
    request: &'a LlmRequest,
}

impl GoogleOAuthProvider {
    fn should_retry_inline(err: &LlmTransportError) -> bool {
        matches!(err.http_status, Some(400 | 404))
            || err.raw.as_deref().is_some_and(|raw| {
                raw.contains("fileData") || raw.contains("fileUri") || raw.contains("file_uri")
            })
    }

    /// Which of the caller's generation options a Cloud Code request carries.
    ///
    /// `generationConfig` expresses every supported generation control:
    /// `maxOutputTokens` is always written, while sampling controls and stop
    /// sequences are written whenever the caller sets them.
    pub(crate) fn generation_disposition(req: &LlmRequest) -> GenerationReceipt {
        GenerationReceipt {
            output_token_cap: GenerationOptionOutcome::applied(
                req.generation.output_token_cap.is_some(),
            ),
            temperature: GenerationOptionOutcome::applied(req.generation.temperature.is_some()),
            seed: GenerationOptionOutcome::applied(req.generation.seed.is_some()),
            stop_sequences: GenerationOptionOutcome::applied(
                !req.generation.stop_sequences.is_empty(),
            ),
            // Cloud Code reports cached-token usage, but Lash emits no
            // prompt-cache directive in this request dialect.
            cache: lash_llm_transport::cache_intent_disposition(req, false),
        }
    }

    pub(crate) async fn execute_request(
        &self,
        access_token: &str,
        request: Value,
        stream_events: Option<lash_core::llm::types::LlmEventSender>,
        provider_trace: Option<lash_core::llm::types::LlmProviderTraceSender>,
        stream_termination: StreamTermination,
        generation_disposition: Option<GenerationReceipt>,
    ) -> Result<LlmResponse, LlmTransportError> {
        let request_body_bytes = serde_json::to_vec(&request).map_err(|err| {
            LlmTransportError::new(format!("Failed to serialize Cloud Code body: {err}"))
                .with_kind(lash_core::ProviderFailureKind::Validation)
        })?;
        let request_body = Some(String::from_utf8_lossy(&request_body_bytes).into_owned());
        let method = if stream_events.is_some() {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        emit_provider_request_trace(
            provider_trace.as_ref(),
            "google",
            method,
            &request_body_bytes,
        );
        let mut url = self.method_url(method);
        if stream_events.is_some() {
            url.push_str("?alt=sse");
        }
        let http_request = LlmHttpRequest::post(url.clone(), request_body_bytes)
            .with_header("Authorization", format!("Bearer {access_token}"))
            .with_header("Content-Type", "application/json")
            .with_body_for_error(request_body.clone().unwrap_or_default())
            .with_response_start_timeout_message("Cloud Code response start timed out");
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
                timeouts.request_timeout,
                "Cloud Code response body timed out",
            )
            .await
            .unwrap_or_default();
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
            ResponseMetadataCapture::from_response(&self.options, &resp.headers);
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
            let origin_model = request.get("model").and_then(Value::as_str);
            let parts = self.response_parts_from_value(&value, origin_model);
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
                    .with_adapter_code(TurnFailureCode::from_wire(error.code()))
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
                expose_thinking: Some(self.options.expose_thinking),
            });
        }

        let mut stream_state = GoogleStreamState::default();
        stream_state.expose_thinking = self.options.expose_thinking;
        stream_state.execution_evidence =
            provider_request_id.map(|provider_request_id| ExecutionEvidence {
                provider_request_id: Some(provider_request_id),
                ..Default::default()
            });
        let origin_model = request
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
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
                    && self.options.expose_thinking
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
            if self.options.expose_thinking {
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
                    .with_adapter_code(TurnFailureCode::StreamEndedBeforeFinishReason)
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
                .with_header("Authorization", format!("Bearer {access_token}"))
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
                self.options.llm_timeouts().request_timeout,
                "Cloud Code loadCodeAssist body timed out",
            )
            .await
            .unwrap_or_default();
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

    async fn complete_with_credential(
        &mut self,
        req: LlmRequest,
        credential: Lease<GoogleCredential>,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let stream_termination = req
            .model_capability
            .stream_termination
            .unwrap_or(self.stream_termination);
        let GoogleCredential {
            access_token,
            refresh_token,
            ..
        } = credential.value;
        // The single deliberate exposure point: from here the plaintext only
        // feeds request headers and the upload path.
        let access_token = access_token.into_inner();
        let refresh_token = refresh_token.into_inner();
        if self.project_id.is_none() {
            self.project_id = self.resolve_project_id(&access_token).await?;
        }
        let project_id = self.project_id.clone();

        let inline_attachment_parts = req
            .attachments()
            .iter()
            .map(|source| {
                (
                    (*source).clone(),
                    Self::inline_attachment_part(&req, source),
                )
            })
            .collect::<Vec<_>>();
        let inline_contents =
            self.build_contents_with_attachment_parts(&req, &inline_attachment_parts)?;

        let (attachment_parts, uploaded_keys) = self
            .prepare_attachment_parts(&access_token, &refresh_token, project_id.as_deref(), &req)
            .await?;
        let contents = if uploaded_keys.is_empty() {
            inline_contents.clone()
        } else {
            self.build_contents_with_attachment_parts(&req, &attachment_parts)?
        };

        let request = Self::build_request(self, &req, contents, project_id.as_deref())?;
        let generation_disposition = Some(Self::generation_disposition(&req));

        match self
            .execute_request(
                &access_token,
                request,
                stream_events.clone(),
                provider_trace.clone(),
                stream_termination,
                generation_disposition,
            )
            .await
        {
            Ok(response) => Ok(response),
            Err(err) if !uploaded_keys.is_empty() && Self::should_retry_inline(&err) => {
                // The error does not name which file reference the API rejected,
                // so every cached URI this request relied on is suspect; evict
                // them all rather than re-attempting a dead URI on the next
                // request.
                {
                    let mut cache = Self::uploaded_attachment_cache().lock().await;
                    for key in &uploaded_keys {
                        cache.remove(key);
                    }
                }
                let inline_request =
                    Self::build_request(self, &req, inline_contents, project_id.as_deref())?;
                self.execute_request(
                    &access_token,
                    inline_request,
                    stream_events,
                    provider_trace,
                    stream_termination,
                    generation_disposition,
                )
                .await
            }
            Err(err) => Err(err),
        }
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

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let credential = self.credentials.snapshot();
        let mut map = serde_json::Map::new();
        map.insert(
            "access_token".to_string(),
            serde_json::Value::String(credential.access_token.expose_secret().to_string()),
        );
        map.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(credential.refresh_token.expose_secret().to_string()),
        );
        map.insert(
            "expires_at".to_string(),
            serde_json::Value::Number(credential.expires_at.into()),
        );
        map.insert(
            "oauth_client_id".to_string(),
            serde_json::Value::String(self.oauth_client.id.clone()),
        );
        map.insert(
            "oauth_client_secret".to_string(),
            serde_json::Value::String(self.oauth_client.secret.expose_secret().to_string()),
        );
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
        if self.stream_termination != StreamTermination::EofTolerated {
            map.insert(
                "stream_termination".to_string(),
                serde_json::to_value(self.stream_termination).unwrap_or(Value::Null),
            );
        }
        serialize_options_tail(&mut map, &self.options);
        serde_json::Value::Object(map)
    }

    async fn complete(&mut self, req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        self.route_identity_for_model(&req.model)
            .validate_endpoint()
            .map_err(|error| {
                LlmTransportError::new(error.to_string())
                    .with_kind(ProviderFailureKind::Validation)
                    .with_adapter_code(TurnFailureCode::InvalidProviderEndpoint)
            })?;
        let req = self.reasoning_retention_safe_request(&req)?.into_owned();
        Self::validate_attachments(&req)?;
        let manager = Arc::clone(&self.credentials);
        let mut context = GoogleCredentialCallContext {
            provider: self,
            request: &req,
        };
        manager
            .execute(&mut context, |context, lease| {
                Box::pin(async move {
                    match context
                        .provider
                        .complete_with_credential(context.request.clone(), lease)
                        .await
                    {
                        Ok(response) => Ok(response),
                        Err(error) if error.http_status == Some(401) => {
                            Err(CredentialCallError::PreOutputAuth(error))
                        }
                        Err(error) => Err(CredentialCallError::Failed(error)),
                    }
                })
            })
            .await
            .map_err(|error| match error {
                CredentialExecuteError::Credential(error) => error.into_transport_error(),
                CredentialExecuteError::Call(error) => error,
                // Unknown failures cannot establish that replay is safe.
                _ => LlmTransportError::new(error.to_string())
                    .with_retry_verdict(TransportRetryVerdict::Forbidden),
            })
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
    struct ApiErrorTransport;

    #[derive(Debug)]
    struct ProjectResolutionTransport {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmHttpTransport for ApiErrorTransport {
        async fn send(
            &self,
            _request: LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 400,
                headers: Vec::new(),
                body: lash_llm_transport::LlmHttpBody::buffered(
                    r#"{"error":{"message":"Gemini API detail"}}"#,
                ),
            })
        }
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
                1 => {
                    assert!(request.url.ends_with(":generateContent"));
                    r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}"#
                }
                _ => panic!("unexpected provider request {attempt}"),
            };
            Ok(lash_llm_transport::LlmHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: lash_llm_transport::LlmHttpBody::buffered(body),
            })
        }
    }

    fn completion_request() -> LlmRequest {
        LlmRequest {
            instructions: None,
            model: "gemini-3.1-pro-preview".to_string(),
            messages: vec![lash_core::llm::types::LlmMessage::text(
                LlmRole::User,
                "hello",
            )],
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::<lash_core::llm::types::LlmToolSpec>::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: Default::default(),
            model_capability: Default::default(),
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

    #[tokio::test]
    async fn generate_content_error_surfaces_api_message() {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            u64::MAX,
            crate::GoogleOAuthClient {
                id: "oauth-client-id".into(),
                secret: "oauth-client-secret".into(),
            },
        )
        .with_transport(Arc::new(ApiErrorTransport));
        let error = provider
            .execute_request(
                "access",
                json!({ "model": "gemini-test" }),
                None,
                None,
                StreamTermination::EofTolerated,
                None,
            )
            .await
            .expect_err("fixture is an HTTP error");
        assert!(error.message.contains("Gemini API detail"));
    }

    #[tokio::test]
    async fn load_code_assist_error_surfaces_api_message() {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            u64::MAX,
            crate::GoogleOAuthClient {
                id: "oauth-client-id".into(),
                secret: "oauth-client-secret".into(),
            },
        )
        .with_transport(Arc::new(ApiErrorTransport));
        let error = provider
            .resolve_project_id("access")
            .await
            .expect_err("fixture is an HTTP error");
        assert!(error.message.contains("Gemini API detail"));
    }

    #[tokio::test]
    async fn complete_retains_resolved_project_on_original_provider() {
        let transport = Arc::new(ProjectResolutionTransport {
            calls: AtomicUsize::new(0),
        });
        let mut provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            u64::MAX,
            crate::GoogleOAuthClient {
                id: "oauth-client-id".into(),
                secret: "oauth-client-secret".into(),
            },
        )
        .with_transport(transport.clone());

        let response = provider
            .complete(completion_request())
            .await
            .expect("credentialed completion succeeds");

        assert_eq!(response.full_text(), "done");
        assert_eq!(provider.project_id.as_deref(), Some("resolved-project"));
        assert_eq!(
            provider.serialize_config()["project_id"],
            json!("resolved-project")
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unsupported_retention_is_refused_before_project_resolution() {
        let transport = Arc::new(ProjectResolutionTransport {
            calls: AtomicUsize::new(0),
        });
        let mut provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            u64::MAX,
            crate::GoogleOAuthClient {
                id: "oauth-client-id".into(),
                secret: "oauth-client-secret".into(),
            },
        )
        .with_transport(transport.clone());
        let mut request = completion_request();
        *request.model_capability.reasoning_retention = lash_core::ReasoningRetentionPolicy {
            capability: Some(lash_core::ReasoningRetentionCapability::OpenAiContext {
                supported: vec![lash_core::OpenAiReasoningContext::CurrentTurn],
            }),
            selection: lash_core::ReasoningRetentionSelection::OpenAiContext {
                context: lash_core::OpenAiReasoningContext::CurrentTurn,
            },
        };

        let error = provider
            .complete(request)
            .await
            .expect_err("provider-native retention must be refused");

        assert_eq!(error.kind, ProviderFailureKind::Unsupported);
        assert_eq!(
            error.code.as_ref().map(|code| code.to_string()),
            Some("adapter:unsupported_reasoning_retention".to_string())
        );
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            0,
            "retention refusal must precede project-resolution HTTP"
        );
    }
}
