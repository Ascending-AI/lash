//! Tests for resident session state: the snapshot projection, keyed
//! checkpoint components, their resident bodies, and the resident
//! execution graph.

use super::*;

/// `key`'s binding as a registry mints it: its own wire model and a 32k
/// window.
fn recorded_llm_profile(key: &str) -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new(key),
        lash_core_llm::llm_profile::LlmProfileMetadata::builder(format!("{key}-wire"))
            .cache_retention(lash_core_llm::provider::CacheRetention::Short)
            .context_window_tokens(32_000)
            .build()
            .expect("model"),
    ))
}

/// A SessionExecution graph whose root holds `value`, a text past the
/// inline bound: a leaf beneath the root, as a large binding is.
fn execution(
    value: &str,
) -> (
    crate::checkpoint_graph::NodeId,
    crate::plugin::ExecutionGraphCapture,
) {
    let capture =
        crate::testing::store_fixtures::execution_graph(&format!("{value}{}", "x".repeat(1024)));
    let crate::plugin::ExecutionGraphCapture::Replace(graph) = &capture else {
        unreachable!("the fixture stages a root")
    };
    (graph.root, capture)
}

fn ambient() -> RuntimeSessionState {
    RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ))
}

fn commit_result_for(state: &RuntimeSessionState) -> crate::store::RuntimeCommitReceipt {
    let commit = crate::RuntimeCommit::persisted_state_for_test(state);
    crate::store::RuntimeCommitReceipt {
        committed_at_ms: 0,
        schema_version: crate::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
        head_revision: state.head_revision + 1,
        checkpoint_ref: "checkpoint-ref".to_string().into(),
        manifest: commit
            .checkpoint
            .manifest(crate::store::FleetFormat::current())
            .expect("project the committed manifest"),
        committed_leaf_node_id: None,
        realized_node_timestamps: Vec::new(),
        failure_evidence: Vec::new(),
        outcome: None,
        trace: None,
        command_outcomes: Default::default(),
        turn_input_applications: Vec::new(),
        turn_cancel_input_outcome: Default::default(),
        work_remaining: true,
        receipt_replayed: false,
    }
}

#[test]
fn commit_result_mismatch_remains_sticky_until_execution_state_staging() {
    let mut resident = ambient();
    let (_, staged) = execution("generation-a");
    resident.set_execution_graph(staged);

    let mut tampered = commit_result_for(&resident);
    let (other, _) = execution("another build's root");
    tampered.manifest.execution_ref = Some(crate::store::BlobRef(other.to_hex()));
    resident.apply_persisted_commit_result(tampered).unwrap();

    let hydration_before_discard = resident.execution_state_hydration();
    assert!(
        matches!(
            hydration_before_discard,
            Err(crate::StoreError::StoredDataCorrupt { .. })
        ),
        "a mismatched commit result must refuse hydration; got {hydration_before_discard:?}"
    );
    resident.discard_runtime_snapshots();
    let hydration_after_discard = resident.execution_state_hydration();
    assert!(
        matches!(
            hydration_after_discard,
            Err(crate::StoreError::StoredDataCorrupt { .. })
        ),
        "discard_runtime_snapshots must not launder a mismatched commit result; got {hydration_after_discard:?}"
    );

    let (recovered_root, recovered) = execution("recovered");
    let crate::plugin::ExecutionGraphCapture::Replace(graph) = recovered.clone() else {
        unreachable!("the fixture stages a root")
    };
    resident.set_execution_graph(recovered);
    let hydrated = resident
        .execution_state_hydration()
        .expect("fresh execution-state staging recovers hydration")
        .expect("fresh execution-state staging restores a run");
    assert_eq!(hydrated.root, recovered_root);
    assert_eq!(hydrated, graph);
}

/// A storeless commit keeps the accepted execution graph resident: the
/// same-frame restore that follows rebuilds from it, the next commit
/// replaces it, and a frame clear drops it (FIG-2521).
#[test]
fn storeless_body_release_keeps_the_accepted_execution_for_restore() {
    let mut state = ambient();
    let (first_root, first) = execution("first");
    state.set_execution_graph(first);
    state.set_plugin_state(Some(crate::PluginState::default()));
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert!(
        state.plugin_state().is_none(),
        "plugin snapshots are released like every other committed body"
    );
    let accepted = state
        .execution_state_hydration()
        .expect("the accepted execution hydrates")
        .expect("the accepted execution stays resident");
    assert_eq!(accepted.root, first_root);

    // A restore releases again; the accepted execution stays.
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state.execution_state_hydration().expect("still resident"),
        Some(accepted)
    );

    // The next commit supersedes it.
    let (second_root, second) = execution("second");
    state.set_execution_graph(second);
    state.discard_runtime_snapshots_retaining_accepted_execution();
    let superseded = state
        .execution_state_hydration()
        .expect("resident")
        .expect("the next commit's execution is resident");
    assert_eq!(superseded.root, second_root);

    // A frame switch clears the execution: nothing is kept and nothing is
    // refused, because the session no longer holds a run at all.
    state.set_execution_graph(crate::plugin::ExecutionGraphCapture::Clear);
    state.discard_runtime_snapshots_retaining_accepted_execution();
    assert_eq!(
        state
            .execution_state_hydration()
            .expect("no run is not corrupt"),
        None
    );
}

#[test]
fn incomplete_checkpoint_component_projection_is_a_typed_error() {
    let projected = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ))
    .to_snapshot();
    let state = RuntimeSessionState::from_snapshot(
        projected,
        crate::session_state::RuntimeSessionAuthority::fixture(),
    )
    .unwrap();

    let error = state
        .checkpoint_components
        .build_checkpoint(
            crate::PersistedTurnState::default(),
            crate::store::FleetFormat::current(),
        )
        .expect_err("snapshot projection cannot prove the complete keyed set");

    assert!(matches!(
        error,
        crate::StoreError::IncompleteCheckpointComponentSet
    ));
}

#[test]
#[should_panic(expected = "adopted head revision must advance")]
fn persisted_commit_cannot_adopt_nonadvancing_revision() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    let mut receipt = commit_result_for(&state);
    receipt.head_revision = state.head_revision;
    state.apply_persisted_commit_result(receipt).unwrap();
}

fn fresh_state_with_initial_frame() -> RuntimeSessionState {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    state.ensure_agent_frame_initialized().unwrap();
    state
}

fn initial_frame_protocol_turn_options(state: &RuntimeSessionState) -> crate::ProtocolTurnOptions {
    state
        .current_agent_frame()
        .expect("the initial frame is open")
        .protocol_turn_options()
}

/// The initial frame's committed payload, without its wall-clock timestamp.
fn initial_frame_payload(state: &RuntimeSessionState) -> (crate::NodeId, serde_json::Value) {
    let [node] = state.session_graph.nodes.as_slice() else {
        panic!("the state holds only its initial frame");
    };
    (
        node.node_id.clone(),
        serde_json::to_value(&node.payload).expect("encode the frame payload"),
    )
}

/// A plugin configuration whose protocol namespace is `payload`.
fn protocol_config(payload: serde_json::Value) -> crate::PluginConfig {
    let mut config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    config.insert("protocol", payload).unwrap();
    config
}

/// A fresh session's eagerly opened initial frame is re-stamped under the
/// configuration the state installs before it persists, and a reopen of its
/// durable head opens it under that configuration: both commit the same
/// frame (FIG-3684, FIG-4379).
#[test]
fn an_unpersisted_initial_frame_opens_under_the_installed_plugin_config() {
    let mut fresh = fresh_state_with_initial_frame();
    assert_eq!(
        initial_frame_protocol_turn_options(&fresh),
        crate::ProtocolTurnOptions::default()
    );
    let settled = protocol_config(serde_json::json!({ "channel": "cell" }));
    fresh.authority.plugin_config = settled.clone();
    fresh
        .open_unpersisted_initial_frame_under_current_assignment()
        .unwrap();
    assert_eq!(
        initial_frame_protocol_turn_options(&fresh),
        settled.protocol_turn_options()
    );

    let mut reopened = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    reopened.authority.plugin_config = settled;
    reopened.ensure_agent_frame_initialized().unwrap();
    assert_eq!(
        initial_frame_payload(&fresh),
        initial_frame_payload(&reopened),
        "a fresh session and a reopen of its head commit the same initial frame"
    );
}

/// A persisted frame is a historical snapshot: a later configuration never
/// rewrites it.
#[test]
fn a_persisted_initial_frame_keeps_the_config_it_opened_under() {
    let mut state = fresh_state_with_initial_frame();
    let opened_under = initial_frame_protocol_turn_options(&state);
    let persisted = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(persisted);
    state.authority.plugin_config = protocol_config(serde_json::json!({ "channel": "cell" }));
    state
        .open_unpersisted_initial_frame_under_current_assignment()
        .unwrap();
    assert_eq!(initial_frame_protocol_turn_options(&state), opened_under);
}

/// Install a run view that runs under `config`, resolved against the
/// state's sticky config.
fn install_view(state: &mut RuntimeSessionState, config: &crate::PersistedSessionConfig) {
    let run = crate::run_spec::ResolvedRun::snapshot(
        crate::store::persisted_session_config_from_state(state),
    )
    .with_config(config.clone());
    state.install_run_view(&run).unwrap();
}

/// FIG-4646: uninstalling a run view restores the sticky config under it,
/// every fact of it, so nothing read from the state afterwards is the
/// run's. A second view installed over the first keeps the session's
/// sticky config, not the first view's.
#[test]
fn taking_a_run_view_restores_the_sticky_config() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    state.session_id = SessionId::from("take-run-view-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    state.config_revision = 3;
    let sticky = crate::store::persisted_session_config_from_state(&state);
    assert!(state.take_run_view().is_none(), "no view is installed");

    let mut first = sticky.clone();
    first.model = Some(recorded_llm_profile("first-run-route"));
    first.generation.seed = Some(7);
    install_view(&mut state, &first);
    let mut second = sticky.clone();
    second.model = Some(recorded_llm_profile("second-run-route"));
    install_view(&mut state, &second);
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        second
    );
    assert_eq!(
        state.authority.run_view().map(|view| &view.sticky),
        Some(&sticky),
        "a view installed over another keeps the session's sticky config"
    );

    let taken = state.take_run_view().expect("the installed view");
    assert_eq!(taken.sticky, sticky);
    assert_eq!(taken.run.config(), &second);
    assert!(state.authority.run_view().is_none());
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        sticky,
        "the resident config is the sticky one again"
    );
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
}

/// FIG-4529: an observer's view of a state under a recorded run view reads
/// the sticky config, while the state's own policy stays the run's.
#[test]
fn recorded_session_view_reads_the_sticky_config_under_a_run_view() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    state.session_id = SessionId::from("recorded-view-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    let sticky = state.policy.clone();
    assert_eq!(
        crate::SessionReadView::recorded_from_runtime_state(&state)
            .policy()
            .model,
        sticky.model,
        "without a run view the recorded view is the resident policy"
    );

    let mut root = crate::store::persisted_session_config_from_state(&state);
    root.model = Some(recorded_llm_profile("run-route"));
    root.generation.seed = Some(7);
    install_view(&mut state, &root);

    assert_eq!(
        state.policy.model, root.model,
        "the run executes its own view"
    );
    let recorded = crate::SessionReadView::recorded_from_runtime_state(&state);
    assert_eq!(recorded.policy().model, sticky.model);
    assert_eq!(recorded.policy().generation, sticky.generation);
    assert_eq!(
        crate::store::recorded_session_policy_from_state(&state).model,
        sticky.model
    );
}

#[test]
fn recorded_run_view_never_becomes_sticky_after_commit_replay_or_failed_settlement() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    state.session_id = SessionId::from("run-config-law");
    state.policy.model = Some(recorded_llm_profile("sticky-route"));
    let sticky = crate::store::persisted_session_config_from_state(&state);
    let mut run = sticky.clone();
    run.model = Some(recorded_llm_profile("run-route"));

    install_view(&mut state, &run);
    assert_eq!(
        crate::store::execution_session_config_from_state(&state),
        run
    );
    let commit =
        crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            &state,
            crate::store::GraphAppend::PreserveHead,
            boundary_operation(&state.session_id, "root", "commit"),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::store::FleetFormat::current(),
        )
        .expect("run commit");
    assert_eq!(commit.config, sticky);
    assert_eq!(
        commit.config.model,
        Some(recorded_llm_profile("sticky-route"))
    );

    // A failed settlement leaves only the in-memory execution view. A
    // subsequent head reload and recorded replay must still commit the head.
    let head = crate::store::SessionWindowRead::new(
        state.session_id.clone(),
        1,
        sticky.clone(),
        crate::SessionGraph::default(),
        None,
        None,
    )
    .expect("head window");
    adopt_durable_head(
        &mut state,
        head.clone(),
        crate::store::FleetFormat::current(),
    )
    .expect("head reload");
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
    install_view(&mut state, &run);
    assert_eq!(
        crate::store::persisted_session_config_from_state(&state),
        sticky
    );
    adopt_durable_head(
        &mut state,
        head.clone(),
        crate::store::FleetFormat::current(),
    )
    .expect("next root reload");
    assert_eq!(
        state.to_snapshot().policy.model,
        Some(recorded_llm_profile("sticky-route"))
    );
}

/// A run's commit is identified by the view it ran under, never by the
/// head's sticky config it writes back: a redrive that replays a committed
/// run after a config change moved the head builds the same identity, so
/// the store answers its receipt instead of refusing a conflict. The written
/// config is still the head's.
#[test]
fn a_run_commit_identity_covers_its_view_not_the_sticky_config_it_writes() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    state.session_id = SessionId::from("run-config-identity");
    state.policy.model = Some(recorded_llm_profile("first-route"));
    let first = crate::store::persisted_session_config_from_state(&state);
    let commit_under = |state: &RuntimeSessionState| {
        crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            crate::store::GraphAppend::PreserveHead,
            boundary_operation(&state.session_id, "root", "final"),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::store::FleetFormat::current(),
        )
        .expect("run commit")
    };

    // The first execution: the run's recorded view is the head's config.
    install_view(&mut state, &first);
    let original = commit_under(&state);
    assert_eq!(original.config, first);
    assert!(
        original.execution_config.is_none(),
        "the view is the head's"
    );

    // A config change lands on the head after the run committed; the
    // redrive adopts that head, then replays the run's recorded view.
    let mut changed = first.clone();
    changed.model = Some(recorded_llm_profile("second-route"));
    changed.config_revision += 1;
    state.take_run_view();
    adopt_session_config(&mut state, &changed);
    install_view(&mut state, &first);
    let replayed = commit_under(&state);
    assert_eq!(
        replayed.config, changed,
        "the commit writes the head's config"
    );
    assert_eq!(
        replayed.turn_commit_hash().expect("replay identity"),
        original.turn_commit_hash().expect("original identity"),
        "the replay is the run's committed operation"
    );

    // A run that ran under another view is another operation.
    let mut other = first.clone();
    other.model = Some(recorded_llm_profile("other-route"));
    install_view(&mut state, &other);
    assert_ne!(
        commit_under(&state)
            .turn_commit_hash()
            .expect("other identity"),
        original.turn_commit_hash().expect("original identity"),
    );
}

fn projection_text(id: &str) -> crate::Message {
    crate::Message {
        id: id.to_string(),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part::text(
            format!("{id}.p0"),
            id.to_string(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    }
}

fn assert_read_views_current_frame(
    state: &RuntimeSessionState,
    expected: Option<&crate::FrameNodeId>,
) {
    let snapshot = state.to_snapshot();
    let policy = state.policy.clone();
    let options = crate::ProtocolTurnOptions::default();
    let views = [
        crate::SessionReadView::from_snapshot(&snapshot),
        crate::SessionReadView::from_persisted_state(state),
        crate::SessionReadView::from_persisted_state_with_relation(
            state,
            crate::SessionRelation::Root,
        ),
        crate::SessionReadView::from_runtime_state(state, policy.clone(), options.clone()),
        crate::SessionReadView::recorded_from_runtime_state(state),
        crate::SessionReadView::derived_from_persisted_state(
            state,
            policy.clone(),
            state.turn_index,
            options.clone(),
            std::sync::Arc::new(state.session_graph.clone()),
            crate::MessageSequence::from(vec![projection_text("projected")]),
        ),
        crate::SessionReadView::derived_from_persisted_state(
            state,
            policy,
            state.turn_index,
            options,
            std::sync::Arc::new(state.session_graph.clone()),
            crate::MessageSequence::default(),
        ),
    ];
    for view in views {
        assert_eq!(view.current_frame(), expected);
        assert_eq!(
            view.current_frame(),
            view.to_snapshot()
                .expect("source fixture graph is admitted")
                .current_frame_node_id
                .as_ref()
        );
    }
}

/// FIG-5248: a fresh session exposes its initial frame without a snapshot projection.
#[test]
fn read_view_current_frame_is_the_initial_frame() {
    let mut state = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    assert_read_views_current_frame(&state, None);
    state.ensure_agent_frame_initialized().unwrap();
    let initial_frame = state.current_frame_node_id.as_ref().expect("initial frame");
    assert_read_views_current_frame(&state, Some(initial_frame));
}

/// ADR 0112 §9, §14 test 6: once a frame switch is durable, the resident
/// graph is the new frame. The window base is the new `FrameOpen`, both
/// resident nodes stay durable, and one frame
/// record remains, continuing from the old frame.
#[test]
fn a_durable_frame_switch_leaves_only_the_new_frame_resident() {
    use crate::facade_support::AgentFrameReasonFacadeOps as _;
    let clock = crate::SystemClock;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("frame-residency"),
        ..RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    state
        .append_active_conversation_messages(&[projection_text("a1"), projection_text("a2")])
        .unwrap();
    let old_frame = state.current_frame_node_id.clone().expect("initial frame");
    let old_view = crate::SessionReadView::from_persisted_state(&state);
    let durable = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(durable);
    assert_eq!(
        state.session_graph.nodes.len(),
        3,
        "nothing retires in one frame"
    );

    let opened = open_agent_frame_in_state_with_clock(
        &mut state,
        crate::OpenAgentFrameRequest::new(
            crate::FrameKey::from_caller_material("continued").expect("frame material"),
            crate::AgentFrameReason::continue_as(),
        ),
        &clock,
    )
    .expect("open the new frame");
    assert!(opened.opened);
    assert!(
        state.read_model().messages.is_empty(),
        "the pending frame already owns the projection"
    );
    state
        .append_active_conversation_messages(&[projection_text("b1")])
        .unwrap();
    assert_eq!(
        state.session_graph.nodes.len(),
        5,
        "pending nodes stay resident"
    );

    let new_frame = state.current_frame_node_id.clone().expect("new frame");
    let committed = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    state.mark_node_ids_persisted(committed);

    assert_eq!(state.current_frame_node_id.as_ref(), Some(&new_frame));
    assert_ne!(new_frame, old_frame);
    assert_read_views_current_frame(&state, Some(&new_frame));
    assert_eq!(old_view.current_frame(), Some(&old_frame));
    let anchor = state.session_graph.anchor().expect("re-anchored").clone();
    assert_eq!(anchor.frame_node_id, new_frame);
    assert_eq!(anchor.previous_frame_node_id.as_ref(), Some(&old_frame));
    assert_eq!(anchor.generation, 3);
    assert_eq!(state.session_graph.nodes.len(), 2);
    assert_eq!(state.session_graph.durable_len(), 2);
    assert_eq!(state.agent_frames.len(), 1);
    assert_eq!(
        state.agent_frames[0].previous_frame_node_id.as_ref(),
        Some(&old_frame)
    );
    assert_eq!(state.read_model().messages.len(), 1);
}

fn capped_plugin_config(cap: u64) -> crate::PluginConfig {
    let mut config = crate::PluginConfig::default();
    config
        .insert("cap_owner", serde_json::json!({ "cap": cap }))
        .unwrap();
    config
}

/// FIG-4379: a run executes under the configuration it was admitted under. A
/// redrive installs the run's recorded `ResolvedRun` over a head a later
/// config patch moved on, and the hook input and a process the run starts
/// both carry the admitted configuration at its admitted revision, not the
/// head's.
#[test]
fn a_redriven_run_executes_under_its_admitted_plugin_config_revision() {
    let policy = crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    );
    let mut admitted = crate::PersistedSessionConfig::from_policy(&policy);
    admitted.plugin_config = capped_plugin_config(12);
    admitted.config_revision = 4;
    let resolved = crate::run_spec::RunSpec::default()
        .resolve(
            &admitted,
            None,
            &crate::provider::EmptyLlmProfiles,
            &crate::run_spec::NoRunOptionsOwner,
        )
        .expect("resolve the run");

    let mut head = admitted.clone();
    head.plugin_config = capped_plugin_config(20);
    head.config_revision = 5;
    let mut state = RuntimeSessionState::fixture(policy.clone());
    adopt_session_config(&mut state, &head);
    assert_eq!(
        state.admitted_plugin_config(),
        crate::AdmittedPluginConfig::new(capped_plugin_config(20), 5),
        "outside a run the head's configuration is the installed one"
    );

    state.install_run_view(&resolved).unwrap();
    let expected = crate::AdmittedPluginConfig::new(capped_plugin_config(12), 4);
    assert_eq!(state.admitted_plugin_config(), expected);
}

/// FIG-4379: every frame open captures the installed configuration, so a
/// fork point carries the configuration its frame ran under.
#[test]
fn a_frame_captures_the_installed_plugin_config() {
    let mut state = fresh_state_with_initial_frame();
    state.authority.plugin_config = capped_plugin_config(9);
    state
        .open_unpersisted_initial_frame_under_current_assignment()
        .unwrap();
    let frame = state
        .session_graph
        .nodes
        .first()
        .expect("the initial frame");
    assert_eq!(
        frame
            .frame_config()
            .expect("a frame carries its config")
            .plugin_config,
        capped_plugin_config(9)
    );
}

/// The wire fence (FIG-6074): a checkpoint that still names the retired
/// tool-state component is refused at decode, typed, never kept as an
/// opaque component.
#[test]
fn a_checkpoint_naming_the_retired_tool_state_component_is_refused() {
    let body = b"{}";
    let checkpoint = crate::store::HydratedSessionCheckpoint {
        components: [(
            crate::store::RETIRED_TOOL_STATE_CHECKPOINT_COMPONENT.to_owned(),
            crate::store::HydratedCheckpointComponent::Changed {
                encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
                body_ref: crate::BlobRef::for_content(body),
                body: std::sync::Arc::from(&body[..]),
            },
        )]
        .into_iter()
        .collect(),
        ..crate::store::HydratedSessionCheckpoint::default()
    };
    assert!(matches!(
        RuntimeCheckpointComponents::from_hydrated(
            &checkpoint,
            crate::store::FleetFormat::current()
        ),
        Err(crate::StoreError::RetiredStoredShape {
            shape: crate::store::RETIRED_TOOL_STATE_CHECKPOINT_COMPONENT,
            ..
        })
    ));
}

/// COMMITTED-BODY-RELEASE and PLUGIN-KEY-LOCAL-PUBLICATION: discarding
/// committed payloads drops the encoding cache, retaining only the addresses
/// needed to avoid publishing unchanged values with the next head.
#[test]
fn released_plugin_bodies_drop_their_cache_and_keep_only_held_addresses() {
    let namespace = |value: &str, generation| crate::plugin_state::PluginNamespaceState {
        generation,
        values: std::collections::BTreeMap::from([
            ("a".to_owned(), serde_json::Value::String(value.to_owned())),
            (
                "z".to_owned(),
                serde_json::Value::String("x".repeat(1024 * 1024)),
            ),
        ])
        .into(),
        ..Default::default()
    };
    let mut state = ambient();
    let initial = namespace("0", 0);
    let values = std::sync::Arc::downgrade(&initial.values);
    state.set_plugin_state(Some(crate::PluginState {
        plugins: std::collections::BTreeMap::from([("keyed".into(), initial)]),
    }));
    state
        .apply_persisted_commit_result(commit_result_for(&state))
        .unwrap();
    assert!(state.plugin_state().is_none());
    assert!(
        values.upgrade().is_none(),
        "a discarded namespace's encoding cache retained its values"
    );

    state.set_plugin_state(Some(crate::PluginState {
        plugins: std::collections::BTreeMap::from([("keyed".into(), namespace("1", 1))]),
    }));
    let next = crate::RuntimeCommit::persisted_state_for_test(&state);
    assert!(next.checkpoint.namespace_nodes.len() <= 3);
    assert!(
        next.checkpoint
            .namespace_nodes
            .values()
            .map(|node| node.bytes.len())
            .sum::<usize>()
            < 1024
    );
}

/// FIG-6910: durable nodes are the resident graph's prefix. A commit carries
/// exactly the pending suffix, and its receipt makes durable exactly what it
/// carried: a node appended while the commit was in flight stays pending and
/// rides the next commit, so no appended node is ever skipped or carried
/// twice.
#[test]
fn a_commit_carries_the_pending_suffix_and_its_receipt_makes_only_that_durable() {
    fn ids(commit: &crate::GraphAppend) -> Vec<crate::NodeId> {
        commit
            .nodes()
            .iter()
            .map(|node| node.node_id.clone())
            .collect()
    }
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("durable-prefix"),
        ..RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    state
        .append_active_conversation_messages(&[projection_text("a1"), projection_text("a2")])
        .unwrap();
    let first = state.pending_graph_commit();
    let first_ids = ids(&first);
    assert_eq!(
        first_ids,
        state
            .session_graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>(),
        "a fresh graph's first commit carries every node: the frame and both messages"
    );

    state
        .append_active_conversation_messages(&[projection_text("a3")])
        .unwrap();
    let in_flight = state.session_graph.leaf_node_id.clone().expect("a3's node");
    state.mark_node_ids_persisted(first_ids.clone());
    assert!(first_ids.iter().all(|id| state.is_node_persisted(id)));
    assert!(!state.is_node_persisted(&in_flight));
    let second = state.pending_graph_commit();
    assert_eq!(
        ids(&second),
        vec![in_flight.clone()],
        "the node appended behind the first commit rides the next one alone"
    );

    state.mark_node_ids_persisted(ids(&second));
    assert!(state.is_node_persisted(&in_flight));
    assert!(matches!(
        state.pending_graph_commit(),
        crate::GraphAppend::PreserveHead
    ));
    assert_eq!(
        state.session_graph.durable_len(),
        state.session_graph.nodes.len()
    );
}
