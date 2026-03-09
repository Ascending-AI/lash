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

pub struct OpenAiGenericAdapter {
    client: reqwest::Client,
}

#[derive(Clone, Debug, Default)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl Default for OpenAiGenericAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiGenericAdapter {
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
        if msg.kind == "tool_call" {
            return json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": msg.tool_call_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "type": "function",
                    "function": {
                        "name": msg.tool_name.clone().unwrap_or_default(),
                        "arguments": msg.content,
                    }
                }]
            });
        }

        if msg.kind == "tool_result" {
            return json!({
                "role": "tool",
                "tool_call_id": msg.tool_call_id,
                "name": msg.tool_name,
                "content": msg.content,
            });
        }

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

    fn assistant_tool_calls_json(text: Option<&str>, tool_calls: &[LlmToolCall]) -> Value {
        json!({
            "role": "assistant",
            "content": text.unwrap_or_default(),
            "tool_calls": tool_calls
                .iter()
                .map(|msg| json!({
                    "id": if msg.call_id.is_empty() { uuid::Uuid::new_v4().to_string() } else { msg.call_id.clone() },
                    "type": "function",
                    "function": {
                        "name": msg.tool_name.clone(),
                        "arguments": msg.input_json,
                    }
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn build_messages(&self, req: &LlmRequest) -> Vec<Value> {
        let mut messages = vec![json!({"role": "system", "content": req.system_prompt})];
        for chunk in coalesce_replay_messages(&req.messages) {
            match chunk {
                LlmReplayChunk::Message(msg) => messages.push(self.message_to_json(&msg, req)),
                LlmReplayChunk::AssistantToolCalls { text, tool_calls } => {
                    messages.push(Self::assistant_tool_calls_json(
                        text.as_deref(),
                        &tool_calls,
                    ));
                }
                LlmReplayChunk::ToolResults { results } => {
                    messages.extend(results.iter().map(|msg| self.message_to_json(msg, req)));
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
        // Accumulate streaming tool call deltas.
        if let Some(tool_calls) = tool_calls
            && let Some(choices) = event.get("choices").and_then(|v| v.as_array())
        {
            for choice in choices {
                let Some(tcs) = choice
                    .get("delta")
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|t| t.as_array())
                else {
                    continue;
                };
                for tc in tcs {
                    let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    while tool_calls.len() <= index {
                        tool_calls.push(StreamingToolCall::default());
                    }
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        tool_calls[index].id = id.to_string();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                            tool_calls[index].name = name.to_string();
                        }
                        if let Some(args) = f.get("arguments").and_then(|a| a.as_str()) {
                            tool_calls[index].arguments.push_str(args);
                        }
                    }
                }
            }
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

    fn response_parts_from_value(value: &Value) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        let Some(choice) = value
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
        else {
            return parts;
        };
        let Some(message) = choice.get("message") else {
            return parts;
        };

        if let Some(text) = message.get("content").and_then(|c| c.as_str())
            && !text.is_empty()
        {
            parts.push(LlmOutputPart::Text {
                text: text.to_string(),
            });
        } else if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
            for item in content {
                if let Some(text) = item.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    parts.push(LlmOutputPart::Text {
                        text: text.to_string(),
                    });
                }
            }
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for tool_call in tool_calls {
                let Some(name) = tool_call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                else {
                    continue;
                };
                let arguments = tool_call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_else(|| "{}".to_string());
                parts.push(LlmOutputPart::ToolCall {
                    call_id: tool_call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    tool_name: name.to_string(),
                    input_json: arguments,
                });
            }
        }

        parts
    }
}

#[async_trait]
impl LlmTransport for OpenAiGenericAdapter {
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
        if model.starts_with("openrouter/") {
            model.to_string()
        } else {
            format!("openrouter/{model}")
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
        let (api_key, base_url) = match provider {
            Provider::OpenAiGeneric { api_key, base_url } => (api_key.clone(), base_url.clone()),
            _ => {
                return Err(LlmTransportError::new(
                    "OpenAI-generic adapter received non-OpenAI-generic provider",
                ));
            }
        };

        let messages = self.build_messages(&req);

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "temperature": 0,
            "max_tokens": 32768,
            "stream": stream_events.is_some(),
        });
        if stream_events.is_some() {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|tool| json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }
                    }))
                    .collect::<Vec<_>>()
            );
            body["tool_choice"] = match req.tool_choice {
                crate::llm::types::LlmToolChoice::Auto => json!("auto"),
                crate::llm::types::LlmToolChoice::None => json!("none"),
                crate::llm::types::LlmToolChoice::Required => json!("required"),
            };
        }

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
                message: format!("OpenAI-generic request failed with {}", status.as_u16()),
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
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid OpenRouter response JSON: {e}"))
                    .with_raw(text.clone())
            })?;
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
    fn parses_openai_generic_sse_deltas_and_usage() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();

        OpenAiGenericAdapter::process_sse_event(
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            &mut full,
            &mut deltas,
            &mut usage,
        )
        .unwrap();
        OpenAiGenericAdapter::process_sse_event(
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

    #[test]
    fn streaming_accumulates_tool_calls() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut tool_calls = Vec::new();

        // First SSE event: tool call start with id and name
        OpenAiGenericAdapter::process_sse_event_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_abc");
        assert_eq!(tool_calls[0].name, "read_file");

        // Second SSE event: argument chunk
        OpenAiGenericAdapter::process_sse_event_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            Some(&mut tool_calls),
        )
        .unwrap();

        // Third SSE event: argument continuation
        OpenAiGenericAdapter::process_sse_event_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
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
    fn build_messages_groups_assistant_multi_tool_turns() {
        let adapter = OpenAiGenericAdapter::new();
        let req = LlmRequest {
            model: "gpt-5.4".to_string(),
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
            ],
            attachments: vec![],
            tools: vec![],
            tool_choice: crate::llm::types::LlmToolChoice::Auto,
            reasoning_effort: None,
            session_id: None,
            stream_events: None,
        };

        let messages = adapter.build_messages(&req);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "Checking both files.");
        assert_eq!(messages[1]["tool_calls"].as_array().map(Vec::len), Some(2));
        assert_eq!(messages[2]["role"], "tool");
    }
}
