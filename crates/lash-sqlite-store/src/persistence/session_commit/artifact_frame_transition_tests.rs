use super::*;
use lash_core_execution::{
    ArtifactName, ArtifactReferrer, ArtifactStoreId, FrameEnvironmentId, FrameNodeId,
};

#[test]
fn first_commit_may_end_its_own_appended_frame_open() {
    let session = SessionId::from("first-turn-switch");
    let first = FrameNodeId::new("first-frame").expect("frame id");
    let successor = FrameNodeId::new("successor-frame").expect("frame id");
    let graph = lash_core_execution::store::GraphAppend::Extend {
        nodes: vec![lash_core_execution::SessionNodeRecord {
            node_id: first.as_str().to_owned().into(),
            parent_node_id: None,
            timestamp: "2026-09-29T00:00:00Z".into(),
            payload: lash_core_execution::SessionNodePayload::FrameOpen {
                frame_key: lash_core_execution::FrameKey::from_caller_material("first-frame")
                    .expect("frame key"),
                reason: lash_core_execution::AgentFrameReason::initial(),
                assignment: lash_core_execution::AgentFrameAssignment::from_policy(
                    lash_core_execution::SessionPolicy::new(
                        lash_core_execution::TurnBudget::Unbounded,
                    ),
                ),
                protocol_turn_options: lash_core_execution::ProtocolTurnOptions::default(),
            },
        }],
    };
    let frames_left = |prior: Option<&FrameNodeId>, new_head: &FrameNodeId| {
        lash_core_execution::store::frames_left_by_commit(prior, &graph, Some(new_head))
    };
    assert_eq!(frames_left(None, &successor), vec![first.clone()]);
    assert!(frames_left(None, &first).is_empty());
    let prior = FrameNodeId::new("prior-frame").expect("frame id");
    assert_eq!(
        frames_left(Some(&prior), &successor),
        vec![prior.clone(), first.clone()]
    );

    let mut conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("create durable core");
    let ended = FrameEnvironmentId::new(session.clone(), first);
    let ended_referrer = ArtifactReferrer::FrameEnvironment(ended.clone());
    conn.execute(
        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref) VALUES (?1, 'module', 'blob')",
        params![crate::artifact_store::MODULE_ARTIFACT_NAMESPACE],
    )
    .expect("artifact pointer");
    conn.execute(
        crate::artifact_store::artifact_sql()
            .edges
            .insert_edge
            .sql(),
        params![
            crate::artifact_store::MODULE_ARTIFACT_NAMESPACE,
            "module",
            ended_referrer.kind().as_str(),
            ended_referrer.canonical_id(),
        ],
    )
    .expect("first frame edge");
    let transition = lash_core_execution::store::FrameTransition {
        ended,
        successor: FrameEnvironmentId::new(session, successor),
        carries: Vec::new(),
        gate: lash_sansio::ExecutionScope::runtime_operation("first-turn-switch")
            .journal_identity()
            .expect("journal identity"),
    };
    let tx = conn.transaction().expect("transaction");
    commit_frame_transition_tx(
        &tx,
        &transition,
        &[transition.ended.frame_node_id().clone()],
        123,
    )
    .expect("transition");
    tx.commit().expect("commit");
    assert!(
        crate::artifact_store::artifact_fenced_tx(&conn, &ended_referrer)
            .expect("first frame fence")
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_cleanup_obligations WHERE referrer_kind = 'frame_environment'",
            [],
            |row| row.get(0),
        )
        .expect("cleanup obligation");
    assert_eq!(count, 1);
}

#[test]
fn frame_transition_carries_then_fences_and_records_gated_cleanup() {
    let mut conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("create durable core");
    let session = SessionId::from("frame-transition-test");
    let old = FrameEnvironmentId::new(
        session.clone(),
        FrameNodeId::new("old-frame").expect("frame id"),
    );
    let next = FrameEnvironmentId::new(session, FrameNodeId::new("next-frame").expect("frame id"));
    let source = ArtifactReferrer::FrameEnvironment(old.clone());
    conn.execute(
        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref) VALUES (?1, ?2, 'blob')",
        params![crate::artifact_store::MODULE_ARTIFACT_NAMESPACE, "carried"],
    )
    .expect("artifact pointer");
    conn.execute(
        crate::artifact_store::artifact_sql()
            .edges
            .insert_edge
            .sql(),
        params![
            crate::artifact_store::MODULE_ARTIFACT_NAMESPACE,
            "carried",
            source.kind().as_str(),
            source.canonical_id()
        ],
    )
    .expect("source edge");
    let transition = lash_core_execution::store::FrameTransition {
        ended: old,
        successor: next.clone(),
        carries: vec![ArtifactName {
            store: ArtifactStoreId::LashlangModule,
            artifact_ref: "carried".into(),
        }],
        gate: lash_sansio::ExecutionScope::runtime_operation("switch")
            .journal_identity()
            .expect("journal identity"),
    };
    let tx = conn.transaction().expect("transaction");
    commit_frame_transition_tx(
        &tx,
        &transition,
        &[transition.ended.frame_node_id().clone()],
        123,
    )
    .expect("transition");
    tx.commit().expect("commit");
    let successor = ArtifactReferrer::FrameEnvironment(next);
    assert!(crate::artifact_store::artifact_fenced_tx(&conn, &source).expect("source fence"));
    assert!(
        conn.query_row(
            crate::artifact_store::artifact_sql()
                .edges
                .select_edge_exists
                .sql(),
            params![
                crate::artifact_store::MODULE_ARTIFACT_NAMESPACE,
                "carried",
                successor.kind().as_str(),
                successor.canonical_id()
            ],
            |row| row.get::<_, bool>(0)
        )
        .expect("successor edge")
    );
    let body: String = conn
        .query_row(
            "SELECT cleanup_json FROM artifact_cleanup_obligations",
            [],
            |row| row.get(0),
        )
        .expect("cleanup record");
    assert_eq!(
        lash_core_execution::ArtifactCleanup::from_json(&body, &source).expect("decode cleanup"),
        transition.ended_cleanup()
    );
}

/// A session bound on a fresh store whose first frame is committed.
async fn committed_first_frame(
    store: &Store,
    session: &str,
    clock: &lash_core::testing::TestClock,
) -> lash_core_execution::RuntimeSessionState {
    let session_id = SessionId::from(session);
    store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(&session_id))
        .await
        .expect("bind session");
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id,
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized_with_clock(clock);
    commit_frame_opens(store, &mut state, None).await;
    state
}

/// Commit every pending frame open in `state`, with `transition` attached.
async fn commit_frame_opens(
    store: &Store,
    state: &mut lash_core_execution::RuntimeSessionState,
    transition: Option<lash_core_execution::store::FrameTransition>,
) {
    let mut commit = RuntimeCommit::persisted_state_for_test(state, &[]);
    let appended = commit
        .graph
        .nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    commit.frame_transition = transition;
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit frame opens");
    state.apply_persisted_commit_result(receipt);
    state.mark_node_ids_persisted(appended);
}

fn open_resident_frame(
    state: &mut lash_core_execution::RuntimeSessionState,
    key: &str,
    clock: &lash_core::testing::TestClock,
) -> FrameNodeId {
    lash_core::runtime::state::open_agent_frame_in_state_with_clock(
        state,
        lash_core_execution::OpenAgentFrameRequest::new(
            lash_core_execution::FrameKey::from_caller_material(key).expect("frame key"),
            lash_core_execution::AgentFrameReason::new("test"),
        ),
        clock,
    )
    .expect("open frame");
    state.current_frame_node_id.clone().expect("current frame")
}

async fn hold_module(store: &Store, frame: &FrameEnvironmentId, module_ref: &str) {
    let claim = lash_core_execution::ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(
        frame.clone(),
    ))
    .expect("frame claim");
    lash_core_execution::ModuleArtifactStore::publish_module_artifact(
        store,
        &claim,
        module_ref,
        module_ref.as_bytes(),
    )
    .await
    .expect("hold module under frame");
}

/// Whether `frame` is fenced, and the cleanup record that will sever its
/// edges, if one exists.
async fn frame_end(
    store: &Store,
    frame: &FrameEnvironmentId,
) -> (bool, Option<lash_core_execution::ArtifactCleanup>) {
    let referrer = ArtifactReferrer::FrameEnvironment(frame.clone());
    store
        .conn
        .call(move |conn| {
            let fenced = crate::artifact_store::artifact_fenced_tx(conn, &referrer)?;
            let body: Option<String> = conn
                .query_row(
                    "SELECT cleanup_json FROM artifact_cleanup_obligations \
                     WHERE referrer_kind = ?1 AND referrer_id = ?2",
                    params![referrer.kind().as_str(), referrer.canonical_id()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok((
                fenced,
                body.map(|body| {
                    lash_core_execution::ArtifactCleanup::from_json(&body, &referrer)
                        .expect("decode cleanup")
                }),
            ))
        })
        .await
        .expect("read frame end")
}

fn gate() -> lash_sansio::EffectJournalIdentity {
    lash_sansio::ExecutionScope::runtime_operation("frame-switch-turn")
        .journal_identity()
        .expect("journal identity")
}

/// FIG-4031 corner (a): a turn admitted on a frame opened only in
/// resident state after a committed frame switches away from it. Its one
/// final commit leaves both the committed head frame and the resident
/// frame, and neither may keep its edges.
#[tokio::test]
async fn a_switch_out_of_a_resident_frame_ends_every_frame_the_commit_leaves() {
    let store = crate::test_support::memory_store().await.expect("store");
    let clock = lash_core::testing::TestClock::new(1_000);
    let mut state = committed_first_frame(&store, "resident-frame-switch", &clock).await;
    let session = state.session_id.clone();
    let first = FrameEnvironmentId::new(
        session.clone(),
        state.current_frame_node_id.clone().expect("first frame"),
    );
    let resident = FrameEnvironmentId::new(
        session.clone(),
        open_resident_frame(&mut state, "resident", &clock),
    );
    hold_module(&store, &resident, "held-by-resident-frame").await;
    let successor = FrameEnvironmentId::new(
        session,
        open_resident_frame(&mut state, "successor", &clock),
    );
    // The transition a switch out of an uncommitted frame names: the
    // last committed frame, with no carries.
    let transition = lash_core_execution::store::FrameTransition {
        ended: first.clone(),
        successor: successor.clone(),
        carries: Vec::new(),
        gate: gate(),
    };
    commit_frame_opens(&store, &mut state, Some(transition)).await;

    for frame in [&first, &resident] {
        let (fenced, cleanup) = frame_end(&store, frame).await;
        assert!(fenced, "{frame:?} must be fenced by the switch commit");
        assert_eq!(
            cleanup,
            Some(lash_core_execution::ArtifactCleanup::ended(
                ArtifactReferrer::FrameEnvironment(frame.clone()),
                Vec::new(),
                Some(gate()),
            )),
            "{frame:?} must owe a gated cleanup that severs its edges"
        );
    }
    assert_eq!(frame_end(&store, &successor).await, (false, None));
}

/// FIG-4031 corner (b): a frame opened directly in resident state is
/// committed by a commit that is neither a turn nor a compaction (a park
/// or a session command), so no transition rides it. The frame it leaves
/// ends anyway.
#[tokio::test]
async fn a_commit_without_a_transition_that_changes_the_frame_ends_the_frame_it_leaves() {
    let store = crate::test_support::memory_store().await.expect("store");
    let clock = lash_core::testing::TestClock::new(1_000);
    let mut state = committed_first_frame(&store, "park-frame-switch", &clock).await;
    let session = state.session_id.clone();
    let first = FrameEnvironmentId::new(
        session.clone(),
        state.current_frame_node_id.clone().expect("first frame"),
    );
    hold_module(&store, &first, "held-by-first-frame").await;
    let opened = FrameEnvironmentId::new(
        session,
        open_resident_frame(&mut state, "opened-directly", &clock),
    );
    commit_frame_opens(&store, &mut state, None).await;

    let (fenced, cleanup) = frame_end(&store, &first).await;
    assert!(fenced, "the frame a park leaves must be fenced");
    assert_eq!(
        cleanup,
        Some(lash_core_execution::ArtifactCleanup::ended(
            ArtifactReferrer::FrameEnvironment(first.clone()),
            Vec::new(),
            None,
        )),
        "the frame a park leaves must owe a cleanup that severs its edges"
    );
    assert_eq!(frame_end(&store, &opened).await, (false, None));
}

/// FIG-4031 corner (c): a `continue_as` seed carrying a forged process value
/// names a module the frame holds no edge of, either never stored or held
/// only by another referrer. The switch fails closed with
/// `ArtifactCarryMissing` and writes nothing: I-frame is broken, and no
/// commit may paper over it.
#[tokio::test]
async fn a_switch_carrying_a_module_its_frame_does_not_hold_fails_closed() {
    let store = crate::test_support::memory_store().await.expect("store");
    let clock = lash_core::testing::TestClock::new(1_000);
    let mut state = committed_first_frame(&store, "forged-carry", &clock).await;
    let session = state.session_id.clone();
    let first = FrameEnvironmentId::new(
        session.clone(),
        state.current_frame_node_id.clone().expect("first frame"),
    );
    let pinned = lash_core_execution::ReferrerClaim::unguarded(ArtifactReferrer::HostPin(
        lash_core_execution::HostArtifactPin::mint(),
    ))
    .expect("host pin claim");
    lash_core_execution::ModuleArtifactStore::publish_module_artifact(
        &store,
        &pinned,
        "held-by-a-host-pin",
        b"bytes",
    )
    .await
    .expect("hold module under a host pin");
    let head_revision = state.head_revision;
    let successor = FrameEnvironmentId::new(
        session,
        open_resident_frame(&mut state, "successor", &clock),
    );
    for forged in ["never-stored", "held-by-a-host-pin"] {
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.frame_transition = Some(lash_core_execution::store::FrameTransition {
            ended: first.clone(),
            successor: successor.clone(),
            carries: vec![ArtifactName {
                store: ArtifactStoreId::LashlangModule,
                artifact_ref: forged.into(),
            }],
            gate: gate(),
        });
        let refused = store.commit_runtime_state(commit).await;
        assert!(
            matches!(
                &refused,
                Err(StoreError::ArtifactCarryMissing { artifact_ref, to })
                    if artifact_ref == forged
                        && *to == ArtifactReferrer::FrameEnvironment(successor.clone())
            ),
            "{forged}: {refused:?}"
        );
        assert_eq!(frame_end(&store, &first).await, (false, None), "{forged}");
        let head = lash_core_execution::store::load_persisted_session_state(&store)
            .await
            .expect("load head")
            .expect("head exists");
        assert_eq!(head.head_revision, head_revision, "{forged}");
    }
}
