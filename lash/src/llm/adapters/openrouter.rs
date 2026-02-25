use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::llm::adapters::streaming::stream_chunk_timeout;
use crate::llm::transport::{LlmTransport, LlmTransportError};
use crate::llm::types::{
    LlmMessage, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, LlmUsage, ModelSelection,
};
use crate::provider::Provider;

pub struct OpenRouterAdapter {
    client: reqwest::Client,
}

impl Default for OpenRouterAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenRouterAdapter {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn map_role(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
        }
    }

    fn message_to_json(&self, msg: &LlmMessage, req: &LlmRequest) -> Value {
        if msg.kind == "image"
            && msg.image_idx >= 0
            && let Some(att) = req.attachments.get(msg.image_idx as usize)
        {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let data_url = format!("data:{};base64,{}", att.mime, b64);
            return json!({
                "role": Self::map_role(&msg.role),
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": data_url}
                }]
            });
        }

        json!({
            "role": Self::map_role(&msg.role),
            "content": msg.content,
        })
    }

    fn parse_i64(v: Option<&Value>) -> i64 {
        match v {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    }

    fn usage_from_value(value: &Value) -> Option<LlmUsage> {
        let usage = value.get("usage")?;
        Some(LlmUsage {
            input_tokens: Self::parse_i64(usage.get("prompt_tokens")),
            output_tokens: Self::parse_i64(usage.get("completion_tokens")),
            cached_input_tokens: Self::parse_i64(
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .or_else(|| usage.get("cached_prompt_tokens"))
                    .or_else(|| usage.get("cached_tokens")),
            ),
        })
    }

    fn extract_text_parts(value: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Some(choices) = value.get("choices").and_then(|v| v.as_array()) else {
            return out;
        };
        for choice in choices {
            if let Some(text) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
            {
                if !text.is_empty() {
                    out.push(text.to_string());
                }
                continue;
            }
            if let Some(parts) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_array())
            {
                for p in parts {
                    if let Some(text) = p.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        out.push(text.to_string());
                    }
                }
                continue;
            }
            if let Some(text) = choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                if !text.is_empty() {
                    out.push(text.to_string());
                }
                continue;
            }
            if let Some(parts) = choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for p in parts {
                    if let Some(text) = p.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        out.push(text.to_string());
                    }
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
        let raw = raw.trim();
        if raw.is_empty() || raw == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid OpenRouter SSE payload: {e}")).with_raw(raw)
        })?;
        if let Some(err) = event.get("error") {
            return Err(LlmTransportError::new("OpenRouter stream error").with_raw(err.to_string()));
        }
        if let Some(new_usage) = Self::usage_from_value(&event)
            && (new_usage.input_tokens > 0
                || new_usage.output_tokens > 0
                || new_usage.cached_input_tokens > 0)
        {
            *usage = new_usage;
        }
        for piece in Self::extract_text_parts(&event) {
            Self::apply_stream_piece(full, deltas, &piece);
        }
        Ok(())
    }

    fn parse_non_stream_response(raw: &str) -> Result<(String, LlmUsage), LlmTransportError> {
        let value: Value = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid OpenRouter response JSON: {e}"))
                .with_raw(raw.to_string())
        })?;
        let mut full = String::new();
        for piece in Self::extract_text_parts(&value) {
            Self::apply_stream_piece(&mut full, &mut Vec::new(), &piece);
        }
        let usage = Self::usage_from_value(&value).unwrap_or_default();
        Ok((full, usage))
    }
}

#[async_trait]
impl LlmTransport for OpenRouterAdapter {
    fn default_root_model(&self) -> &'static str {
        "anthropic/claude-sonnet-4.6"
    }

    fn default_agent_model(&self, tier: &str) -> Option<ModelSelection> {
        match tier {
            "low" => Some(ModelSelection {
                model: "minimax/minimax-m2.5",
                reasoning_effort: None,
            }),
            "medium" => Some(ModelSelection {
                model: "z-ai/glm-5",
                reasoning_effort: None,
            }),
            "high" => Some(ModelSelection {
                model: "anthropic/claude-sonnet-4.6",
                reasoning_effort: None,
            }),
            _ => None,
        }
    }

    fn normalize_model(&self, model: &str) -> String {
        model.to_string()
    }

    fn context_lookup_model(&self, model: &str) -> String {
        model.to_string()
    }

    async fn ensure_ready(&self, _provider: &mut Provider) -> Result<bool, LlmTransportError> {
        Ok(false)
    }

    async fn complete(
        &self,
        provider: &mut Provider,
        req: LlmRequest,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_events = req.stream_events.clone();
        let (api_key, base_url) = match provider {
            Provider::OpenRouter { api_key, base_url } => (api_key.clone(), base_url.clone()),
            _ => {
                return Err(LlmTransportError::new(
                    "OpenRouter adapter received non-OpenRouter provider",
                ));
            }
        };

        let mut messages = vec![json!({"role": "system", "content": req.system_prompt})];
        messages.extend(req.messages.iter().map(|m| self.message_to_json(m, &req)));

        let body = json!({
            "model": req.model,
            "messages": messages,
            "temperature": 0,
            "max_tokens": 32768,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        let request_body = serde_json::to_string(&body).ok();
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmTransportError::new(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmTransportError {
                message: format!("OpenRouter request failed with {}", status.as_u16()),
                retryable: status.as_u16() == 429 || status.as_u16() >= 500,
                raw: Some(text),
                code: Some(status.as_u16().to_string()),
            });
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);

        if !is_sse {
            let text = resp.text().await.unwrap_or_default();
            let (content, usage) = Self::parse_non_stream_response(&text)?;
            if let Some(tx) = &stream_events {
                if !content.is_empty() {
                    let _ = tx.send(LlmStreamEvent::Delta(content.clone()));
                }
                if usage != LlmUsage::default() {
                    let _ = tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
            }
            return Ok(LlmResponse {
                deltas: vec![content.clone()],
                full_text: content,
                usage,
                request_body,
                http_summary: Some(format!("HTTP POST {}", url)),
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
                .map_err(|_| LlmTransportError::new("OpenRouter stream chunk timed out"))?
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
            deltas,
            full_text: full,
            usage,
            request_body,
            http_summary: Some(format!("HTTP POST {} (stream)", url)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openrouter_sse_deltas_and_usage() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();

        OpenRouterAdapter::process_sse_event(
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();
        OpenRouterAdapter::process_sse_event(
            r#"{"choices":[{"delta":{"content":"lo"}}],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();

        assert_eq!(full, "Hello");
        assert_eq!(deltas, vec!["Hel".to_string(), "lo".to_string()]);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cached_input_tokens, 4);
    }
}
