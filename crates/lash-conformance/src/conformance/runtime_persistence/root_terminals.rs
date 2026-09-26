//! A logical root's terminal evidence and the drive fence, as store laws
//! (FIG-3600 S7, ADR 0105 §2): the head commit of a root's final physical
//! turn writes the root's evidence in its own transaction, a queued run's
//! failed or empty settlement writes it in the settlement's, a root keeps
//! the first terminal it reached, and a commit sealed under an admission a
//! successor superseded is refused before it writes anything.

use super::*;
use lash_core::store::{
    AdmissionId, BeginQueuedRun, DriveEpochSeal, DriveFence, QueuedRunRequest, RootStartNonce,
    RootTerminalCause, RootTerminalKind, RootTerminalWrite, TurnCommitId,
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
    let turn = lash_core::store::QueuedRunPosition::derive_turn_id(&root, u64::from(ordinal));
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

/// L-T3, the settlement half: a queued run that settles failed, or empty,
/// ends its root without a head commit, and the settlement's own
/// transaction writes the root's evidence. The root is the run's scope.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_settled_queued_run_writes_its_roots_terminal(store: Arc<dyn RuntimePersistence>) {
    use lash_core::store::{QueuedRunCommit, QueuedRunProgress, QueuedRunTerminal};

    // Each run is admitted in a session of its own and selects nothing (an
    // empty session's frozen empty selection), which both settlements accept.
    let settle = |session: &'static str, terminal: QueuedRunTerminal| {
        let store = Arc::clone(&store);
        let session = SessionId::from(session);
        async move {
            let lease =
                claim_session_execution_lease_for_test(&store, &session, "settled-run").await;
            let authority = lease.authority();
            let request = BeginQueuedRun {
                session_id: session.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state(&session), &[])
                    .config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            };
            let run = store
                .begin_or_resume_queued_run(&authority, request)
                .await
                .expect("admit the run");
            let root = TurnId::from(run.scope.id());
            assert_eq!(
                store
                    .root_terminal(&session, &root)
                    .await
                    .expect("read the root's terminal evidence"),
                None,
                "an admitted run has no evidence yet"
            );
            let selection = store
                .select_queued_run(
                    &authority,
                    &run.scope,
                    &lease.owner,
                    64,
                    &run.configuration,
                    lash_core::testing::queued_work_claim_policy(64),
                )
                .await
                .expect("select the run's members");
            assert_eq!(selection.admission.members, Some(Vec::new()));
            store
                .settle_queued_run(
                    &authority,
                    QueuedRunCommit {
                        scope: run.scope.clone(),
                        expected_revision: selection.admission.revision,
                        progress: QueuedRunProgress::Settle { terminal },
                    },
                )
                .await
                .expect("settle the run");
            store
                .root_terminal(&session, &root)
                .await
                .expect("read the root's terminal evidence")
                .expect("the settlement wrote the root's evidence")
        }
    };

    let failed = settle(
        "root-terminal-settled",
        QueuedRunTerminal::Failed {
            code: crate::RuntimeErrorCode::QueuedWork,
            message: "the run failed before any commit".to_string(),
        },
    )
    .await;
    assert_eq!(failed.kind, RootTerminalKind::Failed);
    assert_eq!(
        failed.cause,
        RootTerminalCause::SettledFailed {
            code: crate::RuntimeErrorCode::QueuedWork,
            message: "the run failed before any commit".to_string(),
        }
    );
    assert_eq!(failed.head_revision, None, "a settlement moves no head");
    assert_eq!(failed.commit(), None);

    let empty = settle("root-terminal-settled-empty", QueuedRunTerminal::Empty).await;
    assert_eq!(empty.kind, RootTerminalKind::Answered);
    assert_eq!(empty.cause, RootTerminalCause::SettledEmpty);
}
