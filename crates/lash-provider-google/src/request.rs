//! Request-body construction: translating an [`LlmRequest`] into the Cloud
//! Code (Gemini) wire shape (contents, systemInstruction, tools, generation
//! and thinking config), plus the inline-attachment-part helpers.

use crate::support::*;
use lash_core::GoogleDialect;
use lash_core::facade_support::{
    ProviderSchemaCapabilities, SchemaPurpose, SchemaResolutionRequest, resolve_schema,
};

impl GoogleOAuthProvider {
    pub(crate) fn reasoning_retention_safe_request<'a>(
        &self,
        req: &'a LlmRequest,
    ) -> Result<std::borrow::Cow<'a, LlmRequest>, LlmTransportError> {
        let serving_route = self.route_identity_for_model(&req.model);
        req.reasoning_retention_safe_for(
            &serving_route,
            "Google Gemini",
            ProviderReasoningRetentionSupport::ClientSideUserSegments,
        )
        .map_err(|error: ReasoningRetentionValidationError| {
            LlmTransportError::new(error.message)
                .with_kind(ProviderFailureKind::Unsupported)
                .with_lash_code(TurnFailureCode::UnsupportedReasoningRetention)
                .with_retry_verdict(TransportRetryVerdict::Forbidden)
        })
    }

    pub(crate) const PROVIDER_KIND: &'static str = "google_oauth";

    #[expect(
        clippy::expect_used,
        reason = "this arm only matches Inline/Stored sources, which always carry a MIME, and validate_attachments refuses unresolved stored bytes before any part is built"
    )]
    pub(crate) fn inline_attachment_part(req: &LlmRequest, source: &AttachmentSource) -> Value {
        match source {
            AttachmentSource::ProviderFile { id, .. } => {
                json!({"fileData": {"fileUri": id}})
            }
            AttachmentSource::ExternalUrl { media_type, url } => {
                json!({"fileData": {"mimeType": media_type, "fileUri": url}})
            }
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

    #[expect(
        clippy::expect_used,
        reason = "Stored sources always carry attachment_ref.media_type; only ProviderFile can lack a caller MIME"
    )]
    pub(crate) fn validate_attachments(req: &LlmRequest) -> Result<(), LlmTransportError> {
        for (message_index, message) in req.messages.iter().enumerate() {
            for source in message
                .blocks
                .iter()
                .flat_map(LlmContentBlock::attachment_sources)
            {
                let validation = (|| {
                    let supported = req
                        .model_capability
                        .attachment_acceptance
                        .accepts("Google Gemini", source);
                    if !supported {
                        let accepted_by = known_attachment_acceptors(
                            &req.model_capability.attachment_acceptance,
                            source,
                        );
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
                .with_lash_code(TurnFailureCode::StoredAttachmentNotResolved));
                    }

                    Ok(())
                })();
                validation.map_err(|mut error: LlmTransportError| {
                    error.message = format!("message index {message_index}: {}", error.message);
                    error
                })?;
            }
        }
        Ok(())
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

    #[expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "every content entry here is built by this fn with `parts` as a JSON array; the merge guard re-checks is_array and the sort sees the entries it built"
    )]
    pub(crate) fn build_contents_with_attachment_parts(
        &self,
        req: &LlmRequest,
        attachment_parts: &[(AttachmentSource, Value)],
    ) -> Result<Vec<Value>, LlmTransportError> {
        let safe_request = self.reasoning_retention_safe_request(req)?;
        let req = safe_request.as_ref();
        let mut out: Vec<Value> = Vec::new();
        let attachment_part = |source: &AttachmentSource| {
            attachment_parts
                .iter()
                .find(|(candidate, _)| candidate == source)
                .map(|(_, part)| part.clone())
                .unwrap_or_else(|| Self::inline_attachment_part(req, source))
        };
        // Gemini 3 accepts media inside a function response; older dialects
        // (and Claude on Vertex) take it only as ordinary user parts.
        let multimodal_function_response =
            matches!(req.model_capability.google_dialect, GoogleDialect::Gemini3);
        let missing_signature = match req.model_capability.google_dialect {
            GoogleDialect::Gemini3 => Some("skip_thought_signature_validator"),
            GoogleDialect::Legacy | GoogleDialect::ClaudeOnVertex => None,
        };

        for msg in &req.messages {
            let role = match msg.role {
                LlmRole::Assistant => "model",
                LlmRole::User | LlmRole::System => "user",
            };

            let mut parts: Vec<Value> = Vec::new();
            if matches!(msg.role, LlmRole::System) {
                let text = msg
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect::<String>();
                parts.push(json!({"text": format!("<runtime_feedback>{text}</runtime_feedback>")}));
            }
            for block in msg.blocks.iter().filter(|block| {
                !matches!(msg.role, LlmRole::System)
                    || !matches!(block, LlmContentBlock::Text { .. })
            }) {
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
                    LlmContentBlock::Attachment { source } => {
                        if matches!(msg.role, LlmRole::User | LlmRole::System) {
                            parts.push(attachment_part(source));
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
                                "args": tool_call_input_replay_value(input_json),
                            }
                        });
                        // The host's explicit Gemini-3 dialect opts into the wire's
                        // missing-signature escape value; model names carry no policy.
                        let effective = replay
                            .as_ref()
                            .and_then(|meta| meta.opaque.clone())
                            .or_else(|| missing_signature.map(str::to_owned));
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
                        // One function response per call. Its text keeps
                        // `[Attachment N]` markers where attachments sat. On
                        // Gemini 3 the attachments Google accepts inside a
                        // function response ride in its `parts`; every other
                        // attachment (and all of them on older dialects)
                        // follows the response as user parts, each after its
                        // marker.
                        let mut response = json!({
                            "functionResponse": {
                                "id": call_id,
                                "name": tool_name.clone().unwrap_or_else(|| "tool".to_string()),
                                "response": { "output": tool_result_text(content) },
                            }
                        });
                        let mut inside = Vec::new();
                        let mut after = Vec::new();
                        for (index, source) in content
                            .iter()
                            .filter_map(ModelToolReturnPart::attachment)
                            .enumerate()
                        {
                            if multimodal_function_response
                                && function_response_part_accepts(source)
                            {
                                inside.push(attachment_part(source));
                            } else {
                                after
                                    .push(json!({ "text": format!("[Attachment {}]", index + 1) }));
                                after.push(attachment_part(source));
                            }
                        }
                        if !inside.is_empty() {
                            response["functionResponse"]["parts"] = Value::Array(inside);
                        }
                        parts.push(response);
                        if !after.is_empty() {
                            parts.push(json!({
                                "text": format!("Attachments from tool result {call_id}:")
                            }));
                            parts.extend(after);
                        }
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
        for content in &mut out {
            if content["role"] == "user" {
                // Keep parallel function responses together before the tagged
                // feedback in their coalesced user turn.
                content["parts"]
                    .as_array_mut()
                    .expect("content parts")
                    .sort_by_key(|part| part.get("functionResponse").is_none());
            }
        }
        Ok(out)
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
        req.instructions
            .as_ref()
            .map(|text| json!({"parts": [{"text": text}]}))
    }

    /// What Cloud Code's `generationConfig` can carry for this request.
    /// Gemini has no parallel-tool-call control. Claude served through
    /// Vertex pins sampling while it thinks, as it does on Anthropic.
    fn generation_wire(req: &LlmRequest) -> GenerationWire {
        GenerationWire {
            label: "Google Cloud Code",
            output_token_cap: OutputCapWire::Optional,
            temperature: true,
            seed: true,
            stop_sequences: true,
            parallel_tool_calls: false,
            thinking_summary: ThinkingSummaryWire::Always,
            active_thinking_pins_sampling: matches!(
                req.model_capability.google_dialect,
                GoogleDialect::ClaudeOnVertex
            ),
        }
    }

    /// The one Google reasoning mapping: a resolved intent onto
    /// `thinkingConfig` for the host-selected dialect, or a refusal.
    ///
    /// | Dialect | Effort | Budget | Off |
    /// |---|---|---|---|
    /// | Legacy | `thinkingLevel` | `thinkingBudget` | `thinkingBudget: 0` |
    /// | Gemini3 | `thinkingLevel` | `thinkingBudget` | refused |
    /// | ClaudeOnVertex | refused | `thinkingBudget`, below the cap | refused |
    ///
    /// Off also requires the host capability's `disable`, which resolution
    /// already checked. `includeThoughts` requests the summary whenever
    /// `expose_thinking` is set, with or without a reasoning intent.
    fn thinking_config(
        dialect: GoogleDialect,
        intent: Option<&ReasoningIntent>,
        include_thoughts: bool,
        max_output_tokens: Option<u64>,
    ) -> Result<Option<Value>, LlmTransportError> {
        let unrepresentable = |detail: &str| {
            LlmTransportError::new(format!("reasoning selection cannot be sent: {detail}"))
                .with_lash_code(TurnFailureCode::ReasoningEncodingUnrepresentable)
                .with_retry_verdict(TransportRetryVerdict::Forbidden)
        };
        let mut config = match (dialect, intent) {
            (_, None) => json!({}),
            (
                GoogleDialect::Legacy | GoogleDialect::Gemini3,
                Some(ReasoningIntent::Effort(level)),
            ) => {
                json!({ "thinkingLevel": level })
            }
            (GoogleDialect::ClaudeOnVertex, Some(ReasoningIntent::Effort(_))) => {
                return Err(unrepresentable(
                    "Claude on Vertex takes a thinking budget, not a thinking level",
                ));
            }
            (GoogleDialect::ClaudeOnVertex, Some(ReasoningIntent::Budget(budget)))
                if max_output_tokens.is_some_and(|cap| u64::from(*budget) >= cap) =>
            {
                return Err(LlmTransportError::new(format!(
                    "Claude on Vertex needs a thinking budget below the output-token cap; the selected budget of {budget} tokens does not fit."
                ))
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::ReasoningBudgetExceedsOutputCap)
                .with_retry_verdict(TransportRetryVerdict::Forbidden));
            }
            (_, Some(ReasoningIntent::Budget(budget))) => json!({ "thinkingBudget": budget }),
            (GoogleDialect::Legacy, Some(ReasoningIntent::Off)) => json!({ "thinkingBudget": 0 }),
            (GoogleDialect::Gemini3, Some(ReasoningIntent::Off)) => {
                return Err(unrepresentable("Gemini 3 cannot turn thinking off"));
            }
            (GoogleDialect::ClaudeOnVertex, Some(ReasoningIntent::Off)) => {
                return Err(unrepresentable(
                    "Claude on Vertex has no verified thinking-off field",
                ));
            }
        };
        if include_thoughts {
            config["includeThoughts"] = json!(true);
        }
        Ok((config != json!({})).then_some(config))
    }

    /// Resolve every host setting for this request and map its reasoning,
    /// refusing what Cloud Code cannot send. Pure, so `complete` runs it
    /// before the project lookup and any upload, and the builder agrees.
    pub(crate) fn resolve_generation(
        provider: &GoogleOAuthProvider,
        req: &LlmRequest,
    ) -> Result<(ResolvedGenerationPolicy, Option<Value>), LlmTransportError> {
        let policy = resolve_generation_policy(
            req,
            &provider.options,
            Self::PROVIDER_KIND,
            &Self::generation_wire(req),
        )?;
        let thinking_config = Self::thinking_config(
            req.model_capability.google_dialect,
            policy.reasoning.as_ref(),
            policy.request_thinking_summary,
            policy.max_output_tokens,
        )?;
        Ok((policy, thinking_config))
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_request(
        provider: &GoogleOAuthProvider,
        req: &LlmRequest,
        contents: Vec<Value>,
        project_id: Option<&str>,
    ) -> Result<Value, LlmTransportError> {
        Self::build_request_with_receipt(provider, req, contents, project_id).map(|(body, _)| body)
    }

    /// The Cloud Code request and the receipt of the host settings it carries.
    pub(crate) fn build_request_with_receipt(
        provider: &GoogleOAuthProvider,
        req: &LlmRequest,
        contents: Vec<Value>,
        project_id: Option<&str>,
    ) -> Result<(Value, GenerationReceipt), LlmTransportError> {
        let (policy, thinking_config) = Self::resolve_generation(provider, req)?;
        let mut emission = GenerationEmission::default();
        let mut generation_config = json!({});
        if let Some(temperature) = &policy.temperature {
            generation_config["temperature"] = Value::Number(temperature.clone().into());
            emission.temperature = true;
        }
        if let Some(max_output_tokens) = policy.max_output_tokens {
            generation_config["maxOutputTokens"] = json!(max_output_tokens);
            emission.output_token_cap = true;
        }
        if let Some(seed) = policy.seed {
            generation_config["seed"] = json!(seed);
            emission.seed = true;
        }
        if !policy.stop_sequences.is_empty() {
            generation_config["stopSequences"] = json!(policy.stop_sequences);
            emission.stop_sequences = true;
        }
        if let Some(thinking_config) = thinking_config {
            generation_config["thinkingConfig"] = thinking_config;
            emission.reasoning = policy.reasoning.is_some();
            emission.thinking_summary = policy.request_thinking_summary;
        }
        let mut request = json!({
            "model": req.model,
            "user_prompt_id": uuid::Uuid::new_v4().to_string(),
            "request": {
                "contents": contents,
                "generationConfig": generation_config,
            }
        });
        if let Some(system_instruction) = Self::system_instruction(req) {
            request["request"]["systemInstruction"] = system_instruction;
        }
        request["request"]["sessionId"] = json!(req.provider_session_affinity_key());
        if !req.tools.is_empty() {
            let use_claude_on_vertex_parameters = matches!(
                req.model_capability.google_dialect,
                GoogleDialect::ClaudeOnVertex
            );
            request["request"]["tools"] = json!([{
                "functionDeclarations": req
                    .tools
                    .iter()
                    .map(|tool| {
                        let schema = Self::project_schema(&tool.input_schema, SchemaPurpose::ToolInput)?;
                        let mut declaration = json!({
                            "name": tool.name.clone(),
                            "description": tool.description.clone(),
                        });
                        if use_claude_on_vertex_parameters {
                            declaration["parameters"] =
                                Self::sanitized_claude_on_vertex_schema(&schema);
                        } else {
                            declaration["parametersJsonSchema"] =
                                schema;
                        }
                        Ok::<_, LlmTransportError>(declaration)
                    })
                    .collect::<Result<Vec<_>, _>>()?
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
                    Self::project_schema(&schema.schema, SchemaPurpose::StructuredOutput)?;
            }
        }
        if let Some(project) = project_id.filter(|p| !p.trim().is_empty()) {
            request["project"] = json!(project);
        }
        // Cloud Code reports cached-token usage, but Lash emits no
        // prompt-cache directive in this request dialect.
        let receipt = policy.receipt(req, &emission);
        Ok((request, receipt))
    }

    fn project_schema(
        schema: &lash_core::SchemaContract,
        purpose: SchemaPurpose,
    ) -> Result<Value, LlmTransportError> {
        let capabilities = ProviderSchemaCapabilities::google();
        resolve_schema(
            schema,
            SchemaResolutionRequest {
                provider: "Google",
                purpose,
                dialects: capabilities.dialects_for(purpose),
            },
        )
        .map(|resolved| resolved.schema)
        .map_err(|error| {
            LlmTransportError::new(format!(
                "Google schema projection failed: {}",
                error.first_diagnostic()
            ))
            .with_kind(ProviderFailureKind::Validation)
        })
    }
}

/// Whether Gemini accepts `source` as a multimodal function-response part.
/// Google documents images (PNG, JPEG, WebP) and documents (PDF, plain text)
/// there; audio, video and anything else must travel as ordinary user parts.
fn function_response_part_accepts(source: &AttachmentSource) -> bool {
    source.media_type().is_some_and(|media_type| {
        matches!(
            media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "application/pdf" | "text/plain"
        )
    })
}
