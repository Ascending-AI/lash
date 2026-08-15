//! Request-body construction: translating an [`LlmRequest`] into the Cloud
//! Code (Gemini) wire shape (contents, systemInstruction, tools, generation
//! and thinking config), plus the inline-attachment-part helpers.

use crate::support::*;

/// Pi-mono sentinel: Gemini 3 refuses to run when a function_call is
/// replayed without a thoughtSignature. The server recognises this magic
/// string and skips signature validation for the item, so lash can round-
/// trip tool calls captured from non-Gemini models without crashing the
/// turn. Matches `google-shared.ts:51`.
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoogleThinkingConfig {
    Level { level: String },
    Budget { budget_tokens: u32 },
    ToggleFalse,
}

impl GoogleOAuthProvider {
    pub(crate) const PROVIDER_KIND: &'static str = "google_oauth";

    pub(crate) fn inline_attachment_part(req: &LlmRequest, source: &AttachmentSource) -> Value {
        match source {
            AttachmentSource::ProviderFile { id, .. } => {
                json!({"fileData": {"fileUri": id}})
            }
            AttachmentSource::ExternalUrl { .. } => unreachable!("validated as unsupported"),
            AttachmentSource::Inline { .. } | AttachmentSource::Stored { .. } => {
                let media_type = source.media_type().expect("MIME-bearing source");
                let bytes = req
                    .attachment_bytes(source)
                    .expect("validated attachment bytes");
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                json!({
                    "inlineData": {
                        "mimeType": media_type,
                        "data": data,
                    }
                })
            }
        }
    }

    pub(crate) fn validate_attachments(req: &LlmRequest) -> Result<(), LlmTransportError> {
        for source in &req.attachments {
            let supported = match source {
                AttachmentSource::ExternalUrl { .. } => false,
                AttachmentSource::ProviderFile { provider_scope, .. } => matches!(
                    provider_scope.provider.to_ascii_lowercase().as_str(),
                    "google" | "google_oauth" | "gemini"
                ),
                source => source.media_type().is_some_and(|mime| {
                    GOOGLE_IMAGE_MIMES.contains(&mime.as_str())
                        || GOOGLE_MEDIA_FAMILIES.contains(&mime.family())
                        || GOOGLE_FILE_MIMES.contains(&mime.as_str())
                }),
            };
            if !supported {
                let accepted_by = known_attachment_acceptors(source);
                return Err(unsupported_attachment_capability(
                    "Google Gemini",
                    source,
                    &accepted_by,
                ));
            }
            if matches!(source, AttachmentSource::Stored { .. })
                && req.attachment_bytes(source).is_none()
            {
                let mime = source.media_type().expect("stored source MIME");
                return Err(LlmTransportError::new(format!(
                    "Google Gemini could not materialize stored attachment MIME `{mime}` because session-guard resolution did not provide its bytes"
                ))
                .with_kind(ProviderFailureKind::Validation)
                .with_code("stored_attachment_not_resolved"));
            }
        }
        Ok(())
    }

    fn attachment_part_for_index(attachment_parts: &[Value], idx: usize) -> Value {
        attachment_parts
            .get(idx)
            .cloned()
            .unwrap_or_else(|| json!({ "text": "[Attachment]" }))
    }

    fn valid_text_signature(meta: &ResponseTextMeta) -> Option<String> {
        let signature = meta.provider_payload.as_deref()?.trim();
        if signature.is_empty() {
            return None;
        }
        base64::engine::general_purpose::STANDARD
            .decode(signature)
            .ok()
            .filter(|bytes| !bytes.is_empty())?;
        Some(signature.to_string())
    }

    pub(crate) fn build_contents_with_attachment_parts(
        req: &LlmRequest,
        attachment_parts: &[Value],
    ) -> Vec<Value> {
        let serving_route = Self::route_identity_for_model(&req.model);
        let safe_request = req.replay_safe_for(&serving_route);
        let req = safe_request.as_ref();
        let mut out: Vec<Value> = Vec::new();
        // Wire-protocol dialect fact, not a model-catalog capability fact.
        let is_gemini_3 = req.model.to_ascii_lowercase().contains("gemini-3");

        for msg in &req.messages {
            if matches!(msg.role, LlmRole::System) {
                // System content is hoisted into `systemInstruction` on the
                // Gemini request, not the `contents` list.
                continue;
            }
            let role = match msg.role {
                LlmRole::Assistant => "model",
                LlmRole::User | LlmRole::System => "user",
            };

            let mut parts: Vec<Value> = Vec::new();
            for block in msg.blocks.iter() {
                match block {
                    LlmContentBlock::Text {
                        text,
                        response_meta,
                        ..
                    } => {
                        if text.is_empty() {
                            continue;
                        }
                        let mut part = json!({ "text": text });
                        if matches!(msg.role, LlmRole::Assistant)
                            && let Some(signature) =
                                response_meta.as_ref().and_then(Self::valid_text_signature)
                        {
                            part["thoughtSignature"] = Value::String(signature);
                        }
                        parts.push(part);
                    }
                    LlmContentBlock::Attachment { attachment_idx } => {
                        if matches!(msg.role, LlmRole::User) {
                            parts.push(Self::attachment_part_for_index(
                                attachment_parts,
                                *attachment_idx,
                            ));
                        }
                    }
                    LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                        ..
                    } => {
                        let mut part = json!({
                            "functionCall": {
                                "id": call_id,
                                "name": tool_name,
                                "args": serde_json::from_str::<Value>(input_json)
                                    .unwrap_or_else(|_| json!({"_raw": input_json})),
                            }
                        });
                        // Gemini 3 rejects turns where a function_call
                        // from a thinking-enabled run is replayed without
                        // its original thoughtSignature. When we don't
                        // have the real signature (cross-model hop, older
                        // session), drop in the pi sentinel.
                        let effective = replay
                            .as_ref()
                            .and_then(|meta| meta.opaque.clone())
                            .or_else(|| is_gemini_3.then(|| SKIP_THOUGHT_SIGNATURE.to_string()));
                        if let Some(sig) = effective {
                            part["thoughtSignature"] = Value::String(sig);
                        }
                        parts.push(part);
                    }
                    LlmContentBlock::ToolResult {
                        call_id,
                        content,
                        tool_name,
                    } => {
                        parts.push(json!({
                            "functionResponse": {
                                "id": call_id,
                                "name": tool_name.clone().unwrap_or_else(|| "tool".to_string()),
                                "response": { "output": content },
                            }
                        }));
                    }
                    LlmContentBlock::Reasoning { text, replay, .. } => {
                        // Gemini replays reasoning as a `thought:true`
                        // text part carrying the thoughtSignature.
                        let sig = replay.as_ref().and_then(|meta| meta.signature.clone());
                        if sig.is_none() && text.trim().is_empty() {
                            continue;
                        }
                        let mut part = json!({
                            "text": if text.is_empty() { String::from(" ") } else { text.clone() },
                            "thought": true,
                        });
                        if let Some(s) = sig {
                            part["thoughtSignature"] = Value::String(s);
                        }
                        parts.push(part);
                    }
                }
            }

            if parts.is_empty() {
                continue;
            }

            // Merge with previous same-role turn so text + images + tool
            // calls land as a single `contents` entry (matches the old
            // behavior expected by Gemini clients).
            if let Some(prev) = out.last_mut()
                && prev.get("role").and_then(|r| r.as_str()) == Some(role)
                && prev.get("parts").is_some_and(|p| p.is_array())
            {
                prev["parts"].as_array_mut().unwrap().extend(parts);
            } else {
                out.push(json!({
                    "role": role,
                    "parts": parts,
                }));
            }
        }
        out
    }

    /// Claude models served through the Google/Vertex gateway take their tool
    /// schema under the `parameters` field (with JSON-Schema meta keys stripped)
    /// instead of the Gemini-native `parametersJsonSchema`. This is a live,
    /// always-on special case for `claude-*` on Vertex, not deprecated behavior.
    fn uses_claude_on_vertex_tool_parameters(model: &str) -> bool {
        // Wire-protocol dialect fact, not a model-catalog capability fact.
        model.starts_with("claude-")
    }

    /// Strip the JSON-Schema meta keys the Vertex `parameters` field rejects for
    /// `claude-*` models (`$schema`, `$defs`, `$id`, `definitions`), recursing
    /// through nested objects and arrays.
    fn sanitized_claude_on_vertex_schema(schema: &Value) -> Value {
        match schema {
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (key, value) in map {
                    if matches!(key.as_str(), "$schema" | "$defs" | "$id" | "definitions") {
                        continue;
                    }
                    out.insert(key.clone(), Self::sanitized_claude_on_vertex_schema(value));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(Self::sanitized_claude_on_vertex_schema)
                    .collect::<Vec<_>>(),
            ),
            other => other.clone(),
        }
    }

    fn google_tool_choice(choice: &LlmToolChoice) -> &'static str {
        match choice {
            LlmToolChoice::Auto => "AUTO",
            LlmToolChoice::None => "NONE",
            LlmToolChoice::Required => "ANY",
        }
    }

    fn system_instruction(req: &LlmRequest) -> Option<Value> {
        let mut parts: Vec<String> = Vec::new();
        for msg in &req.messages {
            if !matches!(msg.role, LlmRole::System) {
                continue;
            }
            for block in msg.blocks.iter() {
                if let LlmContentBlock::Text { text, .. } = block
                    && !text.is_empty()
                {
                    parts.push(text.to_string());
                }
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(json!({
                "parts": [{ "text": parts.join("\n\n") }],
            }))
        }
    }

    fn thinking_config_from_capability(req: &LlmRequest) -> Option<GoogleThinkingConfig> {
        let reasoning = req.model_capability.reasoning.as_ref()?;
        match &req.model_variant {
            ReasoningSelection::ProviderDefault => None,
            ReasoningSelection::Effort(variant) => match &reasoning.encoding {
                ReasoningEncoding::Effort => Some(GoogleThinkingConfig::Level {
                    level: variant.clone(),
                }),
                ReasoningEncoding::Budget(map) => {
                    map.get(variant)
                        .map(|budget_tokens| GoogleThinkingConfig::Budget {
                            budget_tokens: *budget_tokens,
                        })
                }
            },
            ReasoningSelection::Disabled => match reasoning.disable.as_ref()? {
                ReasoningDisableEncoding::Native => {
                    Some(GoogleThinkingConfig::Budget { budget_tokens: 0 })
                }
                ReasoningDisableEncoding::Omit => None,
                ReasoningDisableEncoding::Effort(level) => Some(GoogleThinkingConfig::Level {
                    level: level.clone(),
                }),
                ReasoningDisableEncoding::Budget(budget_tokens) => {
                    Some(GoogleThinkingConfig::Budget {
                        budget_tokens: *budget_tokens,
                    })
                }
                ReasoningDisableEncoding::ToggleFalse => Some(GoogleThinkingConfig::ToggleFalse),
            },
        }
    }

    pub(crate) fn build_request(
        provider: &GoogleOAuthProvider,
        req: &LlmRequest,
        contents: Vec<Value>,
        project_id: Option<&str>,
    ) -> Value {
        let thinking_config = Self::thinking_config_from_capability(req);
        let policy =
            resolve_generation_policy(&req.generation, &provider.options, 32_768, thinking_config);
        // Both sampling controls live under `generationConfig`. The adapter's
        // greedy temperature default applies only when the caller expressed no
        // preference; a seed is emitted only when one was asked for.
        let temperature = policy
            .temperature
            .clone()
            .map(|value| Value::Number(value.into()))
            .unwrap_or_else(|| json!(0));
        let mut request = json!({
            "model": req.model,
            "user_prompt_id": uuid::Uuid::new_v4().to_string(),
            "request": {
                "contents": contents,
                "generationConfig": {
                    "temperature": temperature,
                    "maxOutputTokens": policy.max_output_tokens,
                }
            }
        });
        if let Some(seed) = policy.seed {
            request["request"]["generationConfig"]["seed"] = json!(seed);
        }
        if !policy.stop_sequences.is_empty() {
            request["request"]["generationConfig"]["stopSequences"] = json!(policy.stop_sequences);
        }
        if let Some(system_instruction) = Self::system_instruction(req) {
            request["request"]["systemInstruction"] = system_instruction;
        }
        request["request"]["sessionId"] = json!(req.session_id());
        if let Some(config) = policy.thinking {
            match config {
                GoogleThinkingConfig::Level { level } => {
                    let mut thinking_config = json!({
                        "thinkingLevel": level,
                    });
                    if policy.expose_thinking {
                        thinking_config["includeThoughts"] = json!(true);
                    }
                    request["request"]["generationConfig"]["thinkingConfig"] = thinking_config;
                }
                GoogleThinkingConfig::Budget { budget_tokens } => {
                    let mut thinking_config = json!({
                        "thinkingBudget": budget_tokens,
                    });
                    if policy.expose_thinking {
                        thinking_config["includeThoughts"] = json!(true);
                    }
                    request["request"]["generationConfig"]["thinkingConfig"] = thinking_config;
                }
                GoogleThinkingConfig::ToggleFalse => {
                    request["request"]["generationConfig"]["thinkingConfig"] =
                        json!({ "thinkingEnabled": false });
                }
            }
        }
        if !req.tools.is_empty() {
            let use_claude_on_vertex_parameters =
                Self::uses_claude_on_vertex_tool_parameters(&req.model);
            request["request"]["tools"] = json!([{
                "functionDeclarations": req
                    .tools
                    .iter()
                    .map(|tool| {
                        let mut declaration = json!({
                            "name": tool.name.clone(),
                            "description": tool.description.clone(),
                        });
                        if use_claude_on_vertex_parameters {
                            declaration["parameters"] =
                                Self::sanitized_claude_on_vertex_schema(tool.input_schema.canonical());
                        } else {
                            declaration["parametersJsonSchema"] =
                                tool.input_schema.canonical().clone();
                        }
                        declaration
                    })
                    .collect::<Vec<_>>()
            }]);
            request["request"]["toolConfig"] = json!({
                "functionCallingConfig": {
                    "mode": Self::google_tool_choice(&req.tool_choice),
                }
            });
        }
        if let Some(output_spec) = &req.output_spec {
            request["request"]["generationConfig"]["responseMimeType"] = json!("application/json");
            if let LlmOutputSpec::JsonSchema(schema) = output_spec {
                request["request"]["generationConfig"]["responseSchema"] =
                    schema.schema.canonical().clone();
            }
        }
        if let Some(project) = project_id.filter(|p| !p.trim().is_empty()) {
            request["project"] = json!(project);
        }
        request
    }
}
