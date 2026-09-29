use super::*;
use lash_core_execution::{
    ArtifactCleanup, ArtifactReferrer, HostArtifactPin, ModuleArtifactStore as _, ReferrerClaim,
    ResolvedArtifactCleanup,
};

fn frame_state(session_id: &str) -> lash_core_execution::RuntimeSessionState {
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(session_id),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();
    state
}

fn append_successor_frame(
    state: &mut lash_core_execution::RuntimeSessionState,
) -> lash_core_execution::FrameNodeId {
    // The commit derives a frame's node id from its key, so the successor
    // carries that id, the one the committed head will name.
    let key =
        lash_core_execution::FrameKey::from_caller_material("successor-frame").expect("frame key");
    let successor =
        lash_core_execution::session_graph::frame_node_id(&state.session_id, key.as_str());
    assert!(state.session_graph.append_frame_open_with_id_at(
        successor.clone(),
        key,
        lash_core_execution::AgentFrameReason::initial(),
        lash_core_execution::AgentFrameAssignment::from_policy(state.policy.clone()),
        state.protocol_turn_options.clone(),
        "2026-09-29T00:00:00Z".into(),
    ));
    state.current_frame_node_id = Some(successor.clone());
    successor
}

fn frame_transition(
    session_id: &SessionId,
    ended: lash_core_execution::FrameNodeId,
    successor: lash_core_execution::FrameNodeId,
) -> lash_core_execution::store::FrameTransition {
    lash_core_execution::store::FrameTransition {
        ended: lash_core_execution::FrameEnvironmentId::new(session_id.clone(), ended),
        successor: lash_core_execution::FrameEnvironmentId::new(session_id.clone(), successor),
        carries: Vec::new(),
        gate: lash_sansio::ExecutionScope::runtime_operation("postgres-frame-switch")
            .journal_identity()
            .expect("journal identity"),
    }
}

async fn assert_no_frame_commit_rows(storage: &PostgresStorage, session_id: &SessionId) {
    for table in ["lash_sessions", "lash_graph_nodes", "lash_session_meta"] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE session_id = $1"
        ))
        .bind(session_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count refused commit rows");
        assert_eq!(count, 0, "refused commit wrote {table}");
    }
    let fences: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_artifact_referrer_fences")
        .fetch_one(storage.pool())
        .await
        .expect("count fences");
    let cleanups: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_artifact_cleanup_obligations")
            .fetch_one(storage.pool())
            .await
            .expect("count cleanups");
    assert_eq!((fences, cleanups), (0, 0));
}

#[tokio::test]
async fn postgres_first_commit_may_end_its_own_appended_frame_open() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let state = frame_state("first-turn-switch");
    let session_id = state.session_id.clone();
    let ended = state.current_frame_node_id.clone().expect("initial frame");
    let ended_referrer = ArtifactReferrer::FrameEnvironment(
        lash_core_execution::FrameEnvironmentId::new(session_id.clone(), ended.clone()),
    );
    let claim = ReferrerClaim::unguarded(ended_referrer.clone()).expect("frame claim");
    let artifacts = storage.lashlang_artifact_store();
    artifacts
        .publish_module_artifact(&claim, "first-turn-module", b"bytes")
        .await
        .expect("publish under first frame");
    let mut state = state;
    let successor = append_successor_frame(&mut state);
    let commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_frame_transition(frame_transition(&session_id, ended, successor));
    storage
        .session_store(session_id)
        .commit_runtime_state(commit)
        .await
        .expect("commit first-turn frame switch");

    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_artifact_referrer_fences
         WHERE referrer_kind = $1 AND referrer_id = $2)",
    )
    .bind(ended_referrer.kind().as_str())
    .bind(ended_referrer.canonical_id())
    .fetch_one(storage.pool())
    .await
    .expect("read first-frame fence");
    let ended_cleanup: String = sqlx::query_scalar(
        "SELECT cleanup_json FROM lash_artifact_cleanup_obligations
         WHERE referrer_kind = $1 AND referrer_id = $2",
    )
    .bind(ended_referrer.kind().as_str())
    .bind(ended_referrer.canonical_id())
    .fetch_one(storage.pool())
    .await
    .expect("read ended cleanup");
    let cleanup = ArtifactCleanup::from_json(&ended_cleanup, &ended_referrer)
        .expect("decode first-frame cleanup");
    assert!(fenced, "first frame must be fenced in the commit");
    assert!(matches!(
        cleanup.plan,
        lash_core_execution::ArtifactCleanupPlan::Ended { .. }
    ));
    assert!(matches!(
        artifacts
            .publish_module_artifact(&claim, "first-turn-module", b"bytes")
            .await,
        Err(lash_core_execution::ArtifactStoreError::ReferrerEnded { .. })
    ));
}

#[tokio::test]
async fn postgres_first_commit_rejects_unappended_transition_source() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("first-turn-invalid-source");
    let session_id = state.session_id.clone();
    let successor = append_successor_frame(&mut state);
    let missing = lash_core_execution::FrameNodeId::new("not-appended").expect("frame id");
    let commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_frame_transition(frame_transition(&session_id, missing, successor));
    assert!(matches!(
        storage.session_store(session_id.clone()).commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    assert_no_frame_commit_rows(&storage, &session_id).await;
}

#[tokio::test]
async fn postgres_later_commit_rejects_transition_source_other_than_head_frame() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let state = frame_state("later-invalid-source");
    let session_id = state.session_id.clone();
    let store = storage.session_store(session_id.clone());
    store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect("commit initial frame");
    let mut state = lash_core_execution::store::load_persisted_session_state(&store)
        .await
        .expect("load state")
        .expect("persisted state");
    let prior_revision = state.head_revision;
    let successor = append_successor_frame(&mut state);
    let wrong = lash_core_execution::FrameNodeId::new("wrong-ended-frame").expect("frame id");
    let commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_frame_transition(frame_transition(&session_id, wrong, successor));
    assert!(matches!(
        store.commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    let head = store
        .load_session()
        .await
        .expect("read head")
        .expect("head exists");
    assert_eq!(head.head_revision, prior_revision);
}

#[tokio::test]
async fn postgres_first_commit_rejects_transition_successor_other_than_committed_frame() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("first-turn-invalid-successor");
    let session_id = state.session_id.clone();
    let ended = state.current_frame_node_id.clone().expect("initial frame");
    append_successor_frame(&mut state);
    let wrong = lash_core_execution::FrameNodeId::new("wrong-successor").expect("frame id");
    let commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_frame_transition(frame_transition(&session_id, ended, wrong));
    assert!(matches!(
        storage.session_store(session_id.clone()).commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    assert_no_frame_commit_rows(&storage, &session_id).await;
}

/// Commit every pending frame open in `state`, with `transition` attached.
async fn commit_frame_opens(
    store: &impl SessionCommitStore,
    state: &mut lash_core_execution::RuntimeSessionState,
    transition: Option<lash_core_execution::store::FrameTransition>,
) {
    let mut commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(state, &[]);
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
) -> lash_core_execution::FrameEnvironmentId {
    lash_core::runtime::state::open_agent_frame_in_state_with_clock(
        state,
        lash_core_execution::OpenAgentFrameRequest::new(
            lash_core_execution::FrameKey::from_caller_material(key).expect("frame key"),
            lash_core_execution::AgentFrameReason::new("test"),
        ),
        &lash_core::testing::TestClock::new(1_000),
    )
    .expect("open frame");
    current_frame(state)
}

fn current_frame(
    state: &lash_core_execution::RuntimeSessionState,
) -> lash_core_execution::FrameEnvironmentId {
    lash_core_execution::FrameEnvironmentId::new(
        state.session_id.clone(),
        state.current_frame_node_id.clone().expect("current frame"),
    )
}

/// Whether `frame` is fenced, and the cleanup record that will sever its
/// edges, if one exists.
async fn frame_end(
    storage: &PostgresStorage,
    frame: &lash_core_execution::FrameEnvironmentId,
) -> (bool, Option<ArtifactCleanup>) {
    let referrer = ArtifactReferrer::FrameEnvironment(frame.clone());
    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_artifact_referrer_fences
         WHERE referrer_kind = $1 AND referrer_id = $2)",
    )
    .bind(referrer.kind().as_str())
    .bind(referrer.canonical_id())
    .fetch_one(storage.pool())
    .await
    .expect("read frame fence");
    let cleanup: Option<String> = sqlx::query_scalar(
        "SELECT cleanup_json FROM lash_artifact_cleanup_obligations
         WHERE referrer_kind = $1 AND referrer_id = $2",
    )
    .bind(referrer.kind().as_str())
    .bind(referrer.canonical_id())
    .fetch_optional(storage.pool())
    .await
    .expect("read frame cleanup");
    (
        fenced,
        cleanup.map(|body| ArtifactCleanup::from_json(&body, &referrer).expect("decode cleanup")),
    )
}

/// FIG-4031 corner (a) on PostgreSQL: a turn admitted on a frame opened only
/// in resident state after a committed frame switches away from it. Its one
/// final commit leaves both frames, and neither may keep its edges.
#[tokio::test]
async fn postgres_switch_out_of_a_resident_frame_ends_every_frame_the_commit_leaves() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("resident-frame-switch");
    let store = storage.session_store(state.session_id.clone());
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let resident = open_resident_frame(&mut state, "resident");
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(resident.clone()))
        .expect("frame claim");
    storage
        .lashlang_artifact_store()
        .publish_module_artifact(&claim, "held-by-resident-frame", b"bytes")
        .await
        .expect("hold module under the resident frame");
    let successor = open_resident_frame(&mut state, "successor");
    // The transition a switch out of an uncommitted frame names: the last
    // committed frame, with no carries.
    let transition = lash_core_execution::store::FrameTransition {
        ended: first.clone(),
        successor: successor.clone(),
        carries: Vec::new(),
        gate: lash_sansio::ExecutionScope::runtime_operation("postgres-frame-switch")
            .journal_identity()
            .expect("journal identity"),
    };
    let gate = transition.gate.clone();
    commit_frame_opens(&store, &mut state, Some(transition)).await;

    for frame in [&first, &resident] {
        assert_eq!(
            frame_end(&storage, frame).await,
            (
                true,
                Some(ArtifactCleanup::ended(
                    ArtifactReferrer::FrameEnvironment(frame.clone()),
                    Vec::new(),
                    Some(gate.clone()),
                ))
            ),
            "{frame:?} must be fenced and owe a gated cleanup that severs its edges"
        );
    }
    assert_eq!(frame_end(&storage, &successor).await, (false, None));
    assert!(matches!(
        storage
            .lashlang_artifact_store()
            .publish_module_artifact(&claim, "held-by-resident-frame", b"bytes")
            .await,
        Err(lash_core_execution::ArtifactStoreError::ReferrerEnded { .. })
    ));
}

/// FIG-4031 corner (b) on PostgreSQL: a frame opened directly in resident
/// state is committed by a park or a session command, so no transition rides
/// the commit. The frame it leaves ends anyway, ungated.
#[tokio::test]
async fn postgres_commit_without_a_transition_that_changes_the_frame_ends_the_frame_it_leaves() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("park-frame-switch");
    let store = storage.session_store(state.session_id.clone());
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(first.clone()))
        .expect("frame claim");
    storage
        .lashlang_artifact_store()
        .publish_module_artifact(&claim, "held-by-first-frame", b"bytes")
        .await
        .expect("hold module under the first frame");
    let opened = open_resident_frame(&mut state, "opened-directly");
    commit_frame_opens(&store, &mut state, None).await;

    assert_eq!(
        frame_end(&storage, &first).await,
        (
            true,
            Some(ArtifactCleanup::ended(
                ArtifactReferrer::FrameEnvironment(first.clone()),
                Vec::new(),
                None,
            ))
        ),
        "the frame a park leaves must be fenced and owe a cleanup that severs its edges"
    );
    assert_eq!(frame_end(&storage, &opened).await, (false, None));
}

/// FIG-4031 corner (c) on PostgreSQL: a `continue_as` seed carrying a forged
/// process value names a module the frame holds no edge of, either never
/// stored or held only by another referrer. The switch fails closed with
/// `ArtifactCarryMissing` and writes nothing.
#[tokio::test]
async fn postgres_switch_carrying_a_module_its_frame_does_not_hold_fails_closed() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("forged-carry");
    let store = storage.session_store(state.session_id.clone());
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let (_, pinned) = host_pin();
    storage
        .lashlang_artifact_store()
        .publish_module_artifact(&pinned, "held-by-a-host-pin", b"bytes")
        .await
        .expect("hold module under a host pin");
    let head_revision = state.head_revision;
    let successor = open_resident_frame(&mut state, "successor");
    for forged in ["never-stored", "held-by-a-host-pin"] {
        let mut transition = frame_transition(
            &state.session_id,
            first.frame_node_id().clone(),
            successor.frame_node_id().clone(),
        );
        transition.carries = vec![lash_core_execution::ArtifactName {
            store: lash_core_execution::ArtifactStoreId::LashlangModule,
            artifact_ref: forged.into(),
        }];
        let commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_frame_transition(transition);
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
        assert_eq!(frame_end(&storage, &first).await, (false, None), "{forged}");
        let head = store
            .load_session()
            .await
            .expect("read head")
            .expect("head exists");
        assert_eq!(head.head_revision, head_revision, "{forged}");
    }
}

fn host_pin() -> (ArtifactReferrer, ReferrerClaim) {
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("host pin is unguarded");
    (referrer, claim)
}

fn end(referrer: ArtifactReferrer) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

async fn lock_artifact_mutations<'a>(
    storage: &'a PostgresStorage,
    artifact_ref: &str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:lashlang_module:{artifact_ref}"))
        .execute(&mut *tx)
        .await
        .expect("lock artifact mutation key");
    tx
}

async fn wait_until_a_mutation_waits(storage: &PostgresStorage) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
             WHERE pid <> pg_backend_pid() AND datname = current_database()
               AND state = 'active' AND wait_event_type = 'Lock'
               AND query LIKE '%pg_advisory_xact_lock%')",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock wait");
        if waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mutation did not reach its artifact lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_referrer_end_preserves_an_edge_committed_ahead_of_it() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (a, a_claim) = host_pin();
    let (b, _) = host_pin();
    let artifact_ref = "artifact-race-preserve";
    store
        .publish_module_artifact(&a_claim, artifact_ref, b"bytes")
        .await
        .expect("publish first edge");

    let mut publisher = lock_artifact_mutations(&storage, artifact_ref).await;
    sqlx::query(
        "INSERT INTO lash_artifact_referrer_edges
        (namespace, artifact_ref, referrer_kind, referrer_id)
        VALUES ('lashlang_module', $1, $2, $3)",
    )
    .bind(artifact_ref)
    .bind(b.kind().as_str())
    .bind(b.canonical_id())
    .execute(&mut *publisher)
    .await
    .expect("stage second edge");

    let ending = store.clone();
    let end_a = tokio::spawn(async move { ending.end_module_referrer(&end(a)).await });
    wait_until_a_mutation_waits(&storage).await;
    publisher.commit().await.expect("commit second edge");
    end_a.await.expect("join end").expect("end first referrer");
    assert_eq!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read bytes"),
        Some(b"bytes".to_vec())
    );
    store
        .end_module_referrer(&end(b))
        .await
        .expect("end second referrer");
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_final_referrer_ends_reclaim_bytes() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (a, a_claim) = host_pin();
    let (b, b_claim) = host_pin();
    let artifact_ref = "artifact-race-final";
    store
        .publish_module_artifact(&a_claim, artifact_ref, b"bytes")
        .await
        .expect("publish");
    store
        .acquire_module_artifact(&b_claim, artifact_ref)
        .await
        .expect("acquire");
    let blocker = lock_artifact_mutations(&storage, artifact_ref).await;
    let left_store = store.clone();
    let left = tokio::spawn(async move { left_store.end_module_referrer(&end(a)).await });
    let right_store = store.clone();
    let right = tokio::spawn(async move { right_store.end_module_referrer(&end(b)).await });
    wait_until_a_mutation_waits(&storage).await;
    blocker.commit().await.expect("release lock");
    left.await
        .expect("join first end")
        .expect("end first referrer");
    right
        .await
        .expect("join second end")
        .expect("end second referrer");
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_referrer_fence_refuses_a_late_publisher() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (referrer, claim) = host_pin();
    let artifact_ref = "artifact-race-late";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish");
    let blocker = lock_artifact_mutations(&storage, artifact_ref).await;
    let ending = store.clone();
    let retirement = tokio::spawn(async move { ending.end_module_referrer(&end(referrer)).await });
    wait_until_a_mutation_waits(&storage).await;
    let publishing = store.clone();
    let late = tokio::spawn(async move {
        publishing
            .publish_module_artifact(&claim, artifact_ref, b"bytes")
            .await
    });
    blocker.commit().await.expect("release lock");
    retirement.await.expect("join end").expect("fence referrer");
    assert!(matches!(
        late.await.expect("join late publish"),
        Err(lash_core_execution::ArtifactStoreError::ReferrerEnded { .. })
    ));
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}

#[tokio::test]
async fn postgres_ended_cleanup_arms_a_fence_before_delivery() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (referrer, claim) = host_pin();
    let artifact_ref = "artifact-host-pin-release";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish under live host pin");
    lash_core_execution::store::ArtifactCleanupLedger::arm_cleanup(
        storage.artifact_cleanup().as_ref(),
        &ArtifactCleanup::ended(referrer, Vec::new(), None),
        42,
    )
    .await
    .expect("arm ended cleanup");
    assert!(matches!(
        store
            .publish_module_artifact(&claim, artifact_ref, b"bytes")
            .await,
        Err(lash_core_execution::ArtifactStoreError::ReferrerEnded { .. })
    ));
}

#[tokio::test]
async fn postgres_artifact_read_refuses_an_undecodable_referrer_id() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (_, claim) = host_pin();
    let artifact_ref = "artifact-corrupt-edge";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish under live host pin");
    let empty_id = sqlx::query(
        "UPDATE lash_artifact_referrer_edges SET referrer_id = ''
         WHERE namespace = 'lashlang_module' AND artifact_ref = $1",
    )
    .bind(artifact_ref)
    .execute(storage.pool())
    .await;
    assert!(
        empty_id.is_err(),
        "an empty stored referrer id must fail CHECK"
    );
    sqlx::query(
        "UPDATE lash_artifact_referrer_edges SET referrer_id = 'invalid-host-pin'
         WHERE namespace = 'lashlang_module' AND artifact_ref = $1",
    )
    .bind(artifact_ref)
    .execute(storage.pool())
    .await
    .expect("inject undecodable stored referrer");
    let error = store
        .get_module_artifact(artifact_ref)
        .await
        .expect_err("corrupt edge must refuse the read");
    assert!(
        error.to_string().contains("data is corrupt"),
        "wrong read error: {error}"
    );
}
