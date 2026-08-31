use crate::llm::transport::LlmTransportError;
use crate::llm::types::{
    AttachmentSource, LlmContentBlock, LlmEventSender, LlmJsonSchema, LlmMessage, LlmOutputSpec,
    LlmRequest, LlmRequestScope, LlmResponse, LlmRole, LlmStreamEvent, LlmToolChoice,
};
use crate::provider::{ModelCapability, ModelEffortValidationCategory, ProviderHandle};
use crate::{LashSchema, SchemaContract};
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
    Attachment(usize),
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
pub struct DirectRequest {
    pub model: String,
    #[serde(default)]
    pub model_variant: crate::ReasoningSelection,
    #[serde(default, skip_serializing_if = "ModelCapability::is_empty")]
    pub model_capability: ModelCapability,
    #[serde(default)]
    pub messages: Vec<DirectMessage>,
    #[serde(default)]
    pub attachments: Vec<AttachmentSource>,
    #[serde(default)]
    pub output: DirectOutputSpec,
    #[serde(default)]
    pub generation: crate::GenerationOptions,
    #[serde(default, skip)]
    pub stream_events: Option<LlmEventSender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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
    pub fn text(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            model_variant: crate::ReasoningSelection::ProviderDefault,
            model_capability: ModelCapability::default(),
            messages: vec![DirectMessage {
                role: DirectRole::User,
                parts: vec![DirectPart::Text(prompt.into())],
            }],
            attachments: Vec::new(),
            output: DirectOutputSpec::Text,
            generation: crate::GenerationOptions::default(),
            stream_events: None,
            session_id: None,
            caused_by: None,
            replay: None,
        }
    }

    pub fn json(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            output: DirectOutputSpec::JsonObject,
            ..Self::text(model, prompt)
        }
    }

    pub fn json_schema(
        model: impl Into<String>,
        prompt: impl Into<String>,
        schema: DirectJsonSchema,
    ) -> Self {
        Self {
            output: DirectOutputSpec::JsonSchema(schema),
            ..Self::text(model, prompt)
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
pub enum DirectLlmError {
    #[error("invalid request: {message}")]
    InvalidRequest {
        category: ModelEffortValidationCategory,
        message: String,
    },
    #[error("invalid response: {message}")]
    InvalidResponse {
        message: String,
        result: Box<DirectLlmOutcome>,
    },
    #[error("transport error: {0}")]
    Transport(#[from] Box<LlmTransportError>),
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
    trace_sink: Option<Arc<dyn TraceSink>>,
    trace_context: TraceContext,
    clock: Arc<dyn crate::Clock>,
}

impl DirectLlmClient {
    pub fn new(provider: ProviderHandle) -> Self {
        Self {
            provider,
            trace_sink: None,
            trace_context: TraceContext::default(),
            clock: Arc::new(crate::SystemClock),
        }
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn TraceSink>>) -> Self {
        self.trace_sink = sink;
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

    pub fn provider(&self) -> &ProviderHandle {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut ProviderHandle {
        &mut self.provider
    }

    pub async fn complete(
        &mut self,
        mut request: DirectRequest,
    ) -> Result<DirectLlmOutcome, DirectLlmError> {
        // Validate the requested effort against the capability that travels
        // with the request, and write the resolved (alias-normalized) effort
        // back so the provider never sees an un-clamped value.
        request.model_variant = request
            .model_capability
            .validate_selection(&request.model, self.provider.kind(), &request.model_variant)
            .map_err(|error| DirectLlmError::InvalidRequest {
                category: error.category,
                message: error.message,
            })?;

        let output_for_validation = request.output.clone();
        let model = request.model.clone();
        let llm_request = build_llm_request(&self.provider, request, model);
        let request_model = llm_request.model.clone();
        let llm_call_id = if self.trace_sink.is_some() {
            let id = uuid::Uuid::new_v4().to_string();
            crate::runtime::effect::emit_llm_trace_started(
                &self.trace_sink,
                &self.trace_context,
                TraceContext::default().for_llm_call(id.clone()),
                &llm_request,
                self.clock.as_ref(),
            );
            Some(id)
        } else {
            None
        };
        match self.provider.complete(llm_request).await {
            Ok(response) => {
                let result = DirectLlmOutcome {
                    response: response.response,
                    llm_call: response.call_record,
                };
                if let Err(message) =
                    validate_direct_output(&output_for_validation, &result.response)
                {
                    let error = DirectLlmError::InvalidResponse {
                        message,
                        result: Box::new(result),
                    };
                    if let Some(llm_call_id) = llm_call_id {
                        let call_record = match &error {
                            DirectLlmError::InvalidResponse { result, .. } => &result.llm_call,
                            _ => unreachable!("constructed InvalidResponse above"),
                        };
                        crate::runtime::effect::emit_llm_trace_failed(
                            &self.trace_sink,
                            &self.trace_context,
                            TraceContext::default().for_llm_call(llm_call_id),
                            crate::runtime::effect::LlmTraceFailure::invalid_structured_output(
                                error.to_string(),
                            ),
                            None,
                            Some(call_record),
                            self.clock.as_ref(),
                        );
                    }
                    return Err(error);
                }
                if let Some(llm_call_id) = llm_call_id {
                    crate::runtime::effect::emit_llm_trace_completed(
                        &self.trace_sink,
                        &self.trace_context,
                        TraceContext::default().for_llm_call(llm_call_id),
                        &result.response,
                        &request_model,
                        0,
                        None,
                        Some(&result.llm_call),
                        self.clock.as_ref(),
                    );
                }
                Ok(result)
            }
            Err(error) => {
                if let Some(llm_call_id) = llm_call_id {
                    crate::runtime::effect::emit_llm_trace_failed(
                        &self.trace_sink,
                        &self.trace_context,
                        TraceContext::default().for_llm_call(llm_call_id),
                        crate::runtime::effect::LlmTraceFailure::from(&error.error),
                        None,
                        Some(&error.call_record),
                        self.clock.as_ref(),
                    );
                }
                Err(DirectLlmError::from(Box::new(error.error)))
            }
        }
    }
}

pub(crate) fn build_llm_request(
    provider: &ProviderHandle,
    request: DirectRequest,
    model: String,
) -> LlmRequest {
    let stream_events = transport_stream_events_for_direct(provider, request.stream_events);
    let DirectRequest {
        model: _,
        model_variant,
        model_capability,
        messages,
        attachments,
        output,
        generation,
        stream_events: _,
        session_id,
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
                DirectPart::Attachment(idx) => {
                    blocks.push(LlmContentBlock::Attachment {
                        attachment_idx: idx,
                    });
                }
            }
        }
        if !blocks.is_empty() {
            llm_messages.push(LlmMessage::new(role, blocks));
        }
    }

    let scope = match session_id {
        // This request id is transport metadata for the DirectRequest path;
        // its durable position was selected from replay/ordinal before
        // normalization. Callers of direct_llm_completion must instead supply
        // their own per-logical-call request id.
        Some(session_id) => LlmRequestScope::new(
            session_id.clone(),
            format!("{session_id}:frame:direct"),
            format!("{session_id}:direct"),
        ),
        None => {
            let request_id = uuid::Uuid::new_v4().to_string();
            LlmRequestScope::new(
                format!("direct:{request_id}"),
                format!("direct:{request_id}:frame"),
                request_id,
            )
        }
    };

    LlmRequest {
        model,
        messages: llm_messages,
        attachments,
        resolved_stored: Default::default(),
        tools: Vec::new().into(),
        tool_choice: LlmToolChoice::None,
        model_variant,
        model_capability,
        generation,
        scope,
        output_spec,
        stream_events,
        provider_trace: None,
    }
}

fn validate_direct_output(output: &DirectOutputSpec, response: &LlmResponse) -> Result<(), String> {
    let DirectOutputSpec::JsonSchema(schema) = output else {
        return Ok(());
    };
    let response_text = response.full_text();
    let parsed: serde_json::Value = serde_json::from_str(response_text.trim())
        .map_err(|err| format!("expected JSON: {err}"))?;
    LashSchema::new(schema.schema.canonical().clone()).validate(&parsed)
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
mod tests {
    use super::*;
    use crate::llm::types::{LlmOutputPart, LlmTerminalReason, LlmUsage};
    use crate::provider::{ProviderOptions, ProviderReliability};
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

        fn timestamp_ms(&self) -> u64 {
            0
        }

        fn timestamp_rfc3339(&self) -> String {
            self.timestamp_datetime().to_rfc3339()
        }

        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from(std::time::UNIX_EPOCH)
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
        DirectLlmClient::new(provider.into_handle().with_clock(Arc::clone(&clock)))
            .with_trace_sink(Some(trace_sink))
            .with_clock(clock)
    }

    #[test]
    fn json_schema_request_preserves_output_schema() {
        let schema = DirectJsonSchema {
            name: "answer_shape".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "answer": { "type": "string" }
                },
                "required": ["answer"]
            })
            .into(),
            strict: true,
        };

        let request = DirectRequest::json_schema("model-a", "return json", schema.clone());

        assert_eq!(
            request.output,
            DirectOutputSpec::JsonSchema(schema),
            "DirectRequest::json_schema must carry the requested output schema"
        );
    }

    #[test]
    fn direct_client_provider_accessors_expose_owned_provider_handle() {
        let provider = TestProvider::builder()
            .kind("direct-accessor-provider")
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(provider);

        assert_eq!(client.provider().kind(), "direct-accessor-provider");

        let options = ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(7),
            max_output_tokens: Some(123),
            ..Default::default()
        };
        client.provider_mut().set_options(options.clone());

        assert_eq!(client.provider().options(), options);
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
            .complete(DirectRequest::text("trace-model", "trace success"))
            .await
            .expect("direct success should complete");
        assert_eq!(response.full_text(), "direct success");

        let provider = TestProvider::builder()
            .kind("direct-trace-failure")
            .complete_error("direct transport failure")
            .build();
        let mut client = traced_client(provider, &sink, &clock);
        let error = client
            .complete(DirectRequest::text("trace-model", "trace failure"))
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
                "trace-model",
                "trace structured rejection",
                DirectJsonSchema {
                    name: "answer_shape".to_string(),
                    schema: json!({
                        "type": "object",
                        "required": ["answer"],
                        "properties": {"answer": {"type": "string"}}
                    })
                    .into(),
                    strict: true,
                },
            ))
            .await
            .expect_err("invalid structured output should be rejected");
        assert!(matches!(error, DirectLlmError::InvalidResponse { .. }));

        let actual: Vec<String> = canonical_trace_bytes(&sink)
            .into_iter()
            .map(|bytes| String::from_utf8(bytes).expect("trace bytes are UTF-8"))
            .collect();
        let expected = [
                r#"{"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace success"}],"role":"user"}],"model":"trace-model","stream":false,"tool_choice":"none"},"schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#
                    .to_string(),
                r#"{"attempts":[{"duration_ms":0,"ordinal":1,"outcome":"completed"}],"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","response":{"duration_ms":0,"parts":[{"text":"direct success","type":"text"}],"request_model":"trace-model","terminal_reason":"stop","text":"direct success"},"schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_completed","usage":{"cache_read_input_tokens":0,"cache_write_input_tokens":0,"input_tokens":11,"output_tokens":3,"reasoning_output_tokens":0}}"#
                    .to_string(),
                r#"{"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace failure"}],"role":"user"}],"model":"trace-model","stream":false,"tool_choice":"none"},"schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#
                    .to_string(),
                r#"{"attempts":[{"delay_ms":2000,"duration_ms":0,"ordinal":1,"outcome":"failed","reason":"unknown; retry: failure_before_response"},{"delay_ms":4000,"duration_ms":0,"ordinal":2,"outcome":"failed","reason":"unknown; retry: failure_before_response"},{"delay_ms":8000,"duration_ms":0,"ordinal":3,"outcome":"failed","reason":"unknown; retry: failure_before_response"},{"duration_ms":0,"ordinal":4,"outcome":"failed","reason":"unknown; retry: retry_budget_exhausted"}],"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"error":{"message":"direct transport failure","retryable":true,"terminal_reason":"provider_error"},"id":"trace-id","schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_failed"}"#
                    .to_string(),
                r#"{"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"id":"trace-id","request":{"messages":[{"blocks":[{"kind":"text","text":"trace structured rejection"}],"role":"user"}],"model":"trace-model","output_spec":{"name":"answer_shape","schema":{"canonical":{"properties":{"answer":{"type":"string"}},"required":["answer"],"type":"object"}},"strict":true,"type":"json_schema"},"stream":false,"tool_choice":"none"},"schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_started"}"#
                    .to_string(),
                r#"{"attempts":[{"duration_ms":0,"ordinal":1,"outcome":"completed"}],"context":{"graph_node_id":"llm:llm-call-id","llm_call_id":"llm-call-id"},"error":{"code":"invalid_structured_output","message":"invalid response: \"answer\" is a required property","retryable":false,"terminal_reason":"provider_error"},"id":"trace-id","schema_version":14,"timestamp":"1970-01-01T00:00:00+00:00","type":"llm_call_failed"}"#
                    .to_string(),
            ];

        {
            let failure_trace: serde_json::Value =
                serde_json::from_str(&actual[3]).expect("failed trace record is JSON");
            let attempts = failure_trace["attempts"]
                .as_array()
                .expect("failed trace record has attempts");
            for (index, (attempt, (minimum, maximum))) in attempts
                .iter()
                .take(3)
                .zip([(2_000, 2_500), (4_000, 4_500), (8_000, 8_500)])
                .enumerate()
            {
                let delay_ms = attempt["delay_ms"]
                    .as_u64()
                    .expect("retry attempt has a delay");
                assert!(
                    (minimum..=maximum).contains(&delay_ms),
                    "retry delay for attempt {index} must stay within the bounded jitter envelope, got {delay_ms} ms"
                );
            }
        }

        let mut actual_failure: serde_json::Value =
            serde_json::from_str(&actual[3]).expect("failed trace record is JSON");
        let mut expected_failure: serde_json::Value =
            serde_json::from_str(&expected[3]).expect("expected failed trace record is JSON");
        for trace in [&mut actual_failure, &mut expected_failure] {
            for attempt in trace["attempts"]
                .as_array_mut()
                .expect("trace record has attempts")
                .iter_mut()
                .take(3)
            {
                attempt
                    .as_object_mut()
                    .expect("attempt is an object")
                    .remove("delay_ms");
            }
        }

        assert_eq!(
            &actual[..3],
            &expected[..3],
            "stable direct trace records are the byte-level compatibility contract"
        );
        assert_eq!(actual_failure, expected_failure);
        assert_eq!(
            &actual[4..],
            &expected[4..],
            "stable direct trace records are the byte-level compatibility contract"
        );
    }

    #[tokio::test]
    async fn direct_client_complete_delegates_to_provider_and_returns_response() {
        let captured_request: Arc<Mutex<Option<LlmRequest>>> = Arc::new(Mutex::new(None));
        let captured_for_provider = Arc::clone(&captured_request);
        let provider = TestProvider::builder()
            .kind("direct-complete-provider")
            .complete(move |request| {
                let captured_for_provider = Arc::clone(&captured_for_provider);
                async move {
                    *captured_for_provider.lock_recover() = Some(request);
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "provider delegated response".to_string(),
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
                }
            })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(provider);
        let mut request = DirectRequest::json("direct-model", "answer as json");
        request.session_id = Some("direct-session".to_string());

        let response = client
            .complete(request)
            .await
            .expect("direct completion should delegate");

        assert_eq!(response.full_text(), "provider delegated response");
        assert_eq!(response.llm_call.attempts.len(), 1);
        let captured = captured_request
            .lock_recover()
            .clone()
            .expect("provider should receive a request");
        assert_eq!(captured.model, "direct-model");
        assert_eq!(captured.scope.session_id, "direct-session");
        assert_eq!(captured.scope.agent_frame_id, "direct-session:frame:direct");
        assert_eq!(captured.scope.request_id, "direct-session:direct");
        assert!(matches!(
            captured.output_spec,
            Some(LlmOutputSpec::JsonObject)
        ));
        assert_eq!(captured.messages.len(), 1);
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
        let mut client = DirectLlmClient::new(provider);
        let request = DirectRequest::json_schema(
            "direct-model",
            "return items",
            DirectJsonSchema {
                name: "items_result".to_string(),
                schema: json!({
                    "type": "object",
                    "required": ["items"],
                    "properties": {
                        "items": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "type": "string" }
                        }
                    }
                })
                .into(),
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

    fn reasoning_capability() -> ModelCapability {
        ModelCapability {
            reasoning: Some(crate::ReasoningCapability {
                efforts: ["low", "medium", "high", "max"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                aliases: std::collections::BTreeMap::from([(
                    "xhigh".to_string(),
                    "max".to_string(),
                )]),
                ..Default::default()
            }),
            cache_control: None,
            stream_termination: None,
            sampling: crate::SamplingCapability::Configurable,
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
        let mut client = DirectLlmClient::new(provider);

        let mut request = DirectRequest::text("direct-model", "hi");
        request.model_variant = crate::ReasoningSelection::Effort("turbo".to_string());
        request.model_capability = reasoning_capability();

        let err = client
            .complete(request)
            .await
            .expect_err("unsupported effort must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: ModelEffortValidationCategory::UnsupportedEffort,
                ..
            }
        ));
        assert!(err.to_string().contains("Unsupported effort `turbo`"));
        assert!(
            !*called.lock_recover(),
            "the provider must not be called when the effort is rejected"
        );
    }

    #[tokio::test]
    async fn direct_client_normalizes_alias_effort_into_outgoing_request() {
        let captured: Arc<Mutex<Option<crate::ReasoningSelection>>> = Arc::new(Mutex::new(None));
        let captured_for_provider = Arc::clone(&captured);
        let provider = TestProvider::builder()
            .kind("direct-alias")
            .complete(move |request| {
                let captured = Arc::clone(&captured_for_provider);
                async move {
                    *captured.lock_recover() = Some(request.model_variant.clone());
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
        let mut client = DirectLlmClient::new(provider);

        let mut request = DirectRequest::text("direct-model", "hi");
        request.model_variant = crate::ReasoningSelection::Effort("XHigh".to_string());
        request.model_capability = reasoning_capability();

        client.complete(request).await.expect("completion");
        let seen = captured
            .lock_recover()
            .clone()
            .expect("provider must be called");
        assert_eq!(
            seen,
            crate::ReasoningSelection::Effort("max".to_string()),
            "alias `XHigh` must clamp to canonical `max` before the provider sees the request"
        );
    }

    #[tokio::test]
    async fn direct_client_rejects_effort_when_model_is_not_configurable() {
        let provider = TestProvider::builder()
            .kind("direct-not-configurable")
            .complete(|_request| async { Ok(LlmResponse::default()) })
            .build()
            .into_handle();
        let mut client = DirectLlmClient::new(provider);

        let mut request = DirectRequest::text("direct-model", "hi");
        request.model_variant = crate::ReasoningSelection::Effort("high".to_string());
        // No capability: the model exposes no configurable effort.

        let err = client
            .complete(request)
            .await
            .expect_err("effort on a non-configurable model must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: ModelEffortValidationCategory::EffortNotConfigurable,
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
        let mut client = DirectLlmClient::new(provider);

        let mut capability = reasoning_capability();
        capability.reasoning.as_mut().expect("reasoning").mandatory = true;
        let mut request = DirectRequest::text("direct-model", "hi");
        request.model_capability = capability;
        // No model_variant supplied, but the model requires one.

        let err = client
            .complete(request)
            .await
            .expect_err("missing mandatory effort must be rejected");
        assert!(matches!(
            err,
            DirectLlmError::InvalidRequest {
                category: ModelEffortValidationCategory::EffortRequired,
                ..
            }
        ));
    }

    #[test]
    fn build_llm_request_preserves_nonempty_content_and_drops_empty_messages() {
        let provider = TestProvider::default().into_handle();
        let request = DirectRequest {
            model: "input-model".to_string(),
            messages: vec![
                DirectMessage {
                    role: DirectRole::System,
                    parts: vec![DirectPart::Text(String::new())],
                },
                DirectMessage {
                    role: DirectRole::User,
                    parts: vec![
                        DirectPart::Text("hello".to_string()),
                        DirectPart::Text(String::new()),
                    ],
                },
                DirectMessage {
                    role: DirectRole::Assistant,
                    parts: vec![DirectPart::Attachment(2)],
                },
            ],
            attachments: Vec::new(),
            output: DirectOutputSpec::Text,
            generation: crate::GenerationOptions::default(),
            stream_events: None,
            session_id: None,
            model_variant: Default::default(),
            model_capability: ModelCapability::default(),
            caused_by: None,
            replay: None,
        };

        let llm_request = build_llm_request(&provider, request, "transport-model".to_string());

        assert_eq!(llm_request.model, "transport-model");
        assert_eq!(
            llm_request.messages.len(),
            2,
            "empty normalized messages must be dropped"
        );
        assert_eq!(llm_request.messages[0].role, LlmRole::User);
        assert_eq!(llm_request.messages[0].blocks.len(), 1);
        assert!(matches!(
            &llm_request.messages[0].blocks[0],
            LlmContentBlock::Text { text, .. } if text.as_ref() == "hello"
        ));
        assert_eq!(llm_request.messages[1].role, LlmRole::Assistant);
        assert!(matches!(
            &llm_request.messages[1].blocks[0],
            LlmContentBlock::Attachment { attachment_idx: 2 }
        ));
    }

    #[test]
    fn build_llm_request_preserves_direct_stream_sender_and_adds_required_noop_sender() {
        let captured_events: Arc<Mutex<Vec<LlmStreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_sender = Arc::clone(&captured_events);
        let requested_sender = LlmEventSender::new(move |event| {
            captured_for_sender.lock_recover().push(event);
        });
        let mut request = DirectRequest::text("model", "prompt");
        request.stream_events = Some(requested_sender);
        let provider = TestProvider::default().into_handle();

        let llm_request = build_llm_request(&provider, request, "model".to_string());
        let sender = llm_request
            .stream_events
            .expect("explicit direct stream sender must be preserved");
        sender.send(LlmStreamEvent::Delta("delta".to_string()));
        assert_eq!(captured_events.lock_recover().len(), 1);

        let streaming_provider = TestProvider::builder()
            .requires_streaming(true)
            .build()
            .into_handle();
        let llm_request = build_llm_request(
            &streaming_provider,
            DirectRequest::text("model", "prompt"),
            "model".to_string(),
        );
        assert!(
            llm_request.stream_events.is_some(),
            "providers that require streaming need a no-op sender even when direct caller did not request one"
        );
    }
}
