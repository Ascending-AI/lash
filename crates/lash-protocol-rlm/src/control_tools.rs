use async_trait::async_trait;
use lash_core::{
    ToolArgumentProjectionPolicy, ToolCall, ToolContract, ToolControl, ToolDefinition,
    ToolManifest, ToolProvider, ToolResult,
};
use lash_lashlang_runtime::{LashlangToolBinding, ToolDefinitionLashlangExt};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::projection::RlmSeed;

pub(crate) struct RlmControlToolsProvider;

#[async_trait]
impl ToolProvider for RlmControlToolsProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![continue_as_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "continue_as").then(|| Arc::new(continue_as_tool_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        let result = match call.name {
            "continue_as" => continue_as_switch_frame(call.args),
            _ => return ToolResult::err_fmt(format_args!("Unknown tool: {}", call.name)),
        };
        finalise_tool_result(result)
    }
}

pub fn continue_as_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:continue_as",
        "continue_as",
        "Tail-call into a fresh RLM AgentFrame inside the current session with a clean window.\n\nThe new frame inherits **nothing** implicitly — no variables or message history. Pass everything it needs via `seed: { name: value, ... }`. Seed values copied from read-only values stay read-only in the new frame; computed expressions become writable variables.\n\n- Use when the current trajectory is stale, dominated by failed attempts, or the context budget is tight.\n- Treat `control.continue_as(...)` as a terminal control action: make it the last meaningful statement in the lashlang block, and do not call `finish` or perform more work after it.\n- `task` packs the concrete goal, constraints, and next steps the new frame must act on.\n- `seed` packs the concrete state (paths, facts already learned, partial results, read-only values) the new frame needs in scope; leave bulky raw output behind.\n- `continue_as` only changes the active AgentFrame. It does not start, transfer, list, cancel, or otherwise manage processes.",
        continue_as_input_schema(),
        continue_as_output_schema(),
    )
    .with_examples(vec![
        r#"await control.continue_as({ task: "continue the audit from the summarized findings", seed: { problem: input.prompt, findings: findings } })?"#.into(),
    ])
    .with_lashlang_binding(LashlangToolBinding::new(["control"], "continue_as"))
    .with_argument_projection(ToolArgumentProjectionPolicy::preserve_projected_refs_in_field(
        "seed",
    ))
}

fn continue_as_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ok": { "type": "boolean" },
            "frame_id": { "type": "string" },
            "task": { "type": "string" },
            "seed_keys": {
                "type": "array",
                "items": { "type": "string" }
            },
            "seed_count": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "ok",
            "frame_id",
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

fn continue_as_switch_frame(args: &Value) -> Result<ContinueAsResult, String> {
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
    let frame_id = uuid::Uuid::new_v4().to_string();
    let initial_nodes = crate::rlm_seed_initial_nodes(seed);

    Ok(ContinueAsResult {
        value: json!({
            "ok": true,
            "frame_id": frame_id.clone(),
            "task": task.clone(),
            "seed_keys": seed_keys,
            "seed_count": seed_count,
        }),
        control: ToolControl::SwitchAgentFrame {
            frame_id,
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

fn finalise_tool_result(result: Result<ContinueAsResult, String>) -> ToolResult {
    match result {
        Ok(result) => ToolResult::ok(result.value).with_control(result.control),
        Err(err) => ToolResult::err(json!(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{decode_rlm_protocol_event, rlm_protocol_event};
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
        lash_core::ModelSpec::from_token_limits(model, Default::default(), 200_000, None)
            .expect("valid test model spec")
    }

    #[test]
    fn continue_as_contract_documents_switch_result() {
        let definition = continue_as_tool_definition();

        assert_eq!(
            definition.contract.output_schema.canonical["required"],
            json!(["ok", "frame_id", "task", "seed_keys", "seed_count"])
        );
        let rendered = definition.compact_contract().render_signature();
        assert!(rendered.contains("frame_id"), "{rendered}");
        assert!(!rendered.contains("handle_count"), "{rendered}");
        assert!(!rendered.contains("projected_count"), "{rendered}");
        assert!(!rendered.contains("global_count"), "{rendered}");
    }

    #[derive(Default)]
    struct BatonManager {
        snapshot: RuntimeSessionState,
        created: Mutex<Vec<SessionCreateRequest>>,
        closed: Mutex<Vec<String>>,
    }

    #[test]
    fn continue_as_tool_definition_preserves_projected_seed_refs_by_metadata() {
        assert_eq!(
            continue_as_tool_definition().manifest.argument_projection,
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
            self.created.lock().expect("created").push(request.clone());
            Ok(SessionHandle {
                session_id: request.session_id.unwrap_or_else(|| "child".to_string()),
                parent_session_id: request.relation.parent_session_id().map(ToOwned::to_owned),
                policy: request.policy.unwrap_or_default(),
                observed_processes: Vec::new(),
            })
        }

        async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
            self.closed
                .lock()
                .expect("closed")
                .push(session_id.to_string());
            Ok(())
        }
    }

    #[async_trait]
    impl SessionGraphService for BatonManager {}

    #[async_trait]
    impl lash_core::ProcessService for BatonManager {
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

        async fn signal(
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

    async fn run_continue_as(
        provider: &RlmControlToolsProvider,
        manager: Arc<BatonManager>,
        args: &Value,
    ) -> ToolResult {
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
            Arc::new(lash_core::SessionAttachmentStore::in_memory()),
            lash_core::DirectCompletionClient::from_fn(|_, _| {
                Err(lash_core::PluginError::Session(
                    "direct completions are unavailable in continue_as tests".to_string(),
                ))
            }),
            Some("continue-as-test".to_string()),
        );
        provider
            .execute(lash_core::ToolCall {
                name: "continue_as",
                args,
                context: &context,
                progress: None,
            })
            .await
    }

    #[test]
    fn rlm_control_definitions_include_continue_as_only() {
        let provider = RlmControlToolsProvider;
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
                    ..SessionPolicy::default()
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
                ..RuntimeSessionState::default()
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider;

        let args = json!({
            "task": "finish from here",
            "seed": { "x": 1, "query": "original" }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        let value = result.value_for_projection();
        assert!(value.get("frame_id").and_then(Value::as_str).is_some());
        assert_eq!(value.get("seed_keys"), Some(&json!(["query", "x"])));
        assert_eq!(value.get("seed_count"), Some(&json!(2)));
        assert!(value.get("projected_count").is_none());
        assert!(value.get("global_count").is_none());
        let Some(ToolControl::SwitchAgentFrame {
            frame_id,
            initial_nodes,
            task,
        }) = result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        assert_eq!(
            value.get("frame_id").and_then(Value::as_str),
            Some(frame_id.as_str())
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
        assert!(manager.created.lock().expect("created").is_empty());
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
                    ..SessionPolicy::default()
                },
                ..RuntimeSessionState::default()
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider;

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
        assert!(manager.created.lock().expect("created").is_empty());
    }

    #[tokio::test]
    async fn continue_as_preserves_process_shaped_seed_without_processes() {
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::default()
                },
                ..RuntimeSessionState::default()
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider;

        let args = json!({
            "task": "continue with background work",
            "seed": {
                "one": { "__handle__": "process", "id": "h1", "tool": "slow" },
                "nested": [{ "h": { "__handle__": "process", "id": "h2", "tool": "slow" } }]
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
            json!({ "__handle__": "process", "id": "h1", "tool": "slow" })
        );
        assert_eq!(
            seed.globals["nested"],
            json!([{ "h": { "__handle__": "process", "id": "h2", "tool": "slow" } }])
        );
    }

    #[tokio::test]
    async fn continue_as_does_not_validate_unknown_seed_handles() {
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: model_spec("model"),
                    ..SessionPolicy::default()
                },
                ..RuntimeSessionState::default()
            },
            created: Mutex::new(Vec::new()),
            ..BatonManager::default()
        });
        let provider = RlmControlToolsProvider;

        let args = json!({
            "task": "continue",
            "seed": { "h": { "__handle__": "process", "id": "missing", "tool": "slow" } }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        assert!(manager.created.lock().expect("created").is_empty());
    }
}
