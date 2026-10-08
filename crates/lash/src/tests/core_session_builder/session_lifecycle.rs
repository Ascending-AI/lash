//! Session lifecycle laws over the durable substrate (SQLite memory stores,
//! the core's node serving its sessions), re-written by FIG-5307 for the
//! laws the deleted engine-double session lifecycle tests owed.

use super::*;
use lash_core::SessionId;

mod session_binding;

fn session(text: &str) -> crate::SessionId {
    crate::SessionId::parse(text).expect("nonblank host identity")
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_cell_answer_preserves_the_exact_string_without_presentation_policy() {
    let answer = "  <raw>\n# literal\n\ttrailing spaces  ";
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("rlm-string-answer")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock_recover().push(format!(
                        "{}\n{}",
                        system_text(&request),
                        request_text(&request)
                    ));
                    Ok(text_response(&typescript_block(&format!(
                        "finish({});",
                        serde_json::to_string(answer).expect("string encodes")
                    ))))
                }
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("rlm core");
    let created = core
        .session(session("rlm-exact-string"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let result = created
        .send(crate::TurnInput::text("return the literal string"))
        .output()
        .await
        .expect("cell answers");
    assert_eq!(result.final_value(), Some(&serde_json::json!(answer)));
    {
        let prompts = seen.lock_recover();
        assert_eq!(prompts.len(), 1);
        assert!(
            !prompts[0].contains("FINAL ANSWER FORMAT"),
            "{}",
            prompts[0]
        );
        assert!(
            !prompts[0].contains("nicely formatted Markdown"),
            "{}",
            prompts[0]
        );
    }
    core.shutdown().await.expect("shutdown");
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_rlm_create_extras_fail_child_session_creation() {
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("rlm core");
    core.session(session("rlm-root"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("parent created");
    let mut plugin_options = lash_core::PluginOptions {
        plugins: std::collections::BTreeMap::new(),
    };
    plugin_options.insert_versioned(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
        crate::plugins::FormatVersion::ONE,
        serde_json::json!({ "termination": { "kind": "unknown" } }),
    );
    let err = match core
        .session(session("rlm-child-bad-extras"))
        .create(crate::SessionCreation::child_of(
            crate::plugins::SessionToolAccess::ambient(),
            SessionId::from("rlm-root"),
            mock_session_spec().plugin_options(plugin_options),
        ))
        .await
    {
        Ok(_) => panic!("malformed RLM create extras should fail session creation"),
        Err(error) => error,
    };
    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = &err
    else {
        panic!("expected a typed session config refusal, got: {err:?}");
    };
    assert_eq!(refusal.owner, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
    assert_eq!(refusal.at, lash_core::RefusalSite::Creation);
    assert!(
        matches!(
            refusal.reason,
            lash_core::ConfigRefusalReason::Unreadable {
                role: lash_core::ConfigValueRole::CreationInput,
                ..
            }
        ),
        "{refusal:?}"
    );
    assert!(
        !core
            .session(session("rlm-child-bad-extras"))
            .durable()
            .await
            .expect("handle")
            .exists()
            .await
            .expect("existence"),
        "the refused child was not created"
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_session_state_appends_preserve_concurrent_retirement_refusals() {
    let backend = sqlite_memory_store_backend().await;
    let factory = backend.session_store_factory();
    let core = standard_core_over(backend);
    for (session_id, append_plugin_body) in [
        ("retired-append-messages", false),
        ("retired-append-plugin-body", true),
    ] {
        core.session(session(session_id))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
            .await
            .expect("created");
        let opened = core
            .session(session(session_id))
            .open()
            .await
            .expect("open");
        factory
            .delete_session(&SessionId::from(session_id))
            .await
            .expect("retire session before public state append");
        let error = if append_plugin_body {
            Box::pin(opened.admin().state().append_plugin_body(
                "test-plugin",
                serde_json::json!({ "retired": true }),
                "host:session_lifecycle:append_plugin_body:235".to_string(),
            ))
            .await
            .expect_err("plugin-body append must preserve the retirement refusal")
        } else {
            Box::pin(opened.admin().state().append_messages(
                vec![lash_core::PluginMessage::text(
                    lash_core::MessageRole::User,
                    "must not append",
                )],
                "host:session_lifecycle:append_messages:240".to_string(),
            ))
            .await
            .expect_err("message append must preserve the retirement refusal")
        };
        // A host append is a session command (FIG-4202): the retired session
        // refuses its submission, typed, before anything is queued.
        assert!(
            matches!(
                &error,
                EmbedError::Runtime(runtime)
                    if runtime.code == lash_core::RuntimeErrorCode::SessionDeleted
                        && matches!(
                            &runtime.cause,
                            Some(lash_core::RuntimeErrorCause::SessionDeleted {
                                session_id: deleted_session_id,
                            }) if deleted_session_id == session_id
                        )
            ),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(
                &StoreError::SessionDeleted {
                    session_id: SessionId::from(session_id),
                }
                .to_string()
            ),
            "{error}"
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// The task the frame switch hands the new frame.
const FRAME_TASK: &str = "carry on under the patched model";

/// A tool that switches the agent frame, so the frame that follows records
/// the policy the session runs then.
struct SwitchFrame;

#[async_trait]
impl crate::tools::StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("patched-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(FRAME_TASK.to_owned()),
            })
            .into()
    }
}

/// FIG-4099: a model change is a config patch, and the patched model reaches
/// every runtime consumer: the session's policy, the model request, the
/// persisted state, the frame a later switch opens and a process's
/// execution environment. A frame recorded before the patch keeps its model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_model_patch_reaches_all_runtime_consumers() {
    use lash_core::facade_support::RuntimeSessionStateFacadeOps as _;
    let historical_model = llm_profile_spec("historical-model", None, 11_111);
    let builder_model = llm_profile_spec("builder-model", None, 77_777);
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let requests = Arc::clone(&requests);
            move |request: LlmRequest| {
                requests
                    .lock_recover()
                    .push(request.model.wire_model().to_string());
                let text = last_user_text(&request);
                async move {
                    if text == "switch" {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "patched-frame-switch".to_owned(),
                                tool_name: "switch_frame".to_owned(),
                                input_json: "{}".to_owned(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        });
                    }
                    Ok(text_response("ok"))
                }
            }
        })
        .build()
        .into_handle();
    let definition = lash_core::ToolDefinition::raw(
        "switch_frame",
        "switch_frame",
        "Switches the agent frame.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas")
    .with_execution(std::time::Duration::from_secs(120));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .llm_profiles(test_catalog(
        provider,
        [historical_model.clone(), builder_model.clone()],
    ))
    .tools(Arc::new(crate::tools::StaticToolProvider::new(
        vec![definition],
        SwitchFrame,
    )))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session_id = SessionId::from("reconcile-open");
    core.session(session("reconcile-open"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            session_spec_for(&historical_model),
        ))
        .await
        .expect("created");
    let opened = core
        .session(session("reconcile-open"))
        .open()
        .await
        .expect("open");
    assert_eq!(
        opened.policy_snapshot().wire_model(),
        Some("historical-model"),
        "creation records the historical model"
    );
    let historical = opened
        .send(crate::TurnInput::text("record the historical frame"))
        .output()
        .await
        .expect("the historical turn answers");
    assert!(historical.is_success(), "{historical:?}");
    let historical_frame_id = history_frames(
        durable_history(&opened.durable()).await.expect("history"),
        &session_id,
    )
    .last()
    .expect("the historical turn owns a frame")
    .frame_node_id
    .clone();

    opened
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("builder-model"),
            },
        ))
        .await
        .expect("the patch applies");
    let opened = core
        .session(session("reconcile-open"))
        .open()
        .await
        .expect("reopen");
    let policy = opened.policy_snapshot();
    assert_eq!(
        policy.model,
        Some(recorded_llm_profile(builder_model.clone())),
        "consumer 1: the session's policy"
    );
    requests.lock_recover().clear();
    let switched = opened
        .send(crate::TurnInput::text("switch"))
        .output()
        .await
        .expect("the switching turn answers");
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    // The switch mailed its task: the session's next turn runs it on the new
    // frame.
    let follow_on = opened
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await
        .expect("the follow-on answers");
    assert!(follow_on.is_success(), "{follow_on:?}");
    let seen = requests.lock_recover().clone();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|model| model == "builder-model"),
        "consumer 2: every model request runs the patched model: {seen:?}"
    );

    let state = opened
        .admin()
        .state()
        .persist_current()
        .await
        .expect("persisted state");
    assert_eq!(
        state.policy.model,
        Some(recorded_llm_profile(builder_model.clone())),
        "consumer 3: the persisted state"
    );
    let frames = history_frames(
        durable_history(&opened.durable()).await.expect("history"),
        &session_id,
    );
    let historical = frames
        .iter()
        .find(|frame| frame.frame_node_id == historical_frame_id)
        .expect("historical frame remains");
    assert_eq!(
        historical.assignment.policy.wire_model(),
        Some("historical-model"),
        "a frame recorded before the patch keeps its model"
    );
    let current = frames.last().expect("the switched-to frame");
    assert_ne!(current.frame_node_id, historical_frame_id);
    assert_eq!(
        current.assignment.policy.model,
        Some(recorded_llm_profile(builder_model.clone())),
        "consumer 4: the frame a later switch opens"
    );
    let execution_env = state.process_execution_env_spec(&policy);
    assert_eq!(
        execution_env.policy.model,
        Some(recorded_llm_profile(builder_model)),
        "consumer 5: a process's execution environment"
    );
    core.shutdown().await.expect("shutdown");
}

/// The state a host supplies to `open_with_state` is the session's, config
/// included: its model survives whatever the session was created with, and
/// its frame history is not rewritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_with_state_keeps_supplied_policy_without_rewriting_frame_history() {
    use lash_core::facade_support::{AgentFrameReasonFacadeOps as _, SessionGraphFacadeOps as _};
    let session_id = SessionId::from("reconcile-open-with-state");
    let unbounded = || {
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    };
    let with_model = |model: &str, window: usize| lash_core::SessionPolicy {
        model: Some(recorded_llm_profile(llm_profile_spec(model, None, window))),
        ..unbounded()
    };
    let mut persisted = lash_core::RuntimeSessionState {
        session_id: session_id.clone(),
        policy: with_model("historical-model", 11_111),
        agent_frames: Vec::new(),
        current_frame_node_id: None,
        ..lash_core::RuntimeSessionState::new(unbounded())
    };
    persisted.ensure_agent_frame_initialized();
    let historical_frame_id = persisted
        .current_frame_node_id
        .clone()
        .expect("the initial frame");
    let frame_key = lash_core::FrameKey::from_caller_material("conflicting-frame")
        .expect("non-empty frame material");
    let frame_node_id = lash_core::facade_support::frame_node_id(&session_id, frame_key.as_str());
    let mut nodes = persisted.session_graph.nodes.clone();
    nodes.push(Arc::new(lash_core::SessionNodeRecord {
        node_id: lash_core::NodeId::fixture(frame_node_id.to_string()),
        parent_node_id: persisted.session_graph.leaf_node_id.clone(),
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("fixture timestamp"),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core::AgentFrameReason::continue_as(),
            assignment: lash_core::AgentFrameAssignment::unconfigured(with_model(
                "current-frame-model",
                22_222,
            )),
        },
    }));
    persisted.session_graph = lash_core::SessionGraph::from_shared_nodes(
        nodes,
        Some(lash_core::NodeId::fixture(frame_node_id.to_string())),
    )
    .expect("the fixture graph is valid");
    persisted.current_frame_node_id = Some(frame_node_id);
    persisted.agent_frames = persisted.session_graph.agent_frame_records(&session_id);
    persisted.policy = with_model("top-level-model", 33_333);
    let supplied_model = persisted.policy.model.clone();

    let builder_model = llm_profile_spec("builder-model", None, 77_777);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), builder_model.clone())
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    core.session(session("reconcile-open-with-state"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            session_spec_for(&builder_model),
        ))
        .await
        .expect("created");
    let opened = core
        .session(session("reconcile-open-with-state"))
        .open_with_state(persisted)
        .await
        .expect("open with state");
    let state = opened.admin().state().export().await;
    assert_eq!(state.policy.model, supplied_model);
    let frames = state.session_graph.agent_frame_records(&session_id);
    assert_eq!(
        frames
            .last()
            .expect("current frame")
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default(),
        "current-frame-model"
    );
    assert_eq!(
        frames
            .iter()
            .find(|frame| frame.frame_node_id == historical_frame_id)
            .expect("historical frame")
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default(),
        "historical-model"
    );
    core.shutdown().await.expect("shutdown");
}
