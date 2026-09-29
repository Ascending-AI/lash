//! A logical root's terminal evidence and the drive fence, as store laws
//! (FIG-3600 S7, ADR 0105 §2): the head commit of a root's final physical
//! turn writes the root's evidence in its own transaction, whichever
//! turn-lane family heads the root, a root keeps
//! the first terminal it reached, and a commit sealed under an admission a
//! successor superseded is refused before it writes anything.

use super::*;
use lash_core::store::{
    AdmissionId, AdmittedHead, DriveEpochSeal, DriveFence, RootStartNonce, RootTerminalKind,
    RootTerminalWrite, TurnCommitId,
};
use pretty_assertions::assert_eq;

fn state(session_id: &SessionId) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state
}

/// The terminal commit of physical turn `turn` over `state`, carrying
/// `root_terminal` and fenced by `drive_fence`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a derivable graph is established by the setup"
)]
fn turn_commit(
    state: &RuntimeSessionState,
    turn: &str,
    root_terminal: Option<RootTerminalWrite>,
    drive_fence: Option<DriveFence>,
) -> RuntimeCommit {
    let operation = crate::OperationId::turn(state.session_id.as_str(), turn, "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )
    .expect("build the commit");
    commit.root_terminal = root_terminal.map(Box::new);
    commit.drive_fence = drive_fence.map(Box::new);
    commit
}

/// What the commit of physical turn `ordinal` of `root` writes when it ends
/// the root with `stop`.
fn ends(root: &str, ordinal: u32, stop: Option<crate::TurnStop>) -> RootTerminalWrite {
    let root = TurnId::from(root);
    let turn = lash_core::store::PhysicalTurn::derive_turn_id(&root, u64::from(ordinal));
    RootTerminalWrite {
        commit: TurnCommitId::new(root.clone(), ordinal),
        root,
        turn,
        stop,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn terminal_of(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    root: &str,
) -> Option<lash_core::store::RootTerminal> {
    store
        .root_terminal(session_id, &TurnId::from(root))
        .await
        .expect("read the root's terminal evidence")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn head_revision(store: &Arc<dyn RuntimePersistence>) -> u64 {
    store
        .load_session_head_meta()
        .await
        .expect("load the head")
        .map_or(0, |meta| meta.head_revision)
}

/// L-T1 and L-S6: a root's terminal evidence commits in the head transaction
/// of its final physical turn. A commit refused at its head compare-and-set
/// leaves none; the landed commit writes exactly its cause; a retry of it
/// rewrites nothing; and a later commit naming another terminal for the same
/// root is refused whole, the first terminal standing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn root_terminal_evidence_commits_in_the_head_transaction(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("root-terminal-head");
    let state = state(&session_id);
    assert_eq!(terminal_of(&store, &session_id, "r").await, None);

    let mut conflicted = turn_commit(&state, "r", Some(ends("r", 0, None)), None);
    conflicted.expected_head_revision = 7;
    assert!(
        matches!(
            store.commit_runtime_state(conflicted).await,
            Err(crate::StoreError::HeadRevisionConflict { .. })
        ),
        "the forced head conflict refuses the commit"
    );
    assert_eq!(
        terminal_of(&store, &session_id, "r").await,
        None,
        "a refused head commit leaves no evidence"
    );

    let landed = turn_commit(&state, "r", Some(ends("r", 0, None)), None);
    let receipt = store
        .commit_runtime_state(landed.clone())
        .await
        .expect("the root's final commit lands");
    let terminal = terminal_of(&store, &session_id, "r")
        .await
        .expect("the landed commit wrote the root's evidence");
    assert_eq!(terminal.kind, RootTerminalKind::Answered);
    assert_eq!(terminal.cause, ends("r", 0, None).cause());
    assert_eq!(terminal.head_revision, Some(receipt.head_revision));
    assert_eq!(
        terminal.commit(),
        Some(&TurnCommitId::new(TurnId::from("r"), 0))
    );

    store
        .commit_runtime_state(landed)
        .await
        .expect("a retried commit replays its receipt");
    assert_eq!(
        terminal_of(&store, &session_id, "r").await,
        Some(terminal.clone()),
        "a retry rewrites nothing"
    );

    let revision = head_revision(&store).await;
    let resumed = loaded_conformance_state(&store).await;
    let other = turn_commit(
        &resumed,
        "r:1",
        Some(ends("r", 1, Some(crate::TurnStop::ToolFailure))),
        None,
    );
    assert!(
        matches!(
            store.commit_runtime_state(other).await,
            Err(crate::StoreError::RootAlreadyTerminal { .. })
        ),
        "a second, different terminal is refused"
    );
    assert_eq!(
        head_revision(&store).await,
        revision,
        "the refused commit moved no head"
    );
    assert_eq!(
        terminal_of(&store, &session_id, "r").await,
        Some(terminal),
        "the first terminal stands"
    );
}

/// The successor-sealed commit refusal (ADR 0105 §2, Q-B3): a root's commit
/// carries the fence its admission was sealed with, and a successor's seal
/// makes that fence stale. The stale commit is refused
/// [`StaleDriveFence`](crate::StoreError::StaleDriveFence) before it writes
/// anything, head or evidence; the successor's own commit lands.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_commit_sealed_under_a_superseded_admission_is_refused(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("root-terminal-fence");
    let state = state(&session_id);
    store
        .commit_runtime_state(turn_commit(&state, "seed", None, None))
        .await
        .expect("the seed commit creates the session");
    let seal = |admission: &'static str, observed: u64| {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        async move {
            match store
                .seal_drive_epoch(
                    &session_id,
                    &AdmissionId::new(admission),
                    observed,
                    &RootStartNonce::new(admission),
                )
                .await
                .expect("seal the admission")
            {
                DriveEpochSeal::Sealed(fence) => fence,
                other => panic!("admission `{admission}` seals: {other:?}"),
            }
        }
    };
    let stale = seal("drive-a#0", 0).await;
    let current = seal("drive-b#0", 1).await;
    assert_eq!((stale.epoch(), current.epoch()), (1, 2));

    let resumed = loaded_conformance_state(&store).await;
    let revision = head_revision(&store).await;
    assert!(
        matches!(
            store
                .commit_runtime_state(turn_commit(
                    &resumed,
                    "r",
                    Some(ends("r", 0, None)),
                    Some(stale),
                ))
                .await,
            Err(crate::StoreError::StaleDriveFence {
                fence_epoch: 1,
                current_epoch: 2,
                ..
            })
        ),
        "a commit sealed under a superseded admission is refused"
    );
    assert_eq!(head_revision(&store).await, revision, "no head moved");
    assert_eq!(terminal_of(&store, &session_id, "r").await, None);

    store
        .commit_runtime_state(turn_commit(
            &resumed,
            "r",
            Some(ends("r", 0, None)),
            Some(current),
        ))
        .await
        .expect("the successor's commit lands");
    assert!(terminal_of(&store, &session_id, "r").await.is_some());
}

/// L-T3 for a queued-headed root: a root admitted on a queued-work batch
/// ends like any root. The head commit of its final physical turn writes the
/// root's evidence in its own transaction, settles the batch it was admitted
/// with, and leaves the session with no unfinished root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_queued_headed_root_writes_its_terminal_like_any_root(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("root-terminal-queued");
    let batch = store
        .enqueue_queued_work(checkpoint_admissions::queued_draft(
            &session_id,
            "queued head",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the head batch");
    let authority = seal_drive_fence_for_test(&store, &session_id, "queued-root").await;
    let admission = root_admissions::admitted_on(
        &store,
        &authority,
        &session_id,
        "q",
        AdmittedHead::Batch(batch.batch_id.clone()),
    )
    .await;
    assert_eq!(
        store
            .unfinished_root(&session_id)
            .await
            .expect("read the unfinished root"),
        Some(lash_core::store::UnfinishedRoot {
            root: TurnId::from("q"),
            head: AdmittedHead::Batch(batch.batch_id.clone()),
        })
    );
    assert_eq!(terminal_of(&store, &session_id, "q").await, None);

    let commit = settling_commit_for_test(
        turn_commit(&state(&session_id), "q", Some(ends("q", 0, None)), None),
        &authority,
        completing_admission("q", &admission),
    );
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("the root's final commit lands");
    let terminal = terminal_of(&store, &session_id, "q")
        .await
        .expect("the landed commit wrote the root's evidence");
    assert_eq!(terminal.kind, RootTerminalKind::Answered);
    assert_eq!(terminal.cause, ends("q", 0, None).cause());
    assert_eq!(terminal.head_revision, Some(receipt.head_revision));
    assert_eq!(
        store
            .unfinished_root(&session_id)
            .await
            .expect("read the unfinished root"),
        None,
        "the terminal ends the root"
    );
    assert!(
        store
            .list_open_queued_work(&session_id)
            .await
            .expect("list pending queued work")
            .is_empty(),
        "the final commit settles the batch the root was admitted with"
    );
}
