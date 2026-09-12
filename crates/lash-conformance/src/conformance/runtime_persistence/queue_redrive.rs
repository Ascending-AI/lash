use super::*;
use pretty_assertions::assert_eq;

pub async fn same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(
    store: Arc<dyn RuntimePersistence>,
) {
    const ROW_COUNT: usize = 34;

    let queue_session = "bounded-scan-queue";
    let queue_owner = lease_owner("bounded-scan-queue-owner");
    let mut queue_batches = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        queue_batches.push(
            store
                .enqueue_queued_work(queued_draft(
                    &SessionId::from(queue_session),
                    &format!("bounded queue {index}"),
                    DeliveryPolicy::EarliestSafeBoundary,
                ))
                .await
                .expect("enqueue bounded-scan queued work"),
        );
    }
    let queue_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(queue_session),
            &queue_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor",
            60_000,
        )
        .await
        .expect("claim bounded-scan queue session lease")
        .acquired()
        .expect("bounded-scan queue session lease is free");
    for expected in &queue_batches {
        let claim = store
            .claim_ready_queued_work(
                &SessionId::from(queue_session),
                &queue_lease.fence(),
                &queue_owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(1),
            )
            .await
            .expect("claim bounded-scan queued work")
            .claim()
            .expect("bounded-scan queued work remains reachable");
        assert_eq!(claim.batches[0].batch_id, expected.batch_id);
    }
    release_session_execution_lease_for_test(&store, &queue_lease).await;

    let command_session = "bounded-scan-command";
    let command_owner = lease_owner("bounded-scan-command-owner");
    let mut command_batches = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        command_batches.push(
            store
                .enqueue_queued_work(queued_session_command_draft(
                    &SessionId::from(command_session),
                    &format!("bounded command {index}"),
                ))
                .await
                .expect("enqueue bounded-scan session command"),
        );
    }
    let command_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(command_session),
            &command_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor-2",
            60_000,
        )
        .await
        .expect("claim bounded-scan command session lease")
        .acquired()
        .expect("bounded-scan command session lease is free");
    for expected in &command_batches {
        let claim = store
            .claim_leading_ready_session_command(
                &SessionId::from(command_session),
                &command_lease.fence(),
                &command_owner,
            )
            .await
            .expect("claim bounded-scan session command")
            .expect("bounded-scan session command remains reachable");
        assert_eq!(claim.batches[0].batch_id, expected.batch_id);
    }
    release_session_execution_lease_for_test(&store, &command_lease).await;

    let input_session = "bounded-scan-turn-input";
    let input_owner = lease_owner("bounded-scan-turn-input-owner");
    let mut inputs = Vec::with_capacity(ROW_COUNT);
    for index in 0..ROW_COUNT {
        inputs.push(
            store
                .enqueue_pending_turn_input(pending_next_turn_input_draft(
                    &SessionId::from(input_session),
                    &format!("bounded turn input {index}"),
                ))
                .await
                .expect("enqueue bounded-scan turn input"),
        );
    }
    let input_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(input_session),
            &input_owner,
            "same-generation-claim-scans-reach-rows-beyond-the-scan-surplus-executor-3",
            60_000,
        )
        .await
        .expect("claim bounded-scan turn-input session lease")
        .acquired()
        .expect("bounded-scan turn-input session lease is free");
    for expected in &inputs {
        let claim = store
            .claim_next_turn_inputs(
                &SessionId::from(input_session),
                &input_lease.fence(),
                &input_owner,
                1,
            )
            .await
            .expect("claim bounded-scan turn input")
            .expect("bounded-scan turn input remains reachable");
        assert_eq!(claim.inputs[0].input_id, expected.input_id);
    }
    release_session_execution_lease_for_test(&store, &input_lease).await;
}

pub(super) async fn queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(
    store: Arc<dyn RuntimePersistence>,
) {
    store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "not ready",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_available_at_ms(4_102_444_800_000),
        )
        .await
        .expect("enqueue unavailable work");
    let exclusive = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "exclusive",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue exclusive work");
    let joined = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "joined",
                DeliveryPolicy::EarliestSafeBoundary,
            )
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

    // Both root claims run under one live session lease: advancing through the
    // queue relies on each claimed batch staying held by the current generation,
    // so a same-generation follow-up claim skips it (ADR 0029).
    let root_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim root")
        .claim()
        .expect("root claim");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![exclusive.batch_id.as_str()],
        "an exclusive batch must claim alone and unavailable earlier work must be skipped"
    );
    let next_root = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim joined")
        .claim()
        .expect("joined claim");
    release_session_execution_lease_for_test(&store, &root_session_lease).await;
    assert_eq!(next_root.batches[0].batch_id, joined.batch_id);
    let other_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("other"), "owner-c").await;
    let other_claim = store
        .claim_ready_queued_work(
            &SessionId::from("other"),
            &other_session_lease.fence(),
            &lease_owner("owner-c"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim other")
        .claim()
        .expect("other claim");
    release_session_execution_lease_for_test(&store, &other_session_lease).await;
    assert_eq!(
        other_claim.batches[0].batch_id, other.batch_id,
        "claiming one session must not consume queued work from another session"
    );

    let reclaimed_source = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("reclaim"),
            "superseded claim",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue reclaim work");
    let first_generation_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("reclaim"), "owner-a")
            .await;
    let first_generation_claim = store
        .claim_ready_queued_work(
            &SessionId::from("reclaim"),
            &first_generation_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim under the first generation")
        .claim()
        .expect("first-generation claim");
    release_session_execution_lease_for_test(&store, &first_generation_lease).await;
    let reclaim_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("reclaim"), "owner-b")
            .await;
    let reclaimed = store
        .claim_ready_queued_work(
            &SessionId::from("reclaim"),
            &reclaim_session_lease.fence(),
            &lease_owner("owner-b"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("reclaim under a new generation")
        .claim()
        .expect("reclaimed superseded claim");
    release_session_execution_lease_for_test(&store, &reclaim_session_lease).await;
    assert_eq!(reclaimed.batches[0].batch_id, reclaimed_source.batch_id);
    assert!(
        reclaimed.fencing_token > first_generation_claim.fencing_token,
        "reclaiming a claim across a session-lease generation must bump the fencing token"
    );

    let limited_first = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("limited"),
                "one",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("limited"),
        )
        .await
        .expect("enqueue limited one");
    let limited_second = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("limited"),
                "two",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("limited"),
        )
        .await
        .expect("enqueue limited two");
    let limited_third = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("limited"),
                "three",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("limited"),
        )
        .await
        .expect("enqueue limited three");
    // One live lease: the capped claim keeps the first two batches held, so the
    // same-generation follow-up claim only sees the third (ADR 0029).
    let limited_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("limited"), "owner").await;
    let limited = store
        .claim_ready_queued_work(
            &SessionId::from("limited"),
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("limited claim")
        .claim()
        .expect("limited claim exists");
    assert_eq!(
        limited
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            limited_first.batch_id.as_str(),
            limited_second.batch_id.as_str()
        ],
        "max_batches must cap a join claim"
    );
    let remaining = store
        .claim_ready_queued_work(
            &SessionId::from("limited"),
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("remaining claim")
        .claim()
        .expect("remaining claim exists");
    release_session_execution_lease_for_test(&store, &limited_session_lease).await;
    assert_eq!(remaining.batches[0].batch_id, limited_third.batch_id);
}

pub(super) async fn queued_work_join_groups_by_delivery_policy_and_merge_key(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
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
                &SessionId::from("root"),
                "group a two",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue group a two");
    let different_merge = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "group b",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("b"),
        )
        .await
        .expect("enqueue group b");
    let different_delivery = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "after commit",
                DeliveryPolicy::AfterCurrentTurnCommit,
            )
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue after-commit");

    // All three group claims run under one live session lease so each claimed
    // group stays held by the current generation and the next same-generation
    // claim advances to the following group (ADR 0029).
    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-a").await;
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim first group")
        .claim()
        .expect("first group claim");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()],
        "join claims must group only adjacent batches with the same delivery policy and merge key"
    );
    let second_claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim second group")
        .claim()
        .expect("second group claim");
    assert_eq!(second_claim.batches[0].batch_id, different_merge.batch_id);
    let third_claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim third group")
        .claim()
        .expect("third group claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(third_claim.batches[0].batch_id, different_delivery.batch_id);
}

pub(super) async fn queued_work_redrive_preserves_interrupted_batch_composition(
    store: Arc<dyn RuntimePersistence>,
) {
    for (source_key, label) in [("redrive-w1", "w1"), ("redrive-w2", "w2")] {
        store
            .enqueue_queued_work(
                queued_draft(
                    &SessionId::from("interrupted-batch-redrive"),
                    label,
                    DeliveryPolicy::EarliestSafeBoundary,
                )
                .with_source_key(source_key)
                .with_merge_key("redrive-key"),
            )
            .await
            .expect("enqueue original redrive row");
    }

    let first_owner = lease_owner("redrive-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("interrupted-batch-redrive"),
        &first_owner.owner_id,
    )
    .await;
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from("interrupted-batch-redrive"),
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim original redrive batch")
        .claim()
        .expect("original redrive batch exists");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)]
    );

    // Model an interruption after the claimed composition has escaped to a
    // journaled command, but before the queue completion commits. Releasing the
    // session lease makes the intact predecessor claim reclaimable without
    // abandoning or settling it.
    release_session_execution_lease_for_test(&store, &first_lease).await;
    store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("interrupted-batch-redrive"),
                "w3",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("redrive-w3")
            .with_merge_key("redrive-key"),
        )
        .await
        .expect("enqueue post-interruption compatible row");

    let successor_owner = lease_owner("redrive-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("interrupted-batch-redrive"),
        &successor_owner.owner_id,
    )
    .await;
    let redriven = store
        .claim_ready_queued_work(
            &SessionId::from("interrupted-batch-redrive"),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive interrupted claim")
        .claim()
        .expect("interrupted claim remains reclaimable");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)],
        "redrive must retain the literal predecessor batch composition"
    );
    assert_ne!(first_claim.claim_id, redriven.claim_id);
    release_session_execution_lease_for_test(&store, &successor_lease).await;

    let third_owner = lease_owner("redrive-owner-c");
    let third_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("interrupted-batch-redrive"),
        &third_owner.owner_id,
    )
    .await;
    let twice_redriven = store
        .claim_ready_queued_work(
            &SessionId::from("interrupted-batch-redrive"),
            &third_lease.fence(),
            &third_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive second interrupted generation")
        .claim()
        .expect("second interrupted generation remains reclaimable");
    assert_eq!(
        twice_redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w1"), 1), (Some("redrive-w2"), 2)],
        "a third generation must recover the second generation's literal composition"
    );
    assert_ne!(redriven.claim_id, twice_redriven.claim_id);

    let subsequent = store
        .claim_ready_queued_work(
            &SessionId::from("interrupted-batch-redrive"),
            &third_lease.fence(),
            &third_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim post-interruption row")
        .claim()
        .expect("post-interruption row remains separately claimable");
    assert_eq!(
        subsequent
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("redrive-w3"), 3)],
        "new compatible work must wait for a separate successor claim"
    );
    release_session_execution_lease_for_test(&store, &third_lease).await;
}

pub(super) async fn abandoned_predecessor_claim_pair_is_only_reclaimable_across_lease_generations(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "abandoned-predecessor-generation";
    let batch = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from(session_id),
            "generation-pinned predecessor",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue generation-pinned predecessor");

    let predecessor_owner = lease_owner("abandoned-predecessor-owner");
    let predecessor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &predecessor_owner.owner_id,
    )
    .await;
    let predecessor_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim predecessor generation")
        .claim()
        .expect("predecessor generation claim exists");
    release_session_execution_lease_for_test(&store, &predecessor_lease).await;

    let successor_owner = lease_owner("abandoned-successor-owner");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor_owner.owner_id,
    )
    .await;
    assert!(
        successor_lease.fencing_token > predecessor_lease.fencing_token,
        "the reclaiming successor must hold a newer lease generation"
    );
    let successor_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("reclaim predecessor under successor generation")
        .claim()
        .expect("successor generation claim exists");
    assert_eq!(
        successor_claim.abandon_restore_claim_id.as_deref(),
        Some(predecessor_claim.claim_id.as_str())
    );
    assert_eq!(
        successor_claim.abandon_restore_claim_token.as_deref(),
        Some(predecessor_claim.lease_token.as_str())
    );
    store
        .abandon_queued_work_claim(&successor_claim)
        .await
        .expect("abandon successor and restore predecessor pair");

    let reclaimed_restored_pair = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("reclaim restored predecessor under newer generation")
        .claim()
        .expect("restored predecessor remains reclaimable across generations");
    assert_eq!(reclaimed_restored_pair.batches.len(), 1);
    assert_eq!(reclaimed_restored_pair.batches[0].batch_id, batch.batch_id);

    let same_generation_reclaim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("probe same-generation reclaim");
    assert!(
        same_generation_reclaim.claim().is_none(),
        "the generation that reclaimed the restored pair must not self-steal it"
    );

    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let stale_completion = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .borrowing_session_execution_lease(successor_lease.fence())
                .completing_queue_claim(predecessor_claim.completion()),
        )
        .await;
    assert!(
        matches!(
            stale_completion,
            Err(StoreError::QueuedWorkClaimSuperseded { .. })
        ),
        "the reclaiming generation must supersede the restored predecessor completion pair: \
         {stale_completion:?}"
    );
    let preserved = store
        .list_queued_work(&SessionId::from(session_id))
        .await
        .expect("stale predecessor attempts preserve queued work");
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0].batch_id, batch.batch_id);

    release_session_execution_lease_for_test(&store, &successor_lease).await;
    let final_owner = lease_owner("abandoned-final-owner");
    let final_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &final_owner.owner_id,
    )
    .await;
    assert!(
        final_lease.fencing_token > successor_lease.fencing_token,
        "the final claimant must hold a newer lease generation"
    );
    let reclaimed = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &final_lease.fence(),
            &final_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("reclaim successor claim under final generation")
        .claim()
        .expect("successor claim remains reclaimable across generations");
    assert_eq!(reclaimed.batches.len(), 1);
    assert_eq!(reclaimed.batches[0].batch_id, batch.batch_id);
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(final_lease.completion())
                .completing_queue_claim(reclaimed.completion()),
        )
        .await
        .expect("newer generation reclaims and completes restored predecessor");
    assert!(
        store
            .list_queued_work(&SessionId::from(session_id))
            .await
            .expect("list completed predecessor queue")
            .is_empty(),
        "the newer generation must be able to settle the restored predecessor"
    );
}

#[doc(hidden)]
/// FIG-1575: a lane holding a deferred row is not an exhausted lane.
///
/// Both states present the same "no claimable candidate" view to the claim
/// state machine, and a host reading one as the other either abandons intact
/// work or waits forever on a queue that will never fill. Every backend must
/// tell them apart identically.
pub(super) async fn queued_work_names_a_deferred_lane_apart_from_an_exhausted_one(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let session_id = "deferred-versus-exhausted";
    let owner = lease_owner("deferred-owner");
    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &owner.owner_id,
    )
    .await;

    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("claim an empty lane")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::Empty),
        "a lane that never held a row is exhausted, not deferred"
    );

    let deferred = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from(session_id),
                "deferred",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("deferred-row")
            .with_available_at_ms(lease_timing.delayed_queue_row_available_at_ms()),
        )
        .await
        .expect("enqueue deferred work");

    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("claim a deferred lane")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::NotYetAvailable),
        "work whose availability has not arrived is intact, so the lane is not \
         exhausted"
    );

    store
        .cancel_queued_work_batch(&SessionId::from(session_id), &deferred.batch_id)
        .await
        .expect("cancel the deferred row");

    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("claim a drained lane")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::Empty),
        "a lane whose last row is gone is exhausted again"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
}

pub async fn queued_work_redrive_selects_claim_identity_across_ready_gap(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let session_id = "interrupted-batch-ready-gap";
    store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from(session_id),
                "w1",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("gap-w1")
            .with_merge_key("gap-key"),
        )
        .await
        .expect("enqueue ready gap W1");
    store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from(session_id),
                "w2",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("gap-w2")
            .with_merge_key("gap-key")
            .with_available_at_ms(lease_timing.delayed_queue_row_available_at_ms()),
        )
        .await
        .expect("enqueue delayed gap W2");
    store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from(session_id),
                "w3",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("gap-w3")
            .with_merge_key("gap-key"),
        )
        .await
        .expect("enqueue ready gap W3");

    let first_owner = lease_owner("gap-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &first_owner.owner_id,
    )
    .await;
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim ready rows across delayed gap")
        .claim()
        .expect("ready W1 and W3 form the original claim");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w1"), 1), (Some("gap-w3"), 3)]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;
    lease_timing.cross_delayed_queue_row_boundary().await;

    let successor = lease_owner("gap-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor.owner_id,
    )
    .await;
    let redriven = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive ready-gap claim")
        .claim()
        .expect("interrupted identity remains reclaimable across gap");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w1"), 1), (Some("gap-w3"), 3)]
    );
    let delayed = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim newly ready gap row")
        .claim()
        .expect("W2 remains a separate claim");
    assert_eq!(
        delayed
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gap-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

pub(super) async fn queued_work_redrive_obeys_delivery_boundary_before_identity(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "interrupted-batch-delivery-gate";
    for (source_key, label) in [("gate-w1", "w1"), ("gate-w2", "w2")] {
        store
            .enqueue_queued_work(
                queued_draft(
                    &SessionId::from(session_id),
                    label,
                    DeliveryPolicy::AfterCurrentTurnCommit,
                )
                .with_source_key(source_key)
                .with_merge_key("gate-key"),
            )
            .await
            .expect("enqueue delivery-gated redrive row");
    }
    let first_owner = lease_owner("gate-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &first_owner.owner_id,
    )
    .await;
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim delivery-gated work while idle")
        .claim()
        .expect("idle boundary admits after-commit work");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-w1"), 1), (Some("gate-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;

    let successor = lease_owner("gate-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor.owner_id,
    )
    .await;
    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &successor_lease.fence(),
                &successor,
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("apply active checkpoint gate before identity redrive")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::DeliveryBoundaryBlocked),
        "the active checkpoint boundary must produce a literal empty claim, and \
         every backend must name that same reason"
    );
    for (source_key, label) in [("gate-fresh-w1", "fresh-w1"), ("gate-fresh-w2", "fresh-w2")] {
        store
            .enqueue_queued_work(
                queued_draft(
                    &SessionId::from(session_id),
                    label,
                    DeliveryPolicy::EarliestSafeBoundary,
                )
                .with_source_key(source_key)
                .with_merge_key("gate-fresh-key"),
            )
            .await
            .expect("enqueue fresh checkpoint-deliverable work");
    }
    let fresh_checkpoint_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim fresh work while idle-only predecessor remains withheld")
        .claim()
        .expect("fresh checkpoint-deliverable work remains claimable");
    assert_eq!(
        fresh_checkpoint_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-fresh-w1"), 3), (Some("gate-fresh-w2"), 4),]
    );
    store
        .abandon_queued_work_claim(&fresh_checkpoint_claim)
        .await
        .expect("return fresh checkpoint claim before idle redrive");
    let after_boundary = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("redrive after delivery boundary clears")
        .claim()
        .expect("original composition remains intact");
    assert_eq!(
        after_boundary
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("gate-w1"), 1), (Some("gate-w2"), 2)]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

/// FIG-1313: a journaled drain composition outlives the policy that chose it.
///
/// The predecessor generation coalesced three rows under a host policy that
/// drains everything. A successor booting with the shipped one-row default must
/// still redrive that exact committed composition: the policy runs pre-request
/// and its selection is journaled with the claim, so replay serves history
/// instead of re-deciding it.
pub(super) async fn queued_work_redrive_ignores_a_changed_drain_policy(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "interrupted-batch-drain-policy";
    for (source_key, label) in [
        ("policy-w1", "w1"),
        ("policy-w2", "w2"),
        ("policy-w3", "w3"),
    ] {
        store
            .enqueue_queued_work(
                queued_draft(
                    &SessionId::from(session_id),
                    label,
                    DeliveryPolicy::EarliestSafeBoundary,
                )
                .with_source_key(source_key)
                .with_merge_key("policy-key"),
            )
            .await
            .expect("enqueue drain-policy redrive row");
    }
    let expected = vec![
        (Some("policy-w1"), 1),
        (Some("policy-w2"), 2),
        (Some("policy-w3"), 3),
    ];

    let first_owner = lease_owner("policy-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &first_owner.owner_id,
    )
    .await;
    let mut coalescing_policy = crate::testing::queued_work_claim_policy(64);
    coalescing_policy.drain_policy = Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            coalescing_policy,
        )
        .await
        .expect("claim three-row predecessor")
        .claim()
        .expect("three-row predecessor exists");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        expected
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;

    let successor = lease_owner("policy-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor.owner_id,
    )
    .await;
    let mut one_at_a_time = crate::testing::queued_work_claim_policy(64);
    one_at_a_time.drain_policy = crate::default_queued_drain_policy();
    let redriven = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            one_at_a_time,
        )
        .await
        .expect("redrive under a one-row successor policy")
        .claim()
        .expect("predecessor composition survives a changed drain policy");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        expected,
        "a redrive must serve the journaled composition, not re-run the successor's drain policy"
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

pub(super) async fn queued_work_redrive_ignores_successor_row_limit(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "interrupted-batch-row-limit";
    for (source_key, label) in [
        ("limit-w1", "w1"),
        ("limit-w2", "w2"),
        ("limit-w3", "w3"),
        ("limit-w4", "w4"),
        ("limit-w5", "w5"),
    ] {
        store
            .enqueue_queued_work(
                queued_draft(
                    &SessionId::from(session_id),
                    label,
                    DeliveryPolicy::EarliestSafeBoundary,
                )
                .with_source_key(source_key)
                .with_merge_key("limit-key"),
            )
            .await
            .expect("enqueue row-limit redrive row");
    }
    let first_owner = lease_owner("limit-owner-a");
    let first_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &first_owner.owner_id,
    )
    .await;
    let first_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &first_lease.fence(),
            &first_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim five-row predecessor")
        .claim()
        .expect("five-row predecessor exists");
    assert_eq!(
        first_claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &first_lease).await;

    let successor = lease_owner("limit-owner-b");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor.owner_id,
    )
    .await;
    let redriven = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("redrive under smaller successor row limit")
        .claim()
        .expect("predecessor composition ignores successor row limit");
    assert_eq!(
        redriven
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &successor_lease).await;

    let selected_owner = lease_owner("limit-owner-c");
    let selected_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("interrupted-batch-row-limit"),
        &selected_owner.owner_id,
    )
    .await;
    let selected_redrive = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("interrupted-batch-row-limit"),
            &selected_lease.fence(),
            &selected_owner,
            QueuedWorkClaimBoundary::Idle,
            &redriven
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("selected redrive under smaller successor row limit")
        .expect("selected predecessor composition ignores successor row limit");
    assert_eq!(
        selected_redrive
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("limit-w1"), 1),
            (Some("limit-w2"), 2),
            (Some("limit-w3"), 3),
            (Some("limit-w4"), 4),
            (Some("limit-w5"), 5),
        ]
    );
    release_session_execution_lease_for_test(&store, &selected_lease).await;
}

pub(super) async fn queued_work_selected_multi_identity_validation_and_abandon_restore(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "selected-multi-identity";
    let mut batches = Vec::new();
    for (source_key, label) in [
        ("selected-claim-a1", "a1"),
        ("selected-claim-a2", "a2"),
        ("selected-claim-b1", "b1"),
        ("selected-claim-b2", "b2"),
    ] {
        batches.push(
            store
                .enqueue_queued_work(
                    queued_draft(
                        &SessionId::from(session_id),
                        label,
                        DeliveryPolicy::EarliestSafeBoundary,
                    )
                    .with_source_key(source_key)
                    .with_merge_key("selected-multi-identity-key"),
                )
                .await
                .expect("enqueue selected multi-identity row"),
        );
    }
    let predecessor_owner = lease_owner("selected-multi-predecessor");
    let predecessor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &predecessor_owner.owner_id,
    )
    .await;
    let claim_a = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor A")
        .claim()
        .expect("predecessor A exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-a1"), Some("selected-claim-a2")]
    );
    let claim_b = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor B")
        .claim()
        .expect("predecessor B exists");
    assert_eq!(
        claim_b
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-b1"), Some("selected-claim-b2")]
    );
    release_session_execution_lease_for_test(&store, &predecessor_lease).await;

    let successor_owner = lease_owner("selected-multi-successor");
    let successor_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        &successor_owner.owner_id,
    )
    .await;
    let mixed = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &[
                batches[0].batch_id.clone(),
                batches[1].batch_id.clone(),
                batches[2].batch_id.clone(),
            ],
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    assert!(
        matches!(
            &mixed,
            Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) if required_batch_ids == &vec![
                batches[2].batch_id.clone(),
                batches[3].batch_id.clone(),
            ]
        ),
        "full A plus partial B must name B's literal complete composition: {mixed:?}"
    );

    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from(session_id),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("select two complete interrupted identities")
        .expect("the physically earliest identity is reclaimed");
    assert_eq!(
        successor_claim
            .batches
            .iter()
            .map(|batch| batch.source_key.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("selected-claim-a1"), Some("selected-claim-a2")]
    );
    assert_eq!(
        successor_claim.abandon_restore_claim_id.as_deref(),
        Some(claim_a.claim_id.as_str())
    );
    assert_eq!(
        successor_claim.abandon_restore_claim_token.as_deref(),
        Some(claim_a.lease_token.as_str())
    );
    store
        .abandon_queued_work_claim(&successor_claim)
        .await
        .expect("abandon successor A claim");

    for (selected, required) in [
        (
            &batches[0],
            vec![batches[0].batch_id.clone(), batches[1].batch_id.clone()],
        ),
        (
            &batches[2],
            vec![batches[2].batch_id.clone(), batches[3].batch_id.clone()],
        ),
    ] {
        let partial = store
            .claim_ready_queued_work_by_batch_ids(
                &SessionId::from(session_id),
                &successor_lease.fence(),
                &successor_owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&selected.batch_id),
                crate::testing::queued_work_claim_policy(64),
            )
            .await;
        assert!(
            matches!(
                &partial,
                Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                    required_batch_ids,
                }) if required_batch_ids == &required
            ),
            "abandon must restore both predecessor identities: {partial:?}"
        );
    }
    release_session_execution_lease_for_test(&store, &successor_lease).await;
}

pub(super) async fn process_wakes_batch_by_default(store: Arc<dyn RuntimePersistence>) {
    let merged_wakes = [
        policy_test_wake(
            &SessionId::from("wake-default-batch"),
            &ProcessId::from("process-a"),
            1,
        ),
        policy_test_wake(
            &SessionId::from("wake-default-batch"),
            &ProcessId::from("process-b"),
            1,
        ),
    ];
    for wake in &merged_wakes {
        store
            .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
            .await
            .expect("enqueue default-key wake");
    }
    let merge_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("wake-default-batch"),
        "merge-owner",
    )
    .await;
    let merged = store
        .claim_ready_queued_work(
            &SessionId::from("wake-default-batch"),
            &merge_lease.fence(),
            &lease_owner("merge-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim default-key wakes")
        .claim()
        .expect("default-key wakes exist");
    assert_eq!(
        merged.batches.len(),
        2,
        "the constant wake merge key must batch compatible wakes across processes"
    );
    assert!(
        merged
            .batches
            .iter()
            .all(|batch| { batch.merge_key.as_deref() == Some(crate::PROCESS_WAKE_MERGE_KEY) })
    );
    let state = RuntimeSessionState {
        session_id: SessionId::from("wake-default-batch"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(merge_lease.completion())
                .completing_queue_claim(merged.completion()),
        )
        .await
        .expect("settle every batch in merged wake claim");
    assert!(
        store
            .list_queued_work(&SessionId::from("wake-default-batch"))
            .await
            .expect("list merged queue after settlement")
            .is_empty(),
        "merged settlement must delete every claimed receiver row"
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
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("wake:{process_id}:{sequence}"),
        target_session_id: SessionId::from(session_id.to_string()),
        process_id: ProcessId::from(process_id.to_string()),
        process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: RuntimeAttribution::for_session(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: ProcessId::from(process_id.to_string()),
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

pub(super) async fn queued_work_completion_is_lease_guarded(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "join one",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("joined"),
        )
        .await
        .expect("enqueue first joined batch");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "join two",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_merge_key("joined"),
        )
        .await
        .expect("enqueue second joined batch");
    let claim_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &claim_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim joined batches")
        .claim()
        .expect("joined claim exists");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );

    let mut stale_completion = claim.completion();
    stale_completion.lease_token.push_str(":stale");
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_completion),
        )
        .await
        .expect_err("stale queued-work completion must fail");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("stale completion preserves queued work")
            .len(),
        2
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(claim_session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("valid queued-work completion commits");
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("valid completion clears queued work")
            .is_empty()
    );
}

pub(super) async fn queue_completion_and_turn_commit_stamp_are_atomic(
    store: Arc<dyn RuntimePersistence>,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "atomic queue",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue queue batch");
    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "queue-owner")
            .await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim queue")
        .claim()
        .expect("queue claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from("root"),
            "atomic pending input",
        ))
        .await
        .expect("enqueue atomic pending input");
    let input_claim = store
        .claim_next_turn_inputs(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            1,
        )
        .await
        .expect("claim atomic pending input")
        .expect("atomic pending input claim");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 41,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut base_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    base_commit.enqueued_queue_batches = vec![
        QueuedWorkBatchDraft::new(
            "root",
            DeliveryPolicy::AfterCurrentTurnCommit,
            crate::TurnWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(&SessionId::from("root"), "follow-frame"),
                "follow-on task",
                None,
            ),
        )
        .with_source_key("agent-frame-handoff:turn-atomic"),
    ];
    let turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "turn-atomic", "final"));
    base_commit.turn_commit = turn_commit.clone();
    let mut stale_queue_completion = claim.completion();
    stale_queue_completion.lease_token.push_str(":stale");
    let err = store
        .commit_runtime_state(
            base_commit
                .clone()
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(stale_queue_completion),
        )
        .await
        .expect_err("stale queue completion must reject the whole final commit");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after rejected atomic commit")
            .is_none(),
        "rejected queue completion must not persist session state"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after rejected atomic commit")
            .len(),
        1,
        "rejected queue completion must preserve queued work"
    );

    let mut cross_session_outbox = base_commit.clone();
    cross_session_outbox.enqueued_queue_batches[0].session_id =
        SessionId::from("other-session".to_string());
    let err = store
        .commit_runtime_state(
            cross_session_outbox
                .completing_queue_claim(claim.completion())
                .completing_turn_input_claim(input_claim.completion()),
        )
        .await
        .expect_err("outbox enqueue failure must reject the whole final commit");
    assert!(matches!(err, StoreError::SessionBindingMismatch { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after rejected outbox enqueue")
            .is_none(),
        "rejected outbox enqueue must roll back session state"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after rejected outbox enqueue")
            .len(),
        1,
        "rejected outbox enqueue must roll back inbound queue completion"
    );

    let first = store
        .commit_runtime_state(
            base_commit
                .clone()
                .releasing_session_execution_lease(session_lease.completion())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion()),
        )
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
            retry
                .releasing_session_execution_lease(session_lease.completion())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion())
        })
        .await
        .expect("same final turn commit stamp retries idempotently");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(
        retry.realized_node_timestamps,
        first.realized_node_timestamps
    );
    assert_eq!(first.enqueued_queue_batches.len(), 1);
    assert_eq!(retry.enqueued_queue_batches.len(), 1);
    assert_eq!(
        retry.enqueued_queue_batches[0].batch_id, first.enqueued_queue_batches[0].batch_id,
        "idempotent commit retry must return the original outbox identity"
    );
    assert!(
        store
            .load_session()
            .await
            .expect("load after accepted atomic commit")
            .is_some()
    );
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after accepted atomic commit")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .eq([first.enqueued_queue_batches[0].batch_id.as_str()])
    );
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list inputs after accepted atomic commit")
            .is_empty(),
        "accepted switch commit must complete inbound input with the outbox enqueue"
    );
}
