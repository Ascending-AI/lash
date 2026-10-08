use crate::AttachmentRef;
use crate::SchemaContract;
use crate::llm::transport::LlmTransportError;
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
        source: crate::ValueMismatch,
        result: Box<DirectLlmOutcome>,
    },
    #[error("transport error: {0}")]
    Transport(#[from] Box<LlmTransportError>),
}

impl DirectLlmError {
    pub fn value_mismatch(&self) -> Option<&crate::ValueMismatch> {
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
    telemetry_content: lash_trace::TelemetryContent,
    trace_context: TraceContext,
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
            telemetry_content: lash_trace::TelemetryContent::standard(),
            trace_context: TraceContext::default(),
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
    /// response. Defaults to [`lash_trace::TelemetryContent::standard`]
    /// (omitted).
    pub fn with_telemetry_content(mut self, content: lash_trace::TelemetryContent) -> Self {
        self.telemetry_content = content;
        self
    }

    pub fn with_trace_context(mut self, context: TraceContext) -> Self {
        self.trace_context = context;
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
        // A host-owned call made outside any journal: nothing replays it, so
        // the call is its own live attempt, and it belongs to no admitted
        // scope.
        let traced = self.trace_sink.as_ref().map(|sink| {
            let standing = crate::trace::TraceRuntime::new(Arc::clone(&self.clock))
                .with_trace_sink(Arc::clone(sink))
                .with_content(self.telemetry_content)
                .with_base_context(self.trace_context.clone())
                .unreplayed(None);
            let id = uuid::Uuid::new_v4().to_string();
            crate::runtime::effect::emit_llm_trace_started(
                &standing,
                TraceContext::default().for_llm_call(id.clone()),
                &llm_request,
            );
            (standing, id)
        });
        // No lash execution owns this call: the host that made it owns its
        // billing, and lash keeps no ledger row for it (ADR 0127).
        let sideband =
            lash_core_llm::core_internal::prepare_completion(&self.provider, &mut llm_request);
        let sideband = match &traced {
            Some((standing, id)) => standing
                .provider_attempts(sideband, TraceContext::default().for_llm_call(id.clone())),
            None => sideband,
        };
        let template = match self.provider.lower(&llm_request).await {
            Ok(template) => Arc::new(template),
            Err(error) => return Err(DirectLlmError::from(Box::new(error))),
        };
        match lash_core_llm::core_internal::complete_prepared(
            &mut self.provider,
            lash_sansio::llm::types::ResponseContext::of_request(&llm_request),
            &template,
            self.deliveries.as_ref(),
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &Default::default(),
            traced
                .as_ref()
                .and_then(|(standing, _)| standing.body_permit()),
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
                    if let Some((standing, llm_call_id)) = traced {
                        let call_record = match &error {
                            DirectLlmError::InvalidResponse { result, .. } => &result.llm_call,
                            _ => unreachable!("constructed InvalidResponse above"),
                        };
                        crate::runtime::effect::emit_llm_trace_failed(
                            &standing,
                            TraceContext::default().for_llm_call(llm_call_id),
                            crate::runtime::effect::LlmTraceFailure::invalid_structured_output(),
                            None,
                            Some(call_record),
                        );
                    }
                    return Err(error);
                }
                if let Some((standing, llm_call_id)) = traced {
                    crate::runtime::effect::emit_llm_trace_completed(
                        &standing,
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
                if let Some((standing, llm_call_id)) = traced {
                    crate::runtime::effect::emit_llm_trace_failed(
                        &standing,
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
    // durable position was selected from replay/ordinal before normalization.
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
    })
}

fn validate_direct_output(
    output: &DirectOutputSpec,
    response: &LlmResponse,
) -> Result<(), crate::ValueMismatch> {
    let DirectOutputSpec::JsonSchema(schema) = output else {
        return Ok(());
    };
    let response_text = response.full_text();
    let parsed: serde_json::Value =
        serde_json::from_str(response_text.trim()).map_err(|err| crate::ValueMismatch {
            instance_path: String::new(),
            message: format!("expected JSON: {err}"),
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
    use lash_sansio::sync::MutexExt;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct FrozenClock {
        instant: Instant,
    }

    impl FrozenClock {
        fn new() -> Self {
            Self {
                instant: Instant::now(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::Clock for FrozenClock {
        fn now(&self) -> Instant {
            self.instant
        }

        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            let timestamp_ms = 0;
            chrono::DateTime::from(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
            )
        }

        async fn sleep(&self, _duration: Duration) {}

        async fn sleep_until(&self, _deadline: Instant) {}
    }

    #[derive(Default)]
    struct CapturingTraceSink(Mutex<Vec<lash_trace::TraceRecord>>);

    impl TraceSink for CapturingTraceSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            self.0.lock_recover().push(record.clone());
            Ok(())
        }
    }

    fn canonical_trace_bytes(sink: &CapturingTraceSink) -> Vec<Vec<u8>> {
        sink.0
            .lock_recover()
            .iter()
            .map(|record| {
                let mut value = serde_json::to_value(record).expect("trace record is serializable");
                let object = value.as_object_mut().expect("trace record is an object");
                object.insert("id".to_string(), json!("trace-id"));
                let context = object
                    .get_mut("context")
                    .and_then(serde_json::Value::as_object_mut)
                    .expect("trace record has a context object");
                context.insert("llm_call_id".to_string(), json!("llm-call-id"));
                context.insert("graph_node_id".to_string(), json!("llm:llm-call-id"));
                serde_json::to_vec(&value).expect("canonical trace record is serializable")
            })
            .collect()
    }

    fn traced_client(
        provider: TestProvider,
        sink: &Arc<CapturingTraceSink>,
        clock: &Arc<FrozenClock>,
    ) -> DirectLlmClient {
        let trace_sink: Arc<dyn TraceSink> = sink.clone();
        let clock: Arc<dyn crate::Clock> = clock.clone();
        DirectLlmClient::new(
            provider.into_handle().with_clock(Arc::clone(&clock)),
            profile("trace-model"),
            crate::ExecutionBudgets::recommended(),
        )
        .with_trace_sink(Some(trace_sink))
        .with_telemetry_content(lash_trace::TelemetryContent::Captured)
        .with_clock(clock)
    }

    #[tokio::test]
    async fn direct_client_trace_records_preserve_current_bytes() {
        let sink = Arc::new(CapturingTraceSink::default());
        let clock = Arc::new(FrozenClock::new());

        let provider = TestProvider::builder()
            .kind("direct-trace-success")
            .complete(|_request| async {
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "direct success".to_string(),
                        response_meta: None,
                    }],
                    usage: LlmUsage {
                        input_tokens: 11,
                        output_tokens: 3,
                        ..Default::default()
                    },
                    terminal_reason: LlmTerminalReason::Stop,
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            })
            .build();
        let mut client = traced_client(provider, &sink, &clock);
        let response = client
            .complete(DirectRequest::text("trace success"))
            .await
            .expect("direct success should complete");
        assert_eq!(response.full_text(), "direct success");

        let provider = TestProvider::builder()
            .kind("direct-trace-failure")
            .complete_error("direct transport failure")
            .build();
        let mut client = traced_client(provider, &sink, &clock);
        let error = client
            .complete(DirectRequest::text("trace failure"))
            .await
            .expect_err("direct transport failure should be returned");
        assert!(matches!(error, DirectLlmError::Transport(_)));

        let provider = TestProvider::builder()
            .kind("direct-trace-structured-rejection")
            .complete(|_request| async {
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "{}".to_string(),
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
            .build();
        let mut client = traced_client(provider, &sink, &clock);
        let error = client
            .complete(DirectRequest::json_schema(
                "trace structured rejection",
                DirectJsonSchema {
                    name: "answer_shape".to_string(),
                    schema: lash_sansio::SchemaContract::admit(json!({
                        "type": "object",
                        "required": ["answer"],
                        "properties": {"answer": {"type": "string"}}
                    }))
                    .expect("valid declared schema"),
                    strict: true,
                },
            ))
            .await
            .expect_err("invalid structured output should be rejected");
        assert!(matches!(error, DirectLlmError::InvalidResponse { .. }));

        // A scheduled retry's delay carries jitter: check it against its
        // envelope, then take it out of the bytes that are pinned.
        let mut scheduled_delays_ms = Vec::new();
        let actual: Vec<String> = canonical_trace_bytes(&sink)
            .into_iter()
            .map(|bytes| {
                let mut record: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("trace record is JSON");
                let is_call_failure = record["type"] == "llm_call_failed";
                let attempts = match record.get_mut("attempts") {
                    Some(serde_json::Value::Array(attempts)) => attempts.iter_mut().collect(),
                    _ => record.get_mut("attempt").into_iter().collect::<Vec<_>>(),
                };
                for attempt in attempts {
                    let Some(delay) = attempt
                        .get_mut("retry_decision")
                        .and_then(serde_json::Value::as_object_mut)
                        .and_then(|decision| decision.remove("delay"))
                    else {
                        continue;
                    };
                    let delay: std::time::Duration =
                        serde_json::from_value(delay).expect("retry delay is a duration");
                    if is_call_failure {
                        scheduled_delays_ms.push(delay.as_millis());
                    }
                }
                serde_json::to_string(&record).expect("canonical trace record is serializable")
            })
            .collect();
        assert_eq!(scheduled_delays_ms.len(), 3, "{scheduled_delays_ms:?}");
        for (index, (delay_ms, (minimum, maximum))) in scheduled_delays_ms
            .iter()
            .zip([(2_000, 2_500), (4_000, 4_500), (8_000, 8_500)])
            .enumerate()
        {
            assert!(
                (minimum..=maximum).contains(delay_ms),
                "retry delay for attempt {index} must stay within the bounded jitter envelope, got {delay_ms} ms"
            );
        }
        // Keep the current trace contract byte pins literal. Each provider
        // attempt is its own record ahead of the call's outcome record, and
        // carries the same sealed attempt the outcome record lists.
        let expected = [
            r#"{"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace success"}],"role":"user"}],"model":"trace-model","stream":false,"tool_choice":"none"},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#.to_string(),
            r#"{"attempt":{"ordinal":1,"outcome":"completed","protocol_position":"terminal_observed","retry_budget_consumed":true,"usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":11,"output_tokens":3,"reasoning_output_tokens":0}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-success","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempts":[{"ordinal":1,"outcome":"completed","protocol_position":"terminal_observed","retry_budget_consumed":true,"usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":11,"output_tokens":3,"reasoning_output_tokens":0}}],"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","provider_usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":11,"output_tokens":3,"reasoning_output_tokens":0},"response":{"duration_ms":0,"parts":[{"Text":{"response_meta":null,"text":"direct success"}}],"request_model":"trace-model","terminal_reason":"stop","text":"direct success"},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_completed","usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":11,"output_tokens":3,"reasoning_output_tokens":0}}"#.to_string(),
            r#"{"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace failure"}],"role":"user"}],"model":"trace-model","stream":false,"tool_choice":"none"},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#.to_string(),
            r#"{"attempt":{"error":{"class":"unknown"},"ordinal":1,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-failure","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempt":{"error":{"class":"unknown"},"ordinal":2,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-failure","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempt":{"error":{"class":"unknown"},"ordinal":3,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-failure","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempt":{"error":{"class":"unknown"},"ordinal":4,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"cause":"retry_budget_exhausted","outcome":"declined"}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-failure","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempts":[{"error":{"class":"unknown"},"ordinal":1,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},{"error":{"class":"unknown"},"ordinal":2,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},{"error":{"class":"unknown"},"ordinal":3,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"class":{"class":"no_response"},"outcome":"scheduled","wait":"backoff"}},{"error":{"class":"unknown"},"ordinal":4,"outcome":"failed","protocol_position":"no_response","retry_budget_consumed":true,"retry_decision":{"cause":"retry_budget_exhausted","outcome":"declined"}}],"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"error":{"failure_kind":"unknown","retryable":true,"terminal_reason":"provider_error"},"id":"trace-id","schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_failed"}"#.to_string(),
            r#"{"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace structured rejection"}],"role":"user"}],"model":"trace-model","output_spec":{"name":"answer_shape","schema":{"canonical":{"properties":{"answer":{"type":"string"}},"required":["answer"],"type":"object"}},"strict":true,"type":"json_schema"},"stream":false,"tool_choice":"none"},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#.to_string(),
            r#"{"attempt":{"ordinal":1,"outcome":"completed","protocol_position":"terminal_observed","retry_budget_consumed":true,"usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":17,"output_tokens":3,"reasoning_output_tokens":0}},"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","observation":{"ended_at_ms":0,"provider":"direct-trace-structured-rejection","request_model":"trace-model","started_at_ms":0},"schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_attempt_completed"}"#.to_string(),
            r#"{"attempts":[{"ordinal":1,"outcome":"completed","protocol_position":"terminal_observed","retry_budget_consumed":true,"usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":17,"output_tokens":3,"reasoning_output_tokens":0}}],"content":"captured","context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"error":{"code":"lash:invalid_structured_output","failure_kind":"unknown","retryable":false,"terminal_reason":"provider_error"},"id":"trace-id","schema_version":36,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_failed"}"#.to_string(),
        ];

        assert_eq!(
            actual, expected,
            "direct trace records are the byte-level compatibility contract"
        );
    }

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
                schema: lash_sansio::SchemaContract::admit(json!({
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
            block: lash_sansio::llm::types::StreamBlockIdentity::new("text:0", 0),
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
