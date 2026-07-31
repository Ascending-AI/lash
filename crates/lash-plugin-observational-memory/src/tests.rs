use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash_core::plugin::{
    PluginError, PluginLifecycleEvent, SessionGraphService, SessionStateService,
};
use lash_core::{
    AppendSessionNodesRequest, AppendSessionNodesResult, DirectCompletion, DirectCompletionClient,
    DirectRequest, Message, MessageRole, Part, PartKind, SessionAppendNode, SessionGraph,
    SessionReadView, SessionSnapshot, SessionStateChangedContext,
};

use crate::ObservationalMemoryConfig;
use crate::constants::{ACTIVE_STATE_PLUGIN_TYPE, BUFFERED_OBSERVATION_PLUGIN_TYPE};
use crate::graph_state::{
    build_graph_state, prefix_len_covering_tokens, retained_message_tokens_by_message_id,
};
use crate::model::MessageNode;
use crate::prompts::parse_memory_output;

fn user_message(id: &str, content: &str) -> MessageNode {
    MessageNode {
        timestamp: "2026-04-14T10:00:00Z".to_string(),
        message: Message {
            id: id.to_string(),
            role: MessageRole::User,
            parts: vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: content.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: lash_core::PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]
            .into(),
            origin: None,
        },
    }
}

#[derive(Default)]
struct RecordingHost {
    append_requests: Mutex<Vec<(String, AppendSessionNodesRequest)>>,
    reject_as_stale: AtomicBool,
}

#[async_trait]
impl SessionStateService for RecordingHost {}

#[async_trait]
impl SessionGraphService for RecordingHost {
    async fn append_session_nodes(
        &self,
        session_id: &str,
        request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesResult, PluginError> {
        let node_ids = request
            .nodes
            .iter()
            .enumerate()
            .map(|(index, _)| format!("appended-{index}"))
            .collect::<Vec<_>>();
        let leaf_node_id = node_ids
            .last()
            .cloned()
            .or_else(|| request.requires_ancestor_node_id.clone())
            .unwrap_or_else(|| "empty-append".to_string());
        let required_ancestor = request.requires_ancestor_node_id.clone();
        self.append_requests
            .lock()
            .expect("append requests lock")
            .push((session_id.to_string(), request));
        if self.reject_as_stale.load(Ordering::SeqCst) {
            Ok(AppendSessionNodesResult::StaleBranch {
                current_leaf_node_id: required_ancestor,
            })
        } else {
            Ok(AppendSessionNodesResult::Appended {
                node_ids,
                leaf_node_id,
            })
        }
    }
}

#[tokio::test]
async fn stale_observation_append_is_dropped_without_mutating_the_local_graph() {
    let host = Arc::new(RecordingHost::default());
    host.reject_as_stale.store(true, Ordering::SeqCst);
    let mut graph = SessionGraph::default();
    graph.append_message(user_message("committed", "durable transcript").message);
    let graph_before = serde_json::to_value(&graph).expect("serialize graph before stale append");
    let sessions: Arc<dyn SessionGraphService> = host.clone();
    let om_host = crate::host::OmRuntimeHost::new(
        "session",
        &sessions,
        DirectCompletionClient::from_fn(|_, _| {
            Err(PluginError::Session(
                "direct completion must not run in append test".to_string(),
            ))
        }),
    );

    let result = om_host
        .append_plugin_nodes(
            &graph,
            vec![(
                BUFFERED_OBSERVATION_PLUGIN_TYPE.to_string(),
                serde_json::json!({"observations": "derived from stale transcript"}),
            )],
        )
        .await
        .expect("stale branch is an intentional best-effort drop");

    assert!(result.is_none(), "stale derived memory must be dropped");
    assert_eq!(
        serde_json::to_value(&graph).expect("serialize graph after stale append"),
        graph_before,
        "dropping stale observational memory must not mutate the caller's graph"
    );
    assert_eq!(
        host.append_requests
            .lock()
            .expect("append requests lock")
            .len(),
        1,
        "the drop ruling must be exercised through an actual host CAS response"
    );
}

fn post_persist_context_with_completion(
    session_id: &str,
    graph: SessionGraph,
    host: Arc<RecordingHost>,
    completion_text: String,
) -> SessionStateChangedContext<'static> {
    let sessions: Arc<dyn SessionStateService> = host.clone();
    let session_graph: Arc<dyn SessionGraphService> = host;
    SessionStateChangedContext {
        session_id: session_id.to_string(),
        state: SessionReadView::from_snapshot(&SessionSnapshot {
            session_id: session_id.to_string(),
            session_graph: graph,
            policy: lash_core::testing::mock_session_policy(),
            ..Default::default()
        }),
        sessions,
        session_graph,
        direct_completions: DirectCompletionClient::from_fn(
            move |_request: DirectRequest, _usage_source: String| {
                let completion_text = completion_text.clone();
                Ok(DirectCompletion {
                    text: completion_text,
                    usage: Default::default(),
                    llm_call: lash_core::LlmCallRecord {
                        call_id: lash_core::LlmCallId("observational-memory-test".to_string()),
                        label: None,
                        attempts: Vec::new(),
                    },
                })
            },
        ),
    }
}

#[tokio::test]
async fn maintenance_uses_post_persist_leaf_as_append_cas_ancestor() {
    let host = Arc::new(RecordingHost::default());
    let config = ObservationalMemoryConfig {
        observation_buffer_tokens: 1,
        observation_max_tokens_per_batch: 1,
        ..Default::default()
    };
    let hook = crate::observational_memory_event_hook(config);

    let mut graph = SessionGraph::default();
    graph.append_message(user_message("committed-message", &"x".repeat(64)).message);
    let committed_leaf = graph
        .leaf_node_id
        .clone()
        .expect("committed graph should have a leaf");
    let completion = "<observations>\nDate: May 19, 2026\n- User needs the post-persist graph as the CAS base.\n</observations>\n<current-task>\nVerify OM append ancestry.\n</current-task>\n<suggested-response>\nContinue.\n</suggested-response>"
        .to_string();

    hook(PluginLifecycleEvent::TurnPersisted(Box::new(
        post_persist_context_with_completion("session", graph, host.clone(), completion),
    )))
    .await
    .expect("turn persisted hook");

    let append_requests = host.append_requests.lock().expect("append requests lock");
    assert_eq!(append_requests.len(), 1);
    let (session_id, request) = &append_requests[0];
    assert_eq!(session_id, "session");
    assert_eq!(
        request.requires_ancestor_node_id.as_deref(),
        Some(committed_leaf.as_str())
    );
    assert_eq!(request.nodes.len(), 1);
    let SessionAppendNode::Plugin {
        plugin_type, body, ..
    } = &request.nodes[0]
    else {
        panic!("expected OM maintenance to append a plugin node");
    };
    assert_eq!(plugin_type, BUFFERED_OBSERVATION_PLUGIN_TYPE);
    assert_eq!(
        body.get("observed_through_message_id")
            .and_then(|value| value.as_str()),
        Some("committed-message")
    );
    assert!(
        body.get("observations")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .contains("post-persist graph")
    );
}

#[tokio::test]
async fn maintenance_hook_only_runs_from_post_persisted_graph() {
    let host = Arc::new(RecordingHost::default());
    let config = ObservationalMemoryConfig {
        observation_buffer_tokens: 1,
        ..Default::default()
    };
    let hook = crate::observational_memory_event_hook(config);

    hook(PluginLifecycleEvent::TurnFinalized(Arc::new(
        lash_core::testing::mock_assembled_turn("session", "done"),
    )))
    .await
    .expect("turn finalized hook");
    assert!(
        host.append_requests
            .lock()
            .expect("append requests lock")
            .is_empty(),
        "pre-persist turn finalization must not run OM maintenance"
    );

    let mut graph = SessionGraph::default();
    graph.append_message(user_message("post-persist-message", "x".repeat(64).as_str()).message);
    hook(PluginLifecycleEvent::TurnPersisted(Box::new(
        post_persist_context_with_completion(
            "session",
            graph,
            host.clone(),
            "<observations>\n- Persisted graph only.\n</observations>".to_string(),
        ),
    )))
    .await
    .expect("turn persisted hook");

    assert_eq!(
        host.append_requests
            .lock()
            .expect("append requests lock")
            .len(),
        1
    );
}

#[test]
fn build_graph_state_resets_buffers_after_active_state() {
    let mut graph = SessionGraph::default();
    graph.append_message(user_message("m1", "hello").message);
    graph.append_plugin(
        BUFFERED_OBSERVATION_PLUGIN_TYPE,
        serde_json::json!({
            "observed_through_message_id": "m1",
            "observations": "old buffered",
            "observation_tokens": 10
        }),
    );
    graph.append_plugin(
        ACTIVE_STATE_PLUGIN_TYPE,
        serde_json::json!({
            "observed_through_message_id": "m1",
            "observations": "active memory"
        }),
    );
    graph.append_message(user_message("m2", "need help").message);
    graph.append_plugin(
        BUFFERED_OBSERVATION_PLUGIN_TYPE,
        serde_json::json!({
            "observed_through_message_id": "m2",
            "observations": "new buffered",
            "observation_tokens": 20
        }),
    );

    let state = build_graph_state(&graph);
    assert_eq!(
        state.active.as_ref().map(|item| item.observations.as_str()),
        Some("active memory")
    );
    assert_eq!(state.buffered_observations.len(), 1);
    assert_eq!(
        state.buffered_observations[0].observations,
        "new buffered".to_string()
    );
}

#[test]
fn retained_message_tokens_tracks_suffix_after_message() {
    let messages = vec![
        user_message("m1", &"a".repeat(4000)),
        user_message("m2", &"b".repeat(4000)),
        user_message("m3", &"c".repeat(4000)),
    ];
    let retained = retained_message_tokens_by_message_id(&messages);
    assert_eq!(retained.get("m3").copied(), Some(0));
    assert!(retained.get("m2").copied().unwrap_or_default() > 0);
    assert!(retained.get("m1").copied().unwrap_or_default() > retained["m2"]);
}

#[test]
fn prefix_len_covering_tokens_handles_partial_prefix() {
    let messages = vec![
        user_message("m1", &"a".repeat(4000)),
        user_message("m2", &"b".repeat(4000)),
        user_message("m3", &"c".repeat(4000)),
    ];
    let prefix = prefix_len_covering_tokens(&messages, 2000).expect("prefix");
    assert_eq!(prefix, 2);
}

#[test]
fn parse_memory_output_extracts_xml_sections() {
    let parsed = parse_memory_output(
        "<observations>\nDate: Apr 12, 2026\n* 🔴 Test\n</observations>\n<current-task>\nWork\n</current-task>\n<suggested-response>\nContinue\n</suggested-response>",
    );
    assert!(parsed.observations.contains("Test"));
    assert_eq!(parsed.current_task.as_deref(), Some("Work"));
    assert_eq!(parsed.suggested_response.as_deref(), Some("Continue"));
}

#[tokio::test]
async fn maintenance_calls_carry_the_session_options_bounded_by_the_session_model() {
    // These calls take their generation options from the session policy, and
    // the direct path they use has no model limits to check a cap against. A
    // session that capped output and then moved to a smaller model must not
    // keep taking turns (which clamp) while every maintenance call sends an
    // over-capacity cap and fails at the provider.
    let host = Arc::new(RecordingHost::default());
    let config = ObservationalMemoryConfig {
        observation_buffer_tokens: 1,
        observation_max_tokens_per_batch: 1,
        ..Default::default()
    };
    let hook = crate::observational_memory_event_hook(config);

    let mut graph = SessionGraph::default();
    graph.append_message(user_message("committed-message", &"x".repeat(64)).message);

    let requested = lash_core::GenerationOptions {
        output_token_cap: std::num::NonZeroUsize::new(32_000),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
    };
    let mut policy = lash_core::testing::mock_session_policy();
    policy.model = lash_core::ModelSpec::from_token_limits(
        "small-output-model",
        Default::default(),
        200_000,
        Some(2_048),
    )
    .expect("valid test model");
    policy.generation = requested.clone();

    let captured: Arc<Mutex<Vec<lash_core::GenerationOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_client = Arc::clone(&captured);
    let sessions: Arc<dyn SessionStateService> = host.clone();
    let session_graph: Arc<dyn SessionGraphService> = host.clone();
    let context = SessionStateChangedContext {
        session_id: "session".to_string(),
        state: SessionReadView::from_snapshot(&SessionSnapshot {
            session_id: "session".to_string(),
            session_graph: graph,
            policy,
            ..Default::default()
        }),
        sessions,
        session_graph,
        direct_completions: DirectCompletionClient::from_fn(
            move |request: DirectRequest, _usage_source: String| {
                captured_for_client
                    .lock()
                    .expect("capture lock")
                    .push(request.generation.clone());
                Ok(DirectCompletion {
                    text: "<observations>\nDate: May 19, 2026\n- Bounded.\n</observations>\n<current-task>\nVerify.\n</current-task>\n<suggested-response>\nContinue.\n</suggested-response>"
                        .to_string(),
                    usage: Default::default(),
                    llm_call: lash_core::LlmCallRecord {
                        call_id: lash_core::LlmCallId("observational-memory-test".to_string()),
                        label: None,
                        attempts: Vec::new(),
                    },
                })
            },
        ),
    };

    hook(PluginLifecycleEvent::TurnPersisted(Box::new(context)))
        .await
        .expect("turn persisted hook");

    let seen = captured.lock().expect("capture lock").clone();
    let sent = seen.first().expect("one maintenance call");
    assert_eq!(
        sent.output_token_cap,
        std::num::NonZeroUsize::new(2_048),
        "the cap is bounded by what the session's model can produce"
    );
    assert_eq!(
        (sent.temperature.clone(), sent.seed),
        (requested.temperature, requested.seed),
        "the session's sampling intent still reaches its own maintenance calls"
    );
}
