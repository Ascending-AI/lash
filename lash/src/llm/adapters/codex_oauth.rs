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

pub struct CodexOAuthAdapter {
    client: reqwest::Client,
}

impl Default for CodexOAuthAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexOAuthAdapter {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn role_name(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
        }
    }

    fn assistant_text_input_item(text: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        })
    }

    fn tool_call_input_item(call: &LlmToolCall) -> Value {
        json!({
            "type": "function_call",
            "call_id": call.call_id,
            "name": call.tool_name,
            "arguments": call.input_json,
        })
    }

    fn message_to_input_item(msg: &LlmMessage, req: &LlmRequest) -> Value {
        if msg.kind == "tool_call" {
            return json!({
                "type": "function_call",
                "call_id": msg.tool_call_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                "name": msg.tool_name.clone().unwrap_or_default(),
                "arguments": msg.content,
            });
        }

        if msg.kind == "tool_result" {
            return json!({
                "type": "function_call_output",
                "call_id": msg.tool_call_id,
                "output": msg.content,
            });
        }

        if msg.kind == "image" && msg.image_idx >= 0 {
            if matches!(msg.role, LlmRole::Assistant) {
                // Codex Responses expects assistant content parts to be output_*.
                // If we ever see an assistant image here, degrade to text instead
                // of sending an invalid input_* part.
                return json!({
                    "role": Self::role_name(&msg.role),
                    "content": [{"type": "output_text", "text": "[assistant image omitted]"}],
                });
            }
            if let Some(att) = req.attachments.get(msg.image_idx as usize) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
                return json!({
                    "role": Self::role_name(&msg.role),
                    "content": [{
                        "type": "input_image",
                        "image_base64": b64,
                        "mime_type": att.mime,
                    }]
                });
            }
        }

        let text_part_type = if matches!(msg.role, LlmRole::Assistant) {
            "output_text"
        } else {
            "input_text"
        };

        json!({
            "role": Self::role_name(&msg.role),
            "content": [{"type": text_part_type, "text": msg.content}],
        })
    }

    fn extract_text(value: &Value) -> String {
        if let Some(s) = value.get("output_text").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        if let Some(arr) = value.get("output").and_then(|v| v.as_array()) {
            let mut out = String::new();
            for item in arr {
                if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                    for part in content {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            out.push_str(text);
                        }
                    }
                }
            }
            return out;
        }
        String::new()
    }

    fn parse_i64(v: Option<&Value>) -> i64 {
        match v {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    }

    fn extract_usage(value: &Value) -> LlmUsage {
        let usage = value.get("usage").unwrap_or(&Value::Null);
        LlmUsage {
            input_tokens: Self::parse_i64(usage.get("input_tokens")),
            output_tokens: Self::parse_i64(usage.get("output_tokens")),
            cached_input_tokens: Self::parse_i64(
                usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .or_else(|| usage.get("cached_input_tokens"))
                    .or_else(|| usage.get("cached_tokens")),
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
        final_response: &mut Option<Value>,
    ) -> Result<(), LlmTransportError> {
        let raw = raw.trim();
        if raw.is_empty() || raw == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid Codex SSE payload: {e}")).with_raw(raw)
        })?;
        if event.get("type").and_then(|t| t.as_str()) == Some("error") {
            let msg = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Codex stream error");
            return Err(LlmTransportError::new(msg).with_raw(event.to_string()));
        }

        if let Some(resp_value) = event.get("response") {
            *final_response = Some(resp_value.clone());
            let u = Self::extract_usage(resp_value);
            Self::merge_usage(usage, &u);
        } else {
            let u = Self::extract_usage(&event);
            Self::merge_usage(usage, &u);
        }

        match event.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                    Self::apply_stream_piece(full, deltas, delta);
                }
            }
            "response.output_text.done" => {
                if let Some(text) = event.get("text").and_then(|t| t.as_str()) {
                    Self::apply_stream_piece(full, deltas, text);
                }
            }
            "response.completed" => {
                if let Some(resp_value) = event.get("response") {
                    let final_text = Self::extract_text(resp_value);
                    Self::apply_stream_piece(full, deltas, &final_text);
                }
            }
            "response.failed" => {
                let msg = event
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Codex response failed");
                return Err(LlmTransportError::new(msg).with_raw(event.to_string()));
            }
            _ => {}
        }
        Ok(())
    }

    fn parse_sse_payload(
        payload: &str,
        full: &mut String,
        deltas: &mut Vec<String>,
        usage: &mut LlmUsage,
        final_response: &mut Option<Value>,
    ) -> Result<(), LlmTransportError> {
        let mut event_lines: Vec<String> = Vec::new();
        for mut line in payload.lines().map(|l| l.to_string()) {
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(data) = line.strip_prefix("data:") {
                event_lines.push(data.trim().to_string());
                continue;
            }
            if line.starts_with("event:") {
                continue;
            }
            if line.trim().is_empty() {
                if !event_lines.is_empty() {
                    let raw = event_lines.join("\n");
                    Self::process_sse_event(&raw, full, deltas, usage, final_response)?;
                    event_lines.clear();
                }
                continue;
            }
        }
        if !event_lines.is_empty() {
            let raw = event_lines.join("\n");
            Self::process_sse_event(&raw, full, deltas, usage, final_response)?;
        }
        Ok(())
    }

    fn looks_like_sse_payload(payload: &str) -> bool {
        let trimmed = payload.trim_start();
        trimmed.starts_with("event:")
            || trimmed.starts_with("data:")
            || payload.contains("\nevent:")
            || payload.contains("\ndata:")
    }

    fn response_parts_from_value(value: &Value) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
            for item in output {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "message" => {
                        if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                            for block in content {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str())
                                    && !text.is_empty()
                                {
                                    parts.push(LlmOutputPart::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let input_json = item
                            .get("arguments")
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| v.to_string())
                            })
                            .unwrap_or_else(|| "{}".to_string());
                        parts.push(LlmOutputPart::ToolCall {
                            call_id: item
                                .get("call_id")
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
        }
        if parts.is_empty()
            && let Some(text) = value.get("output_text").and_then(|v| v.as_str())
            && !text.is_empty()
        {
            parts.push(LlmOutputPart::Text {
                text: text.to_string(),
            });
        }
        parts
    }
}

#[async_trait]
impl LlmTransport for CodexOAuthAdapter {
    fn default_root_model(&self) -> &'static str {
        "gpt-5.4"
    }

    fn default_agent_model(&self, tier: &str) -> Option<ModelSelection> {
        match tier {
            "low" => Some(ModelSelection {
                model: "gpt-5.3-codex-spark",
                reasoning_effort: None,
            }),
            "medium" => Some(ModelSelection {
                model: "gpt-5.4",
                reasoning_effort: Some("medium"),
            }),
            "high" => Some(ModelSelection {
                model: "gpt-5.4",
                reasoning_effort: Some("high"),
            }),
            _ => None,
        }
    }

    fn reasoning_effort_for_model(&self, model: &str) -> Option<&'static str> {
        if matches!(model, "gpt-5.4" | "gpt-5.3-codex") {
            Some("high")
        } else {
            None
        }
    }

    fn normalize_model(&self, model: &str) -> String {
        model.to_string()
    }

    fn context_lookup_model(&self, model: &str) -> String {
        if model.contains('/') {
            model.to_string()
        } else {
            format!("openai/{model}")
        }
    }

    fn requires_streaming(&self) -> bool {
        true
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
        let (access_token, account_id) = match provider {
            Provider::Codex {
                access_token,
                account_id,
                ..
            } => (access_token.clone(), account_id.clone()),
            _ => {
                return Err(LlmTransportError::new(
                    "Codex adapter received non-Codex provider",
                ));
            }
        };

        let mut input = vec![json!({
            "role": "system",
            "content": [{"type": "input_text", "text": req.system_prompt}],
        })];
        for chunk in coalesce_replay_messages(&req.messages) {
            match chunk {
                LlmReplayChunk::Message(msg) => {
                    input.push(Self::message_to_input_item(&msg, &req));
                }
                LlmReplayChunk::AssistantToolCalls { text, tool_calls } => {
                    if let Some(text) = text.as_deref()
                        && !text.is_empty()
                    {
                        input.push(Self::assistant_text_input_item(text));
                    }
                    input.extend(tool_calls.iter().map(Self::tool_call_input_item));
                }
                LlmReplayChunk::ToolResults { results } => {
                    input.extend(
                        results
                            .iter()
                            .map(|msg| Self::message_to_input_item(msg, &req)),
                    );
                }
            }
        }

        let mut body = json!({
            "model": req.model,
            "input": input,
            "stream": stream_events.is_some(),
            "store": false,
            "instructions": "",
        });
        if let Some(effort) = req.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|tool| json!({
                        "type": "function",
                        "name": tool.name.clone(),
                        "description": tool.description.clone(),
                        "parameters": tool.input_schema.clone(),
                    }))
                    .collect::<Vec<_>>()
            );
            body["tool_choice"] = match req.tool_choice {
                crate::llm::types::LlmToolChoice::Auto => json!("auto"),
                crate::llm::types::LlmToolChoice::None => json!("none"),
                crate::llm::types::LlmToolChoice::Required => json!("required"),
            };
            body["parallel_tool_calls"] = json!(true);
        }

        let request_body = serde_json::to_string(&body).ok();
        let url = "https://chatgpt.com/backend-api/codex/responses".to_string();
        let mut http = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("originator", "lash")
            .header(
                "User-Agent",
                format!(
                    "lash/{} ({}; {})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .json(&body);
        if let Some(session_id) = req.session_id.as_deref() {
            http = http.header("session_id", session_id);
        }
        if let Some(id) = account_id {
            http = http.header("ChatGPT-Account-Id", id);
        }

        let resp = http
            .send()
            .await
            .map_err(|e| LlmTransportError::new(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmTransportError {
                message: format!("Codex request failed with {}", status.as_u16()),
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
            if Self::looks_like_sse_payload(&text) {
                let mut full = String::new();
                let mut deltas = Vec::new();
                let mut usage = LlmUsage::default();
                let mut final_response = None;
                Self::parse_sse_payload(
                    &text,
                    &mut full,
                    &mut deltas,
                    &mut usage,
                    &mut final_response,
                )?;
                let parts = final_response
                    .as_ref()
                    .map(Self::response_parts_from_value)
                    .filter(|parts| !parts.is_empty())
                    .unwrap_or_else(|| {
                        if full.is_empty() {
                            Vec::new()
                        } else {
                            vec![LlmOutputPart::Text { text: full.clone() }]
                        }
                    });
                if let Some(tx) = &stream_events {
                    for piece in &deltas {
                        let _ = tx.send(LlmStreamEvent::Delta(piece.clone()));
                    }
                    if usage != LlmUsage::default() {
                        let _ = tx.send(LlmStreamEvent::Usage(usage.clone()));
                    }
                }
                return Ok(LlmResponse {
                    deltas,
                    full_text: full,
                    parts,
                    usage,
                    request_body,
                    http_summary: Some(format!("HTTP POST {} (stream/fallback)", url)),
                });
            }
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid Codex response JSON: {e}"))
                    .with_raw(text.clone())
            })?;
            let content = Self::extract_text(&value);
            let usage = Self::extract_usage(&value);
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
        let mut final_response = None;
        let mut pending = String::new();
        let mut event_lines: Vec<String> = Vec::new();
        let mut resp = resp;
        loop {
            let chunk_opt = tokio::time::timeout(stream_chunk_timeout(), resp.chunk())
                .await
                .map_err(|_| LlmTransportError::new("Codex stream chunk timed out"))?
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
                        Self::process_sse_event(
                            &raw,
                            &mut full,
                            &mut deltas,
                            &mut usage,
                            &mut final_response,
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
            Self::process_sse_event(
                &raw,
                &mut full,
                &mut deltas,
                &mut usage,
                &mut final_response,
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

        let parts = final_response
            .as_ref()
            .map(Self::response_parts_from_value)
            .filter(|parts| !parts.is_empty())
            .unwrap_or_else(|| {
                if full.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmOutputPart::Text { text: full.clone() }]
                }
            });

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
    fn parses_codex_sse_delta_and_completed_usage() {
        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut final_response = None;

        CodexOAuthAdapter::process_sse_event(
            r#"{"type":"response.output_text.delta","delta":"Hi "}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            &mut final_response,
        )
        .unwrap();
        CodexOAuthAdapter::process_sse_event(
            r#"{"type":"response.completed","response":{"output_text":"Hi there","usage":{"input_tokens":30,"output_tokens":8,"input_tokens_details":{"cached_tokens":10}}}}"#,
            &mut full,
            &mut deltas,
            &mut usage,
            &mut final_response,
        )
        .unwrap();

        assert_eq!(full, "Hi there");
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cached_input_tokens, 10);
    }

    #[test]
    fn parses_codex_sse_payload_when_header_missing() {
        let payload = r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"Hey "}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"there"}

event: response.completed
data: {"type":"response.completed","response":{"output_text":"Hey there","usage":{"input_tokens":9,"output_tokens":2,"input_tokens_details":{"cached_tokens":3}}}}
"#;

        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut final_response = None;
        CodexOAuthAdapter::parse_sse_payload(
            payload,
            &mut full,
            &mut deltas,
            &mut usage,
            &mut final_response,
        )
        .unwrap();

        assert_eq!(full, "Hey there");
        assert_eq!(usage.input_tokens, 9);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, 3);
    }

    #[test]
    fn extracts_tool_calls_from_completed_stream_response() {
        let payload = r#"event: response.completed
data: {"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}],"usage":{"input_tokens":12,"output_tokens":3}}}
"#;

        let mut full = String::new();
        let mut deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut final_response = None;
        CodexOAuthAdapter::parse_sse_payload(
            payload,
            &mut full,
            &mut deltas,
            &mut usage,
            &mut final_response,
        )
        .unwrap();

        let parts = final_response
            .as_ref()
            .map(CodexOAuthAdapter::response_parts_from_value)
            .unwrap_or_default();
        assert_eq!(
            parts,
            vec![LlmOutputPart::ToolCall {
                call_id: "call_1".to_string(),
                tool_name: "read_file".to_string(),
                input_json: "{\"path\":\"README.md\"}".to_string(),
            }]
        );
    }

    #[test]
    fn codex_transport_requires_streaming() {
        let adapter = CodexOAuthAdapter::new();
        assert!(LlmTransport::requires_streaming(&adapter));
    }

    #[test]
    fn assistant_messages_use_output_text_content_type() {
        let req = LlmRequest {
            model: "gpt-5.4".to_string(),
            system_prompt: "sys".to_string(),
            messages: vec![],
            attachments: vec![],
            tools: vec![],
            tool_choice: crate::llm::types::LlmToolChoice::None,
            reasoning_effort: None,
            session_id: None,
            stream_events: None,
        };

        let item = CodexOAuthAdapter::message_to_input_item(
            &LlmMessage {
                role: LlmRole::Assistant,
                content: "hello".to_string(),
                kind: "text".to_string(),
                image_idx: -1,
                tool_call_id: None,
                tool_name: None,
            },
            &req,
        );

        assert_eq!(item["role"], "assistant");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "hello");
    }
}
