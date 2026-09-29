//! Queued-work composition under a root's admission (FIG-3927 §2.2): a
//! batch-headed root takes the ready prefix its policy admits, the next
//! root takes what follows once the first settles, and a resumed root drives
//! exactly the composition its admission recorded.

use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_core::store::{AdmittedHead, IngressSettlement};
use lash_core::testing::RuntimeStoreTestDriveExt as _;
use pretty_assertions::assert_eq;

fn batch_ids(admission: &lash_core::store::RootAdmission) -> Vec<String> {
    admission
        .batch_ids()
        .iter()
        .map(|batch| batch.as_str().to_string())
        .collect()
}

fn ids(batches: &[&QueuedWorkBatch]) -> Vec<String> {
    batches
        .iter()
        .map(|batch| batch.batch_id.as_str().to_string())
        .collect()
}

/// Admit `root` on `head` under `fence` with `policy` in place of the
/// fixture's generous one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
async fn admitted_under(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::DriveFence,
    root: &str,
    head: AdmittedHead,
    policy: crate::TurnLaneAdmissionPolicy,
) -> lash_core::store::RootAdmission {
    let mut request = admit_root_request_for_test(fence, &TurnId::from(root), head);
    request.policy = policy;
    store
        .admit_root(&request)
        .await
        .expect("admit the root")
        .expect("the root's admission reaches its head")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_respects_membership_limits_exclusivity_and_sessions(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("queued-membership");
    let exclusive = store
        .enqueue_queued_work(queued_draft(
            &session,
            "exclusive",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue exclusive work");
    let joined = store
        .enqueue_queued_work(
            queued_draft(&session, "joined", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("root"),
        )
        .await
        .expect("enqueue joined work");
    let other = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("other"),
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue other session work");

    // One root at a time: each root's admission takes its prefix, and the
    // next root sees what follows only once the first has settled.
    let fence = seal_drive_fence_for_test(&store, &session, "owner-a").await;
    let first = drive_root_to_end(
        &store,
        &fence,
        "membership-exclusive",
        AdmittedHead::Batch(exclusive.batch_id.clone()),
    )
    .await;
    assert_eq!(
        batch_ids(&first),
        ids(&[&exclusive]),
        "an exclusive batch must be admitted alone"
    );
    let next = drive_root_to_end(
        &store,
        &fence,
        "membership-joined",
        AdmittedHead::Batch(joined.batch_id.clone()),
    )
    .await;
    assert_eq!(batch_ids(&next), ids(&[&joined]));

    assert_eq!(
        store
            .list_open_queued_work(&SessionId::from("other"))
            .await
            .expect("list the other session's open work")
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>(),
        vec![other.batch_id.clone()],
        "admitting one session's roots must not take queued work from another session"
    );

    let mut limited = Vec::new();
    for text in ["one", "two", "three"] {
        limited.push(
            store
                .enqueue_queued_work(
                    queued_draft(&session, text, DeliveryPolicy::EarliestSafeBoundary)
                        .with_merge_key("limited"),
                )
                .await
                .expect("enqueue limited work"),
        );
    }
    let capped = admitted_under(
        &store,
        &fence,
        "membership-limited",
        AdmittedHead::Batch(limited[0].batch_id.clone()),
        crate::testing::queued_work_claim_policy(2),
    )
    .await;
    assert_eq!(
        batch_ids(&capped),
        ids(&[&limited[0], &limited[1]]),
        "max_batches must cap a joined admission"
    );
    end_root(
        &store,
        &fence,
        completing_admission("membership-limited", &capped),
    )
    .await;
    let remaining = drive_root_to_end(
        &store,
        &fence,
        "membership-remaining",
        AdmittedHead::Batch(limited[2].batch_id.clone()),
    )
    .await;
    assert_eq!(batch_ids(&remaining), ids(&[&limited[2]]));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_join_groups_by_delivery_policy_and_merge_key(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("queued-join");
    let first = store
        .enqueue_queued_work(
            queued_draft(
                &session,
                "group a one",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue group a one");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                &session,
                "group a two",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue group a two");
    let different_merge = store
        .enqueue_queued_work(
            queued_draft(&session, "group b", DeliveryPolicy::EarliestSafeBoundary)
                .with_merge_key("b"),
        )
        .await
        .expect("enqueue group b");
    let different_delivery = store
        .enqueue_queued_work(
            queued_draft(
                &session,
                "after commit",
                DeliveryPolicy::AfterCurrentTurnCommit,
            )
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue after-commit");

    let fence = seal_drive_fence_for_test(&store, &session, "owner-a").await;
    let first_root = drive_root_to_end(
        &store,
        &fence,
        "join-a",
        AdmittedHead::Batch(first.batch_id.clone()),
    )
    .await;
    assert_eq!(
        batch_ids(&first_root),
        ids(&[&first, &second]),
        "a joined admission must group only adjacent batches with the same delivery policy and merge key"
    );
    let second_root = drive_root_to_end(
        &store,
        &fence,
        "join-b",
        AdmittedHead::Batch(different_merge.batch_id.clone()),
    )
    .await;
    assert_eq!(batch_ids(&second_root), ids(&[&different_merge]));
    let third_root = drive_root_to_end(
        &store,
        &fence,
        "join-after-commit",
        AdmittedHead::Batch(different_delivery.batch_id.clone()),
    )
    .await;
    assert_eq!(batch_ids(&third_root), ids(&[&different_delivery]));
    // FIG-3156. The runbook's phase-4 scorecard row asks for "two Each claims
    // vs one Coalesce claim". No `EachWake`/`Coalesce` delivery policy exists:
    // `DeliveryPolicy` is `EarliestSafeBoundary | AfterCurrentTurnCommit`
    // (`crates/lash-core-store/src/queued_work_vocabulary.rs:69-72`). The
    // admission shapes this law actually establishes are recorded instead.
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "queued_work_claims_join_by_policy_and_merge_key",
        "first_claim_batch_count": first_root.batch_ids().len(),
        "first_claim_delivery_policy": DeliveryPolicy::EarliestSafeBoundary.as_str(),
        "first_claim_merge_key": "a",
        "second_claim_batch_count": second_root.batch_ids().len(),
        "second_claim_merge_key": "b",
        "second_claim_split_reason": "merge_key",
        "third_claim_batch_count": third_root.batch_ids().len(),
        "third_claim_delivery_policy": DeliveryPolicy::AfterCurrentTurnCommit.as_str(),
        "third_claim_split_reason": "delivery_policy",
    }));
}

/// FIG-1313, FIG-3927 N3: a root's admission outlives the fence, the policy
/// and the limits that chose it. A worker that dies after the admission
/// commits leaves its successor exactly that composition: a later fence
/// under a one-row drain policy and a smaller row limit, with rows enqueued
/// in between, reads the recorded admission back byte for byte and takes
/// nothing more.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_resumed_root_drives_exactly_its_recorded_admission(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("resumed-root-admission");
    let mut rows = Vec::new();
    for (source_key, label) in [
        ("resume-w1", "w1"),
        ("resume-w2", "w2"),
        ("resume-w3", "w3"),
    ] {
        rows.push(
            store
                .enqueue_queued_work(
                    keyed_queued_draft(
                        &session,
                        label,
                        DeliveryPolicy::EarliestSafeBoundary,
                        source_key,
                    )
                    .with_merge_key("resume-key"),
                )
                .await
                .expect("enqueue resumed-root row"),
        );
    }
    let head = AdmittedHead::Batch(rows[0].batch_id.clone());
    let first = seal_drive_fence_for_test(&store, &session, "resume-owner-a").await;
    let mut coalescing = crate::testing::queued_work_claim_policy(64);
    coalescing.drain_policy = Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    let admitted = admitted_under(&store, &first, "resumed-root", head.clone(), coalescing).await;
    assert_eq!(batch_ids(&admitted), ids(&[&rows[0], &rows[1], &rows[2]]));

    store
        .enqueue_queued_work(
            keyed_queued_draft(
                &session,
                "late",
                DeliveryPolicy::EarliestSafeBoundary,
                "resume-late",
            )
            .with_merge_key("resume-key"),
        )
        .await
        .expect("enqueue a row after the admission");
    store
        .supersede_drive_epoch_for_test(&first)
        .await
        .expect("the predecessor's drive is superseded");
    let successor = seal_drive_fence_for_test(&store, &session, "resume-owner-b").await;
    let mut one_at_a_time = crate::testing::queued_work_claim_policy(1);
    one_at_a_time.drain_policy = crate::default_queued_drain_policy();
    let resumed = admitted_under(&store, &successor, "resumed-root", head, one_at_a_time).await;
    assert_eq!(
        serde_json::to_value(&resumed).expect("encode the resumed admission"),
        serde_json::to_value(&admitted).expect("encode the recorded admission"),
        "a resumed root must drive its recorded admission, not a composition re-decided \
         under the successor's policy, limits or later rows"
    );
    assert!(
        store
            .admit_root(&admit_root_request_for_test(
                &first,
                &TurnId::from("resumed-root"),
                AdmittedHead::Batch(rows[0].batch_id.clone()),
            ))
            .await
            .is_err_and(|error| matches!(error, StoreError::StaleDriveFence { .. })),
        "the superseded fence reads nothing back"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_wakes_batch_by_default(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("wake-default-batch");
    let merged_wakes = [
        policy_test_wake(&session, &crate::ProcessId::fixture("process-a"), 1),
        policy_test_wake(&session, &crate::ProcessId::fixture("process-b"), 1),
    ];
    let mut heads = Vec::new();
    for wake in &merged_wakes {
        heads.push(
            store
                .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
                .await
                .expect("enqueue default-key wake"),
        );
    }
    let fence = seal_drive_fence_for_test(&store, &session, "merge-owner").await;
    let merged = admitted_root(
        &store,
        &fence,
        "wake-default-batch-root",
        AdmittedHead::Batch(heads[0].batch_id.clone()),
    )
    .await;
    let batches = merged.queued.as_ref().expect("the wakes are admitted");
    assert_eq!(
        batches.batches.len(),
        2,
        "the constant wake merge key must batch compatible wakes across processes"
    );
    assert!(
        batches
            .batches
            .iter()
            .all(|batch| { batch.merge_key.as_deref() == Some(crate::PROCESS_WAKE_MERGE_KEY) })
    );
    end_root(
        &store,
        &fence,
        completing_admission("wake-default-batch-root", &merged),
    )
    .await;
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("list merged queue after settlement")
            .is_empty(),
        "merged settlement must delete every admitted receiver row"
    );
    for wake in merged_wakes {
        let error = store
            .enqueue_queued_work(crate::process_wake_batch_draft(wake))
            .await
            .expect_err("settled wake without a live row must trip the receiver floor");
        assert!(matches!(
            error,
            StoreError::ProcessWakeSequenceRewound { .. }
        ));
    }
}

pub(super) fn policy_test_wake(
    session_id: &SessionId,
    process_id: &ProcessId,
    sequence: u64,
) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("wake:{process_id}:{sequence}"),
        target_session_id: SessionId::from(session_id.to_string()),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: RuntimeAttribution::for_session(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: process_id.to_string(),
        created_at_ms: 1,
    }
}

/// A queued-work completion settles only under the live fence of the root
/// that admitted the rows: a completion under a superseded fence is refused
/// `StaleDriveFence`, one naming a row the root does not hold is refused
/// `IngressRowNotAdmitted`, and neither removes a row.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_completion_is_fenced_and_root_keyed(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("queued-completion-fence");
    let mut joined = Vec::new();
    for text in ["join one", "join two"] {
        joined.push(
            store
                .enqueue_queued_work(
                    queued_draft(&session, text, DeliveryPolicy::EarliestSafeBoundary)
                        .with_merge_key("joined"),
                )
                .await
                .expect("enqueue joined batch"),
        );
    }
    let fence = seal_drive_fence_for_test(&store, &session, "owner-a").await;
    let admission = admitted_root(
        &store,
        &fence,
        "completion-root",
        AdmittedHead::Batch(joined[0].batch_id.clone()),
    )
    .await;
    assert_eq!(batch_ids(&admission), ids(&[&joined[0], &joined[1]]));

    let mut foreign = IngressSettlement::new(TurnId::from("another-root"));
    foreign.completed_batches.push(
        admission
            .queued
            .as_ref()
            .expect("admitted batches")
            .completion(),
    );
    let err = try_end_root(&store, &fence, foreign)
        .await
        .expect_err("a completion keyed by a root that does not hold the rows must fail");
    assert!(matches!(err, StoreError::IngressRowNotAdmitted { .. }));
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("a refused completion preserves queued work")
            .len(),
        2
    );

    let successor = seal_drive_fence_for_test(&store, &session, "owner-b").await;
    let err = try_end_root(
        &store,
        &fence,
        completing_admission("completion-root", &admission),
    )
    .await
    .expect_err("a completion under a superseded fence must fail");
    assert!(matches!(err, StoreError::StaleDriveFence { .. }));
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("a stale completion preserves queued work")
            .len(),
        2
    );

    end_root(
        &store,
        &successor,
        completing_admission("completion-root", &admission),
    )
    .await;
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("a fenced completion clears queued work")
            .is_empty()
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queue_completion_and_turn_commit_stamp_are_atomic(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("root");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session,
            "atomic pending input",
        ))
        .await
        .expect("enqueue atomic pending input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            &session,
            "atomic queue",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue queue batch");
    let bystander = store
        .enqueue_queued_work(queued_draft(
            &session,
            "atomic bystander",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue a batch no root admits");
    let fence = seal_drive_fence_for_test(&store, &session, "queue-owner").await;
    let root = "turn-atomic";
    let admission = admitted_root(
        &store,
        &fence,
        root,
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await;
    assert_eq!(admission.input_ids(), vec![input.input_id.clone()]);
    let checkpoint = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &TurnId::from(root),
        &TurnId::from(root),
        crate::CheckpointKind::AfterWork,
        "turn-atomic:checkpoint:0",
        64,
        crate::testing::queued_work_claim_policy(1),
    )
    .await
    .expect("the checkpoint admits the ready batch");
    assert_eq!(
        checkpoint
            .queued
            .as_ref()
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        vec![batch.batch_id.clone()]
    );
    let settlement = completing_checkpoint(completing_admission(root, &admission), &checkpoint);

    let mut state = RuntimeSessionState {
        session_id: session.clone(),
        turn_index: 41,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    // The switch commit writes the follow-on it owes onto the head, in the
    // same transaction that settles the root's admitted rows (ADR 0101 §3).
    let follow_on = crate::store::PendingFollowOn {
        follow_on_turn_id: crate::TurnId::from("turn-atomic:agent-frame:1"),
        frame_id: state
            .current_frame_node_id
            .clone()
            .expect("the initial frame is current"),
        task: "follow-on task".to_string(),
        options: None,
        resolved_run: None,
        chain_depth: 1,
        attempts: 0,
    };
    let mut base_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    base_commit.pending_follow_on = Some(follow_on.clone());
    let turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "turn-atomic", "final"));
    base_commit.turn_commit = turn_commit.clone();

    let mut unadmitted = settlement.clone();
    unadmitted
        .released
        .push(lash_core::store::IngressRowId::Batch(
            bystander.batch_id.clone(),
        ));
    let err = store
        .commit_runtime_state(settling_commit_for_test(
            base_commit.clone(),
            &fence,
            unadmitted,
        ))
        .await
        .expect_err(
            "a settlement naming a row the root does not hold must reject the whole commit",
        );
    assert!(matches!(err, StoreError::IngressRowNotAdmitted { .. }));
    assert!(
        store
            .load_session_window(
                &SessionId::from("root"),
                crate::store::WindowSelector::Current
            )
            .await
            .expect("load after rejected atomic commit")
            .is_none(),
        "a rejected settlement must not persist session state"
    );
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("list after rejected atomic commit")
            .len(),
        2,
        "a rejected settlement must preserve queued work"
    );

    let mut elsewhere = base_commit.clone();
    elsewhere.pending_follow_on = Some(crate::store::PendingFollowOn {
        frame_id: crate::session_graph::frame_node_id(&session, "elsewhere"),
        ..follow_on.clone()
    });
    let err = store
        .commit_runtime_state(settling_commit_for_test(
            elsewhere,
            &fence,
            settlement.clone(),
        ))
        .await
        .expect_err("a follow-on off the current frame must reject the whole final commit");
    assert!(matches!(err, StoreError::FollowOnFrameNotCurrent { .. }));
    assert!(
        store
            .load_session_window(
                &SessionId::from("root"),
                crate::store::WindowSelector::Current
            )
            .await
            .expect("load after rejected follow-on")
            .is_none(),
        "a rejected follow-on must roll back session state"
    );
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("list after rejected follow-on")
            .len(),
        2,
        "a rejected follow-on must roll back inbound queue completion"
    );

    let first = store
        .commit_runtime_state(settling_commit_for_test(
            base_commit.clone(),
            &fence,
            settlement.clone(),
        ))
        .await
        .expect("valid final commit clears queue and records the turn stamp atomically");
    let retry = store
        .commit_runtime_state({
            let mut retry = base_commit;
            retry.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::turn(
                "root",
                "turn-atomic",
                "final",
            ));
            settling_commit_for_test(retry, &fence, settlement)
        })
        .await
        .expect("same final turn commit stamp retries idempotently");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(
        retry.realized_node_timestamps,
        first.realized_node_timestamps
    );
    assert_eq!(first.pending_follow_on.as_ref(), Some(&follow_on));
    assert_eq!(
        retry.pending_follow_on, first.pending_follow_on,
        "an idempotent commit retry returns the follow-on the switch wrote"
    );
    assert_eq!(
        store
            .load_session_window(
                &SessionId::from("root"),
                crate::store::WindowSelector::Current
            )
            .await
            .expect("load after accepted atomic commit")
            .expect("committed head")
            .pending_follow_on,
        Some(follow_on),
        "the switch commit leaves its follow-on on the head"
    );
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("list after accepted atomic commit")
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![bystander.batch_id],
        "the commit settles the admitted batch and leaves the open one; a frame handoff is never a queue row"
    );
    assert!(
        store
            .list_pending_turn_inputs(&session)
            .await
            .expect("list inputs after accepted atomic commit")
            .is_empty(),
        "accepted switch commit must complete inbound input with the follow-on write"
    );
}
