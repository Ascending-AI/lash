use async_trait::async_trait;
use lash_core::{
    ToolArgumentProjectionPolicy, ToolCall, ToolContract, ToolDefinition, ToolManifest,
    ToolOutcome, ToolProvider, TurnControl, TurnControls,
};
use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::projection::RlmSeed;

pub(crate) struct RlmControlToolsProvider {
    /// The dialect this session's model writes, so the control tools' docs
    /// show calls it can actually make.
    pub(crate) vocabulary: crate::dialect::DialectPromptVocabulary,
}

#[async_trait]
impl ToolProvider for RlmControlToolsProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![
            finish_tool_definition_for(self.vocabulary).manifest(),
            continue_as_tool_definition_for(self.vocabulary).manifest(),
            read_output_tool_definition().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        match name {
            FINISH_TOOL_NAME => Some(Arc::new(
                finish_tool_definition_for(self.vocabulary).contract(),
            )),
            "continue_as" => Some(Arc::new(
                continue_as_tool_definition_for(self.vocabulary).contract(),
            )),
            "read_output" => Some(Arc::new(read_output_tool_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "read_output" {
            return read_output(call).await;
        }
        match call.name() {
            // The value is the whole input: the turn ends with it.
            FINISH_TOOL_NAME => ToolOutcome::finish(call.args.clone()).into(),
            "continue_as" => match continue_as_switch_frame(call.args, call.context) {
                Ok(control) => ToolOutcome::turn_control(control).into(),
                Err(err) => ToolOutcome::err(json!(err)).into(),
            },
            _ => ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into(),
        }
    }
}

#[expect(clippy::expect_used, reason = "the tool declares fixed valid schemas")]
fn read_output_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:read_output",
        "read_output",
        "Read a step's complete ordered print values. Pass history[N].output_archive.attachment as archive; inline steps already have their full values in history[N].output. Reads are explicit and never expand the prompt automatically.",
        json!({
            "type": "object",
            "properties": {"archive": {"type": "object"}},
            "required": ["archive"],
            "additionalProperties": false,
        }),
        json!({"type": "array", "items": {}}),
    ).expect("valid schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(ToolBinding::new(["control"], "read_output"))
}

async fn read_output(call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
    let source = call.args.get("archive").cloned().unwrap_or(Value::Null);
    let Ok(lash_core::ToolValue::Attachment(attachment_ref)) =
        serde_json::from_value::<lash_core::ToolValue>(source)
    else {
        return ToolOutcome::failure(lash_core::ToolFailure::invalid_request(
            "invalid_output_archive",
            "archive must be a history attachment ref",
        ))
        .into();
    };
    let bytes = match call.context.attachments().read(&attachment_ref).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return lash_core::ToolAttemptOutcome::host_failed(
                lash_core::RuntimeEffectControllerError::output_retention_failed(&error),
            );
        }
    };
    match decode_output_archive(&bytes) {
        Ok(values) => ToolOutcome::ok(Value::Array(values)).into(),
        Err(error) => lash_core::ToolAttemptOutcome::host_failed(error),
    }
}

pub(crate) fn decode_output_archive(
    bytes: &[u8],
) -> Result<Vec<Value>, lash_core::RuntimeEffectControllerError> {
    serde_json::from_slice::<Vec<lash_core::CellPrint>>(bytes)
        .map(|observations| {
            observations
                .into_iter()
                .map(|observation| observation.value)
                .collect()
        })
        .map_err(|error| {
            lash_core::RuntimeEffectControllerError::output_retention_failed(
                &lash_core::AttachmentStoreError::Contract(format!(
                    "the step output archive is corrupt: {error}"
                )),
            )
        })
}

/// The name of Lash's finish control tool: what a turn it ended records as
/// its finishing tool.
pub const FINISH_TOOL_NAME: &str = "finish";

/// The finish control tool's id. Its one input is the turn's answer, whole.
pub(crate) const FINISH_TOOL_ID: &str = "tool:finish";

/// Lash's finish tool as `catalog` offers it, taking its value under the
/// finish schema the turn's `options` state: the binding a cell admits a
/// `control.finish` call under, so the call's own input validation refuses
/// a value the turn's required output does not admit. `None` when the turn
/// states no schema or the catalog offers no finish.
///
/// # Errors
///
/// Options that are not the RLM namespace's.
pub(crate) fn turn_finish_binding(
    catalog: &lash_core::ToolCatalog,
    options: &lash_core::ProtocolTurnOptions,
) -> Result<Option<ToolDefinition>, String> {
    let Some(schema) = crate::rlm_support::decode_rlm_termination_options(options)?.finish_schema
    else {
        return Ok(None);
    };
    let Some(finish) = catalog
        .tools
        .iter()
        .find(|tool| tool.manifest.id.as_str() == FINISH_TOOL_ID)
    else {
        return Ok(None);
    };
    let mut contract = finish.contract.as_ref().clone();
    contract.input_schema = lash_core::SchemaContract::new(schema);
    Ok(Some(ToolDefinition::from_parts(
        finish.manifest.clone(),
        contract,
    )))
}

/// The `finish` control tool as a session in `dialect` advertises it.
pub fn finish_tool_definition(dialect: &dyn crate::dialect::DialectPrompts) -> ToolDefinition {
    finish_tool_definition_for(dialect.prompt_vocabulary())
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool schema and admission checks its invariant"
)]
pub(crate) fn finish_tool_definition_for(
    vocabulary: crate::dialect::DialectPromptVocabulary,
) -> ToolDefinition {
    ToolDefinition::control(
        FINISH_TOOL_ID,
        FINISH_TOOL_NAME,
        format!(
            "End the turn with `value` as its answer. Nothing after the call runs: make it the last thing the {cell_noun} does, once all its other work has finished. It returns nothing. When the turn states a required output, `value` must match it; a value that does not fails and the turn goes on.",
            cell_noun = vocabulary.cell_noun
        ),
        // The whole input is the answer: any JSON value, unless the turn
        // states a finish schema, which a cell's call is admitted under
        // instead (`turn_finish_binding`).
        json!({}),
        TurnControls::finish(),
    )
    .expect("valid declared tool schema")
    // No work of its own: the call is its control.
    .with_execution(std::time::Duration::from_secs(30))
    // Its body reads its argument and nothing else, so running it again
    // is safe: a call a crash cut off runs again on the owner that resumes
    // the cell, and one that timed out behind a lost owner is retried. The
    // turn then ends on it rather than on a second model call. The body
    // never fails on its own: what refuses a value is the call's input
    // validation under the turn's finish schema.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("three is non-zero"),
        100,
        1_000,
    ))
    .with_examples(vec![vocabulary.finish_call.into()])
    .with_tool_binding(ToolBinding::new(["control"], FINISH_TOOL_NAME))
}

/// The `continue_as` control tool as a session in `dialect` advertises it.
pub fn continue_as_tool_definition(dialect: &dyn crate::dialect::DialectPrompts) -> ToolDefinition {
    continue_as_tool_definition_for(dialect.prompt_vocabulary())
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn continue_as_tool_definition_for(
    vocabulary: crate::dialect::DialectPromptVocabulary,
) -> ToolDefinition {
    ToolDefinition::control(
        "tool:continue_as",
        "continue_as",
        format!("Switch to a fresh AgentFrame when context is stale or crowded. `task` states the goal and next steps; `seed` carries all needed state: nothing is inherited. Read-only seeds stay read-only. It ends the turn and returns nothing: nothing after the call runs, so make it the last thing the {cell_noun} does, once all its other work has finished.", cell_noun = vocabulary.cell_noun),
        continue_as_input_schema(),
        TurnControls::switch_agent_frame(),
    ).expect("valid declared tool schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))
    .with_examples(vec![vocabulary.continue_as_example.into()])
    .with_tool_binding(ToolBinding::new(["control"], "continue_as"))
    .with_argument_projection(ToolArgumentProjectionPolicy::preserve_projected_refs_in_field(
        "seed",
    ))
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
) -> Result<TurnControl, String> {
    let task = required_string(args, "task")?;
    let seed = RlmSeed::from_tool_args(args).map_err(|err| format!("continue_as {err}"))?;
    let frame_key = lash_core::FrameKey::from_call_site(
        context
            .session_id()
            .map_err(|err| format!("continue_as {err}"))?,
        context
            .agent_frame_id()
            .map_err(|err| format!("continue_as {err}"))?,
        context.call_id(),
    );
    let initial_nodes = crate::rlm_seed_initial_nodes(seed, context.fleet_format());

    Ok(TurnControl::SwitchAgentFrame {
        frame_key,
        initial_nodes,
        task,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{decode_rlm_protocol_event, rlm_protocol_event};
    use lash_sansio::sync::MutexExt;
    use lash_sansio::{ProcessId, SessionId};
    use std::sync::{Arc, Mutex};

    use lash_core::plugin::runtime_host::{
        SessionGraphService, SessionLifecycleService, SessionStateService,
    };
    use lash_core::plugin::{PluginError, SessionHandle};
    use lash_core::runtime::RuntimeSessionState;
    use lash_core::{
        SessionAppendNode, SessionCreateRequest, SessionPolicy, SessionSnapshot, ToolProvider,
    };
    use lash_core::{ToolControl, TurnControl};
    use lash_rlm_types::RlmProtocolEvent;

    fn llm_profile_spec(model: &str) -> Option<lash_core::LlmProfileConfig> {
        Some(lash_core::testing::test_llm_profile_config(
            model,
            lash_core::testing::test_llm_profile_metadata(model),
        ))
    }

    struct BatonManager {
        snapshot: RuntimeSessionState,
        created: Mutex<Vec<SessionCreateRequest>>,
    }

    impl Default for BatonManager {
        fn default() -> Self {
            Self {
                snapshot: RuntimeSessionState::ambient_fixture(SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                )),
                created: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionStateService for BatonManager {
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
    impl SessionLifecycleService for BatonManager {
        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            self.created.lock_recover().push(request.clone());
            Ok(SessionHandle {
                session_id: request
                    .session_id
                    .unwrap_or_else(|| SessionId::from("child")),
                parent_session_id: request.relation.parent_session_id().map(SessionId::from),
                policy: request
                    .policy
                    .expect("test session creation requires an explicit policy"),
                observed_processes: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl SessionGraphService for BatonManager {}

    #[async_trait]
    impl lash_core::ProcessService for BatonManager {
        async fn stage_recorded_start(
            &self,
            _owner: &lash_core::RuntimeOwner,
            _request: lash_core::ProcessStartRequest,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::StagedProcessStart, PluginError> {
            Err(PluginError::Session(
                "recorded process starts are unavailable in this test".to_string(),
            ))
        }

        async fn start(
            &self,
            _session_id: &SessionId,
            _registration: lash_core::ProcessStartRegistration,
            _options: lash_core::ProcessStartOptions,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::facade_support::ObservedProcess, PluginError> {
            Err(PluginError::Session(
                "process starts are unavailable in this test".to_string(),
            ))
        }

        async fn await_process(
            &self,
            _process_id: &ProcessId,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessAwaitOutput, PluginError> {
            Err(PluginError::Session(
                "process awaiting is unavailable in this test".to_string(),
            ))
        }

        async fn list_visible(
            &self,
            _session_id: &SessionId,
            _mode: lash_core::ProcessListMode,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<Vec<lash_core::facade_support::ObservedProcess>, PluginError> {
            Ok(Vec::new())
        }

        async fn validate_visible(
            &self,
            _owner: &lash_core::RuntimeOwner,
            _handle_ids: &[ProcessId],
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            Err(PluginError::Session(
                "continue_as must not validate process handles".to_string(),
            ))
        }

        async fn cancel(
            &self,
            _owner: &lash_core::RuntimeOwner,
            _process_id: &ProcessId,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::facade_support::ObservedProcess, PluginError> {
            Err(PluginError::Session(
                "process cancellation is unavailable in this test".to_string(),
            ))
        }

        async fn cancel_recorded_intent(
            &self,
            _owner: &lash_core::RuntimeOwner,
            _process_id: &ProcessId,
            _identity: lash_core::ToolIntentIdentity,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::facade_support::ObservedProcess, PluginError> {
            Err(PluginError::Session(
                "recorded process cancellation is unavailable in this test".to_string(),
            ))
        }

        async fn transfer(
            &self,
            _from_session_id: &SessionId,
            _to_session_id: &SessionId,
            _process_ids: Vec<ProcessId>,
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
        let processes: Arc<dyn lash_core::ProcessService> = manager.clone();
        let context = lash_core::testing::ToolCallFixture::with_host_and_direct_completions(
            manager,
            lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
                Err(lash_core::PluginError::Session(
                    "direct completions are unavailable in continue_as tests".to_string(),
                ))
            }),
        )
        .processes(processes)
        .call_id(lash_core::ToolCallId::fixture(tool_call_id))
        .owner_as(lash_core::ExecutionOwner::SessionFrame {
            session_id: SessionId::from("test-session"),
            agent_frame_id: lash_core::facade_support::frame_node_id(
                &SessionId::from("test-session"),
                "test-lineage",
            ),
        })
        .attempt("test-turn");
        let manifest = provider
            .resolve_manifest("continue_as")
            .expect("continue_as manifest");
        match provider
            .execute(lash_core::ToolCall::new(&manifest, args, &context))
            .await
        {
            lash_core::ToolAttemptOutcome::Done { result, intents } => {
                assert!(intents.is_empty(), "continue_as emits no intents");
                ToolOutcome::from_output(result.into_output())
            }
            lash_core::ToolAttemptOutcome::HostFailed(error) => {
                panic!("unexpected host fault: {error}")
            }
            lash_core::ToolAttemptOutcome::Pending(pending) => {
                ToolOutcome::Pending(Box::new(pending))
            }
        }
    }

    async fn run_continue_as(
        provider: &RlmControlToolsProvider,
        manager: Arc<BatonManager>,
        args: &Value,
    ) -> ToolOutcome {
        run_continue_as_at_call(provider, manager, args, "continue-as-test").await
    }

    #[tokio::test]
    async fn continue_as_creates_empty_rlm_frame_with_seed_and_task() {
        let mut session_graph = lash_core::SessionGraph::default();
        session_graph.append_protocol_event(rlm_protocol_event(
            RlmProtocolEvent::RlmGlobalsPatch(lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([("diary".to_string(), json!([]))]),
            }),
            lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                crate::RLM_PROTOCOL_EVENT_VERSION
            )),
        ));
        let manager = Arc::new(BatonManager {
            snapshot: {
                let mut snapshot = RuntimeSessionState {
                    policy: SessionPolicy {
                        model: llm_profile_spec("model"),
                        ..SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                            lash_core::MaxToolCalls::new(1024),
                            lash_core::NoProgressBudget::bounded(12),
                        )
                    },
                    session_graph,
                    ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    ))
                };
                snapshot.authority.plugin_config = lash_core::PluginConfig::for_protocol(Some(
                    crate::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                ));
                snapshot.authority.plugin_config.insert(
                    crate::RLM_PROTOCOL_PLUGIN_ID,
                    lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmTurnOptions {
                        termination: Some(lash_core::TerminationMode::TerminalRequired),
                        finish_schema: Some(
                            lash_sansio::JsonSchema::admit(json!({
                                "type": "object",
                                "properties": { "answer": { "type": "string" } },
                                "required": ["answer"]
                            }))
                            .expect("valid finish schema"),
                        ),
                        ..Default::default()
                    })
                    .expect("valid rlm turn options")
                    .payload,
                );
                snapshot
            },
            created: Mutex::new(Vec::new()),
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::DialectPrompts::prompt_vocabulary(
                &crate::dialect::TypescriptPrompts,
            ),
        };

        let args = json!({
            "task": "finish from here",
            "seed": { "x": 1, "query": "original" }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;

        assert!(result.is_success(), "{:?}", result.value_for_projection());
        // The switch is the call's whole result: it answers nothing.
        assert_eq!(result.value_for_projection(), Value::Null);
        let Some(ToolControl::Turn {
            control:
                TurnControl::SwitchAgentFrame {
                    initial_nodes,
                    task,
                    ..
                },
        }) = result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        assert_eq!(task, "finish from here");
        assert_eq!(initial_nodes.len(), 1);
        let SessionAppendNode::ProtocolEvent {
            event: protocol_event,
            ..
        } = &initial_nodes[0]
        else {
            panic!("expected seed globals event");
        };
        let Some(RlmProtocolEvent::RlmSeed(seed)) =
            decode_rlm_protocol_event(protocol_event).expect("valid history fixture")
        else {
            panic!("expected RlmSeed");
        };
        assert_eq!(seed.globals["x"], json!(1));
        assert_eq!(seed.globals["query"], json!("original"));
        assert!(seed.projected.is_empty());
        assert!(manager.created.lock_recover().is_empty());
    }

    fn frame_key(result: &ToolOutcome) -> &lash_core::FrameKey {
        let Some(ToolControl::Turn {
            control: TurnControl::SwitchAgentFrame { frame_key, .. },
        }) = result.as_output().control.as_ref()
        else {
            panic!("expected frame switch control");
        };
        frame_key
    }

    #[tokio::test]
    async fn continue_as_redrive_derives_the_same_frame_identity() {
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::DialectPrompts::prompt_vocabulary(
                &crate::dialect::TypescriptPrompts,
            ),
        };
        let args = json!({ "task": "continue deterministically" });
        let manager = Arc::new(BatonManager::default());

        let first =
            run_continue_as_at_call(&provider, Arc::clone(&manager), &args, "redriven-call").await;
        let redriven = run_continue_as_at_call(&provider, manager, &args, "redriven-call").await;

        assert_eq!(
            frame_key(&first).as_str(),
            "frame-key/v2/c6886b0a0352c9234847f52e26bcdac8ff92eb18671563487c9bd63eeceeb829"
        );
        assert_eq!(
            frame_key(&redriven).as_str(),
            "frame-key/v2/c6886b0a0352c9234847f52e26bcdac8ff92eb18671563487c9bd63eeceeb829"
        );
    }

    #[tokio::test]
    async fn identical_continue_as_tasks_at_distinct_calls_derive_distinct_keys() {
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::DialectPrompts::prompt_vocabulary(
                &crate::dialect::TypescriptPrompts,
            ),
        };
        let args = json!({ "task": "same task" });
        let manager = Arc::new(BatonManager::default());

        let first =
            run_continue_as_at_call(&provider, Arc::clone(&manager), &args, "call-one").await;
        let second = run_continue_as_at_call(&provider, manager, &args, "call-two").await;

        assert_eq!(
            frame_key(&first).as_str(),
            "frame-key/v2/eb9ea68cffc503f90f348a3b4e4834d4f00b2a9b2883349eea474bd6884bd182"
        );
        assert_eq!(
            frame_key(&second).as_str(),
            "frame-key/v2/cab0da7cef16c366ded2e1e4377e93e79782e46f926124e834037ac152bab400"
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
                    model: llm_profile_spec("model"),
                    ..SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    )
                },
                ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ))
            },
            created: Mutex::new(Vec::new()),
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::DialectPrompts::prompt_vocabulary(
                &crate::dialect::TypescriptPrompts,
            ),
        };

        let args = json!({
            "task": "finish from here",
            "seed": {
                "proj": {
                    "__projected__": {
                        "kind": "materialized",
                        "value": "carry-over"
                    }
                },
                "glob": 7
            }
        });
        let result = run_continue_as(&provider, manager.clone(), &args).await;
        assert!(result.is_success(), "{:?}", result.value_for_projection());

        let Some(ToolControl::Turn {
            control: TurnControl::SwitchAgentFrame { initial_nodes, .. },
        }) = result.as_output().control.as_ref()
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
        let Some(RlmProtocolEvent::RlmSeed(seed)) =
            decode_rlm_protocol_event(protocol_event).expect("valid history fixture")
        else {
            panic!("expected RlmSeed");
        };
        assert_eq!(seed.globals.len(), 1, "only `glob` should land as a global");
        assert_eq!(seed.globals["glob"], json!(7));
        assert!(!seed.globals.contains_key("proj"));
        assert_eq!(seed.projected.entries.len(), 1);
        assert_eq!(seed.projected.entries[0].0, "proj");
        assert_eq!(
            seed.projected.entries[0].1,
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(json!("carry-over"))
        );
        assert!(manager.created.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn continue_as_preserves_process_shaped_seed_without_processes() {
        let manager = Arc::new(BatonManager {
            snapshot: RuntimeSessionState {
                policy: SessionPolicy {
                    model: llm_profile_spec("model"),
                    ..SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    )
                },
                ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ))
            },
            created: Mutex::new(Vec::new()),
        });
        let provider = RlmControlToolsProvider {
            vocabulary: crate::dialect::DialectPrompts::prompt_vocabulary(
                &crate::dialect::TypescriptPrompts,
            ),
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
        let Some(ToolControl::Turn {
            control: TurnControl::SwitchAgentFrame { initial_nodes, .. },
        }) = result.as_output().control.as_ref()
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
        let Some(RlmProtocolEvent::RlmSeed(seed)) =
            decode_rlm_protocol_event(protocol_event).expect("valid history fixture")
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
}
