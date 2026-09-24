use super::*;
use crate::EffectHost as _;
use crate::SessionId;
use crate::runtime::tests::helpers::{FixedAttachmentRoots, RecordingStore};
use crate::session_model::{ConversationRecord, MessageRole, Part};
use crate::store::SessionExecutionLeaseStore;
use crate::store::TurnInputStore;
use crate::testing::conformance_support::TurnCancelPeekIdentity;
use crate::{
    AgentFrameReason, FrameKey, Message, OpenAgentFrameRequest, SessionGraph, TokenUsage,
    shared_parts,
};
use lash_sansio::core_support::MessageSequenceCoreSupport;
use lash_sansio::sync::MutexExt;
const UNBOUNDED: crate::TurnBudget = crate::TurnBudget::Unbounded;
fn cancelled_outcome() -> TurnOutcome {
    TurnOutcome::Stopped(crate::TurnStop::Cancelled {
        evidence: crate::TurnCancellationEvidence::internal("turn-boundary-test"),
    })
}
fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
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
    }
}
fn usage_entry(source: &str, model: &str, input_tokens: i64) -> crate::TokenLedgerEntry {
    crate::TokenLedgerEntry {
        source: source.to_string(),
        model: model.to_string(),
        usage: TokenUsage {
            input_tokens,
            output_tokens: 2,
            cache_read_input_tokens: 1,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        usage_disposition: Default::default(),
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
fn summarize_protocol_event(event: &crate::ProtocolEvent) -> String {
    let Some(value) = event
        .decode::<serde_json::Value>("test_protocol")
        .expect("test protocol event decodes")
    else {
        return format!("protocol:{}", event.plugin_id);
    };
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    format!("protocol:{kind}")
}
fn persisted_event_order(graph: &SessionGraph) -> Vec<String> {
    graph
        .nodes
        .iter()
        .filter_map(|node| match node.event()? {
            crate::SessionHistoryRecord::Conversation(record) => {
                Some(format!("message:{}", record.id))
            }
            crate::SessionHistoryRecord::Protocol(event) => Some(summarize_protocol_event(event)),
        })
        .collect()
}
fn chronological_event_order(graph: &SessionGraph) -> Vec<String> {
    let read_model = graph.read_model(None).unwrap();
    crate::chronological::ChronologicalProjection::from_read_model(&read_model)
        .entries()
        .iter()
        .map(|entry| match &entry.payload {
            crate::chronological::ChronologicalPayload::Message(message) => {
                format!("message:{}", message.id)
            }
            crate::chronological::ChronologicalPayload::ProtocolEvent(event) => {
                summarize_protocol_event(event)
            }
        })
        .collect()
}
fn stored_graph_with_head_leaf(store: &RecordingStore) -> SessionGraph {
    let graph = store.session_graph.lock_recover().clone();
    SessionGraph::from_shared_nodes(
        graph.nodes.clone(),
        store
            .session_head_meta
            .lock_recover()
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone()),
    )
    .expect("recorded head leaf resolves in the stored graph")
}
fn state_with_graph(graph: SessionGraph) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("session-1"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED))
    };
    state.ensure_agent_frame_initialized();
    if !graph.nodes.is_empty() {
        let frame_node_id = state.current_frame_node_id.clone().expect("initial frame");
        let mut nodes = state.session_graph.nodes.clone();
        nodes.extend(graph.nodes.iter().map(|node| {
            let mut node = node.as_ref().clone();
            if node.parent_node_id.is_none() {
                node.parent_node_id = Some(frame_node_id.to_string().into());
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
fn switch_request(
    operation_id: &str,
    frame_key: FrameKey,
    reason: AgentFrameReason,
    initial_nodes: Vec<crate::SessionAppendNode>,
) -> crate::SwitchAgentFrameRequest {
    crate::SwitchAgentFrameRequest::new(operation_id, frame_key, reason)
        .with_initial_nodes(initial_nodes)
}
fn seed_node(text: &str) -> crate::SessionAppendNode {
    crate::SessionAppendNode::message(crate::PluginMessage::text(MessageRole::Assistant, text))
}
fn frame_switch_commit_input<'a>(
    returned_state: &'a crate::SessionSnapshot,
    outcome: &'a TurnOutcome,
    store: &'a RecordingStore,
) -> FinalCommitInput<'a> {
    FinalCommitInput {
        returned_state,
        tool_calls: &[],
        omitted: None,
        plugins: None,
        execution_state_update: ExecutionStateUpdate::Clear,
        agent_frame_switch_materializes: true,
        store: Some(store),
        usage_deltas: &[],
        failure_evidence: &[],
        outcome,
        claim_settlement: TurnClaimSettlement::for_test(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        ),
        current_session_lease_fence: None,
        queued_run: None,
        enqueued_queue_batches: Vec::new(),
        interrupted_turn_input_turn_id: None,
        interrupted_turn_input_cancellation: None,
        interrupted_turn_cancel_intent: None,
        turn_cancel_closure_settlement: None,
        turn_control_resolver: None,
        recorded_attachment_intent_ids: Default::default(),
        session_execution_lease_completion: None,
    }
}

#[tokio::test]
async fn final_commit_retry_preserves_honoured_after_step_settlement() {
    let host = Arc::new(crate::NativeEffectHost::default());
    let store = Arc::new(
        RecordingStore::default().with_turn_cancellation_authority_for_testing(
            crate::TurnCancellationAuthority::new(host.turn_control_binding_id(), host.clone()),
        ),
    );
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
    state.session_id = SessionId::from("final-cancel-cas");
    state.ensure_agent_frame_initialized();
    let turn_id = crate::TurnId::from("final-cancel-cas:turn");
    let address = crate::TurnAddress::new(&state.session_id, &turn_id);
    let pending = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &state.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("after-step cancellation drops this input"),
        ))
        .await
        .expect("enqueue active-turn input");
    let base_request =
        crate::TurnCancelRequest::new(address.clone(), "final-cancel-cas:base", None)
            .mode(crate::TurnCancelMode::AfterStep)
            .undelivered(crate::TurnCancelDisposition::Drop);
    let driver =
        crate::TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
    driver
        .request_cancel(base_request.clone())
        .await
        .expect("accept after-step cancellation");
    let control =
        crate::runtime::turn_control::ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("create turn gate");
    let scoped = host
        .scoped(
            crate::AdmittedScope::unpinned(address.execution_scope().clone())
                .expect("a turn address admits unpinned"),
        )
        .expect("scope final-cancel CAS controller");
    let honoured = control
        .observe_pending_cancel(
            &scoped,
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 7,
            },
        )
        .await
        .expect("observe after-step gate")
        .expect("after-step cancellation wins");
    assert_eq!(honoured.honoured_after_step, Some(7));

    let (mut pipeline, lease) = leased_boundary(store.as_ref(), state).await;
    let observed = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot base intent");
    let binding_id = host.turn_control_binding_id();
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            &lease.fence(),
            &binding_id,
            &address.execution_scope(),
        )
        .await
        .expect("bind cancellation owner");
    let authorization = control
        .closure_authorization(
            &binding_id,
            address.execution_scope(),
            &lease.fence(),
            observed.clone(),
            false,
            Some(honoured.clone()),
        )
        .expect("materialize exact closure");
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("authorize exact closure");
    let settlement = control
        .settle_authorized(host.as_ref(), &authorization)
        .await
        .expect("settle exact closure");
    assert_eq!(
        settlement
            .effective_cancellation()
            .and_then(|evidence| evidence.honoured_after_step),
        Some(7)
    );
    assert_eq!(settlement.effective_cancellation(), Some(&honoured));
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read authorized closure before commit"),
        vec![authorization]
    );

    let later_request =
        crate::TurnCancelRequest::new(address.clone(), "final-cancel-cas:later", None)
            .undelivered(crate::TurnCancelDisposition::Drop);
    store.inject_turn_cancel_before_next_runtime_commit(later_request);
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::default(),
            None,
        )
        .await
        .expect("prepare stable final state");
    let returned_state = pipeline.export_state_for_assembly();
    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(store.as_ref()),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled {
                evidence: honoured.clone(),
            }),
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: Some(turn_id.clone()),
            interrupted_turn_input_cancellation: Some(honoured),
            interrupted_turn_cancel_intent: Some(observed),
            turn_cancel_closure_settlement: Some(settlement),
            turn_control_resolver: Some(host.as_ref()),
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: Some(lease.completion()),
        })
        .await
        .expect("refresh stale predicate without discarding execution enrichment");
    assert_eq!(store.commit_write_transaction_count(), 2);
    assert_eq!(*store.runtime_commit_count.lock_recover(), 1);
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read authenticated base request")
        .expect("authenticated base remains retained");
    assert_eq!(durable.request, base_request);
    let affected = durable
        .outcome
        .expect("one logical commit records the affected input")
        .affected_inputs;
    assert_eq!(affected.len(), 1);
    assert_eq!(affected[0].input_id, pending.input_id);
    assert_eq!(affected[0].disposition, crate::TurnCancelDisposition::Drop);
    assert!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read closure pins")
            .is_empty()
    );
}
async fn leased_boundary(
    store: &RecordingStore,
    state: RuntimeSessionState,
) -> (TurnBoundary, crate::SessionExecutionLease) {
    crate::SessionCommitStore::admit_and_bind_session(
        store,
        &crate::SessionBinding::root(state.session_id.clone()),
    )
    .await
    .expect("admit turn-boundary test session");
    let owner = lease_owner("turn-boundary-test");
    let lease = store
        .try_claim_session_execution_lease(
            &state.session_id,
            &owner,
            "leased-boundary-executor",
            60_000,
        )
        .await
        .expect("claim test session execution lease")
        .acquired()
        .expect("test session execution lease");
    (TurnBoundary::from_state(state), lease)
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
        .record_frame_switch(
            &state.session_id.clone(),
            state.current_frame_node_id.as_deref(),
            &switch_request(
                "session-1:turn:turn-outcome-frame-switch",
                frame_key.clone(),
                AgentFrameReason::continue_as(),
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
    let current_read = state
        .session_graph
        .read_model(Some(&expected_frame_node_id))
        .unwrap();
    assert_eq!(current_read.messages.len(), 1);
    assert_eq!(current_read.messages[0].parts[0].content(), "seed message");
    let previous_read = state
        .session_graph
        .read_model(previous_frame_node_id.as_ref())
        .unwrap();
    assert_eq!(previous_read.messages.len(), 1);
    assert_eq!(previous_read.messages[0].parts[0].content(), "old frame");
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
    let mut nodes = state.session_graph.nodes.clone();
    let previous = nodes
        .iter_mut()
        .find(|node| node.node_id == previous_frame_node_id_value)
        .expect("current frame node");
    let previous = std::sync::Arc::make_mut(previous);
    let crate::SessionNodePayload::FrameOpen {
        protocol_turn_options,
        ..
    } = &mut previous.payload
    else {
        panic!("current frame id must identify FrameOpen");
    };
    *protocol_turn_options =
        crate::ProtocolTurnOptions::from_payload(serde_json::json!({ "mode": "test" }));
    state.protocol_turn_options = protocol_turn_options.clone();
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
        current.protocol_turn_options.payload,
        serde_json::json!({ "mode": "test" })
    );

    let current_read = state
        .session_graph
        .read_model(Some(
            &crate::FrameNodeId::new(opened.frame_node_id.clone()).unwrap(),
        ))
        .unwrap();
    assert_eq!(current_read.messages.len(), 1);
    assert_eq!(
        current_read.messages[0].parts[0].content(),
        "Compaction summary:\nold work"
    );
    assert!(matches!(
        current_read.messages[0].origin.as_ref(),
        Some(crate::MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == "standard_compaction"
    ));

    let previous_read = state
        .session_graph
        .read_model(previous_frame_node_id.as_ref())
        .unwrap();
    assert_eq!(previous_read.messages.len(), 1);
    assert_eq!(
        previous_read.messages[0].parts[0].content(),
        "old durable frame"
    );

    let replay = super::super::open_agent_frame_in_state_with_clock(
        &mut state,
        frame_request(frame_key, AgentFrameReason::compaction())
            .with_initial_nodes(vec![seed_node]),
        &crate::SystemClock,
    )
    .expect("replay the current compaction frame");
    assert!(!replay.opened);
    let replay_read = state
        .session_graph
        .read_model(Some(
            &crate::FrameNodeId::new(replay.frame_node_id.clone()).unwrap(),
        ))
        .unwrap();
    assert_eq!(replay_read.messages.len(), 1);
}

#[test]
fn reopening_a_previous_frame_refuses_and_keeps_the_current_frame() {
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("frame-switch-back"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED))
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

/// A turn carries at most one agent-frame switch (FIG-3303). A plugin that
/// recorded a switch and a protocol outcome naming a *different* frame are two
/// authors of one switch: the commit is refused before any durable write
/// rather than opening both frames and leaving the run to die on the handoff
/// target check.
#[tokio::test]
async fn final_commit_refuses_a_second_frame_switch_author_naming_another_frame() {
    let store = RecordingStore::default();
    let state = state_with_graph(SessionGraph::from_active_read_state(&[text_message(
        "u0",
        MessageRole::User,
        "first frame",
    )]));
    let opening_frame_node_id = state.current_frame_node_id.clone();
    let expected_frames =
        serde_json::to_value(&state.agent_frames).expect("agent frame records serialize");

    let (mut pipeline, _lease) = leased_boundary(&store, state).await;
    let session_id = pipeline.state().session_id.clone();
    let current_frame_node_id = pipeline.state().current_frame_node_id.clone();
    let recorded = pipeline
        .graph_appends()
        .record_frame_switch(
            &session_id,
            current_frame_node_id.as_deref(),
            &switch_request(
                "standard-compaction:recovery",
                frame_key("frame-plugin"),
                AgentFrameReason::compaction(),
                vec![seed_node("compaction summary")],
            ),
        )
        .expect("a plugin records the turn's frame switch");
    assert!(recorded.opened);

    let outcome = TurnOutcome::AgentFrameSwitch {
        frame_key: frame_key("frame-outcome"),
        task: "continue as the outcome's frame".to_string(),
        initial_nodes: Vec::new(),
    };
    let returned_state = pipeline.export_state_for_assembly();
    let error = pipeline
        .final_commit_with_snapshots(frame_switch_commit_input(&returned_state, &outcome, &store))
        .await
        .expect_err("two authors naming different frames must refuse the commit");

    let runtime_error = crate::runtime::runtime_error_from_store_commit(error);
    assert_eq!(
        runtime_error.code,
        crate::RuntimeErrorCode::AgentFrameSwitchAuthorConflict
    );
    assert!(runtime_error.code.is_terminal());
    assert!(
        runtime_error
            .message
            .contains("one turn materializes at most one switch"),
        "refusal names the rule: {}",
        runtime_error.message
    );
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

    let state = pipeline.into_final_state();
    assert_eq!(state.current_frame_node_id, opening_frame_node_id);
    assert_eq!(
        serde_json::to_value(&state.agent_frames).expect("agent frame records serialize"),
        expected_frames
    );
}

/// Same frame key, different seed nodes: the plugin was already answered with
/// draft ids for its seeds, so the commit refuses rather than silently
/// dropping one author's seeds past a frame-node-id-only guard.
#[tokio::test]
async fn final_commit_refuses_a_second_frame_switch_author_with_other_seed_nodes() {
    let store = RecordingStore::default();
    let state = state_with_graph(SessionGraph::from_active_read_state(&[text_message(
        "u0",
        MessageRole::User,
        "first frame",
    )]));
    let opening_frame_node_id = state.current_frame_node_id.clone();

    let (mut pipeline, _lease) = leased_boundary(&store, state).await;
    let session_id = pipeline.state().session_id.clone();
    let current_frame_node_id = pipeline.state().current_frame_node_id.clone();
    let recorded = pipeline
        .graph_appends()
        .record_frame_switch(
            &session_id,
            current_frame_node_id.as_deref(),
            &switch_request(
                "standard-compaction:recovery",
                frame_key("frame-next"),
                AgentFrameReason::compaction(),
                vec![seed_node("compaction summary")],
            ),
        )
        .expect("a plugin records the turn's frame switch");
    assert_eq!(recorded.initial_node_ids.len(), 1);

    let outcome = TurnOutcome::AgentFrameSwitch {
        frame_key: frame_key("frame-next"),
        task: "continue as the same frame".to_string(),
        initial_nodes: Vec::new(),
    };
    let returned_state = pipeline.export_state_for_assembly();
    let error = pipeline
        .final_commit_with_snapshots(frame_switch_commit_input(&returned_state, &outcome, &store))
        .await
        .expect_err("a second author with other seed nodes must refuse the commit");

    let runtime_error = crate::runtime::runtime_error_from_store_commit(error);
    assert_eq!(
        runtime_error.code,
        crate::RuntimeErrorCode::AgentFrameSwitchAuthorConflict
    );
    assert!(
        runtime_error
            .message
            .contains("must name the same seed nodes"),
        "refusal names the rule: {}",
        runtime_error.message
    );
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
    assert_eq!(
        pipeline.into_final_state().current_frame_node_id,
        opening_frame_node_id
    );
}

/// One switch, two authors that agree: the frame opens once, carrying the
/// seed nodes and the reason of the author the slot already answered.
#[tokio::test]
async fn final_commit_opens_one_frame_for_two_agreeing_switch_authors() {
    let store = RecordingStore::default();
    let state = state_with_graph(SessionGraph::from_active_read_state(&[text_message(
        "u0",
        MessageRole::User,
        "first frame",
    )]));
    let opening_frame_node_id = state.current_frame_node_id.clone();

    let (mut pipeline, _lease) = leased_boundary(&store, state).await;
    let session_id = pipeline.state().session_id.clone();
    let current_frame_node_id = pipeline.state().current_frame_node_id.clone();
    let seeds = vec![seed_node("compaction summary")];
    pipeline
        .graph_appends()
        .record_frame_switch(
            &session_id,
            current_frame_node_id.as_deref(),
            &switch_request(
                "standard-compaction:recovery",
                frame_key("frame-next"),
                AgentFrameReason::compaction(),
                seeds.clone(),
            ),
        )
        .expect("a plugin records the turn's frame switch");

    let outcome = TurnOutcome::AgentFrameSwitch {
        frame_key: frame_key("frame-next"),
        task: "continue as the same frame".to_string(),
        initial_nodes: seeds,
    };
    let returned_state = pipeline.export_state_for_assembly();
    pipeline
        .final_commit_with_snapshots(frame_switch_commit_input(&returned_state, &outcome, &store))
        .await
        .expect("two authors of one switch commit once");
    assert_eq!(*store.runtime_commit_count.lock_recover(), 1);

    let state = pipeline.into_final_state();
    let expected_frame_node_id =
        crate::session_graph::frame_node_id(&session_id, frame_key("frame-next").as_str());
    assert_eq!(
        state.current_frame_node_id.as_deref(),
        Some(expected_frame_node_id.as_str())
    );
    // Exactly one frame was opened by this commit, and it is the recorded
    // author's: the outcome's `continue_as` never overwrote the reason the
    // plugin was answered with.
    assert_eq!(state.agent_frames.len(), 2);
    let current = state.current_agent_frame().expect("current frame");
    assert_eq!(current.reason.as_str(), crate::AgentFrameReason::COMPACTION);
    assert_eq!(
        current.previous_frame_node_id.as_deref(),
        opening_frame_node_id.as_deref()
    );
    // The plugin's seed survives the commit exactly once.
    let current_read = state
        .session_graph
        .read_model(Some(&expected_frame_node_id))
        .expect("read the frame this commit opened");
    assert_eq!(current_read.messages.len(), 1);
    assert_eq!(
        current_read.messages[0].parts[0].content(),
        "compaction summary"
    );
}

/// A turn outcome naming a persisted, non-current frame aborts the commit
/// with the typed refusal, leaves resident state untouched, and writes
/// nothing durable; the next turn on the same state commits normally.
#[tokio::test]
async fn final_commit_refuses_a_historical_frame_switch_outcome_before_any_durable_write() {
    let clock = crate::SystemClock;
    let store = RecordingStore::default();
    let mut state = state_with_graph(SessionGraph::default());
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
    state.protocol_turn_options = crate::ProtocolTurnOptions {
        payload: serde_json::json!({ "mode": "frame-b" }),
    };
    let expected_policy = state.policy.clone();
    let expected_protocol_turn_options = state.protocol_turn_options.clone();
    let expected_leaf = state.session_graph.leaf_node_id.clone();
    let expected_head_revision = state.head_revision;
    let expected_frames =
        serde_json::to_value(&state.agent_frames).expect("agent frame records serialize");

    let (mut pipeline, _lease) = leased_boundary(&store, state).await;
    // The refused turn drafts no conversation of its own, so every resident
    // fact below is attributable to the switch alone.
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::from_base(Vec::new().into()),
            None,
        )
        .await
        .expect("prepare checkpoint in memory");
    let outcome = TurnOutcome::AgentFrameSwitch {
        frame_key: frame_key("frame-a"),
        task: "return to frame a".to_string(),
        initial_nodes: vec![crate::SessionAppendNode::message(
            crate::PluginMessage::text(MessageRole::Assistant, "new frame-a seed"),
        )],
    };
    let returned_state = pipeline.export_state_for_assembly();
    let error = pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clear,
            agent_frame_switch_materializes: true,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &outcome,
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect_err("a historical frame switch outcome must refuse the commit");

    let runtime_error = crate::runtime::runtime_error_from_store_commit(error);
    assert_eq!(
        runtime_error.code,
        crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert!(runtime_error.code.is_terminal());
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

    let state = pipeline.into_final_state();
    assert_eq!(
        state.current_frame_node_id.as_deref(),
        Some(frame_b.frame_node_id.as_str())
    );
    assert_eq!(state.policy, expected_policy);
    assert_eq!(state.protocol_turn_options, expected_protocol_turn_options);
    assert_eq!(state.session_graph.leaf_node_id, expected_leaf);
    assert_eq!(state.head_revision, expected_head_revision);
    assert_eq!(
        serde_json::to_value(&state.agent_frames).expect("agent frame records serialize"),
        expected_frames
    );
    assert_eq!(
        state
            .session_graph
            .nearest_frame_node_id(state.session_graph.leaf_node_id.as_deref())
            .map(crate::NodeId::as_str),
        Some(frame_b.frame_node_id.as_str())
    );

    let mut next_turn = TurnBoundary::from_state(state);
    let user = text_message("u0", MessageRole::User, "hello");
    next_turn
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::from_base(vec![user].into()),
            None,
        )
        .await
        .expect("prepare the next turn's checkpoint in memory");
    let returned_state = next_turn.export_state_for_assembly();
    next_turn
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect("the next turn commits normally after the refused switch");
    assert_eq!(*store.runtime_commit_count.lock_recover(), 1);
    let stored_graph = stored_graph_with_head_leaf(&store);
    assert_eq!(
        stored_graph
            .nearest_frame_node_id(stored_graph.leaf_node_id.as_deref())
            .map(crate::NodeId::as_str),
        Some(frame_b.frame_node_id.as_str())
    );
}

#[tokio::test]
async fn progress_boundaries_accumulate_protocol_events_in_the_draft() {
    let user = text_message("u0", MessageRole::User, "hello");
    let assistant = text_message("a0", MessageRole::Assistant, "hi");
    let mut pipeline = TurnBoundary::from_state(state_with_graph(SessionGraph::default()));
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::from_base(vec![user.clone()].into()),
            None,
        )
        .await
        .expect("prepare checkpoint in memory");
    let protocol_event =
        crate::ProtocolEvent::typed("test_protocol", serde_json::json!({"step": "started"}))
            .expect("protocol event serializes");
    let event_delta = vec![crate::SessionHistoryRecord::Protocol(protocol_event)];

    let boundary = pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED),
            turn_index: 1,
            messages: MessageSequence::from_base(vec![user, assistant].into()),
            event_delta,
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("progress boundary");

    assert_eq!(boundary.protocol_events.len(), 1);
    assert_eq!(pipeline.state().turn_index, 1);
    assert_eq!(pipeline.state().head_revision, 0);
}

#[tokio::test]
async fn final_commit_persists_the_complete_turn_tail_once() {
    let user = text_message("u0", MessageRole::User, "hello");
    let assistant = text_message("a0", MessageRole::Assistant, "hi");
    let trajectory = test_protocol_event("trajectory");
    let store = RecordingStore::default();
    let (mut pipeline, _lease) =
        leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::from_base(vec![user.clone()].into()),
            None,
        )
        .await
        .expect("prepare checkpoint in memory");
    pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED),
            turn_index: 1,
            messages: MessageSequence::from_base(vec![user, assistant.clone()].into()),
            event_delta: vec![
                crate::SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                    assistant,
                )),
                crate::SessionHistoryRecord::Protocol(trajectory),
            ],
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("progress boundary");
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

    let returned_state = pipeline.export_state_for_assembly();
    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect("final commit");

    assert_eq!(*store.runtime_commit_count.lock_recover(), 1);
    let stored_graph = stored_graph_with_head_leaf(&store);
    let expected = vec!["message:u0", "message:a0", "protocol:trajectory"];
    assert_eq!(persisted_event_order(&stored_graph), expected);
    assert_eq!(chronological_event_order(&stored_graph), expected);
}

#[tokio::test]
async fn a_skipped_boundary_keeps_queued_appends_for_the_next_one() {
    let user = text_message("u0", MessageRole::User, "hello");
    let assistant = text_message("a0", MessageRole::Assistant, "hi");
    // An assistant tool call without its result is not prompt-resume-safe,
    // so the boundary carrying it is skipped.
    let pending_tool_call = Message {
        id: "a-tool".to_string(),
        role: MessageRole::Assistant,
        parts: shared_parts(vec![Part::tool_call(
            "a-tool.p0".to_string(),
            "{}".to_string(),
            "call-1".to_string(),
            "probe".to_string(),
            None,
        )]),
        origin: None,
    };
    let store = RecordingStore::default();
    let (mut pipeline, _lease) =
        leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
    let session_id = pipeline.state().session_id.clone();
    pipeline
        .prepared_checkpoint(
            SessionPolicy::new(UNBOUNDED),
            0,
            &MessageSequence::from_base(vec![user.clone()].into()),
            None,
        )
        .await
        .expect("prepare checkpoint in memory");
    let queued = match pipeline
        .graph_appends()
        .record(
            &session_id,
            &crate::AppendSessionNodesRequest {
                operation_id: "queued-append".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "test.queued",
                    serde_json::json!({"probe": true}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .expect("record in-turn append")
    {
        crate::AppendSessionNodesOutcome::Appended { node_ids, .. } => node_ids,
        other => panic!("expected an appended outcome, got {other:?}"),
    };

    let skipped = pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED),
            turn_index: 1,
            messages: MessageSequence::from_base(vec![user.clone(), pending_tool_call].into()),
            event_delta: Vec::new(),
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("skipped boundary");
    assert!(skipped.protocol_events.is_empty());
    assert_eq!(pipeline.state().turn_index, 0, "the boundary was skipped");

    pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED),
            turn_index: 2,
            messages: MessageSequence::from_base(vec![user, assistant].into()),
            event_delta: Vec::new(),
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: None,
        })
        .await
        .expect("resume-safe boundary");
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

    let returned_state = pipeline.export_state_for_assembly();
    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect("final commit");

    assert_eq!(*store.runtime_commit_count.lock_recover(), 1);
    let stored_graph = stored_graph_with_head_leaf(&store);
    let path = stored_graph.active_path_nodes();
    let plugin_nodes = path
        .iter()
        .filter(|node| matches!(node.payload, crate::SessionNodePayload::Plugin { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        plugin_nodes.len(),
        1,
        "the skipped boundary must not drop the append"
    );
    assert!(
        !path.iter().any(|node| node.node_id == queued[0]),
        "draft ids never persist"
    );
    let a0_index = path
        .iter()
        .position(|node| node.message().is_some_and(|message| message.id == "a0"))
        .expect("a0 is durable");
    assert_eq!(
        plugin_nodes[0].node_id,
        path[a0_index + 1].node_id,
        "the append lands at the next applied boundary, after the messages it carried"
    );
    assert_eq!(
        stored_graph.leaf_node_id.as_deref(),
        Some(plugin_nodes[0].node_id.as_str())
    );
}

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
    let store = RecordingStore::default();
    let (mut pipeline, _lease) =
        leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
    pipeline
        .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy: SessionPolicy::new(UNBOUNDED),
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
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
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
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
    assert!(store.raw_graph_nodes_for_testing().is_empty());
}
#[tokio::test]
async fn replayed_exec_tool_output_is_a_gc_root_without_pending_or_message_refs() {
    let backend = crate::InMemoryAttachmentStore::new();
    let attachment = crate::AttachmentStore::put(
        &backend,
        vec![1, 2, 3],
        crate::AttachmentCreateMeta::new(
            crate::MediaType::parse("image/png").unwrap(),
            Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("replayed-only".to_string()),
        ),
    )
    .await
    .expect("put attachment bytes");
    let tool_calls = vec![crate::ToolCallRecord {
        call_id: Some("replayed-exec-call".to_string()),
        tool: "executor_state_only".to_string(),
        args: serde_json::json!({}),
        output: crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
            crate::AttachmentSource::stored(attachment.clone()),
        )),
        duration_ms: 1,
    }];
    let state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
    let committed = committed_attachment_ids(&state, &tool_calls, None);
    assert_eq!(committed, vec![attachment.id.clone()]);

    let roots = FixedAttachmentRoots(committed.into_iter().collect());
    let report = crate::reclaim_unreferenced_attachments(
        &roots,
        &backend,
        crate::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: crate::EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("grace-period GC");

    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(
        crate::AttachmentStore::get(&backend, &attachment.id)
            .await
            .expect("replayed exec attachment survives GC")
            .bytes,
        vec![1, 2, 3]
    );
}

#[tokio::test]
async fn final_commit_merges_usage_and_updates_persisted_graph_count() {
    let graph =
        SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "hello")]);
    let usage_entries = vec![
        usage_entry("child", "gpt", 5),
        usage_entry("turn", "gpt", 17),
    ];
    let store = RecordingStore::default();
    let (mut pipeline, _lease) = leased_boundary(&store, state_with_graph(graph.clone())).await;
    let usage =
        crate::store::RuntimeUsageDelta::for_operation(&pipeline.final_operation(), &usage_entries)
            .expect("stage test usage");
    let returned_state = pipeline.export_state_for_assembly();

    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Replace(
                crate::plugin::ExecutionStateSnapshot::from_root(Some(b"runtime".to_vec().into())),
            ),
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &usage,
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            tool_calls: &[],
            omitted: None,
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect("commit");

    assert_eq!(store.usage_deltas.lock_recover().len(), 2);
    assert_eq!(pipeline.state_mut().token_ledger.len(), 2);
    assert!(pipeline.state_mut().execution_state_snapshot().is_none());
    assert!(pipeline.state_mut().head_revision > 0);
}

/// FIG-3552: a peer's reclaim supersedes a queued-work claim the recovered
/// turn restored from its predecessor's generation. The recovered commit cedes
/// and writes nothing; it never drops the row and commits the rest of the turn.
#[tokio::test]
async fn recovered_final_commit_cedes_when_a_peer_supersedes_its_restored_queue_row() {
    let store = RecordingStore::default();
    let graph = SessionGraph::from_active_read_state(&[text_message(
        "u0",
        MessageRole::User,
        "recovered content",
    )]);
    let state = state_with_graph(graph);
    let (mut pipeline, predecessor_lease) = leased_boundary(&store, state).await;
    let batch = crate::QueuedWorkStore::enqueue_queued_work(
        &store,
        crate::QueuedWorkBatchDraft::new(
            "session-1",
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::TurnWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(&SessionId::from("session-1"), "fig905-frame"),
                "peer-owned row",
                None,
            ),
        ),
    )
    .await
    .expect("enqueue FIG-905 row");
    let predecessor_claim = crate::QueuedWorkStore::claim_ready_queued_work(
        &store,
        &SessionId::from("session-1"),
        &predecessor_lease.fence(),
        &predecessor_lease.owner,
        crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
        crate::testing::queued_work_claim_policy(64),
    )
    .await
    .expect("claim predecessor row")
    .claim()
    .expect("predecessor claim exists");
    assert_eq!(predecessor_claim.batches[0].batch_id, batch.batch_id);
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .expect("release crashed predecessor lease");

    let peer_owner = lease_owner("fig905-peer");
    let peer_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from("session-1"),
            &peer_owner,
            "recovered-final-commit-cedes-to-a-peer-superseded-queue-row-executor",
            60_000,
        )
        .await
        .expect("claim peer lease")
        .acquired()
        .expect("peer lease acquired");
    let peer_claim = crate::QueuedWorkStore::claim_ready_queued_work(
        &store,
        &SessionId::from("session-1"),
        &peer_lease.fence(),
        &peer_owner,
        crate::QueuedWorkClaimBoundary::Idle,
        crate::testing::queued_work_claim_policy(64),
    )
    .await
    .expect("peer reclaims row")
    .claim()
    .expect("peer claim exists");
    store
        .release_session_execution_lease(&peer_lease.completion())
        .await
        .expect("release peer lease without settling its row");

    let recovery_owner = lease_owner("fig905-recovery");
    let recovery_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from("session-1"),
            &recovery_owner,
            "recovered-final-commit-cedes-to-a-peer-superseded-queue-row-executor-2",
            60_000,
        )
        .await
        .expect("claim recovery lease")
        .acquired()
        .expect("recovery lease acquired");
    let returned_state = pipeline.export_state_for_assembly();
    let head_revision = pipeline.state_mut().head_revision;
    let ceded = pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            tool_calls: &[],
            omitted: None,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            claim_settlement: TurnClaimSettlement::for_test(
                vec![predecessor_claim.completion()],
                Vec::new(),
                vec![predecessor_claim.completion()],
                Vec::new(),
                std::iter::once((
                    predecessor_claim.claim_id.clone(),
                    predecessor_claim.session_lease_generation,
                ))
                .collect(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: Some(recovery_lease.fence()),
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: Some(recovery_lease.completion()),
        })
        .await
        .expect_err("a recovered commit whose restored row a peer reclaimed must cede");
    let StoreError::TurnOutcomeMaterializationRefused { error } = &ceded else {
        panic!("the recovered commit cedes with a typed refusal: {ceded:?}");
    };
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::AcceptedTurnInputCeded,
        "{ceded:?}"
    );
    assert_eq!(
        pipeline.state_mut().head_revision,
        head_revision,
        "the ceded commit publishes nothing"
    );

    let queued = crate::QueuedWorkStore::list_queued_work(&store, &SessionId::from("session-1"))
        .await
        .expect("list peer-owned row");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, peer_claim.batches[0].batch_id);
    let raw = store.raw_queued_work_for_testing();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].5, Some(peer_claim.session_lease_generation));
}

#[tokio::test]
async fn final_commit_rejects_claim_derived_content_without_settlement() {
    let graph = SessionGraph::from_active_read_state(&[text_message(
        "claimed-input",
        MessageRole::User,
        "claimed content",
    )]);
    let queue_origin = crate::QueuedWorkCompletion {
        session_id: SessionId::from("session-1"),
        claim_id: "queue-claim".to_string(),
        lease_token: "queue-token".to_string(),
        data: crate::QueuedWorkCompletionData {
            batch_ids: vec!["queue-batch".into()],
        },
    };
    let turn_input_origin = crate::TurnInputCompletion {
        session_id: SessionId::from("session-1"),
        claim: Some(crate::TurnInputSettlementClaim {
            claim_id: "turn-input-claim".to_string(),
            lease_token: "turn-input-token".to_string(),
        }),
        data: crate::TurnInputCompletionData {
            input_ids: vec!["turn-input".into()],
            applications: Vec::new(),
        },
    };
    let store = RecordingStore::default();
    let (mut queue_pipeline, queue_lease) =
        leased_boundary(&store, state_with_graph(graph.clone())).await;
    let queue_state = queue_pipeline.export_state_for_assembly();
    let queue_err = queue_pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &queue_state,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            tool_calls: &[],
            omitted: None,
            claim_settlement: TurnClaimSettlement::for_test(
                vec![queue_origin],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect_err("queue-derived content requires claim settlement");
    assert!(matches!(
        queue_err,
        StoreError::UnsettledQueuedWorkClaim { ref claim_id, .. }
            if claim_id == "queue-claim"
    ));
    store
        .release_session_execution_lease(&queue_lease.completion())
        .await
        .expect("release queue-case execution lease");

    let (mut input_pipeline, _lease) = leased_boundary(&store, state_with_graph(graph)).await;
    let input_state = input_pipeline.export_state_for_assembly();
    let input_err = input_pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &input_state,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: Some(&store),
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            tool_calls: &[],
            omitted: None,
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                vec![turn_input_origin],
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect_err("turn-input-derived content requires claim settlement");
    assert!(matches!(
        input_err,
        StoreError::UnsettledTurnInputClaim { ref claim_id, .. }
            if claim_id == "turn-input-claim"
    ));
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        0,
        "invalid commits must be rejected before reaching persistence"
    );
}

#[tokio::test]
async fn no_store_final_commit_discards_snapshots_without_touching_graph_or_usage() {
    let graph =
        SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "hello")]);
    let usage = vec![usage_entry("turn", "model", 5)];
    let mut state = state_with_graph(graph.clone());
    state.token_ledger = usage.clone();
    state.set_tool_state_snapshot(Some(crate::ToolState::default()));
    state.set_plugin_state(Some(crate::PluginState::default()));
    state.set_execution_state_snapshot(Some(b"runtime".to_vec().into()));
    let mut pipeline = TurnBoundary::from_state(state);
    let returned_state = pipeline.export_state_for_assembly();

    pipeline
        .final_commit_with_snapshots(FinalCommitInput {
            returned_state: &returned_state,
            plugins: None,
            execution_state_update: ExecutionStateUpdate::Clean,
            agent_frame_switch_materializes: false,
            store: None,
            usage_deltas: &[],
            failure_evidence: &[],
            outcome: &cancelled_outcome(),
            tool_calls: &[],
            omitted: None,
            claim_settlement: TurnClaimSettlement::for_test(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            ),
            current_session_lease_fence: None,
            queued_run: None,
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            turn_control_resolver: None,
            recorded_attachment_intent_ids: Default::default(),
            session_execution_lease_completion: None,
        })
        .await
        .expect("no-store commit");

    let state = pipeline.state_mut();
    assert_eq!(state.session_graph.nodes.len(), graph.nodes.len() + 1);
    assert_eq!(state.token_ledger.len(), usage.len());
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

#[test]
fn state_after_export_is_the_real_committed_state() {
    let mut boundary = TurnBoundary::from_state(state_with_graph(SessionGraph::default()));
    boundary.state_mut().turn_index = 7;

    let snapshot = boundary.export_state_for_assembly();
    let state = boundary.state();

    // Finalization must hand out the turn's real state. A fabricated
    // `RuntimeSessionState::new` placeholder — the old mem::replace
    // throwaway — would carry a fresh session id and turn_index 0.
    assert_eq!(state.session_id, SessionId::from("session-1"));
    assert_eq!(state.turn_index, 7);
    assert_eq!(snapshot.session_id, state.session_id);
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
            call_id.to_string(),
            "shot".to_string(),
            None,
        )]),
        origin: None,
    };
    let result = |message_id: &str, call_id: &str, content| Message {
        id: message_id.to_string(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::tool_result(
            format!("{message_id}.p0"),
            content,
            call_id.to_string(),
            "shot".to_string(),
        )]),
        origin: None,
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
            SessionPolicy::new(UNBOUNDED),
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
            policy: SessionPolicy::new(UNBOUNDED),
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
