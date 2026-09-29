//! Durable store-recovery laws over fresh persistence handles.

use super::*;
use lash_core::testing::RuntimeStoreTestDriveExt as _;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::time::Duration;

const RECOVERY_TTL: Duration = Duration::from_millis(300);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);
const RECOVERY_SUCCESSOR_TTL_MS: u64 = 60_000;
const RECOVERY_ACQUIRE_DEADLINE: Duration = Duration::from_secs(3);

/// How store-recovery conformance drives the predecessor lease to expiry.
#[derive(Clone)]
pub enum StoreRecoveryLeaseTiming {
    /// Let a realtime backend's authoritative clock advance explicitly.
    Realtime,
    /// Advance the injected embedded-backend clock by the exact semantic TTL.
    Controlled(std::sync::Arc<dyn Fn(u64) + Send + Sync>),
}

impl StoreRecoveryLeaseTiming {
    pub fn controlled(advance: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self::Controlled(std::sync::Arc::new(advance))
    }

    async fn expire_predecessor(&self) {
        match self {
            Self::Realtime => tokio::time::sleep(RECOVERY_TTL).await,
            Self::Controlled(advance) => advance(RECOVERY_TTL.as_millis() as u64),
        }
    }

    fn is_realtime(&self) -> bool {
        matches!(self, Self::Realtime)
    }
}

/// The backend's maker hands every recovery law its own persistence handle
/// rather than one shared instance.
pub async fn store_recovery_fresh_instances<F>(make: &F, label: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let first = make(label);
    let second = make(label);
    assert_fresh_instances(&first, &second, "store_recovery");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("300ms TTL / 100ms renew satisfies ttl >= 3x renew")
}

fn owner(id: impl Into<String>) -> crate::LeaseOwnerIdentity {
    let id = id.into();
    crate::LeaseOwnerIdentity::opaque(id.clone(), format!("{id}:incarnation"))
}

fn queued_work(session_id: &SessionId, source: &str) -> crate::QueuedWorkBatchDraft {
    crate::conformance::helpers::process_wake_work(
        session_id,
        &format!("{session_id}:{source}"),
        1,
        source,
        crate::DeliveryPolicy::EarliestSafeBoundary,
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn seed_and_admit(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    source: &str,
    lease_ttl_ms: u64,
) -> (crate::store::DriveFence, lash_core::store::RootAdmission) {
    bind_conformance_session(store, session_id).await;
    let batch = store
        .enqueue_queued_work(queued_work(session_id, source))
        .await
        .expect("seed store-recovery queued work");
    let lease_owner = owner(format!("{source}:owner-a"));
    let lease = store
        .seal_drive_epoch_for_test(
            session_id,
            &lease_owner,
            "seed-and-admit-executor",
            lease_ttl_ms,
        )
        .await
        .expect("seal the store-recovery drive")
        .acquired()
        .expect("fresh store-recovery drive");
    let admission = admitted_root(
        store,
        &lease,
        &root_of(source),
        lash_core::store::AdmittedHead::Batch(batch.batch_id.clone()),
    )
    .await;
    assert_eq!(
        admission.batch_ids(),
        vec![batch.batch_id],
        "the root's admission takes the seeded batch"
    );
    (lease, admission)
}

fn root_of(source: &str) -> String {
    format!("{source}:root")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn acquire_successor<F>(
    make: &F,
    session_id: &SessionId,
    source: &str,
    lease_timing: &StoreRecoveryLeaseTiming,
) -> (Arc<dyn RuntimeStore>, crate::store::DriveFence)
where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let successor = owner(format!("{source}:owner-b"));
    lease_timing.expire_predecessor().await;
    tokio::time::timeout(RECOVERY_ACQUIRE_DEADLINE, async {
        loop {
            let store = make(session_id);
            bind_conformance_session(&store, session_id).await;
            let acquired = store
                .seal_drive_epoch_for_test(
                    session_id,
                    &successor,
                    "acquire-successor-executor",
                    RECOVERY_SUCCESSOR_TTL_MS,
                )
                .await
                .expect("retry expired session lease")
                .acquired();
            if let Some(lease) = acquired {
                break (store, lease);
            }
            drop(store);
            if !lease_timing.is_realtime() {
                panic!("controlled predecessor expiry must make the successor claimable");
            }
            tokio::time::sleep(RECOVERY_RENEW).await;
        }
    })
    .await
    .expect("expired lease becomes claimable within its recovery TTL")
}

fn committed_state(session_id: &SessionId, marker: &str) -> crate::RuntimeSessionState {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, &format!("{session_id}:{marker}"), marker);
    state
}

/// Resume `source`'s root under `fence`: the recorded admission reads back
/// unchanged.
async fn resume(
    store: &Arc<dyn RuntimeStore>,
    fence: &crate::store::DriveFence,
    source: &str,
    recorded: &lash_core::store::RootAdmission,
) -> lash_core::store::RootAdmission {
    let resumed = admitted_root(store, fence, &root_of(source), recorded.head.clone()).await;
    assert_eq!(
        resumed.batch_ids(),
        recorded.batch_ids(),
        "the successor resumes exactly the recorded admission"
    );
    resumed
}

/// While the root is unfinished no second root takes its rows: the session
/// admits one root at a time, and a bound row is no other root's head.
async fn assert_no_second_root(
    store: &Arc<dyn RuntimeStore>,
    fence: &crate::store::DriveFence,
    admission: &lash_core::store::RootAdmission,
) {
    let result = admit_root_for_test(
        store,
        fence,
        &crate::TurnId::from("store-recovery-second-root"),
        admission.head.clone(),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(crate::StoreError::UnfinishedRootConflict { .. })
        ),
        "admitted work must not be delivered again before settlement: {result:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_settled_once(make: impl Fn(&str) -> Arc<dyn RuntimeStore>, session_id: &SessionId) {
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_survives_before_claim_settlement<F>(
    make: &F,
    prefix: &str,
    lease_timing: &StoreRecoveryLeaseTiming,
) where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let session_id = SessionId::from(format!("{prefix}:checkpoint-before-settlement"));
    let source = "checkpoint-before-settlement";
    let writer = make(&session_id);
    let (_expired_lease, admission) =
        seed_and_admit(&writer, &session_id, source, recovery_timings().ttl_ms()).await;
    writer
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &committed_state(&session_id, "checkpoint-committed"),
            &[],
        ))
        .await
        .expect("commit checkpoint before the root settles");
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
        acquire_successor(make, &session_id, source, lease_timing).await;
    let resumed = resume(&successor_store, &successor_lease, source, &admission).await;
    assert_no_second_root(&successor_store, &successor_lease, &resumed).await;
    successor_store
        .commit_runtime_state(final_commit(
            crate::RuntimeCommit::persisted_state_for_test(&recovered_state, &[]),
            &successor_lease,
            completing_admission(&root_of(source), &resumed),
        ))
        .await
        .expect("settle the checkpoint-associated root");
    drop(successor_store);
    assert_settled_once(make, &session_id).await;
}

/// A root's final commit settles its admitted rows and publishes its state in
/// one transaction, once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_commit_settles_admitted_rows_once<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let session_id = SessionId::from(format!("{prefix}:atomic-settlement"));
    let source = "atomic-settlement";
    let writer = make(&session_id);
    let (lease, admission) =
        seed_and_admit(&writer, &session_id, source, RECOVERY_SUCCESSOR_TTL_MS).await;
    writer
        .commit_runtime_state(final_commit(
            crate::RuntimeCommit::persisted_state_for_test(
                &committed_state(&session_id, "atomically-settled"),
                &[],
            ),
            &lease,
            completing_admission(&root_of(source), &admission),
        ))
        .await
        .expect("atomically commit state and settle the root's rows");
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
    assert!(
        reader
            .unfinished_root(&session_id)
            .await
            .expect("read the unfinished root")
            .is_none(),
        "the final commit ends the root"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn recorded_commit_replay_is_idempotent<F>(make: &F, prefix: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let session_id = SessionId::from(format!("{prefix}:commit-replay"));
    let source = "commit-replay";
    let writer = make(&session_id);
    let (lease, admission) =
        seed_and_admit(&writer, &session_id, source, RECOVERY_SUCCESSOR_TTL_MS).await;
    let operation = crate::OperationId::turn(&session_id, "recorded-commit", "final");
    let (commit, _) = crate::RuntimeCommit::persisted_state_for_test(
        &committed_state(&session_id, "recorded-commit"),
        &[],
    )
    .with_operation(operation)
    .expect("stamp recorded commit");
    let commit = final_commit(
        commit,
        &lease,
        completing_admission(&root_of(source), &admission),
    );
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
