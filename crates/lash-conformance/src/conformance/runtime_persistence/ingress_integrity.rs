//! The session ingress's integrity laws (ADR 0101 §5.1, §5.2, §8): a
//! submitted delivery is never rewritten, a turn address the session cannot
//! name is refused before anything is allocated, every kind answers a
//! resubmission by its digest, every kind leaves a tombstone with a closed
//! cause, and `authority` and `merge_key` are per-item data rather than
//! composition gates.

use super::*;
use crate::conformance::admission_support::*;
use lash_core::store::{AdmittedHead, IngressTerminal, IngressTerminalCause};
use pretty_assertions::assert_eq;

/// A process wake of `process` at `sequence` carrying `text`, keyed by the
/// wake's own source key.
fn wake(session: &SessionId, process: &str, sequence: u64, text: &str) -> QueuedWorkBatchDraft {
    crate::conformance::helpers::process_wake_work(
        session,
        process,
        sequence,
        text,
        DeliveryPolicy::EarliestSafeBoundary,
    )
}

/// A `RefreshToolCatalog` command for `reason` filed under `source_key`.
fn keyed_command(session: &SessionId, source_key: &str, reason: &str) -> QueuedWorkBatchDraft {
    queued_session_command_draft(session, reason).with_source_key(source_key)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn read_input(
    store: &Arc<dyn RuntimeStore>,
    session: &SessionId,
    input_id: &crate::InputId,
) -> crate::PendingTurnInputRead {
    store
        .pending_turn_input(session, input_id)
        .await
        .expect("read the input")
        .expect("the input is open")
}

/// The stored row an identical resubmission of `draft` answers: a terminal
/// input is no longer listed, so its tombstone is read back this way.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own resubmissions"
)]
async fn resubmitted_input(
    store: &Arc<dyn RuntimeStore>,
    draft: crate::PendingTurnInputDraft,
    original: &crate::PendingTurnInput,
) -> crate::PendingTurnInput {
    let existing = store
        .enqueue_pending_turn_input(draft)
        .await
        .expect("an identical resubmission is answered");
    assert_eq!(existing.input_id, original.input_id);
    assert_eq!(existing.enqueue_seq, original.enqueue_seq);
    existing
}

/// The terminal of a batch an identical resubmission answers: it must be
/// answered as the existing batch, never enqueued again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn resubmitted_terminal(
    store: &Arc<dyn RuntimeStore>,
    draft: QueuedWorkBatchDraft,
    original: &QueuedWorkBatch,
) -> Option<IngressTerminal> {
    let outcome = store
        .enqueue_queued_work_with_outcome(draft)
        .await
        .expect("an identical resubmission is answered");
    let crate::QueuedWorkEnqueueOutcome::Existing(existing) = outcome else {
        panic!("an identical resubmission must answer the stored batch, not enqueue another");
    };
    assert_eq!(existing.batch_id, original.batch_id);
    assert_eq!(existing.enqueue_seq, original.enqueue_seq);
    assert_eq!(existing.submission_digest, original.submission_digest);
    assert!(
        serde_json::to_value(&existing.payload).expect("encode payload")
            == serde_json::to_value(&original.payload).expect("encode original payload"),
        "the tombstone keeps its submission"
    );
    existing.terminal
}

/// ADR 0101 §5.1: an input's submitted delivery is written once. An input
/// addressed to a running root's turn that no checkpoint admitted keeps that
/// address when the root ends: it is next-turn input by rule, at its own
/// sequence position, with no rewrite of its stored delivery, and the next
/// root admits it ahead of later input.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_deferred_input_keeps_its_submitted_delivery(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("ingress-immutable-delivery");
    let head = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session, "head").with_source_key("delivery-root"),
        )
        .await
        .expect("enqueue the head");
    let fence = seal_drive_fence_for_test(&store, &session, "delivery-owner").await;
    let admission = admitted_root(
        &store,
        &fence,
        "delivery-root",
        AdmittedHead::Input(head.input_id.clone()),
    )
    .await;
    let root_turn = TurnId::from("delivery-root");
    let addressed_draft = pending_active_turn_input_draft(
        &session,
        &root_turn,
        crate::TurnInputCheckpointBoundary::AfterWork,
        "addressed to the running root",
    )
    .with_source_key("addressed-input");
    let addressed = store
        .enqueue_pending_turn_input(addressed_draft.clone())
        .await
        .expect("an address to the running root is accepted");
    let submitted = addressed.state.ingress();
    let later = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session, "later"))
        .await
        .expect("enqueue later next-turn input");
    end_root(
        &store,
        &fence,
        completing_admission("delivery-root", &admission),
    )
    .await;

    let after = read_input(&store, &session, &addressed.input_id).await;
    assert_eq!(
        after.input.state.ingress(),
        submitted,
        "the root's end must not rewrite the input's submitted delivery"
    );
    assert!(
        matches!(after.status, crate::PendingTurnInputReadStatus::Open),
        "the input is open after its turn ended: {:?}",
        after.status
    );
    assert!(
        after.input.state.is_next_turn_input(None),
        "with no turn running the addressed input is next-turn input by rule"
    );
    let next = admitted_root(
        &store,
        &fence,
        "delivery-next",
        AdmittedHead::Input(addressed.input_id.clone()),
    )
    .await;
    assert_eq!(
        next.input_ids(),
        vec![addressed.input_id.clone(), later.input_id.clone()],
        "the next root admits the addressed input at its own position"
    );
    end_root(&store, &fence, completing_admission("delivery-next", &next)).await;
    let delivered = resubmitted_input(&store, addressed_draft, &addressed).await;
    assert_eq!(delivered.state.ingress(), submitted);
    assert_eq!(delivered.state.kind(), crate::TurnInputStateKind::Completed);
}

/// ADR 0101 §5.1: an address is accepted only if the turn is this session's
/// running turn or has ended. An unknown turn, including another session's
/// running turn, is refused with `IngressTurnAddressUnknown`, and nothing is
/// allocated: no row and no sequence number.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unknown_turn_address_is_refused_without_a_row_or_sequence(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-turn-address");
    let other = SessionId::from("ingress-turn-address-other");
    let refused = |session: SessionId, turn: &'static str| {
        let store = Arc::clone(&store);
        async move {
            let error = store
                .enqueue_pending_turn_input(pending_active_turn_input_draft(
                    &session,
                    &TurnId::from(turn),
                    crate::TurnInputCheckpointBoundary::AfterWork,
                    "addressed",
                ))
                .await
                .expect_err("an unknown turn address is refused");
            assert!(
                matches!(
                    &error,
                    StoreError::IngressTurnAddressUnknown { session_id, turn_id }
                        if *session_id == session && turn_id.as_str() == turn
                ),
                "an unknown address is refused by name: {error:?}"
            );
        }
    };
    refused(session.clone(), "never-ran").await;

    // Another session's running root is not this session's turn.
    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &other,
        ))
        .await
        .expect("admit the other session");
    let foreign_head = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&other, "other head").with_source_key("other-root"),
        )
        .await
        .expect("enqueue the other session's head");
    let other_fence = seal_drive_fence_for_test(&store, &other, "other-owner").await;
    admitted_root(
        &store,
        &other_fence,
        "other-root",
        AdmittedHead::Input(foreign_head.input_id.clone()),
    )
    .await;
    refused(session.clone(), "other-root").await;

    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session, "first").with_source_key("address-root"),
        )
        .await
        .expect("enqueue the first input");
    assert_eq!(
        first.enqueue_seq, 1,
        "a refused address allocated no sequence number"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs")
            .into_iter()
            .map(|read| read.input.input_id)
            .collect::<Vec<_>>(),
        vec![first.input_id.clone()],
        "a refused address stored no row"
    );

    let fence = seal_drive_fence_for_test(&store, &session, "address-owner").await;
    let admission = admitted_root(
        &store,
        &fence,
        "address-root",
        AdmittedHead::Input(first.input_id.clone()),
    )
    .await;
    let running = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session,
            &TurnId::from("address-root"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "to the running root",
        ))
        .await
        .expect("an address to the running root is accepted");
    assert_eq!(running.enqueue_seq, 2);
    end_root(
        &store,
        &fence,
        completing_admission("address-root", &admission),
    )
    .await;
    let ended = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session,
            &TurnId::from("address-root"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "to the ended root",
        ))
        .await
        .expect("an address to an ended root is accepted");
    assert!(
        ended.state.is_next_turn_input(None),
        "input addressed to an ended turn is next-turn input by rule"
    );
    refused(session.clone(), "still-never-ran").await;
}

/// ADR 0101 §8: every kind records an immutable submission digest. The same
/// source key and digest answers the stored batch; a changed digest is the
/// typed `QueuedWorkSourceKeyConflict`, and nothing is stored or adopted. A
/// wake's identity is its process fact, so a redelivery under different
/// host-configured delivery policy or merge key is the same submission.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_changed_resubmission_is_a_typed_conflict_for_every_kind(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-content-conflict");
    let conflicting = |draft: QueuedWorkBatchDraft, original: QueuedWorkBatch| {
        let store = Arc::clone(&store);
        async move {
            let session = draft.session_id.clone();
            let before = store
                .list_queued_work(&session)
                .await
                .expect("list queued work");
            let error = store
                .enqueue_queued_work(draft)
                .await
                .expect_err("changed content under a filed source key is refused");
            assert!(
                matches!(
                    &error,
                    StoreError::QueuedWorkSourceKeyConflict { existing_batch_id, .. }
                        if *existing_batch_id == original.batch_id
                ),
                "a changed resubmission is a typed content conflict: {error:?}"
            );
            let after = store
                .list_queued_work(&session)
                .await
                .expect("list queued work");
            assert_eq!(
                after
                    .iter()
                    .map(|batch| (batch.batch_id.clone(), batch.submission_digest.clone()))
                    .collect::<Vec<_>>(),
                before
                    .iter()
                    .map(|batch| (batch.batch_id.clone(), batch.submission_digest.clone()))
                    .collect::<Vec<_>>(),
                "a refused resubmission stores nothing"
            );
        }
    };

    let wake_batch = store
        .enqueue_queued_work(wake(&session, "conflict-process", 1, "original fact"))
        .await
        .expect("enqueue the wake");
    assert!(
        wake_batch
            .submission_digest
            .starts_with("queued-work-submission:"),
        "admission records the wake's digest: {}",
        wake_batch.submission_digest
    );
    assert_eq!(
        resubmitted_terminal(
            &store,
            wake(&session, "conflict-process", 1, "original fact"),
            &wake_batch
        )
        .await,
        None
    );
    let mut reconfigured = crate::conformance::helpers::process_wake_work(
        &session,
        "conflict-process",
        1,
        "original fact",
        DeliveryPolicy::AfterCurrentTurnCommit,
    )
    .with_merge_key("host-configured");
    reconfigured = reconfigured.with_authority(crate::QueuedWorkAuthority::new("host"));
    assert_eq!(
        resubmitted_terminal(&store, reconfigured, &wake_batch).await,
        None,
        "a wake redelivered under other host configuration is the same submission"
    );
    conflicting(
        wake(&session, "conflict-process", 1, "a different fact"),
        wake_batch.clone(),
    )
    .await;

    let command = store
        .enqueue_queued_work(keyed_command(&session, "conflict-command", "original"))
        .await
        .expect("enqueue the command");
    assert_eq!(
        resubmitted_terminal(
            &store,
            keyed_command(&session, "conflict-command", "original"),
            &command
        )
        .await,
        None
    );
    conflicting(
        keyed_command(&session, "conflict-command", "changed"),
        command.clone(),
    )
    .await;
    conflicting(
        keyed_command(&session, "conflict-command", "original")
            .with_authority(crate::QueuedWorkAuthority::new("someone else")),
        command,
    )
    .await;
}

/// ADR 0101 §8: every terminal queued item keeps a tombstone until host
/// vacuum, with its kind, source key, sequence, submitted delivery and
/// digest, a closed cause and its terminal time, and no admission binding.
/// Open-row selection excludes it, and a resubmission answers it without
/// reopening it. Inputs keep the same tombstone. Vacuum removes the queued
/// tombstones; a vacuumed wake's redelivery then meets its receiver floor.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_terminal_ingress_item_leaves_a_tombstone(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("ingress-tombstones");
    let fence = seal_drive_fence_for_test(&store, &session, "tombstone-owner").await;

    // A withdrawn wake.
    let withdrawn = store
        .enqueue_queued_work(wake(&session, "withdrawn-process", 1, "withdrawn"))
        .await
        .expect("enqueue the withdrawn wake");
    store
        .cancel_queued_work_batch(&session, withdrawn.batch_id.as_str())
        .await
        .expect("withdraw the wake")
        .expect("an open wake is withdrawable");

    // A delivered wake.
    let delivered = store
        .enqueue_queued_work(wake(&session, "delivered-process", 1, "delivered"))
        .await
        .expect("enqueue the delivered wake");
    drive_root_to_end(
        &store,
        &fence,
        "tombstone-wake-root",
        AdmittedHead::Batch(delivered.batch_id.clone()),
    )
    .await;

    // An applied command.
    let applied = store
        .enqueue_queued_work(keyed_command(&session, "applied-command", "applied"))
        .await
        .expect("enqueue the command");
    let run = store
        .open_session_command_run(&fence)
        .await
        .expect("open the command run");
    assert_eq!(run.len(), 1);
    store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &fence,
            crate::QueuedWorkCompletion {
                session_id: session.clone(),
                batch_ids: vec![applied.batch_id.clone()],
            },
        ))
        .await
        .expect("the applying commit settles the command");

    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open work")
            .is_empty(),
        "open-row selection excludes tombstones"
    );
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("list queued work")
            .is_empty(),
        "tombstones are not queued work"
    );
    for (draft, original, cause) in [
        (
            wake(&session, "withdrawn-process", 1, "withdrawn"),
            &withdrawn,
            IngressTerminalCause::Cancelled,
        ),
        (
            wake(&session, "delivered-process", 1, "delivered"),
            &delivered,
            IngressTerminalCause::Delivered,
        ),
        (
            keyed_command(&session, "applied-command", "applied"),
            &applied,
            IngressTerminalCause::Applied,
        ),
    ] {
        let terminal = resubmitted_terminal(&store, draft, original)
            .await
            .unwrap_or_else(|| panic!("{} must be a tombstone", original.batch_id));
        assert_eq!(terminal.cause, cause, "{}", original.batch_id);
        assert!(
            terminal.at_ms > 0,
            "the tombstone records its terminal time"
        );
    }
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("list queued work")
            .is_empty(),
        "a resubmission reopens nothing"
    );

    // Inputs leave the same tombstone: a withdrawn input and a delivered one.
    let withdrawn_draft = pending_next_turn_input_draft(&session, "withdrawn input")
        .with_source_key("withdrawn-input");
    let withdrawn_input = store
        .enqueue_pending_turn_input(withdrawn_draft.clone())
        .await
        .expect("enqueue the withdrawn input");
    let cancelled = store
        .cancel_pending_turn_input(&session, withdrawn_input.input_id.as_str())
        .await
        .expect("withdraw the input");
    let crate::PendingTurnInputCancelOutcome::Cancelled(cancelled) = cancelled else {
        panic!("an open input must be cancelled");
    };
    let returned_terminal = cancelled
        .terminal()
        .expect("the cancellation receipt is terminal");
    let persisted = resubmitted_input(&store, withdrawn_draft.clone(), &withdrawn_input).await;
    assert_eq!(
        Some(returned_terminal),
        persisted.terminal(),
        "the receipt and durable tombstone agree"
    );
    let delivered_draft = pending_next_turn_input_draft(&session, "delivered input")
        .with_source_key("delivered-input");
    let delivered_input = store
        .enqueue_pending_turn_input(delivered_draft.clone())
        .await
        .expect("enqueue the delivered input");
    drive_root_to_end(
        &store,
        &fence,
        "delivered-input",
        AdmittedHead::Input(delivered_input.input_id.clone()),
    )
    .await;
    for (draft, input, cause) in [
        (
            withdrawn_draft,
            &withdrawn_input,
            IngressTerminalCause::Cancelled,
        ),
        (
            delivered_draft,
            &delivered_input,
            IngressTerminalCause::Delivered,
        ),
    ] {
        let read = resubmitted_input(&store, draft, input).await;
        let terminal = read
            .terminal()
            .unwrap_or_else(|| panic!("{} must be a tombstone", input.input_id));
        assert_eq!(terminal.cause, cause, "{}", input.input_id);
        assert!(
            terminal.at_ms > 0,
            "the tombstone records its terminal time"
        );
        assert_eq!(read.state.ingress(), input.state.ingress());
    }

    let vacuum = store
        .vacuum(&session)
        .await
        .expect("vacuum the session's tombstones");
    assert_eq!(vacuum.removed_queued_work_tombstone_count, 3);
    let error = store
        .enqueue_queued_work(wake(&session, "withdrawn-process", 1, "withdrawn"))
        .await
        .expect_err("a vacuumed wake's redelivery meets its receiver floor");
    assert!(
        matches!(error, StoreError::ProcessWakeSequenceRewound { .. }),
        "{error:?}"
    );
}

/// ADR 0101 §8, as FIG-4202 found missing: a session command resubmitted
/// under its source key after the command lane applied it answers the
/// applied tombstone. It is not a new command, and the command lane has
/// nothing more to apply.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_settled_command_resubmitted_under_its_key_is_not_a_new_command(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-command-resubmission");
    let fence = seal_drive_fence_for_test(&store, &session, "command-owner").await;
    let command = store
        .enqueue_queued_work(keyed_command(&session, "host-command-1", "refresh"))
        .await
        .expect("enqueue the command");
    store
        .open_session_command_run(&fence)
        .await
        .expect("open the command run");
    store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            &fence,
            crate::QueuedWorkCompletion {
                session_id: session.clone(),
                batch_ids: vec![command.batch_id.clone()],
            },
        ))
        .await
        .expect("the applying commit settles the command");
    let terminal = resubmitted_terminal(
        &store,
        keyed_command(&session, "host-command-1", "refresh"),
        &command,
    )
    .await;
    assert_eq!(
        terminal.map(|terminal| terminal.cause),
        Some(IngressTerminalCause::Applied)
    );
    assert!(
        store
            .open_session_command_run(&fence)
            .await
            .expect("open the command run")
            .is_empty(),
        "the resubmission queued no second command"
    );
}

/// ADR 0101 §5.2: `authority` and `merge_key` are per-item data for policy
/// and traces, not equality gates. A prefix of wakes with different
/// principals and different or absent merge keys is offered whole, and a
/// draining policy takes it in one admission.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn composition_offers_mixed_authorities_and_merge_keys_as_one_prefix(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-per-item-data");
    let mut batches = Vec::new();
    for (process, principal, merge_key) in [
        ("mixed-a", Some("alice"), Some("a")),
        ("mixed-b", Some("bob"), Some("a")),
        ("mixed-c", None, None),
        ("mixed-d", Some("alice"), Some("b")),
    ] {
        let mut draft = wake(&session, process, 1, process);
        if let Some(principal) = principal {
            draft = draft.with_authority(crate::QueuedWorkAuthority::new(principal));
        }
        if let Some(merge_key) = merge_key {
            draft = draft.with_merge_key(merge_key);
        }
        batches.push(
            store
                .enqueue_queued_work(draft)
                .await
                .expect("enqueue a wake"),
        );
    }
    let fence = seal_drive_fence_for_test(&store, &session, "composition-owner").await;
    let admission = admitted_root_with_policy(
        &store,
        &fence,
        "composition-root",
        AdmittedHead::Batch(batches[0].batch_id.clone()),
        crate::testing::queued_work_admission_policy(64),
    )
    .await;
    assert_eq!(
        admission.batch_ids(),
        batches
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        "neither authority nor merge key stops the prefix"
    );
    let admitted = admission.queued.as_ref().expect("the wakes are admitted");
    assert_eq!(
        admitted
            .batches
            .iter()
            .map(|batch| (batch.authority.clone(), batch.merge_key.clone()))
            .collect::<Vec<_>>(),
        batches
            .iter()
            .map(|batch| (batch.authority.clone(), batch.merge_key.clone()))
            .collect::<Vec<_>>(),
        "each admitted batch keeps its own authority and merge key"
    );
}

/// ADR 0101 §5.2: a host that keeps principals apart does so in its drain
/// policy. A policy that stops at the first principal change admits only
/// the head's run; the rest stays open for the next root, in order.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_drain_policy_keeps_principals_apart(store: Arc<dyn RuntimeStore>) {
    #[derive(Debug)]
    struct OnePrincipalPerTurn;
    impl crate::QueuedDrainPolicy for OnePrincipalPerTurn {
        fn name(&self) -> &str {
            "one_principal_per_turn"
        }

        fn select_drain(
            &self,
            request: &crate::QueuedDrainRequest<'_>,
        ) -> crate::QueuedDrainSelection {
            let candidates = request.candidates();
            let Some(head) = candidates.first() else {
                return crate::QueuedDrainSelection::head_only();
            };
            crate::QueuedDrainSelection::leading(
                candidates
                    .iter()
                    .take_while(|candidate| {
                        candidate.authority.principal == head.authority.principal
                    })
                    .count(),
            )
        }
    }

    let session = SessionId::from("ingress-principal-policy");
    let mut batches = Vec::new();
    for (process, principal) in [
        ("principal-a1", "alice"),
        ("principal-a2", "alice"),
        ("principal-b1", "bob"),
    ] {
        batches.push(
            store
                .enqueue_queued_work(
                    wake(&session, process, 1, process)
                        .with_authority(crate::QueuedWorkAuthority::new(principal)),
                )
                .await
                .expect("enqueue a wake"),
        );
    }
    let fence = seal_drive_fence_for_test(&store, &session, "principal-owner").await;
    let mut policy = crate::testing::queued_work_admission_policy(64);
    policy.drain_policy = Arc::new(OnePrincipalPerTurn);
    let first = admitted_root_with_policy(
        &store,
        &fence,
        "principal-root-a",
        AdmittedHead::Batch(batches[0].batch_id.clone()),
        policy.clone(),
    )
    .await;
    assert_eq!(
        first.batch_ids(),
        vec![batches[0].batch_id.clone(), batches[1].batch_id.clone()],
        "the policy stops the drain at the principal change"
    );
    end_root(
        &store,
        &fence,
        completing_admission("principal-root-a", &first),
    )
    .await;
    let second = admitted_root_with_policy(
        &store,
        &fence,
        "principal-root-b",
        AdmittedHead::Batch(batches[2].batch_id.clone()),
        policy,
    )
    .await;
    assert_eq!(second.batch_ids(), vec![batches[2].batch_id.clone()]);
}

/// A recorded queued-work admission has one payload, so replay cannot hide
/// an unfenced second wake in the same batch.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn a_recorded_queued_batch_refuses_multiple_payloads(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("ingress-one-payload");
    let batch = store
        .enqueue_queued_work(wake(&session, "single-process", 1, "wake"))
        .await
        .expect("enqueue one wake");
    let recorded = serde_json::to_value(&batch).expect("record the batch");
    let restored: QueuedWorkBatch =
        serde_json::from_value(recorded.clone()).expect("restore one payload");
    assert_eq!(restored.batch_id, batch.batch_id);
    let payload = recorded["payload"].clone();
    let mut multiple = recorded.clone();
    multiple["payload"] = serde_json::json!([payload.clone(), payload.clone()]);
    assert!(
        serde_json::from_value::<QueuedWorkBatch>(multiple).is_err(),
        "a recorded admission must refuse multiple payloads"
    );
    let mut legacy = recorded;
    legacy
        .as_object_mut()
        .expect("batch is an object")
        .remove("payload");
    legacy["items"] = serde_json::json!([
        {"item_id": "first", "payload": payload.clone()},
        {"item_id": "second", "payload": payload}
    ]);
    assert!(
        serde_json::from_value::<QueuedWorkBatch>(legacy).is_err(),
        "the retired item array cannot cross the journal boundary"
    );
}
