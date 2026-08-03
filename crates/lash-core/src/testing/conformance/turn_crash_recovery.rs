//! Turn-lifecycle crash recovery conformance over fresh store instances.

use super::*;
use std::time::Duration;

const RECOVERY_TTL: Duration = Duration::from_millis(300);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);

/// Kill representative durable turn work at every persistence boundary and
/// prove the recovery outcome through independently opened store instances.
///
/// The matrix follows the real turn loop's durable lifecycle: claim, model
/// work before a checkpoint, both sides of a checkpoint commit, final commit,
/// post-commit/pre-reply replay, cancellation, and TTL-bounded retry. Effects
/// whose risk is process-local are covered by the backend helper binaries;
/// this suite owns substrate-independent store outcomes.
///
/// Integrator class: conformance-suite embedders (ADR 0051 class 4).
pub async fn turn_crash_phase_recovery_matrix<F>(make: F)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let first = make("fresh-instance-probe");
    let second = make("fresh-instance-probe");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "every conformance role must receive an independent backend instance"
    );
    drop((first, second));

    let prefix = format!("turn-crash-{}", uuid::Uuid::new_v4());
    claim_and_pre_checkpoint_crash_redelivers_once(&|| make("claim-model-retry"), &prefix).await;
    checkpoint_commit_crash_recovers_state_and_redelivers_once(
        &|| make("checkpoint-after-commit"),
        &prefix,
    )
    .await;
    final_commit_crash_completes_without_redelivery(&|| make("final-commit"), &prefix).await;
    post_commit_pre_reply_retry_is_idempotent(&|| make("post-commit-pre-reply"), &prefix).await;
    cancellation_crash_commits_typed_terminal(&|| make("cancellation"), &prefix).await;
}

fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("300ms TTL / 100ms renew satisfies ttl >= 3x renew")
}

fn owner(id: impl Into<String>) -> crate::LeaseOwnerIdentity {
    let id = id.into();
    crate::LeaseOwnerIdentity::opaque(id.clone(), format!("{id}:incarnation"))
}

fn queued_turn(session_id: &str, source: &str) -> crate::QueuedWorkBatchDraft {
    crate::QueuedWorkBatchDraft::new(
        session_id,
        crate::DeliveryPolicy::EarliestSafeBoundary,
        crate::SlotPolicy::Exclusive,
        vec![crate::QueuedWorkPayload::agent_frame_task(
            format!("frame:{source}"),
            source,
            None,
        )],
    )
    .with_source_key(format!("{session_id}:{source}"))
}

async fn seed_and_claim(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    source: &str,
) -> (crate::SessionExecutionLease, crate::QueuedWorkClaim) {
    let batch = store
        .enqueue_queued_work(queued_turn(session_id, source))
        .await
        .expect("seed crash-matrix queued turn");
    let lease_owner = owner(format!("{source}:owner-a"));
    let lease = store
        .try_claim_session_execution_lease(session_id, &lease_owner, recovery_timings().ttl_ms())
        .await
        .expect("claim crash-matrix session lease")
        .acquired()
        .expect("fresh crash-matrix session lease");
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &lease.fence(),
            &lease_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &[batch.batch_id],
        )
        .await
        .expect("claim crash-matrix queued turn")
        .expect("queued turn is claimable");
    (lease, claim)
}

async fn acquire_successor<F>(
    make: &F,
    session_id: &str,
    source: &str,
) -> (Arc<dyn RuntimePersistence>, crate::SessionExecutionLease)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let successor = owner(format!("{source}:owner-b"));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let store = make();
            let acquired = store
                .try_claim_session_execution_lease(
                    session_id,
                    &successor,
                    recovery_timings().ttl_ms(),
                )
                .await
                .expect("retry crashed session lease")
                .acquired();
            if let Some(lease) = acquired {
                break (store, lease);
            }
            drop(store);
            tokio::time::sleep(recovery_timings().renew_interval()).await;
        }
    })
    .await
    .expect("crashed lease becomes claimable within its scenario TTL")
}

fn committed_state(session_id: &str, marker: &str) -> crate::RuntimeSessionState {
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, &format!("{session_id}:{marker}"), marker);
    state
}

async fn claim_and_pre_checkpoint_crash_redelivers_once<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:claim-model-retry");
    let writer = make();
    let (_crashed_lease, crashed_claim) =
        seed_and_claim(&writer, &session_id, "claim-model-retry").await;
    drop(writer);

    let (successor_store, successor_lease) =
        acquire_successor(make, &session_id, "claim-model-retry").await;
    let successor_owner = owner("claim-model-retry:owner-b");
    let successor_claim = successor_store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &successor_lease.fence(),
            &successor_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &crashed_claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
        )
        .await
        .expect("retry pre-checkpoint work")
        .expect("pre-checkpoint crash is redelivered");
    assert_eq!(successor_claim.batches.len(), 1, "redelivered exactly once");
    successor_store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(
                &committed_state(&session_id, "retry-completed"),
                &[],
            )
            .releasing_session_execution_lease(successor_lease.completion())
            .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .expect("complete the redelivered turn");
    drop(successor_store);
    assert!(
        make()
            .list_queued_work(&session_id)
            .await
            .expect("read completed retry queue")
            .is_empty(),
        "completion consumes the sole redelivery"
    );
}

async fn checkpoint_commit_crash_recovers_state_and_redelivers_once<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:checkpoint-after-commit");
    let writer = make();
    let (_crashed_lease, crashed_claim) =
        seed_and_claim(&writer, &session_id, "checkpoint-after-commit").await;
    writer
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &committed_state(&session_id, "checkpoint-committed"),
            &[],
        ))
        .await
        .expect("commit checkpoint before crash");
    drop(writer);

    let cold_reader = make();
    let mut recovered_state = crate::load_persisted_session_state(cold_reader.as_ref())
        .await
        .expect("cold-load committed checkpoint")
        .expect("checkpoint session survives crash");
    assert_eq!(recovered_state.head_revision, 1);
    append_conformance_event_node(
        &mut recovered_state,
        &format!("{session_id}:checkpoint-retry-completed"),
        "checkpoint-retry-completed",
    );
    drop(cold_reader);

    let (successor_store, successor_lease) =
        acquire_successor(make, &session_id, "checkpoint-after-commit").await;
    let successor_claim = successor_store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &successor_lease.fence(),
            &owner("checkpoint-after-commit:owner-b"),
            crate::QueuedWorkClaimBoundary::Idle,
            &crashed_claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
        )
        .await
        .expect("claim checkpoint redelivery")
        .expect("checkpointed turn is redelivered");
    assert_eq!(successor_claim.batches.len(), 1, "redelivered exactly once");
    successor_store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&recovered_state, &[])
                .releasing_session_execution_lease(successor_lease.completion())
                .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .expect("complete checkpoint redelivery");
    drop(successor_store);
    assert!(
        make()
            .list_queued_work(&session_id)
            .await
            .expect("read completed checkpoint retry queue")
            .is_empty(),
        "completion consumes the sole checkpoint redelivery"
    );
}

async fn final_commit_crash_completes_without_redelivery<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:final-commit");
    let writer = make();
    let (lease, claim) = seed_and_claim(&writer, &session_id, "final-commit").await;
    writer
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(
                &committed_state(&session_id, "final-committed"),
                &[],
            )
            .releasing_session_execution_lease(lease.completion())
            .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("atomic final commit");
    drop(writer);

    let recovered = make();
    assert!(
        recovered
            .list_queued_work(&session_id)
            .await
            .expect("read queue after final commit")
            .is_empty(),
        "post-final-commit crash must not redeliver completed work"
    );
    assert!(
        recovered
            .load_session()
            .await
            .expect("load final commit")
            .is_some(),
        "the turn is completed durably"
    );
}

async fn post_commit_pre_reply_retry_is_idempotent<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:post-commit-pre-reply");
    let writer = make();
    let (lease, claim) = seed_and_claim(&writer, &session_id, "post-commit-pre-reply").await;
    let operation = crate::OperationId::turn(&session_id, "reply-lost", "final");
    let (commit, _) = crate::RuntimeCommit::persisted_state_for_test(
        &committed_state(&session_id, "reply-lost"),
        &[],
    )
    .with_operation(operation)
    .expect("stamp post-commit replay");
    let commit = commit
        .releasing_session_execution_lease(lease.completion())
        .completing_queue_claim(claim.completion());
    let first = writer
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit before reply loss");
    drop(writer);
    let replay = make()
        .commit_runtime_state(commit)
        .await
        .expect("retry after reply loss");
    assert_eq!(replay.head_revision, first.head_revision);
    assert_eq!(replay.checkpoint_ref, first.checkpoint_ref);
}

async fn cancellation_crash_commits_typed_terminal<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:cancellation");
    let writer = make();
    let (lease, claim) = seed_and_claim(&writer, &session_id, "cancellation").await;
    let mut state = committed_state(&session_id, "turn.cancelled");
    state.turn_index = 1;
    writer
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("commit cancelled terminal");
    drop(writer);
    let recovered = make()
        .load_session()
        .await
        .expect("recover cancelled terminal")
        .expect("cancelled session exists");
    assert_eq!(
        recovered
            .checkpoint
            .expect("cancelled checkpoint")
            .turn_state
            .turn_index,
        1
    );
}
