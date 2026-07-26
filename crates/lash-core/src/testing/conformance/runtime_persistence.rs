//! [`RuntimePersistence`] conformance, organized by capability segment:
//! [`SessionCommitStore`](crate::SessionCommitStore) (head CAS, checkpoint
//! hydration, metadata, attachment manifest, turn-commit stamps),
//! [`SessionExecutionLeaseStore`](crate::SessionExecutionLeaseStore),
//! [`QueuedWorkStore`](crate::QueuedWorkStore) (claim fencing),
//! [`TurnInputStore`](crate::TurnInputStore), and
//! [`StoreMaintenance`](crate::StoreMaintenance).

use super::*;

/// Attachment-manifest behavior expected from a [`RuntimePersistence`] backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentManifestConformance {
    /// The backend stores and reconciles attachment intent rows.
    Persistent,
    /// The backend explicitly has no attachment-write story and uses the no-op
    /// manifest contract.
    Noop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimePersistenceConformance {
    pub attachment_manifest: AttachmentManifestConformance,
    pub durability_tier: crate::DurabilityTier,
    /// Whether the backend physically reclaims unreachable checkpoint blobs on
    /// [`gc_unreachable`](crate::StoreMaintenance::gc_unreachable). Blob-backed
    /// durable stores (SQLite, Postgres) set this; the in-memory store keeps
    /// everything in RAM and reports an empty [`GcReport`](crate::GcReport), so
    /// the stronger reclamation assertion is gated off for it.
    pub reclaims_unreachable_blobs: bool,
}

impl RuntimePersistenceConformance {
    pub const fn persistent_attachment_manifest(durability_tier: crate::DurabilityTier) -> Self {
        Self {
            attachment_manifest: AttachmentManifestConformance::Persistent,
            durability_tier,
            reclaims_unreachable_blobs: false,
        }
    }

    pub const fn noop_attachment_manifest(durability_tier: crate::DurabilityTier) -> Self {
        Self {
            attachment_manifest: AttachmentManifestConformance::Noop,
            durability_tier,
            reclaims_unreachable_blobs: false,
        }
    }

    /// Assert the backend reclaims unreachable checkpoint blobs on GC, running
    /// the stronger mark/sweep conformance in addition to the minimal
    /// safe-to-call check.
    pub const fn reclaiming_unreachable_blobs(mut self) -> Self {
        self.reclaims_unreachable_blobs = true;
        self
    }
}

impl Default for RuntimePersistenceConformance {
    fn default() -> Self {
        Self {
            attachment_manifest: AttachmentManifestConformance::Persistent,
            durability_tier: crate::DurabilityTier::Durable,
            reclaims_unreachable_blobs: false,
        }
    }
}

/// Run the [`RuntimePersistence`] durability conformance suite against the
/// backend produced by `make`. `make` must return a fresh, empty,
/// single-session store on each call.
///
/// Covers the durability crown jewels owned by the store, grouped by
/// capability segment: optimistic head CAS, session binding, checkpoint/usage
/// hydration, session metadata, attachment manifest intent/commit/GC
/// reconciliation, and idempotent final turn commit stamps
/// ([`SessionCommitStore`](crate::SessionCommitStore)); execution-lane fencing
/// ([`SessionExecutionLeaseStore`](crate::SessionExecutionLeaseStore));
/// queued-work ingress and claim fencing
/// ([`QueuedWorkStore`](crate::QueuedWorkStore)); the pending turn-input
/// lifecycle ([`TurnInputStore`](crate::TurnInputStore)); and tombstone/GC
/// behavior ([`StoreMaintenance`](crate::StoreMaintenance)).
/// Effect-host workflow history is deliberately outside this suite.
pub async fn runtime_persistence<F>(make: F)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    runtime_persistence_with_options(
        make,
        RuntimePersistenceConformance::persistent_attachment_manifest(
            crate::DurabilityTier::Inline,
        ),
    )
    .await;
}

/// Run the full [`RuntimePersistence`] suite plus durable reopen checks.
pub async fn runtime_persistence_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableRuntimePersistence,
{
    runtime_persistence_with_options(
        || make().open,
        RuntimePersistenceConformance::persistent_attachment_manifest(
            crate::DurabilityTier::Durable,
        )
        .reclaiming_unreachable_blobs(),
    )
    .await;
    runtime_persistence_survives_reopen(make()).await;
}

/// Prove lease and claim expiry using an injected embedded-backend clock.
///
/// This complements the full suite's real-time expiry check: the real-time
/// proof guards production `SystemClock` wiring, while this variant proves an
/// embedded store actually consults its injected [`Clock`](crate::Clock).
pub async fn runtime_persistence_clock_expiry(
    store: Arc<dyn RuntimePersistence>,
    advance: impl FnOnce(u64),
) {
    const TTL_MS: u64 = 1_000;
    let session_id = "clock-expiry";
    let stale_owner = lease_owner("clock-expiry-stale");
    let successor = lease_owner("clock-expiry-successor");
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "clock expiry queued work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue clock-expiry queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "clock expiry turn input",
        ))
        .await
        .expect("enqueue clock-expiry turn input");
    let stale_lease = store
        .try_claim_session_execution_lease(session_id, &stale_owner, TTL_MS)
        .await
        .expect("claim clock-expiry stale lease")
        .acquired()
        .expect("clock-expiry stale lease acquired");
    let stale_queue_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .expect("claim clock-expiry queued work")
        .expect("clock-expiry queued work claim exists");
    let stale_input_claim = store
        .claim_next_turn_inputs(session_id, &stale_lease.fence(), &stale_owner, 1)
        .await
        .expect("claim clock-expiry turn input")
        .expect("clock-expiry turn input claim exists");

    advance(TTL_MS + 1);

    let successor_lease = store
        .try_claim_session_execution_lease(session_id, &successor, TTL_MS)
        .await
        .expect("claim clock-expiry successor lease")
        .acquired()
        .expect("expired lease is claimable through injected time");
    assert!(successor_lease.fencing_token > stale_lease.fencing_token);
    let successor_queue_claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &successor_lease.fence(),
            &successor,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .expect("reclaim clock-expiry queued work")
        .expect("dead-generation queued work is reclaimable");
    let successor_input_claim = store
        .claim_next_turn_inputs(session_id, &successor_lease.fence(), &successor, 1)
        .await
        .expect("reclaim clock-expiry turn input")
        .expect("dead-generation turn input is reclaimable");
    assert!(successor_queue_claim.fencing_token > stale_queue_claim.fencing_token);
    assert!(successor_input_claim.fencing_token > stale_input_claim.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_commit = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(stale_lease.fence())
                .completing_queue_claim(stale_queue_claim.completion())
                .completing_turn_input_claim(stale_input_claim.completion()),
        )
        .await;
    assert!(matches!(
        stale_commit,
        Err(StoreError::QueuedWorkClaimSuperseded { .. })
    ));

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(successor_lease.fence())
                .releasing_session_execution_lease(successor_lease.completion())
                .completing_queue_claim(successor_queue_claim.completion())
                .completing_turn_input_claim(successor_input_claim.completion()),
        )
        .await
        .expect("successor settles reclaimed clock-expiry claims");
    assert_eq!(stale_queue_claim.batches[0].batch_id, batch.batch_id);
    assert_eq!(stale_input_claim.inputs[0].input_id, input.input_id);
}

pub async fn runtime_persistence_with_options<F>(make: F, options: RuntimePersistenceConformance)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    // [`SessionCommitStore`]: atomic head commits, reads, metadata, the
    // attachment write-ahead manifest, and turn-commit idempotency.
    runtime_persistence_reports_declared_durability(make(), options.durability_tier).await;
    commit_increments_head_and_round_trips_agent_frames(make()).await;
    concurrent_head_revision_cas_applies_exactly_once(make()).await;
    commit_rejects_a_different_session_id(make()).await;
    load_hydrates_checkpoint_and_usage(make()).await;
    active_path_read_scope_selects_only_requested_ancestry(make()).await;
    session_metadata_round_trips(make()).await;
    match options.attachment_manifest {
        AttachmentManifestConformance::Persistent => {
            attachment_manifest_records_intent_and_commit_stamps(make()).await;
            attachment_manifest_keeps_same_content_ownership_per_session(make()).await;
            attachment_manifest_reference_tracking_and_gc_root_set(make()).await;
        }
        AttachmentManifestConformance::Noop => {
            noop_attachment_manifest_is_explicit_and_empty(make()).await;
        }
    }
    final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(make()).await;
    commit_rejects_non_derived_append_node_ids(make()).await;
    // [`SessionExecutionLeaseStore`]: single-writer lane fencing.
    session_execution_lease_contract(make()).await;
    session_execution_lease_reclaim_contract(make()).await;
    // [`QueuedWorkStore`]: durable queued-work ingress, ordering, and claim
    // leases, plus the commit-side completion atomicity it shares with
    // [`SessionCommitStore`].
    queued_work_source_keys_are_idempotent_and_list_ordered(make()).await;
    concurrent_queue_and_turn_input_claims_have_one_owner(make()).await;
    checkpoint_work_claims_both_families_once(make()).await;
    queued_work_cancel_removes_only_unclaimed_batches(make()).await;
    queued_work_exact_claim_uses_selected_batch_ids(make()).await;
    queued_work_classes_gate_command_and_turn_claims(make()).await;
    queued_work_claims_respect_boundaries_abandon_and_stale_completion(make()).await;
    queued_work_claims_supersede_across_session_lease_generations(make()).await;
    claim_liveness_for_lease_less_paths_tracks_session_generations(make()).await;
    same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(make()).await;
    queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(make()).await;
    queued_work_join_groups_by_delivery_policy_and_merge_key(make()).await;
    queued_work_completion_is_lease_guarded(make()).await;
    queued_wake_delivery_is_source_key_idempotent_and_claimed_once(make()).await;
    queue_completion_and_turn_commit_stamp_are_atomic(make()).await;
    // [`TurnInputStore`]: pending turn-input lifecycle.
    pending_turn_inputs_source_keys_order_cancel_and_cross_session(make()).await;
    pending_turn_input_bulk_and_suffix_cancellation(make()).await;
    pending_turn_input_claims_reclaim_complete_and_fence(make()).await;
    turn_input_application_identity_survives_pending_tombstone_vacuum(make()).await;
    turn_input_claims_supersede_across_session_lease_generations(make()).await;
    pending_turn_input_cancel_covers_active_and_deferred_states(make()).await;
    pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(make()).await;
    // [`StoreMaintenance`]: tombstone/vacuum/GC retention.
    tombstone_vacuum_and_gc_are_minimally_consistent(make()).await;
    if options.reclaims_unreachable_blobs {
        gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(make()).await;
    }
}

async fn turn_input_application_identity_survives_pending_tombstone_vacuum(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "turn-input-application";
    let owner_id = "turn-input-application-owner";
    let lease = claim_session_execution_lease_for_test(&store, session_id, owner_id).await;
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    let mut expected = Vec::new();
    let mut replay = None;

    for (turn_index, turn_id) in ["z-first-application-turn", "a-second-application-turn"]
        .into_iter()
        .enumerate()
    {
        let admitted = futures_util::future::try_join_all((0..2).map(|input_index| {
            store.enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    session_id,
                    &format!("canonical application {turn_index}:{input_index}"),
                )
                .with_source_key(format!(
                    "host:application-source-{turn_index}-{input_index}"
                )),
            )
        }))
        .await
        .expect("enqueue application inputs");
        let mut claim = store
            .claim_next_turn_inputs(session_id, &lease.fence(), &lease_owner(owner_id), 10)
            .await
            .expect("claim application inputs")
            .expect("application input claim");
        let committed_message_id = format!("application-message-{turn_index}");
        claim.record_initial_turn_application(turn_id, &committed_message_id);
        let turn_expected = claim.applications.clone();
        assert_eq!(admitted.len(), turn_expected.len());

        let mut commit = RuntimeCommit::persisted_state(&state, &[])
            .with_session_execution_lease(lease.fence())
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(lease.completion());
        }
        let hash = commit
            .turn_commit_hash()
            .expect("hash application turn commit");
        commit = commit.with_turn_commit(crate::RuntimeTurnCommitStamp::new(
            session_id,
            crate::OperationId::turn(session_id, turn_id, "final"),
            hash,
        ));
        if turn_index == 1 {
            replay = Some(commit.clone());
        }
        let result = store
            .commit_runtime_state(commit)
            .await
            .expect("commit application identity");
        state.head_revision = Some(result.head_revision);

        assert_eq!(result.turn_input_applications, turn_expected);
        expected.extend(turn_expected);
    }

    let replayed = store
        .commit_runtime_state(replay.expect("second turn commit replay"))
        .await
        .expect("replay application turn commit");
    assert_eq!(
        replayed.turn_input_applications,
        expected[2..],
        "an exact turn-commit replay must retain its applications"
    );
    assert_eq!(
        store
            .list_turn_input_applications(session_id)
            .await
            .expect("read durable application identity"),
        expected,
        "applications must follow monotonic turn-commit order and must not double-count a replay"
    );

    store.vacuum().await.expect("vacuum application tombstone");
    assert_eq!(
        store
            .list_turn_input_applications(session_id)
            .await
            .expect("read application identity after tombstone vacuum"),
        expected,
        "application reconciliation must come from the committed turn, not a pending snapshot"
    );
}

async fn checkpoint_work_claims_both_families_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "checkpoint-work";
    let turn_id = "checkpoint-turn";
    let owner = lease_owner("checkpoint-owner");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint input",
        ))
        .await
        .expect("enqueue checkpoint input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "checkpoint queued work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim checkpoint session lease")
        .acquired()
        .expect("checkpoint session lease acquired");

    let (input_claim, queue_claim) = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            10,
        )
        .await
        .expect("claim both checkpoint work families");
    let input_claim = input_claim.expect("checkpoint input claim exists");
    let queue_claim = queue_claim.expect("checkpoint queue claim exists");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    assert_eq!(queue_claim.batches[0].batch_id, batch.batch_id);
    assert_eq!(input_claim.session_lease_generation, lease.fencing_token);
    assert_eq!(queue_claim.session_lease_generation, lease.fencing_token);

    let second = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            10,
        )
        .await
        .expect("same-generation checkpoint re-claim");
    assert!(
        second.0.is_none() && second.1.is_none(),
        "checkpoint claims must be granted exactly once per lease generation"
    );
}

/// Prove checkpoint admission probes stay read-only for empty queues and for
/// deferred queue heads, while real checkpoint work still shares one write
/// transaction and deferred work remains claimable at the idle boundary.
pub async fn checkpoint_claim_probe_transaction_counts(
    store: Arc<dyn RuntimePersistence>,
    session_id: &str,
    counts: impl Fn() -> (usize, usize),
) {
    let turn_id = format!("{session_id}:counter-turn");
    let owner = lease_owner(&format!("{session_id}:checkpoint-counter-owner"));
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim checkpoint counter lease")
        .acquired()
        .expect("checkpoint counter lease acquired");

    let empty = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("probe quiescent checkpoint");
    assert!(empty.0.is_none() && empty.1.is_none());
    assert_eq!(counts(), (1, 0));

    let deferred = store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "deferred checkpoint head",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue deferred checkpoint head");
    let deferred_checkpoint = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("probe deferred checkpoint head");
    assert!(
        deferred_checkpoint.0.is_none() && deferred_checkpoint.1.is_none(),
        "after-current-turn-commit work must not claim at an active checkpoint"
    );
    assert_eq!(
        counts(),
        (2, 0),
        "a deferred queue head must not open a checkpoint write transaction"
    );

    let deferred_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            64,
        )
        .await
        .expect("claim deferred work at idle boundary")
        .expect("deferred work remains claimable at idle boundary");
    assert_eq!(deferred_claim.batches[0].batch_id, deferred.batch_id);

    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session_id,
            crate::TurnInputIngress::active_turn(
                turn_id.clone(),
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("pending checkpoint input"),
        ))
        .await
        .expect("enqueue counter input");
    store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "pending checkpoint work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue counter work");
    let pending = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            64,
        )
        .await
        .expect("claim pending checkpoint work");
    assert!(pending.0.is_some() && pending.1.is_some());
    assert_eq!(counts(), (3, 1));
}

/// Build a queued process-wake draft for backend conformance tests.
pub fn queued_process_wake_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
) -> QueuedWorkBatchDraft {
    let wake = ProcessWakeDelivery {
        wake_id: format!("wake:{session_id}:{text}"),
        target_session_id: session_id.to_string(),
        target_scope_id: SessionScopeId::new(format!("session:{session_id}")),
        process_id: format!("process:{text}"),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: format!("process:{text}"),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        dedupe_key: format!("wake:{session_id}:{text}:1"),
        input: text.to_string(),
        created_at_ms: 1,
    };
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        slot_policy,
        vec![QueuedWorkPayload::process_wake(wake)],
    )
}

fn queued_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
) -> QueuedWorkBatchDraft {
    queued_process_wake_draft(session_id, text, delivery_policy, slot_policy)
}

fn queued_session_command_draft(session_id: &str, reason: &str) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        DeliveryPolicy::EarliestSafeBoundary,
        SlotPolicy::Exclusive,
        vec![QueuedWorkPayload::session_command(
            crate::SessionCommand::RefreshToolCatalog {
                reason: reason.to_string(),
            },
        )],
    )
}

fn queued_batch_text(batch: &QueuedWorkBatch) -> Option<&str> {
    let payload = batch.items.first().map(|item| &item.payload)?;
    match payload {
        QueuedWorkPayload::ProcessWake { wake } => Some(wake.input.as_str()),
        QueuedWorkPayload::AgentFrameTask { task, .. } => Some(task.as_str()),
        QueuedWorkPayload::SessionCommand { .. } => None,
    }
}

fn pending_next_turn_input_draft(session_id: &str, text: &str) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::NextTurn,
        crate::TurnInput::text(text),
    )
}

fn inline_png(bytes: Vec<u8>) -> crate::AttachmentSource {
    crate::AttachmentSource::inline(crate::MediaType::parse("image/png").unwrap(), bytes)
}

fn pending_active_turn_input_draft(
    session_id: &str,
    turn_id: &str,
    min_boundary: crate::TurnInputCheckpointBoundary,
    text: &str,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::active_turn(turn_id, min_boundary),
        crate::TurnInput::text(text),
    )
}

fn pending_input_text(input: &crate::PendingTurnInput) -> Option<&str> {
    match input.input.items.first()? {
        crate::InputItem::Text { text } => Some(text.as_str()),
        crate::InputItem::Attachment { .. } => None,
    }
}

fn expect_cancelled_pending_input(
    outcome: crate::PendingTurnInputCancelOutcome,
    input_id: &str,
) -> crate::PendingTurnInput {
    match outcome {
        crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
            assert_eq!(input.input_id, input_id);
            assert_eq!(input.state, crate::TurnInputState::Cancelled);
            input
        }
        other => panic!("expected cancelled pending turn input `{input_id}`, got {other:?}"),
    }
}

fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

fn local_lease_owner(
    owner_id: &str,
    incarnation_id: &str,
    host_id: &str,
    boot_id: &str,
    pid: u32,
    process_start: &str,
) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity {
        owner_id: owner_id.to_string(),
        incarnation_id: incarnation_id.to_string(),
        liveness: crate::LeaseOwnerLiveness::local_process_for_test(
            host_id,
            boot_id,
            pid,
            process_start,
        ),
    }
}

async fn claim_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner_id: &str,
) -> crate::SessionExecutionLease {
    let owner = lease_owner(owner_id);
    store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease is free")
}

async fn release_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    lease: &crate::SessionExecutionLease,
) {
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release session execution lease");
}

async fn commit_runtime_state_for_test(
    store: &Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    owner_id: &str,
) -> Result<crate::store::RuntimeCommitResult, StoreError> {
    let session_id = commit.session_id.clone();
    let lease = claim_session_execution_lease_for_test(store, &session_id, owner_id).await;
    store
        .commit_runtime_state(
            commit
                .with_session_execution_lease(lease.fence())
                .releasing_session_execution_lease(lease.completion()),
        )
        .await
}

fn sample_session_node(id: &str, parent: Option<&str>) -> SessionNodeRecord {
    SessionNodeRecord {
        node_id: id.to_string(),
        parent_node_id: parent.map(ToOwned::to_owned),
        caused_by: None,
        agent_frame_id: None,
        timestamp: "1970-01-01T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                ProtocolEvent::typed("conformance", serde_json::json!({ "node": id }))
                    .expect("protocol event"),
            ),
        },
    }
}

fn attachment_intent(id: &str) -> AttachmentIntent {
    AttachmentIntent {
        attachment_id: AttachmentId::new(id.to_string()),
        session_id: "root".to_string(),
        canonical_uri: format!("sha256:{id}"),
        intent_at_epoch_ms: 100,
        owner_kind: None,
        owner_id: None,
    }
}

async fn commit_increments_head_and_round_trips_agent_frames(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        policy: SessionPolicy {
            model: ModelSpec::from_token_limits("gpt-5.4-mini", Default::default(), 200_000, None)
                .expect("valid model spec"),
            ..SessionPolicy::default()
        },
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let previous_frame_id = state.current_agent_frame_id.clone();
    let assignment = state
        .current_agent_frame()
        .expect("initial frame")
        .assignment
        .clone();
    let custom_reason = AgentFrameReason::new("plan_mode");
    state.append_agent_frame(AgentFrameRecord::new(
        "frame-2".to_string(),
        "root".to_string(),
        Some(previous_frame_id),
        custom_reason.clone(),
        None,
        assignment,
        ProtocolTurnOptions::default(),
    ));
    state.set_execution_state_snapshot(Some(b"frame-vm".to_vec()));

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&state, &[]),
        "commit-round-trip",
    )
    .await
    .expect("commit runtime state");
    let read = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load session")
        .expect("session read");

    assert_eq!(read.current_agent_frame_id, "frame-2");
    assert_eq!(read.agent_frames.len(), 2);
    let current = read
        .agent_frames
        .iter()
        .find(|frame| frame.frame_id == "frame-2")
        .expect("current frame");
    assert_eq!(current.reason, custom_reason);
    assert_eq!(
        current.execution_state_snapshot.as_deref(),
        Some(&b"frame-vm"[..])
    );
    assert_eq!(
        read.checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.execution_state.as_deref()),
        Some(&b"frame-vm"[..])
    );
}

async fn concurrent_head_revision_cas_applies_exactly_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "concurrent-head-cas";
    let lease = claim_session_execution_lease_for_test(&store, session_id, "cas-owner").await;
    let make_commit = |node_id: &str| {
        let state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::default()
        };
        let node = sample_session_node(node_id, None);
        RuntimeCommit {
            expected_head_revision: Some(0),
            graph: crate::GraphCommitDelta::ReplaceFull(crate::SessionGraph::from_nodes(
                vec![node],
                Some(node_id.to_string()),
            )),
            ..RuntimeCommit::persisted_state(&state, &[])
        }
        .with_session_execution_lease(lease.fence())
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_commit = make_commit("cas-left");
    let right_commit = make_commit("cas-right");
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store.commit_runtime_state(left_commit).await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store.commit_runtime_state(right_commit).await
    });

    barrier.wait().await;
    let left = left.await.expect("join left head-CAS writer");
    let right = right.await.expect("join right head-CAS writer");
    let winners = [&left, &right]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    let conflicts = [&left, &right]
        .into_iter()
        .filter(|result| matches!(result, Err(StoreError::HeadRevisionConflict { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent writer must win head CAS, got left={left:?} right={right:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the losing writer must receive HeadRevisionConflict, got left={left:?} right={right:?}"
    );

    let persisted = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load state after concurrent head CAS")
        .expect("concurrent head-CAS winner persisted a session");
    assert_eq!(persisted.head_revision, 1, "exactly one commit applied");
    assert_eq!(persisted.graph.nodes.len(), 1, "exactly one graph applied");
    assert!(
        matches!(
            persisted.graph.nodes[0].node_id.as_str(),
            "cas-left" | "cas-right"
        ),
        "the persisted graph must come from one of the two writers"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
}

async fn commit_rejects_a_different_session_id(store: Arc<dyn RuntimePersistence>) {
    let alpha = RuntimeSessionState {
        session_id: "alpha".to_string(),
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&alpha, &[]),
        "bind-alpha",
    )
    .await
    .expect("first commit binds the session");
    let beta = RuntimeSessionState {
        session_id: "beta".to_string(),
        ..RuntimeSessionState::default()
    };
    let result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&beta, &[]),
        "bind-beta",
    )
    .await;
    assert!(
        result.is_err(),
        "a single-session store must reject a commit for a different session id"
    );
}

async fn load_hydrates_checkpoint_and_usage(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "hydrated".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(9)),
        plugin_snapshot_revision: Some(12),
        plugin_snapshot: Some(PluginSessionSnapshot {
            plugins: Default::default(),
        }),
        ..RuntimeSessionState::default()
    };
    let usage = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "mock-model".to_string(),
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 5,
        },
    };

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&state, &[usage]),
        "hydrate",
    )
    .await
    .expect("commit");

    let read = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load")
        .expect("session");
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(read.session_id, "hydrated");
    assert_eq!(
        checkpoint
            .tool_state
            .expect("dynamic snapshot")
            .generation(),
        9
    );
    assert_eq!(checkpoint.plugin_snapshot_revision, Some(12));
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].usage.input_tokens, 11);
}

async fn runtime_persistence_reports_declared_durability(
    store: Arc<dyn RuntimePersistence>,
    expected_tier: crate::DurabilityTier,
) {
    assert_eq!(
        store.durability_tier(),
        expected_tier,
        "runtime persistence conformance must pin the backend's declared durability tier"
    );
}

async fn session_execution_lease_contract(store: Arc<dyn RuntimePersistence>) {
    let first = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let owner_a = lease_owner("owner-a");
    let owner_a_next = crate::LeaseOwnerIdentity::opaque("owner-a", "owner-a:next-incarnation");
    let owner_b = lease_owner("owner-b");
    let owner_c = lease_owner("owner-c");
    let owner_expired = lease_owner("owner-expired");
    let reentered = store
        .try_claim_session_execution_lease("root", &owner_a, 120_000)
        .await
        .expect("same incarnation may re-enter live session lease")
        .acquired()
        .expect("same incarnation receives existing session lease");
    assert_eq!(reentered.lease_token, first.lease_token);
    assert_eq!(reentered.fencing_token, first.fencing_token);
    assert!(reentered.expires_at_epoch_ms >= first.expires_at_epoch_ms);
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_a_next, 60_000)
                .await
                .expect("try same owner next incarnation"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude the same owner in a different incarnation"
    );
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_b, 60_000)
                .await
                .expect("try concurrent session lease"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude concurrent owners"
    );
    let renewed = store
        .renew_session_execution_lease(&reentered.fence(), 120_000)
        .await
        .expect("renew live session lease");
    assert_eq!(renewed.lease_token, first.lease_token);
    assert!(renewed.expires_at_epoch_ms >= reentered.expires_at_epoch_ms);

    let mut stale_fence = reentered.fence();
    stale_fence.lease_token.push_str(":stale");
    let err = store
        .renew_session_execution_lease(&stale_fence, 60_000)
        .await
        .expect_err("stale session lease renew must fail");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    store
        .release_session_execution_lease(&crate::SessionExecutionLeaseCompletion {
            session_id: first.session_id.clone(),
            owner: first.owner.clone(),
            lease_token: format!("{}:stale", first.lease_token),
            fencing_token: first.fencing_token,
        })
        .await
        .expect("stale release is fenced and idempotent");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_b, 60_000)
                .await
                .expect("try after stale release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "stale release must not clear the live lease"
    );
    release_session_execution_lease_for_test(&store, &renewed).await;
    let second = claim_session_execution_lease_for_test(&store, "root", "owner-b").await;
    assert!(
        second.fencing_token > first.fencing_token,
        "reclaimed session leases must advance the fencing token"
    );
    store
        .release_session_execution_lease(&first.completion())
        .await
        .expect("old release is idempotent");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("root", &owner_c, 60_000)
                .await
                .expect("try after old release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "old release must not clear a newer lease"
    );
    release_session_execution_lease_for_test(&store, &second).await;

    let expired = store
        .try_claim_session_execution_lease("root", &owner_expired, 0)
        .await
        .expect("claim expiring lease")
        .acquired()
        .expect("expiring lease");
    let reclaimed = claim_session_execution_lease_for_test(&store, "root", "owner-reclaim").await;
    assert!(reclaimed.fencing_token > expired.fencing_token);
    release_session_execution_lease_for_test(&store, &reclaimed).await;

    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let lease_free_commit = store
        .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
        .await
        .expect("head CAS, not the advisory lease, authorizes commit");
    state.head_revision = Some(lease_free_commit.head_revision);

    let commit_lease = claim_session_execution_lease_for_test(&store, "root", "commit-owner").await;
    let lease_commit = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(commit_lease.fence())
                .releasing_session_execution_lease(commit_lease.completion()),
        )
        .await
        .expect("advisory lease-bearing commit");
    state.head_revision = Some(lease_commit.head_revision);
    let after_commit = claim_session_execution_lease_for_test(&store, "root", "after-commit").await;
    release_session_execution_lease_for_test(&store, &after_commit).await;

    let turn_state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        head_revision: state.head_revision,
        ..RuntimeSessionState::default()
    };
    let turn_commit = RuntimeCommit::persisted_state(&turn_state, &[]);
    let turn_hash = turn_commit.turn_commit_hash().expect("turn hash");
    let turn_commit = turn_commit.with_turn_commit(RuntimeTurnCommitStamp::new(
        "root",
        crate::OperationId::turn("root", "lease-replay-turn", "final"),
        turn_hash,
    ));
    let turn_lease = claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    let first_result = store
        .commit_runtime_state(
            turn_commit
                .clone()
                .with_session_execution_lease(turn_lease.fence())
                .releasing_session_execution_lease(turn_lease.completion()),
        )
        .await
        .expect("first final commit under session lease");
    let replay = store
        .commit_runtime_state(turn_commit)
        .await
        .expect("idempotent replay returns without live session lease");
    assert_eq!(replay.head_revision, first_result.head_revision);

    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "fenced queue",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue fenced queue work");
    let err = store
        .claim_ready_queued_work(
            "root",
            &commit_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect_err("queued-work claims require a live session lease");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    let queue_lease = claim_session_execution_lease_for_test(&store, "root", "queue-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &queue_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim fenced queue work")
        .expect("queue work claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    release_session_execution_lease_for_test(&store, &queue_lease).await;
}

async fn session_execution_lease_reclaim_contract(store: Arc<dyn RuntimePersistence>) {
    let pid = std::process::id();
    let dead_holder = local_lease_owner(
        "dead-holder",
        "dead-holder:incarnation",
        "host-a",
        "boot-a",
        pid,
        "not-the-current-process-start",
    );
    let claimant = local_lease_owner(
        "claimant",
        "claimant:incarnation",
        "host-a",
        "boot-a",
        pid,
        "claimant-start",
    );
    let holder = store
        .try_claim_session_execution_lease("reclaim-dead", &dead_holder, 60_000)
        .await
        .expect("claim dead-holder lease")
        .acquired()
        .expect("dead-holder lease acquired");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease("reclaim-dead", &claimant, 60_000)
                .await
                .expect("try claimant against dead holder"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "plain claim must report busy before the caller performs fenced reclaim"
    );
    let reclaimed = store
        .reclaim_session_execution_lease("reclaim-dead", &claimant, &holder.fence(), 60_000)
        .await
        .expect("reclaim dead holder")
        .acquired()
        .expect("dead holder is reclaimable before ttl");
    assert!(
        reclaimed.fencing_token > holder.fencing_token,
        "fenced reclaim must advance the fencing token"
    );
    let stale_reclaim = store
        .reclaim_session_execution_lease(
            "reclaim-dead",
            &local_lease_owner(
                "late-claimant",
                "late-claimant:incarnation",
                "host-a",
                "boot-a",
                pid,
                "late-claimant-start",
            ),
            &holder.fence(),
            60_000,
        )
        .await
        .expect("stale observed-holder reclaim");
    assert!(
        matches!(
            stale_reclaim,
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a stale observed holder must not clear the newer lease"
    );
    release_session_execution_lease_for_test(&store, &reclaimed).await;

    let race_holder = store
        .try_claim_session_execution_lease("reclaim-race", &dead_holder, 60_000)
        .await
        .expect("claim race holder")
        .acquired()
        .expect("race holder acquired");
    let race_fence = race_holder.fence();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_fence = race_fence.clone();
    let right_fence = race_fence.clone();
    let left_claimant = local_lease_owner(
        "race-left",
        "race-left:incarnation",
        "host-a",
        "boot-a",
        pid,
        "race-left-start",
    );
    let right_claimant = local_lease_owner(
        "race-right",
        "race-right:incarnation",
        "host-a",
        "boot-a",
        pid,
        "race-right-start",
    );
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .reclaim_session_execution_lease("reclaim-race", &left_claimant, &left_fence, 60_000)
            .await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .reclaim_session_execution_lease("reclaim-race", &right_claimant, &right_fence, 60_000)
            .await
    });
    barrier.wait().await;
    let left = left
        .await
        .expect("join left reclaim race")
        .expect("left reclaim race");
    let right = right
        .await
        .expect("join right reclaim race")
        .expect("right reclaim race");
    let mut race_winners = [left, right]
        .into_iter()
        .filter_map(crate::SessionExecutionLeaseClaimOutcome::acquired)
        .collect::<Vec<_>>();
    assert_eq!(
        race_winners.len(),
        1,
        "exactly one claimant may win a fenced reclaim race"
    );
    let race_winner = race_winners.pop().expect("race winner");
    assert!(race_winner.fencing_token > race_holder.fencing_token);
    release_session_execution_lease_for_test(&store, &race_winner).await;

    let cross_host_holder = store
        .try_claim_session_execution_lease("reclaim-cross-host", &dead_holder, 60_000)
        .await
        .expect("claim cross-host holder")
        .acquired()
        .expect("cross-host holder acquired");
    let cross_host_result = store
        .reclaim_session_execution_lease(
            "reclaim-cross-host",
            &local_lease_owner(
                "cross-host-claimant",
                "cross-host-claimant:incarnation",
                "host-b",
                "boot-a",
                pid,
                "claimant-start",
            ),
            &cross_host_holder.fence(),
            60_000,
        )
        .await
        .expect("cross-host reclaim");
    assert!(
        matches!(
            cross_host_result,
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "dead-looking local-process holders are reclaimable only on the same host"
    );
    release_session_execution_lease_for_test(&store, &cross_host_holder).await;

    let different_boot_holder = store
        .try_claim_session_execution_lease("reclaim-different-boot", &dead_holder, 60_000)
        .await
        .expect("claim different-boot holder")
        .acquired()
        .expect("different-boot holder acquired");
    let different_boot_result = store
        .reclaim_session_execution_lease(
            "reclaim-different-boot",
            &local_lease_owner(
                "different-boot-claimant",
                "different-boot-claimant:incarnation",
                "host-a",
                "boot-b",
                pid,
                "claimant-start",
            ),
            &different_boot_holder.fence(),
            60_000,
        )
        .await
        .expect("different-boot reclaim");
    assert!(
        matches!(
            different_boot_result,
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "same-host process metadata from a different boot is not proof of death"
    );
    release_session_execution_lease_for_test(&store, &different_boot_holder).await;

    let opaque_holder = store
        .try_claim_session_execution_lease("reclaim-opaque", &lease_owner("opaque-holder"), 60_000)
        .await
        .expect("claim opaque holder")
        .acquired()
        .expect("opaque holder acquired");
    let opaque_result = store
        .reclaim_session_execution_lease(
            "reclaim-opaque",
            &claimant,
            &opaque_holder.fence(),
            60_000,
        )
        .await
        .expect("opaque reclaim");
    assert!(
        matches!(
            opaque_result,
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "opaque holders fall back to ttl rather than fast reclaim"
    );
    release_session_execution_lease_for_test(&store, &opaque_holder).await;

    if let Some(liveness) = crate::LeaseOwnerLiveness::current_local_process("live-host") {
        let crate::LeaseOwnerLiveness::LocalProcess {
            host_id, boot_id, ..
        } = &liveness
        else {
            unreachable!("current local-process liveness is either local or absent");
        };
        let host_id = host_id.clone();
        let boot_id = boot_id.clone();
        let live_holder_owner = crate::LeaseOwnerIdentity {
            owner_id: "live-holder".to_string(),
            incarnation_id: "live-holder:incarnation".to_string(),
            liveness,
        };
        let live_claimant = local_lease_owner(
            "live-claimant",
            "live-claimant:incarnation",
            &host_id,
            &boot_id,
            pid,
            "live-claimant-start",
        );
        let live_holder = store
            .try_claim_session_execution_lease("reclaim-live", &live_holder_owner, 60_000)
            .await
            .expect("claim live holder")
            .acquired()
            .expect("live holder acquired");
        let live_result = store
            .reclaim_session_execution_lease(
                "reclaim-live",
                &live_claimant,
                &live_holder.fence(),
                60_000,
            )
            .await
            .expect("live reclaim");
        assert!(
            matches!(
                live_result,
                crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
            ),
            "a live local process holder remains busy before ttl"
        );
        release_session_execution_lease_for_test(&store, &live_holder).await;
    }
}

async fn active_path_read_scope_selects_only_requested_ancestry(
    store: Arc<dyn RuntimePersistence>,
) {
    let graph = crate::SessionGraph::from_nodes(
        vec![
            sample_session_node("root-node", None),
            sample_session_node("left-node", Some("root-node")),
            sample_session_node("left-leaf", Some("left-node")),
            sample_session_node("right-leaf", Some("root-node")),
        ],
        Some("left-leaf".to_string()),
    );
    let state = RuntimeSessionState {
        session_id: "branchy".to_string(),
        session_graph: graph,
        graph_replace_required: true,
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&state, &[]),
        "active-path",
    )
    .await
    .expect("commit branchy graph");

    let full = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load full graph")
        .expect("full graph exists");
    assert_eq!(
        full.graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["root-node", "left-node", "left-leaf", "right-leaf"],
        "FullGraph must retain every non-tombstoned branch"
    );

    let persisted_leaf_path = store
        .load_session(SessionReadScope::ActivePath { leaf_node_id: None })
        .await
        .expect("load persisted active path")
        .expect("active path exists");
    assert_eq!(
        persisted_leaf_path
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["root-node", "left-node", "left-leaf"],
        "ActivePath with no explicit leaf must use the persisted leaf and hide sibling branches"
    );
    assert_eq!(
        persisted_leaf_path.graph.leaf_node_id.as_deref(),
        Some("left-leaf")
    );

    let explicit_right_path = store
        .load_session(SessionReadScope::ActivePath {
            leaf_node_id: Some("right-leaf".to_string()),
        })
        .await
        .expect("load explicit active path")
        .expect("explicit active path exists");
    assert_eq!(
        explicit_right_path
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["root-node", "right-leaf"],
        "ActivePath with an explicit leaf must select that ancestry, not the persisted leaf"
    );
    assert_eq!(
        explicit_right_path.graph.leaf_node_id.as_deref(),
        Some("right-leaf")
    );
}

async fn attachment_manifest_records_intent_and_commit_stamps(store: Arc<dyn RuntimePersistence>) {
    let committed_by_runtime = AttachmentId::new("runtime-commit".to_string());
    let committed_out_of_band = AttachmentId::new("manual-commit".to_string());
    let orphan = AttachmentId::new("orphan".to_string());
    for id in [&committed_by_runtime, &committed_out_of_band, &orphan] {
        store
            .record_intent(attachment_intent(id.as_str()))
            .expect("record attachment intent");
    }

    let mut uncommitted = store
        .list_uncommitted(200)
        .expect("list uncommitted attachment intents");
    uncommitted.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
    assert_eq!(uncommitted.len(), 3);

    store
        .commit_refs("root", std::slice::from_ref(&committed_out_of_band))
        .expect("commit attachment ref out of band");
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&state, &[])
            .with_committed_attachments([committed_by_runtime.clone()]),
        "attachment-manifest",
    )
    .await
    .expect("runtime commit stamps attachment manifest");

    let still_uncommitted = store
        .list_uncommitted(200)
        .expect("list remaining uncommitted attachments");
    assert_eq!(still_uncommitted.len(), 1);
    assert_eq!(still_uncommitted[0].attachment_id, orphan);
    assert!(still_uncommitted[0].committed_at_epoch_ms.is_none());

    store
        .forget("root", &orphan)
        .expect("forget orphan attachment");
    assert!(
        store
            .list_uncommitted(200)
            .expect("list after forget")
            .is_empty()
    );
}

async fn attachment_manifest_keeps_same_content_ownership_per_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let attachment = AttachmentId::new("same-content");
    for session_id in ["committed-owner", "orphan-owner"] {
        store
            .record_intent(AttachmentIntent {
                attachment_id: attachment.clone(),
                session_id: session_id.to_string(),
                canonical_uri: format!("session:{session_id}:sha256:{attachment}"),
                intent_at_epoch_ms: 100,
                owner_kind: None,
                owner_id: None,
            })
            .expect("record independent owner intent");
    }
    store
        .commit_refs("committed-owner", std::slice::from_ref(&attachment))
        .expect("commit first owner");

    let uncommitted = store.list_uncommitted(200).expect("list owner orphan");
    assert!(
        uncommitted.iter().any(|entry| {
            entry.session_id == "orphan-owner" && entry.attachment_id == attachment
        })
    );
    assert!(!uncommitted.iter().any(|entry| {
        entry.session_id == "committed-owner" && entry.attachment_id == attachment
    }));

    store
        .forget("orphan-owner", &attachment)
        .expect("forget only orphan owner");
    store
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: "committed-owner".to_string(),
            canonical_uri: format!("session:committed-owner:sha256:{attachment}"),
            intent_at_epoch_ms: 150,
            owner_kind: None,
            owner_id: None,
        })
        .expect("repeat committed owner intent");
    assert!(
        !store
            .list_uncommitted(200)
            .expect("committed ownership remains stamped")
            .iter()
            .any(|entry| entry.session_id == "committed-owner"
                && entry.attachment_id == attachment),
        "a colliding owner or repeated put must not erase another session's commit stamp"
    );
}

async fn noop_attachment_manifest_is_explicit_and_empty(store: Arc<dyn RuntimePersistence>) {
    let attachment = AttachmentId::new("noop".to_string());
    store
        .record_intent(attachment_intent(attachment.as_str()))
        .expect("noop record intent succeeds");
    store
        .commit_refs("root", std::slice::from_ref(&attachment))
        .expect("noop commit refs succeeds");
    assert!(
        store
            .list_uncommitted(200)
            .expect("noop list uncommitted")
            .is_empty(),
        "declared no-op attachment manifests must not retain intent rows"
    );
    store
        .forget("noop-session", &attachment)
        .expect("noop forget succeeds");
}

async fn queued_work_source_keys_are_idempotent_and_list_ordered(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "first",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("enqueue first batch");
    let replay = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "different replay payload",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("replay first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            "root",
            "second",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue second batch");
    store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue other session");

    assert_eq!(
        first.batch_id, replay.batch_id,
        "replaying a source key must return the original batch"
    );
    assert_eq!(first.items[0].item_id, replay.items[0].item_id);
    assert_eq!(
        queued_batch_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_queued_work("root")
        .await
        .expect("list queued work");
    assert_eq!(
        listed
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
}

async fn concurrent_queue_and_turn_input_claims_have_one_owner(store: Arc<dyn RuntimePersistence>) {
    let session_id = "concurrent-claim-races";
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "single-owner queue batch",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue queue batch for claim race");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "single-owner turn input",
        ))
        .await
        .expect("enqueue turn input for claim race");
    let lease =
        claim_session_execution_lease_for_test(&store, session_id, "claim-race-lease").await;

    let queue_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&queue_barrier);
    let right_barrier = Arc::clone(&queue_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_queue = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_ready_queued_work(
                session_id,
                &left_fence,
                &lease_owner("queue-left"),
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
    });
    let right_queue = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_ready_queued_work(
                session_id,
                &right_fence,
                &lease_owner("queue-right"),
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
    });
    queue_barrier.wait().await;
    let left_queue = left_queue
        .await
        .expect("join left queue claimant")
        .expect("left queue claim race resolves cleanly");
    let right_queue = right_queue
        .await
        .expect("join right queue claimant")
        .expect("right queue claim race resolves cleanly");
    let queue_winners = [left_queue.as_ref(), right_queue.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        queue_winners.len(),
        1,
        "exactly one owner may claim the same queue batch"
    );
    assert_eq!(queue_winners[0].batches[0].batch_id, batch.batch_id);
    assert!(
        store
            .list_pending_queued_work(session_id)
            .await
            .expect("list queue after claim race")
            .is_empty(),
        "the winning queue claim must exclusively hide its batch"
    );

    let input_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&input_barrier);
    let right_barrier = Arc::clone(&input_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_input = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_next_turn_inputs(session_id, &left_fence, &lease_owner("input-left"), 1)
            .await
    });
    let right_input = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_next_turn_inputs(session_id, &right_fence, &lease_owner("input-right"), 1)
            .await
    });
    input_barrier.wait().await;
    let left_input = left_input
        .await
        .expect("join left turn-input claimant")
        .expect("left turn-input claim race resolves cleanly");
    let right_input = right_input
        .await
        .expect("join right turn-input claimant")
        .expect("right turn-input claim race resolves cleanly");
    let input_winners = [left_input.as_ref(), right_input.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        input_winners.len(),
        1,
        "exactly one owner may claim the same turn input"
    );
    assert_eq!(input_winners[0].inputs[0].input_id, input.input_id);
    assert_ne!(
        queue_winners[0].owner.owner_id, input_winners[0].owner.owner_id,
        "the queue and turn-input races use independent logical owners"
    );

    release_session_execution_lease_for_test(&store, &lease).await;
}

async fn queued_work_cancel_removes_only_unclaimed_batches(store: Arc<dyn RuntimePersistence>) {
    let cancellable = store
        .enqueue_queued_work(queued_draft(
            "root",
            "cancel me",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue cancellable batch");
    let cancelled = store
        .cancel_queued_work_batch("root", &cancellable.batch_id)
        .await
        .expect("cancel unclaimed batch")
        .expect("unclaimed batch is returned");
    assert_eq!(cancelled.batch_id, cancellable.batch_id);
    assert_eq!(queued_batch_text(&cancelled), Some("cancel me"));
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after cancellation")
            .is_empty(),
        "cancelled batches must be removed from the durable queue"
    );

    let claimed = store
        .enqueue_queued_work(queued_draft(
            "root",
            "claimed",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue claimed batch");
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim batch")
        .expect("claim exists");
    assert_eq!(claim.batches[0].batch_id, claimed.batch_id);
    // The session lease stays live here: a claim is live for lease-less host
    // callers exactly while the generation it pins still holds the session lease
    // (ADR 0029), so the hiding/cancel guards below must observe a live lease.
    assert!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending during active claim")
            .is_empty(),
        "active claims must disappear from user-editable queue snapshots"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("raw durable list during active claim")
            .len(),
        1,
        "claimed batches remain durable until their claim is completed"
    );
    assert!(
        store
            .cancel_queued_work_batch("root", &claimed.batch_id)
            .await
            .expect("cancel active claim")
            .is_none(),
        "actively claimed batches must not be cancelled"
    );
    store
        .abandon_queued_work_claim(&claim)
        .await
        .expect("abandon claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending after abandoned claim")
            .len(),
        1,
        "abandoned claims become user-editable queue work again"
    );
    assert!(
        store
            .cancel_queued_work_batch("root", &claimed.batch_id)
            .await
            .expect("cancel abandoned claim")
            .is_some(),
        "abandoned batches become cancellable again"
    );
}

async fn queued_work_exact_claim_uses_selected_batch_ids(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(queued_draft(
            "root",
            "first",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            "root",
            "second",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue second batch");

    let selected_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner").await;
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                "root",
                &selected_session_lease.fence(),
                &lease_owner("owner"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                std::slice::from_ref(&second.batch_id),
            )
            .await
            .expect("boundary-gated exact claim")
            .is_none(),
        "exact selection must preserve the delivery boundary gate"
    );
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                "root",
                &selected_session_lease.fence(),
                &lease_owner("owner"),
                QueuedWorkClaimBoundary::Idle,
                &[first.batch_id.clone(), second.batch_id.clone()],
            )
            .await
            .expect("slot-policy-gated exact claim")
            .is_none(),
        "exact selection must not combine exclusive batches into one claim"
    );
    let selected = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
        )
        .await
        .expect("claim out-of-order exact batch")
        .expect("selected exact batch exists");
    assert_eq!(selected.batches[0].batch_id, second.batch_id);
    assert_eq!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list after out-of-order exact claim")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(selected_session_lease.fence())
                .releasing_session_execution_lease(selected_session_lease.completion())
                .completing_queue_claim(selected.completion()),
        )
        .await
        .expect("complete out-of-order exact batch");

    let accepted_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner").await;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
        )
        .await
        .expect("claim first exact batch")
        .expect("first exact claim exists");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    // Both selected batches are hidden now: `second` was atomically completed
    // above, while `first` remains held by this live claim.
    assert!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending after exact claim")
            .is_empty()
    );
    release_session_execution_lease_for_test(&store, &accepted_session_lease).await;
}

async fn queued_work_classes_gate_command_and_turn_claims(store: Arc<dyn RuntimePersistence>) {
    let command = store
        .enqueue_queued_work(queued_session_command_draft("root", "refresh before turn"))
        .await
        .expect("enqueue command");
    let turn = store
        .enqueue_queued_work(queued_draft(
            "root",
            "user turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue turn");

    let rejected_turn_lease =
        claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &rejected_turn_lease.fence(),
                &lease_owner("turn-owner"),
                QueuedWorkClaimBoundary::Idle,
                10,
            )
            .await
            .expect("turn claim with leading command")
            .is_none(),
        "turn claims must not skip a leading session command"
    );
    release_session_execution_lease_for_test(&store, &rejected_turn_lease).await;

    let command_lease =
        claim_session_execution_lease_for_test(&store, "root", "command-owner").await;
    let command_claim = store
        .claim_leading_ready_session_command(
            "root",
            &command_lease.fence(),
            &lease_owner("command-owner"),
        )
        .await
        .expect("claim leading command")
        .expect("leading command claim exists");
    assert_eq!(
        command_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(command_lease.fence())
                .releasing_session_execution_lease(command_lease.completion())
                .completing_queue_claim(command_claim.completion()),
        )
        .await
        .expect("complete command claim");

    let selected_turn_lease =
        claim_session_execution_lease_for_test(&store, "root", "turn-owner").await;
    let selected_turn = store
        .claim_ready_queued_work_by_batch_ids(
            "root",
            &selected_turn_lease.fence(),
            &lease_owner("turn-owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&turn.batch_id),
        )
        .await
        .expect("claim selected turn after command")
        .expect("selected turn claim exists");
    release_session_execution_lease_for_test(&store, &selected_turn_lease).await;
    assert_eq!(selected_turn.batches[0].batch_id, turn.batch_id);

    let first_turn = store
        .enqueue_queued_work(queued_draft(
            "turn-first",
            "first turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue first turn");
    let second_command = store
        .enqueue_queued_work(queued_session_command_draft("turn-first", "later refresh"))
        .await
        .expect("enqueue later command");
    let rejected_command_lease =
        claim_session_execution_lease_for_test(&store, "turn-first", "command-owner").await;
    assert!(
        store
            .claim_leading_ready_session_command(
                "turn-first",
                &rejected_command_lease.fence(),
                &lease_owner("command-owner"),
            )
            .await
            .expect("claim command behind turn")
            .is_none(),
        "session commands must not jump ahead of earlier turn work"
    );
    let turn_claim = store
        .claim_ready_queued_work(
            "turn-first",
            &rejected_command_lease.fence(),
            &lease_owner("command-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim turn before later command")
        .expect("turn claim exists");
    assert_eq!(turn_claim.batches[0].batch_id, first_turn.batch_id);
    store
        .abandon_queued_work_claim(&turn_claim)
        .await
        .expect("abandon turn claim");
    release_session_execution_lease_for_test(&store, &rejected_command_lease).await;
    assert_eq!(
        store
            .list_queued_work("turn-first")
            .await
            .expect("list turn-first queue")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            first_turn.batch_id.as_str(),
            second_command.batch_id.as_str()
        ]
    );
}

async fn queued_work_claims_respect_boundaries_abandon_and_stale_completion(
    store: Arc<dyn RuntimePersistence>,
) {
    let after_commit = store
        .enqueue_queued_work(queued_draft(
            "root",
            "after current commit",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue after-commit work");
    let earliest = store
        .enqueue_queued_work(queued_draft(
            "root",
            "earliest",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue earliest work");

    // A single live session lease governs the whole flow: a queued-work claim
    // blocks the checkpoint boundary only while its own generation still holds
    // the session lease (ADR 0029).
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &session_lease.fence(),
                &lease_owner("owner-a"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                10,
            )
            .await
            .expect("checkpoint claim")
            .is_none(),
        "after-current-commit work at the queue head must wait for the idle boundary"
    );

    let idle_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("idle claim")
        .expect("idle claim exists");
    assert_eq!(idle_claim.batches.len(), 1);
    assert_eq!(idle_claim.batches[0].batch_id, after_commit.batch_id);

    // With the after-commit head held by this generation's own live claim, the
    // checkpoint boundary skips past it to the earliest-safe-boundary batch.
    let checkpoint_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            10,
        )
        .await
        .expect("checkpoint claim after head is leased")
        .expect("checkpoint claim exists");
    assert_eq!(checkpoint_claim.batches[0].batch_id, earliest.batch_id);

    // Abandoning the idle claim frees the after-commit batch; reclaiming it under
    // the same live lease advances the fencing token.
    store
        .abandon_queued_work_claim(&idle_claim)
        .await
        .expect("abandon idle claim");
    let reclaimed = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("reclaim abandoned work")
        .expect("reclaimed work exists");
    assert_eq!(reclaimed.batches[0].batch_id, after_commit.batch_id);
    assert!(
        reclaimed.fencing_token > idle_claim.fencing_token,
        "reclaiming abandoned work must advance the fencing token"
    );
    release_session_execution_lease_for_test(&store, &session_lease).await;

    // The pre-abandon claim's completion no longer owns any row: the reclaim
    // rewrote the batch's claim id + lease token, so committing the stale
    // completion is rejected as superseded (ADR 0029 keeps completion validation
    // by claim id + lease token; the abandon+reclaim is what supersedes it).
    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_err = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&stale_state, &[])
            .completing_queue_claim(idle_claim.completion()),
        "owner-d",
    )
    .await
    .expect_err("stale pre-abandon completion must be rejected");
    assert!(
        matches!(stale_err, StoreError::QueuedWorkClaimSuperseded { .. }),
        "stale completion produced the wrong error: {stale_err:?}"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("rejected stale completion preserves queued work")
            .len(),
        2,
        "rejected stale completion must not delete the reclaimed batch"
    );
}

pub async fn queued_work_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "generation work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue generation work");

    // (a) Same generation: a live claim cannot re-claim its own row. The
    // caller's validated-live fence generation matches the row's pinned
    // generation, so self-steal is unrepresentable (ADR 0029).
    let lease_a = claim_session_execution_lease_for_test(&store, "root", "gen-owner-a").await;
    let claim_a = store
        .claim_ready_queued_work(
            "root",
            &lease_a.fence(),
            &lease_owner("gen-owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("first-generation claim")
        .expect("first-generation claim exists");
    assert_eq!(claim_a.batches[0].batch_id, batch.batch_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_ready_queued_work(
                "root",
                &lease_a.fence(),
                &lease_owner("gen-owner-a"),
                QueuedWorkClaimBoundary::Idle,
                10,
            )
            .await
            .expect("same-generation re-claim")
            .is_none(),
        "a live claim must not be re-claimable under its own session-lease generation"
    );

    // (b) Release + re-acquire mints a new generation; the batch is claimable by
    // it and the old generation's completion is superseded.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b = claim_session_execution_lease_for_test(&store, "root", "gen-owner-b").await;
    assert!(
        lease_b.fencing_token > lease_a.fencing_token,
        "re-acquisition must mint a fresh generation"
    );
    let claim_b = store
        .claim_ready_queued_work(
            "root",
            &lease_b.fence(),
            &lease_owner("gen-owner-b"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("next-generation reclaim")
        .expect("next-generation reclaim exists");
    assert_eq!(claim_b.batches[0].batch_id, batch.batch_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(lease_b.fence())
                .completing_queue_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded-generation completion must fail");
    assert!(matches!(
        stale_err,
        StoreError::QueuedWorkClaimSuperseded { .. }
    ));
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) A dead-owner takeover mints a new generation without any release, so
    // the pre-takeover claim is superseded the same way.
    let pid = std::process::id();
    let dead_owner = local_lease_owner(
        "gen-dead",
        "gen-dead:incarnation",
        "host-a",
        "boot-a",
        pid,
        "not-the-current-process-start",
    );
    let dead_lease = store
        .try_claim_session_execution_lease("root", &dead_owner, 60_000)
        .await
        .expect("claim dead-owner lease")
        .acquired()
        .expect("dead-owner lease acquired");
    let claim_dead = store
        .claim_ready_queued_work(
            "root",
            &dead_lease.fence(),
            &dead_owner,
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("dead-owner claim")
        .expect("dead-owner claim exists");
    let taker = local_lease_owner(
        "gen-taker",
        "gen-taker:incarnation",
        "host-a",
        "boot-a",
        pid,
        "gen-taker-start",
    );
    let taker_lease = store
        .reclaim_session_execution_lease("root", &taker, &dead_lease.fence(), 60_000)
        .await
        .expect("reclaim dead owner")
        .acquired()
        .expect("dead owner is reclaimable");
    assert!(taker_lease.fencing_token > dead_lease.fencing_token);
    let claim_taker = store
        .claim_ready_queued_work(
            "root",
            &taker_lease.fence(),
            &taker,
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("post-takeover claim")
        .expect("post-takeover claim exists");
    assert_eq!(claim_taker.batches[0].batch_id, batch.batch_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(taker_lease.fence())
                .completing_queue_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::QueuedWorkClaimSuperseded { .. }
    ));
}

async fn claim_both_generation_fenced_lanes(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> (
    QueuedWorkBatch,
    crate::PendingTurnInput,
    crate::SessionExecutionLease,
    crate::QueuedWorkClaim,
    crate::TurnInputClaim,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "lease-less liveness work",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue generation-fenced queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "lease-less liveness input",
        ))
        .await
        .expect("enqueue generation-fenced turn input");
    let lease = store
        .try_claim_session_execution_lease(session_id, owner, lease_ttl_ms)
        .await
        .expect("claim session lease for both claim lanes")
        .acquired()
        .expect("session lease for both claim lanes is free");
    let queue_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            owner,
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim generation-fenced queued work")
        .expect("generation-fenced queued work claim exists");
    let input_claim = store
        .claim_next_turn_inputs(session_id, &lease.fence(), owner, 1)
        .await
        .expect("claim generation-fenced turn input")
        .expect("generation-fenced turn input claim exists");
    (batch, input, lease, queue_claim, input_claim)
}

async fn assert_both_retained_claims_are_visible_and_cancellable(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    batch: &QueuedWorkBatch,
    input: &crate::PendingTurnInput,
) {
    assert_eq!(
        store
            .list_pending_queued_work(session_id)
            .await
            .expect("list queued work after claim generation stopped being live")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![batch.batch_id.as_str()],
        "a queued-work claim whose generation is no longer live must be visible"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(session_id)
            .await
            .expect("list turn inputs after claim generation stopped being live")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "a turn-input claim whose generation is no longer live must be visible"
    );
    let cancelled_batch = store
        .cancel_queued_work_batch(session_id, &batch.batch_id)
        .await
        .expect("cancel queued work after claim generation stopped being live")
        .expect("queued work with a non-live claim generation is cancellable");
    assert_eq!(cancelled_batch.batch_id, batch.batch_id);
    let cancelled_input = store
        .cancel_pending_turn_input(session_id, &input.input_id)
        .await
        .expect("cancel turn input after claim generation stopped being live");
    expect_cancelled_pending_input(cancelled_input, &input.input_id);
}

async fn claim_liveness_for_lease_less_paths_tracks_session_generations(
    store: Arc<dyn RuntimePersistence>,
) {
    // Release: retain both claim rows, then clear the lease token without
    // abandoning either claim. Lease-less paths must immediately treat both
    // rows as pending again.
    let release_owner = lease_owner("lease-less-release-owner");
    let (batch, input, lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(&store, "lease-less-release", &release_owner, 60_000)
            .await;
    release_session_execution_lease_for_test(&store, &lease).await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-release",
        &batch,
        &input,
    )
    .await;

    // Expiry: the lease row still carries the generation, but its token is no
    // longer live once the TTL elapses. The correlated SQL predicates must not
    // mistake generation equality alone for a live claim.
    let expiry_owner = lease_owner("lease-less-expiry-owner");
    let (batch, input, _lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(&store, "lease-less-expiry", &expiry_owner, 1_000).await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-expiry",
        &batch,
        &input,
    )
    .await;

    // Dead-owner takeover: the lease stays live but advances to a different
    // generation. Claims retained from the dead generation are no longer live
    // for lease-less callers even before a successor reclaims the rows.
    let pid = std::process::id();
    let dead_owner = local_lease_owner(
        "lease-less-dead",
        "lease-less-dead:incarnation",
        "host-a",
        "boot-a",
        pid,
        "not-the-current-process-start",
    );
    let (batch, input, dead_lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(&store, "lease-less-takeover", &dead_owner, 60_000)
            .await;
    let taker = local_lease_owner(
        "lease-less-taker",
        "lease-less-taker:incarnation",
        "host-a",
        "boot-a",
        pid,
        "lease-less-taker-start",
    );
    let taker_lease = store
        .reclaim_session_execution_lease("lease-less-takeover", &taker, &dead_lease.fence(), 60_000)
        .await
        .expect("take over lease for lease-less liveness checks")
        .acquired()
        .expect("dead owner is reclaimable for lease-less liveness checks");
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        "lease-less-takeover",
        &batch,
        &input,
    )
    .await;
    release_session_execution_lease_for_test(&store, &taker_lease).await;
}

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
                    queue_session,
                    &format!("bounded queue {index}"),
                    DeliveryPolicy::EarliestSafeBoundary,
                    SlotPolicy::Exclusive,
                ))
                .await
                .expect("enqueue bounded-scan queued work"),
        );
    }
    let queue_lease = store
        .try_claim_session_execution_lease(queue_session, &queue_owner, 60_000)
        .await
        .expect("claim bounded-scan queue session lease")
        .acquired()
        .expect("bounded-scan queue session lease is free");
    for expected in &queue_batches {
        let claim = store
            .claim_ready_queued_work(
                queue_session,
                &queue_lease.fence(),
                &queue_owner,
                QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
            .expect("claim bounded-scan queued work")
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
                    command_session,
                    &format!("bounded command {index}"),
                ))
                .await
                .expect("enqueue bounded-scan session command"),
        );
    }
    let command_lease = store
        .try_claim_session_execution_lease(command_session, &command_owner, 60_000)
        .await
        .expect("claim bounded-scan command session lease")
        .acquired()
        .expect("bounded-scan command session lease is free");
    for expected in &command_batches {
        let claim = store
            .claim_leading_ready_session_command(
                command_session,
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
                    input_session,
                    &format!("bounded turn input {index}"),
                ))
                .await
                .expect("enqueue bounded-scan turn input"),
        );
    }
    let input_lease = store
        .try_claim_session_execution_lease(input_session, &input_owner, 60_000)
        .await
        .expect("claim bounded-scan turn-input session lease")
        .acquired()
        .expect("bounded-scan turn-input session lease is free");
    for expected in &inputs {
        let claim = store
            .claim_next_turn_inputs(input_session, &input_lease.fence(), &input_owner, 1)
            .await
            .expect("claim bounded-scan turn input")
            .expect("bounded-scan turn input remains reachable");
        assert_eq!(claim.inputs[0].input_id, expected.input_id);
    }
    release_session_execution_lease_for_test(&store, &input_lease).await;
}

async fn queued_work_respects_membership_limits_exclusivity_reclaim_and_sessions(
    store: Arc<dyn RuntimePersistence>,
) {
    store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "not ready",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
            )
            .with_available_at_ms(4_102_444_800_000),
        )
        .await
        .expect("enqueue unavailable work");
    let exclusive = store
        .enqueue_queued_work(queued_draft(
            "root",
            "exclusive",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue exclusive work");
    let joined = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "joined",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("root".to_string())),
        )
        .await
        .expect("enqueue joined work");
    let other = store
        .enqueue_queued_work(queued_draft(
            "other",
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue other session work");

    // Both root claims run under one live session lease: advancing through the
    // queue relies on each claimed batch staying held by the current generation,
    // so a same-generation follow-up claim skips it (ADR 0029).
    let root_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim root")
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
            "root",
            &root_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim joined")
        .expect("joined claim");
    release_session_execution_lease_for_test(&store, &root_session_lease).await;
    assert_eq!(next_root.batches[0].batch_id, joined.batch_id);
    let other_session_lease =
        claim_session_execution_lease_for_test(&store, "other", "owner-c").await;
    let other_claim = store
        .claim_ready_queued_work(
            "other",
            &other_session_lease.fence(),
            &lease_owner("owner-c"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim other")
        .expect("other claim");
    release_session_execution_lease_for_test(&store, &other_session_lease).await;
    assert_eq!(
        other_claim.batches[0].batch_id, other.batch_id,
        "claiming one session must not consume queued work from another session"
    );

    let reclaimed_source = store
        .enqueue_queued_work(queued_draft(
            "reclaim",
            "superseded claim",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue reclaim work");
    let first_generation_lease =
        claim_session_execution_lease_for_test(&store, "reclaim", "owner-a").await;
    let first_generation_claim = store
        .claim_ready_queued_work(
            "reclaim",
            &first_generation_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim under the first generation")
        .expect("first-generation claim");
    release_session_execution_lease_for_test(&store, &first_generation_lease).await;
    let reclaim_session_lease =
        claim_session_execution_lease_for_test(&store, "reclaim", "owner-b").await;
    let reclaimed = store
        .claim_ready_queued_work(
            "reclaim",
            &reclaim_session_lease.fence(),
            &lease_owner("owner-b"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("reclaim under a new generation")
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
                "limited",
                "one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited one");
    let limited_second = store
        .enqueue_queued_work(
            queued_draft(
                "limited",
                "two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited two");
    let limited_third = store
        .enqueue_queued_work(
            queued_draft(
                "limited",
                "three",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("limited".to_string())),
        )
        .await
        .expect("enqueue limited three");
    // One live lease: the capped claim keeps the first two batches held, so the
    // same-generation follow-up claim only sees the third (ADR 0029).
    let limited_session_lease =
        claim_session_execution_lease_for_test(&store, "limited", "owner").await;
    let limited = store
        .claim_ready_queued_work(
            "limited",
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            2,
        )
        .await
        .expect("limited claim")
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
            "limited",
            &limited_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("remaining claim")
        .expect("remaining claim exists");
    release_session_execution_lease_for_test(&store, &limited_session_lease).await;
    assert_eq!(remaining.batches[0].batch_id, limited_third.batch_id);
}

async fn queued_work_join_groups_by_delivery_policy_and_merge_key(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group a one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue group a one");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group a two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue group a two");
    let different_merge = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "group b",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("b".to_string())),
        )
        .await
        .expect("enqueue group b");
    let different_delivery = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "after commit",
                DeliveryPolicy::AfterCurrentTurnCommit,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("a".to_string())),
        )
        .await
        .expect("enqueue after-commit");

    // All three group claims run under one live session lease so each claimed
    // group stays held by the current generation and the next same-generation
    // claim advances to the following group (ADR 0029).
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let first_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim first group")
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
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim second group")
        .expect("second group claim");
    assert_eq!(second_claim.batches[0].batch_id, different_merge.batch_id);
    let third_claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim third group")
        .expect("third group claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(third_claim.batches[0].batch_id, different_delivery.batch_id);
}

async fn queued_work_completion_is_lease_guarded(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "join one",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("joined".to_string())),
        )
        .await
        .expect("enqueue first joined batch");
    let second = store
        .enqueue_queued_work(
            queued_draft(
                "root",
                "join two",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
            )
            .with_merge_key(MergeKey::Group("joined".to_string())),
        )
        .await
        .expect("enqueue second joined batch");
    let claim_session_lease =
        claim_session_execution_lease_for_test(&store, "root", "owner-a").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &claim_session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim joined batches")
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
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(claim_session_lease.fence())
                .completing_queue_claim(stale_completion),
        )
        .await
        .expect_err("stale queued-work completion must fail");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("stale completion preserves queued work")
            .len(),
        2
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(claim_session_lease.fence())
                .releasing_session_execution_lease(claim_session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("valid queued-work completion commits");
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("valid completion clears queued work")
            .is_empty()
    );
}

async fn queue_completion_and_turn_commit_stamp_are_atomic(store: Arc<dyn RuntimePersistence>) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            "root",
            "atomic queue",
            DeliveryPolicy::EarliestSafeBoundary,
            SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue queue batch");
    let session_lease = claim_session_execution_lease_for_test(&store, "root", "queue-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim queue")
        .expect("queue claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "atomic pending input",
        ))
        .await
        .expect("enqueue atomic pending input");
    let input_claim = store
        .claim_next_turn_inputs(
            "root",
            &session_lease.fence(),
            &lease_owner("queue-owner"),
            1,
        )
        .await
        .expect("claim atomic pending input")
        .expect("atomic pending input claim");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 41,
        ..RuntimeSessionState::default()
    };
    let mut base_commit = RuntimeCommit::persisted_state(&state, &[]);
    base_commit.enqueued_queue_batches = vec![
        QueuedWorkBatchDraft::new(
            "root",
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
            vec![QueuedWorkPayload::agent_frame_task(
                "follow-frame",
                "follow-on task",
                None,
            )],
        )
        .with_source_key("agent-frame-handoff:turn-atomic"),
    ];
    let commit_hash = base_commit.turn_commit_hash().expect("turn commit hash");
    let turn_commit = RuntimeTurnCommitStamp::new(
        "root",
        crate::OperationId::turn("root", "turn-atomic", "final"),
        commit_hash.clone(),
    );
    let mut stale_queue_completion = claim.completion();
    stale_queue_completion.lease_token.push_str(":stale");
    let err = store
        .commit_runtime_state(
            base_commit
                .clone()
                .with_session_execution_lease(session_lease.fence())
                .with_turn_commit(turn_commit.clone())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(stale_queue_completion),
        )
        .await
        .expect_err("stale queue completion must reject the whole final commit");
    assert!(matches!(err, StoreError::QueuedWorkClaimSuperseded { .. }));
    assert!(
        store
            .load_session(SessionReadScope::FullGraph)
            .await
            .expect("load after rejected atomic commit")
            .is_none(),
        "rejected queue completion must not persist session state"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("list after rejected atomic commit")
            .len(),
        1,
        "rejected queue completion must preserve queued work"
    );

    let mut cross_session_outbox = base_commit.clone();
    cross_session_outbox.enqueued_queue_batches[0].session_id = "other-session".to_string();
    let err = store
        .commit_runtime_state(
            cross_session_outbox
                .with_session_execution_lease(session_lease.fence())
                .completing_queue_claim(claim.completion())
                .completing_turn_input_claim(input_claim.completion()),
        )
        .await
        .expect_err("outbox enqueue failure must reject the whole final commit");
    assert!(matches!(err, StoreError::SessionBindingMismatch { .. }));
    assert!(
        store
            .load_session(SessionReadScope::FullGraph)
            .await
            .expect("load after rejected outbox enqueue")
            .is_none(),
        "rejected outbox enqueue must roll back session state"
    );
    assert_eq!(
        store
            .list_queued_work("root")
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
                .with_session_execution_lease(session_lease.fence())
                .releasing_session_execution_lease(session_lease.completion())
                .with_turn_commit(turn_commit.clone())
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("valid final commit clears queue and records the turn stamp atomically");
    let retry = store
        .commit_runtime_state(
            base_commit
                .with_session_execution_lease(session_lease.fence())
                .releasing_session_execution_lease(session_lease.completion())
                .with_turn_commit(RuntimeTurnCommitStamp::new(
                    "root",
                    crate::OperationId::turn("root", "turn-atomic", "final"),
                    commit_hash,
                ))
                .completing_turn_input_claim(input_claim.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("same final turn commit stamp retries idempotently");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(first.enqueued_queue_batches.len(), 1);
    assert_eq!(retry.enqueued_queue_batches.len(), 1);
    assert_eq!(
        retry.enqueued_queue_batches[0].batch_id, first.enqueued_queue_batches[0].batch_id,
        "idempotent commit retry must return the original outbox identity"
    );
    assert!(
        store
            .load_session(SessionReadScope::FullGraph)
            .await
            .expect("load after accepted atomic commit")
            .is_some()
    );
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after accepted atomic commit")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .eq([first.enqueued_queue_batches[0].batch_id.as_str()])
    );
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list inputs after accepted atomic commit")
            .is_empty(),
        "accepted switch commit must complete inbound input with the outbox enqueue"
    );
}

async fn pending_turn_inputs_source_keys_order_cancel_and_cross_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("enqueue first pending input");
    let replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("replay first pending input");
    let conflict = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "different replay payload")
                .with_source_key("source:first"),
        )
        .await
        .expect_err("same source key with changed content must conflict");
    assert!(matches!(
        conflict,
        StoreError::PendingTurnInputSourceKeyConflict {
            session_id,
            source_key,
            existing_input_id,
        } if session_id == "root"
            && source_key == "source:first"
            && existing_input_id == first.input_id
    ));
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "second"))
        .await
        .expect("enqueue second pending input");
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("other", "other session"))
        .await
        .expect("enqueue other session pending input");

    assert_eq!(
        first.input_id, replay.input_id,
        "replaying a source key must return the original pending input"
    );
    assert_eq!(
        pending_input_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list pending turn inputs");
    assert_eq!(
        listed
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
    assert!(listed.iter().all(|input| input.session_id == "root"));

    let cancelled = store
        .cancel_pending_turn_input("root", &second.input_id)
        .await
        .expect("cancel pending turn input");
    expect_cancelled_pending_input(cancelled, &second.input_id);
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &second.input_id)
            .await
            .expect("cancel pending turn input replay"),
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == second.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after cancel")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str()]
    );

    let cancelled_first = store
        .cancel_pending_turn_input("root", &first.input_id)
        .await
        .expect("cancel source-keyed pending turn input");
    expect_cancelled_pending_input(cancelled_first, &first.input_id);
    let terminal_replay = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "first").with_source_key("source:first"),
        )
        .await
        .expect("exact replay after cancellation");
    assert_eq!(terminal_replay.input_id, first.input_id);
    assert_eq!(terminal_replay.state, crate::TurnInputState::Cancelled);
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after terminal replay")
            .is_empty()
    );
    let vacuum = store
        .vacuum()
        .await
        .expect("vacuum pending input tombstones");
    assert_eq!(vacuum.removed_node_count, 0);
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 2);
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &second.input_id)
            .await
            .expect("cancel pruned tombstone"),
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("other")
            .await
            .expect("list other session after tombstone vacuum")
            .len(),
        1,
        "vacuum must prune terminal evidence without removing live pending input"
    );
}

async fn pending_turn_input_bulk_and_suffix_cancellation(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "bulk first").with_source_key("bulk:first"),
        )
        .await
        .expect("enqueue first bulk input");
    let second = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "bulk second").with_source_key("bulk:second"),
        )
        .await
        .expect("enqueue second bulk input");
    let third = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "bulk third"))
        .await
        .expect("enqueue third bulk input");
    let bulk = store
        .cancel_pending_turn_inputs(
            "root",
            &[
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
                crate::PendingTurnInputCancelTarget::input_id(&third.input_id),
                crate::PendingTurnInputCancelTarget::source_key("bulk:missing"),
                crate::PendingTurnInputCancelTarget::source_key("bulk:first"),
            ],
        )
        .await
        .expect("bulk cancel pending turn inputs");
    assert_eq!(bulk.len(), 4);
    expect_cancelled_pending_input(bulk[0].outcome.clone(), &first.input_id);
    expect_cancelled_pending_input(bulk[1].outcome.clone(), &third.input_id);
    assert!(matches!(
        bulk[2].outcome,
        crate::PendingTurnInputCancelOutcome::NotFound
    ));
    assert!(matches!(
        &bulk[3].outcome,
        crate::PendingTurnInputCancelOutcome::AlreadyCancelled(input)
            if input.input_id == first.input_id
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after bulk cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );

    let suffix_anchor = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix anchor").with_source_key("suffix:anchor"),
        )
        .await
        .expect("enqueue suffix anchor");
    let active_claimed = store
        .enqueue_pending_turn_input(
            pending_active_turn_input_draft(
                "root",
                "suffix-active-turn",
                crate::TurnInputCheckpointBoundary::AfterWork,
                "suffix accepted active",
            )
            .with_source_key("suffix:claimed"),
        )
        .await
        .expect("enqueue suffix claimed input");
    let suffix_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix later").with_source_key("suffix:later"),
        )
        .await
        .expect("enqueue suffix later");
    let lease = claim_session_execution_lease_for_test(&store, "root", "suffix-cancel-owner").await;
    let active_claim = store
        .claim_active_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("suffix-cancel-owner"),
            "suffix-active-turn",
            crate::CheckpointKind::AfterWork,
            10,
        )
        .await
        .expect("claim suffix active input")
        .expect("suffix active input claim");

    let suffix = store
        .cancel_pending_turn_input_suffix(
            "root",
            &crate::PendingTurnInputCancelTarget::source_key("suffix:anchor"),
        )
        .await
        .expect("suffix cancel by source key");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("expected suffix outcomes, got {suffix:?}");
    };
    assert_eq!(outcomes.len(), 3);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_anchor.input_id);
    match &outcomes[1] {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed { input, claim } => {
            assert_eq!(input.input_id, active_claimed.input_id);
            assert_eq!(
                claim.as_ref().and_then(|claim| claim.claim_id.as_deref()),
                Some(active_claim.claim_id.as_str())
            );
        }
        other => panic!("expected already-claimed suffix outcome, got {other:?}"),
    }
    expect_cancelled_pending_input(outcomes[2].clone(), &suffix_later.input_id);

    let suffix_by_id_anchor = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "suffix by id anchor"))
        .await
        .expect("enqueue suffix by id anchor");
    let suffix_by_id_later = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft("root", "suffix by id later")
                .with_source_key("suffix:id-later"),
        )
        .await
        .expect("enqueue suffix by id later");
    let suffix_by_id = store
        .cancel_pending_turn_input_suffix(
            "root",
            &crate::PendingTurnInputCancelTarget::input_id(&suffix_by_id_anchor.input_id),
        )
        .await
        .expect("suffix cancel by input id");
    let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix_by_id else {
        panic!("expected input-id suffix outcomes, got {suffix_by_id:?}");
    };
    assert_eq!(outcomes.len(), 2);
    expect_cancelled_pending_input(outcomes[0].clone(), &suffix_by_id_anchor.input_id);
    expect_cancelled_pending_input(outcomes[1].clone(), &suffix_by_id_later.input_id);

    assert!(matches!(
        store
            .cancel_pending_turn_input_suffix(
                "root",
                &crate::PendingTurnInputCancelTarget::source_key("suffix:missing"),
            )
            .await
            .expect("missing suffix anchor"),
        crate::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. }
    ));
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after suffix cancellation")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.input_id.as_str()]
    );
}

async fn pending_turn_input_claims_reclaim_complete_and_fence(store: Arc<dyn RuntimePersistence>) {
    let first = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("enqueue first next input");
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft("root", "second next"))
        .await
        .expect("enqueue second next input");
    let lease = claim_session_execution_lease_for_test(&store, "root", "turn-input-owner").await;
    let claim = store
        .claim_next_turn_inputs("root", &lease.fence(), &lease_owner("turn-input-owner"), 10)
        .await
        .expect("claim next inputs")
        .expect("next input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    assert!(matches!(
        claim
            .materialize_for_turn()
            .items
            .iter()
            .find(|item| matches!(item, crate::InputItem::Attachment { .. })),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[1, 2, 3]
    ));
    match store
        .cancel_pending_turn_input("root", &first.input_id)
        .await
        .expect("cancel claimed input")
    {
        crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
            input,
            claim: diagnostics,
        } => {
            assert_eq!(input.input_id, first.input_id);
            assert_eq!(
                diagnostics
                    .as_ref()
                    .and_then(|diagnostics| diagnostics.claim_id.as_deref()),
                Some(claim.claim_id.as_str())
            );
        }
        other => panic!("live claimed pending input must not be cancellable, got {other:?}"),
    }
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list claimed inputs")
            .is_empty(),
        "live claimed pending inputs must be hidden from queue previews"
    );

    store
        .abandon_turn_input_claim(&claim)
        .await
        .expect("abandon pending input claim");
    assert_eq!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after abandon")
            .len(),
        2
    );
    let reclaimed = store
        .claim_next_turn_inputs("root", &lease.fence(), &lease_owner("turn-input-owner"), 10)
        .await
        .expect("reclaim next inputs")
        .expect("reclaimed next claim");
    assert!(
        reclaimed.fencing_token > claim.fencing_token,
        "reclaiming abandoned pending inputs must advance the fencing token"
    );

    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(lease.fence())
                .completing_turn_input_claim(claim.completion()),
        )
        .await
        .expect_err("stale turn-input completion must fail");
    assert!(matches!(err, StoreError::TurnInputClaimSuperseded { .. }));
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list reclaimed live inputs")
            .is_empty(),
        "stale completion must not abandon the live reclaimed claim"
    );

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(lease.fence())
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(reclaimed.completion()),
        )
        .await
        .expect("valid pending input completion commits");
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after valid completion")
            .is_empty()
    );
    assert!(matches!(
        store
            .cancel_pending_turn_input("root", &first.input_id)
            .await
            .expect("cancel completed input"),
        crate::PendingTurnInputCancelOutcome::AlreadyCompleted(input)
            if input.input_id == first.input_id
    ));
    let completed_replay = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("first next").with_attachment(inline_png(vec![1, 2, 3])),
            )
            .with_source_key("next:first"),
        )
        .await
        .expect("exact replay after completion");
    assert_eq!(completed_replay.input_id, first.input_id);
    assert_eq!(completed_replay.state, crate::TurnInputState::Completed);
    let post_completion_lease =
        claim_session_execution_lease_for_test(&store, "root", "post-completion-owner").await;
    assert!(
        store
            .claim_next_turn_inputs(
                "root",
                &post_completion_lease.fence(),
                &lease_owner("post-completion-owner"),
                10,
            )
            .await
            .expect("claim after completing inputs")
            .is_none(),
        "completed pending input tombstones must not be claimable"
    );
}

pub async fn turn_input_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
) {
    // The DeferredNextTurn idle-retry shape: a failed turn releases its lease
    // and the next idle acquisition re-claims the same next-turn input under a
    // fresh generation, while the stale claim's completion is rejected. This was
    // the latent unrenewed-claim bug (ADR 0029).
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "generation next input",
        ))
        .await
        .expect("enqueue next-turn input");

    // (a) Same generation: a live next-turn claim is not re-claimable.
    let lease_a = claim_session_execution_lease_for_test(&store, "root", "tin-owner-a").await;
    let claim_a = store
        .claim_next_turn_inputs("root", &lease_a.fence(), &lease_owner("tin-owner-a"), 10)
        .await
        .expect("first next-turn claim")
        .expect("first next-turn claim exists");
    assert_eq!(claim_a.inputs[0].input_id, input.input_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_next_turn_inputs("root", &lease_a.fence(), &lease_owner("tin-owner-a"), 10)
            .await
            .expect("same-generation re-claim")
            .is_none(),
        "a live next-turn claim must not be re-claimable under its own generation"
    );

    // (b) Idle retry after lease release + re-acquire: the same next-turn input
    // is re-claimable by the new generation and the stale completion is
    // superseded.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b = claim_session_execution_lease_for_test(&store, "root", "tin-owner-b").await;
    let claim_b = store
        .claim_next_turn_inputs("root", &lease_b.fence(), &lease_owner("tin-owner-b"), 10)
        .await
        .expect("idle-retry next-turn claim")
        .expect("idle-retry next-turn claim exists");
    assert_eq!(claim_b.inputs[0].input_id, input.input_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(lease_b.fence())
                .completing_turn_input_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded next-turn completion must fail");
    assert!(matches!(
        stale_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) Dead-owner takeover mints a new generation without a release.
    let pid = std::process::id();
    let dead_owner = local_lease_owner(
        "tin-dead",
        "tin-dead:incarnation",
        "host-a",
        "boot-a",
        pid,
        "not-the-current-process-start",
    );
    let dead_lease = store
        .try_claim_session_execution_lease("root", &dead_owner, 60_000)
        .await
        .expect("claim dead-owner lease")
        .acquired()
        .expect("dead-owner lease acquired");
    let claim_dead = store
        .claim_next_turn_inputs("root", &dead_lease.fence(), &dead_owner, 10)
        .await
        .expect("dead-owner next-turn claim")
        .expect("dead-owner next-turn claim exists");
    let taker = local_lease_owner(
        "tin-taker",
        "tin-taker:incarnation",
        "host-a",
        "boot-a",
        pid,
        "tin-taker-start",
    );
    let taker_lease = store
        .reclaim_session_execution_lease("root", &taker, &dead_lease.fence(), 60_000)
        .await
        .expect("reclaim dead owner")
        .acquired()
        .expect("dead owner is reclaimable");
    let claim_taker = store
        .claim_next_turn_inputs("root", &taker_lease.fence(), &taker, 10)
        .await
        .expect("post-takeover next-turn claim")
        .expect("post-takeover next-turn claim exists");
    assert_eq!(claim_taker.inputs[0].input_id, input.input_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&stale_state, &[])
                .with_session_execution_lease(taker_lease.fence())
                .completing_turn_input_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover next-turn completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::TurnInputClaimSuperseded { .. }
    ));
}

async fn pending_turn_input_cancel_covers_active_and_deferred_states(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "cancel-active-turn";
    let active_keep = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active that defers",
        ))
        .await
        .expect("enqueue active input to defer");
    let active_cancel = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "active cancelled before interrupt",
        ))
        .await
        .expect("enqueue active input to cancel");
    let next_cancel = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            "root",
            "next cancelled before claim",
        ))
        .await
        .expect("enqueue next input to cancel");

    let cancelled_active = store
        .cancel_pending_turn_input("root", &active_cancel.input_id)
        .await
        .expect("cancel active input");
    let cancelled_active =
        expect_cancelled_pending_input(cancelled_active, &active_cancel.input_id);
    assert!(matches!(
        cancelled_active.ingress,
        crate::TurnInputIngress::ActiveTurn { .. }
    ));
    let cancelled_next = store
        .cancel_pending_turn_input("root", &next_cancel.input_id)
        .await
        .expect("cancel next input");
    expect_cancelled_pending_input(cancelled_next, &next_cancel.input_id);

    let lease = claim_session_execution_lease_for_test(&store, "root", "cancel-input-owner").await;
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(lease.fence())
                .deferring_interrupted_turn_inputs(turn_id),
        )
        .await
        .expect("interrupt commit defers uncancelled active input");

    let pending_after_interrupt = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list after interrupt");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![active_keep.input_id.as_str()],
        "cancelled active and next-turn inputs must not be resurrected by interrupt deferral"
    );
    assert!(matches!(
        pending_after_interrupt[0].ingress,
        crate::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending_after_interrupt[0].state,
        crate::TurnInputState::DeferredNextTurn
    );

    let cancelled_deferred = store
        .cancel_pending_turn_input("root", &active_keep.input_id)
        .await
        .expect("cancel deferred input");
    expect_cancelled_pending_input(cancelled_deferred, &active_keep.input_id);
    assert!(
        store
            .claim_next_turn_inputs(
                "root",
                &lease.fence(),
                &lease_owner("cancel-input-owner"),
                10,
            )
            .await
            .expect("claim after cancelling deferred input")
            .is_none(),
        "cancelled deferred input must not be claimable"
    );
}

async fn pending_active_turn_inputs_defer_unaccepted_once_on_interrupt(
    store: Arc<dyn RuntimePersistence>,
) {
    let turn_id = "active-turn-1";
    let accepted = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                "root",
                crate::TurnInputIngress::active_turn(
                    turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("accepted active")
                    .with_attachment(inline_png(vec![9, 8, 7])),
            )
            .with_source_key("active:accepted"),
        )
        .await
        .expect("enqueue accepted active input");
    let unaccepted = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "unaccepted active",
        ))
        .await
        .expect("enqueue unaccepted active input");
    let before_completion = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            turn_id,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "before-completion active",
        ))
        .await
        .expect("enqueue before-completion active input");
    let other_active = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            "root",
            "other-turn",
            crate::TurnInputCheckpointBoundary::AfterWork,
            "other active",
        ))
        .await
        .expect("enqueue other active input");

    let lease = claim_session_execution_lease_for_test(&store, "root", "active-input-owner").await;
    let claim = store
        .claim_active_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("active-input-owner"),
            turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("claim active inputs")
        .expect("active input claim");
    assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![accepted.input_id.as_str()],
        "AfterWork claims must include matching active inputs admitted at that boundary in order"
    );
    assert!(matches!(
        claim.materialize_for_turn().items.last(),
        Some(crate::InputItem::Attachment {
            source: crate::AttachmentSource::Inline { bytes, .. }
        }) if bytes == &[9, 8, 7]
    ));

    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    let interrupt_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(lease.fence())
                .completing_turn_input_claim(claim.completion())
                .deferring_interrupted_turn_inputs(turn_id),
        )
        .await
        .expect("interrupt commit completes accepted inputs and defers unaccepted inputs");
    let mut state = state;
    state.head_revision = Some(interrupt_result.head_revision);
    let pending_after_interrupt = store
        .list_pending_turn_inputs("root")
        .await
        .expect("list after interrupt deferral");
    assert_eq!(
        pending_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str(),
            other_active.input_id.as_str(),
        ],
        "interrupt must complete accepted input, defer matching unaccepted inputs, and retain other-turn active input"
    );
    let deferred_after_interrupt = pending_after_interrupt
        .iter()
        .filter(|input| input.ingress.active_turn_id().is_none())
        .collect::<Vec<_>>();
    assert_eq!(
        deferred_after_interrupt
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ],
        "accepted active inputs must be completed and only unaccepted matching active inputs become next-turn work"
    );
    assert!(deferred_after_interrupt.iter().all(|input| {
        matches!(input.ingress, crate::TurnInputIngress::NextTurn)
            && input.state == crate::TurnInputState::DeferredNextTurn
    }));
    assert!(
        pending_after_interrupt
            .iter()
            .any(|input| input.ingress.active_turn_id() == Some("other-turn")),
        "inputs for other active turns must not be deferred by this interrupt"
    );

    let next_claim = store
        .claim_next_turn_inputs(
            "root",
            &lease.fence(),
            &lease_owner("active-input-owner"),
            10,
        )
        .await
        .expect("claim deferred next inputs")
        .expect("deferred next input claim");
    assert_eq!(
        next_claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            unaccepted.input_id.as_str(),
            before_completion.input_id.as_str()
        ]
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(lease.fence())
                .releasing_session_execution_lease(lease.completion())
                .completing_turn_input_claim(next_claim.completion()),
        )
        .await
        .expect("complete deferred next input");
    assert!(
        store
            .list_pending_turn_inputs("root")
            .await
            .expect("list after completing deferred input")
            .iter()
            .all(|input| input.ingress.active_turn_id() == Some("other-turn")),
        "inputs for other active turns must not be deferred by this interrupt"
    );
}

async fn session_metadata_round_trips(store: Arc<dyn RuntimePersistence>) {
    let meta = SessionMeta {
        session_id: "root".to_string(),
        session_name: "Conformance Root".to_string(),
        created_at: "2026-06-02T00:00:00Z".to_string(),
        model: "gpt-5.4-mini".to_string(),
        cwd: Some("/tmp/lash-conformance".to_string()),
        relation: SessionRelation::Root,
    };
    store
        .save_session_meta(meta.clone())
        .await
        .expect("save session meta");
    let loaded = store
        .load_session_meta()
        .await
        .expect("load session meta")
        .expect("session meta present");
    assert_eq!(loaded.session_id, meta.session_id);
    assert_eq!(loaded.session_name, meta.session_name);
    assert_eq!(loaded.created_at, meta.created_at);
    assert_eq!(loaded.model, meta.model);
    assert_eq!(loaded.cwd, meta.cwd);
    assert_eq!(loaded.relation, meta.relation);
}

async fn tombstone_vacuum_and_gc_are_minimally_consistent(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        session_graph: crate::SessionGraph::from_nodes(
            vec![
                sample_session_node("node-live", None),
                sample_session_node("node-delete", Some("node-live")),
            ],
            Some("node-delete".to_string()),
        ),
        graph_replace_required: true,
        ..RuntimeSessionState::default()
    };
    state.head_revision = None;
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&state, &[]),
        "tombstone",
    )
    .await
    .expect("commit graph");
    assert!(
        store
            .load_node("node-delete")
            .await
            .expect("load node before tombstone")
            .is_some()
    );
    store
        .tombstone_nodes(&["node-delete".to_string()])
        .await
        .expect("tombstone node");
    assert!(
        store
            .load_node("node-delete")
            .await
            .expect("load node after tombstone")
            .is_none(),
        "tombstoned nodes must be hidden from direct loads"
    );
    let read = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load graph after tombstone")
        .expect("session after tombstone");
    assert!(
        !read
            .graph
            .nodes
            .iter()
            .any(|node| node.node_id == "node-delete"),
        "tombstoned nodes must be hidden from session graph loads"
    );
    let vacuum = store.vacuum().await.expect("vacuum");
    assert!(
        vacuum.removed_node_count <= 1,
        "vacuum must report only rows removed by this call, got {vacuum:?}"
    );
    store
        .gc_unreachable()
        .await
        .expect("gc_unreachable should be safe to call");
}

/// Blob-backed backends must physically reclaim the checkpoint blob a superseding
/// commit orphaned, while preserving the live one. Generalizes the SQLite-only
/// `gc_unreachable_keeps_rooted_checkpoint_blobs` test to every reclaiming
/// backend via the [`GcReport`](crate::GcReport) counters plus a post-GC load.
async fn gc_reclaims_unreachable_checkpoint_blobs_and_preserves_live(
    store: Arc<dyn RuntimePersistence>,
) {
    // First commit writes a live checkpoint blob.
    let v1 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(1)),
        ..RuntimeSessionState::default()
    };
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&v1, &[]),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let v2 = RuntimeSessionState {
        session_id: "gc-blobs".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(2)),
        head_revision: Some(v1_result.head_revision),
        ..RuntimeSessionState::default()
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state(&v2, &[]),
        "gc-blobs-v2",
    )
    .await
    .expect("commit v2");

    let report = store
        .gc_unreachable()
        .await
        .expect("gc reclaims unreachable checkpoint blobs");
    assert!(
        report.root_count >= 1,
        "a live checkpoint must be rooted, got {report:?}"
    );
    assert!(
        report.retained_blob_count >= 1,
        "the live checkpoint blob must be retained, got {report:?}"
    );
    assert!(
        report.deleted_blob_count >= 1,
        "the superseded checkpoint blob must be reclaimed, got {report:?}"
    );

    // The reachable checkpoint survived: the session still loads at generation 2.
    let read = store
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load after gc")
        .expect("session after gc");
    assert_eq!(
        read.checkpoint
            .and_then(|checkpoint| checkpoint.tool_state)
            .map(|tool_state| tool_state.generation()),
        Some(2),
        "gc must preserve the reachable checkpoint's snapshots"
    );

    // Idempotent: with nothing newly unreachable, a second sweep deletes nothing.
    let second = store.gc_unreachable().await.expect("second gc");
    assert_eq!(
        second.deleted_blob_count, 0,
        "gc must never reclaim reachable blobs, got {second:?}"
    );
}

/// The durable manifest is the reference layer: it answers `holds_ref` for the
/// session-boundary guard, exposes every live ref (intent or committed) for the
/// GC root set, and drops refs on `forget`. Both intents and commits count as
/// live refs; a forgotten ref disappears from both queries.
async fn attachment_manifest_reference_tracking_and_gc_root_set(
    store: Arc<dyn RuntimePersistence>,
) {
    let intent_id = AttachmentId::new(format!("{:x}", sha256_of(b"intent-only")));
    let committed_id = AttachmentId::new(format!("{:x}", sha256_of(b"committed")));
    let intent = |id: &AttachmentId, at: u64| AttachmentIntent {
        attachment_id: id.clone(),
        session_id: "root".to_string(),
        canonical_uri: format!("lash-attachment://sha256/{id}"),
        intent_at_epoch_ms: at,
        owner_kind: None,
        owner_id: None,
    };
    store
        .record_intent(intent(&intent_id, 100))
        .expect("record intent-only");
    store
        .record_intent(intent(&committed_id, 100))
        .expect("record committed intent");
    store
        .commit_refs("root", std::slice::from_ref(&committed_id))
        .expect("commit attachment ref");

    // Boundary guard: both an intent and a commit are live refs for their
    // session; another session (or an unknown id) holds no ref.
    assert!(
        store.holds_ref("root", &intent_id).expect("holds intent"),
        "an uncommitted intent is a live ref"
    );
    assert!(
        store
            .holds_ref("root", &committed_id)
            .expect("holds commit"),
        "a committed attachment is a live ref"
    );
    assert!(
        !store
            .holds_ref("other-session", &committed_id)
            .expect("no cross-session ref"),
        "a ref belongs only to its own session"
    );
    assert!(
        !store
            .holds_ref("root", &AttachmentId::new("sha256:never-referenced"))
            .expect("no ref for unknown id"),
        "an id never referenced holds no ref"
    );

    // Root set: every live ref, intent or committed.
    let refs = store.list_all_refs().expect("list all refs");
    assert!(refs.contains(&intent_id), "intents feed the GC root set");
    assert!(refs.contains(&committed_id), "commits feed the GC root set");

    // Uncommitted listing still distinguishes intents from commits.
    let uncommitted = store.list_uncommitted(1_000_000).expect("list uncommitted");
    assert!(
        uncommitted
            .iter()
            .any(|entry| entry.attachment_id == intent_id),
        "an uncommitted intent is listed as uncommitted"
    );
    assert!(
        !uncommitted
            .iter()
            .any(|entry| entry.attachment_id == committed_id),
        "a committed attachment is not listed as uncommitted"
    );

    // Forget drops the ref from both the boundary guard and the root set.
    store.forget("root", &intent_id).expect("forget intent ref");
    assert!(
        !store.holds_ref("root", &intent_id).expect("ref dropped"),
        "a forgotten ref is no longer held"
    );
    assert!(
        !store
            .list_all_refs()
            .expect("list after forget")
            .contains(&intent_id),
        "a forgotten ref leaves the root set"
    );
}

fn sha256_of(bytes: &[u8]) -> impl std::fmt::LowerHex {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
}

async fn runtime_persistence_survives_reopen(factory: ReopenableRuntimePersistence) {
    session_execution_lease_first_claim_excludes_concurrent_reopen_handles(&factory).await;

    let meta = SessionMeta {
        session_id: "root".to_string(),
        session_name: "Durable Root".to_string(),
        created_at: "2026-06-02T00:00:00Z".to_string(),
        model: "gpt-5.4-mini".to_string(),
        cwd: Some("/tmp/lash-reopen".to_string()),
        relation: SessionRelation::Root,
    };
    factory
        .open
        .save_session_meta(meta.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(77)),
        ..RuntimeSessionState::default()
    };
    let initial_commit = commit_runtime_state_for_test(
        &factory.open,
        RuntimeCommit::persisted_state(&state, &[]),
        "reopen",
    )
    .await
    .expect("commit state");
    state.head_revision = Some(initial_commit.head_revision);

    let application_lease =
        claim_session_execution_lease_for_test(&factory.open, "root", "reopen-applications").await;
    let mut expected_applications = Vec::new();
    for (turn_index, turn_id) in ["z-reopen-application", "a-reopen-application"]
        .into_iter()
        .enumerate()
    {
        factory
            .open
            .enqueue_pending_turn_input(
                pending_next_turn_input_draft("root", &format!("reopen application {turn_index}"))
                    .with_source_key(format!("host:reopen-application-{turn_index}")),
            )
            .await
            .expect("enqueue reopen application");
        let mut claim = factory
            .open
            .claim_next_turn_inputs(
                "root",
                &application_lease.fence(),
                &lease_owner("reopen-applications"),
                1,
            )
            .await
            .expect("claim reopen application")
            .expect("reopen application claim");
        claim.record_initial_turn_application(
            turn_id,
            &format!("reopen-application-message-{turn_index}"),
        );
        expected_applications.extend(claim.applications.clone());

        let mut commit = RuntimeCommit::persisted_state(&state, &[])
            .with_session_execution_lease(application_lease.fence())
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(application_lease.completion());
        }
        let hash = commit
            .turn_commit_hash()
            .expect("hash reopen application turn commit");
        let result = factory
            .open
            .commit_runtime_state(commit.with_turn_commit(RuntimeTurnCommitStamp::new(
                "root",
                crate::OperationId::turn("root", turn_id, "final"),
                hash,
            )))
            .await
            .expect("commit reopen application");
        state.head_revision = Some(result.head_revision);
    }
    let queued = factory
        .open
        .enqueue_queued_work(
            queued_draft(
                "root",
                "survives reopen",
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
            )
            .with_source_key("reopen:queued"),
        )
        .await
        .expect("enqueue queued work");
    let attachment = AttachmentId::new("reopen-attachment".to_string());
    factory
        .open
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: "root".to_string(),
            canonical_uri: "sha256:reopen-attachment".to_string(),
            intent_at_epoch_ms: 100,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record attachment intent");

    let reopened_meta = factory
        .reopen
        .load_session_meta()
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta.session_name, meta.session_name);
    let reopened = factory
        .reopen
        .load_session(SessionReadScope::FullGraph)
        .await
        .expect("load reopened state")
        .expect("reopened state");
    assert_eq!(reopened.session_id, "root");
    assert_eq!(
        reopened
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.tool_state.as_ref())
            .map(|tool_state| tool_state.generation()),
        Some(77)
    );
    assert_eq!(
        factory
            .reopen
            .list_turn_input_applications("root")
            .await
            .expect("list applications from reopened handle"),
        expected_applications,
        "a fresh durable handle must reconcile applications in turn-commit order"
    );
    let reopened_queue = factory
        .reopen
        .list_queued_work("root")
        .await
        .expect("list reopened queue");
    assert_eq!(reopened_queue.len(), 1);
    assert_eq!(reopened_queue[0].batch_id, queued.batch_id);
    assert_eq!(
        queued_batch_text(&reopened_queue[0]),
        Some("survives reopen")
    );
    let reopened_intents = factory
        .reopen
        .list_uncommitted(200)
        .expect("list reopened attachment intents");
    assert!(
        reopened_intents
            .iter()
            .any(|intent| intent.attachment_id == attachment),
        "attachment intent rows must survive reopening a durable store"
    );
}

async fn session_execution_lease_first_claim_excludes_concurrent_reopen_handles(
    factory: &ReopenableRuntimePersistence,
) {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let open = Arc::clone(&factory.open);
    let reopen = Arc::clone(&factory.reopen);
    let open_barrier = Arc::clone(&barrier);
    let reopen_barrier = Arc::clone(&barrier);
    let open_owner = lease_owner("owner-a");
    let reopen_owner = lease_owner("owner-b");

    let open_claim = crate::task::spawn(async move {
        open_barrier.wait().await;
        open.try_claim_session_execution_lease("first-claim-race", &open_owner, 60_000)
            .await
    });
    let reopen_claim = crate::task::spawn(async move {
        reopen_barrier.wait().await;
        reopen
            .try_claim_session_execution_lease("first-claim-race", &reopen_owner, 60_000)
            .await
    });

    barrier.wait().await;
    let open_claim = open_claim
        .await
        .expect("join open first-claim race")
        .expect("open first-claim race");
    let reopen_claim = reopen_claim
        .await
        .expect("join reopen first-claim race")
        .expect("reopen first-claim race");
    let open_lease = open_claim.acquired();
    let reopen_lease = reopen_claim.acquired();
    let claim_count = usize::from(open_lease.is_some()) + usize::from(reopen_lease.is_some());
    assert_eq!(
        claim_count, 1,
        "exactly one concurrent first claim may acquire a session execution lease"
    );
    if let Some(lease) = open_lease.as_ref().or(reopen_lease.as_ref()) {
        factory
            .open
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release first-claim race winner");
    }
}

async fn queued_wake_delivery_is_source_key_idempotent_and_claimed_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let wake = ProcessWakeDelivery {
        wake_id: "wake-1".to_string(),
        target_session_id: "root".to_string(),
        target_scope_id: SessionScopeId::new("session:root"),
        process_id: "process-1".to_string(),
        sequence: 7,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new("root"),
            subject: RuntimeSubject::ProcessEvent {
                process_id: "process-1".to_string(),
                sequence: 7,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        dedupe_key: "wake-dedupe-1".to_string(),
        input: "wake payload".to_string(),
        created_at_ms: 1,
    };
    let first = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("enqueue wake");
    let replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake))
        .await
        .expect("replay wake enqueue");
    assert_eq!(
        first.batch_id, replay.batch_id,
        "wake source-key replay must return the original queued batch"
    );
    assert_eq!(
        store
            .list_queued_work("root")
            .await
            .expect("list queued wakes")
            .len(),
        1,
        "replayed wake must not create a second queued delivery"
    );

    let session_lease = claim_session_execution_lease_for_test(&store, "root", "wake-owner").await;
    let claim = store
        .claim_ready_queued_work(
            "root",
            &session_lease.fence(),
            &lease_owner("wake-owner"),
            QueuedWorkClaimBoundary::Idle,
            10,
        )
        .await
        .expect("claim wake")
        .expect("wake claim");
    assert_eq!(claim.batches.len(), 1);
    assert_eq!(claim.batches[0].items.len(), 1);
    assert!(matches!(
        claim.batches[0].items[0].payload,
        QueuedWorkPayload::ProcessWake { .. }
    ));
    let state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(session_lease.fence())
                .releasing_session_execution_lease(session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("wake delivery completion commits");
    assert!(
        store
            .list_queued_work("root")
            .await
            .expect("list after wake completion")
            .is_empty(),
        "completed wake delivery must be removed exactly once"
    );
}

async fn final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    state.agent_frames[0].created_at = "2026-07-26T10:00:00Z".to_string();
    let commit = RuntimeCommit::persisted_state(&state, &[]);
    let turn_commit_hash = commit.turn_commit_hash().expect("turn commit hash");
    let stamped_commit = commit.with_turn_commit(RuntimeTurnCommitStamp::new(
        "root",
        crate::OperationId::turn("root", "provider-turn", "final"),
        turn_commit_hash.clone(),
    ));

    let session_lease =
        claim_session_execution_lease_for_test(&store, "root", "provider-turn").await;
    let first = store
        .commit_runtime_state(
            stamped_commit
                .clone()
                .with_session_execution_lease(session_lease.fence())
                .releasing_session_execution_lease(session_lease.completion()),
        )
        .await
        .expect("first final commit requires a live session execution lease");
    let mut replay_state = state.clone();
    replay_state.agent_frames[0].created_at = "2026-07-26T10:00:09Z".to_string();
    let replay_commit = RuntimeCommit::persisted_state(&replay_state, &[]);
    let replay_hash = replay_commit.turn_commit_hash().expect("replay hash");
    assert_eq!(replay_hash, turn_commit_hash);
    let retry = store
        .commit_runtime_state(replay_commit.with_turn_commit(RuntimeTurnCommitStamp::new(
            "root",
            crate::OperationId::turn("root", "provider-turn", "final"),
            replay_hash,
        )))
        .await
        .expect("same final commit retries idempotently without a live lease");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(
        first.realization_digest,
        crate::store::graph_realization_digest(&stamped_commit.graph)
    );
    assert_eq!(
        retry.agent_frames[0].created_at, "2026-07-26T10:00:00Z",
        "receipt must return store-realized frame time from the first attempt"
    );
    replay_state.apply_persisted_commit_result(retry.clone());
    assert_eq!(
        replay_state.agent_frames[0].created_at, "2026-07-26T10:00:00Z",
        "runtime must rehydrate store-realized frame time from the receipt"
    );

    let mut retry_from_new_head = RuntimeCommit::persisted_state(&state, &[]);
    retry_from_new_head.expected_head_revision = Some(first.head_revision);
    let retry_hash = retry_from_new_head
        .turn_commit_hash()
        .expect("retry commit hash");
    assert_eq!(
        retry_hash, turn_commit_hash,
        "turn commit identity must not depend on the optimistic CAS revision"
    );

    let changed_state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        ..RuntimeSessionState::default()
    };
    let changed = RuntimeCommit::persisted_state(&changed_state, &[]);
    let changed_hash = changed.turn_commit_hash().expect("changed commit hash");
    let err = store
        .commit_runtime_state(changed.with_turn_commit(RuntimeTurnCommitStamp::new(
            "root",
            crate::OperationId::turn("root", "provider-turn", "final"),
            changed_hash,
        )))
        .await
        .expect_err("same provider turn id with a different commit hash must conflict");
    assert!(matches!(err, StoreError::RuntimeTurnCommitConflict { .. }));
}

async fn commit_rejects_non_derived_append_node_ids(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        ..RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "guard-turn", "final");
    let graph = crate::GraphCommitDelta::Append {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "rogue-node-id".to_string(),
            parent_node_id: None,
            caused_by: None,
            agent_frame_id: Some(state.current_agent_frame_id.clone()),
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "guard".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"ok": true})),
            },
        }],
        leaf_node_id: Some("rogue-node-id".to_string()),
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[])
        .with_operation(operation)
        .expect("stamp rogue proposal");
    let err = commit_runtime_state_for_test(&store, commit, "node-guard")
        .await
        .expect_err("store must rederive append node ids before writing");
    assert!(matches!(err, StoreError::NodeIdDerivationMismatch { .. }));
    assert!(
        store
            .load_session(SessionReadScope::FullGraph)
            .await
            .expect("load after guard rejection")
            .is_none(),
        "guard rejection must happen before any durable write"
    );
}
