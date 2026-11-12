use super::*;
use lash_core_store::artifact_referrer::ArtifactCleanup;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::HostArtifactPin;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;

/// An admitted view of `session_id` on `storage`'s catalog.
async fn session_store(
    storage: &PostgresStorage,
    session_id: SessionId,
) -> lash_core_store::store::SessionStore {
    let store: Arc<dyn lash_execution_contracts::deployment::DeploymentStore> =
        Arc::new(storage.store());
    lash_execution_contracts::deployment::admit_session_view(
        &store,
        &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
    )
    .await
    .expect("admit the session")
}

fn frame_state(session_id: &str) -> lash_core_store::session_state::RuntimeSessionState {
    let mut state = lash_core_store::session_state::RuntimeSessionState {
        session_id: SessionId::fixture(session_id),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    state.ensure_agent_frame_initialized();
    state
}

fn append_successor_frame(
    state: &mut lash_core_store::session_state::RuntimeSessionState,
) -> lash_core_store::session_identity::FrameNodeId {
    // The commit derives a frame's node id from its key, so the successor
    // carries that id, the one the committed head will name.
    let key =
        lash_protocol_types::FrameKey::from_caller_material("successor-frame").expect("frame key");
    let successor = lash_core_store::session_graph::frame_node_id(&state.session_id, key.as_str());
    assert!(
        state.session_graph.append_frame_open_with_id_at(
            successor.clone(),
            key,
            lash_core_store::session_identity::AgentFrameReason::initial(),
            lash_core_store::session_identity::AgentFrameAssignment::unconfigured(
                state.policy.clone()
            ),
            "2026-09-29T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
        )
    );
    state.current_frame_node_id = Some(successor.clone());
    successor
}

fn frame_transition(
    session_id: &SessionId,
    ended: lash_core_store::session_identity::FrameNodeId,
    successor: lash_core_store::session_identity::FrameNodeId,
) -> lash_core_store::store::FrameTransition {
    lash_core_store::store::FrameTransition {
        ended: lash_core_store::artifact_referrer::FrameEnvironmentId::new(
            session_id.clone(),
            ended,
        ),
        successor: lash_core_store::artifact_referrer::FrameEnvironmentId::new(
            session_id.clone(),
            successor,
        ),
        carries: Vec::new(),
        gate: lash_protocol_types::ExecutionScope::runtime_operation("postgres-frame-switch")
            .journal_identity()
            .expect("journal identity"),
    }
}

/// `(head_revision, config_ref, checkpoint_ref, leaf_node_id)`, in the order
/// `committed_rows` selects them.
type SessionHeadRow = (i64, String, Option<String>, Option<String>);

/// A session's committed rows as admission leaves them: the catalog row and
/// the created head row written in the same transaction (FIG-4561). A
/// refused commit must leave every one identical.
#[derive(Debug, PartialEq)]
struct AdmittedRows {
    /// `lash_session_head`' head row beside the key.
    head: Option<SessionHeadRow>,
    graph_nodes: i64,
    session_meta: i64,
}

/// The session's committed rows as they stand; snapshot them right after
/// admission to get `admitted`.
async fn committed_rows(storage: &PostgresStorage, session_id: &SessionId) -> AdmittedRows {
    let head = sqlx::query_as(
        "SELECT head_revision, encode((SELECT hash FROM lash_blobs WHERE id = config_ref), 'hex'), encode((SELECT hash FROM lash_blobs WHERE id = checkpoint_ref), 'hex'), leaf_node_id
         FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision) WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_optional(storage.pool())
    .await
    .expect("read the session's head row");
    let graph_nodes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_graph_nodes WHERE session_id = $1")
            .bind(session_id.as_str())
            .fetch_one(storage.pool())
            .await
            .expect("count graph rows");
    let session_meta: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_session_meta WHERE session_id = $1")
            .bind(session_id.as_str())
            .fetch_one(storage.pool())
            .await
            .expect("count catalog rows");
    AdmittedRows {
        head,
        graph_nodes,
        session_meta,
    }
}

/// The refused commit changed nothing the admission wrote: the head row is
/// still admission's created head (revision 0, `is_created`), and no fence
/// or cleanup obligation exists.
async fn assert_no_frame_commit_rows(
    storage: &PostgresStorage,
    session_id: &SessionId,
    admitted: &AdmittedRows,
) {
    let after = committed_rows(storage, session_id).await;
    assert_eq!(
        &after, admitted,
        "the refused commit changed the admitted rows"
    );
    let (head_revision, _, checkpoint_ref, leaf_node_id) = after
        .head
        .as_ref()
        .expect("admission wrote the created head");
    assert!(
        *head_revision == 0 && checkpoint_ref.is_none() && leaf_node_id.is_none(),
        "the head row is still admission's created head"
    );
    let fences: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_referrer_fences")
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
        lash_core_store::artifact_referrer::FrameEnvironmentId::new(
            session_id.clone(),
            ended.clone(),
        ),
    );
    let claim = ReferrerClaim::unguarded(ended_referrer.clone()).expect("frame claim");
    let artifacts = storage.lash_vm_artifact_store();
    artifacts
        .publish_module_artifact(&claim, "first-turn-module", b"bytes")
        .await
        .expect("publish under first frame");
    let mut state = state;
    let successor = append_successor_frame(&mut state);
    let transition = frame_transition(&session_id, ended, successor);
    let gate = transition.gate.clone();
    let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state)
        .with_frame_transition(transition);
    session_store(&storage, session_id)
        .await
        .commit_runtime_state(commit)
        .await
        .expect("commit first-turn frame switch");

    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_referrer_fences
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
        cleanup,
        lash_core_store::artifact_referrer::ArtifactCleanup::Ended { .. }
    ));
    assert!(matches!(
        artifacts
            .publish_module_artifact(&claim, "first-turn-module", b"bytes")
            .await,
        Err(lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { .. })
    ));
    assert_eq!(
        artifacts
            .get_module_artifact("first-turn-module")
            .await
            .expect("read gated bytes"),
        Some(b"bytes".to_vec()),
        "the unfinished gate retains the source frame's bytes"
    );
    // Exercise the same helper used by terminal owner commits, with the
    // typed gate key rather than a serialized-body search pattern.
    let answer: serde_json::Value = sqlx::query_scalar(
        "SELECT lash_artifact_end_local('execution', $1, $1, 124,
             ARRAY(SELECT DISTINCT namespace FROM lash_artifact_referrer_edges))",
    )
    .bind(gate.key())
    .fetch_one(storage.pool())
    .await
    .expect("settle the switching execution's SQL share");
    assert_eq!(answer["plans"].as_array().expect("gated plans").len(), 1);
    assert_eq!(
        artifacts
            .get_module_artifact("first-turn-module")
            .await
            .expect("read after gate settlement"),
        None,
        "gate settlement reclaims its frame's SQL artifacts without a due pass"
    );
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
    let missing =
        lash_core_store::session_identity::FrameNodeId::new("not-appended").expect("frame id");
    let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state)
        .with_frame_transition(frame_transition(&session_id, missing, successor));
    let store = session_store(&storage, session_id.clone()).await;
    let admitted = committed_rows(&storage, &session_id).await;
    assert!(matches!(
        store.commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    assert_no_frame_commit_rows(&storage, &session_id, &admitted).await;
}

#[tokio::test]
async fn postgres_later_commit_rejects_transition_source_other_than_head_frame() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let state = frame_state("later-invalid-source");
    let session_id = state.session_id.clone();
    let store = session_store(&storage, session_id.clone()).await;
    store
        .commit_runtime_state(
            lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state),
        )
        .await
        .expect("commit initial frame");
    let mut state = lash_core_store::store::load_session_window_state(
        &store,
        lash_core_store::store::WindowSelector::Current,
    )
    .await
    .expect("load state")
    .expect("persisted state")
    .state;
    let prior_revision = state.head_revision;
    let successor = append_successor_frame(&mut state);
    let wrong =
        lash_core_store::session_identity::FrameNodeId::new("wrong-ended-frame").expect("frame id");
    let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state)
        .with_frame_transition(frame_transition(&session_id, wrong, successor));
    assert!(matches!(
        store.commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    let head = store
        .load_session_head_meta()
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
    let wrong =
        lash_core_store::session_identity::FrameNodeId::new("wrong-successor").expect("frame id");
    let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state)
        .with_frame_transition(frame_transition(&session_id, ended, wrong));
    let store = session_store(&storage, session_id.clone()).await;
    let admitted = committed_rows(&storage, &session_id).await;
    assert!(matches!(
        store.commit_runtime_state(commit).await,
        Err(StoreError::Backend(message)) if message == "frame transition does not match the committed head"
    ));
    assert_no_frame_commit_rows(&storage, &session_id, &admitted).await;
}

/// Commit every pending frame open in `state`, with `transition` attached.
async fn commit_frame_opens(
    store: &lash_core_store::store::SessionStore,
    state: &mut lash_core_store::session_state::RuntimeSessionState,
    transition: Option<lash_core_store::store::FrameTransition>,
) {
    let mut commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(state);
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
    state: &mut lash_core_store::session_state::RuntimeSessionState,
    key: &str,
) -> lash_core_store::artifact_referrer::FrameEnvironmentId {
    lash_core::runtime::state::open_agent_frame_in_state_with_clock(
        state,
        lash_core_store::session_identity::OpenAgentFrameRequest::new(
            lash_protocol_types::FrameKey::from_caller_material(key).expect("frame key"),
            lash_core_store::session_identity::AgentFrameReason::new("test"),
        ),
        &lash_core::testing::TestClock::new(1_000),
    )
    .expect("open frame");
    current_frame(state)
}

fn current_frame(
    state: &lash_core_store::session_state::RuntimeSessionState,
) -> lash_core_store::artifact_referrer::FrameEnvironmentId {
    lash_core_store::artifact_referrer::FrameEnvironmentId::new(
        state.session_id.clone(),
        state.current_frame_node_id.clone().expect("current frame"),
    )
}

/// Whether `frame` is fenced, and the cleanup record that will sever its
/// edges, if one exists.
async fn frame_end(
    storage: &PostgresStorage,
    frame: &lash_core_store::artifact_referrer::FrameEnvironmentId,
) -> (bool, Option<ArtifactCleanup>) {
    let referrer = ArtifactReferrer::FrameEnvironment(frame.clone());
    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_referrer_fences
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
    let store = session_store(&storage, state.session_id.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let resident = open_resident_frame(&mut state, "resident");
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(resident.clone()))
        .expect("frame claim");
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(&claim, "held-by-resident-frame", b"bytes")
        .await
        .expect("hold module under the resident frame");
    let successor = open_resident_frame(&mut state, "successor");
    // The transition a switch out of an uncommitted frame names: the last
    // committed frame, with no carries.
    let transition = lash_core_store::store::FrameTransition {
        ended: first.clone(),
        successor: successor.clone(),
        carries: Vec::new(),
        gate: lash_protocol_types::ExecutionScope::runtime_operation("postgres-frame-switch")
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
            .lash_vm_artifact_store()
            .publish_module_artifact(&claim, "held-by-resident-frame", b"bytes")
            .await,
        Err(lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { .. })
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
    let store = session_store(&storage, state.session_id.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(first.clone()))
        .expect("frame claim");
    storage
        .lash_vm_artifact_store()
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
    let store = session_store(&storage, state.session_id.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let (_, pinned) = host_pin();
    storage
        .lash_vm_artifact_store()
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
        transition.carries = vec![lash_core_store::artifact_referrer::ArtifactName {
            store: lash_core_store::artifact_referrer::ArtifactStoreId::KernelDocument,
            artifact_ref: forged.into(),
        }];
        let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state)
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
            .load_session_head_meta()
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

/// Hold `artifact_ref`'s mutation lock in an open transaction. Returns the
/// transaction and its backend pid, which `wait_until_mutations_wait`
/// names as the holder.
async fn lock_artifact_mutations<'a>(
    storage: &'a PostgresStorage,
    artifact_ref: &str,
) -> (sqlx::Transaction<'a, sqlx::Postgres>, i32) {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:kernel_document:{artifact_ref}"))
        .execute(&mut *tx)
        .await
        .expect("lock artifact mutation key");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .expect("read blocker pid");
    (tx, pid)
}

/// Wait until `count` other backends wait for the advisory lock `holder`
/// holds. The lock table names the wait itself, so it is seen whether the
/// client statement takes the lock or a server function it calls does.
async fn wait_until_mutations_wait(storage: &PostgresStorage, holder: i32, count: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT waiting.pid) >= $2 FROM pg_locks waiting
             JOIN pg_locks held ON held.locktype = waiting.locktype
               AND held.database = waiting.database AND held.classid = waiting.classid
               AND held.objid = waiting.objid AND held.objsubid = waiting.objsubid
             WHERE waiting.locktype = 'advisory' AND NOT waiting.granted
               AND waiting.pid <> $1 AND held.pid = $1 AND held.granted",
        )
        .bind(holder)
        .bind(count)
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
    let store = storage.lash_vm_artifact_store();
    let (a, a_claim) = host_pin();
    let (b, _) = host_pin();
    let artifact_ref = "artifact-race-preserve";
    store
        .publish_module_artifact(&a_claim, artifact_ref, b"bytes")
        .await
        .expect("publish first edge");

    let (mut publisher, holder) = lock_artifact_mutations(&storage, artifact_ref).await;
    sqlx::query(
        "INSERT INTO lash_artifact_referrer_edges
        (namespace, artifact_ref, referrer_kind, referrer_id)
        VALUES ('kernel_document', $1, $2, $3)",
    )
    .bind(artifact_ref)
    .bind(b.kind().as_str())
    .bind(b.canonical_id())
    .execute(&mut *publisher)
    .await
    .expect("stage second edge");

    let ending = store.clone();
    let end_a = tokio::spawn(async move { ending.end_module_referrer(&end(a)).await });
    wait_until_mutations_wait(&storage, holder, 1).await;
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
    let store = storage.lash_vm_artifact_store();
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
    let (blocker, holder) = lock_artifact_mutations(&storage, artifact_ref).await;
    let left_store = store.clone();
    let left = tokio::spawn(async move { left_store.end_module_referrer(&end(a)).await });
    let right_store = store.clone();
    let right = tokio::spawn(async move { right_store.end_module_referrer(&end(b)).await });
    // Both ends wait on the artifact lock, so they race for the last edge.
    wait_until_mutations_wait(&storage, holder, 2).await;
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
    let store = storage.lash_vm_artifact_store();
    let (referrer, claim) = host_pin();
    let artifact_ref = "artifact-race-late";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish");
    let (blocker, holder) = lock_artifact_mutations(&storage, artifact_ref).await;
    let ending = store.clone();
    let retirement = tokio::spawn(async move { ending.end_module_referrer(&end(referrer)).await });
    wait_until_mutations_wait(&storage, holder, 1).await;
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
        Err(lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { .. })
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
    let store = storage.lash_vm_artifact_store();
    let (referrer, claim) = host_pin();
    let artifact_ref = "artifact-host-pin-release";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish under live host pin");
    lash_core_store::store::ArtifactCleanupLedger::arm_cleanup(
        storage.artifact_cleanup().as_ref(),
        &ArtifactCleanup::ended(referrer, Vec::new(), None),
        42,
    )
    .await
    .expect("arm ended cleanup");
    let edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_artifact_referrer_edges WHERE artifact_ref = $1",
    )
    .bind(artifact_ref)
    .fetch_one(storage.pool())
    .await
    .expect("read ended pin edges");
    assert_eq!(
        edges, 0,
        "the end transaction severs local edges before relay delivery"
    );
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read ended pin bytes")
            .is_none(),
        "the same transaction reclaims bytes with no other referrer"
    );
    assert!(matches!(
        store
            .publish_module_artifact(&claim, artifact_ref, b"bytes")
            .await,
        Err(lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { .. })
    ));
}

#[tokio::test]
async fn postgres_stored_referrer_ids_are_never_empty() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lash_vm_artifact_store();
    let (_, claim) = host_pin();
    let artifact_ref = "artifact-empty-edge";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish under live host pin");
    let empty_id = sqlx::query(
        "UPDATE lash_artifact_referrer_edges SET referrer_id = ''
         WHERE namespace = 'kernel_document' AND artifact_ref = $1",
    )
    .bind(artifact_ref)
    .execute(storage.pool())
    .await;
    assert!(
        empty_id.is_err(),
        "an empty stored referrer id must fail CHECK"
    );
}

#[tokio::test]
async fn cleanup_kind_mismatch_is_stored_corruption() {
    let Some((_fixture, storage)) = storage().await else {
        panic!("PostgreSQL law requires a database");
    };
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let ledger = storage.artifact_cleanup();
    let id = ledger
        .arm_cleanup(&ArtifactCleanup::ended(referrer.clone(), vec![], None), 1)
        .await
        .expect("arm");
    for body in [
        serde_json::json!({"referrer": referrer, "plan": {"plan": "await_journal"}, "gate": null})
            .to_string(),
        r#"{"plan":"await_journal"}"#.to_owned(),
    ] {
        sqlx::query("UPDATE lash_artifact_cleanup_obligations SET cleanup_json = $1 WHERE obligation_id = $2").bind(body).bind(id.as_str()).execute(storage.pool()).await.expect("inject mismatched guard");
        let result = ledger.load_cleanup(&id).await;
        assert!(
            matches!(result, Err(StoreError::StoredDataCorrupt { .. })),
            "mismatched guard must be corrupt: {result:?}"
        );
    }
}

/// Publish, under `claim`, an environment over one link to document
/// `round`, with the document and the link: the environment's reference,
/// its bytes and the document's reference.
async fn environment_over_document(
    storage: &PostgresStorage,
    claim: &ReferrerClaim,
    round: u8,
) -> (
    lash_execution_contracts::process::ProcessEnvRef,
    Vec<u8>,
    String,
) {
    use lash_durable::packages::PackageStore as _;
    use lash_execution_contracts::process::ProcessEnvStore as _;

    let store = storage.lash_vm_artifact_store();
    let document = lash_kernel_value::names::DocumentId::from_bytes([round; 32]);
    store
        .publish_module_artifact(claim, &document.to_string(), b"linked document")
        .await
        .expect("publish the document");
    let link = lash_kernel_value::package::Link {
        helper_fills: Default::default(),
        document,
        fills: Default::default(),
    };
    let link = lash_core_store::packages::LinkRecord {
        id: link.identity(),
        link,
    };
    store.put_link(claim, &link, &[]).await.expect("link");
    let mut execution = lash_core_store::tool_surface::ExecutionEnvironment::empty(
        lash_vocabulary::claims::RegistryKind::Process,
    );
    execution.authored.insert(link.id);
    let env = lash_core_store::process_env::ProcessEnv::builder(
        lash_protocol_types::session_model::MaxToolCalls::new(1024),
    )
    .execution(execution)
    .build()
    .expect("environment");
    let env_ref = env.stable_ref().expect("environment reference");
    let bytes = env.to_store_bytes().expect("environment bytes");
    store
        .publish_process_env(claim, &env_ref, &bytes)
        .await
        .expect("publish the environment");
    (env_ref, bytes, document.to_string())
}

/// The closure that holds `document`.
async fn closure_holding(storage: &PostgresStorage, document: &str) -> String {
    sqlx::query_scalar(
        "SELECT holder_ref FROM lash_artifact_dependency_edges
         WHERE holder_namespace = 'env_closure' AND namespace = 'kernel_document'
           AND artifact_ref = $1",
    )
    .bind(document)
    .fetch_one(storage.pool())
    .await
    .expect("read the closure holding the document")
}

/// One head commit that leaves two frames takes every artifact lock of both
/// ends in one sorted acquisition, so it never waits on a publisher's
/// sorted set while holding a lock that set needs (FIG-7700). Frame `first`
/// holds module `M` directly; frame `closures` holds two environments
/// through their closures, the later `C` holding `M`, the earlier `X`
/// sorting first. With `X` held, the commit stalls inside the ends; a
/// publisher under an unrelated pin republishes `C`'s environment, which
/// locks `C`, then `M`. Ending `first` before taking `closures`' locks
/// leaves the commit holding `M` while the publisher holds `C`: a deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_frames_one_commit_leaves_never_deadlock_a_closure_publisher() {
    use lash_execution_contracts::process::ProcessEnvStore as _;

    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let mut state = frame_state("two-frame-end-lock-order");
    let store = session_store(&storage, state.session_id.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let first = current_frame(&state);
    let closures = open_resident_frame(&mut state, "closures");
    let closures_claim =
        ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(closures.clone()))
            .expect("frame claim");
    let mut published = Vec::new();
    for round in [41, 42] {
        let (env_ref, bytes, document) =
            environment_over_document(&storage, &closures_claim, round).await;
        let closure = closure_holding(&storage, &document).await;
        published.push((closure, env_ref, bytes, document));
    }
    published.sort_by(|left, right| left.0.cmp(&right.0));
    let (earlier, _, _, _) = &published[0];
    let (_, later_env, later_bytes, module) = published[1].clone();
    let first_claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(first.clone()))
        .expect("frame claim");
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(&first_claim, &module, b"linked document")
        .await
        .expect("first frame holds the module directly");
    open_resident_frame(&mut state, "head");
    let commit = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state);

    let mut blocker = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:env_closure:{earlier}"))
        .execute(&mut *blocker)
        .await
        .expect("hold the earlier closure's lock");
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .expect("read blocker pid");
    let committing = tokio::spawn({
        let store = store.clone();
        async move { store.commit_runtime_state(commit).await }
    });
    wait_until_mutations_wait(&storage, holder, 1).await;
    let publishing = tokio::spawn({
        let artifacts = storage.lash_vm_artifact_store();
        let (_, pin) = host_pin();
        async move {
            artifacts
                .publish_process_env(&pin, &later_env, &later_bytes)
                .await
        }
    });
    // The publisher completes, or waits on a lock the commit holds.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock waits");
        if publishing.is_finished() || waits >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "publisher neither finished nor waited"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    blocker.commit().await.expect("release the earlier closure");
    let within = std::time::Duration::from_secs(60);
    let committed = tokio::time::timeout(within, committing)
        .await
        .expect("the commit finishes")
        .expect("commit task");
    let republished = tokio::time::timeout(within, publishing)
        .await
        .expect("the publisher finishes")
        .expect("publish task");
    assert!(committed.is_ok(), "head commit: {committed:?}");
    assert!(republished.is_ok(), "publisher: {republished:?}");
    for frame in [&first, &closures] {
        assert_eq!(
            frame_end(&storage, frame).await,
            (
                true,
                Some(ArtifactCleanup::ended(
                    ArtifactReferrer::FrameEnvironment(frame.clone()),
                    Vec::new(),
                    None,
                ))
            ),
            "{frame:?}"
        );
    }
}

/// Claim `session`'s actor for owner writes, as a node claims it: its key
/// and epoch. Heads committed through the store path come first.
async fn claim_owner(
    storage: &PostgresStorage,
    session: &SessionId,
) -> (lash_durable::ActorKey, lash_durable::Epoch) {
    claim_owners(storage, std::slice::from_ref(session)).await[0].clone()
}

/// Claim every session's actor on one node, in one claim, each matched to
/// its own epoch: a claim takes any ready actor, and a second registration
/// of the node would re-claim the first actor under a newer epoch.
async fn claim_owners(
    storage: &PostgresStorage,
    sessions: &[SessionId],
) -> Vec<(lash_durable::ActorKey, lash_durable::Epoch)> {
    use lash_durable::DurableStore as _;
    let store = storage.durable_store();
    let node = store
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("lock-plan-owner"),
            decodes: vec![
                lash_durable::FormatSet::unstarted_session(),
                lash_durable::FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS),
            ],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 60_000,
        })
        .await
        .expect("register the owner's node");
    let actors: Vec<_> = sessions
        .iter()
        .map(|session| lash_durable::ActorKey::session(session.as_str()).expect("actor key"))
        .collect();
    let mut wake = lash_durable::MailTx::new();
    for actor in &actors {
        wake.wake(actor.clone());
    }
    store
        .commit_mail(wake, lash_durable::CommitLabel::new("law.wake"))
        .await
        .expect("wake the actors");
    let claimed = store
        .claim(&node, actors.len())
        .await
        .expect("claim the actors");
    actors
        .into_iter()
        .map(|actor| {
            let epoch = claimed
                .iter()
                .find(|claim| claim.actor == actor)
                .unwrap_or_else(|| panic!("{actor:?} was not claimed"))
                .epoch;
            (actor, epoch)
        })
        .collect()
}

/// Write `commit` on `tx` as its owner's head commit.
fn write_head(tx: &mut lash_durable::ActorTx, commit: &lash_core_store::store::RuntimeCommit) {
    let encoded = lash_core_store::store::encode_session_commit(commit).expect("encode the head");
    match encoded.descriptor {
        Some(descriptor) => {
            tx.stage([descriptor]);
        }
        None => {
            if let Some(root) = encoded.execution {
                tx.require(root);
            }
        }
    }
    tx.write(lash_durable::DomainWrite::SessionCommit(
        lash_durable::domain::SessionCommitWrite {
            session: commit.session_id.clone(),
            expected_head: commit.expected_head_revision,
            commit_json: encoded.json,
            execution: encoded.execution,
        },
    ));
}

/// A claim of session operation `operation`'s execution.
fn operation_claim(session: &SessionId, operation: &str) -> ReferrerClaim {
    ReferrerClaim::guarded(lash_core_store::artifact_referrer::ReferrerGuard::Journal(
        lash_protocol_types::ExecutionScope::session_operation(session.clone(), operation)
            .journal_identity()
            .expect("operation journal"),
    ))
}

/// The write that ends session operation `operation`'s execution.
fn operation_end(session: &SessionId, operation: &str) -> lash_durable::DomainWrite {
    lash_durable::DomainWrite::Process(lash_durable::domain::ProcessWrite::ScopeClosed {
        scope: lash_durable::domain::ScopeKey::Operation(session.clone(), operation.to_owned()),
    })
}

/// Hold advisory lock `key` in an open transaction: the transaction and
/// its backend pid.
async fn hold_lock(
    storage: &PostgresStorage,
    key: String,
) -> (sqlx::Transaction<'_, sqlx::Postgres>, i32) {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *tx)
        .await
        .expect("take the blocker's lock");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .expect("read blocker pid");
    (tx, pid)
}

fn referrer_lock(referrer: &ArtifactReferrer) -> String {
    format!(
        "lash-artifact-referrer:{}:{}",
        referrer.kind().as_str(),
        referrer.canonical_id()
    )
}

/// Count, from now on, the advisory locks a transaction asks for after
/// its artifact locks that it does not already hold (FIG-7700).
async fn count_unplanned_locks(storage: &PostgresStorage) {
    sqlx::query(&format!(
        "CREATE SEQUENCE {}",
        lash_postgres_store::testing::UNPLANNED_LOCKS
    ))
    .execute(storage.pool())
    .await
    .expect("count unplanned locks");
}

/// The unplanned lock requests counted so far.
async fn unplanned_locks(storage: &PostgresStorage) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM {}",
        lash_postgres_store::testing::UNPLANNED_LOCKS
    ))
    .fetch_one(storage.pool())
    .await
    .expect("read the unplanned lock count")
}

/// The deadlocks PostgreSQL has detected in this law's database.
async fn deadlocks(storage: &PostgresStorage) -> i64 {
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(storage.pool())
        .await
        .expect("read the deadlock count")
}

/// PostgreSQL detects no deadlock beyond `before`. A backend reports its
/// statistics within ten seconds of going idle, so the count is watched
/// that long.
async fn assert_no_deadlock(storage: &PostgresStorage, before: i64) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(11);
    while std::time::Instant::now() < until {
        assert_eq!(
            deadlocks(storage).await,
            before,
            "PostgreSQL detected a deadlock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Republish environment `env` under an unrelated pin: its closure, the
/// closure's modules and its head, in one sorted set.
fn republish_env(
    storage: &PostgresStorage,
    env: lash_execution_contracts::process::ProcessEnvRef,
    bytes: Vec<u8>,
) -> impl FnOnce() -> tokio::task::JoinHandle<
    Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError>,
> {
    let artifacts = storage.lash_vm_artifact_store();
    move || {
        tokio::spawn(async move {
            use lash_execution_contracts::process::ProcessEnvStore as _;
            let (_, pin) = host_pin();
            artifacts.publish_process_env(&pin, &env, &bytes).await
        })
    }
}

/// Republish module `module` under an unrelated pin: its lock alone.
fn republish_module(
    storage: &PostgresStorage,
    module: String,
) -> impl FnOnce() -> tokio::task::JoinHandle<
    Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError>,
> {
    let artifacts = storage.lash_vm_artifact_store();
    move || {
        tokio::spawn(async move {
            let (_, pin) = host_pin();
            artifacts
                .publish_module_artifact(&pin, &module, b"linked document")
                .await
        })
    }
}

/// Wait until `publishing` finishes or waits on a lock: the publisher
/// finished while the blocker held its lock.
async fn published_while_stalled<T>(
    storage: &PostgresStorage,
    stalled: i64,
    publishing: &tokio::task::JoinHandle<T>,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock waits");
        if publishing.is_finished() || waits > stalled {
            return publishing.is_finished();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "publisher neither finished nor waited"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Race `committing`, stalled on `blocker`'s lock, against `publish`, a
/// publisher under an unrelated pin, then release the blocker. A commit
/// that takes every artifact lock of its transaction in one sorted
/// acquisition, after its other locks, holds none while it stalls on a
/// lock that is not an artifact's, so the publisher finishes first; one
/// that took an artifact lock early holds it, and the publisher waits on
/// it, or deadlocks with it once the blocker lets the commit go on. Every
/// lock the commit asks for after its artifact locks it already holds.
async fn race_a_publisher<A: std::fmt::Debug, B: std::fmt::Debug>(
    storage: &PostgresStorage,
    blocker: sqlx::Transaction<'_, sqlx::Postgres>,
    holder: i32,
    committing: tokio::task::JoinHandle<Result<A, B>>,
    publish: impl FnOnce() -> tokio::task::JoinHandle<
        Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError>,
    >,
) {
    wait_until_mutations_wait(storage, holder, 1).await;
    let publishing = publish();
    let published_while_stalled = published_while_stalled(storage, 1, &publishing).await;
    blocker.commit().await.expect("release the blocker");
    let within = std::time::Duration::from_secs(60);
    let committed = tokio::time::timeout(within, committing)
        .await
        .expect("the commit finishes")
        .expect("commit task");
    let republished = tokio::time::timeout(within, publishing)
        .await
        .expect("the publisher finishes")
        .expect("publish task");
    assert!(committed.is_ok(), "owner commit: {committed:?}");
    assert!(republished.is_ok(), "publisher: {republished:?}");
    assert!(
        published_while_stalled,
        "the publisher waited on an artifact lock the stalled commit held"
    );
    assert_eq!(unplanned_locks(storage).await, 0, "unplanned locks");
}

const LOCK_PLAN_LAW: lash_durable::CommitLabel = lash_durable::CommitLabel::new("law.lock-plan");

/// An owner write that ends an execution holding module `M`, then
/// registers a process on environment `H`, whose closure `C` holds `M`,
/// takes the artifact locks of both in one sorted acquisition before
/// either (FIG-7700). Ending the execution first holds `M` while the
/// registration waits on its process record's lock; a publisher of `H`
/// then holds `C` and waits on `M`, and the registration's `C` closes the
/// cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_execution_end_beside_a_registration_locks_its_environment_with_it() {
    use lash_durable::DurableStore as _;
    use lash_execution_contracts::process::{
        EngineLaunch, Lifetime, ProcessInput, ProcessProvenance, ProcessRegistrar as _,
        ProcessRegistration, StagedRegistration,
    };
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let session = SessionId::fixture("lock-plan-registration");
    session_store(&storage, session.clone()).await;
    let (_, keeper) = host_pin();
    let (env, bytes, module) = environment_over_document(&storage, &keeper, 51).await;
    let operation = "o/lock-plan-registration";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            &module,
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let prepared = storage
        .process_registry()
        .prepare_process_registration(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "lock-plan".into(),
                    payload: serde_json::json!({}),
                    launch: EngineLaunch::new(env.clone(), None),
                },
                ProcessProvenance::host(),
                Lifetime::Detached,
            ),
            &[],
        )
        .await
        .expect("prepare the registration");
    let (registration, observers, process_id, _, prepared_at_ms) = prepared.into_commit();
    let rows = StagedRegistration {
        execution: None,
        initial_resolutions: Vec::new(),
        registration,
        observers,
        process_id: process_id.clone(),
        prepared_at_ms,
    }
    .rows()
    .expect("the start's rows");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let (blocker, holder) = hold_lock(
        &storage,
        referrer_lock(&ArtifactReferrer::ProcessRecord(process_id)),
    )
    .await;
    let durable = storage.durable_store();
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, operation));
    tx.write(lash_durable::DomainWrite::Process(
        lash_durable::domain::ProcessWrite::Register(rows),
    ));
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        committing,
        republish_env(&storage, env, bytes),
    )
    .await;
}

/// An owner write ordered [execution end, head] takes the artifact locks
/// of the execution's end and of the frame the head leaves in one sorted
/// acquisition before either (FIG-7700). The execution holds `M`; the
/// left frame `F` holds environment `H` through closure `C`, which holds
/// `M`. Ending the execution first holds `M` while the head waits on
/// `F`'s referrer lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_execution_end_before_a_head_locks_the_frame_it_leaves_with_it() {
    use lash_durable::DurableStore as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let mut state = frame_state("lock-plan-head");
    let session = state.session_id.clone();
    let store = session_store(&storage, session.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let left = current_frame(&state);
    let left_claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(left.clone()))
        .expect("frame claim");
    let (env, bytes, module) = environment_over_document(&storage, &left_claim, 52).await;
    let operation = "o/lock-plan-head";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            &module,
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    open_resident_frame(&mut state, "lock-plan-next");
    let head = lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state);
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let (blocker, holder) = hold_lock(
        &storage,
        referrer_lock(&ArtifactReferrer::FrameEnvironment(left.clone())),
    )
    .await;
    let durable = storage.durable_store();
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, operation));
    write_head(&mut tx, &head);
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        committing,
        republish_env(&storage, env, bytes),
    )
    .await;
    assert!(
        frame_end(&storage, &left).await.0,
        "the head ends the frame it leaves"
    );
}

/// A fresh owner write whose head is already recorded answers the head
/// from its receipt and still ends its executions; their artifact locks
/// are taken in one sorted acquisition, whether or not the head runs
/// (FIG-7700). The first execution holds `M`; the second holds two
/// environments, through closures `X` and `C`, `X` sorting first and `C`
/// holding `M`. Ending the first execution alone holds `M` while the
/// second's end waits on `X`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_executions_ended_beside_a_recorded_head_lock_their_artifacts_once() {
    use lash_durable::DurableStore as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let mut state = frame_state("lock-plan-recorded");
    let session = state.session_id.clone();
    let store = session_store(&storage, session.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let (first, second) = ("o/lock-plan-first", "o/lock-plan-second");
    let second_claim = operation_claim(&session, second);
    let mut published = Vec::new();
    for round in [53, 54] {
        let (env_ref, bytes, document) =
            environment_over_document(&storage, &second_claim, round).await;
        let closure = closure_holding(&storage, &document).await;
        published.push((closure, env_ref, bytes, document));
    }
    published.sort_by(|left, right| left.0.cmp(&right.0));
    let earlier = published[0].0.clone();
    let (_, later_env, later_bytes, module) = published[1].clone();
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, first),
            &module,
            b"linked document",
        )
        .await
        .expect("the first execution holds the module");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let head = lash_core_store::store::append_request_commit_with_clock_for_testing(
        &mut state,
        "lock-plan-recorded",
        &[
            lash_protocol_types::session_model::SessionAppendNode::message(
                lash_protocol_types::plugin::PluginMessage::text(
                    lash_protocol_types::session_model::MessageRole::Assistant,
                    "recorded",
                ),
            ),
        ],
        None,
        &lash_core_ids::clock::SystemClock,
    )
    .expect("the head commit");
    let durable = storage.durable_store();
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    write_head(&mut tx, &head);
    durable
        .commit(tx, LOCK_PLAN_LAW)
        .await
        .expect("record the head");
    let (blocker, holder) =
        hold_lock(&storage, format!("lash-artifact:env_closure:{earlier}")).await;
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    write_head(&mut tx, &head);
    tx.write(operation_end(&session, first));
    tx.write(operation_end(&session, second));
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        committing,
        republish_env(&storage, later_env, later_bytes),
    )
    .await;
}

/// An owner write that ends an execution and deletes its session takes
/// the artifact locks of the execution's end and of the deleted session's
/// frame in one sorted acquisition before either (FIG-7700). The
/// execution holds `M`; the session's frame `F` holds environment `H`
/// through closure `C`, which holds `M`. Ending the execution first holds
/// `M` while the delete waits on `F`'s referrer lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_execution_end_beside_a_session_delete_locks_the_deleted_frame_with_it() {
    use lash_durable::DurableStore as _;
    use lash_durable::domain::{SessionCloseStep, SessionCloseWrite};
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let mut state = frame_state("lock-plan-delete");
    let session = state.session_id.clone();
    let store = session_store(&storage, session.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let frame = current_frame(&state);
    let frame_claim = ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(frame.clone()))
        .expect("frame claim");
    let (env, bytes, module) = environment_over_document(&storage, &frame_claim, 55).await;
    let operation = "o/lock-plan-delete";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            &module,
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let durable = storage.durable_store();
    for (label, write) in [
        (
            lash_durable::CommitLabel::SESSION_CLOSE_BEGIN,
            SessionCloseWrite::Begin {
                session: session.clone(),
            },
        ),
        (
            lash_durable::CommitLabel::SESSION_CLOSE_CANCEL,
            SessionCloseWrite::Step {
                session: session.clone(),
                step: SessionCloseStep::Cancel,
            },
        ),
        (
            lash_durable::CommitLabel::SESSION_CLOSE_REVOKE,
            SessionCloseWrite::Step {
                session: session.clone(),
                step: SessionCloseStep::Revoke,
            },
        ),
        (
            lash_durable::CommitLabel::SESSION_CLOSE_END_SCOPE,
            SessionCloseWrite::Step {
                session: session.clone(),
                step: SessionCloseStep::EndScope,
            },
        ),
    ] {
        let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
        tx.write(lash_durable::DomainWrite::SessionClose(write));
        durable.commit(tx, label).await.expect("the close step");
    }
    let (blocker, holder) = hold_lock(
        &storage,
        referrer_lock(&ArtifactReferrer::FrameEnvironment(frame.clone())),
    )
    .await;
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, operation));
    tx.write(lash_durable::DomainWrite::SessionClose(
        SessionCloseWrite::Step {
            session: session.clone(),
            step: SessionCloseStep::Artifacts,
        },
    ));
    let committing = tokio::spawn(async move {
        durable
            .commit(tx, lash_durable::CommitLabel::SESSION_CLOSE_ARTIFACTS)
            .await
    });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        committing,
        republish_env(&storage, env, bytes),
    )
    .await;
    assert!(
        frame_end(&storage, &frame).await.0,
        "the delete ends the session's frame"
    );
}

/// A publisher that loses a lock race publishes on its retry (FIG-7700).
/// A raw transaction holds module `M`'s lock; a publisher of environment
/// `H`, whose closure `C` holds `M`, takes `C` and waits on `M`; the raw
/// transaction then asks for `C`. The publisher has waited longest, so
/// its backend detects the cycle and aborts its own first attempt: the
/// raw transaction is granted `C`, which only that abort releases, and the
/// publication succeeds on its retry once the raw transaction ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_publisher_that_loses_a_lock_race_publishes_on_its_retry() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (_, keeper) = host_pin();
    let (env, bytes, module) = environment_over_document(&storage, &keeper, 56).await;
    let closure = closure_holding(&storage, &module).await;
    let (mut blocker, holder) = lock_artifact_mutations(&storage, &module).await;
    let publishing = republish_env(&storage, env, bytes)();
    wait_until_mutations_wait(&storage, holder, 1).await;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:env_closure:{closure}"))
        .execute(&mut *blocker)
        .await
        .expect("the publisher's first attempt is the one aborted, not the raw transaction");
    assert!(
        !publishing.is_finished(),
        "the publisher finished while the raw transaction held its module"
    );
    blocker.commit().await.expect("end the raw transaction");
    let republished = tokio::time::timeout(std::time::Duration::from_secs(60), publishing)
        .await
        .expect("the publisher finishes")
        .expect("publish task");
    assert!(republished.is_ok(), "publisher: {republished:?}");
}

/// A publisher's retry is bounded as a whole operation by the store
/// profile's deadline (FIG-7700): an attempt that starts inside it and
/// waits on a lock whose timeout runs past it ends at the deadline, with
/// the store's contention answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_publisher_ends_at_its_operation_deadline_with_contention() {
    let Some((database, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (_, keeper) = host_pin();
    let (env, bytes, module) = environment_over_document(&storage, &keeper, 57).await;
    let deadline = std::time::Duration::from_millis(1_500);
    let mut config = lash_postgres_store::testing::fixture_config();
    // Each attempt waits a second on the module, so the second attempt
    // starts inside the deadline and would run past it.
    config.guards.ordinary.lock =
        lash_postgres_store::host::ServerTimeout::Limit(std::time::Duration::from_millis(1_000));
    config.guards.ordinary.statement = lash_postgres_store::host::ServerTimeout::Limit(deadline);
    config.guards.ordinary.operation_deadline = Some(deadline);
    let bounded = lash_postgres_store::testing::connect_with(database.url(), &config)
        .await
        .expect("connect under a short deadline");
    let (blocker, holder) = lock_artifact_mutations(&storage, &module).await;
    let started = std::time::Instant::now();
    let publishing = republish_env(&bounded, env, bytes)();
    wait_until_mutations_wait(&storage, holder, 1).await;
    let published = tokio::time::timeout(std::time::Duration::from_secs(30), publishing)
        .await
        .expect("the publisher answers")
        .expect("publish task");
    let elapsed = started.elapsed();
    blocker.rollback().await.expect("release the module");
    assert!(
        matches!(
            published,
            Err(
                lash_execution_contracts::module_artifacts::ArtifactStoreError::StoreFault(
                    lash_core_store::store::StoreFault::Contended
                )
            )
        ),
        "publisher: {published:?}"
    );
    assert!(
        elapsed >= deadline && elapsed < deadline + std::time::Duration::from_millis(300),
        "the publisher answered after {elapsed:?}, not at its {deadline:?} deadline"
    );
}

/// What the racing owner write does with execution `X`, whose referrer is
/// `R`: registers it, adopts it as stored, or seeds a resolution of its
/// stored use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecutionWrite {
    Register,
    Adopt,
    Seed,
}

/// An owner write that ends an execution holding module `M` and registers,
/// adopts or seeds execution `X` takes `X`'s referrer lock `R` in its plan,
/// before any artifact lock (FIG-7700): a registration or adoption names
/// `R`, a seed reads it off the stored execution. A publisher under `R`
/// holds `R`, then locks `M`; the blocker holds `R` as it would. Taking `R`
/// after the plan's `M` leaves the stalled commit holding `M`, so an
/// unrelated publisher of `M` waits on it, and the publisher under `R`
/// would deadlock with it.
async fn race_an_execution_write_beside_an_execution_end(
    fixture: &str,
    turn: &'static str,
    write: ExecutionWrite,
) {
    use lash_durable::DurableStore as _;
    use lash_durable::resolutions::{
        DeferredUse, ExecutionId, ExecutionRegistration, ResolutionEntry, ResolutionKey,
        ResolutionRecord,
    };
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let before = deadlocks(&storage).await;
    let session = SessionId::fixture(fixture);
    session_store(&storage, session.clone()).await;
    let operation = format!("o/{fixture}");
    let module = format!("{fixture}-module");
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, &operation),
            &module,
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let run = lash_vocabulary::identity::TurnId::from(turn);
    let registered = ArtifactReferrer::Execution(
        lash_protocol_types::ExecutionScope::turn(session.clone(), run.clone())
            .journal_identity()
            .expect("turn journal"),
    );
    let execution = ExecutionId::cell(&session, &run, "cell");
    let used = lash_kernel_value::names::OperationUse {
        link: lash_kernel_value::package::LinkId::from_bytes([7; 32]),
        slot: lash_kernel_value::names::ImportSlot(1).into(),
    };
    let requested = lash_kernel_value::names::QualifiedName {
        package: lash_kernel_value::names::PackagePath::parse("wip").expect("package"),
        member: lash_kernel_value::names::Segment::new("lookup").expect("member"),
        type_member: None,
    };
    let registration = ExecutionRegistration {
        execution: execution.clone(),
        uses: [(
            used,
            DeferredUse {
                requested: requested.clone(),
                discovery: Default::default(),
            },
        )]
        .into(),
        referrer: registered.clone(),
        requirements: Default::default(),
    };
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let durable = storage.durable_store();
    if write != ExecutionWrite::Register {
        let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
        tx.register_execution(registration.clone());
        durable
            .commit(tx, LOCK_PLAN_LAW)
            .await
            .expect("store the execution");
    }
    let (blocker, holder) = hold_lock(&storage, referrer_lock(&registered)).await;
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, &operation));
    match write {
        ExecutionWrite::Register => tx.register_execution(registration),
        ExecutionWrite::Adopt => tx.adopt_execution(registration),
        ExecutionWrite::Seed => tx.seed_resolution(ResolutionRecord {
            key: ResolutionKey {
                execution,
                use_: used,
            },
            requested,
            entry: ResolutionEntry::Refused {
                note: "lock plan law".into(),
            },
        }),
    };
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        committing,
        republish_module(&storage, module),
    )
    .await;
    assert_no_deadlock(&storage, before).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_execution_registration_beside_an_execution_end_locks_its_referrer_first() {
    race_an_execution_write_beside_an_execution_end(
        "lock-plan-package",
        "lock-plan-package-turn",
        ExecutionWrite::Register,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_execution_adoption_beside_an_execution_end_locks_its_referrer_first() {
    race_an_execution_write_beside_an_execution_end(
        "lock-plan-adopt",
        "lock-plan-adopt-turn",
        ExecutionWrite::Adopt,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_stored_execution_seed_beside_an_execution_end_locks_its_referrer_first() {
    race_an_execution_write_beside_an_execution_end(
        "lock-plan-seed",
        "lock-plan-seed-turn",
        ExecutionWrite::Seed,
    )
    .await;
}

/// A detached process on environment `env`, registered directly and
/// completed: terminal, so a prune takes it.
async fn terminal_process(
    storage: &PostgresStorage,
    env: &lash_execution_contracts::process::ProcessEnvRef,
) -> lash_vocabulary::identity::ProcessId {
    use lash_execution_contracts::process::{ProcessLifecycle as _, ProcessRegistrar as _};
    let registry = storage.process_registry();
    let registered = registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "lock-plan-prune".into(),
                payload: serde_json::json!({}),
                launch: lash_execution_contracts::process::EngineLaunch::new(env.clone(), None),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register the process");
    registry
        .complete_process(
            &registered.id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(format!(
                "lock-plan-prune-{}",
                registered.id.as_str()
            )),
        )
        .await
        .expect("complete the process");
    registered.id
}

/// A batch prune takes the artifact locks of every process it ends in one
/// sorted acquisition, after their referrer locks (FIG-7700). Processes
/// `P1` before `P2` hold environments whose closures hold documents `D1`
/// and `D2`; the blocker holds `P2`'s closure. Ending `P1` alone first
/// holds `D1` while `P2`'s end waits on its closure, so an unrelated
/// publisher of `D1` waits on the prune, and one that holds the closure
/// and then locks `D1` would deadlock with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_batch_prune_locks_every_ended_process_artifact_at_once() {
    use lash_execution_contracts::process::ProcessRetention as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let before = deadlocks(&storage).await;
    let (_, keeper) = host_pin();
    let mut processes = Vec::new();
    for round in [61, 62] {
        let (env, _, document) = environment_over_document(&storage, &keeper, round).await;
        let closure = closure_holding(&storage, &document).await;
        let process = terminal_process(&storage, &env).await;
        processes.push((process.as_str().to_owned(), closure, document));
    }
    processes.sort();
    let (_, _, first_document) = processes[0].clone();
    let (_, later_closure, _) = processes[1].clone();
    let (blocker, holder) = hold_lock(
        &storage,
        format!("lash-artifact:env_closure:{later_closure}"),
    )
    .await;
    let pruning = tokio::spawn({
        let registry = storage.process_registry();
        async move {
            registry
                .prune_terminal_processes(
                    u64::MAX,
                    None,
                    lash_execution_contracts::process::ProjectionWatermark::NoProjector,
                )
                .await
        }
    });
    race_a_publisher(
        &storage,
        blocker,
        holder,
        pruning,
        republish_module(&storage, first_document),
    )
    .await;
    assert_no_deadlock(&storage, before).await;
}

/// The registration of a process that closes with session `S`, as an
/// engine start on `env`.
fn session_scoped(
    session: &SessionId,
    env: &lash_execution_contracts::process::ProcessEnvRef,
) -> lash_execution_contracts::process::ProcessRegistration {
    lash_execution_contracts::process::ProcessRegistration::new(
        lash_execution_contracts::process::ProcessInput::Engine {
            kind: "lock-plan-scope".into(),
            payload: serde_json::json!({}),
            launch: lash_execution_contracts::process::EngineLaunch::new(env.clone(), None),
        },
        lash_execution_contracts::process::ProcessProvenance::host(),
        lash_execution_contracts::process::Lifetime::Until(
            lash_execution_contracts::process::ScopeRef::host_session_lookup(session.clone()),
        ),
    )
}

/// An owner write that registers a process under session `S` on
/// environment `H` takes `S`'s scope lock in its plan, before `H`'s
/// artifact locks (FIG-7700). A direct registrar of another process under
/// `S` holds `S`, then locks `H`'s closure `C`. With the blocker on `S`,
/// the registrar queues first, then the owner write. Taking `C` before
/// `S` leaves the stalled owner write holding `C`: an unrelated publisher
/// of `H` waits on it, and once the blocker goes the registrar, granted
/// `S`, waits on `C` while the owner write waits on `S`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_registration_owner_write_takes_its_scope_lock_before_a_direct_registrar_closure()
 {
    use lash_durable::DurableStore as _;
    use lash_execution_contracts::process::{ProcessRegistrar as _, StagedRegistration};
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let before = deadlocks(&storage).await;
    let session = SessionId::fixture("lock-plan-scope");
    session_store(&storage, session.clone()).await;
    let (_, keeper) = host_pin();
    let (env, bytes, _) = environment_over_document(&storage, &keeper, 63).await;
    let prepared = storage
        .process_registry()
        .prepare_process_registration(session_scoped(&session, &env), &[])
        .await
        .expect("prepare the owner write's registration");
    let (registration, observers, process_id, _, prepared_at_ms) = prepared.into_commit();
    let rows = StagedRegistration {
        execution: None,
        initial_resolutions: Vec::new(),
        registration,
        observers,
        process_id,
        prepared_at_ms,
    }
    .rows()
    .expect("the start's rows");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let scope = lash_core_store::scope_identity::ScopeId::Session(session.clone());
    let (blocker, holder) = hold_lock(
        &storage,
        format!(
            "lash-parent-end:{}:{}",
            scope.storage_kind(),
            scope.storage_id()
        ),
    )
    .await;
    let registering = tokio::spawn({
        let registry = storage.process_registry();
        let registration = session_scoped(&session, &env);
        async move { registry.register_process(registration).await }
    });
    wait_until_mutations_wait(&storage, holder, 1).await;
    let durable = storage.durable_store();
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(lash_durable::DomainWrite::Process(
        lash_durable::domain::ProcessWrite::Register(rows),
    ));
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    wait_until_mutations_wait(&storage, holder, 2).await;
    let publishing = republish_env(&storage, env, bytes)();
    let published_while_stalled = published_while_stalled(&storage, 2, &publishing).await;
    blocker.commit().await.expect("release the scope");
    let within = std::time::Duration::from_secs(60);
    let registered = tokio::time::timeout(within, registering)
        .await
        .expect("the direct registrar finishes")
        .expect("registrar task");
    let committed = tokio::time::timeout(within, committing)
        .await
        .expect("the owner write finishes")
        .expect("commit task");
    let republished = tokio::time::timeout(within, publishing)
        .await
        .expect("the publisher finishes")
        .expect("publish task");
    assert!(registered.is_ok(), "direct registrar: {registered:?}");
    assert!(committed.is_ok(), "owner write: {committed:?}");
    assert!(republished.is_ok(), "publisher: {republished:?}");
    assert!(
        published_while_stalled,
        "the publisher waited on an artifact lock the stalled owner write held"
    );
    assert_eq!(unplanned_locks(&storage).await, 0, "unplanned locks");
    assert_no_deadlock(&storage, before).await;
}

/// After its artifact locks a transaction never waits for an advisory
/// lock (FIG-7700): an owner write whose seam asks for one more lock after
/// its plan, held elsewhere, is answered contention at once and runs
/// again, never queueing on it, and commits on the attempt after the lock
/// is free. The count records each attempt's unplanned request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_sealed_owner_write_retries_an_unplanned_lock_instead_of_waiting() {
    use lash_durable::DurableStore as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    count_unplanned_locks(&storage).await;
    let before = deadlocks(&storage).await;
    let session = SessionId::fixture("lock-plan-seal");
    session_store(&storage, session.clone()).await;
    let operation = "o/lock-plan-seal";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            "lock-plan-seal-module",
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let unplanned = "lash-law:after-the-plan";
    let (blocker, holder) = hold_lock(&storage, unplanned.to_owned()).await;
    let durable = storage
        .durable_store()
        .with_lock_after_plan_for_testing(unplanned);
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, operation));
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while unplanned_locks(&storage).await == 0 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks waiting JOIN pg_locks held
               ON held.locktype = waiting.locktype AND held.database = waiting.database
              AND held.classid = waiting.classid AND held.objid = waiting.objid
              AND held.objsubid = waiting.objsubid
             WHERE waiting.locktype = 'advisory' AND NOT waiting.granted
               AND held.pid = $1 AND held.granted",
        )
        .bind(holder)
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock waits");
        assert_eq!(
            waiting, 0,
            "the sealed owner write waited on the unplanned lock"
        );
        assert!(
            !committing.is_finished(),
            "the owner write finished past the held lock"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the owner write never asked for the unplanned lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    blocker.commit().await.expect("release the unplanned lock");
    let committed = tokio::time::timeout(std::time::Duration::from_secs(60), committing)
        .await
        .expect("the owner write finishes")
        .expect("commit task");
    assert!(committed.is_ok(), "owner write: {committed:?}");
    assert!(
        unplanned_locks(&storage).await >= 2,
        "the owner write committed without retrying its contended attempt"
    );
    assert_no_deadlock(&storage, before).await;
}

/// Wait until `count` backends of this database wait on a lock of any
/// kind: advisory, row or transaction.
async fn wait_until_backends_wait(storage: &PostgresStorage, count: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock waits");
        if waiting >= count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{waiting} of {count} transactions reached their lock wait"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Wait for both tasks, then require each to succeed.
async fn both_answer<
    A: std::fmt::Debug,
    B: std::fmt::Debug,
    C: std::fmt::Debug,
    D: std::fmt::Debug,
>(
    first: tokio::task::JoinHandle<Result<A, B>>,
    second: tokio::task::JoinHandle<Result<C, D>>,
) {
    let within = std::time::Duration::from_secs(60);
    let first = tokio::time::timeout(within, first)
        .await
        .expect("the first finishes")
        .expect("first task");
    let second = tokio::time::timeout(within, second)
        .await
        .expect("the second finishes")
        .expect("second task");
    assert!(first.is_ok(), "first: {first:?}");
    assert!(second.is_ok(), "second: {second:?}");
}

/// Hold process `process`'s row in an open transaction: the transaction
/// and its backend pid.
async fn hold_process_row<'a>(
    storage: &'a PostgresStorage,
    process: &lash_vocabulary::identity::ProcessId,
) -> (sqlx::Transaction<'a, sqlx::Postgres>, i32) {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM lash_processes WHERE process_id = $1 FOR UPDATE",
    )
    .bind(process.as_str())
    .fetch_one(&mut *tx)
    .await
    .expect("lock the process row");
    (tx, pid)
}

/// An owner terminal of process `P` with no retained preimage locks `P`'s
/// row before its artifact locks, as a prune of `P` does (FIG-7700). The
/// owner write ends an execution holding module `M` and writes `P`'s
/// terminal; `P`'s environment closure holds `M` too. With `M` held, the
/// owner write queues on it, then the prune, holding `P`'s row, or queued
/// behind the owner write on it. Locking the row after the plan leaves the
/// owner write, granted `M`, waiting on the row while the prune holds the
/// row and waits on `M`: a deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_an_owner_terminal_without_a_preimage_locks_its_process_row_before_a_prune() {
    use lash_durable::DurableStore as _;
    use lash_execution_contracts::process::ProcessRetention as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let before = deadlocks(&storage).await;
    let session = SessionId::fixture("lock-plan-terminal");
    session_store(&storage, session.clone()).await;
    let (_, keeper) = host_pin();
    let (env, _, module) = environment_over_document(&storage, &keeper, 73).await;
    let process = terminal_process(&storage, &env).await;
    let operation = "o/lock-plan-terminal";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            &module,
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let mut outcome = Vec::new();
    lash_execution_contracts::process::ProcessAwaitOutput::serialize(
        &lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::Value::Null),
        ),
        &mut serde_json::Serializer::new(&mut outcome),
    )
    .expect("encode the terminal");
    let outcome = String::from_utf8(outcome).expect("the terminal is text");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let (blocker, holder) = lock_artifact_mutations(&storage, &module).await;
    let durable = storage.durable_store();
    let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
    tx.write(operation_end(&session, operation));
    tx.write(lash_durable::DomainWrite::Process(
        lash_durable::domain::ProcessWrite::Terminal {
            process,
            outcome_json: outcome,
        },
    ));
    let committing = tokio::spawn(async move { durable.commit(tx, LOCK_PLAN_LAW).await });
    wait_until_mutations_wait(&storage, holder, 1).await;
    let pruning = tokio::spawn({
        let registry = storage.process_registry();
        async move {
            registry
                .prune_terminal_processes(
                    u64::MAX,
                    None,
                    lash_execution_contracts::process::ProjectionWatermark::NoProjector,
                )
                .await
        }
    });
    wait_until_backends_wait(&storage, 2).await;
    blocker.commit().await.expect("release the module");
    both_answer(committing, pruning).await;
    assert_no_deadlock(&storage, before).await;
}

/// A sealed transaction never waits long for a row, and a held row does
/// not exhaust its retries (FIG-7700): an owner write that, after its
/// artifact locks, updates process `P`'s row another transaction holds for
/// 3 s waits at most the sealed lock timeout per attempt, below
/// PostgreSQL's `deadlock_timeout`, and the one `durable.commit` call
/// commits once the row is free. Each wait on the row is one attempt's
/// statement: more than one shows the bound fired inside the call, and
/// more than the policy's four shows those expiries spent no attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_sealed_owner_write_retries_a_held_row_within_its_lock_timeout() {
    use lash_durable::DurableStore as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let before = deadlocks(&storage).await;
    let session = SessionId::fixture("lock-plan-sealed-row");
    session_store(&storage, session.clone()).await;
    let (_, keeper) = host_pin();
    let (env, _, _) = environment_over_document(&storage, &keeper, 74).await;
    let process = terminal_process(&storage, &env).await;
    let operation = "o/lock-plan-sealed-row";
    storage
        .lash_vm_artifact_store()
        .publish_module_artifact(
            &operation_claim(&session, operation),
            "lock-plan-sealed-row-module",
            b"linked document",
        )
        .await
        .expect("the execution holds the module");
    let deadlock_timeout: f64 = sqlx::query_scalar(
        "SELECT setting::float8 / 1000 FROM pg_settings WHERE name = 'deadlock_timeout'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read deadlock_timeout");
    let (actor, epoch) = claim_owner(&storage, &session).await;
    let (blocker, holder) = hold_process_row(&storage, &process).await;
    let durable = storage.durable_store();
    let committing = tokio::spawn(async move {
        let mut tx = durable.begin(&actor, epoch, u64::MAX).await.expect("open");
        tx.write(operation_end(&session, operation));
        tx.write(lash_durable::DomainWrite::Process(
            lash_durable::domain::ProcessWrite::Published {
                process: process.clone(),
                through: 1,
            },
        ));
        durable.commit(tx, LOCK_PLAN_LAW).await
    });
    let (waits, longest) = lock_waits_while_held(
        &storage,
        holder,
        std::time::Duration::from_secs(3),
        &committing,
    )
    .await;
    blocker.commit().await.expect("release the row");
    let committed = tokio::time::timeout(std::time::Duration::from_secs(30), committing)
        .await
        .expect("the owner write finishes")
        .expect("commit task");
    eprintln!("sealed owner write: {waits} waits on the row held 3 s");
    assert!(committed.is_ok(), "owner write: {committed:?}");
    assert!(
        waits > 4,
        "the owner write waited on the row {waits} time(s), not once an attempt past the policy's four"
    );
    assert!(
        longest < deadlock_timeout,
        "the sealed owner write waited {longest:.3}s on the row, past deadlock_timeout"
    );
    assert_no_deadlock(&storage, before).await;
}

/// How many distinct lock waits (backend, statement start) other than
/// `holder`'s the server shows while `holder` keeps its lock for `hold`,
/// and the longest one; `task`, the waiter, must not finish meanwhile.
async fn lock_waits_while_held<T: std::fmt::Debug>(
    storage: &PostgresStorage,
    holder: i32,
    hold: std::time::Duration,
    task: &tokio::task::JoinHandle<T>,
) -> (usize, f64) {
    let mut waits = std::collections::BTreeSet::new();
    let mut longest = 0.0_f64;
    let held_until = std::time::Instant::now() + hold;
    while std::time::Instant::now() < held_until {
        let rows: Vec<(i32, String, f64)> = sqlx::query_as(
            "SELECT pid, query_start::text,
                    EXTRACT(EPOCH FROM clock_timestamp() - query_start)::float8
             FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock' AND pid <> $1",
        )
        .bind(holder)
        .fetch_all(storage.pool())
        .await
        .expect("inspect lock waits");
        for (pid, started, waited) in rows {
            waits.insert((pid, started));
            longest = longest.max(waited);
        }
        assert!(
            !task.is_finished(),
            "the waiter finished past the held row after waits {waits:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    (waits.len(), longest)
}

/// Prepare a session whose deletion seals (its frame holds an
/// environment), and hold its head row from another transaction: the
/// session, the blocker and the blocker's backend.
async fn session_with_held_head(
    storage: &PostgresStorage,
    fixture: &str,
) -> (SessionId, sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut state = frame_state(fixture);
    let session = state.session_id.clone();
    let store = session_store(storage, session.clone()).await;
    commit_frame_opens(&store, &mut state, None).await;
    let frame_claim =
        ReferrerClaim::unguarded(ArtifactReferrer::FrameEnvironment(current_frame(&state)))
            .expect("frame claim");
    environment_over_document(storage, &frame_claim, 76).await;
    let mut blocker = storage.pool().begin().await.expect("begin blocker");
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .expect("the blocker's backend");
    sqlx::query("SELECT 1 FROM lash_session_head WHERE session_id = $1 FOR UPDATE")
        .bind(session.as_str())
        .fetch_one(&mut *blocker)
        .await
        .expect("hold the head row");
    (session, blocker, holder)
}

/// A session deletion seals when it ends its frame's edges, so a row it
/// deletes after that is waited for at most the sealed lock timeout per
/// attempt; such an expiry spends none of the store's attempts, so the
/// deletion keeps retrying under its operation deadline and succeeds once
/// the row is free (FIG-7700). Held 3 s, the head row outlasts the four
/// attempts' sealed waits, which ended the deletion with contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_session_delete_retries_a_row_held_past_its_seal() {
    use lash_core_store::store::SessionCatalogStore as _;
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (session, blocker, holder) =
        session_with_held_head(&storage, "lock-plan-delete-retry").await;
    let catalog = storage.store();
    let deleting = tokio::spawn({
        let session = session.clone();
        async move { catalog.delete_session(&session).await }
    });
    let (waits, _) = lock_waits_while_held(
        &storage,
        holder,
        std::time::Duration::from_secs(3),
        &deleting,
    )
    .await;
    blocker.commit().await.expect("release the head row");
    let deleted = tokio::time::timeout(std::time::Duration::from_secs(60), deleting)
        .await
        .expect("the deletion finishes")
        .expect("delete task");
    eprintln!("session delete: {waits} waits on the head row held 3 s");
    assert!(deleted.is_ok(), "session delete: {deleted:?}");
    assert!(matches!(
        storage.store().lookup_session(&session).await,
        Ok(lash_core_store::store::SessionLookup::Deleted)
    ));
}

/// The retry of sealed waits is bounded by the operation (FIG-7700): a
/// session deletion whose head row is held past the store profile's
/// operation deadline answers the store's contention at that deadline,
/// not later, though its sealed expiries spent no attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_a_session_delete_ends_at_its_operation_deadline_with_contention() {
    use lash_core_store::store::SessionCatalogStore as _;
    let Some((database, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let deadline = std::time::Duration::from_millis(2_000);
    let mut config = lash_postgres_store::testing::fixture_config();
    // The ordinary guards shrink to fit under the deadline; the waits that
    // matter are the sealed ones, 250 ms each.
    config.guards.ordinary.lock =
        lash_postgres_store::host::ServerTimeout::Limit(std::time::Duration::from_millis(1_000));
    config.guards.ordinary.statement = lash_postgres_store::host::ServerTimeout::Limit(deadline);
    config.guards.ordinary.operation_deadline = Some(deadline);
    let bounded = lash_postgres_store::testing::connect_with(database.url(), &config)
        .await
        .expect("connect under a short deadline");
    let (session, blocker, _) = session_with_held_head(&storage, "lock-plan-delete-deadline").await;
    let started = std::time::Instant::now();
    let deleted = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        bounded.store().delete_session(&session),
    )
    .await
    .expect("the deletion answers");
    let elapsed = started.elapsed();
    blocker.rollback().await.expect("release the head row");
    assert!(
        matches!(
            deleted,
            Err(lash_core_store::store::MaintenanceFailure {
                stop: lash_core_store::store::MaintenanceStop::Failed(
                    lash_core_store::store::StoreError::Contended
                ),
                ..
            })
        ),
        "session delete: {deleted:?}"
    );
    // The last pause not taken (the policy's longest, 20 ms) may end the
    // loop just before the deadline.
    assert!(
        elapsed + std::time::Duration::from_millis(50) >= deadline
            && elapsed < deadline + std::time::Duration::from_millis(300),
        "the deletion answered after {elapsed:?}, not at its {deadline:?} deadline"
    );
}

/// Prepare an environment owner under `actor`, session `session`'s
/// claimed actor: its referrer.
async fn environment_owner(
    storage: &PostgresStorage,
    actor: &lash_durable::ActorKey,
    epoch: lash_durable::Epoch,
) -> ArtifactReferrer {
    use lash_durable::DurableStore as _;
    use lash_durable::environment::{self as env, EnvironmentWrite};
    let body = env::assemble(None, []).expect("environment body");
    let owner = env::VersionOwner::preparing(body.id().clone());
    let durable = storage.durable_store();
    let mut tx = durable.begin(actor, epoch, u64::MAX).await.expect("open");
    tx.write(lash_durable::DomainWrite::Environment(
        EnvironmentWrite::prepare(owner.clone(), &body).expect("prepare the owner"),
    ));
    durable
        .commit(tx, LOCK_PLAN_LAW)
        .await
        .expect("commit the owner");
    ArtifactReferrer::EnvironmentVersion(owner.id().clone())
}

/// Two ends whose carries go to environment owners of the same two
/// sessions, in opposite session order by owner, lock both sessions in
/// session order before any owner row (FIG-7700). End 1 carries to `x` of
/// `S` and `y` of `T`, `x` sorting first; end 2 carries to `u` of `T` and
/// `v` of `S`, `u` first. With `x`'s row held, end 1 queues on it, then
/// end 2. Locking each owner's session with its row, one owner at a time,
/// leaves end 1 holding `S` and end 2 holding `T`, each waiting on the
/// other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_ends_carrying_to_environment_owners_lock_their_sessions_in_session_order() {
    use lash_core_store::artifact_referrer::{ArtifactCarry, ArtifactName, ArtifactStoreId};
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let before = deadlocks(&storage).await;
    let mut sessions = [
        SessionId::fixture("lock-plan-owner-a"),
        SessionId::fixture("lock-plan-owner-b"),
    ];
    sessions.sort();
    for session in &sessions {
        session_store(&storage, session.clone()).await;
    }
    let claims = claim_owners(&storage, &sessions).await;
    // Owner ids are fresh: prepare one more owner of each session until
    // one of `S` sorts before one of `T` and another of `T` before
    // another of `S`.
    let mut owners: Vec<(String, ArtifactReferrer, usize)> = Vec::new();
    let pairs = loop {
        for (index, (actor, epoch)) in claims.iter().enumerate() {
            let owner = environment_owner(&storage, actor, *epoch).await;
            owners.push((referrer_lock(&owner), owner, index));
        }
        owners.sort_by(|left, right| left.0.cmp(&right.0));
        let pair = |first: usize, taken: &[usize]| {
            owners.iter().enumerate().find_map(|(at, (_, _, session))| {
                (*session == first && !taken.contains(&at))
                    .then_some(at)
                    .and_then(|at| {
                        owners
                            .iter()
                            .enumerate()
                            .skip(at + 1)
                            .find(|(later, (_, _, session))| {
                                *session != first && !taken.contains(later)
                            })
                            .map(|(later, _)| (at, later))
                    })
            })
        };
        if let Some((x, y)) = pair(0, &[])
            && let Some((u, v)) = pair(1, &[x, y])
        {
            break [(x, y), (u, v)];
        }
        assert!(owners.len() < 32, "owner ids never interleaved");
    };
    let store = storage.lash_vm_artifact_store();
    let mut ends = Vec::new();
    for (index, (first, second)) in pairs.into_iter().enumerate() {
        let (referrer, claim) = host_pin();
        let module = format!("lock-plan-owner-module-{index}");
        store
            .publish_module_artifact(&claim, &module, b"carried document")
            .await
            .expect("the ending pin holds the module");
        let carries = [first, second]
            .into_iter()
            .map(|at| ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::KernelDocument,
                    artifact_ref: module.clone(),
                },
                to: owners[at].1.clone(),
            })
            .collect();
        ends.push(ResolvedArtifactCleanup { referrer, carries });
    }
    let ArtifactReferrer::EnvironmentVersion(x) = &ends[0].carries[0].to else {
        unreachable!("the carries go to environment owners");
    };
    let mut blocker = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT 1 FROM lash_environment_owners WHERE owner_id = $1 FOR UPDATE")
        .bind(x.to_string())
        .fetch_one(&mut *blocker)
        .await
        .expect("lock the first owner's row");
    let mut ending = ends.into_iter().map(|cleanup| {
        let store = store.clone();
        tokio::spawn(async move { store.end_module_referrer(&cleanup).await })
    });
    let first = ending.next().expect("end 1");
    wait_until_backends_wait(&storage, 1).await;
    let second = ending.next().expect("end 2");
    wait_until_backends_wait(&storage, 2).await;
    blocker.commit().await.expect("release the owner row");
    both_answer(first, second).await;
    assert_no_deadlock(&storage, before).await;
}
