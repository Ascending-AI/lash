//! FIG-3927's admission-binding laws over the store surface: a stale fence
//! writes nothing (N4), the command lane binds nothing (N6), a run's
//! terminal commit leaves no row bound to it (N2's commit paths), and a
//! settlement is predicated on the run that holds its rows (N10).

use super::*;
use lash_core::store::{AdmittedHead, IngressSettlement, ShiftFence};
use pretty_assertions::assert_eq;

/// Everything a refused write must leave as it found: the session's rows
/// with their bindings, its open queue, its unfinished run and its head.
#[derive(Debug, PartialEq)]
struct DurableIngress {
    inputs: serde_json::Value,
    batches: Vec<crate::BatchId>,
    open_batches: Vec<crate::BatchId>,
    unfinished_run: Option<TurnId>,
    head_revision: u64,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn durable_ingress(store: &Arc<dyn RuntimeStore>, session: &SessionId) -> DurableIngress {
    let ids = |batches: Vec<crate::QueuedWorkBatch>| {
        batches
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>()
    };
    DurableIngress {
        inputs: serde_json::to_value(
            store
                .list_pending_turn_inputs(session)
                .await
                .expect("list inputs"),
        )
        .expect("encode inputs"),
        batches: ids(store.list_queued_work(session).await.expect("list batches")),
        open_batches: ids(store
            .list_open_queued_work(session)
            .await
            .expect("list open batches")),
        unfinished_run: store
            .unfinished_run(session)
            .await
            .expect("read the unfinished run")
            .map(|unfinished| unfinished.run),
        head_revision: store
            .load_session_head_meta(session)
            .await
            .expect("load the head")
            .map_or(0, |meta| meta.head_revision),
    }
}

/// The inputs a read reports admitted to `run`, and the batches a run
/// holds: listed but not open. Each law's session has one run at a time,
/// so a held batch is that run's. Every listed input's point read
/// (`pending_turn_input`) must answer the status the list answers.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn rows_bound_to(
    store: &Arc<dyn RuntimeStore>,
    session: &SessionId,
    run: &str,
) -> (Vec<crate::InputId>, Vec<crate::BatchId>) {
    let listed = store
        .list_pending_turn_inputs(session)
        .await
        .expect("list inputs");
    for read in &listed {
        assert_eq!(
            store
                .pending_turn_input(session, &read.input.input_id)
                .await
                .expect("read the input by id")
                .map(|by_id| by_id.status),
            Some(read.status.clone()),
            "input `{}` reads by id as the list reads it",
            read.input.input_id
        );
    }
    let inputs = listed
        .into_iter()
        .filter(|read| {
            matches!(
                &read.status,
                crate::PendingTurnInputReadStatus::Admitted { run: holder } if holder.as_str() == run
            )
        })
        .map(|read| read.input.input_id)
        .collect();
    let open = store
        .list_open_queued_work(session)
        .await
        .expect("list open batches")
        .into_iter()
        .map(|batch| batch.batch_id)
        .collect::<Vec<_>>();
    let batches = store
        .list_queued_work(session)
        .await
        .expect("list batches")
        .into_iter()
        .map(|batch| batch.batch_id)
        .filter(|batch| !open.contains(batch))
        .collect();
    (inputs, batches)
}

fn is_stale_fence(result: &Result<impl std::fmt::Debug, StoreError>) -> bool {
    matches!(result, Err(StoreError::StaleShiftFence { .. }))
}

/// A `Cancelled` stop, as a cancelled run's final turn records it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the evidence literal is well formed"
)]
fn cancelled_stop() -> crate::TurnStop {
    crate::TurnStop::Cancelled {
        evidence: serde_json::from_value(serde_json::json!({ "request_id": "no-bound-row" }))
            .expect("decode the cancellation evidence"),
    }
}

/// FIG-3927 N2, the commit paths: whatever the run's final commit settles,
/// its terminal write leaves no row bound to the run. The session's runs
/// end one after another: a failed and a cancelled run settle nothing, and
/// an answered run completes its inputs and says nothing of the batches its
/// checkpoint took. After each terminal every row the run held is answered
/// or open again at its position, free for a later run to admit.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_row_stays_bound_after_a_runs_terminal_commit(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("no-bound-row-commit");
    for (case, stop) in [
        ("failed", Some(crate::TurnStop::ProviderError)),
        ("cancelled", Some(cancelled_stop())),
        ("answered", None),
    ] {
        store
            .enqueue_pending_turn_input(pending_next_turn_input_draft(
                &session,
                &format!("{case} input"),
            ))
            .await
            .expect("enqueue input");
        store
            .enqueue_queued_work(queued_draft(
                &session,
                &format!("{case} batch"),
                DeliveryPolicy::EarliestSafeBoundary,
            ))
            .await
            .expect("enqueue batch");
        let fence = seal_shift_fence_for_test(&store, &session, case).await;
        let run = format!("{case}-run");
        // An earlier run's released batch heads the lane at its position.
        let first_input = store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs")
            .into_iter()
            .find(|read| read.input.state.kind() == crate::TurnInputStateKind::DeferredNextTurn)
            .map(|read| {
                (
                    read.input.enqueue_seq,
                    AdmittedHead::Input(read.input.input_id),
                )
            });
        let first_batch = store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches")
            .into_iter()
            .next()
            .map(|batch| (batch.enqueue_seq, AdmittedHead::Batch(batch.batch_id)));
        let (_, head) = first_input
            .into_iter()
            .chain(first_batch)
            .min_by_key(|(seq, _)| *seq)
            .expect("the turn lane has a head");
        let admission = admitted_run(&store, &fence, &run, head).await;
        admit_at_checkpoint_for_test(
            &store,
            &fence,
            &TurnId::fixture(run.as_str()),
            &TurnId::fixture(run.as_str()),
            crate::CheckpointKind::AfterWork,
            &format!("{run}:checkpoint"),
            64,
            crate::testing::queued_work_admission_policy(64),
        )
        .await
        .expect("admit at the checkpoint");
        let (inputs, batches) = rows_bound_to(&store, &session, &run).await;
        assert!(
            !batches.is_empty(),
            "{case}: the run holds the rows it was admitted with: {inputs:?} {batches:?}"
        );

        let settlement = if stop.is_none() {
            let mut settlement = completing_admission(&run, &admission);
            settlement.completed_batches.clear();
            settlement
        } else {
            IngressSettlement::new(TurnId::fixture(run.as_str()))
        };
        let mut commit = final_commit(head_commit(&store, &session).await, &fence, settlement);
        if let (Some(terminal), Some(stop)) = (commit.run_terminal.as_mut(), &stop) {
            terminal.outcome = lash_core::store::RunCommittedOutcome::Stopped(stop.clone());
        }
        store
            .commit_runtime_state(commit)
            .await
            .expect("the run's final commit lands");

        assert_eq!(
            rows_bound_to(&store, &session, &run).await,
            (Vec::new(), Vec::new()),
            "{case}: no row stays bound to a run after its terminal"
        );
        let open = store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches");
        assert!(
            batches
                .iter()
                .all(|held| open.iter().any(|batch| batch.batch_id == *held)),
            "{case}: every batch the run never settled is open again"
        );
        let pending = store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs");
        for held in &inputs {
            let read = pending.iter().find(|read| read.input.input_id == *held);
            match stop {
                None => assert!(read.is_none(), "answered: {held} is no longer pending"),
                Some(_) => assert!(
                    read.is_some_and(|read| matches!(
                        read.status,
                        crate::PendingTurnInputReadStatus::Open
                    )),
                    "{case}: the unanswered {held} is open again"
                ),
            }
        }
    }
}

/// FIG-3946, the terminal-write invariant extended to addressed input: every
/// open row addressed to a turn of a run with terminal evidence is
/// next-turn input. The turns a run ends are its own physical turns and the
/// turn each member of its admission was accepted under (the member's source
/// key): a member composed into this run never runs as a run of its own,
/// so input addressed to it is accepted while the run executes. Its terminal
/// write applies the run's disposition to that input. `Defer`, with no
/// cancellation recorded, writes nothing: the row keeps its submitted
/// delivery and is next-turn input by rule at its own position (ADR 0101
/// §5.1), and the next run admits it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn open_input_addressed_to_an_ended_run_is_next_turn_input(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("addressed-to-ended-run");
    {
        let case = "defer";
        let run = format!("{case}-composing-run");
        let member_turn = TurnId::fixture(format!("{case}-member-turn"));
        let enqueue = |draft: crate::PendingTurnInputDraft| {
            let store = Arc::clone(&store);
            async move {
                store
                    .enqueue_pending_turn_input(draft)
                    .await
                    .expect("enqueue input")
            }
        };
        let head = enqueue(
            pending_next_turn_input_draft(&session, "head")
                .with_source_key(format!("{case}-head-turn")),
        )
        .await;
        let member = enqueue(
            pending_next_turn_input_draft(&session, "member").with_source_key(member_turn.as_str()),
        )
        .await;
        let fence = seal_shift_fence_for_test(&store, &session, &run).await;
        let admission = admitted_run(
            &store,
            &fence,
            &run,
            AdmittedHead::Input(head.input_id.clone()),
        )
        .await;
        assert_eq!(
            admission.input_ids(),
            vec![head.input_id.clone(), member.input_id.clone()],
            "{case}: the run composes the member's acceptance"
        );

        // While the run executes, input may address any turn it runs.
        let addressed = enqueue(pending_active_turn_input_draft(
            &session,
            &member_turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed to the member's turn",
        ))
        .await;
        let follow_on = enqueue(pending_active_turn_input_draft(
            &session,
            &lash_core::store::PhysicalTurn::derive_turn_id(&member_turn, 1),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed to the member turn's follow-on",
        ))
        .await;
        let own = enqueue(pending_active_turn_input_draft(
            &session,
            &TurnId::fixture(run.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed to the run's own turn",
        ))
        .await;
        let addressed_rows = [&addressed, &follow_on, &own];
        let swept = addressed_rows.map(|input| input.input_id.clone()).to_vec();

        end_run(&store, &fence, completing_admission(&run, &admission)).await;

        let pending = store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs");
        for read in &pending {
            assert!(
                read.input.state.is_next_turn_input(None),
                "{case}: {} is open and addressed to a turn of an ended run, so it is \
                 next-turn input: {:?}",
                read.input.input_id,
                read.input.state
            );
        }

        for input in addressed_rows {
            let read = pending
                .iter()
                .find(|read| read.input.input_id == input.input_id)
                .expect("a deferred input is still pending");
            assert!(
                read.input.state == input.state
                    && matches!(read.status, crate::PendingTurnInputReadStatus::Open),
                "defer: {} is open with its submitted delivery: {:?}",
                input.input_id,
                read.input.state
            );
        }
        let next = admitted_run(
            &store,
            &fence,
            "defer-next-run",
            AdmittedHead::Input(addressed.input_id.clone()),
        )
        .await;
        assert_eq!(
            next.input_ids(),
            swept,
            "defer: the next run admits the deferred input at its own positions"
        );
        end_run(
            &store,
            &fence,
            completing_admission("defer-next-run", &next),
        )
        .await;
    }
}

/// FIG-3927 N4: a stale fence writes nothing. After a later seal, the
/// earlier fence's run admission, checkpoint admission, settling commit and
/// command commit are each refused `StaleShiftFence`, and the rows, their
/// bindings, the unfinished run and the head are unchanged. The live fence
/// then settles the same rows.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_stale_fence_writes_nothing(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("stale-fence-writes-nothing");
    let run = "stale-fence-run";
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "fenced input"))
        .await
        .expect("enqueue input");
    let stale = seal_shift_fence_for_test(&store, &session, "stale-fence").await;
    let admission = admitted_run(
        &store,
        &stale,
        run,
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await;
    let batch = store
        .enqueue_queued_work(queued_draft(
            &session,
            "checkpoint batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue batch");
    let command = store
        .enqueue_queued_work(queued_session_command_draft(&session, "fenced command"))
        .await
        .expect("enqueue command");
    let live: ShiftFence = seal_shift_fence_for_test(&store, &session, "live-fence").await;
    assert!(live.epoch() > stale.epoch(), "the later seal supersedes");
    let before = durable_ingress(&store, &session).await;

    let readmitted = admit_run_for_test(
        &store,
        &stale,
        &TurnId::from(run),
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await;
    assert!(is_stale_fence(&readmitted), "admit_run: {readmitted:?}");
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &stale,
        &TurnId::from(run),
        &TurnId::from(run),
        crate::CheckpointKind::AfterWork,
        "stale-fence-run:checkpoint",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await;
    assert!(
        is_stale_fence(&checkpoint),
        "admit_at_checkpoint: {checkpoint:?}"
    );
    let settled = try_end_run(&store, &stale, completing_admission(run, &admission)).await;
    assert!(is_stale_fence(&settled), "settling commit: {settled:?}");
    let applied = store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &stale,
            crate::QueuedWorkCompletion {
                session_id: session.clone(),
                batch_ids: vec![command.batch_id.clone()],
            },
        ))
        .await;
    assert!(is_stale_fence(&applied), "command commit: {applied:?}");
    assert_eq!(
        durable_ingress(&store, &session).await,
        before,
        "a stale fence's refused writes change nothing"
    );

    // The live fence settles what the stale one could not.
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &live,
        &TurnId::from(run),
        &TurnId::from(run),
        crate::CheckpointKind::AfterWork,
        "stale-fence-run:checkpoint",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("the live fence admits at the checkpoint");
    assert_eq!(
        checkpoint
            .queued
            .as_ref()
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        vec![batch.batch_id.clone()]
    );
    end_run(
        &store,
        &live,
        completing_checkpoint(completing_admission(run, &admission), &checkpoint),
    )
    .await;
    store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &live,
            crate::QueuedWorkCompletion {
                session_id: session.clone(),
                batch_ids: vec![command.batch_id.clone()],
            },
        ))
        .await
        .expect("the live fence applies the command");
    assert!(
        store
            .queued_work_batch_completion(&session, command.batch_id.as_str())
            .await
            .expect("read the command's completion")
            .is_some(),
        "the live fence's command commit settles the command"
    );
}

/// FIG-3927 N6: the command lane is bindless. A command row is settled only
/// by the fenced commit that applied it. A host withdraws a command until a
/// shift's fenced read of the lane admits it (FIG-4202); a commit that names
/// a withdrawn command is refused whole (`SessionCommandWithdrawn`) and
/// nothing is written, and a read command is no longer withdrawn. A command
/// is never bound to a run, by a run's admission or its checkpoint.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_command_lane_is_bindless(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("command-lane-bindless");
    let fence = seal_shift_fence_for_test(&store, &session, "command-lane").await;
    let command_completion = |batch: &crate::QueuedWorkBatch| crate::QueuedWorkCompletion {
        session_id: session.clone(),
        batch_ids: vec![batch.batch_id.clone()],
    };

    // A withdrawal before any shift read the command removes it, and a
    // commit that names it anyway is refused with nothing written.
    let withdrawn = store
        .enqueue_queued_work(queued_session_command_draft(&session, "withdrawn"))
        .await
        .expect("enqueue the withdrawn command");
    assert!(
        store
            .cancel_queued_work_batch(&session, withdrawn.batch_id.as_str())
            .await
            .expect("withdraw the command")
            .is_some(),
        "a command no shift read is withdrawable"
    );
    let before = durable_ingress(&store, &session).await;
    let refused = store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &fence,
            command_completion(&withdrawn),
        ))
        .await;
    assert!(
        matches!(refused, Err(StoreError::SessionCommandWithdrawn { .. })),
        "a withdrawn command refuses its applying commit: {refused:?}"
    );
    assert_eq!(
        durable_ingress(&store, &session).await,
        before,
        "a refused command commit writes nothing"
    );

    // The lane's read admits the command (FIG-4202): a withdrawal no longer
    // reaches it, and the commit that applies it settles it.
    let applied = store
        .enqueue_queued_work(queued_session_command_draft(&session, "applied"))
        .await
        .expect("enqueue the applied command");
    let command_run = store
        .open_session_command_run(&fence)
        .await
        .expect("open the command run");
    assert_eq!(
        command_run
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![applied.batch_id.clone()]
    );
    assert!(
        store
            .cancel_queued_work_batch(&session, applied.batch_id.as_str())
            .await
            .expect("try to withdraw the read command")
            .is_none(),
        "a command the lane read is admitted, and no longer withdrawn"
    );
    store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &fence,
            command_completion(&applied),
        ))
        .await
        .expect("the applying commit settles its command");
    assert!(
        store
            .queued_work_batch_completion(&session, applied.batch_id.as_str())
            .await
            .expect("read the command's completion")
            .is_some(),
        "the applied command is settled"
    );

    // Neither a run's admission nor its checkpoint binds a command: the
    // run is admitted first, and a command enqueued behind it stays open.
    let turn = store
        .enqueue_queued_work(queued_draft(
            &session,
            "turn work",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue turn work");
    let run = "command-lane-run";
    let admission = admitted_run(
        &store,
        &fence,
        run,
        AdmittedHead::Batch(turn.batch_id.clone()),
    )
    .await;
    let command = store
        .enqueue_queued_work(queued_session_command_draft(&session, "behind the turn"))
        .await
        .expect("enqueue a command behind the turn");
    assert_eq!(admission.batch_ids(), vec![turn.batch_id.clone()]);
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &TurnId::from(run),
        &TurnId::from(run),
        crate::CheckpointKind::AfterWork,
        "command-lane-run:checkpoint",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("admit at the checkpoint");
    assert!(
        checkpoint
            .queued
            .as_ref()
            .is_none_or(|queued| !queued.batch_ids().contains(&command.batch_id)),
        "a checkpoint never binds a command"
    );
    assert_eq!(
        rows_bound_to(&store, &session, run).await.1,
        vec![turn.batch_id.clone()],
        "the run holds its turn work and never the command"
    );
    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches")
            .iter()
            .any(|open| open.batch_id == command.batch_id),
        "the command stays open while a run is unfinished"
    );
}

/// ADR 0101 §4 under FIG-3927: a session command enqueued after the shift
/// chose the turn lane never holds a run's head back, whichever table the
/// head is in. The shift admitted the turn lane at a boundary whose command
/// lane was empty, so the run's admission takes the run enqueued before the
/// command, and only the rows enqueued after it wait for the next boundary,
/// where the command applies first.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_command_enqueued_behind_a_runs_head_never_starves_it(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("command-behind-the-head");
    let fence = seal_shift_fence_for_test(&store, &session, "command-behind-the-head").await;
    let apply = |command: &crate::QueuedWorkBatch| {
        let store = Arc::clone(&store);
        let session = session.clone();
        let fence = fence.clone();
        let command = command.batch_id.clone();
        async move {
            store
                .commit_runtime_state(applying_commands(
                    head_commit(&store, &session).await,
                    &fence,
                    crate::QueuedWorkCompletion {
                        session_id: session.clone(),
                        batch_ids: vec![command],
                    },
                ))
                .await
                .expect("the command applies at the next boundary");
        }
    };

    // An input-headed run.
    let head = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "input head"))
        .await
        .expect("enqueue the head input");
    let command = store
        .enqueue_queued_work(queued_session_command_draft(&session, "after the input"))
        .await
        .expect("enqueue a command behind the head input");
    let behind = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session,
            "input behind the command",
        ))
        .await
        .expect("enqueue an input behind the command");
    let run = "input-headed-run";
    let admission = admitted_run(
        &store,
        &fence,
        run,
        AdmittedHead::Input(head.input_id.clone()),
    )
    .await;
    assert_eq!(
        admission.input_ids(),
        vec![head.input_id.clone()],
        "the run takes the inputs enqueued before the command, and none behind it"
    );
    let pending = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list inputs");
    assert!(
        pending
            .iter()
            .any(|read| read.input.input_id == behind.input_id
                && matches!(read.status, crate::PendingTurnInputReadStatus::Open)),
        "the input behind the command stays open"
    );
    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches")
            .iter()
            .any(|batch| batch.batch_id == command.batch_id),
        "the command stays open for the next boundary"
    );
    // The next boundary applies the command, and the input behind it is
    // then the lane's head.
    end_run(&store, &fence, completing_admission(run, &admission)).await;
    apply(&command).await;
    execute_run_to_end(
        &store,
        &fence,
        "input-behind-run",
        AdmittedHead::Input(behind.input_id.clone()),
    )
    .await;

    // A batch-headed run, the same way.
    let head = store
        .enqueue_queued_work(queued_draft(
            &session,
            "batch head",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue the head batch");
    let command = store
        .enqueue_queued_work(queued_session_command_draft(&session, "after the head"))
        .await
        .expect("enqueue a command behind the head");
    let behind = store
        .enqueue_queued_work(queued_draft(
            &session,
            "batch behind the command",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue a batch behind the command");
    let admission = admit_run_for_test(
        &store,
        &fence,
        &TurnId::from("batch-headed-run"),
        AdmittedHead::Batch(head.batch_id.clone()),
    )
    .await
    .expect("admit the batch-headed run")
    .expect("a command behind the head never makes the run miss its head");
    assert_eq!(
        admission.batch_ids(),
        vec![head.batch_id.clone()],
        "the run takes the run enqueued before the command, and no row behind it"
    );
    assert_eq!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches")
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>(),
        vec![command.batch_id.clone(), behind.batch_id.clone()],
        "the command and the row behind it stay open for the next boundary"
    );
}

/// FIG-3927 N10: a settlement is predicated on the run. A commit whose
/// settlement names a row bound to another run, or to none, is refused
/// `IngressRowNotAdmitted` and writes nothing; the owning run then settles
/// its row once, and a second settlement of it is refused because no run
/// holds it any more.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn settlement_is_predicated_on_the_run(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("settlement-predicated-on-run");
    let run = "settlement-owner-run";
    let bound = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "bound input"))
        .await
        .expect("enqueue the bound input");
    let fence = seal_shift_fence_for_test(&store, &session, "settlement-owner").await;
    let admission = admitted_run(
        &store,
        &fence,
        run,
        AdmittedHead::Input(bound.input_id.clone()),
    )
    .await;
    assert_eq!(admission.input_ids(), vec![bound.input_id.clone()]);
    let open_input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "open input"))
        .await
        .expect("enqueue an open input");
    let open_batch = store
        .enqueue_queued_work(queued_draft(
            &session,
            "open batch",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue an open batch");
    let before = durable_ingress(&store, &session).await;

    let refused = |settlement: IngressSettlement| {
        let store = Arc::clone(&store);
        let session = session.clone();
        let fence = fence.clone();
        async move {
            let commit =
                settling_commit_for_test(head_commit(&store, &session).await, &fence, settlement);
            store.commit_runtime_state(commit).await
        }
    };
    let foreign = refused(completing_admission("another-run", &admission)).await;
    assert!(
        matches!(
            &foreign,
            Err(StoreError::IngressRowNotAdmitted { admitted_run: Some(holder), .. })
                if holder.as_str() == run
        ),
        "a row bound to another run refuses the settlement: {foreign:?}"
    );
    for (row, settlement) in [
        ("input", releasing(run, [input_row(&open_input)])),
        ("batch", releasing(run, [batch_row(&open_batch)])),
    ] {
        let unbound = refused(settlement).await;
        assert!(
            matches!(
                &unbound,
                Err(StoreError::IngressRowNotAdmitted {
                    admitted_run: None,
                    ..
                })
            ),
            "an open {row} refuses the settlement: {unbound:?}"
        );
    }
    let mixed = refused({
        let mut settlement = completing_admission(run, &admission);
        settlement.released.push(input_row(&open_input));
        settlement
    })
    .await;
    assert!(
        matches!(mixed, Err(StoreError::IngressRowNotAdmitted { .. })),
        "one foreign row refuses the whole settlement: {mixed:?}"
    );
    assert_eq!(
        durable_ingress(&store, &session).await,
        before,
        "a refused settlement writes nothing"
    );

    let settlement = completing_admission(run, &admission);
    store
        .commit_runtime_state(settling_commit_for_test(
            head_commit(&store, &session).await,
            &fence,
            settlement.clone(),
        ))
        .await
        .expect("the owning run settles its row");
    let again = refused(settlement).await;
    assert!(
        matches!(
            again,
            Err(StoreError::IngressRowNotAdmitted {
                admitted_run: None,
                ..
            })
        ),
        "a settled row is settled once: {again:?}"
    );
}
