use std::sync::Arc;

use async_trait::async_trait;
use lash_core::plugin::{PluginError, PluginFactory, PluginSessionContext};
use lash_core::{
    AttemptContext, ToolCall, ToolDefinition, ToolOutcome, ToolProvider,
    facade_support::DirectJsonSchema, facade_support::DirectMessage,
    facade_support::DirectOutputSpec, facade_support::DirectPart, facade_support::DirectRequest,
    facade_support::DirectRole, facade_support::PluginSpec, facade_support::PluginSpecFactory,
};
use lash_tool_support::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt,
};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default)]
pub struct LlmToolsPluginFactory {
    model: Option<String>,
    model_variant: Option<lash_core::ReasoningSelection>,
    model_capability: Option<lash_core::ModelCapability>,
}

impl LlmToolsPluginFactory {
    pub fn with_model(
        mut self,
        model: impl Into<String>,
        model_variant: Option<lash_core::ReasoningSelection>,
    ) -> Self {
        self.model = Some(model.into());
        self.model_variant = model_variant;
        self
    }

    pub fn with_model_variant(mut self, model_variant: impl Into<String>) -> Self {
        self.model_variant = Some(lash_core::ReasoningSelection::Effort(model_variant.into()));
        self
    }

    /// Capability metadata for an overridden model. Without it an override
    /// model has no effort controls, so an explicit variant will be rejected.
    pub fn with_model_capability(mut self, capability: lash_core::ModelCapability) -> Self {
        self.model_capability = Some(capability);
        self
    }
}

impl PluginFactory for LlmToolsPluginFactory {
    fn id(&self) -> &'static str {
        "llm_tools"
    }

    fn build(
        &self,
        ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, PluginError> {
        let provider: Arc<dyn ToolProvider> = Arc::new(llm_query_provider(
            self.model.clone(),
            self.model_variant.clone(),
            self.model_capability.clone(),
        ));

        PluginSpecFactory::new(
            "llm_tools",
            Arc::new(move |_ctx| Ok(PluginSpec::new().with_tool_provider(Arc::clone(&provider)))),
        )
        .build(ctx)
    }
}

pub struct LlmToolsProvider {
    model: Option<String>,
    model_variant: Option<lash_core::ReasoningSelection>,
    model_capability: Option<lash_core::ModelCapability>,
}

/// Build the `llm_query` tool provider for the given optional model override.
pub fn llm_query_provider(
    model: Option<String>,
    model_variant: Option<lash_core::ReasoningSelection>,
    model_capability: Option<lash_core::ModelCapability>,
) -> StaticToolProvider<LlmToolsProvider> {
    StaticToolProvider::new(
        vec![llm_query_tool_definition()],
        LlmToolsProvider {
            model,
            model_variant,
            model_capability,
        },
    )
}

impl LlmToolsProvider {
    async fn llm_query(&self, args: &Value, context: &AttemptContext<'_>) -> Result<Value, String> {
        let task = required_string(args, "task")?;
        let inputs = args.get("inputs").cloned().unwrap_or(Value::Null);
        let output_schema = lash_lashlang_runtime::parse_output_schema(args.get("output"))
            .map_err(|err| err.to_string())?;
        let session_model = context
            .sessions()
            .model()
            .await
            .map_err(|err| format!("failed to read current session model: {err}"))?;
        // An override model carries the override capability (empty when the
        // host supplied none); the session model carries the session's.
        let (model, model_capability) = match self.model.clone() {
            Some(model) => (model, self.model_capability.clone().unwrap_or_default()),
            None => (session_model.model, session_model.model_capability),
        };
        let model_variant = self
            .model_variant
            .clone()
            .unwrap_or(session_model.model_variant);
        // The sub-question runs on the session's behalf, so it carries the
        // session's sampling intent rather than provider defaults.
        let generation = session_model.generation;
        let response_schema = llm_query_response_schema(output_schema.as_ref());
        let prompt = llm_query_prompt(&task, &inputs, output_schema.as_ref());

        let output = DirectOutputSpec::JsonSchema(DirectJsonSchema {
            name: "llm_query_result".to_string(),
            schema: response_schema.clone().into(),
            strict: true,
        });

        let completion = context
            .direct_completions()
            .complete(
                DirectRequest {
                    model,
                    model_variant,
                    model_capability,
                    messages: vec![
                        DirectMessage {
                            role: DirectRole::System,
                            parts: vec![DirectPart::Text(
                                "Answer the focused sub-question using only the supplied task and inputs. Return only JSON matching the requested result wrapper. Use kind=\"error\" with a concise error only when the task cannot be answered from the supplied inputs."
                                    .to_string(),
                            )],
                        },
                        DirectMessage {
                            role: DirectRole::User,
                            parts: vec![DirectPart::Text(prompt)],
                        },
                    ],
                    attachments: Vec::new(),
                    output,
                    stream_events: None,
                    generation,
                    session_id: Some(format!("{}-llm-query", context.session_id())),
                    caused_by: None,
                    replay: None,
                },
                "llm_query",
            )
            .await
            .map_err(|err| format!("llm_query failed: {err}"))?;

        parse_llm_query_result(&completion.text, &response_schema)
    }
}

#[async_trait]
impl StaticToolExecute for LlmToolsProvider {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let result = match call.name {
            "llm_query" => self.llm_query(call.args, call.context).await,
            _ => Err(format!("Unknown tool: {}", call.name)),
        };
        finalise_tool_result(result)
    }
}

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
    .with_lashlang_binding(LashlangToolBinding::new(["llm"], "query"))
    .with_output_from_input_schema("output", Some(json!({ "type": "string" })))
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

fn parse_llm_query_result(text: &str, schema: &Value) -> Result<Value, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("llm_query returned empty output".to_string());
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
    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|err| format!("llm_query output schema is invalid: {err}"))?;
    if let Err(errors) = compiled.validate(&value) {
        let message = errors
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("llm_query output did not match schema: {message}"));
    }
    match value.get("kind").and_then(Value::as_str) {
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
    }
}

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

fn finalise_tool_result(result: Result<Value, String>) -> ToolOutcome {
    match result {
        Ok(value) => ToolOutcome::ok(value),
        Err(err) => ToolOutcome::err(json!(err)),
    }
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
    use lash_sansio::sync::MutexExt;

    fn model_spec(model: &str, variant: Option<&str>) -> lash_core::ModelSpec {
        lash_core::ModelSpec::builder(model)
            .variant(
                variant
                    .map(|effort| lash_core::ReasoningSelection::Effort(effort.to_string()))
                    .unwrap_or_default(),
            )
            .context_window_tokens(200_000)
            .build()
            .expect("valid test model spec")
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
            _session_id: &str,
        ) -> Result<SessionSnapshot, PluginError> {
            Ok(self.snapshot.to_snapshot())
        }
        async fn tool_catalog(
            &self,
            _session_id: &str,
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

        async fn close_session(&self, _session_id: &str) -> Result<(), PluginError> {
            Ok(())
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
        lash_core::testing::mock_attempt_context_from(
            &lash_core::testing::mock_tool_context_with_host_and_direct_completions(
                manager,
                completions,
            ),
        )
    }

    #[test]
    fn llm_definitions_include_llm_query_only() {
        let provider = llm_query_provider(None, None, None);
        let manifests = provider.tool_manifests();
        let names = manifests
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["llm_query"]);
        // The lashlang binding surface is feature-unified across the
        // workspace (`lash-tool-support/lashlang` may be enabled by any
        // sibling crate in the build graph), so this crate's own `lashlang`
        // feature cannot tell whether bindings are real or the no-op stub.
        // Derive the expectation from what the binding ext actually produces
        // in this build instead of guessing from cfg.
        let probe = lash_core::ToolDefinition::raw(
            "probe",
            "probe",
            "probe",
            json!({ "type": "object" }),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["probe"], "probe"));
        assert_eq!(
            manifests[0].bindings.keys().collect::<Vec<_>>(),
            probe.manifest().bindings.keys().collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn llm_query_uses_current_policy_and_direct_completion() {
        let manager = Arc::new(DirectCompletionManager {
            snapshot: RuntimeSessionState {
                policy: lash_core::SessionPolicy {
                    model: model_spec("root-model", Some("fast")),
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
            },
            requests: Mutex::new(Vec::new()),
            response_text:
                r#"{"kind":"value","value":{"root_cause":"missing config","confidence":0.8},"error":null}"#
                    .to_string(),
        });
        let provider = llm_query_provider(None, None, None);
        let context = direct_completion_attempt_context(manager.clone());

        let args = json!({
            "task": "extract root cause",
            "inputs": { "log": "failed" },
            "output": { "root_cause": "str", "confidence": "float" }
        });
        let result = provider
            .execute(ToolCall {
                name: "llm_query",
                args: &args,
                context: &context,
            })
            .await;

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
        assert_eq!(request.model, "root-model");
        assert_eq!(request.model_variant.effort(), Some("fast"));
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
    async fn llm_query_uses_configured_model_override() {
        let manager = Arc::new(DirectCompletionManager {
            snapshot: RuntimeSessionState {
                policy: lash_core::SessionPolicy {
                    model: model_spec("root-model", Some("medium")),
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            requests: Mutex::new(Vec::new()),
            response_text: r#"{"kind":"value","value":"done","error":null}"#.to_string(),
        });
        let provider = llm_query_provider(
            Some("gpt-5.5".to_string()),
            Some(lash_core::ReasoningSelection::Effort("low".to_string())),
            None,
        );
        let context = direct_completion_attempt_context(manager.clone());

        let args = json!({ "task": "answer directly" });
        let result = provider
            .execute(ToolCall {
                name: "llm_query",
                args: &args,
                context: &context,
            })
            .await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        let requests = manager.requests.lock_recover();
        assert_eq!(requests.len(), 1);
        let (request, usage_source) = &requests[0];
        assert_eq!(usage_source, "llm_query");
        assert_eq!(request.model, "gpt-5.5");
        assert_eq!(request.model_variant.effort(), Some("low"));
    }

    #[tokio::test]
    async fn llm_query_error_result_fails_tool_call() {
        let manager = Arc::new(DirectCompletionManager {
            snapshot: RuntimeSessionState {
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            requests: Mutex::new(Vec::new()),
            response_text: r#"{"kind":"error","value":null,"error":"missing required evidence"}"#
                .to_string(),
        });
        let provider = llm_query_provider(None, None, None);
        let context = direct_completion_attempt_context(manager);

        let args = json!({ "task": "answer from missing evidence" });
        let result = provider
            .execute(ToolCall {
                name: "llm_query",
                args: &args,
                context: &context,
            })
            .await;

        assert!(!result.is_success());
        assert_eq!(
            result.value_for_projection(),
            json!("missing required evidence")
        );
    }
}
