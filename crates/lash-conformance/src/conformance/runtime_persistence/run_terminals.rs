//! A logical run's terminal evidence and the shift fence, as store laws
//! (FIG-3600 S7, ADR 0105 §2): the head commit of a run's final physical
//! turn writes the run's evidence in its own transaction, whichever
//! turn-lane family heads the run, a run keeps
//! the first terminal it reached, and a commit sealed under an admission a
//! successor superseded is refused before it writes anything.

use super::*;
use lash_core::store::{
    AdmissionId, AdmittedHead, RunStartNonce, RunTerminalKind, RunTerminalWrite, ShiftEpochSeal,
    ShiftFence, TurnCommitId,
};
use pretty_assertions::assert_eq;

fn state(session_id: &SessionId) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    state
}

/// The terminal commit of physical turn `turn` over `state`, carrying
/// `run_terminal` and fenced by `shift_fence`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a derivable graph is established by the setup"
)]
fn turn_commit(
    state: &RuntimeSessionState,
    turn: &str,
    run_terminal: Option<RunTerminalWrite>,
    shift_fence: Option<ShiftFence>,
) -> RuntimeCommit {
    let operation = crate::OperationId::turn(
        state.session_id.clone(),
        TurnId::fixture(turn.to_string()),
        "final",
    );
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit =
        RuntimeCommit::persisted_state_with_graph_commit_and_operation(state, graph, operation)
            .expect("build the commit");
    commit.run_terminal = run_terminal.map(Box::new);
    commit.shift_fence = shift_fence.map(Box::new);
    commit
}

/// What the commit of physical turn `ordinal` of `run` writes when it ends
/// the run with `stop`.
fn ends(run: &str, ordinal: u32, stop: Option<crate::TurnStop>) -> RunTerminalWrite {
    let run = TurnId::fixture(run);
    let turn = lash_core::store::PhysicalTurn::derive_turn_id(&run, u64::from(ordinal));
    RunTerminalWrite {
        commit: TurnCommitId::new(run.clone(), ordinal),
        run,
        turn,
        outcome: match stop {
            None => lash_core::store::RunCommittedOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage {
                    text: String::new(),
                },
            ),
            Some(stop) => lash_core::store::RunCommittedOutcome::Stopped(stop),
        },
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn terminal_of(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    run: &str,
) -> Option<lash_core::store::RunTerminal> {
    store
        .run_terminal(session_id, &TurnId::fixture(run))
        .await
        .expect("read the run's terminal evidence")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn head_revision(store: &Arc<dyn RuntimeStore>, session_id: &SessionId) -> u64 {
    store
        .load_session_head_meta(session_id)
        .await
        .expect("load the head")
        .map_or(0, |meta| meta.head_revision)
}

/// L-T1 and L-S6: a run's terminal evidence commits in the head transaction
/// of its final physical turn. A commit refused at its head compare-and-set
/// leaves none; the landed commit writes exactly its cause; a retry of it
/// rewrites nothing; and a later commit naming another terminal for the same
/// run is refused whole, the first terminal standing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn run_terminal_evidence_commits_in_the_head_transaction(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("run-terminal-head");
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
        .expect("the run's final commit lands");
    let terminal = terminal_of(&store, &session_id, "r")
        .await
        .expect("the landed commit wrote the run's evidence");
    assert_eq!(terminal.kind(), RunTerminalKind::Answered);
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

    let revision = head_revision(&store, &session_id).await;
    let resumed = loaded_conformance_state(&store, &session_id).await;
    let other = turn_commit(
        &resumed,
        "r:1",
        Some(ends("r", 1, Some(crate::TurnStop::ToolFailure))),
        None,
    );
    assert!(
        matches!(
            store.commit_runtime_state(other).await,
            Err(crate::StoreError::RunAlreadyTerminal { .. })
        ),
        "a second, different terminal is refused"
    );
    assert_eq!(
        head_revision(&store, &session_id).await,
        revision,
        "the refused commit moved no head"
    );
    assert_eq!(
        terminal_of(&store, &session_id, "r").await,
        Some(terminal),
        "the first terminal stands"
    );
    for (session, run) in [("scope-id:a:b", "c"), ("scope-id:a", "b:c")] {
        let session_id = SessionId::from(session);
        store
            .admit_session(&crate::testing::store_fixtures::root_session_request(
                &session_id,
            ))
            .await
            .expect("admit delimiter-bearing session");
        let state = self::state(&session_id);
        store
            .commit_runtime_state(turn_commit(&state, run, Some(ends(run, 0, None)), None))
            .await
            .expect("distinct typed scope-close keys cannot collide");
        assert!(terminal_of(&store, &session_id, run).await.is_some());
    }
}

/// The successor-sealed commit refusal (ADR 0105 §2, Q-B3): a run's commit
/// carries the fence its admission was sealed with, and a successor's seal
/// makes that fence stale. The stale commit is refused
/// [`StaleShiftFence`](crate::StoreError::StaleShiftFence) before it writes
/// anything, head or evidence; the successor's own commit lands.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_commit_sealed_under_a_superseded_admission_is_refused(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("run-terminal-fence");
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
                .seal_shift_epoch(
                    &session_id,
                    &AdmissionId::new(admission),
                    observed,
                    &RunStartNonce::new(admission),
                    None,
                )
                .await
                .expect("seal the admission")
            {
                ShiftEpochSeal::Sealed(fence) => fence,
                other => panic!("admission `{admission}` seals: {other:?}"),
            }
        }
    };
    let stale = seal("shift-a#0", 0).await;
    let current = seal("shift-b#0", 1).await;
    assert_eq!((stale.epoch(), current.epoch()), (1, 2));

    let resumed = loaded_conformance_state(&store, &session_id).await;
    let revision = head_revision(&store, &session_id).await;
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
            Err(crate::StoreError::StaleShiftFence {
                fence_epoch: 1,
                current_epoch: 2,
                ..
            })
        ),
        "a commit sealed under a superseded admission is refused"
    );
    assert_eq!(
        head_revision(&store, &session_id).await,
        revision,
        "no head moved"
    );
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

/// L-T3 for a queued-headed run: a run admitted on a queued-work batch
/// ends like any run. The head commit of its final physical turn writes the
/// run's evidence in its own transaction, settles the batch it was admitted
/// with, and leaves the session with no unfinished run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_queued_headed_run_writes_its_terminal_like_any_run(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("run-terminal-queued");
    let batch = store
        .enqueue_queued_work(checkpoint_admissions::queued_draft(
            &session_id,
            "queued head",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the head batch");
    let authority = seal_shift_fence_for_test(&store, &session_id, "queued-run").await;
    let admission = run_admissions::admitted_on(
        &store,
        &authority,
        &session_id,
        "q",
        AdmittedHead::Batch(batch.batch_id.clone()),
    )
    .await;
    assert_eq!(
        store
            .unfinished_run(&session_id)
            .await
            .expect("read the unfinished run"),
        Some(lash_core::store::UnfinishedRun {
            run: TurnId::from("q"),
            head: AdmittedHead::Batch(batch.batch_id.clone()),
            executor: admission.executor.clone(),
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
        .expect("the run's final commit lands");
    let terminal = terminal_of(&store, &session_id, "q")
        .await
        .expect("the landed commit wrote the run's evidence");
    assert_eq!(terminal.kind(), RunTerminalKind::Answered);
    assert_eq!(terminal.cause, ends("q", 0, None).cause());
    assert_eq!(terminal.head_revision, Some(receipt.head_revision));
    assert_eq!(
        store
            .unfinished_run(&session_id)
            .await
            .expect("read the unfinished run"),
        None,
        "the terminal ends the run"
    );
    assert!(
        store
            .list_open_queued_work(&session_id)
            .await
            .expect("list pending queued work")
            .is_empty(),
        "the final commit settles the batch the run was admitted with"
    );
}
