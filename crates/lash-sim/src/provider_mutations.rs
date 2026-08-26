use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmRequest, LlmRole, LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::{DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier};
use lash_llm_transport::LlmHttpTransport;
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
use serde_json::{Value, json};

use crate::canonical_scripts::{
    ANTHROPIC_MESSAGES_TEXT, GOOGLE_STREAM_GENERATE_TEXT, OPENAI_COMPAT_DISCONNECT,
    OPENAI_COMPAT_RATE_LIMIT, OPENAI_COMPAT_RESPONSE_START_TIMEOUT,
    OPENAI_COMPAT_STREAM_CHUNK_TIMEOUT, OPENAI_COMPAT_TOOL_CALL, OPENAI_RESPONSES_TEXT,
};
use crate::provider::{ProviderWireScript, ScriptedLlmHttpTransport};
use crate::scheduler::BoundaryEvent;
use crate::trace::value_digest;

#[derive(Debug)]
pub struct ProviderMutationExecutionError {
    message: String,
}

impl ProviderMutationExecutionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProviderMutationExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "provider mutation execution failed: {}", self.message)
    }
}

impl std::error::Error for ProviderMutationExecutionError {}

impl From<serde_json::Error> for ProviderMutationExecutionError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<lash_core::facade_support::LlmTransportError> for ProviderMutationExecutionError {
    fn from(value: lash_core::facade_support::LlmTransportError) -> Self {
        Self::new(value.to_string())
    }
}

/// Transport- and HTTP-level mutation classes (distinct from the JSON
/// parser-error and rate-limit classes). Each perturbs the wire transport while
/// preserving provider-native framing and is classified on its own named
/// failure path. Generator-driven selection draws from this set so live runs
/// exercise socket disconnects, response-start/chunk timeouts, and retryable
/// 5xx sequences, not only the fixed parser-rejection corpus.
pub const TRANSPORT_PROVIDER_MUTATIONS: &[&str] = &[
    "mid_stream_disconnect",
    "response_start_timeout",
    "stream_chunk_timeout",
    "retryable_server_error_sequence",
];

/// Whether a mutation class is one of the transport/HTTP perturbation classes.
pub fn is_transport_provider_mutation(mutation: &str) -> bool {
    TRANSPORT_PROVIDER_MUTATIONS.contains(&mutation)
}

#[derive(Default)]
pub struct ProviderMutationMatrixCache {
    by_mutation: BTreeMap<String, Value>,
}

impl ProviderMutationMatrixCache {
    pub async fn augment_observation(
        &mut self,
        event: &BoundaryEvent,
        mut observed: Value,
    ) -> Result<Value, ProviderMutationExecutionError> {
        let mutation = event
            .payload
            .get("mutation")
            .and_then(Value::as_str)
            .unwrap_or("unknown_mutation")
            .to_string();
        let matrix = if let Some(matrix) = self.by_mutation.get(&mutation) {
            matrix.clone()
        } else {
            let matrix = execute_provider_mutation_matrix(&mutation).await?;
            self.by_mutation.insert(mutation.clone(), matrix.clone());
            matrix
        };
        observed
            .as_object_mut()
            .ok_or_else(|| {
                ProviderMutationExecutionError::new(
                    "provider mutation observation was not an object",
                )
            })?
            .insert("provider_parser_matrix".to_string(), matrix);
        Ok(observed)
    }
}

pub async fn execute_provider_mutation_matrix(
    mutation: &str,
) -> Result<Value, ProviderMutationExecutionError> {
    let specs = mutation_specs(mutation)?;
    let mut proofs = Vec::with_capacity(specs.len());
    for spec in specs {
        proofs.push(run_mutation_script(spec).await?);
    }
    let provider_kinds = proofs
        .iter()
        .filter_map(|proof| proof.get("provider_kind").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let matrix = json!({
        "schema": "lash.sim.provider-mutation-parser-matrix.v1",
        "mutation": mutation,
        "real_provider_parser_execution": true,
        "provider_kinds": provider_kinds,
        "proof_count": proofs.len(),
        "proofs": proofs,
    });
    Ok(json!({
        "matrix": matrix,
        "digest": value_digest(&matrix),
    }))
}

struct MutationScriptSpec {
    proof_name: &'static str,
    provider_kind: &'static str,
    script_content: String,
    request_kind: MutationRequestKind,
    expected_status: Option<u16>,
    /// When set, the real provider failure classifier must report this
    /// `ProviderFailureKind` (e.g. `Stream`, `Timeout`, `Http`), proving the
    /// mutation class drives a distinct, named failure path and not just a
    /// generic parse error.
    expected_kind: Option<&'static str>,
    /// When set, the classified failure's `retryable` flag must match, so a
    /// retryable transport/server fault is never silently treated as terminal.
    expected_retryable: Option<bool>,
}

impl MutationScriptSpec {
    fn parser_error(
        proof_name: &'static str,
        provider_kind: &'static str,
        script_content: String,
        request_kind: MutationRequestKind,
    ) -> Self {
        Self {
            proof_name,
            provider_kind,
            script_content,
            request_kind,
            expected_status: None,
            expected_kind: None,
            expected_retryable: None,
        }
    }
}

enum MutationRequestKind {
    OpenAiCompatible { stream: bool },
    OpenAiResponses,
    Anthropic,
    Google { stream: bool },
}

fn mutation_specs(
    mutation: &str,
) -> Result<Vec<MutationScriptSpec>, ProviderMutationExecutionError> {
    match mutation {
        "malformed_sse_chunk" => malformed_parser_specs("malformed-sse", "malformed_sse_chunk"),
        "dropped_terminal_event" => {
            malformed_parser_specs("dropped-terminal-event", "dropped_terminal_event")
        }
        "duplicate_tool_call_delta" => {
            malformed_parser_specs("duplicate-tool-call-delta", "duplicate_tool_call_delta")
        }
        "wrong_provider_schema" => {
            malformed_parser_specs("wrong-provider-schema", "wrong_provider_schema")
        }
        "rate_limit_error_envelope" => Ok(vec![
            MutationScriptSpec {
                proof_name: "mutation.rate-limit.openai-compatible",
                provider_kind: "openai-compatible",
                script_content: OPENAI_COMPAT_RATE_LIMIT.to_string(),
                request_kind: MutationRequestKind::OpenAiCompatible { stream: false },
                expected_status: Some(429),
                expected_kind: None,
                expected_retryable: None,
            },
            MutationScriptSpec {
                proof_name: "mutation.rate-limit.openai",
                provider_kind: "openai",
                script_content: rate_limit_script(
                    OPENAI_RESPONSES_TEXT,
                    "openai.responses-rate-limit-429",
                    json!({
                        "error": {
                            "message": "Rate limit reached for responses",
                            "type": "rate_limit_error",
                            "code": "rate_limit_exceeded"
                        }
                    }),
                )?,
                request_kind: MutationRequestKind::OpenAiResponses,
                expected_status: Some(429),
                expected_kind: None,
                expected_retryable: None,
            },
            MutationScriptSpec {
                proof_name: "mutation.rate-limit.anthropic",
                provider_kind: "anthropic",
                script_content: rate_limit_script(
                    ANTHROPIC_MESSAGES_TEXT,
                    "anthropic.messages-rate-limit-429",
                    json!({
                        "type": "error",
                        "error": {
                            "type": "rate_limit_error",
                            "message": "rate limit exceeded"
                        }
                    }),
                )?,
                request_kind: MutationRequestKind::Anthropic,
                expected_status: Some(429),
                expected_kind: None,
                expected_retryable: None,
            },
        ]),
        "mid_stream_disconnect"
        | "response_start_timeout"
        | "stream_chunk_timeout"
        | "retryable_server_error_sequence" => transport_mutation_specs(mutation),
        other => Err(ProviderMutationExecutionError::new(format!(
            "unknown provider mutation `{other}`"
        ))),
    }
}

fn malformed_parser_specs(
    script_stem: &'static str,
    mutation: &'static str,
) -> Result<Vec<MutationScriptSpec>, ProviderMutationExecutionError> {
    Ok(vec![
        MutationScriptSpec::parser_error(
            "mutation.parser-error.openai-compatible",
            "openai-compatible",
            malformed_sse_script(
                OPENAI_COMPAT_TOOL_CALL,
                &format!("openai-compatible.chat-{script_stem}"),
                mutation,
            )?,
            MutationRequestKind::OpenAiCompatible { stream: true },
        ),
        MutationScriptSpec::parser_error(
            "mutation.parser-error.openai",
            "openai",
            malformed_sse_script(
                OPENAI_RESPONSES_TEXT,
                &format!("openai.responses-{script_stem}"),
                mutation,
            )?,
            MutationRequestKind::OpenAiResponses,
        ),
        MutationScriptSpec::parser_error(
            "mutation.parser-error.anthropic",
            "anthropic",
            malformed_sse_script(
                ANTHROPIC_MESSAGES_TEXT,
                &format!("anthropic.messages-{script_stem}"),
                mutation,
            )?,
            MutationRequestKind::Anthropic,
        ),
        MutationScriptSpec::parser_error(
            "mutation.parser-error.google",
            "google_oauth",
            malformed_sse_script(
                GOOGLE_STREAM_GENERATE_TEXT,
                &format!("google.stream-generate-content-{script_stem}"),
                mutation,
            )?,
            MutationRequestKind::Google { stream: true },
        ),
    ])
}

/// Transport- and HTTP-level mutation classes that run a real, schema-valid
/// provider wire script through the production provider and assert the failure
/// is classified on a distinct named path. Unlike the parser-error classes,
/// these never corrupt the JSON payload — they perturb the *transport* (a
/// mid-stream socket close, a response-start or chunk read timeout, or a
/// retryable 5xx) while preserving provider-native wire framing.
fn transport_mutation_specs(
    mutation: &str,
) -> Result<Vec<MutationScriptSpec>, ProviderMutationExecutionError> {
    let spec = match mutation {
        "mid_stream_disconnect" => MutationScriptSpec {
            proof_name: "mutation.transport.mid-stream-disconnect.openai-compatible",
            provider_kind: "openai-compatible",
            script_content: OPENAI_COMPAT_DISCONNECT.to_string(),
            request_kind: MutationRequestKind::OpenAiCompatible { stream: true },
            expected_status: None,
            expected_kind: Some("Stream"),
            expected_retryable: Some(true),
        },
        "response_start_timeout" => MutationScriptSpec {
            proof_name: "mutation.transport.response-start-timeout.openai-compatible",
            provider_kind: "openai-compatible",
            script_content: OPENAI_COMPAT_RESPONSE_START_TIMEOUT.to_string(),
            request_kind: MutationRequestKind::OpenAiCompatible { stream: true },
            expected_status: None,
            expected_kind: Some("Timeout"),
            expected_retryable: Some(true),
        },
        "stream_chunk_timeout" => MutationScriptSpec {
            proof_name: "mutation.transport.stream-chunk-timeout.openai-compatible",
            provider_kind: "openai-compatible",
            script_content: OPENAI_COMPAT_STREAM_CHUNK_TIMEOUT.to_string(),
            request_kind: MutationRequestKind::OpenAiCompatible { stream: true },
            expected_status: None,
            expected_kind: Some("Timeout"),
            expected_retryable: Some(true),
        },
        "retryable_server_error_sequence" => MutationScriptSpec {
            proof_name: "mutation.transport.retryable-503.openai-compatible",
            provider_kind: "openai-compatible",
            script_content: retryable_server_error_script(
                OPENAI_COMPAT_TOOL_CALL,
                "openai-compatible.chat-retryable-503",
            )?,
            request_kind: MutationRequestKind::OpenAiCompatible { stream: true },
            expected_status: Some(503),
            expected_kind: None,
            expected_retryable: Some(true),
        },
        other => {
            return Err(ProviderMutationExecutionError::new(format!(
                "unknown transport mutation `{other}`"
            )));
        }
    };
    Ok(vec![spec])
}

fn retryable_server_error_script(
    script: &str,
    name: &str,
) -> Result<String, ProviderMutationExecutionError> {
    let mut value: Value = serde_json::from_str(script)?;
    value["name"] = Value::String(name.to_string());
    value["timeline"] = json!([
        {
            "at": 10,
            "event": "http_error",
            "status": 503,
            "headers": [
                { "name": "content-type", "value": "application/json" },
                { "name": "retry-after", "value": "1" },
                { "name": "x-request-id", "value": format!("req-{name}") }
            ],
            "body": json!({
                "error": {
                    "message": "The server is overloaded, please retry",
                    "type": "server_error",
                    "code": "service_unavailable"
                }
            })
            .to_string()
        }
    ]);
    value["expected_provider"] = json!({
        "mutation": "retryable_server_error_sequence",
        "status": 503,
        "retryable": true,
    });
    Ok(serde_json::to_string(&value)?)
}

fn malformed_sse_script(
    script: &str,
    name: &str,
    mutation: &str,
) -> Result<String, ProviderMutationExecutionError> {
    let mut value: Value = serde_json::from_str(script)?;
    value["name"] = Value::String(name.to_string());
    let timeline = value
        .get_mut("timeline")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ProviderMutationExecutionError::new("script timeline was not an array"))?;
    let Some(event) = timeline
        .iter_mut()
        .find(|event| event.get("event").and_then(Value::as_str) == Some("sse"))
    else {
        return Err(ProviderMutationExecutionError::new(
            "script had no SSE event to mutate",
        ));
    };
    let (behavior, data) = match mutation {
        "malformed_sse_chunk" => ("invalid_json_sse_chunk", "{ malformed provider event"),
        "dropped_terminal_event" => (
            "truncated_terminal_event_payload",
            r#"{"terminal_event":"dropped""#,
        ),
        "duplicate_tool_call_delta" => (
            "truncated_duplicate_tool_call_delta",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"lookup","arguments":""#,
        ),
        "wrong_provider_schema" => (
            "wrong_provider_schema_payload",
            r#"{"provider":"wrong","payload":["#,
        ),
        _ => ("unknown_parser_mutation", "{ malformed provider event"),
    };
    event["data"] = Value::String(data.to_string());
    value["expected_provider"] = json!({
        "mutation": mutation,
        "behavior": behavior,
        "expected": "provider parser error",
    });
    Ok(serde_json::to_string(&value)?)
}

fn rate_limit_script(
    script: &str,
    name: &str,
    body: Value,
) -> Result<String, ProviderMutationExecutionError> {
    let mut value: Value = serde_json::from_str(script)?;
    value["name"] = Value::String(name.to_string());
    value["timeline"] = json!([
        {
            "at": 10,
            "event": "http_error",
            "status": 429,
            "headers": [
                { "name": "content-type", "value": "application/json" },
                { "name": "retry-after", "value": "3" },
                { "name": "x-request-id", "value": format!("req-{name}") }
            ],
            "body": body.to_string()
        }
    ]);
    value["expected_provider"] = json!({
        "mutation": "rate_limit_error_envelope",
        "status": 429,
    });
    Ok(serde_json::to_string(&value)?)
}

async fn run_mutation_script(
    spec: MutationScriptSpec,
) -> Result<Value, ProviderMutationExecutionError> {
    let parsed = ProviderWireScript::from_json_str(&spec.script_content)?;
    let script_name = parsed.name.clone();
    let mutation_behavior = parsed
        .expected_provider
        .as_ref()
        .and_then(|expected| expected.get("behavior"))
        .and_then(Value::as_str)
        .unwrap_or("unspecified")
        .to_string();
    let transport = Arc::new(ScriptedLlmHttpTransport::new(parsed)?);
    let result = match spec.request_kind {
        MutationRequestKind::OpenAiCompatible { stream } => {
            let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
                .with_transport(provider_transport(&transport));
            provider.complete(openai_compatible_request(stream)).await
        }
        MutationRequestKind::OpenAiResponses => {
            let mut provider =
                OpenAiProvider::new("test-key").with_transport(provider_transport(&transport));
            provider.complete(openai_responses_request()).await
        }
        MutationRequestKind::Anthropic => {
            let mut provider = AnthropicProvider::new("test-key")
                .with_base_url(Some("https://anthropic.test".to_string()))
                .with_transport(provider_transport(&transport));
            provider.complete(anthropic_messages_request()).await
        }
        MutationRequestKind::Google { stream } => {
            let mut provider = GoogleOAuthProvider::new(
                "access-token",
                "refresh-token",
                0,
                lash_provider_google::GoogleOAuthClient {
                    id: "oauth-client-id".into(),
                    secret: "oauth-client-secret".into(),
                },
            )
            .with_project_id(Some("project-1".to_string()))
            .with_transport(provider_transport(&transport));
            provider.complete(google_request(stream)).await
        }
    };
    let err = result.map_or_else(Ok, |_| {
        Err(ProviderMutationExecutionError::new(format!(
            "mutated provider script `{script_name}` unexpectedly succeeded"
        )))
    })?;
    if let Some(expected_status) = spec.expected_status
        && err.status != Some(expected_status)
    {
        return Err(ProviderMutationExecutionError::new(format!(
            "mutated provider script `{script_name}` returned status {:?}, expected {expected_status}",
            err.status
        )));
    }
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    if let Some(expected_kind) = spec.expected_kind {
        let observed_kind = format!("{:?}", classified.kind);
        if observed_kind != expected_kind {
            return Err(ProviderMutationExecutionError::new(format!(
                "mutated provider script `{script_name}` classified as `{observed_kind}`, expected `{expected_kind}`"
            )));
        }
    }
    if let Some(expected_retryable) = spec.expected_retryable
        && classified.is_retryable() != expected_retryable
    {
        return Err(ProviderMutationExecutionError::new(format!(
            "mutated provider script `{script_name}` classified retryable={}, expected {expected_retryable}",
            classified.is_retryable()
        )));
    }
    let exchanges = transport.exchanges()?;
    Ok(json!({
        "proof_name": spec.proof_name,
        "provider_kind": spec.provider_kind,
        "script_name": script_name,
        "mutation_behavior": mutation_behavior,
        "exchange_count": exchanges.len(),
        "endpoint": exchanges.first().map(|exchange| exchange.request.path.clone()),
        "response_events": exchanges.first().map(|exchange| exchange.response.event_names.clone()).unwrap_or_default(),
        "status": err.status,
        "kind": format!("{:?}", err.kind),
        "retryable": err.is_retryable(),
        "terminal_reason": err.terminal_reason.code(),
        "classification": {
            "kind": format!("{:?}", classified.kind),
            "retryable": classified.is_retryable(),
            "status": classified.status,
            "terminal_reason": classified.terminal_reason.code(),
        },
    }))
}

fn provider_transport(transport: &Arc<ScriptedLlmHttpTransport>) -> Arc<dyn LlmHttpTransport> {
    transport.clone()
}

fn openai_compatible_request(stream: bool) -> LlmRequest {
    LlmRequest {
        model: "openai/gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        }]),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: stream.then(|| LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn openai_responses_request() -> LlmRequest {
    LlmRequest {
        model: "gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn anthropic_messages_request() -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn google_request(stream: bool) -> LlmRequest {
    LlmRequest {
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: stream.then(|| LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{BoundaryEvent, BoundaryKind};

    #[tokio::test]
    async fn provider_mutation_matrix_runs_real_provider_parsers() {
        let mut cache = ProviderMutationMatrixCache::default();
        let event = BoundaryEvent::new(
            "session-001:provider-mutation:001",
            "session-001",
            BoundaryKind::ProviderMutation,
            1,
            "provider.script-mutation.rejected",
            json!({
                "mutation": "malformed_sse_chunk",
            }),
        );
        let observed = cache
            .augment_observation(
                &event,
                json!({
                    "session": "session-001",
                    "provider_mutation": true,
                    "mutation": "malformed_sse_chunk",
                    "rejected": true,
                    "first_rejection": true,
                }),
            )
            .await
            .expect("provider mutation matrix");

        let matrix = &observed["provider_parser_matrix"]["matrix"];
        assert_eq!(matrix["real_provider_parser_execution"], true);
        assert_eq!(matrix["proof_count"], 4);
        let providers = matrix["provider_kinds"]
            .as_array()
            .expect("providers")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            providers,
            vec!["anthropic", "google_oauth", "openai", "openai-compatible"]
        );
        assert!(
            matrix["proofs"]
                .as_array()
                .expect("proofs")
                .iter()
                .all(|proof| proof["exchange_count"].as_u64() == Some(1))
        );
    }

    #[tokio::test]
    async fn dropped_terminal_parser_failures_are_explicitly_non_retryable() {
        let matrix = execute_provider_mutation_matrix("dropped_terminal_event")
            .await
            .expect("dropped-terminal provider mutation matrix");

        for proof in matrix["matrix"]["proofs"]
            .as_array()
            .expect("provider parser proofs")
        {
            assert_eq!(
                proof["classification"]["retryable"],
                false,
                "{} parser must declare a truncated terminal event non-retryable: {proof}",
                proof["provider_kind"].as_str().unwrap_or("unknown")
            );
        }
    }

    #[tokio::test]
    async fn transport_mutations_classify_on_distinct_named_paths() {
        let expectations = [
            ("mid_stream_disconnect", "Stream", true, None),
            ("response_start_timeout", "Timeout", true, None),
            ("stream_chunk_timeout", "Timeout", true, None),
            // Non-2xx responses come back in the shared `http_error_envelope`,
            // which pre-labels the raw failure `Http` before classification.
            ("retryable_server_error_sequence", "Http", true, Some(503)),
        ];
        for (mutation, raw_kind, classified_retryable, status) in expectations {
            let matrix = execute_provider_mutation_matrix(mutation)
                .await
                .unwrap_or_else(|err| panic!("mutation {mutation} failed: {err}"));
            let proof = &matrix["matrix"]["proofs"][0];
            assert_eq!(proof["kind"], raw_kind, "raw kind for {mutation}");
            assert_eq!(
                proof["classification"]["retryable"], classified_retryable,
                "classified retryable for {mutation}"
            );
            if let Some(status) = status {
                assert_eq!(proof["status"], status, "status for {mutation}");
            } else {
                assert_eq!(proof["status"], Value::Null, "status for {mutation}");
            }
        }
    }

    #[test]
    fn parser_mutation_labels_generate_distinct_wire_behaviors() {
        let mut behaviors = std::collections::BTreeSet::new();
        for mutation in [
            "malformed_sse_chunk",
            "dropped_terminal_event",
            "duplicate_tool_call_delta",
            "wrong_provider_schema",
        ] {
            let specs = mutation_specs(mutation).expect("mutation specs");
            let first =
                ProviderWireScript::from_json_str(&specs[0].script_content).expect("script parses");
            let behavior = first
                .expected_provider
                .as_ref()
                .and_then(|expected| expected.get("behavior"))
                .and_then(Value::as_str)
                .expect("behavior");
            assert!(
                behaviors.insert(behavior.to_string()),
                "mutation behavior `{behavior}` was reused"
            );
        }
    }
}
