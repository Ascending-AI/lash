use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::llm::adapters::streaming::stream_chunk_timeout;
use crate::llm::transport::{LlmTransport, LlmTransportError};
use crate::llm::types::{
    LlmMessage, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, LlmUsage, ModelSelection,
};
use crate::provider::Provider;

pub struct ClaudeOAuthAdapter {
    client: reqwest::Client,
}

impl Default for ClaudeOAuthAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeOAuthAdapter {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn claude_role(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::Assistant => "assistant",
            LlmRole::User | LlmRole::System => "user",
        }
    }

    fn message_to_json(msg: &LlmMessage, req: &LlmRequest) -> Value {
        if msg.kind == "image"
            && msg.image_idx >= 0
            && let Some(att) = req.attachments.get(msg.image_idx as usize)
        {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            return json!({
                "role": Self::claude_role(&msg.role),
                "content": [{
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": att.mime,
                        "data": b64,
                    }
                }]
            });
        }

        json!({
            "role": Self::claude_role(&msg.role),
            "content": [{"type": "text", "text": msg.content}],
        })
    }

    fn parse_i64(v: Option<&Value>) -> i64 {
        match v {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    }

    fn usage_from_value(value: &Value) -> LlmUsage {
        LlmUsage {
            input_tokens: Self::parse_i64(value.get("input_tokens")),
            output_tokens: Self::parse_i64(value.get("output_tokens")),
            cached_input_tokens: Self::parse_i64(
                value
                    .get("cache_read_input_tokens")
                    .or_else(|| value.get("cached_input_tokens"))
                    .or_else(|| value.get("cached_tokens")),
            ),
        }
    }

    fn merge_usage(dst: &mut LlmUsage, next: &LlmUsage) {
        if next.input_tokens > 0 {
            dst.input_tokens = next.input_tokens;
        }
        if next.output_tokens > 0 {
            dst.output_tokens = next.output_tokens;
        }
        if next.cached_input_tokens > 0 {
            dst.cached_input_tokens = next.cached_input_tokens;
        }
    }

    fn apply_stream_piece(full: &mut String, deltas: &mut Vec<String>, piece: &str) {
        if piece.is_empty() {
            return;
        }
        full.push_str(piece);
        deltas.push(piece.to_string());
    }

    fn text_from_message_content(message: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Some(content) = message.get("content").and_then(|c| c.as_array()) else {
            return out;
        };
        for block in content {
            if let Some(text) = block.get("text").and_then(|t| t.as_str())
                && !text.is_empty()
            {
                out.push(text.to_string());
            }
        }
        out
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
            LlmTransportError::new(format!("Invalid Claude SSE payload: {e}")).with_raw(raw)
        })?;
        if event.get("type").and_then(|t| t.as_str()) == Some("error") {
            let msg = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Claude stream error");
            return Err(LlmTransportError::new(msg).with_raw(event.to_string()));
        }

        if let Some(message_usage) = event
            .get("message")
            .and_then(|m| m.get("usage"))
            .map(Self::usage_from_value)
        {
            Self::merge_usage(usage, &message_usage);
        }
        if let Some(delta_usage) = event.get("usage").map(Self::usage_from_value) {
            Self::merge_usage(usage, &delta_usage);
        }

        match event.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "content_block_delta" => {
                if let Some(text) = event
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    Self::apply_stream_piece(full, deltas, text);
                }
            }
            "content_block_start" => {
                if let Some(text) = event
                    .get("content_block")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    Self::apply_stream_piece(full, deltas, text);
                }
            }
            "message_start" | "message_delta" | "message_stop" => {}
            _ => {
                if let Some(text) = event
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    Self::apply_stream_piece(full, deltas, text);
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl LlmTransport for ClaudeOAuthAdapter {
    fn default_root_model(&self) -> &'static str {
        "claude-opus-4-6"
    }

    fn default_agent_model(&self, tier: &str) -> Option<ModelSelection> {
        match tier {
            "low" => Some(ModelSelection {
                model: "claude-haiku-4-5",
                reasoning_effort: None,
            }),
            "medium" | "high" => Some(ModelSelection {
                model: "claude-sonnet-4-6",
                reasoning_effort: None,
            }),
            _ => None,
        }
    }

    fn normalize_model(&self, model: &str) -> String {
        model
            .strip_prefix("anthropic/")
            .unwrap_or(model)
            .to_string()
    }

    fn context_lookup_model(&self, model: &str) -> String {
        if model.contains('/') {
            model.to_string()
        } else {
            format!("anthropic/{model}")
        }
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
        let access_token = match provider {
            Provider::Claude { access_token, .. } => access_token.clone(),
            _ => {
                return Err(LlmTransportError::new(
                    "Claude adapter received non-Claude provider",
                ));
            }
        };

        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| Self::message_to_json(m, &req))
            .collect();

        let body = json!({
            "model": req.model,
            "system": [{
                "type": "text",
                "text": req.system_prompt,
                "cache_control": { "type": "ephemeral" }
            }],
            "messages": messages,
            "max_tokens": 32768,
            "temperature": 0,
            "stream": true,
        });

        let request_body = serde_json::to_string(&body).ok();
        let url = "https://api.anthropic.com/v1/messages".to_string();
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("anthropic-version", "2023-06-01")
            .header(
                "anthropic-beta",
                "oauth-2025-04-20,interleaved-thinking-2025-05-14,prompt-caching-2024-07-31",
            )
            .header("authorization", format!("Bearer {}", access_token))
            .header("x-api-key", "")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmTransportError::new(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmTransportError {
                message: format!("Claude request failed with {}", status.as_u16()),
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
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid Claude response JSON: {e}"))
                    .with_raw(text.clone())
            })?;
            let content = Self::text_from_message_content(&value).join("");
            let usage = value
                .get("usage")
                .map(Self::usage_from_value)
                .unwrap_or_default();
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
                .map_err(|_| LlmTransportError::new("Claude stream chunk timed out"))?
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
    fn parses_claude_sse_usage_and_text() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();

        ClaudeOAuthAdapter::process_sse_event(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":80}}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();
        ClaudeOAuthAdapter::process_sse_event(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();
        ClaudeOAuthAdapter::process_sse_event(
            r#"{"type":"message_delta","usage":{"output_tokens":12}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();

        assert_eq!(full, "Hello");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.cached_input_tokens, 80);
    }
}
