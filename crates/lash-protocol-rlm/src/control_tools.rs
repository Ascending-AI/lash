use async_trait::async_trait;
use lash_core::{
    ToolArgumentProjectionPolicy, ToolCall, ToolContract, ToolControl, ToolDefinition,
    ToolManifest, ToolOutcome, ToolProvider,
};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::projection::RlmSeed;

pub(crate) struct RlmControlToolsProvider {
    /// The dialect this session's model writes, so the `continue_as` doc shows
    /// a call it can actually make.
    pub(crate) vocabulary: crate::dialect::DialectPromptVocabulary,
}

#[async_trait]
impl ToolProvider for RlmControlToolsProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![continue_as_tool_definition_for(self.vocabulary).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "continue_as")
            .then(|| Arc::new(continue_as_tool_definition_for(self.vocabulary).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let result = match call.name {
            "continue_as" => continue_as_switch_frame(call.args, call.context),
            _ => return ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name)),
        };
        finalise_tool_result(result)
    }
}

/// Public API shape, unchanged: the default dialect's doc and example.
pub fn continue_as_tool_definition() -> ToolDefinition {
    continue_as_tool_definition_for(crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY)
}

pub(crate) fn continue_as_tool_definition_for(
    vocabulary: crate::dialect::DialectPromptVocabulary,
) -> ToolDefinition {
    ToolDefinition::raw(
        "tool:continue_as",
        "continue_as",
        format!("Tail-call into a fresh RLM AgentFrame inside the current session with a clean window.\n\nThe new frame inherits **nothing** implicitly — no variables or message history. Pass everything it needs via `seed: {{ name: value, ... }}`. Seed values copied from read-only values stay read-only in the new frame; computed expressions become writable variables.\n\n- Use when the current trajectory is stale, dominated by failed attempts, or the context budget is tight.\n- Treat `control.continue_as(...)` as a terminal control action: make it the last meaningful statement in the {cell_noun}, and do not call `finish` or perform more work after it.\n- `task` packs the concrete goal, constraints, and next steps the new frame must act on.\n- `seed` packs the concrete state (paths, facts already learned, partial results, read-only values) the new frame needs in scope; leave bulky raw output behind.\n- `continue_as` only changes the active AgentFrame. It does not start, transfer, list, cancel, or otherwise manage processes.", cell_noun = vocabulary.cell_noun),
        continue_as_input_schema(),
        continue_as_output_schema(),
    )
    .with_examples(vec![vocabulary.continue_as_example.into()])
    .with_tool_binding(ToolBinding::new(["control"], "continue_as"))
    .with_argument_projection(ToolArgumentProjectionPolicy::preserve_projected_refs_in_field(
        "seed",
    ))
}

fn continue_as_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ok": { "type": "boolean" },
            "frame_key": { "type": "string" },
            "task": { "type": "string" },
            "seed_keys": {
                "type": "array",
                "items": { "type": "string" }
            },
            "seed_count": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "ok",
            "frame_key",
            "task",
            "seed_keys",
            "seed_count"
        ],
        "additionalProperties": false
    })
}

pub fn continue_as_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task": {
                "type": "string",
                "description": "Task for the new AgentFrame."
            },
            "seed": {
                "type": "object",
                "additionalProperties": true,
                "description": "Optional record/dict of concrete state for the new AgentFrame."
            }
        },
        "required": ["task"],
        "additionalProperties": false
    })
}

fn continue_as_switch_frame(
    args: &Value,
    context: &lash_core::AttemptContext<'_>,
) -> Result<ContinueAsResult, String> {
    let task = required_string(args, "task")?;
    let seed = RlmSeed::from_tool_args(args).map_err(|err| format!("continue_as {err}"))?;
    let mut seed_keys = seed
        .globals
        .keys()
        .cloned()
        .chain(seed.projected.entries.iter().map(|(name, _)| name.clone()))
        .collect::<Vec<_>>();
    seed_keys.sort();
    let seed_count = seed_keys.len();
    let tool_call_id = context
        .tool_call_id()
        .ok_or_else(|| "continue_as requires a stable tool call id".to_string())?;
    let frame_key = lash_core::FrameKey::from_call_site(
        context.session_id(),
        context.agent_frame_id(),
        tool_call_id,
    );
    let initial_nodes = crate::rlm_seed_initial_nodes(seed);

    Ok(ContinueAsResult {
        value: json!({
            "ok": true,
            "frame_key": frame_key.as_str(),
            "task": task.clone(),
            "seed_keys": seed_keys,
            "seed_count": seed_count,
        }),
        control: ToolControl::SwitchAgentFrame {
            frame_key,
            initial_nodes,
            task: Some(task),
        },
    })
}

fn required_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

struct ContinueAsResult {
    value: Value,
    control: ToolControl,
}

fn finalise_tool_result(result: Result<ContinueAsResult, String>) -> ToolOutcome {
    match result {
        Ok(result) => ToolOutcome::ok(result.value).with_control(result.control),
        Err(err) => ToolOutcome::err(json!(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{decode_rlm_protocol_event, rlm_protocol_event};
    use lash_sansio::sync::MutexExt;
    use std::sync::{Arc, Mutex};

    use lash_core::plugin::runtime_host::{
        SessionGraphService, SessionLifecycleService, SessionStateService,
    };
    use lash_core::plugin::{PluginError, SessionHandle};
    use lash_core::runtime::RuntimeSessionState;
    use lash_core::{
        SessionAppendNode, SessionCreateRequest, SessionPolicy, SessionSnapshot, ToolProvider,
    };
    use lash_rlm_types::{RlmProtocolEvent, RlmTermination};

    fn model_spec(model: &str) -> lash_core::ModelSpec {
        lash_core::ModelSpec::builder(model)
            .context_window_tokens(200_000)
            .build()
            .expect("valid test model spec")
    }

    #[test]
    fn continue_as_contract_documents_switch_result() {
        let definition =
            continue_as_tool_definition_for(crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY);

        assert_eq!(
            definition.contract.output_schema.canonical["required"],
            json!(["ok", "frame_key", "task", "seed_keys", "seed_count"])
        );
        let rendered = definition.compact_contract().render_signature();
        assert!(rendered.contains("frame_key"), "{rendered}");
        assert!(!rendered.contains("handle_count"), "{rendered}");
        assert!(!rendered.contains("projected_count"), "{rendered}");
        assert!(!rendered.contains("global_count"), "{rendered}");
    }

    struct BatonManager {
        snapshot: RuntimeSessionState,
        created: Mutex<Vec<SessionCreateRequest>>,
        closed: Mutex<Vec<String>>,
    }

    impl Default for BatonManager {
        fn default() -> Self {
            Self {
                snapshot: RuntimeSessionState::new(SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                )),
                created: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
            }
        }
    }

    #[test]
    fn continue_as_tool_definition_preserves_projected_seed_refs_by_metadata() {
        assert_eq!(
            continue_as_tool_definition_for(crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY)
                .manifest
                .argument_projection,
            ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed")
        );
    }

    #[async_trait]
    impl SessionStateService for BatonManager {
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
    impl SessionLifecycleService for BatonManager {
        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            self.created.lock_recover().push(request.clone());
            Ok(SessionHandle {
                session_id: request.session_id.unwrap_or_else(|| "child".to_string()),
                parent_session_id: request.relation.parent_session_id().map(ToOwned::to_owned),
                policy: request
                    .policy
                    .expect("test session creation requires an explicit policy"),
                observed_processes: Vec::new(),
            })
        }

        async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
            self.closed.lock_recover().push(session_id.to_string());
            Ok(())
        }
    }

    #[async_trait]
    impl SessionGraphService for BatonManager {}

    #[async_trait]
    impl lash_core::ProcessService for BatonManager {
        async fn start_from_recorded_intent(
            &self,
            _session_id: &str,
            _request: lash_core::ProcessStartRequest,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessHandleView, PluginError> {
            Err(PluginError::Session(
                "recorded process starts are unavailable in this test".to_string(),
            ))
        }

        async fn finish_recorded_intent_parent(
            &self,
            _session_id: &str,
            _identity: lash_core::ToolIntentIdentity,
            _process_id: String,
            _policy: lash_core::ProcessParentEndPolicy,
            _reason: String,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ToolIntentParentEndOutcome, PluginError> {
            Err(PluginError::Session(
                "recorded parent end is unavailable in this test".to_string(),
            ))
        }

        async fn start(
            &self,
            _session_id: &str,
            _registration: lash_core::ProcessRegistration,
            _options: lash_core::ProcessStartOptions,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            Err(PluginError::Session(
                "process starts are unavailable in this test".to_string(),
            ))
        }

        async fn await_process(
            &self,
            _process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessAwaitOutput, PluginError> {
            Err(PluginError::Session(
                "process awaiting is unavailable in this test".to_string(),
            ))
        }

        async fn list_visible(
            &self,
            _session_id: &str,
            _mode: lash_core::ProcessListMode,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<Vec<lash_core::ProcessRecord>, PluginError> {
            Ok(Vec::new())
        }

        async fn validate_visible(
            &self,
            _session_id: &str,
            _handle_ids: &[String],
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            Err(PluginError::Session(
                "continue_as must not validate process handles".to_string(),
            ))
        }

        async fn cancel(
            &self,
            _session_id: &str,
            _process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            Err(PluginError::Session(
                "process cancellation is unavailable in this test".to_string(),
            ))
        }

        async fn cancel_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _reason: Option<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            Err(PluginError::Session(
                "recorded process cancellation is unavailable in this test".to_string(),
            ))
        }

        async fn signal_possessed(
            &self,
            _session_id: &str,
            _process_id: &str,
            _signal_name: String,
            _signal_id: String,
            _payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            Err(PluginError::Session(
                "process signalling is unavailable in this test".to_string(),
            ))
        }

        async fn signal_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _signal_name: String,
            _signal_id: String,
            _payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            Err(PluginError::Session(
                "recorded process signalling is unavailable in this test".to_string(),
            ))
        }

        async fn emit_event_recorded_intent(
            &self,
            _session_id: &str,
            _process_id: &str,
            _event_type: String,
            _replay_key: String,
            _payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            Err(PluginError::Session(
                "recorded process event emission is unavailable in this test".to_string(),
            ))
        }

        async fn transfer(
            &self,
            _from_session_id: &str,
            _to_session_id: &str,
            _process_ids: Vec<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            Err(PluginError::Session(
                "continue_as must not transfer process handles".to_string(),
            ))
        }
    }

    async fn run_continue_as_at_call(
        provider: &RlmControlToolsProvider,
        manager: Arc<BatonManager>,
        args: &Value,
        tool_call_id: &str,
    ) -> ToolOutcome {
        let sessions: Arc<dyn SessionStateService> = manager.clone();
        let session_lifecycle: Arc<dyn SessionLifecycleService> = manager.clone();
        let session_graph: Arc<dyn SessionGraphService> = manager.clone();
        let processes: Arc<dyn lash_core::ProcessService> = manager;
        let context = lash_core::ToolContext::__for_testing(
            "test-session".to_string(),
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            Arc::new(lash_core::facade_support::SessionAttachmentStore::in_memory()),
            lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
                Err(lash_core::PluginError::Session(
                    "direct completions are unavailable in continue_as tests".to_string(),
                ))
            }),
            Some(tool_call_id.to_string()),
        );
        let context = lash_core::ToolContext::with_agent_frame_id_for_testing(
            context,
            lash_core::facade_support::frame_node_id("test-session", "test-lineage"),
        );
        let context = lash_core::testing::mock_attempt_context_from(&context);
        provider
            .execute(lash_core::ToolCall {
                name: "continue_as",
                args,
                context: &context,
            })
            .await
    }

    async fn run_continue_as(
        provider: &RlmControlToolsProvider,
        manager: Arc<BatonManager>,
        args: &Value,
    ) -> ToolOutcome {
        run_continue_as_at_call(provider, manager, args, "continue-as-test").await
    }

    #[test]
    fn rlm_control_definitions_include_continue_as_only() {
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };
        let names = provider
            .tool_manifests()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["continue_as"]);
    }

    #[tokio::test]
    async fn continue_as_creates_empty_rlm_frame_with_seed_and_task() {
        let mut session_graph = lash_core::SessionGraph::default();
        session_graph.append_protocol_event(rlm_protocol_event(RlmProtocolEvent::RlmGlobalsPatch(
            lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([("diary".to_string(), json!([]))]),
            },
        )));
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                protocol_turn_options: lash_core::ProtocolTurnOptions::typed(
                    RlmTermination::FinishRequired {
                        schema: Some(json!({
                            "type": "object",
                            "properties": { "answer": { "type": "string" } },
                            "required": ["answer"]
                        })),
                    },
                )
                .expect("valid rlm turn options"),
                session_graph,
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };

        let args = json!({
            "task": "finish from here",
            "seed": { "x": 1, "query": "original" }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        let value = result.value_for_projection();
        assert!(value.get("frame_key").and_then(Value::as_str).is_some());
        assert_eq!(value.get("seed_keys"), Some(&json!(["query", "x"])));
        assert_eq!(value.get("seed_count"), Some(&json!(2)));
        assert!(value.get("projected_count").is_none());
        assert!(value.get("global_count").is_none());
        let Some(ToolControl::SwitchAgentFrame {
            frame_key,
            initial_nodes,
            task,
        }) = result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        assert_eq!(
            value.get("frame_key").and_then(Value::as_str),
            Some(frame_key.as_str())
        );
        assert_eq!(task.as_deref(), Some("finish from here"));
        assert_eq!(initial_nodes.len(), 1);
        let SessionAppendNode::ProtocolEvent {
            event: protocol_event,
            ..
        } = &initial_nodes[0]
        else {
            panic!("expected seed globals event");
        };
        let Some(RlmProtocolEvent::RlmSeed(seed)) = decode_rlm_protocol_event(protocol_event)
        else {
            panic!("expected RlmSeed");
        };
        assert_eq!(seed.globals["x"], json!(1));
        assert_eq!(seed.globals["query"], json!("original"));
        assert!(seed.projected.is_empty());
        assert!(manager.created.lock_recover().is_empty());
    }

    fn frame_key(result: &ToolOutcome) -> &lash_core::FrameKey {
        let Some(ToolControl::SwitchAgentFrame { frame_key, .. }) =
            result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        frame_key
    }

    #[tokio::test]
    async fn continue_as_redrive_derives_the_same_frame_identity() {
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };
        let args = json!({ "task": "continue deterministically" });
        let manager = Arc::new(BatonManager::default());

        let first =
            run_continue_as_at_call(&provider, Arc::clone(&manager), &args, "redriven-call").await;
        let redriven = run_continue_as_at_call(&provider, manager, &args, "redriven-call").await;

        assert_eq!(
            frame_key(&first).as_str(),
            "frame-key/v1/b0c4cb16ff47e16a1d126ed5413686bc759fb6549e34b09d8cd95bbff9f1d1ae"
        );
        assert_eq!(
            frame_key(&redriven).as_str(),
            "frame-key/v1/b0c4cb16ff47e16a1d126ed5413686bc759fb6549e34b09d8cd95bbff9f1d1ae"
        );
    }

    #[tokio::test]
    async fn identical_continue_as_tasks_at_distinct_calls_derive_distinct_keys() {
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };
        let args = json!({ "task": "same task" });
        let manager = Arc::new(BatonManager::default());

        let first =
            run_continue_as_at_call(&provider, Arc::clone(&manager), &args, "call-one").await;
        let second = run_continue_as_at_call(&provider, manager, &args, "call-two").await;

        assert_eq!(
            frame_key(&first).as_str(),
            "frame-key/v1/53008d3473dd2ee526755b78d7196ea64f53a974171fb81be1dcc61b19696cff"
        );
        assert_eq!(
            frame_key(&second).as_str(),
            "frame-key/v1/63b76fc1b7953f10b0e6acd7a9528dfc02e2df440bd7742f7ed1766346fc49a8"
        );
    }

    #[tokio::test]
    async fn continue_as_routes_projected_entries_and_globals_to_one_seed_event() {
        // Mixed seed: `proj` was a projected source on the parent (encoded with
        // the canonical `__projected__` JSON wrapper), `glob` was a regular
        // global. The new frame receives both through one durable RLM seed event.
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };

        let args = json!({
            "task": "finish from here",
            "seed": {
                "proj": { "__projected__": "carry-over" },
                "glob": 7
            }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;
        assert!(result.is_success(), "{:?}", result.value_for_projection());
        let value = result.value_for_projection();
        assert_eq!(value.get("seed_keys"), Some(&json!(["glob", "proj"])));
        assert_eq!(value.get("seed_count"), Some(&json!(2)));
        assert!(value.get("projected_count").is_none());
        assert!(value.get("global_count").is_none());

        let Some(ToolControl::SwitchAgentFrame { initial_nodes, .. }) =
            result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        assert_eq!(initial_nodes.len(), 1);
        let SessionAppendNode::ProtocolEvent {
            event: protocol_event,
            ..
        } = &initial_nodes[0]
        else {
            panic!("expected seed globals event");
        };
        let Some(RlmProtocolEvent::RlmSeed(seed)) = decode_rlm_protocol_event(protocol_event)
        else {
            panic!("expected RlmSeed");
        };
        assert_eq!(seed.globals.len(), 1, "only `glob` should land as a global");
        assert_eq!(seed.globals["glob"], json!(7));
        assert!(!seed.globals.contains_key("proj"));
        assert_eq!(seed.projected.entries.len(), 1);
        assert_eq!(seed.projected.entries[0].0, "proj");
        assert_eq!(seed.projected.entries[0].1, json!("carry-over"));
        assert!(manager.created.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn continue_as_preserves_process_shaped_seed_without_processes() {
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };

        let args = json!({
            "task": "continue with background work",
            "seed": {
                "one": { "__handle__": "process", "id": "h1" },
                "nested": [{ "h": { "__handle__": "process", "id": "h2" } }]
            }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        let Some(ToolControl::SwitchAgentFrame { initial_nodes, .. }) =
            result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        let SessionAppendNode::ProtocolEvent {
            event: protocol_event,
            ..
        } = &initial_nodes[0]
        else {
            panic!("expected seed globals event");
        };
        let Some(RlmProtocolEvent::RlmSeed(seed)) = decode_rlm_protocol_event(protocol_event)
        else {
            panic!("expected RlmSeed");
        };
        assert_eq!(
            seed.globals["one"],
            json!({ "__handle__": "process", "id": "h1" })
        );
        assert_eq!(
            seed.globals["nested"],
            json!([{ "h": { "__handle__": "process", "id": "h2" } }])
        );
    }

    #[tokio::test]
    async fn continue_as_does_not_validate_unknown_seed_handles() {
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        };

        let args = json!({
            "task": "continue",
            "seed": { "h": { "__handle__": "process", "id": "missing" } }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        assert!(manager.created.lock_recover().is_empty());
    }
}
