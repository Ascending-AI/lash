use super::*;
use pretty_assertions::assert_eq;

/// Prove that independently opened handles mint distinct pending-input identities.
pub async fn reopen_mint_identity(probe: ReopenableRuntimeStore) {
    assert_fresh_instances(&probe.open, &probe.reopen, "runtime_persistence_reopenable");
    pending_turn_input_mint_is_unique_across_store_instances(
        probe.open.as_ref(),
        probe.reopen.as_ref(),
    )
    .await;
}

/// Every session read names its session (ADR 0112 §1), so a catalog holding
/// several sessions answers each one's head and window reads for that session
/// alone, and the two projections agree: across `{absent, admitted-only,
/// committed}` sessions of one catalog, `load_session_head_meta` and a
/// `Current` window read agree on presence, id, revision, leaf and checkpoint.
///
/// This replaces the unbound-handle resolution laws: a store no longer infers
/// a sole session, so there is no ambiguity to refuse.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn head_and_window_reads_agree_for_each_named_session(store: Arc<dyn RuntimeStore>) {
    async fn assert_reads_agree(store: &dyn RuntimeStore, session_id: &SessionId) -> Option<u64> {
        let head = store
            .load_session_head_meta(session_id)
            .await
            .expect("read the named head");
        let window = store
            .load_session_window(session_id, crate::store::WindowSelector::Current)
            .await
            .expect("read the named window");
        match (window, head) {
            (None, None) => None,
            (Some(window), Some(head)) => {
                assert_eq!(
                    head.session_id, *session_id,
                    "{session_id}: head session id"
                );
                assert_eq!(
                    window.session_id, *session_id,
                    "{session_id}: window session id"
                );
                assert_eq!(
                    head.head_revision, window.head_revision,
                    "{session_id}: head revision"
                );
                assert_eq!(
                    head.leaf_node_id, window.window.leaf_node_id,
                    "{session_id}: leaf node id"
                );
                assert_eq!(
                    head.checkpoint_ref, window.checkpoint_ref,
                    "{session_id}: checkpoint reference"
                );
                Some(head.head_revision)
            }
            (window, head) => panic!(
                "{session_id}: head and window reads disagreed about presence: window={window:?}, head={head:?}"
            ),
        }
    }

    let committed = SessionId::from("read-agreement");
    let admitted_only = SessionId::from("read-agreement-admitted-only");
    let absent = SessionId::from("read-agreement-absent");
    admit_conformance_session(&store, &admitted_only).await;
    let state = RuntimeSessionState {
        session_id: committed.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "read-agreement",
    )
    .await
    .expect("commit the named session");

    assert_eq!(
        assert_reads_agree(store.as_ref(), &committed).await,
        Some(1),
        "the committed session reads its own head"
    );
    assert_eq!(
        assert_reads_agree(store.as_ref(), &admitted_only).await,
        None,
        "an admitted session with no commit has no head, whatever its neighbours hold"
    );
    assert_eq!(
        assert_reads_agree(store.as_ref(), &absent).await,
        None,
        "a session the catalog never held has no head"
    );
}

/// A newly minted turn-input identity is store-wide rather than handle-local.
///
/// Reopenable backends exercise this through two independently constructed
/// handles over one durable store. Their conformance clocks deliberately keep
/// both admissions in one millisecond so the nonce is the deciding fact.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn pending_turn_input_mint_is_unique_across_store_instances(
    first: &dyn RuntimeStore,
    second: &dyn RuntimeStore,
) {
    let session_id = "pending-turn-input-multi-store-mint";
    let first_input = first
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "first independent-store input",
        ))
        .await
        .expect("first store instance mints a pending turn-input ID");
    let second_input = second
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "second independent-store input",
        ))
        .await
        .expect("second store instance mints a pending turn-input ID");

    assert_ne!(
        first_input.input_id, second_input.input_id,
        "independent store instances must mint distinct pending turn-input IDs in one millisecond"
    );
}

/// FIG-2479: the commanded protocol-turn-options fact round-trips resident
/// state → committed head row (SESSION_HEAD_META v6) → cold load, and the head
/// value is what the loaded state carries.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_plugin_config_round_trips_through_the_committed_head(
    store: Arc<dyn RuntimeStore>,
) {
    let mut expected = crate::PluginConfig::for_protocol(Some("conformance-protocol".to_string()));
    expected.insert(
        "conformance-protocol",
        serde_json::json!({
            "dialect": "conformance-dialect",
            "termination": {"kind": "conformance-termination"},
        }),
    );
    expected.insert("conformance-plugin", serde_json::json!({"turn_cap": 12}));
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("session-plugin-config"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.authority.plugin_config = expected.clone();

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "session-plugin-config",
    )
    .await
    .expect("commit session plugin config");

    let head = store
        .load_session_head_meta(&SessionId::from("session-plugin-config"))
        .await
        .expect("load session head")
        .expect("committed session head");
    assert_eq!(
        head.config.plugin_config, expected,
        "the committed head row must carry every owner's recorded namespace"
    );
    let restored = crate::conformance::helpers::load_window_state(
        &store,
        &SessionId::from("session-plugin-config"),
    )
    .await
    .expect("load persisted session state")
    .expect("committed session state");
    assert_eq!(
        restored.authority.plugin_config, expected,
        "cold load must restore the recorded plugin config from the head"
    );
    assert_eq!(
        restored.effective_protocol_turn_options(),
        expected.protocol_turn_options(),
        "the protocol turn options are the protocol owner's recorded namespace"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn execution_state_replace_then_clear_removes_the_live_checkpoint_ref(
    store: Arc<dyn RuntimeStore>,
) {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.session_id = SessionId::from("execution-state-replace-then-clear".to_string());
    state.set_execution_state_snapshot(Some(b"initial-execution-state".to_vec().into()));

    let initial = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "execution-state-initial",
    )
    .await
    .expect("commit initial execution state");
    state.apply_persisted_commit_result(initial);

    state.set_execution_state_snapshot(Some(b"replacement-execution-state".to_vec().into()));
    let replacement = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "execution-state-replacement",
    )
    .await
    .expect("replace execution state");
    assert!(
        replacement
            .manifest
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_some()
    );
    state.apply_persisted_commit_result(replacement);

    state.set_execution_state_snapshot(None);
    let cleared = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "execution-state-clear",
    )
    .await
    .expect("clear replacement execution state");
    assert!(
        cleared
            .manifest
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none()
    );

    let durable = store
        .load_session_window(
            &SessionId::from("execution-state-replace-then-clear"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load replace-then-clear session")
        .expect("replace-then-clear session is durable");
    let checkpoint = durable
        .checkpoint
        .expect("replace-then-clear session has a checkpoint");
    assert!(
        checkpoint
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none()
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_carried_nondefault_node_budget(store: Arc<dyn RuntimeStore>) {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let parent = sample_session_node(&SessionId::from("root"), "budget-frame", None);
    let child = sample_session_node(
        &SessionId::from("root"),
        "budget-child",
        Some(&parent.node_id),
    );
    let budget = crate::CommitBudget::new(
        crate::CommitBudgetLimit::Unbounded,
        crate::CommitBudgetLimit::bounded(CONFIGURED_NODE_LIMIT),
    );
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, budget);
    commit.graph = crate::GraphAppend::Extend {
        nodes: vec![parent, child],
    };

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("backend must enforce the carried non-default node budget");
    assert!(matches!(
        error,
        StoreError::CommitNodeBudgetExceeded {
            node_count: 2,
            max_nodes: CONFIGURED_NODE_LIMIT,
        }
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_carried_nondefault_byte_budget(store: Arc<dyn RuntimeStore>) {
    const CONFIGURED_BYTE_LIMIT: usize = 64;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let budget = crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
        crate::CommitBudgetLimit::Unbounded,
    );
    let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, budget);
    commit.checkpoint.components.insert(
        crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
        crate::HydratedCheckpointComponent::changed(vec![0; CONFIGURED_BYTE_LIMIT * 2]),
    );

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("backend must enforce the carried non-default byte budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            max_bytes: CONFIGURED_BYTE_LIMIT,
            ..
        }
    ));
}

pub(super) fn commit_budget_conformance_fixture(byte_limit: usize) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    RuntimeCommit::persisted_state_for_test_with_budget(
        &state,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(byte_limit),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_follow_on_bytes_over_budget(store: Arc<dyn RuntimeStore>) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit without a pending follow-on must fit");
    commit.pending_follow_on = Some(crate::store::PendingFollowOn {
        follow_on_turn_id: crate::TurnId::from("oversized:agent-frame:1"),
        frame_id: crate::session_graph::frame_node_id(&SessionId::from("root"), "oversized"),
        task: "q".repeat(BYTE_LIMIT * 2),
        resolved_run: crate::conformance::helpers::default_resolved_run(),
        chain_depth: 1,
        attempts: 0,
    });

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("pending follow-on bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            follow_on_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if follow_on_bytes > BYTE_LIMIT
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_agent_frame_bytes_over_budget(store: Arc<dyn RuntimeStore>) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit without an agent frame must fit");
    commit.current_frame_node_id = Some(
        crate::FrameNodeId::new("f".repeat(BYTE_LIMIT * 2))
            .expect("test frame identity is non-empty"),
    );

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("agent frame bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            agent_frame_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if agent_frame_bytes > BYTE_LIMIT
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_turn_result_bytes_over_budget(store: Arc<dyn RuntimeStore>) {
    const BYTE_LIMIT: usize = 2_048;
    let mut commit = commit_budget_conformance_fixture(BYTE_LIMIT);
    commit
        .validate_budget()
        .expect("the commit with its ordinary turn result must fit");
    commit.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::new(
        crate::ExecutionScope::runtime_operation("t".repeat(BYTE_LIMIT * 2)),
        "commit",
    ));

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("turn result bytes alone must trip the commit budget");
    assert!(matches!(
        error,
        StoreError::CommitByteBudgetExceeded {
            turn_result_bytes,
            max_bytes: BYTE_LIMIT,
            ..
        } if turn_result_bytes > BYTE_LIMIT
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_with_every_payload_family_inside_budget_succeeds(store: Arc<dyn RuntimeStore>) {
    const BYTE_LIMIT: usize = 64 * 1024;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    // A turn's terminal commit, so it may carry the follow-on a frame switch
    // owes (ADR 0101 §3).
    let operation = crate::OperationId::turn("root", "all-families", "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive all-families node ids");
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
        &state,
        graph,
        operation,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
        crate::FleetFormat::current(),
    )
    .expect("build the all-families commit");
    let attachment_id =
        AttachmentId::parse("all-families-attachment").expect("valid attachment id");
    crate::conformance::helpers::record_completed_attachment_write(
        &store,
        crate::AttachmentWrite {
            attachment_id: attachment_id.clone(),
            claim: crate::conformance::attachment_referrers::claim(
                crate::ArtifactReferrer::Session(SessionId::from("root")),
            ),
        },
    )
    .await;
    commit.committed_attachment_ids = vec![attachment_id];
    commit.pending_follow_on = Some(crate::store::PendingFollowOn {
        follow_on_turn_id: crate::TurnId::from("all-families:agent-frame:1"),
        frame_id: state
            .current_frame_node_id
            .clone()
            .expect("the initial frame is current"),
        task: "follow-up".to_string(),
        resolved_run: crate::conformance::helpers::default_resolved_run(),
        chain_depth: 1,
        attempts: 0,
    });

    store
        .commit_runtime_state(commit)
        .await
        .expect("a commit with every payload family inside the limit must succeed");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn head_retirement_gate_distinguishes_leaf_change_from_same_leaf(
    store: Arc<dyn RuntimeStore>,
) {
    let state = seed_append_receipt_state(&store).await;
    let old_leaf = state.session_graph.leaf_node_id.clone().expect("seed leaf");

    let same_leaf_commit = RuntimeCommit::persisted_state_for_test(&state);
    let seed_frame_node_id = same_leaf_commit
        .current_frame_node_id
        .clone()
        .expect("seed frame");
    let same_leaf_planner = crate::store::RuntimeCommitPlanner::prepare(
        same_leaf_commit.clone(),
        lash_core::FleetFormat::current(),
    )
    .expect("prepare same-leaf commit");
    let same_leaf_plan = same_leaf_planner
        .plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: same_leaf_commit.expected_head_revision,
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            existing_pending_follow_on: None,
            published_leaf: crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id.to_string().into(),
            }),
        })
        .expect("plan same-leaf commit");
    assert!(
        !same_leaf_plan.head_changed(),
        "a same-leaf commit must not prescribe ancestry retirement"
    );
    store
        .commit_runtime_state(same_leaf_commit)
        .await
        .expect("same-leaf commit");
    crate::conformance::helpers::load_one_node(store.as_ref(), &SessionId::from("root"), &old_leaf)
        .await
        .expect("a same-leaf commit must tombstone nothing");

    let mut changed_state = loaded_conformance_state(&store, &SessionId::from("root")).await;
    let nodes = vec![crate::SessionAppendNode::plugin(
        "retirement-gate",
        serde_json::json!({"leaf": "replacement"}),
    )];
    let (changed_commit, _) =
        append_request_commit(&mut changed_state, "retirement-gate-change", &nodes, None);
    let changed_planner = crate::store::RuntimeCommitPlanner::prepare(
        changed_commit.clone(),
        lash_core::FleetFormat::current(),
    )
    .expect("prepare leaf-changing commit");
    let changed_plan = changed_planner
        .plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: changed_commit.expected_head_revision,
            requested_ancestor_is_active: true,
            occupied_node_ids: std::collections::HashSet::new(),
            existing_pending_follow_on: None,
            published_leaf: crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                node_id: old_leaf.clone(),
                generation: state.session_graph.active_path_nodes().len() as u64 - 1,
                frame_node_id: seed_frame_node_id.into_inner().into(),
            }),
        })
        .expect("plan leaf-changing commit");
    assert!(
        changed_plan.head_changed(),
        "a leaf-changing commit must prescribe retirement of its abandoned old head"
    );
    assert_eq!(
        changed_plan.old_leaf_node_id(),
        Some(old_leaf.as_str()),
        "the retirement prescription must name the abandoned old head"
    );
    store
        .commit_runtime_state(changed_commit)
        .await
        .expect("leaf-changing commit");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_restore_rejects_turn_index_without_increment_headroom(
    store: Arc<dyn RuntimeStore>,
) {
    let turn_index = usize::MAX - 16;
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "turn index overflow seed",
    )
    .await
    .expect("seed corrupt checkpoint turn index");

    let error = crate::conformance::helpers::load_window_state(&store, &SessionId::from("root"))
        .await
        .expect_err("checkpoint turn index without increment headroom must fail restore");
    assert!(matches!(
        error,
        StoreError::CheckpointTurnIndexOutOfRange {
            turn_index: actual,
            max_exclusive,
        } if actual == turn_index && max_exclusive == turn_index
    ));
}

/// The prompt-side subtotal is not covered by the canonical total: signed
/// counters let a negative `output_tokens` hold the canonical total in range
/// while the prompt-side counters alone overflow. Restore must reject that
/// checkpoint rather than hand a poisoned base to the next turn's merge and to
/// the bare `total()`/`input_total()` policy readers.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows(
    store: Arc<dyn RuntimeStore>,
) {
    let token_usage = crate::TokenUsage {
        input_tokens: i64::MAX,
        output_tokens: i64::MIN,
        cache_read_input_tokens: i64::MAX,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    assert!(
        token_usage.checked_total().is_ok(),
        "the canonical total must stay in range so this pins the prompt subtotal check"
    );
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        token_usage,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state),
        "prompt subtotal overflow seed",
    )
    .await
    .expect("seed corrupt checkpoint token usage");

    let error = crate::conformance::helpers::load_window_state(&store, &SessionId::from("root"))
        .await
        .expect_err("checkpoint usage whose prompt subtotal overflows must fail restore");
    assert!(matches!(
        error,
        StoreError::CheckpointTokenUsageOutOfRange {
            counter: "input_total_tokens"
        }
    ));
}
