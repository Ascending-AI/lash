//! The [`Provider`] trait implementation: config serialization, the `complete`
//! orchestration (attachment prep, request build, inline-fallback retry), the
//! request/stream executor, and project-id resolution.

use crate::support::*;
use std::sync::Arc;

impl GoogleOAuthProvider {
    fn should_retry_inline(err: &LlmTransportError) -> bool {
        matches!(err.code.as_deref(), Some("400" | "404"))
            || err.raw.as_deref().is_some_and(|raw| {
                raw.contains("fileData") || raw.contains("fileUri") || raw.contains("file_uri")
            })
    }

    /// Which of the caller's generation options a Cloud Code request carries.
    ///
    /// `generationConfig` expresses all three — `maxOutputTokens` is always
    /// written, and both sampling controls are written whenever the caller set
    /// them — so nothing this runtime can ask for is dropped on this wire.
    pub(crate) fn generation_disposition(req: &LlmRequest) -> GenerationDisposition {
        GenerationDisposition {
            output_token_cap: GenerationOptionDisposition::applied(
                req.generation.output_token_cap.is_some(),
            ),
            temperature: GenerationOptionDisposition::applied(req.generation.temperature.is_some()),
            seed: GenerationOptionDisposition::applied(req.generation.seed.is_some()),
        }
    }

    pub(crate) async fn execute_request(
        &self,
        access_token: &str,
        request: Value,
        stream_events: Option<lash_core::llm::types::LlmEventSender>,
        provider_trace: Option<lash_core::llm::types::LlmProviderTraceSender>,
        stream_termination: StreamTermination,
        generation_disposition: Option<GenerationDisposition>,
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
        let mut url = Self::method_url(method);
        if stream_events.is_some() {
            url.push_str("?alt=sse");
        }
        let http_request = LlmHttpRequest::post(url.clone(), request_body_bytes)
            .with_header("Authorization", format!("Bearer {access_token}"))
            .with_header("Content-Type", "application/json")
            .with_body_for_error(request_body.clone().unwrap_or_default())
            .with_response_start_timeout_message("Cloud Code response start timed out");
        let resp = self
            .transport
            .send(
                http_request,
                response_start_timeout(
                    self.options.llm_timeouts().request_timeout,
                    self.options.llm_timeouts().chunk_timeout,
                    stream_events.is_some(),
                ),
            )
            .await?;

        if !resp.is_success() {
            let status = resp.status;
            let headers = resp.headers;
            let body = read_http_body_text(
                resp.body,
                self.options.llm_timeouts().request_timeout,
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

        if stream_events.is_none() {
            let text = read_http_body_text(
                resp.body,
                self.options.llm_timeouts().request_timeout,
                "Cloud Code response body timed out",
            )
            .await?;
            emit_provider_trace(provider_trace.as_ref(), "google", &text);
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid Cloud Code response JSON: {e}"))
                    .with_raw(text.clone())
            })?;
            let origin_model = request.get("model").and_then(Value::as_str);
            let parts = Self::response_parts_from_value(&value, origin_model);
            let full_text = lash_core::facade_support::visible_response_text_from_parts(&parts);
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
            return Ok(LlmResponse {
                full_text,
                parts,
                usage,
                terminal_reason,
                terminal_diagnostic: None,
                provider_usage,
                request_body,
                http_summary: Some(format!("HTTP POST {}", url)),
                execution_evidence: None,
                generation_disposition,
                response_metadata: Default::default(),
            });
        }

        let mut full = String::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage: Option<Value> = None;
        let mut output_parts: Vec<LlmOutputPart> = Vec::new();
        let mut tool_call_parts: Vec<LlmOutputPart> = Vec::new();
        let mut finish_event: Option<Value> = None;
        let origin_model = request
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let stream_result = drive_sse_response(
            resp.body,
            self.options.llm_timeouts().chunk_timeout,
            "Cloud Code stream chunk timed out",
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "google", raw);
                let mut text_deltas = Vec::new();
                let mut reasoning_deltas = Vec::new();
                let prev_usage = usage.clone();
                Self::process_sse_event_with_text_parts(
                    raw,
                    SseTextPartSink {
                        full: &mut full,
                        text_deltas: &mut text_deltas,
                        reasoning_deltas: &mut reasoning_deltas,
                        usage: &mut usage,
                        provider_usage: &mut provider_usage,
                        tool_call_parts: Some(&mut tool_call_parts),
                        output_parts: Some(&mut output_parts),
                        finish_event: &mut finish_event,
                    },
                    origin_model.as_deref(),
                )?;
                if let Some(tx) = stream_events.as_ref()
                    && self.options.expose_thinking
                {
                    for delta in reasoning_deltas {
                        tx.send(LlmStreamEvent::ReasoningDelta(delta));
                    }
                }
                emit_stream_progress(stream_events.as_ref(), text_deltas, &usage, &prev_usage);
                Ok(())
            },
        )
        .await;

        let partial_response = || {
            let mut parts = output_parts.clone();
            if parts
                .iter()
                .filter_map(|part| match part {
                    LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
                .is_empty()
                && !full.is_empty()
            {
                parts.push(LlmOutputPart::Text {
                    text: full.clone(),
                    response_meta: None,
                });
            }
            parts.extend(tool_call_parts.clone());
            LlmResponse {
                full_text: full.clone(),
                parts,
                usage: usage.clone(),
                terminal_reason: LlmTerminalReason::Unknown,
                terminal_diagnostic: None,
                provider_usage: provider_usage.clone(),
                request_body: request_body.clone(),
                http_summary: Some(format!("HTTP POST {url} (stream)")),
                execution_evidence: None,
                generation_disposition,
                response_metadata: Default::default(),
            }
        };
        if let Err(error) = stream_result {
            return Err(error.with_partial_response(partial_response()));
        }
        if stream_termination == StreamTermination::RequireTerminalEvidence
            && finish_event.is_none()
        {
            return Err(
                LlmTransportError::new("Google stream ended without finishReason")
                    .with_kind(ProviderFailureKind::Stream)
                    .with_code("stream_ended_before_finish_reason")
                    .retryable(true)
                    .with_partial_response(partial_response()),
            );
        }

        let mut parts = output_parts;
        if parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .is_empty()
            && !full.is_empty()
        {
            parts.push(LlmOutputPart::Text {
                text: full.clone(),
                response_meta: None,
            });
        }
        parts.extend(tool_call_parts);

        // Mirror the non-streaming path: derive the terminal reason from the
        // last `finishReason` observed across the SSE events. When no event
        // carried one, `terminal_reason_from_value` on a value without a
        // finishReason falls back to ToolUse/Stop from the assembled parts.
        let terminal_reason =
            Self::terminal_reason_from_value(finish_event.as_ref().unwrap_or(&Value::Null), &parts);

        Ok(LlmResponse {
            full_text: full,
            parts,
            usage,
            terminal_reason,
            terminal_diagnostic: None,
            provider_usage,
            request_body,
            http_summary: Some(format!("HTTP POST {}", url)),
            execution_evidence: None,
            generation_disposition,
            response_metadata: Default::default(),
        })
    }

    async fn resolve_project_id(
        &self,
        access_token: &str,
        project_hint: Option<&str>,
    ) -> Result<Option<String>, LlmTransportError> {
        let mut metadata = json!({
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        });
        if let Some(project) = project_hint.filter(|p| !p.trim().is_empty()) {
            metadata["duetProject"] = json!(project);
        }

        let req = json!({
            "cloudaicompanionProject": project_hint,
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
            LlmHttpRequest::post(Self::method_url("loadCodeAssist"), request_body_bytes)
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
            .map(|s| s.to_string())
            .or_else(|| project_hint.map(|s| s.to_string())))
    }
}

#[async_trait]
impl Provider for GoogleOAuthProvider {
    fn kind(&self) -> &'static str {
        "google_oauth"
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
            serde_json::Value::String(credential.access_token),
        );
        map.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(credential.refresh_token),
        );
        map.insert(
            "expires_at".to_string(),
            serde_json::Value::Number(credential.expires_at.into()),
        );
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
        Self::validate_attachments(&req)?;
        if self.attempt_credential.is_none() {
            let manager = Arc::clone(&self.credentials);
            let provider = self.clone();
            let (response, project_id) = manager
                .execute(move |lease| {
                    let mut provider = provider.clone();
                    let req = req.clone();
                    provider.attempt_credential = Some(lease);
                    async move {
                        let result = Box::pin(provider.complete(req)).await;
                        match result {
                            Ok(response) => Ok((response, provider.project_id.clone())),
                            Err(error) if error.status == Some(401) => {
                                Err(CredentialCallError::PreOutputAuth(error))
                            }
                            Err(error) => Err(CredentialCallError::Failed(error)),
                        }
                    }
                })
                .await
                .map_err(|error| match error {
                    CredentialExecuteError::Credential(error) => credential_transport_error(error),
                    CredentialExecuteError::Call(error) => error,
                })?;
            self.project_id = project_id;
            return Ok(response);
        }
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let stream_termination = req
            .model_capability
            .stream_termination
            .unwrap_or(self.stream_termination);
        let credential = self
            .attempt_credential
            .take()
            .expect("credential attempt is configured");
        let GoogleCredential {
            access_token,
            refresh_token,
            ..
        } = credential.value;
        if self.project_id.is_none() {
            let hint = std::env::var("GOOGLE_CLOUD_PROJECT")
                .ok()
                .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok());
            self.project_id = self
                .resolve_project_id(&access_token, hint.as_deref())
                .await?;
        }
        let project_id = self.project_id.clone();

        let inline_attachment_parts = req
            .attachments
            .iter()
            .map(|source| Self::inline_attachment_part(&req, source))
            .collect::<Vec<_>>();
        let inline_contents =
            Self::build_contents_with_attachment_parts(&req, &inline_attachment_parts);

        let (attachment_parts, used_uploaded_files) = self
            .prepare_attachment_parts(&access_token, &refresh_token, project_id.as_deref(), &req)
            .await?;
        let contents = if used_uploaded_files {
            Self::build_contents_with_attachment_parts(&req, &attachment_parts)
        } else {
            inline_contents.clone()
        };

        let request = Self::build_request(self, &req, contents, project_id.as_deref());
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
            Err(err) if used_uploaded_files && Self::should_retry_inline(&err) => {
                let inline_request =
                    Self::build_request(self, &req, inline_contents, project_id.as_deref());
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

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
