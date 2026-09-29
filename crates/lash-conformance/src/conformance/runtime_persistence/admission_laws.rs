//! FIG-3927's admission-binding laws over the store surface: a stale fence
//! writes nothing (N4), the command lane binds nothing (N6), a root's
//! terminal commit leaves no row bound to it (N2's commit paths), and a
//! settlement is predicated on the root that holds its rows (N10).

use super::*;
use lash_core::store::{AdmittedHead, DriveFence, IngressSettlement};
use pretty_assertions::assert_eq;

/// Everything a refused write must leave as it found: the session's rows
/// with their bindings, its open queue, its unfinished root and its head.
#[derive(Debug, PartialEq)]
struct DurableIngress {
    inputs: serde_json::Value,
    batches: Vec<crate::BatchId>,
    open_batches: Vec<crate::BatchId>,
    unfinished_root: Option<TurnId>,
    head_revision: u64,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn durable_ingress(
    store: &Arc<dyn RuntimePersistence>,
    session: &SessionId,
) -> DurableIngress {
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
        unfinished_root: store
            .unfinished_root(session)
            .await
            .expect("read the unfinished root")
            .map(|unfinished| unfinished.root),
        head_revision: store
            .load_session_head_meta()
            .await
            .expect("load the head")
            .map_or(0, |meta| meta.head_revision),
    }
}

/// The inputs a read reports admitted to `root`, and the batches a root
/// holds: listed but not open. Each law's session has one root at a time,
/// so a held batch is that root's.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn rows_bound_to(
    store: &Arc<dyn RuntimePersistence>,
    session: &SessionId,
    root: &str,
) -> (Vec<crate::InputId>, Vec<crate::BatchId>) {
    let inputs = store
        .list_pending_turn_inputs(session)
        .await
        .expect("list inputs")
        .into_iter()
        .filter(|read| {
            matches!(
                &read.status,
                crate::PendingTurnInputReadStatus::Admitted { root: holder } if holder.as_str() == root
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
    matches!(result, Err(StoreError::StaleDriveFence { .. }))
}

/// A `Cancelled` stop, as a cancelled root's final turn records it.
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

/// FIG-3927 N2, the commit paths: whatever the root's final commit settles,
/// its terminal write leaves no row bound to the root. The session's roots
/// end one after another: a failed and a cancelled root settle nothing, and
/// an answered root completes its inputs and says nothing of the batches its
/// checkpoint took. After each terminal every row the root held is answered
/// or open again at its position, free for a later root to admit.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_row_stays_bound_after_a_roots_terminal_commit(store: Arc<dyn RuntimePersistence>) {
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
        let fence = seal_drive_fence_for_test(&store, &session, case).await;
        let root = format!("{case}-root");
        // An earlier root's released batch heads the lane at its position.
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
        let admission = admitted_root(&store, &fence, &root, head).await;
        admit_at_checkpoint_for_test(
            &store,
            &fence,
            &TurnId::from(root.as_str()),
            &TurnId::from(root.as_str()),
            crate::CheckpointKind::AfterWork,
            &format!("{root}:checkpoint"),
            64,
            crate::testing::queued_work_admission_policy(64),
        )
        .await
        .expect("admit at the checkpoint");
        let (inputs, batches) = rows_bound_to(&store, &session, &root).await;
        assert!(
            !batches.is_empty(),
            "{case}: the root holds the rows it was admitted with: {inputs:?} {batches:?}"
        );

        let settlement = if stop.is_none() {
            let mut settlement = completing_admission(&root, &admission);
            settlement.completed_batches.clear();
            settlement
        } else {
            IngressSettlement::new(TurnId::from(root.as_str()))
        };
        let mut commit = final_commit(head_commit(&store, &session).await, &fence, settlement);
        if let Some(terminal) = commit.root_terminal.as_mut() {
            terminal.stop = stop.clone();
        }
        store
            .commit_runtime_state(commit)
            .await
            .expect("the root's final commit lands");

        assert_eq!(
            rows_bound_to(&store, &session, &root).await,
            (Vec::new(), Vec::new()),
            "{case}: no row stays bound to a root after its terminal"
        );
        let open = store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches");
        assert!(
            batches
                .iter()
                .all(|held| open.iter().any(|batch| batch.batch_id == *held)),
            "{case}: every batch the root never settled is open again"
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

/// FIG-3946, the terminal-write invariant extended to addressed input: no
/// open row is bound to, or addressed to a turn of, a root with terminal
/// evidence. The turns a root ends are its own physical turns and the turn
/// each member of its admission was accepted under (the member's source
/// key): a member composed into this root never runs as a root of its own.
/// Its terminal write applies the root's disposition to open active-turn
/// input addressed to any of them. `Defer`, with no cancellation recorded,
/// re-opens the row as next-turn input at its own position, and the next
/// root admits it; a recorded `Drop` withdraws the addressed host input and
/// records it on the request's outcome. Input addressed to a turn the root
/// never composed is left alone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_open_row_is_addressed_to_a_turn_of_an_ended_root(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("addressed-to-ended-root");
    for (case, disposition) in [
        ("defer", crate::TurnCancelDisposition::Defer),
        ("drop", crate::TurnCancelDisposition::Drop),
    ] {
        let root = format!("{case}-composing-root");
        let member_turn = TurnId::from(format!("{case}-member-turn"));
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
            &TurnId::from(root.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed to the root's own turn",
        ))
        .await;
        let elsewhere_turn = TurnId::from(format!("{case}-uncomposed-turn"));
        let elsewhere = enqueue(pending_active_turn_input_draft(
            &session,
            &elsewhere_turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed to a turn no root composed",
        ))
        .await;
        let swept = [&addressed, &follow_on, &own]
            .map(|input| input.input_id.clone())
            .to_vec();

        let fence = seal_drive_fence_for_test(&store, &session, &root).await;
        let admission = admitted_root(
            &store,
            &fence,
            &root,
            AdmittedHead::Input(head.input_id.clone()),
        )
        .await;
        assert_eq!(
            admission.input_ids(),
            vec![head.input_id.clone(), member.input_id.clone()],
            "{case}: the root composes the member's acceptance, and no addressed input"
        );
        let address = crate::TurnAddress::new(session.clone(), TurnId::from(root.as_str()));
        if disposition == crate::TurnCancelDisposition::Drop {
            store
                .record_turn_cancel_request(
                    crate::TurnCancelRequest::new(address.clone(), format!("{case}-cancel"), None)
                        .undelivered(disposition),
                )
                .await
                .expect("record the root's cancellation");
        }
        end_root(&store, &fence, completing_admission(&root, &admission)).await;

        let pending = store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs");
        let ended = [
            TurnId::from(root.as_str()),
            TurnId::from(format!("{case}-head-turn")),
            member_turn.clone(),
        ];
        for read in &pending {
            if let crate::TurnInputState::PendingActive(ingress) = &read.input.state {
                assert!(
                    !ended.contains(
                        &lash_core::store::PhysicalTurn::split_turn_id(&ingress.turn_id).0
                    ),
                    "{case}: {} is still addressed to {}, a turn of an ended root",
                    read.input.input_id,
                    ingress.turn_id
                );
            }
        }
        let still_elsewhere = pending
            .iter()
            .find(|read| read.input.input_id == elsewhere.input_id)
            .expect("input addressed elsewhere is still pending");
        assert!(
            matches!(
                &still_elsewhere.input.state,
                crate::TurnInputState::PendingActive(ingress) if ingress.turn_id == elsewhere_turn
            ) && matches!(
                still_elsewhere.status,
                crate::PendingTurnInputReadStatus::Open
            ),
            "{case}: input addressed to a turn no ended root composed is untouched: {:?}",
            still_elsewhere.input.state
        );

        match disposition {
            crate::TurnCancelDisposition::Defer => {
                for input in &swept {
                    let read = pending
                        .iter()
                        .find(|read| read.input.input_id == *input)
                        .expect("a deferred input is still pending");
                    assert!(
                        read.input.state.kind() == crate::TurnInputStateKind::DeferredNextTurn
                            && matches!(read.status, crate::PendingTurnInputReadStatus::Open),
                        "defer: {input} is open next-turn input again: {:?}",
                        read.input.state
                    );
                }
                let next = admitted_root(
                    &store,
                    &fence,
                    "defer-next-root",
                    AdmittedHead::Input(addressed.input_id.clone()),
                )
                .await;
                assert_eq!(
                    next.input_ids(),
                    swept,
                    "defer: the next root admits the re-opened input at its own positions"
                );
                end_root(
                    &store,
                    &fence,
                    completing_admission("defer-next-root", &next),
                )
                .await;
            }
            crate::TurnCancelDisposition::Drop => {
                assert!(
                    pending
                        .iter()
                        .all(|read| !swept.contains(&read.input.input_id)),
                    "drop: the addressed host input is withdrawn"
                );
                let outcome = store
                    .turn_cancel_request(&address)
                    .await
                    .expect("read the root's cancellation")
                    .expect("the root's cancellation is recorded")
                    .outcome
                    .expect("the root's terminal wrote the cancellation's outcome");
                assert_eq!(
                    outcome
                        .affected_inputs
                        .iter()
                        .map(|affected| (affected.input_id.clone(), affected.disposition))
                        .collect::<Vec<_>>(),
                    swept
                        .iter()
                        .map(|input| (input.clone(), crate::TurnCancelDisposition::Drop))
                        .collect::<Vec<_>>(),
                    "drop: the outcome names every withdrawn input"
                );
            }
        }
    }
}

/// FIG-3927 N4: a stale fence writes nothing. After a later seal, the
/// earlier fence's root admission, checkpoint admission, settling commit and
/// command commit are each refused `StaleDriveFence`, and the rows, their
/// bindings, the unfinished root and the head are unchanged. The live fence
/// then settles the same rows.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_stale_fence_writes_nothing(store: Arc<dyn RuntimePersistence>) {
    let session = SessionId::from("stale-fence-writes-nothing");
    let root = "stale-fence-root";
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "fenced input"))
        .await
        .expect("enqueue input");
    let stale = seal_drive_fence_for_test(&store, &session, "stale-fence").await;
    let admission = admitted_root(
        &store,
        &stale,
        root,
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
    let live: DriveFence = seal_drive_fence_for_test(&store, &session, "live-fence").await;
    assert!(live.epoch() > stale.epoch(), "the later seal supersedes");
    let before = durable_ingress(&store, &session).await;

    let readmitted = admit_root_for_test(
        &store,
        &stale,
        &TurnId::from(root),
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await;
    assert!(is_stale_fence(&readmitted), "admit_root: {readmitted:?}");
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &stale,
        &TurnId::from(root),
        &TurnId::from(root),
        crate::CheckpointKind::AfterWork,
        "stale-fence-root:checkpoint",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await;
    assert!(
        is_stale_fence(&checkpoint),
        "admit_at_checkpoint: {checkpoint:?}"
    );
    let settled = try_end_root(&store, &stale, completing_admission(root, &admission)).await;
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
        &TurnId::from(root),
        &TurnId::from(root),
        crate::CheckpointKind::AfterWork,
        "stale-fence-root:checkpoint",
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
    end_root(
        &store,
        &live,
        completing_checkpoint(completing_admission(root, &admission), &checkpoint),
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
            .queued_work_batch_completed(&session, command.batch_id.as_str())
            .await
            .expect("read the command's completion"),
        "the live fence's command commit settles the command"
    );
}

/// FIG-3927 N6: the command lane is bindless. A command row is settled only
/// by the fenced commit that applied it: a host withdrawal that wins the race
/// refuses the applying commit whole (`SessionCommandWithdrawn`) and nothing
/// is written. A command is never bound to a root, by a root's admission or
/// its checkpoint.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_command_lane_is_bindless(store: Arc<dyn RuntimePersistence>) {
    let session = SessionId::from("command-lane-bindless");
    let fence = seal_drive_fence_for_test(&store, &session, "command-lane").await;
    let command_completion = |batch: &crate::QueuedWorkBatch| crate::QueuedWorkCompletion {
        session_id: session.clone(),
        batch_ids: vec![batch.batch_id.clone()],
    };

    // A withdrawal that wins the race refuses the applying commit.
    let withdrawn = store
        .enqueue_queued_work(queued_session_command_draft(&session, "withdrawn"))
        .await
        .expect("enqueue the withdrawn command");
    let run = store
        .open_session_command_run(&fence)
        .await
        .expect("open the command run");
    assert_eq!(
        run.iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![withdrawn.batch_id.clone()]
    );
    assert!(
        store
            .cancel_queued_work_batch(&session, withdrawn.batch_id.as_str())
            .await
            .expect("withdraw the command")
            .is_some(),
        "an open command is withdrawable"
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

    // A command no one withdrew is settled by the commit that applied it.
    let applied = store
        .enqueue_queued_work(queued_session_command_draft(&session, "applied"))
        .await
        .expect("enqueue the applied command");
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
            .queued_work_batch_completed(&session, applied.batch_id.as_str())
            .await
            .expect("read the command's completion"),
        "the applied command is settled"
    );

    // Neither a root's admission nor its checkpoint binds a command: the
    // root is admitted first, and a command enqueued behind it stays open.
    let turn = store
        .enqueue_queued_work(queued_draft(
            &session,
            "turn work",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue turn work");
    let root = "command-lane-root";
    let admission = admitted_root(
        &store,
        &fence,
        root,
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
        &TurnId::from(root),
        &TurnId::from(root),
        crate::CheckpointKind::AfterWork,
        "command-lane-root:checkpoint",
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
        rows_bound_to(&store, &session, root).await.1,
        vec![turn.batch_id.clone()],
        "the root holds its turn work and never the command"
    );
    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open batches")
            .iter()
            .any(|open| open.batch_id == command.batch_id),
        "the command stays open while a root is unfinished"
    );
}

/// ADR 0101 §4 under FIG-3927: a session command enqueued after the drive
/// chose the turn lane never holds a root's head back, whichever table the
/// head is in. The drive admitted the turn lane at a boundary whose command
/// lane was empty, so the root's admission takes the run enqueued before the
/// command, and only the rows enqueued after it wait for the next boundary,
/// where the command applies first.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_command_enqueued_behind_a_roots_head_never_starves_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("command-behind-the-head");
    let fence = seal_drive_fence_for_test(&store, &session, "command-behind-the-head").await;
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

    // An input-headed root.
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
    let root = "input-headed-root";
    let admission = admitted_root(
        &store,
        &fence,
        root,
        AdmittedHead::Input(head.input_id.clone()),
    )
    .await;
    assert_eq!(
        admission.input_ids(),
        vec![head.input_id.clone()],
        "the root takes the inputs enqueued before the command, and none behind it"
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
    end_root(&store, &fence, completing_admission(root, &admission)).await;
    apply(&command).await;
    drive_root_to_end(
        &store,
        &fence,
        "input-behind-root",
        AdmittedHead::Input(behind.input_id.clone()),
    )
    .await;

    // A batch-headed root, the same way.
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
    let admission = admit_root_for_test(
        &store,
        &fence,
        &TurnId::from("batch-headed-root"),
        AdmittedHead::Batch(head.batch_id.clone()),
    )
    .await
    .expect("admit the batch-headed root")
    .expect("a command behind the head never makes the root miss its head");
    assert_eq!(
        admission.batch_ids(),
        vec![head.batch_id.clone()],
        "the root takes the run enqueued before the command, and no row behind it"
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

/// FIG-3927 N10: a settlement is predicated on the root. A commit whose
/// settlement names a row bound to another root, or to none, is refused
/// `IngressRowNotAdmitted` and writes nothing; the owning root then settles
/// its row once, and a second settlement of it is refused because no root
/// holds it any more.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn settlement_is_predicated_on_the_root(store: Arc<dyn RuntimePersistence>) {
    let session = SessionId::from("settlement-predicated-on-root");
    let root = "settlement-owner-root";
    let bound = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "bound input"))
        .await
        .expect("enqueue the bound input");
    let fence = seal_drive_fence_for_test(&store, &session, "settlement-owner").await;
    let admission = admitted_root(
        &store,
        &fence,
        root,
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
    let foreign = refused(completing_admission("another-root", &admission)).await;
    assert!(
        matches!(
            &foreign,
            Err(StoreError::IngressRowNotAdmitted { admitted_root: Some(holder), .. })
                if holder.as_str() == root
        ),
        "a row bound to another root refuses the settlement: {foreign:?}"
    );
    for (row, settlement) in [
        ("input", releasing(root, [input_row(&open_input)])),
        ("batch", releasing(root, [batch_row(&open_batch)])),
    ] {
        let unbound = refused(settlement).await;
        assert!(
            matches!(
                &unbound,
                Err(StoreError::IngressRowNotAdmitted {
                    admitted_root: None,
                    ..
                })
            ),
            "an open {row} refuses the settlement: {unbound:?}"
        );
    }
    let mixed = refused({
        let mut settlement = completing_admission(root, &admission);
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

    let settlement = completing_admission(root, &admission);
    store
        .commit_runtime_state(settling_commit_for_test(
            head_commit(&store, &session).await,
            &fence,
            settlement.clone(),
        ))
        .await
        .expect("the owning root settles its row");
    let again = refused(settlement).await;
    assert!(
        matches!(
            again,
            Err(StoreError::IngressRowNotAdmitted {
                admitted_root: None,
                ..
            })
        ),
        "a settled row is settled once: {again:?}"
    );
}
