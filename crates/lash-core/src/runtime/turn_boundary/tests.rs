use super::*;
use crate::SessionId;
use crate::facade_support::AgentFrameReasonFacadeOps;
use crate::runtime::tests::helpers::RecordingStore;
use crate::session_model::{MessageRole, Part};
use crate::store::SessionStore;
use crate::{
    AgentFrameReason, FrameKey, Message, OpenAgentFrameRequest, SessionGraph, shared_parts,
};
use lash_sansio::core_support::MessageSequenceCoreSupport;
use lash_sansio::sync::MutexExt;
const UNBOUNDED: crate::TurnBudget = crate::TurnBudget::Unbounded;
fn cancelled_outcome() -> TurnOutcome {
    TurnOutcome::Stopped(crate::TurnStop::Cancelled {
        evidence: crate::TurnCancellationEvidence::internal("turn-boundary-test"),
    })
}
fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            content.to_string(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    }
}
#[test]
fn turn_draft_appends_resident_nodes_not_yet_durable() {
    let durable = text_message("durable", MessageRole::User, "already durable");
    let pending = text_message("pending", MessageRole::Assistant, "not durable yet");
    let graph = SessionGraph::from_active_read_state(&[durable, pending]);
    let durable_node_id = graph.nodes[0].node_id.clone();
    let pending_node_id = graph.nodes[1].node_id.clone();
    let mut state = state_with_graph(graph);
    let frame_node_id = state.current_frame_node_id.clone().expect("initial frame");
    state.persisted_node_ids.insert(durable_node_id);

    let draft = TurnCommitDraft::from_state_with_clock(
        state,
        Arc::new(crate::SystemClock),
        "masked-path-regression",
    );
    let graph = draft.graph_commit();
    let nodes = graph.nodes();
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![frame_node_id.as_str(), pending_node_id.as_str()]
    );
}
fn test_protocol_event(kind: &str) -> crate::ProtocolEvent {
    crate::ProtocolEvent::typed(
        "test_protocol",
        serde_json::json!({
            "kind": kind,
            "payload": { "test": true },
        }),
    )
    .expect("test protocol event serializes")
}
/// A recording store over a fresh memory catalog, and the view of the
/// fixtures' `session-1` on it.
async fn recording_session() -> (Arc<RecordingStore>, SessionStore) {
    let recording = Arc::new(crate::testing::unbound_recording_store().await);
    let session = SessionStore::new(recording.clone(), SessionId::from("session-1"))
        .expect("valid fixture session id");
    (recording, session)
}
fn state_with_graph(graph: SessionGraph) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("session-1"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            UNBOUNDED,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    if !graph.nodes.is_empty() {
        let frame_node_id = state.current_frame_node_id.clone().expect("initial frame");
        let mut nodes = state.session_graph.nodes.clone();
        nodes.extend(graph.nodes.iter().map(|node| {
            let mut node = node.as_ref().clone();
            if node.parent_node_id.is_none() {
                node.parent_node_id = Some(crate::NodeId::fixture(frame_node_id.to_string()));
            }
            std::sync::Arc::new(node)
        }));
        state.session_graph = SessionGraph::from_shared_nodes(nodes, graph.leaf_node_id.clone())
            .expect("turn-boundary fixture graph is valid");
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    }
    state
}
fn frame_key(material: &str) -> FrameKey {
    FrameKey::from_caller_material(material).expect("non-empty frame material")
}
fn frame_request(frame_key: FrameKey, reason: AgentFrameReason) -> OpenAgentFrameRequest {
    OpenAgentFrameRequest::new(frame_key, reason)
}
fn outcome_switch(
    operation_id: &str,
    frame_key: FrameKey,
    initial_nodes: Vec<crate::SessionAppendNode>,
) -> super::super::turn_commit_draft::OutcomeFrameSwitch {
    super::super::turn_commit_draft::OutcomeFrameSwitch {
        operation_id: operation_id.to_string(),
        frame_key,
        task: "continue in the next frame".to_string(),
        reason: AgentFrameReason::continue_as(),
        initial_nodes,
    }
}

async fn admitted_boundary(store: &SessionStore, state: RuntimeSessionState) -> TurnBoundary {
    assert_eq!(store.session_id(), &state.session_id);
    store
        .store()
        .admit_session(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: state.session_id.clone(),
            relation: crate::SessionRelation::Root,
            config: state.policy.clone().into(),
            head: crate::SessionCreationHead::Config,
        })
        .await
        .expect("admit turn-boundary test session");
    TurnBoundary::from_state(state)
}
#[test]
fn agent_frame_switch_seeds_the_new_frame_without_a_tool_call_event() {
    let graph =
        SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "old frame")]);
    let mut state = state_with_graph(graph);
    state.ensure_agent_frame_initialized();
    let previous_frame_node_id = state.current_frame_node_id.clone();
    let frame_key = frame_key("frame-2");
    let seed_node = crate::SessionAppendNode::message(crate::PluginMessage::text(
        MessageRole::User,
        "seed message",
    ));
    let draft = TurnGraphAppendDraft::from_resident_state(&state, Arc::new(crate::SystemClock));
    let recorded = draft
        .record_outcome_frame_switch(
            &state.session_id.clone(),
            state.current_frame_node_id.as_deref(),
            &outcome_switch(
                "session-1:turn:turn-outcome-frame-switch",
                frame_key.clone(),
                vec![seed_node],
            ),
        )
        .expect("record the turn's one frame switch");
    assert!(recorded.opened);
    assert_eq!(recorded.initial_node_ids.len(), 1);
    draft
        .fold_into_final_state(&mut state)
        .expect("materialize a fresh frame switch");
    let expected_frame_node_id =
        crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());

    assert_eq!(state.session_id, "session-1");
    assert_eq!(
        state.current_frame_node_id.as_deref(),
        Some(expected_frame_node_id.as_str())
    );
    let current = state.current_agent_frame().expect("current frame");
    assert_eq!(
        current.previous_frame_node_id.as_deref(),
        previous_frame_node_id.as_deref()
    );
    assert_eq!(
        current.reason.as_str(),
        crate::AgentFrameReason::CONTINUE_AS
    );
    let current_read = state.session_graph.read_model();
    assert_eq!(current_read.messages.len(), 1);
    assert_eq!(current_read.messages[0].parts[0].content(), "seed message");
    // Only the current frame is resident: the previous frame's messages
    // leave the read model.
    assert!(
        !current_read
            .messages
            .iter()
            .any(|message| message.parts[0].content() == "old frame")
    );
}
#[test]
fn open_agent_frame_seeds_compaction_frame_and_is_replay_idempotent() {
    let graph = SessionGraph::from_active_read_state(&[text_message(
        "u0",
        MessageRole::User,
        "old durable frame",
    )]);
    let mut state = state_with_graph(graph);
    state.ensure_agent_frame_initialized();
    let previous_frame_node_id = state.current_frame_node_id.clone();
    let previous_frame_node_id_value = previous_frame_node_id
        .as_deref()
        .expect("current frame")
        .to_string();
    let leaf_node_id = state.session_graph.leaf_node_id.clone();
    let mut nodes = state.session_graph.nodes.to_vec();
    let previous = nodes
        .iter_mut()
        .find(|node| node.node_id == previous_frame_node_id_value)
        .expect("current frame node");
    let previous = std::sync::Arc::make_mut(previous);
    let crate::SessionNodePayload::FrameOpen { assignment, .. } = &mut previous.payload else {
        panic!("current frame id must identify FrameOpen");
    };
    assignment.plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    assignment
        .plugin_config
        .insert("protocol", serde_json::json!({ "mode": "test" }));
    state.authority.plugin_config = assignment.plugin_config.clone();
    state.session_graph = SessionGraph::from_shared_nodes(nodes, leaf_node_id)
        .expect("frame-compaction fixture graph is valid");
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    let frame_key = frame_key("frame-compaction");
    let seed_node = crate::SessionAppendNode::message(
        crate::PluginMessage::text(MessageRole::Assistant, "Compaction summary:\nold work")
            .with_origin(crate::MessageOrigin::Plugin {
                plugin_id: "standard_compaction".to_string(),
                transient: false,
            }),
    );
    let opened = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key.clone(), AgentFrameReason::compaction())
            .with_initial_nodes(vec![seed_node.clone()]),
        &crate::SystemClock,
    )
    .expect("open a new compaction frame");
    assert!(opened.opened);
    assert_eq!(
        state.current_frame_node_id.as_deref(),
        Some(opened.frame_node_id.as_str())
    );
    let current = state.current_agent_frame().expect("current frame");
    assert_eq!(current.reason.as_str(), crate::AgentFrameReason::COMPACTION);
    assert_eq!(
        current.previous_frame_node_id.as_deref(),
        previous_frame_node_id.as_deref()
    );
    assert_eq!(
        current.protocol_turn_options().payload,
        serde_json::json!({ "mode": "test" })
    );

    let current_read = state.session_graph.read_model();
    assert_eq!(current_read.messages.len(), 1);
    assert_eq!(
        current_read.messages[0].parts[0].content(),
        "Compaction summary:\nold work"
    );
    assert!(matches!(
        current_read.messages[0].origin.as_ref(),
        Some(crate::MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == "standard_compaction"
    ));

    // Only the current frame is resident: the previous frame's messages
    // leave the read model.
    assert!(
        !current_read
            .messages
            .iter()
            .any(|message| message.parts[0].content() == "old durable frame")
    );

    let replay = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key, AgentFrameReason::compaction())
            .with_initial_nodes(vec![seed_node]),
        &crate::SystemClock,
    )
    .expect("replay the current compaction frame");
    assert!(!replay.opened);
    let replay_read = state.session_graph.read_model();
    assert_eq!(replay_read.messages.len(), 1);
}

#[test]
fn reopening_a_previous_frame_refuses_and_keeps_the_current_frame() {
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("frame-switch-back"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            UNBOUNDED,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized_with_clock(&clock);
    let frame_a = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("frame-a"), AgentFrameReason::new("frame-a")),
        &clock,
    )
    .expect("open frame a");
    assert!(frame_a.opened);
    let frame_b = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("frame-b"), AgentFrameReason::new("frame-b")),
        &clock,
    )
    .expect("open frame b");
    assert!(frame_b.opened);

    let error = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("frame-a"), AgentFrameReason::new("frame-a")).with_initial_nodes(
            vec![crate::SessionAppendNode::message(
                crate::PluginMessage::text(MessageRole::Assistant, "new frame-a seed"),
            )],
        ),
        &clock,
    )
    .expect_err("switching to an existing non-current frame must refuse");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert_eq!(
        state.current_frame_node_id.as_deref(),
        Some(frame_b.frame_node_id.as_str())
    );
    assert_ne!(
        state.session_graph.leaf_node_id.as_deref(),
        Some(frame_a.frame_node_id.as_str())
    );
    assert_eq!(
        state
            .session_graph
            .nearest_frame_node_id(state.session_graph.leaf_node_id.as_deref())
            .map(crate::NodeId::as_str),
        Some(frame_b.frame_node_id.as_str())
    );
}

/// A turn outcome naming a persisted, non-current frame aborts the commit
/// with the typed refusal, leaves resident state untouched, and writes
/// nothing durable; the next turn on the same state commits normally.
#[tokio::test]
async fn final_commit_rejects_a_turn_tail_over_the_node_budget_before_store_mutation() {
    let messages = (0..crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT)
        .map(|index| {
            text_message(
                &format!("message-{index}"),
                MessageRole::Assistant,
                &format!("step {index}"),
            )
        })
        .collect::<Vec<_>>();
    let (recording, store) = recording_session().await;
    let mut pipeline = admitted_boundary(&store, state_with_graph(SessionGraph::default())).await;
    pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED, crate::MaxToolCalls::new(1024)),
            turn_index: 1,
            messages: MessageSequence::from_base(messages.into()),
            event_delta: Vec::new(),
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("build the oversized turn tail in memory");

    let returned_state = pipeline.export_state_for_assembly();
    let error = pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: returned_state.clone(),
            tool_calls: &[],
            omitted: None,
            retained_outputs: &[],
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            ingress_settlement: TurnIngressSettlement::default(),
            pending_follow_on: None,
            recorded_attachment_intent_ids: Default::default(),
        })
        .await
        .expect_err("the final append must enforce the transaction node budget");

    assert!(matches!(
        error,
        StoreError::CommitNodeBudgetExceeded {
            node_count,
            max_nodes,
        } if node_count == crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT + 1
            && max_nodes == crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT
    ));
    assert_eq!(*recording.runtime_commit_count.lock_recover(), 0);
    assert!(
        store
            .load_session_window(crate::store::WindowSelector::Current)
            .await
            .expect("load session")
            .is_none_or(|read| read.window.nodes.is_empty())
    );
}

#[tokio::test]
async fn no_store_final_commit_discards_snapshots_without_touching_graph() {
    let graph =
        SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "hello")]);
    let mut state = state_with_graph(graph.clone());
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.set_plugin_state(Some(crate::PluginState::default()));
    state.set_execution_state_snapshot(Some(b"runtime".to_vec().into()));
    let mut pipeline = TurnBoundary::from_state(state);
    let returned_state = pipeline.export_state_for_assembly();

    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: returned_state.clone(),
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: None,
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            tool_calls: &[],
            omitted: None,
            retained_outputs: &[],
            ingress_settlement: TurnIngressSettlement::default(),
            pending_follow_on: None,
            recorded_attachment_intent_ids: Default::default(),
        })
        .await
        .expect("no-store commit");

    let state = pipeline.state_mut();
    assert_eq!(state.session_graph.nodes.len(), graph.nodes.len() + 1);
    assert!(state.tool_state_snapshot().is_none());
    assert!(state.plugin_state().is_none());
    // Without a store the committed execution snapshot is the only accepted
    // copy, so the storeless release keeps it resident for a later restore.
    assert_eq!(
        state.execution_state_snapshot().as_deref(),
        Some(b"runtime".as_slice()),
        "storeless commits retain the accepted execution snapshot"
    );
}

/// FIG-3515 Done-when: after a tool value that embeds an attachment, the
/// next turn's prepared checkpoint and progress boundary still advance. The
/// result is one part with ordered blocks, so the history the gates check
/// stays resume-safe and the new turn's input and executed call reach the
/// draft instead of being skipped.
#[tokio::test]
async fn gates_advance_after_an_attachment_bearing_tool_result() {
    let call = |message_id: &str, call_id: &str| Message {
        id: message_id.to_string(),
        role: MessageRole::Assistant,
        parts: shared_parts(vec![Part::tool_call(
            format!("{message_id}.p0"),
            "{}".to_string(),
            crate::ToolCallId::fixture(call_id),
            call_id.to_string(),
            "shot".to_string(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    };
    let result = |message_id: &str, call_id: &str, content| Message {
        id: message_id.to_string(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::tool_result(
            format!("{message_id}.p0"),
            content,
            crate::ToolCallId::fixture(call_id),
            "shot".to_string(),
        )]),
        origin: None,
        reply_marker: None,
    };
    let image = crate::AttachmentSource::inline(
        crate::MediaType::parse("image/png").expect("png"),
        vec![1, 2, 3, 4],
    );
    let turn_one = vec![
        text_message("u1", MessageRole::User, "return the array"),
        call("a1", "turn-one-call"),
        result(
            "r1",
            "turn-one-call",
            vec![
                crate::ModelToolReturnPart::text("[\"before\","),
                crate::ModelToolReturnPart::Attachment(image),
                crate::ModelToolReturnPart::text(",\"after\"]"),
            ],
        ),
        text_message("a1-done", MessageRole::Assistant, "turn one done"),
    ];
    let mut prepared = turn_one.clone();
    prepared.push(text_message("u2", MessageRole::User, "turn two input"));

    let mut pipeline = TurnBoundary::from_state(state_with_graph(SessionGraph::default()));
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED, crate::MaxToolCalls::new(1024)),
            7,
            &MessageSequence::from_base(prepared.clone().into()),
            None,
        )
        .await
        .expect("prepared checkpoint");
    assert_eq!(
        pipeline.state().turn_index,
        7,
        "the prepared checkpoint advanced"
    );
    assert!(
        pipeline
            .message_sequence()
            .iter()
            .any(|message| message.id == "u2"),
        "turn 2's input is in the prepared checkpoint"
    );

    let mut progressed = prepared;
    progressed.push(call("a2", "turn-two-call"));
    progressed.push(result(
        "r2",
        "turn-two-call",
        vec![crate::ModelToolReturnPart::text("ok")],
    ));
    let boundary = pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED, crate::MaxToolCalls::new(1024)),
            turn_index: 8,
            messages: MessageSequence::from_base(progressed.into()),
            event_delta: vec![crate::SessionHistoryRecord::Protocol(test_protocol_event(
                "turn-two-step",
            ))],
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("progress boundary");
    assert_eq!(boundary.protocol_events.len(), 1, "the event delta is kept");
    assert_eq!(
        pipeline.state().turn_index,
        8,
        "the progress boundary advanced"
    );
    let ids: Vec<_> = pipeline
        .message_sequence()
        .iter()
        .map(|message| message.id.clone())
        .collect();
    assert!(
        ids.iter().any(|id| id == "a2") && ids.iter().any(|id| id == "r2"),
        "turn 2's executed call is in the draft: {ids:?}"
    );
}

#[test]
fn a_committed_frame_open_clears_execution_state_and_ends_the_last_committed_frame() {
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("frame-transition"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            UNBOUNDED,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized_with_clock(&clock);
    let committed = state
        .current_frame_node_id
        .clone()
        .expect("an initialized session has a frame");
    let committing = crate::ExecutionScope::turn(&state.session_id, "switching-turn");

    // Nothing is committed yet, so an open ends no frame.
    assert_eq!(
        committed_frame_transition(&state, None, SeedCarries::none(), &committing, &[]).unwrap(),
        None
    );
    state.mark_node_ids_persisted([crate::NodeId::fixture(committed.as_str().to_string())]);
    assert_eq!(
        committed_frame_transition(&state, None, SeedCarries::none(), &committing, &[]).unwrap(),
        None,
        "a commit that opens no frame ends none"
    );

    state.set_execution_state_snapshot(Some(b"frame-globals".to_vec().into()));
    let opened = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("frame-a"), AgentFrameReason::new("frame-a")),
        &clock,
    )
    .expect("open frame a");
    assert!(opened.opened);
    assert_eq!(
        state.execution_state_snapshot(),
        None,
        "a frame open wipes the frame's globals"
    );
    // Two opens since the last commit: the commit ends the committed frame.
    super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("frame-b"), AgentFrameReason::new("frame-b")),
        &clock,
    )
    .expect("open frame b");
    let successor = state.current_frame_node_id.clone().expect("frame b");
    let carried = crate::ArtifactName {
        store: crate::ArtifactStoreId::module(),
        artifact_ref: "module:v2:blake3:carried".to_string(),
    };
    let ended = crate::FrameEnvironmentId::new(state.session_id.clone(), committed.clone());
    let opened = crate::FrameEnvironmentId::new(state.session_id.clone(), successor);
    let gate = committing.journal_identity().unwrap();
    // A switch out of the committed frame carries its seed's modules.
    assert_eq!(
        committed_frame_transition(
            &state,
            Some(committed.clone()),
            SeedCarries::from_names(vec![carried.clone()]),
            &committing,
            &[]
        )
        .unwrap(),
        Some(crate::store::FrameTransition {
            ended: ended.clone(),
            successor: opened.clone(),
            carries: vec![carried.clone()],
            gate: gate.clone(),
        })
    );
    // A switch out of a frame opened in resident state since the last
    // commit, which the commit appends, carries its seed's modules out of
    // that frame; the store ends the committed frame beside it.
    let uncommitted = crate::FrameNodeId::new(opened_frame_a(&state)).unwrap();
    let appended = [
        crate::NodeId::fixture(uncommitted.as_str().to_string()),
        crate::NodeId::fixture(opened.frame_node_id().as_str().to_string()),
    ];
    assert_eq!(
        committed_frame_transition(
            &state,
            Some(uncommitted.clone()),
            SeedCarries::from_names(vec![carried.clone()]),
            &committing,
            &appended,
        )
        .unwrap(),
        Some(crate::store::FrameTransition {
            ended: crate::FrameEnvironmentId::new(state.session_id.clone(), uncommitted.clone()),
            successor: opened.clone(),
            carries: vec![carried.clone()],
            gate: gate.clone(),
        })
    );
    // A switch that names no frame, or one the commit does not leave, names
    // the committed frame and carries nothing out of it.
    for named in [None, Some(uncommitted)] {
        assert_eq!(
            committed_frame_transition(
                &state,
                named,
                SeedCarries::from_names(vec![carried.clone()]),
                &committing,
                &[]
            )
            .unwrap(),
            Some(crate::store::FrameTransition {
                ended: ended.clone(),
                successor: opened.clone(),
                carries: Vec::new(),
                gate: gate.clone(),
            })
        );
    }
}

/// The frame `open frame a` opened: the one the current frame follows.
fn opened_frame_a(state: &RuntimeSessionState) -> String {
    state
        .current_agent_frame()
        .and_then(|frame| frame.previous_frame_node_id.clone())
        .expect("frame b follows frame a")
        .into_inner()
}

#[test]
fn a_first_commit_that_switches_ends_the_first_frame_it_opens() {
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("first-turn-switch"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            UNBOUNDED,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized_with_clock(&clock);
    let first = state
        .current_frame_node_id
        .clone()
        .expect("the first frame");
    super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key("successor"), AgentFrameReason::continue_as()),
        &clock,
    )
    .expect("open the successor");
    let successor = state.current_frame_node_id.clone().expect("the successor");
    let committing = crate::ExecutionScope::turn(&state.session_id, "first-turn");
    let carried = crate::ArtifactName {
        store: crate::ArtifactStoreId::module(),
        artifact_ref: "module:v2:blake3:carried".to_string(),
    };
    // The commit appends both frames' opens: the head holds no frame yet, so
    // the first frame, whose edges the turn's cells acquired, ends here.
    let appended = [
        crate::NodeId::fixture(first.as_str().to_string()),
        crate::NodeId::fixture(successor.as_str().to_string()),
    ];
    assert_eq!(
        committed_frame_transition(
            &state,
            Some(first.clone()),
            SeedCarries::from_names(vec![carried.clone()]),
            &committing,
            &appended,
        )
        .unwrap(),
        Some(crate::store::FrameTransition {
            ended: crate::FrameEnvironmentId::new(state.session_id.clone(), first.clone()),
            successor: crate::FrameEnvironmentId::new(state.session_id.clone(), successor.clone()),
            carries: vec![carried.clone()],
            gate: committing.journal_identity().unwrap(),
        })
    );
    // A frame this commit does not append cannot be ended by it.
    assert_eq!(
        committed_frame_transition(
            &state,
            Some(first.clone()),
            SeedCarries::from_names(vec![carried]),
            &committing,
            &[]
        )
        .unwrap(),
        None
    );
}
