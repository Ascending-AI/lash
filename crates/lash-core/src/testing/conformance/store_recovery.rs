//! Durable store-recovery laws over fresh persistence handles.

use super::*;
use std::time::Duration;

const RECOVERY_TTL: Duration = Duration::from_millis(300);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);

/// Prove durable recovery across claim, checkpoint, commit, and settlement
/// boundaries through independently constructed persistence handles.
///
/// These are store laws. They do not execute a runtime turn, model call, tool,
/// or runtime phase hook and therefore do not certify turn-phase recovery.
/// Process-local effect and wait-owner failures are covered by backend helper
/// processes; real turn-loop crash injection remains a separate acceptance
/// instrument.
///
/// Integrator class: conformance-suite embedders (ADR 0051 class 4).
pub async fn runtime_persistence_recovery_laws<F>(make: F)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let first = make("fresh-instance-probe");
    let second = make("fresh-instance-probe");
    assert_fresh_instances(&first, &second, "runtime_persistence_recovery_laws");
    drop((first, second));

    let prefix = format!("store-recovery-{}", uuid::Uuid::new_v4());
    expired_claim_is_recoverable_once(&make, &prefix).await;
    checkpoint_survives_before_claim_settlement(&make, &prefix).await;
    atomic_commit_settles_claim_once(&make, &prefix).await;
    recorded_commit_replay_is_idempotent(&make, &prefix).await;
}

fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("300ms TTL / 100ms renew satisfies ttl >= 3x renew")
}

fn owner(id: impl Into<String>) -> crate::LeaseOwnerIdentity {
    let id = id.into();
    crate::LeaseOwnerIdentity::opaque(id.clone(), format!("{id}:incarnation"))
}

fn queued_work(session_id: &str, source: &str) -> crate::QueuedWorkBatchDraft {
    crate::QueuedWorkBatchDraft::new(
        session_id,
        crate::DeliveryPolicy::EarliestSafeBoundary,
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
    bind_conformance_session(store, session_id).await;
    let batch = store
        .enqueue_queued_work(queued_work(session_id, source))
        .await
        .expect("seed store-recovery queued work");
    let lease_owner = owner(format!("{source}:owner-a"));
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &lease_owner,
            "seed-and-claim-executor",
            recovery_timings().ttl_ms(),
        )
        .await
        .expect("claim store-recovery session lease")
        .acquired()
        .expect("fresh store-recovery session lease");
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &lease.fence(),
            &lease_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &[batch.batch_id],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim store-recovery queued work")
        .expect("queued work is claimable");
    (lease, claim)
}

async fn acquire_successor<F>(
    make: &F,
    session_id: &str,
    source: &str,
) -> (Arc<dyn RuntimePersistence>, crate::SessionExecutionLease)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let successor = owner(format!("{source}:owner-b"));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let store = make(session_id);
            bind_conformance_session(&store, session_id).await;
            let acquired = store
                .try_claim_session_execution_lease(
                    session_id,
                    &successor,
                    "acquire-successor-executor",
                    recovery_timings().ttl_ms(),
                )
                .await
                .expect("retry expired session lease")
                .acquired();
            if let Some(lease) = acquired {
                break (store, lease);
            }
            drop(store);
            tokio::time::sleep(recovery_timings().renew_interval()).await;
        }
    })
    .await
    .expect("expired lease becomes claimable within its recovery TTL")
}

fn committed_state(session_id: &str, marker: &str) -> crate::RuntimeSessionState {
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, &format!("{session_id}:{marker}"), marker);
    state
}

fn claimed_batch_ids(claim: &crate::QueuedWorkClaim) -> Vec<String> {
    claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect()
}

async fn assert_no_parallel_reclaim(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    lease: &crate::SessionExecutionLease,
    claim_owner: &crate::LeaseOwnerIdentity,
    batch_ids: &[String],
) {
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                &lease.fence(),
                claim_owner,
                crate::QueuedWorkClaimBoundary::Idle,
                batch_ids,
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("probe a second pre-settlement reclaim")
            .acquired_no_rows(),
        "durably claimed work must not be delivered again before settlement"
    );
}

async fn assert_settled_once(make: impl Fn(&str) -> Arc<dyn RuntimePersistence>, session_id: &str) {
    let reader = make(session_id);
    bind_conformance_session(&reader, session_id).await;
    assert!(
        reader
            .list_queued_work(session_id)
            .await
            .expect("read settled queue evidence")
            .is_empty(),
        "atomic settlement removes the durable queued-work row exactly once"
    );
}

async fn expired_claim_is_recoverable_once<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:claim-expiry");
    let writer = make(&session_id);
    let (_expired_lease, expired_claim) =
        seed_and_claim(&writer, &session_id, "claim-expiry").await;
    drop(writer);

    let (successor_store, successor_lease) =
        acquire_successor(make, &session_id, "claim-expiry").await;
    let successor_owner = owner("claim-expiry:owner-b");
    let batch_ids = claimed_batch_ids(&expired_claim);
    let successor_claim = successor_store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &successor_lease.fence(),
            &successor_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &batch_ids,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("recover expired claim")
        .expect("expired claim is recoverable");
    assert_eq!(successor_claim.batches.len(), 1);
    assert_no_parallel_reclaim(
        &successor_store,
        &session_id,
        &successor_lease,
        &successor_owner,
        &batch_ids,
    )
    .await;
    successor_store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(
                &committed_state(&session_id, "claim-recovered"),
                &[],
            )
            .releasing_session_execution_lease(successor_lease.completion())
            .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .expect("settle recovered claim");
    drop(successor_store);
    assert_settled_once(make, &session_id).await;
}

async fn checkpoint_survives_before_claim_settlement<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:checkpoint-before-settlement");
    let writer = make(&session_id);
    let (_expired_lease, expired_claim) =
        seed_and_claim(&writer, &session_id, "checkpoint-before-settlement").await;
    writer
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &committed_state(&session_id, "checkpoint-committed"),
            &[],
        ))
        .await
        .expect("commit checkpoint before claim settlement");
    drop(writer);

    let cold_reader = make(&session_id);
    bind_conformance_session(&cold_reader, &session_id).await;
    let mut recovered_state = crate::load_persisted_session_state(cold_reader.as_ref())
        .await
        .expect("load the explicitly bound checkpoint session")
        .expect("checkpoint survives a fresh handle");
    assert_eq!(recovered_state.head_revision, 1);
    append_conformance_event_node(
        &mut recovered_state,
        &format!("{session_id}:settled"),
        "settled",
    );
    drop(cold_reader);

    let (successor_store, successor_lease) =
        acquire_successor(make, &session_id, "checkpoint-before-settlement").await;
    let successor_owner = owner("checkpoint-before-settlement:owner-b");
    let batch_ids = claimed_batch_ids(&expired_claim);
    let successor_claim = successor_store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &successor_lease.fence(),
            &successor_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &batch_ids,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("recover checkpoint-associated claim")
        .expect("checkpoint-associated claim is recoverable");
    assert_eq!(successor_claim.batches.len(), 1);
    assert_no_parallel_reclaim(
        &successor_store,
        &session_id,
        &successor_lease,
        &successor_owner,
        &batch_ids,
    )
    .await;
    successor_store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&recovered_state, &[])
                .releasing_session_execution_lease(successor_lease.completion())
                .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .expect("settle checkpoint-associated claim");
    drop(successor_store);
    assert_settled_once(make, &session_id).await;
}

async fn atomic_commit_settles_claim_once<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:atomic-settlement");
    let writer = make(&session_id);
    let (lease, claim) = seed_and_claim(&writer, &session_id, "atomic-settlement").await;
    writer
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(
                &committed_state(&session_id, "atomically-settled"),
                &[],
            )
            .releasing_session_execution_lease(lease.completion())
            .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("atomically commit state and settle claim");
    drop(writer);

    let reader = make(&session_id);
    bind_conformance_session(&reader, &session_id).await;
    assert!(
        reader
            .list_queued_work(&session_id)
            .await
            .expect("read queue after atomic settlement")
            .is_empty()
    );
    assert!(
        reader
            .load_session()
            .await
            .expect("load explicitly bound atomic commit")
            .is_some(),
        "the state half of the atomic settlement is durable"
    );
}

async fn recorded_commit_replay_is_idempotent<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let session_id = format!("{prefix}:commit-replay");
    let writer = make(&session_id);
    let (lease, claim) = seed_and_claim(&writer, &session_id, "commit-replay").await;
    let operation = crate::OperationId::turn(&session_id, "recorded-commit", "final");
    let (commit, _) = crate::RuntimeCommit::persisted_state_for_test(
        &committed_state(&session_id, "recorded-commit"),
        &[],
    )
    .with_operation(operation)
    .expect("stamp recorded commit");
    let commit = commit
        .releasing_session_execution_lease(lease.completion())
        .completing_queue_claim(claim.completion());
    let first = writer
        .commit_runtime_state(commit.clone())
        .await
        .expect("record commit outcome");
    drop(writer);

    let replay_store = make(&session_id);
    bind_conformance_session(&replay_store, &session_id).await;
    let replay = replay_store
        .commit_runtime_state(commit)
        .await
        .expect("replay recorded commit from a fresh handle");
    assert_eq!(replay.head_revision, first.head_revision);
    assert_eq!(replay.checkpoint_ref, first.checkpoint_ref);
}
