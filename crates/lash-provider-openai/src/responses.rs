use crate::responses_shared as shared;
use crate::support::*;

const PROVIDER: &str = "OpenAI-compatible";

impl OpenAiCompatibleProvider {
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_responses_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        self.build_responses_request(req, stream)
            .map(|built| built.body)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_responses_request(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<BuiltRequest, LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        self.build_responses_request_for_route(req, stream, &serving_route)
    }

    /// What Responses, as this endpoint's compat configures it, can carry.
    fn responses_generation_wire(compat: &OpenAiResolvedCompat) -> GenerationWire {
        GenerationWire {
            label: "OpenAI Responses",
            output_token_cap: output_cap_wire(compat.max_tokens_field),
            temperature: true,
            seed: false,
            stop_sequences: false,
            parallel_tool_calls: compat.request_fields,
            thinking_summary: ThinkingSummaryWire::Always,
            active_thinking_pins_sampling: false,
        }
    }

    pub(crate) fn build_responses_request_for_route(
        &self,
        req: &LlmRequest,
        stream: bool,
        serving_route: &ProviderRouteIdentity,
    ) -> Result<BuiltRequest, LlmTransportError> {
        let safe_request = req
            .reasoning_retention_safe_for(
                serving_route,
                "OpenAI Responses",
                ProviderReasoningRetentionSupport::OpenAiContext,
            )
            .map_err(reasoning_retention_transport_error)?;
        let req = safe_request.as_ref();
        shared::validate_responses_attachments(req, "OpenAI Responses")?;
        let compat = self.resolved_compat(CompletionEndpoint::Responses);
        let policy = resolve_generation_policy(
            req,
            &self.options,
            self.kind(),
            &Self::responses_generation_wire(&compat),
        )?;
        let mut emission = GenerationEmission::default();
        let mut reasoning_body = json!({});
        if let Some(intent) = &policy.reasoning {
            apply_reasoning(
                CompletionEndpoint::Responses,
                compat.reasoning,
                intent,
                &mut reasoning_body,
            )?;
            emission.reasoning = true;
        }
        let tools = shared::build_tools_with_capabilities(
            PROVIDER,
            req,
            compat.strict_tools,
            &compat.schema_capabilities,
        )?;
        let input = shared::build_responses_input(req);
        let mut body = json!({
            "model": req.model,
            "input": null,
            "tools": tools,
            "stream": stream,
        });
        body["input"] = Value::Array(input);
        if let Some(instructions) = &req.instructions {
            body["instructions"] = json!(instructions);
        }
        emission.output_token_cap =
            apply_max_tokens_field(&mut body, compat.max_tokens_field, policy.max_output_tokens);
        if let Some(temperature) = &policy.temperature {
            body["temperature"] = Value::Number(temperature.clone().into());
            emission.temperature = true;
        }
        if !req.tools.is_empty() {
            body["tool_choice"] = json!(shared::tool_choice_value(&req.tool_choice));
        }
        if let Some(parallel_tool_calls) = policy.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel_tool_calls);
            emission.parallel_tool_calls = true;
        }
        // Replay mechanics, not host settings: stateless requests keep the
        // encrypted reasoning items that the next request replays.
        if compat.request_fields {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if compat.store {
            body["store"] = json!(false);
        }
        if let Value::Object(fields) = reasoning_body {
            for (key, value) in fields {
                body[key] = value;
            }
        }
        if let ReasoningRetentionSelection::OpenAiContext { context } =
            req.model_capability.reasoning_retention.selection
        {
            reasoning_object(&mut body)["context"] = json!(context.as_str());
        }
        if policy.request_thinking_summary {
            reasoning_object(&mut body)["summary"] = json!("auto");
            emission.thinking_summary = true;
        }
        if let Some(output_spec) = &req.output_spec {
            let format = match output_spec {
                LlmOutputSpec::JsonObject => json!({ "type": "json_object" }),
                LlmOutputSpec::JsonSchema(schema) => {
                    let projected = shared::projected_schema(
                        PROVIDER,
                        &schema.schema,
                        &compat.schema_capabilities,
                        SchemaPurpose::StructuredOutput,
                    )?;
                    json!({
                        "type": "json_schema",
                        "name": schema.name,
                        "schema": projected,
                        "strict": schema.strict,
                    })
                }
            };
            body["text"] = json!({ "format": format });
        }
        emission.cache = policy.cache_retention != CacheRetention::None && compat.prompt_cache_key;
        if emission.cache {
            body["prompt_cache_key"] = json!(req.provider_prompt_cache_key());
        }
        if policy.cache_retention == CacheRetention::Long && compat.prompt_cache_retention {
            body["prompt_cache_retention"] = json!("24h");
        }
        let mut reserved = reserved_generation_paths(req, "/stop", "/temperature");
        if compat.cache_session_affinity {
            reserved.push("/session_id");
        }
        let passthrough = merge_extra_body(&mut body, &req.extra_body, &reserved)?;
        let mut receipt = policy.receipt(req, &emission);
        receipt.passthrough = if !self.wire.extra_headers.is_empty() {
            lash_core::GenerationOptionOutcome::Applied
        } else {
            passthrough
        };
        Ok(BuiltRequest { body, receipt })
    }

    pub(crate) fn process_sse_event(
        raw: &str,
        state: &mut ResponsesStreamState,
        emitted_parts: Option<&mut Vec<LlmOutputPart>>,
    ) -> Result<(), LlmTransportError> {
        shared::process_sse_event(PROVIDER, raw, state, emitted_parts)
    }

    pub(crate) fn parse_sse_payload(
        payload: &str,
        state: &mut ResponsesStreamState,
    ) -> Result<(), LlmTransportError> {
        shared::parse_sse_payload(PROVIDER, payload, state)
    }
}
