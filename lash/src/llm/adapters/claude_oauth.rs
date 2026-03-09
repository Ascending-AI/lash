use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::llm::adapters::streaming::stream_chunk_timeout;
use crate::llm::transport::{LlmTransport, LlmTransportError};
use crate::llm::types::{
    LlmMessage, LlmOutputPart, LlmReplayChunk, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent,
    LlmToolCall, LlmUsage, ModelSelection, coalesce_replay_messages,
};
use crate::provider::Provider;

#[derive(Clone, Debug, Default)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: String,
}

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
        if msg.kind == "tool_call" {
            let input = serde_json::from_str::<Value>(&msg.content).unwrap_or_else(|_| json!({}));
            return json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": msg.tool_call_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "name": msg.tool_name.clone().unwrap_or_default(),
                    "input": input,
                }]
            });
        }

        if msg.kind == "tool_result" {
            return json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id,
                    "content": [{ "type": "text", "text": msg.content }],
                }]
            });
        }

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

    fn assistant_tool_calls_json(text: Option<&str>, tool_calls: &[LlmToolCall]) -> Value {
        let mut content = Vec::new();
        if let Some(text) = text
            && !text.is_empty()
        {
            content.push(json!({
                "type": "text",
                "text": text,
            }));
        }
        content.extend(tool_calls.iter().map(|msg| {
            let input =
                serde_json::from_str::<Value>(&msg.input_json).unwrap_or_else(|_| json!({}));
            json!({
                "type": "tool_use",
                "id": if msg.call_id.is_empty() { uuid::Uuid::new_v4().to_string() } else { msg.call_id.clone() },
                "name": msg.tool_name.clone(),
                "input": input,
            })
        }));
        json!({
            "role": "assistant",
            "content": content,
        })
    }

    fn user_tool_results_json(tool_results: &[&LlmMessage]) -> Value {
        json!({
            "role": "user",
            "content": tool_results
                .iter()
                .map(|msg| json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id,
                    "content": [{ "type": "text", "text": msg.content }],
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn build_messages(req: &LlmRequest) -> Vec<Value> {
        let mut messages = Vec::new();
        for chunk in coalesce_replay_messages(&req.messages) {
            match chunk {
                LlmReplayChunk::Message(msg) => messages.push(Self::message_to_json(&msg, req)),
                LlmReplayChunk::AssistantToolCalls { text, tool_calls } => {
                    messages.push(Self::assistant_tool_calls_json(
                        text.as_deref(),
                        &tool_calls,
                    ));
                }
                LlmReplayChunk::ToolResults { results } => {
                    messages.push(Self::user_tool_results_json(
                        &results.iter().collect::<Vec<_>>(),
                    ));
                }
            }
        }
        messages
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

    #[cfg(test)]
    fn process_sse_event(
        raw: &str,
        full: &mut String,
        deltas: &mut Vec<String>,
        usage: &mut LlmUsage,
    ) -> Result<(), LlmTransportError> {
        Self::process_sse_event_with_tools(raw, full, deltas, usage, None)
    }

    fn process_sse_event_with_tools(
        raw: &str,
        full: &mut String,
        deltas: &mut Vec<String>,
        usage: &mut LlmUsage,
        tool_calls: Option<&mut Vec<StreamingToolCall>>,
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
                if let Some(tool_calls) = tool_calls
                    && let Some(partial) = event
                        .get("delta")
                        .filter(|d| {
                            d.get("type").and_then(|t| t.as_str()) == Some("input_json_delta")
                        })
                        .and_then(|d| d.get("partial_json"))
                        .and_then(|p| p.as_str())
                {
                    let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    if index < tool_calls.len() {
                        tool_calls[index].arguments.push_str(partial);
                    }
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
                if let Some(tool_calls) = tool_calls
                    && let Some(cb) = event
                        .get("content_block")
                        .filter(|cb| cb.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                {
                    let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    while tool_calls.len() <= index {
                        tool_calls.push(StreamingToolCall::default());
                    }
                    if let Some(id) = cb.get("id").and_then(|v| v.as_str()) {
                        tool_calls[index].id = id.to_string();
                    }
                    if let Some(name) = cb.get("name").and_then(|v| v.as_str()) {
                        tool_calls[index].name = name.to_string();
                    }
                }
            }
            "message_start" | "message_delta" | "message_stop" | "content_block_stop" => {}
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

    fn response_parts_from_value(value: &Value) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        let Some(content) = value.get("content").and_then(|c| c.as_array()) else {
            return parts;
        };
        for block in content {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str())
                        && !text.is_empty()
                    {
                        parts.push(LlmOutputPart::Text {
                            text: text.to_string(),
                        });
                    }
                }
                "tool_use" => {
                    let Some(name) = block.get("name").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let input_json = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    parts.push(LlmOutputPart::ToolCall {
                        call_id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        tool_name: name.to_string(),
                        input_json,
                    });
                }
                _ => {}
            }
        }
        parts
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

        let messages = Self::build_messages(&req);

        let mut body = json!({
            "model": req.model,
            "system": [{
                "type": "text",
                "text": req.system_prompt,
                "cache_control": { "type": "ephemeral" }
            }],
            "messages": messages,
            "max_tokens": 32768,
            "temperature": 0,
            "stream": stream_events.is_some(),
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|tool| json!({
                        "name": tool.name.clone(),
                        "description": tool.description.clone(),
                        "input_schema": tool.input_schema.clone(),
                    }))
                    .collect::<Vec<_>>()
            );
            body["tool_choice"] = match req.tool_choice {
                crate::llm::types::LlmToolChoice::Auto => json!({"type": "auto"}),
                crate::llm::types::LlmToolChoice::None => json!({"type": "none"}),
                crate::llm::types::LlmToolChoice::Required => json!({"type": "any"}),
            };
        }

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
            let mut parts = Self::response_parts_from_value(&value);
            if parts.is_empty() && !content.is_empty() {
                parts.push(LlmOutputPart::Text {
                    text: content.clone(),
                });
            }
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
                parts,
                usage,
                request_body,
                http_summary: Some(format!("HTTP POST {}", url)),
            });
        }

        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut streaming_tool_calls: Vec<StreamingToolCall> = Vec::new();
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
                        Self::process_sse_event_with_tools(
                            &raw,
                            &mut full,
                            &mut deltas,
                            &mut usage,
                            Some(&mut streaming_tool_calls),
                        )?;
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
            Self::process_sse_event_with_tools(
                &raw,
                &mut full,
                &mut deltas,
                &mut usage,
                Some(&mut streaming_tool_calls),
            )?;
            if let Some(tx) = &stream_events {
                for piece in deltas.iter().skip(prev_len) {
                    let _ = tx.send(LlmStreamEvent::Delta(piece.clone()));
                }
                if usage != prev_usage && usage != LlmUsage::default() {
                    let _ = tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
            }
        }

        let mut parts = Vec::new();
        if !full.is_empty() {
            parts.push(LlmOutputPart::Text { text: full.clone() });
        }
        for tc in &streaming_tool_calls {
            if !tc.id.is_empty() && !tc.name.is_empty() {
                parts.push(LlmOutputPart::ToolCall {
                    call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    input_json: tc.arguments.clone(),
                });
            }
        }

        Ok(LlmResponse {
            deltas,
            full_text: full,
            parts,
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

    #[test]
    fn streaming_accumulates_tool_calls() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut tool_calls = Vec::new();

        // content_block_start with tool_use
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_abc","name":"read_file"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_abc");
        assert_eq!(tool_calls[0].name, "read_file");

        // First input_json_delta
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();

        // Second input_json_delta
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();

        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert!(full.is_empty());
    }

    #[test]
    fn streaming_text_and_tool_calls() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut tool_calls = Vec::new();

        // Text block at index 0
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me read that."}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();

        // Tool block at index 1
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xyz","name":"read_file"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();
        ClaudeOAuthAdapter::process_sse_event_with_tools(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b.rs\"}"}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();

        assert_eq!(full, "Let me read that.");
        assert_eq!(tool_calls.len(), 2);
        // Index 0 is empty (text block, not tool)
        assert!(tool_calls[0].id.is_empty());
        assert_eq!(tool_calls[1].id, "toolu_xyz");
        assert_eq!(tool_calls[1].name, "read_file");
        assert_eq!(tool_calls[1].arguments, r#"{"path":"b.rs"}"#);
    }

    #[test]
    fn build_messages_groups_multi_tool_turns_and_results() {
        let req = LlmRequest {
            model: "claude-sonnet".to_string(),
            system_prompt: "sys".to_string(),
            messages: vec![
                LlmMessage {
                    role: LlmRole::Assistant,
                    content: "Checking both files.".to_string(),
                    kind: "text".to_string(),
                    image_idx: -1,
                    tool_call_id: None,
                    tool_name: None,
                },
                LlmMessage {
                    role: LlmRole::Assistant,
                    content: r#"{"path":"a.rs"}"#.to_string(),
                    kind: "tool_call".to_string(),
                    image_idx: -1,
                    tool_call_id: Some("call_a".to_string()),
                    tool_name: Some("read_file".to_string()),
                },
                LlmMessage {
                    role: LlmRole::Assistant,
                    content: r#"{"path":"b.rs"}"#.to_string(),
                    kind: "tool_call".to_string(),
                    image_idx: -1,
                    tool_call_id: Some("call_b".to_string()),
                    tool_name: Some("read_file".to_string()),
                },
                LlmMessage {
                    role: LlmRole::User,
                    content: "file a".to_string(),
                    kind: "tool_result".to_string(),
                    image_idx: -1,
                    tool_call_id: Some("call_a".to_string()),
                    tool_name: Some("read_file".to_string()),
                },
                LlmMessage {
                    role: LlmRole::User,
                    content: "file b".to_string(),
                    kind: "tool_result".to_string(),
                    image_idx: -1,
                    tool_call_id: Some("call_b".to_string()),
                    tool_name: Some("read_file".to_string()),
                },
            ],
            attachments: vec![],
            tools: vec![],
            tool_choice: crate::llm::types::LlmToolChoice::Auto,
            reasoning_effort: None,
            session_id: None,
            stream_events: None,
        };

        let messages = ClaudeOAuthAdapter::build_messages(&req);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"].as_array().map(Vec::len), Some(3));
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"].as_array().map(Vec::len), Some(2));
    }
}
