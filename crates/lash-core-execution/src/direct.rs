use crate::AttachmentRef;
use crate::llm::types::{
    LlmContentBlock, LlmEventSender, LlmJsonSchema, LlmMessage, LlmOutputSpec, LlmRequest,
    LlmRequestScope, LlmResponse, LlmRole, LlmStreamEvent, LlmToolChoice,
};
#[cfg(test)]
use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
#[cfg(test)]
use crate::provider::LlmProfileCapability;
use crate::provider::{
    LlmProfileEffortValidationCategory, NoSlotDeliveries, ProviderHandle, SlotDeliveries,
};
use lash_trace::{TraceContext, TraceSink};
use lash_transport_failure::LlmTransportError;
use lash_vocabulary::schema_contract::SchemaContract;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectRole {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DirectPart {
    Text(String),
    Attachment(Box<AttachmentRef>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectMessage {
    pub role: DirectRole,
    pub parts: Vec<DirectPart>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectJsonSchema {
    pub name: String,
    pub schema: SchemaContract,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DirectOutputSpec {
    #[default]
    Text,
    JsonObject,
    JsonSchema(DirectJsonSchema),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRequest {
    /// The attachment-acceptance rules the request renders its attachments
    /// under. A durable direct completion replaces them with its session's
    /// recorded rules.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    pub attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    #[serde(default)]
    pub messages: Vec<DirectMessage>,
    #[serde(default)]
    pub output: DirectOutputSpec,
    #[serde(default)]
    pub generation: crate::GenerationOptions,
    #[serde(default, skip)]
    pub stream_events: Option<LlmEventSender>,
    /// Who the call is made for. `None` lets the runtime fill the owner it
    /// runs the call for (a tool's session or process); a call nobody
    /// fills is the host's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<crate::LlmRequestOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Caller-owned durable position for this request.
    ///
    /// Sequential unkeyed calls use runtime ordinals scoped by causal lane and
    /// usage source. Their call order must be deterministic on every redrive;
    /// conditional or reordered calls must use stable explicit keys. Calls
    /// that may be polled concurrently under one usage source must provide
    /// distinct keys so task scheduling cannot choose their replay identity.
    /// Independent lifecycle hooks should use distinct usage sources; fan-out
    /// inside one hook still requires per-branch keys.
    pub replay: Option<crate::RuntimeReplay>,
}

impl DirectRequest {
    /// Attachment refs in message order, derived from their owning blocks.
    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        self.messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                DirectPart::Attachment(reference) => Some(reference.as_ref()),
                DirectPart::Text(_) => None,
            })
    }

    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            attachment_acceptance: Arc::default(),
            messages: vec![DirectMessage {
                role: DirectRole::User,
                parts: vec![DirectPart::Text(prompt.into())],
            }],
            output: DirectOutputSpec::Text,
            generation: crate::GenerationOptions::default(),
            stream_events: None,
            owner: None,
            caused_by: None,
            replay: None,
        }
    }

    pub fn json(prompt: impl Into<String>) -> Self {
        Self {
            output: DirectOutputSpec::JsonObject,
            ..Self::text(prompt)
        }
    }

    pub fn json_schema(prompt: impl Into<String>, schema: DirectJsonSchema) -> Self {
        Self {
            output: DirectOutputSpec::JsonSchema(schema),
            ..Self::text(prompt)
        }
    }

    pub fn with_replay_key(mut self, key: impl Into<String>) -> Self {
        self.replay = Some(crate::RuntimeReplay {
            key: key.into(),
            attribution: None,
        });
        self
    }

    pub fn with_caused_by(mut self, caused_by: crate::CausalRef) -> Self {
        self.caused_by = Some(caused_by);
        self
    }
}

#[derive(Debug, thiserror::Error, Clone)]
#[non_exhaustive]
pub enum DirectLlmError {
    #[error(
        "leading System messages are ambiguous; initial instructions are the prompt sections of the call's purpose"
    )]
    LeadingSystemMessage,
    #[error("invalid request: {message}")]
    InvalidRequest {
        category: LlmProfileEffortValidationCategory,
        message: String,
    },
    #[error("invalid response: {source}")]
    InvalidResponse {
        source: lash_kernel_value::json_schema::ValueMismatch,
        result: Box<DirectLlmOutcome>,
    },
    #[error("transport error: {0}")]
    Transport(#[from] Box<LlmTransportError>),
}

impl DirectLlmError {
    pub fn value_mismatch(&self) -> Option<&lash_kernel_value::json_schema::ValueMismatch> {
        match self {
            Self::InvalidResponse { source, .. } => Some(source),
            Self::LeadingSystemMessage | Self::InvalidRequest { .. } | Self::Transport(_) => None,
        }
    }
}

/// Successful single-shot direct LLM result with the sealed provider-attempt
/// history that produced it.
#[derive(Clone, Debug)]
pub struct DirectLlmOutcome {
    pub response: LlmResponse,
    pub llm_call: crate::LlmCallRecord,
}

impl std::ops::Deref for DirectLlmOutcome {
    type Target = LlmResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

impl DirectLlmOutcome {
    pub fn into_response(self) -> LlmResponse {
        self.response
    }
}

pub struct DirectLlmClient {
    provider: ProviderHandle,
    model: crate::LlmProfileConfig,
    /// The host's own instruction text for this client's requests. A
    /// host-owned call composes no prompt sections: it belongs to no session.
    instructions: Option<Arc<str>>,
    trace_sink: Option<Arc<dyn TraceSink>>,
    content_policy: lash_trace::ContentPolicy,
    clock: Arc<dyn crate::Clock>,
    /// What delivers a request's attachment slots on each attempt. A client
    /// with none refuses an attachment-bearing request unsent.
    deliveries: Arc<dyn SlotDeliveries>,
    /// The bounds every completion of this client runs under.
    budgets: crate::ExecutionBudgets,
}

impl DirectLlmClient {
    /// A client over `provider` and `model` whose completions run under
    /// `budgets`. A host's own completion runs under no lash execution, so
    /// nothing else bounds it: the model total, the provider attempt limits
    /// and the attempt count are the host's stated spend decision, and there
    /// is no default.
    pub fn new(
        provider: ProviderHandle,
        model: crate::LlmProfileConfig,
        budgets: crate::ExecutionBudgets,
    ) -> Self {
        Self {
            provider,
            model,
            budgets,
            instructions: None,
            trace_sink: None,
            content_policy: lash_trace::ContentPolicy::standard(),
            clock: Arc::new(crate::SystemClock),
            deliveries: Arc::new(NoSlotDeliveries),
        }
    }

    /// Deliver every request's attachment slots through `deliveries`, such
    /// as the host's attachment store, on each attempt.
    pub fn with_attachment_deliveries(mut self, deliveries: Arc<dyn SlotDeliveries>) -> Self {
        self.deliveries = deliveries;
        self
    }

    /// Send `instructions` as every request's initial instructions.
    pub fn with_instructions(mut self, instructions: Option<Arc<str>>) -> Self {
        self.instructions = instructions;
        self
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn TraceSink>>) -> Self {
        self.trace_sink = sink;
        self
    }

    /// State whether this client's trace records carry the request and the
    /// response. Defaults to [`lash_trace::ContentPolicy::standard`]
    /// (omitted).
    pub fn with_content_policy(mut self, content: lash_trace::ContentPolicy) -> Self {
        self.content_policy = content;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub async fn complete(
        &mut self,
        request: DirectRequest,
    ) -> Result<DirectLlmOutcome, DirectLlmError> {
        self.model
            .validate_reasoning()
            .map_err(|error| DirectLlmError::InvalidRequest {
                category: error.category,
                message: error.message,
            })?;

        let output_for_validation = request.output.clone();
        let model = self.model.clone();
        let mut llm_request = build_llm_request(request, model)?;
        llm_request.instructions = self.instructions.clone();
        llm_request.stream_events =
            transport_stream_events_for_direct(&self.provider, llm_request.stream_events.take());
        let request_model = llm_request.model.wire_model().to_string();
        // The call's id names it on every attempt's request scope and on its
        // trace records alike, whether or not anything observes it.
        let id = uuid::Uuid::new_v4().to_string();
        llm_request.scope.llm_call_id = Some(id.clone());
        // A host-owned call made outside any journal: nothing replays it, so
        // the call is its own live attempt, and it belongs to no admitted
        // scope.
        let traced = self.trace_sink.as_ref().map(|sink| {
            let standing = crate::trace::TraceRuntime::new(Arc::clone(&self.clock))
                .with_trace_sink(Arc::clone(sink))
                .with_content(self.content_policy)
                .unreplayed(None);
            let call = crate::runtime::effect::emit_llm_trace_started(
                &standing,
                TraceContext::default().for_llm_call(id.clone()),
                &llm_request,
                self.provider.kind(),
            );
            (call, id)
        });
        llm_request.scope.trace_carrier = traced
            .as_ref()
            .and_then(|(call, _)| call.carrier().cloned().map(Arc::new));
        // No lash execution owns this call: the host that made it owns its
        // billing, and lash keeps no ledger row for it (ADR 0127).
        let sideband =
            lash_core_llm::core_internal::prepare_completion(&self.provider, &mut llm_request);
        let sideband = match &traced {
            Some((call, id)) => {
                call.provider_attempts(sideband, TraceContext::default().for_llm_call(id.clone()))
            }
            None => sideband,
        };
        let template = match self.provider.lower(&llm_request).await {
            Ok(template) => Arc::new(template),
            Err(error) => return Err(DirectLlmError::from(Box::new(error))),
        };
        match lash_core_llm::core_internal::complete_prepared(
            &mut self.provider,
            lash_protocol_types::llm::types::ResponseContext::of_request(&llm_request),
            &template,
            self.deliveries.as_ref(),
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &Default::default(),
            traced
                .as_ref()
                .and_then(|(call, _)| call.standing().body_permit()),
            // A host's own completion runs under no lash execution, so the
            // budgets its client was created with bound it.
            lash_core_llm::core_internal::ModelCallBounds::unnested(self.budgets.clone()),
        )
        .await
        {
            Ok(response) => {
                let result = DirectLlmOutcome {
                    response: response.response,
                    llm_call: response.call_record,
                };
                if let Err(source) =
                    validate_direct_output(&output_for_validation, &result.response)
                {
                    let error = DirectLlmError::InvalidResponse {
                        source,
                        result: Box::new(result),
                    };
                    if let Some((call, llm_call_id)) = traced {
                        let call_record = match &error {
                            DirectLlmError::InvalidResponse { result, .. } => &result.llm_call,
                            _ => unreachable!("constructed InvalidResponse above"),
                        };
                        crate::runtime::effect::emit_llm_trace_failed(
                            call,
                            TraceContext::default().for_llm_call(llm_call_id),
                            crate::runtime::effect::LlmTraceFailure::invalid_structured_output(),
                            None,
                            Some(call_record),
                        );
                    }
                    return Err(error);
                }
                if let Some((call, llm_call_id)) = traced {
                    crate::runtime::effect::emit_llm_trace_completed(
                        call,
                        TraceContext::default().for_llm_call(llm_call_id),
                        &result.response,
                        &request_model,
                        0,
                        None,
                        Some(&result.llm_call),
                    );
                }
                Ok(result)
            }
            Err(error) => {
                if let Some((call, llm_call_id)) = traced {
                    crate::runtime::effect::emit_llm_trace_failed(
                        call,
                        TraceContext::default().for_llm_call(llm_call_id),
                        crate::runtime::effect::LlmTraceFailure::from(&error.error),
                        None,
                        Some(&error.call_record),
                    );
                }
                Err(DirectLlmError::from(Box::new(error.error)))
            }
        }
    }
}

/// The provider request `request` describes. Nothing here reads a transport:
/// the sender a caller asked for travels as given, and the caller that owns
/// a bound transport adds the one it requires
/// ([`transport_stream_events_for_direct`]).
pub fn build_llm_request(
    request: DirectRequest,
    model: crate::LlmProfileConfig,
) -> Result<LlmRequest, DirectLlmError> {
    if request
        .messages
        .first()
        .is_some_and(|message| matches!(message.role, DirectRole::System))
    {
        return Err(DirectLlmError::LeadingSystemMessage);
    }
    let DirectRequest {
        attachment_acceptance,
        messages,
        output,
        generation,
        stream_events,
        owner,
        caused_by: _,
        replay: _,
    } = request;

    let output_spec = match output {
        DirectOutputSpec::Text => None,
        DirectOutputSpec::JsonObject => Some(LlmOutputSpec::JsonObject),
        DirectOutputSpec::JsonSchema(schema) => Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
            name: schema.name,
            schema: schema.schema,
            strict: schema.strict,
        })),
    };

    let mut llm_messages = Vec::new();
    for message in messages {
        let starts_user_segment = matches!(message.role, DirectRole::User);
        let role = match message.role {
            DirectRole::System => LlmRole::System,
            DirectRole::User => LlmRole::User,
            DirectRole::Assistant => LlmRole::Assistant,
        };
        let mut blocks: Vec<LlmContentBlock> = Vec::new();
        for part in message.parts {
            match part {
                DirectPart::Text(text) => {
                    if !text.is_empty() {
                        blocks.push(LlmContentBlock::Text {
                            text: text.into(),
                            response_meta: None,
                            cache_breakpoint: false,
                        });
                    }
                }
                DirectPart::Attachment(reference) => {
                    blocks.push(LlmContentBlock::Attachment { reference });
                }
            }
        }
        if !blocks.is_empty() {
            let mut message = LlmMessage::new(role, blocks);
            message.starts_user_segment = starts_user_segment;
            llm_messages.push(message);
        }
    }

    // This request id is transport metadata for the DirectRequest path; its
    // durable position was selected from replay/ordinal before normalization,
    // and a durable completion replaces it with its owned call's key.
    // Callers of direct_llm_completion must instead supply their own
    // per-logical-call request id.
    let scope = match owner {
        Some(crate::LlmRequestOwner::Session { session_id }) => LlmRequestScope::owned(
            crate::LlmRequestOwner::Session {
                session_id: session_id.clone(),
            },
            format!("{session_id}:frame:direct"),
            format!("{session_id}:direct"),
        ),
        Some(crate::LlmRequestOwner::Process { process_id }) => LlmRequestScope::owned(
            crate::LlmRequestOwner::Process {
                process_id: process_id.clone(),
            },
            format!("process:{process_id}:frame:direct"),
            format!("process:{process_id}:direct"),
        ),
        Some(crate::LlmRequestOwner::Host) | None => {
            let request_id = uuid::Uuid::new_v4().to_string();
            LlmRequestScope::owned(
                crate::LlmRequestOwner::Host,
                format!("direct:{request_id}:frame"),
                request_id,
            )
        }
    };

    Ok(LlmRequest {
        instructions: None,
        model,
        messages: llm_messages,
        tools: Vec::new().into(),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance,
        generation,
        scope,
        output_spec,
        stream_events,
        provider_trace: None,
        attempt_control: Default::default(),
    })
}

fn validate_direct_output(
    output: &DirectOutputSpec,
    response: &LlmResponse,
) -> Result<(), lash_kernel_value::json_schema::ValueMismatch> {
    let DirectOutputSpec::JsonSchema(schema) = output else {
        return Ok(());
    };
    let response_text = response.full_text();
    let parsed: serde_json::Value = serde_json::from_str(response_text.trim()).map_err(|err| {
        lash_kernel_value::json_schema::ValueMismatch {
            instance_path: String::new(),
            message: format!("expected JSON: {err}"),
        }
    })?;
    schema.schema.canonical.validate(&parsed)
}

fn transport_stream_events_for_direct(
    provider: &ProviderHandle,
    requested: Option<LlmEventSender>,
) -> Option<LlmEventSender> {
    if requested.is_some() {
        return requested;
    }
    if provider.requires_streaming() {
        Some(LlmEventSender::new(|_event: LlmStreamEvent| {}))
    } else {
        None
    }
}

#[cfg(test)]
fn profile(wire_model: &str) -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new("direct-test"),
        crate::LlmProfileMetadata::builder(wire_model)
            .cache_retention(crate::provider::CacheRetention::Short)
            .context_window_tokens(128_000)
            .build()
            .expect("valid standalone profile"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{LlmOutputPart, LlmTerminalReason, LlmUsage};
    use crate::testing::TestProvider;
    use lash_protocol_types::sync::MutexExt;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn direct_client_validates_json_schema_output_against_canonical_schema() {
        let provider = TestProvider::builder()
            .kind("direct-validation-provider")
            .complete(|_request| async {
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: r#"{"items":[]}"#.to_string(),
                        response_meta: None,
                    }],
                    usage: LlmUsage {
                        input_tokens: 17,
                        output_tokens: 3,
                        ..Default::default()
                    },
                    terminal_reason: LlmTerminalReason::Stop,
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );
        let request = DirectRequest::json_schema(
            "return items",
            DirectJsonSchema {
                name: "items_result".to_string(),
                schema: lash_vocabulary::schema_contract::SchemaContract::admit(json!({
                    "type": "object",
                    "required": ["items"],
                    "properties": {
                        "items": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "type": "string" }
                        }
                    }
                }))
                .expect("valid declared schema"),
                strict: true,
            },
        );

        let err = client
            .complete(request)
            .await
            .expect_err("empty items must fail canonical validation");

        let DirectLlmError::InvalidResponse { result, .. } = &err else {
            panic!("expected invalid response, got {err:?}");
        };
        assert_eq!(result.full_text(), r#"{"items":[]}"#);
        assert_eq!(result.usage.input_tokens, 17);
        assert_eq!(result.usage.output_tokens, 3);
        assert_eq!(result.terminal_reason, LlmTerminalReason::Stop);
        assert_eq!(result.llm_call.attempts.len(), 1);
        let error = err.to_string();
        assert!(
            error.contains("items") && error.contains("[] has less than 1 item"),
            "{error}"
        );
    }

    fn reasoning_capability() -> LlmProfileCapability {
        LlmProfileCapability {
            instruction_role: Default::default(),
            native_mid_conversation_system: false,
            google_dialect: Default::default(),
            reasoning: Some(crate::ReasoningCapability {
                efforts: ["low", "medium", "high", "max"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                ..Default::default()
            }),
            cache_control: None,
            stream_termination: None,
            sampling: crate::SamplingCapability::Configurable,
            reasoning_retention: Default::default(),
        }
    }

    #[tokio::test]
    async fn direct_client_rejects_unsupported_effort_before_provider_call() {
        let called = Arc::new(Mutex::new(false));
        let called_for_provider = Arc::clone(&called);
        let provider = TestProvider::builder()
            .kind("direct-reject")
            .complete(move |_request| {
                let called = Arc::clone(&called_for_provider);
                async move {
                    *called.lock_recover() = true;
                    Ok(LlmResponse::default())
                }
            })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );

        let request = DirectRequest::text("hi");
        // Effort names match exactly: no alias, case folding or clamping.
        client.model.reasoning = crate::ReasoningSelection::Effort("MAX".to_string());
        client.model.metadata_mut().capability = reasoning_capability();

        let err = client
            .complete(request)
            .await
            .expect_err("unsupported effort must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: LlmProfileEffortValidationCategory::UnsupportedEffort,
                ..
            }
        ));
        assert!(err.to_string().contains("Unsupported effort `MAX`"));
        assert!(
            !*called.lock_recover(),
            "the provider must not be called when the effort is rejected"
        );
    }

    #[tokio::test]
    async fn direct_client_sends_an_exact_effort_unchanged() {
        let captured: Arc<Mutex<Option<crate::ReasoningSelection>>> = Arc::new(Mutex::new(None));
        let captured_for_provider = Arc::clone(&captured);
        let provider = TestProvider::builder()
            .kind("direct-alias")
            .complete(move |request| {
                let captured = Arc::clone(&captured_for_provider);
                async move {
                    *captured.lock_recover() = Some(request.model.reasoning.clone());
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "ok".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );

        let request = DirectRequest::text("hi");
        client.model.reasoning = crate::ReasoningSelection::Effort("max".to_string());
        client.model.metadata_mut().capability = reasoning_capability();

        client.complete(request).await.expect("completion");
        let seen = captured
            .lock_recover()
            .clone()
            .expect("provider must be called");
        assert_eq!(
            seen,
            crate::ReasoningSelection::Effort("max".to_string()),
            "an advertised effort travels to the provider exactly as selected"
        );
    }

    #[tokio::test]
    async fn direct_client_rejects_effort_when_model_is_not_configurable() {
        let provider = TestProvider::builder()
            .kind("direct-not-configurable")
            .complete(|_request| async { Ok(LlmResponse::default()) })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );

        let request = DirectRequest::text("hi");
        client.model.reasoning = crate::ReasoningSelection::Effort("high".to_string());
        // No capability: the model exposes no configurable effort.

        let err = client
            .complete(request)
            .await
            .expect_err("effort on a non-configurable model must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: LlmProfileEffortValidationCategory::EffortNotConfigurable,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn direct_client_rejects_missing_mandatory_effort() {
        let provider = TestProvider::builder()
            .kind("direct-mandatory")
            .complete(|_request| async { Ok(LlmResponse::default()) })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );

        let mut capability = reasoning_capability();
        capability.reasoning.as_mut().expect("reasoning").mandatory = true;
        let request = DirectRequest::text("hi");
        client.model.metadata_mut().capability = capability;
        // No model_variant supplied, but the model requires one.

        let err = client
            .complete(request)
            .await
            .expect_err("missing mandatory effort must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: LlmProfileEffortValidationCategory::EffortRequired,
                ..
            }
        ));
    }

    #[test]
    fn build_llm_request_preserves_direct_stream_sender_and_adds_required_noop_sender() {
        let captured_events: Arc<Mutex<Vec<LlmStreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_sender = Arc::clone(&captured_events);
        let requested_sender = LlmEventSender::new(move |event| {
            captured_for_sender.lock_recover().push(event);
        });
        let mut request = DirectRequest::text("prompt");
        request.stream_events = Some(requested_sender);
        let provider = TestProvider::default().into_handle();

        let llm_request = build_llm_request(request, profile("model")).unwrap();
        let sender = transport_stream_events_for_direct(&provider, llm_request.stream_events)
            .expect("explicit direct stream sender must be preserved");
        sender.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: lash_protocol_types::llm::types::StreamBlockIdentity::new("text:0", 0),
            text: "delta".to_string(),
        }));
        assert_eq!(captured_events.lock_recover().len(), 1);

        let streaming_provider = TestProvider::builder()
            .requires_streaming(true)
            .build()
            .into_handle();
        let llm_request =
            build_llm_request(DirectRequest::text("prompt"), profile("model")).unwrap();
        assert!(
            llm_request.stream_events.is_none(),
            "the request alone names no sender the caller did not ask for"
        );
        assert!(
            transport_stream_events_for_direct(&streaming_provider, llm_request.stream_events)
                .is_some(),
            "providers that require streaming need a no-op sender even when direct caller did not request one"
        );
    }

    /// FIG-5491: a direct client's completions run under the budgets its
    /// host stated at creation. A model total far below the recommended
    /// preset's ends a call the provider never answers.
    #[tokio::test]
    async fn direct_completion_runs_under_the_budgets_its_client_states() {
        let short = std::time::Duration::from_millis(50);
        let budgets = crate::ExecutionBudgets::new(crate::ExecutionBudgetsConfig {
            model_total: short,
            provider: crate::ProviderAttemptLimits::new(short, short, short, 1)
                .expect("valid provider limits"),
            ..crate::ExecutionBudgetsConfig::recommended()
        })
        .expect("valid budgets");
        let provider = TestProvider::builder()
            .kind("direct-unanswered")
            .complete(|_request| std::future::pending::<Result<LlmResponse, LlmTransportError>>())
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(provider, profile("direct-model"), budgets);

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.complete(DirectRequest::text("hi")),
        )
        .await
        .expect("the stated model total ends the call")
        .expect_err("an unanswered call fails");
        let DirectLlmError::Transport(error) = error else {
            panic!("expected a transport error, got {error:?}");
        };
        assert_eq!(
            error.code,
            Some(crate::FailureCode::lash(
                crate::TurnFailureCode::ModelTotalExceeded
            )),
        );
    }
}

#[cfg(test)]
mod runtime_feedback_tests {
    use super::*;

    #[tokio::test]
    async fn direct_leading_system_is_refused_with_instructions_error() {
        let provider = crate::testing::TestProvider::default().into_handle();
        let mut client = DirectLlmClient::new(
            provider,
            profile("direct-model"),
            crate::ExecutionBudgets::recommended(),
        );
        let mut request = DirectRequest::text("user");
        request.messages.insert(
            0,
            DirectMessage {
                role: DirectRole::System,
                parts: vec![DirectPart::Text("ambiguous".into())],
            },
        );
        let error = client.complete(request).await.unwrap_err();
        assert!(matches!(error, DirectLlmError::LeadingSystemMessage));
        assert!(error.to_string().contains("instructions"));
    }

    #[test]
    fn direct_mid_conversation_system_messages_stay_feedback() {
        let mut request = DirectRequest::text("user");
        request.messages.extend([
            DirectMessage {
                role: DirectRole::Assistant,
                parts: vec![DirectPart::Text("partial".into())],
            },
            DirectMessage {
                role: DirectRole::System,
                parts: vec![DirectPart::Text("retry".into())],
            },
        ]);
        let normalized = build_llm_request(request, profile("model")).unwrap();
        assert_eq!(normalized.instructions, None);
        assert_eq!(
            normalized
                .messages
                .iter()
                .map(|m| m.role.clone())
                .collect::<Vec<_>>(),
            vec![LlmRole::User, LlmRole::Assistant, LlmRole::System]
        );
    }
}
