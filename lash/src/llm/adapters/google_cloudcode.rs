use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::llm::adapters::streaming::stream_chunk_timeout;
use crate::llm::transport::{LlmTransport, LlmTransportError};
use crate::llm::types::{
    LlmAttachment, LlmMessage, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, LlmUsage,
    ModelSelection,
};
use crate::provider::Provider;

const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const CODE_ASSIST_API_VERSION: &str = "v1internal";

pub struct GoogleCloudCodeAdapter {
    client: reqwest::Client,
}

impl Default for GoogleCloudCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleCloudCodeAdapter {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn endpoint_base_url() -> String {
        let endpoint = std::env::var("CODE_ASSIST_ENDPOINT")
            .unwrap_or_else(|_| CODE_ASSIST_ENDPOINT.to_string());
        let version = std::env::var("CODE_ASSIST_API_VERSION")
            .unwrap_or_else(|_| CODE_ASSIST_API_VERSION.to_string());
        format!("{endpoint}/{version}")
    }

    fn method_url(method: &str) -> String {
        format!("{}:{}", Self::endpoint_base_url(), method)
    }

    fn content_role(role: &LlmRole) -> &'static str {
        if matches!(role, LlmRole::Assistant) {
            "model"
        } else {
            "user"
        }
    }

    fn message_to_content(msg: &LlmMessage, attachments: &[LlmAttachment]) -> Option<Value> {
        if msg.kind == "image"
            && msg.image_idx >= 0
            && let Some(att) = attachments.get(msg.image_idx as usize)
        {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            return Some(json!({
                "role": Self::content_role(&msg.role),
                "parts": [{
                    "inlineData": {
                        "mimeType": att.mime,
                        "data": b64,
                    }
                }]
            }));
        }

        if msg.content.is_empty() {
            return None;
        }

        Some(json!({
            "role": Self::content_role(&msg.role),
            "parts": [{ "text": msg.content }],
        }))
    }

    fn parse_i64(v: Option<&Value>) -> i64 {
        match v {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    }

    fn usage_from_event(event: &Value) -> LlmUsage {
        let meta = event
            .get("response")
            .and_then(|r| r.get("usageMetadata"))
            .unwrap_or(&Value::Null);
        LlmUsage {
            input_tokens: Self::parse_i64(
                meta.get("promptTokenCount")
                    .or_else(|| meta.get("inputTokenCount"))
                    .or_else(|| meta.get("inputTokens")),
            ),
            output_tokens: Self::parse_i64(
                meta.get("candidatesTokenCount")
                    .or_else(|| meta.get("outputTokenCount"))
                    .or_else(|| meta.get("outputTokens")),
            ),
            cached_input_tokens: Self::parse_i64(
                meta.get("cachedContentTokenCount")
                    .or_else(|| meta.get("cachedPromptTokenCount"))
                    .or_else(|| meta.get("cachedInputTokenCount")),
            ),
        }
    }

    fn text_parts_from_event(event: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Some(candidates) = event
            .get("response")
            .and_then(|r| r.get("candidates"))
            .and_then(|c| c.as_array())
        else {
            return out;
        };

        for candidate in candidates {
            let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    out.push(text.to_string());
                }
            }
        }

        out
    }

    fn apply_stream_piece(full: &mut String, deltas: &mut Vec<String>, piece: &str) {
        if piece.is_empty() {
            return;
        }
        if piece.starts_with(full.as_str()) {
            let delta = &piece[full.len()..];
            if !delta.is_empty() {
                full.push_str(delta);
                deltas.push(delta.to_string());
            }
            return;
        }
        full.push_str(piece);
        deltas.push(piece.to_string());
    }

    fn process_sse_event(
        raw: &str,
        full: &mut String,
        deltas: &mut Vec<String>,
        usage: &mut LlmUsage,
    ) -> Result<(), LlmTransportError> {
        if raw.trim().is_empty() || raw.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(raw)
            .map_err(|e| LlmTransportError::new(format!("Invalid Cloud Code SSE payload: {e}")))?;
        let new_usage = Self::usage_from_event(&event);
        if new_usage.input_tokens > 0
            || new_usage.output_tokens > 0
            || new_usage.cached_input_tokens > 0
        {
            *usage = new_usage;
        }
        for piece in Self::text_parts_from_event(&event) {
            Self::apply_stream_piece(full, deltas, &piece);
        }
        Ok(())
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

        let resp = self
            .client
            .post(Self::method_url("loadCodeAssist"))
            .bearer_auth(access_token)
            .json(&req)
            .send()
            .await
            .map_err(|e| LlmTransportError::new(format!("HTTP request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmTransportError {
                message: format!("Cloud Code loadCodeAssist failed with {}", status),
                retryable: status == 429 || status >= 500,
                raw: Some(body),
                code: Some(status.to_string()),
            });
        }
        let body: Value = resp.json().await.map_err(|e| {
            LlmTransportError::new(format!("Invalid Cloud Code loadCodeAssist JSON: {e}"))
        })?;
        Ok(body
            .get("cloudaicompanionProject")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| project_hint.map(|s| s.to_string())))
    }
}

#[async_trait]
impl LlmTransport for GoogleCloudCodeAdapter {
    fn default_root_model(&self) -> &'static str {
        "gemini-3.1-pro-preview"
    }

    fn default_agent_model(&self, tier: &str) -> Option<ModelSelection> {
        match tier {
            "low" => Some(ModelSelection {
                model: "gemini-3-flash-preview",
                reasoning_effort: None,
            }),
            "medium" | "high" => Some(ModelSelection {
                model: "gemini-3.1-pro-preview",
                reasoning_effort: None,
            }),
            _ => None,
        }
    }

    fn normalize_model(&self, model: &str) -> String {
        model.strip_prefix("google/").unwrap_or(model).to_string()
    }

    fn context_lookup_model(&self, model: &str) -> String {
        if model.contains('/') {
            model.to_string()
        } else {
            format!("google/{model}")
        }
    }

    async fn ensure_ready(&self, provider: &mut Provider) -> Result<bool, LlmTransportError> {
        let Provider::GoogleOAuth {
            access_token,
            project_id,
            ..
        } = provider
        else {
            return Err(LlmTransportError::new(
                "Google Cloud Code adapter received non-Google provider",
            ));
        };

        if project_id.is_none() {
            let hint = std::env::var("GOOGLE_CLOUD_PROJECT")
                .ok()
                .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok());
            let resolved = self
                .resolve_project_id(access_token, hint.as_deref())
                .await?;
            *project_id = resolved;
            return Ok(true);
        }

        Ok(false)
    }

    async fn complete(
        &self,
        provider: &mut Provider,
        req: LlmRequest,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = req.stream_events.clone();
        let (access_token, project_id) = match provider {
            Provider::GoogleOAuth {
                access_token,
                project_id,
                ..
            } => (access_token.clone(), project_id.clone()),
            _ => {
                return Err(LlmTransportError::new(
                    "Google Cloud Code adapter received non-Google provider",
                ));
            }
        };

        let mut contents = Vec::new();
        for msg in &req.messages {
            if let Some(content) = Self::message_to_content(msg, &req.attachments) {
                contents.push(content);
            }
        }

        let mut request = json!({
            "model": req.model,
            "user_prompt_id": uuid::Uuid::new_v4().to_string(),
            "request": {
                "contents": contents,
                "systemInstruction": {
                    "parts": [{ "text": req.system_prompt }],
                },
                "generationConfig": {
                    "temperature": 0,
                    "maxOutputTokens": 32768,
                }
            }
        });
        if let Some(project) = project_id.filter(|p| !p.trim().is_empty()) {
            request["project"] = json!(project);
        }

        let request_body = serde_json::to_string(&request).ok();
        let url = Self::method_url("streamGenerateContent");
        let resp = self
            .client
            .post(&url)
            .query(&[("alt", "sse")])
            .bearer_auth(access_token)
            .json(&request)
            .send()
            .await
            .map_err(|e| LlmTransportError::new(format!("HTTP request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmTransportError {
                message: format!("Cloud Code request failed with {}", status),
                retryable: status == 429 || status >= 500,
                raw: Some(body),
                code: Some(status.to_string()),
            });
        }

        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();

        let mut pending = String::new();
        let mut event_lines: Vec<String> = Vec::new();
        let mut resp = resp;
        loop {
            let chunk_opt = tokio::time::timeout(stream_chunk_timeout(), resp.chunk())
                .await
                .map_err(|_| LlmTransportError::new("Cloud Code stream chunk timed out"))?
                .map_err(|e| LlmTransportError::new(format!("Stream read failed: {e}")))?;
            let Some(chunk) = chunk_opt else { break };
            pending.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = pending.find('\n') {
                let mut line = pending[..pos].to_string();
                pending.drain(..=pos);
                if line.ends_with('\r') {
                    line.pop();
                }

                if let Some(data) = line.strip_prefix("data:") {
                    event_lines.push(data.trim().to_string());
                    continue;
                }
                if line.trim().is_empty() {
                    if !event_lines.is_empty() {
                        let raw = event_lines.join("\n");
                        let prev_len = deltas.len();
                        let prev_usage = usage.clone();
                        Self::process_sse_event(&raw, &mut full, &mut deltas, &mut usage)?;
                        if let Some(tx) = &stream_events {
                            for piece in deltas.iter().skip(prev_len) {
                                let _ = tx.send(LlmStreamEvent::Delta(piece.clone()));
                            }
                            if usage != prev_usage && usage != LlmUsage::default() {
                                let _ = tx.send(LlmStreamEvent::Usage(usage.clone()));
                            }
                        }
                        event_lines.clear();
                    }
                    continue;
                }
            }
        }

        if !pending.trim().is_empty()
            && let Some(data) = pending.trim().strip_prefix("data:")
        {
            event_lines.push(data.trim().to_string());
        }
        if !event_lines.is_empty() {
            let raw = event_lines.join("\n");
            let prev_len = deltas.len();
            let prev_usage = usage.clone();
            Self::process_sse_event(&raw, &mut full, &mut deltas, &mut usage)?;
            if let Some(tx) = &stream_events {
                for piece in deltas.iter().skip(prev_len) {
                    let _ = tx.send(LlmStreamEvent::Delta(piece.clone()));
                }
                if usage != prev_usage && usage != LlmUsage::default() {
                    let _ = tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
            }
        }

        Ok(LlmResponse {
            full_text: full,
            deltas,
            usage,
            request_body,
            http_summary: Some(format!("HTTP POST {}?alt=sse", url)),
        })
    }
}
