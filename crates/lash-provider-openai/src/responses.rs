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
        self.build_responses_request_body_with_cache_evidence(req, stream)
            .map(|(body, _)| body)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_responses_request_body_with_cache_evidence(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<(Value, bool), LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        self.build_responses_request_body_for_route_with_cache_evidence(req, stream, &serving_route)
    }

    pub(crate) fn build_responses_request_body_for_route_with_cache_evidence(
        &self,
        req: &LlmRequest,
        stream: bool,
        serving_route: &ProviderRouteIdentity,
    ) -> Result<(Value, bool), LlmTransportError> {
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
        let tools = shared::build_tools_with_capabilities(
            PROVIDER,
            req,
            compat.strict_tools,
            &compat.schema_capabilities,
        )?;
        let input = shared::build_responses_input(req, shared::ResponsesInputOptions::OPENAI);
        let policy = resolve_generation_policy(
            &req.generation,
            &self.options,
            DEFAULT_MAX_OUTPUT_TOKENS,
            (),
        );
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
        apply_max_tokens_field(&mut body, compat.max_tokens_field, policy.max_output_tokens);
        // Responses accepts `temperature` but has no `seed` field, so a
        // requested seed is simply not expressible on this wire.
        if let Some(temperature) = &policy.temperature {
            body["temperature"] = Value::Number(temperature.clone().into());
        }
        if !req.tools.is_empty() {
            body["tool_choice"] = json!(shared::tool_choice_value(&req.tool_choice));
        }
        if compat.request_fields {
            body["include"] = json!(["reasoning.encrypted_content"]);
            body["parallel_tool_calls"] = json!(!req.tools.is_empty());
            body["text"] = json!({"verbosity": "medium"});
        }
        if compat.store {
            body["store"] = json!(false);
        }
        if let Some(intent) = reasoning_intent(req) {
            compat
                .reasoning_format
                .encode(CompletionEndpoint::Responses, &intent, &mut body)
                .map_err(|error| {
                    reasoning_encode_transport_error(CompletionEndpoint::Responses, &intent, error)
                })?;
        }
        if let ReasoningRetentionSelection::OpenAiContext { context } =
            req.model_capability.reasoning_retention.selection
        {
            if !body["reasoning"].is_object() {
                body["reasoning"] = json!({});
            }
            body["reasoning"]["context"] = json!(context.as_str());
        }
        if policy.expose_thinking && body["reasoning"].is_object() {
            body["reasoning"]["summary"] = json!("auto");
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
            if body.get("text").is_none() {
                body["text"] = json!({});
            }
            body["text"]["format"] = format;
        }
        let cache_control_emitted =
            policy.cache_retention != CacheRetention::None && compat.prompt_cache_key;
        if cache_control_emitted {
            body["prompt_cache_key"] = json!(req.continuation_key());
        }
        if policy.cache_retention == CacheRetention::Long && compat.prompt_cache_retention {
            body["prompt_cache_retention"] = json!("24h");
        }
        Ok((body, cache_control_emitted))
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
