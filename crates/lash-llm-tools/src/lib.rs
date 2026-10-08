use std::sync::Arc;

use async_trait::async_trait;
use lash_core::plugin::{PluginError, PluginFactory, PluginSessionContext};
use lash_core::{
    AttemptContext, SessionId, ToolCall, ToolDefinition, ToolOutcome, ToolProvider,
    facade_support::DirectJsonSchema, facade_support::DirectMessage,
    facade_support::DirectOutputSpec, facade_support::DirectPart, facade_support::DirectRequest,
    facade_support::DirectRole,
};
use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolDefinitionBindingExt,
};
use serde_json::{Value, json};

/// The plugin id `llm_query` registers under.
pub const LLM_TOOLS_PLUGIN_ID: &str = "llm_tools";

/// The usage source, and the name of the direct purpose, of every
/// `llm_query` sub-question.
pub const LLM_QUERY_PURPOSE: &str = "llm_query";

/// The instructions a sub-question carries: the `llm_tools/llm_query`
/// section, which renders only for the [`LLM_QUERY_PURPOSE`] direct purpose,
/// in the initial instructions unless the host places it.
pub const LLM_QUERY_INSTRUCTIONS: &str = "Answer the focused sub-question using only the supplied task and inputs. Return only JSON matching the requested result wrapper. Use kind=\"error\" with a concise error only when the task cannot be answered from the supplied inputs.";

/// Installs `llm_query`. The tool has no model selection of its own: every
/// query runs the model its session recorded, with that binding's
/// capability, request extensions and request defaults, so a redrive sends
/// what the first attempt sent whatever the deployment now installs
/// (FIG-4531). Its instructions are a prompt section of its own direct
/// purpose, so a sub-question composes them and nothing of the session's
/// turn prompt (ADR 0133 §8).
#[derive(Clone, Debug, Default)]
pub struct LlmToolsPluginFactory {}

impl PluginFactory for LlmToolsPluginFactory {
    fn id(&self) -> &'static str {
        LLM_TOOLS_PLUGIN_ID
    }

    fn build(
        &self,
        _ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, PluginError> {
        Ok(Arc::new(LlmToolsPlugin {
            provider: Arc::new(llm_query_provider()),
        }))
    }
}

struct LlmToolsPlugin {
    provider: Arc<dyn ToolProvider>,
}

impl lash_core::facade_support::SessionPlugin for LlmToolsPlugin {
    fn id(&self) -> &'static str {
        LLM_TOOLS_PLUGIN_ID
    }

    fn register(&self, reg: &mut lash_core::plugin::PluginRegistrar) -> Result<(), PluginError> {
        reg.tools().provider(Arc::clone(&self.provider))?;
        let key = lash_core::prompt_sections::PromptSectionKey::new(LLM_QUERY_PURPOSE)
            .map_err(|error| PluginError::Registration(error.to_string()))?;
        reg.prompt().section(
            lash_core::plugin::prompt::PromptSectionSpec::new(
                key,
                lash_core::prompt_sections::PromptPlacement::InitialInstructions,
            )
            .purposes([lash_core::prompt_sections::PromptPurpose::Direct {
                name: LLM_QUERY_PURPOSE.to_owned(),
            }]),
            Arc::new(|_: &lash_core::plugin::prompt::PromptInput<'_>| {
                Ok(lash_core::plugin::prompt::SectionText::text(
                    LLM_QUERY_INSTRUCTIONS,
                ))
            }),
        )
    }
}

impl lash_core::plugin::PluginDefinition for LlmToolsPluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("llm_tools")
    }
}

#[derive(Debug)]
enum LlmQueryError {
    Message(String),
    Failure(Box<lash_core::ToolFailure>),
}
impl From<String> for LlmQueryError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}
impl From<lash_core::ToolFailure> for LlmQueryError {
    fn from(failure: lash_core::ToolFailure) -> Self {
        Self::Failure(Box::new(failure))
    }
}
pub struct LlmToolsProvider;

pub fn llm_query_provider() -> StaticToolProvider<LlmToolsProvider> {
    StaticToolProvider::new(vec![llm_query_tool_definition()], LlmToolsProvider)
}

impl LlmToolsProvider {
    async fn llm_query(
        &self,
        args: &Value,
        context: &AttemptContext<'_>,
    ) -> Result<Value, LlmQueryError> {
        let task = required_string(args, "task")?;
        let inputs = args.get("inputs").cloned().unwrap_or(Value::Null);
        let output_schema = lash_sansio::schema_contract::parse_output_schema(args.get("output"))
            .map_err(|err| err.to_string())?;
        let session_model = context
            .sessions()
            .model()
            .await
            .map_err(|err| format!("failed to read current session model: {err}"))?;
        // The sub-question runs on the session's behalf, so it carries the
        // session's sampling intent rather than provider defaults.
        let generation = session_model.generation;
        let response_schema =
            lash_sansio::JsonSchema::admit(llm_query_response_schema(output_schema.as_ref()))
                .map_err(|source| {
                    lash_core::ToolFailure::invalid_request(
                        "unusable_output_schema",
                        source.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::SchemaAdmission { source })
                })?;
        let prompt = llm_query_prompt(&task, &inputs, output_schema.as_ref());

        let output = DirectOutputSpec::JsonSchema(DirectJsonSchema {
            name: "llm_query_result".to_string(),
            schema: lash_sansio::SchemaContract::new(response_schema.clone()),
            strict: true,
        });

        let completion = context
            .direct_completions()
            .complete(
                DirectRequest {
                    attachment_acceptance: session_model.attachment_acceptance,
                    messages: vec![DirectMessage {
                        role: DirectRole::User,
                        parts: vec![DirectPart::Text(prompt)],
                    }],
                    output,
                    stream_events: None,
                    generation,
                    session_id: Some(match context.owner().runtime_owner() {
                        lash_core::RuntimeOwner::Session(session_id) => {
                            session_id.with_suffix("-llm-query")
                        }
                        lash_core::RuntimeOwner::Process(process_id) => {
                            SessionId::prefixed("process:", format_args!("{process_id}-llm-query"))
                        }
                    }),
                    caused_by: None,
                    replay: None,
                },
                LLM_QUERY_PURPOSE,
            )
            .await
            .map_err(|error| -> LlmQueryError {
                if let PluginError::ValueMismatch { source, .. } = error {
                    lash_core::ToolFailure::tool(
                        lash_core::ToolFailureClass::External,
                        "invalid_llm_response",
                        source.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::ValueMismatch { source: *source })
                    .into()
                } else if let PluginError::UnusableSchema { source } = error {
                    lash_core::ToolFailure::tool(
                        lash_core::ToolFailureClass::Internal,
                        "unusable_output_schema",
                        source.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::SchemaAdmission { source: *source })
                    .into()
                } else if let PluginError::UnusableToolSchema { source } = error {
                    lash_core::ToolFailure::tool(
                        lash_core::ToolFailureClass::Internal,
                        "unusable_tool_schema",
                        source.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::ToolSchemaAdmission { source })
                    .into()
                } else {
                    LlmQueryError::Message(format!("llm_query failed: {error}"))
                }
            })?;

        parse_llm_query_result(&completion.text, &response_schema)
    }
}

#[async_trait]
impl StaticToolExecute for LlmToolsProvider {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let result = match call.name() {
            "llm_query" => self.llm_query(call.args, call.context).await,
            _ => Err(LlmQueryError::Message(format!(
                "Unknown tool: {}",
                call.name()
            ))),
        };
        match result {
            Ok(value) => ToolOutcome::ok(value).into(),
            Err(LlmQueryError::Message(message)) => ToolOutcome::err(json!(message)).into(),
            Err(LlmQueryError::Failure(failure)) => ToolOutcome::failure(*failure).into(),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "the output default is this tool's fixed string schema"
)]
pub fn llm_query_tool_definition() -> ToolDefinition {
    tool_definition(
        "llm_query",
        "Run a one-shot LLM prompt over supplied data and return its result. The `task` plus everything in `inputs` is rendered into that single prompt; the call cannot use tools, inspect files, or gather more context beyond what you pass it. Use this for extracting information, classification, summarization, judging, or transformation over data already in your variables. `inputs` can be any structured value. `output` is optional and defaults to a string; when present, it requests structured output using record descriptors or `Type { ... }` literals.",
        llm_query_input_schema(),
        vec![
            r#"summary = await llm.query({ task: "Summarize the supplied notes in three bullets", inputs: { notes: notes } })?"#.into(),
            r#"claims = await llm.query({ task: "Extract the key claim from each supplied chunk", inputs: { chunks: chunks }, output: { claims: "list[str]" } })?"#.into(),
        ],
    )
    .with_tool_binding(ToolBinding::new(["llm"], "query"))
    .with_output_from_input_schema(
        "output",
        Some(
            lash_sansio::JsonSchema::admit(json!({ "type": "string" }))
                .expect("valid output default schema"),
        ),
    )
}

fn llm_query_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task": { "type": "string" },
            "inputs": {},
            "output": { "type": "object", "additionalProperties": true }
        },
        "required": ["task"],
        "additionalProperties": false
    })
}

fn llm_query_prompt(task: &str, inputs: &Value, output_schema: Option<&Value>) -> String {
    let mut sections = Vec::new();
    sections.push(format!("Task:\n{task}"));
    sections.push(format!(
        "Inputs:\n```json\n{}\n```",
        serde_json::to_string_pretty(inputs).unwrap_or_else(|_| inputs.to_string())
    ));
    if let Some(schema) = output_schema {
        sections.push(format!(
            "Return `kind=\"value\"` with `value` matching this JSON Schema, or `kind=\"error\"` with a concise error if the task cannot be answered from the supplied inputs:\n```json\n{}\n```",
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
        ));
    } else {
        sections.push("Return `kind=\"value\"` with a concise string `value`, or `kind=\"error\"` with a concise error if the task cannot be answered from the supplied inputs.".to_string());
    }
    sections.join("\n\n")
}

fn llm_query_response_schema(output_schema: Option<&Value>) -> Value {
    let value_schema = output_schema
        .cloned()
        .unwrap_or_else(|| json!({"type": "string"}));
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "value", "error"],
        "properties": {
            "kind": { "type": "string", "enum": ["value", "error"] },
            "value": {
                "anyOf": [
                    value_schema,
                    { "type": "null" }
                ]
            },
            "error": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "null" }
                ]
            }
        }
    })
}

fn parse_llm_query_result(
    text: &str,
    schema: &lash_sansio::JsonSchema,
) -> Result<Value, LlmQueryError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("llm_query returned empty output".to_string().into());
    }
    let value = serde_json::from_str::<Value>(trimmed).or_else(|err| {
        let Some(start) = trimmed.find(['{', '[', '"']) else {
            return Err(format!("llm_query returned non-JSON output: {err}"));
        };
        let end = trimmed
            .rfind(['}', ']', '"'])
            .ok_or_else(|| format!("llm_query returned malformed JSON output: {err}"))?;
        if end < start {
            return Err(format!("llm_query returned malformed JSON output: {err}"));
        }
        serde_json::from_str::<Value>(&trimmed[start..=end])
            .map_err(|parse_err| format!("llm_query returned malformed JSON output: {parse_err}"))
    })?;
    schema.validate(&value).map_err(|source| {
        lash_core::ToolFailure::tool(
            lash_core::ToolFailureClass::External,
            "invalid_llm_response",
            format!("llm_query output did not match schema: {source}"),
        )
        .with_cause(lash_core::ToolFailureCause::ValueMismatch { source })
    })?;
    let result: Result<Value, String> = match value.get("kind").and_then(Value::as_str) {
        Some("value") => value
            .get("value")
            .cloned()
            .filter(|value| !value.is_null())
            .ok_or_else(|| "llm_query returned value result without value".to_string()),
        Some("error") => Err(value
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .unwrap_or("llm_query returned an error")
            .to_string()),
        Some(other) => Err(format!("llm_query returned unknown result kind `{other}`")),
        None => Err("llm_query returned result without kind field".to_string()),
    };
    result.map_err(LlmQueryError::Message)
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn tool_definition(
    name: &str,
    description: impl Into<String>,
    input_schema: Value,
    examples: Vec<String>,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    // One model call: as long as lash lets a model call run.
    .with_execution(std::time::Duration::from_secs(10 * 60))
    .with_examples(examples)
}

fn required_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lash_core::plugin::runtime_host::{
        SessionGraphService, SessionLifecycleService, SessionStateService,
    };
    use lash_core::plugin::{PluginError, SessionHandle};
    use lash_core::runtime::RuntimeSessionState;
    use lash_core::{SessionCreateRequest, SessionSnapshot, ToolCall};

    async fn run_llm_query(
        provider: &StaticToolProvider<LlmToolsProvider>,
        args: &serde_json::Value,
        context: &lash_core::AttemptContext<'_>,
    ) -> lash_core::ToolOutcome {
        let manifest = provider
            .resolve_manifest("llm_query")
            .expect("llm_query manifest resolves");
        match provider
            .execute(ToolCall::new(&manifest, args, context))
            .await
        {
            lash_core::ToolAttemptOutcome::Done { result, intents } => {
                assert!(intents.is_empty(), "llm_query declares no intents");
                lash_core::ToolOutcome::from_output(result.into_output())
            }
            lash_core::ToolAttemptOutcome::HostFailed(error) => {
                panic!("unexpected host fault: {error}")
            }
            lash_core::ToolAttemptOutcome::Pending(pending) => {
                lash_core::ToolOutcome::Pending(Box::new(pending))
            }
        }
    }
    use lash_sansio::sync::MutexExt;

    fn llm_profile_spec(model: &str, variant: Option<&str>) -> Option<lash_core::LlmProfileConfig> {
        let config = lash_core::testing::test_llm_profile_config(
            model,
            lash_core::testing::test_llm_profile_metadata(model),
        );
        Some(match variant {
            Some(effort) => {
                config.with_reasoning(lash_core::ReasoningSelection::Effort(effort.to_string()))
            }
            None => config,
        })
    }

    struct DirectCompletionManager {
        snapshot: RuntimeSessionState,
        requests: Mutex<Vec<(lash_core::facade_support::DirectRequest, String)>>,
        response_text: String,
    }

    impl Default for DirectCompletionManager {
        fn default() -> Self {
            Self {
                snapshot: RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                )),
                requests: Mutex::new(Vec::new()),
                response_text: String::new(),
            }
        }
    }

    #[async_trait]
    impl SessionStateService for DirectCompletionManager {
        async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
            Ok(self.snapshot.to_snapshot())
        }

        async fn snapshot_session(
            &self,
            _session_id: &SessionId,
        ) -> Result<SessionSnapshot, PluginError> {
            Ok(self.snapshot.to_snapshot())
        }
        async fn tool_catalog(
            &self,
            _session_id: &SessionId,
        ) -> Result<Vec<serde_json::Value>, PluginError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl SessionLifecycleService for DirectCompletionManager {
        async fn create_session(
            &self,
            _request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            Err(PluginError::Session("not used".to_string()))
        }
    }

    #[async_trait]
    impl SessionGraphService for DirectCompletionManager {}

    fn direct_completion_attempt_context(
        manager: Arc<DirectCompletionManager>,
    ) -> lash_core::AttemptContext<'static> {
        let completions = lash_core::facade_support::DirectCompletionClient::from_fn({
            let manager = Arc::clone(&manager);
            move |request, usage_source| {
                manager
                    .requests
                    .lock_recover()
                    .push((request, usage_source));
                Ok(lash_core::facade_support::DirectCompletion {
                    text: manager.response_text.clone(),
                    usage: lash_core::TokenUsage::default(),
                    llm_call: lash_core::LlmCallRecord {
                        call_id: lash_core::LlmCallId("llm-tools-test".to_string()),
                        label: None,
                        replay_drops: Vec::new(),
                        attempts: Vec::new(),
                    },
                })
            }
        });
        lash_core::testing::ToolCallFixture::with_host_and_direct_completions(manager, completions)
            .attempt("test-turn")
    }

    #[test]
    fn query_result_validation_preserves_formats_and_all_errors() {
        let output = serde_json::json!({
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "count": { "type": "integer" }
            }
        });
        let schema = lash_sansio::JsonSchema::admit(
            serde_json::json!({ "type": "object", "properties": { "value": output } }),
        )
        .expect("valid result schema");
        let valid = serde_json::json!({ "email": "sam@example.com", "count": 1 });
        assert_eq!(
            parse_llm_query_result(
                &serde_json::json!({ "kind": "value", "value": valid }).to_string(),
                &schema
            )
            .unwrap(),
            valid
        );
        let error = parse_llm_query_result(
            r#"{"kind":"value","value":{"email":"invalid","count":"invalid"}}"#,
            &schema,
        )
        .unwrap_err();
        let LlmQueryError::Failure(failure) = error else {
            panic!("typed value mismatch expected")
        };
        assert!(matches!(
            failure.cause.as_deref(),
            Some(lash_core::ToolFailureCause::ValueMismatch { .. })
        ));
        let error = failure.message;
        assert!(error.contains("schema"), "{error}");
        assert!(error.contains("email"), "{error}");
        assert!(error.contains("integer"), "{error}");
        assert!(error.contains("; "), "{error}");
    }

    #[tokio::test]
    async fn llm_query_uses_current_policy_and_direct_completion() {
        let manager = Arc::new(DirectCompletionManager {
            snapshot: RuntimeSessionState {
                policy: lash_core::SessionPolicy {
                    model: llm_profile_spec("root-model", Some("fast")),
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded, lash_core::MaxToolCalls::new(1024))
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded, lash_core::MaxToolCalls::new(1024)))
            },
            requests: Mutex::new(Vec::new()),
            response_text:
                r#"{"kind":"value","value":{"root_cause":"missing config","confidence":0.8},"error":null}"#
                    .to_string(),
        });
        let provider = llm_query_provider();
        let context = direct_completion_attempt_context(manager.clone());

        let args = json!({
            "task": "extract root cause",
            "inputs": { "log": "failed" },
            "output": { "root_cause": "str", "confidence": "float" }
        });
        let result = run_llm_query(&provider, &args, &context).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        assert_eq!(
            result.value_for_projection()["root_cause"],
            json!("missing config")
        );
        assert_eq!(result.value_for_projection()["confidence"], json!(0.8));

        let requests = manager.requests.lock_recover();
        assert_eq!(requests.len(), 1);
        let (request, usage_source) = &requests[0];
        assert_eq!(usage_source, "llm_query");
        assert_eq!(request.generation, manager.snapshot.policy.generation);
        assert!(matches!(
            request.output,
            lash_core::facade_support::DirectOutputSpec::JsonSchema(_)
        ));
        let prompt = request
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                lash_core::facade_support::DirectPart::Text(text) => Some(text.as_str()),
                lash_core::facade_support::DirectPart::Attachment(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("extract root cause"));
        assert!(prompt.contains("\"log\": \"failed\""));
    }

    #[tokio::test]
    async fn llm_query_error_result_fails_tool_call() {
        let manager = Arc::new(DirectCompletionManager {
            snapshot: RuntimeSessionState {
                policy: lash_core::SessionPolicy {
                    model: llm_profile_spec("root-model", None),
                    ..lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                    )
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                ))
            },
            requests: Mutex::new(Vec::new()),
            response_text: r#"{"kind":"error","value":null,"error":"missing required evidence"}"#
                .to_string(),
        });
        let provider = llm_query_provider();
        let context = direct_completion_attempt_context(manager);

        let args = json!({ "task": "answer from missing evidence" });
        let result = run_llm_query(&provider, &args, &context).await;

        assert!(!result.is_success());
        assert_eq!(
            result.value_for_projection(),
            json!("missing required evidence")
        );
    }
}
