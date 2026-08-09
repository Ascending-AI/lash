//! Shared durable-counter corruption and exhaustion conformance.

use std::future::Future;
use std::sync::Arc;

/// A raw durable counter selected by the shared fence-integrity fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceIntegrityTarget {
    QueuedWorkClaimFence { batch_id: String },
    SessionHeadRevision { session_id: String },
    SessionLeaseFencingToken { session_id: String },
    TriggerRevision { subscription_id: String },
}

/// Raw observation used to prove a refused operation made no mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceIntegrityObservation {
    pub value: i64,
    pub mutation_fingerprint: String,
}

/// Backend-owned seam for injecting and observing otherwise-invalid durable
/// counter values. SQL harnesses mutate their private tables directly; the
/// in-memory reference uses its testing-only raw-state seam.
#[async_trait::async_trait]
pub trait FenceIntegrityInjector: Send + Sync {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64);
    async fn observe_raw_value(&self, target: &FenceIntegrityTarget) -> FenceIntegrityObservation;
}

pub struct FenceIntegrityHandles {
    pub runtime: Arc<dyn crate::RuntimePersistence>,
    pub triggers: Arc<dyn crate::TriggerStore>,
    pub injector: Arc<dyn FenceIntegrityInjector>,
}

/// Run the shared signed-domain, per-row-successor, and no-mutation laws.
pub async fn fence_integrity_conformance<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: Future<Output = FenceIntegrityHandles>,
{
    negative_claim_fence(make("negative-claim-fence").await).await;
    negative_session_head_revision(make("negative-session-head").await).await;
    negative_session_lease_fence(make("negative-session-lease").await).await;
    divergent_claim_fences_advance_per_row(make("divergent-claim-fences").await).await;
    exhausted_claim_fence(make("exhausted-head-claim").await, true).await;
    exhausted_claim_fence(make("exhausted-non-head-claim").await, false).await;
    exhausted_trigger_revision(make("exhausted-trigger-revision").await).await;
}

/// SQL-only write-direction checks for public `u64` values that must fit the
/// durable signed domain before any query can observe a wrapped negative.
pub async fn signed_counter_write_domain_conformance(store: Arc<dyn crate::RuntimePersistence>) {
    let too_large = (i64::MAX as u64) + 1;
    let available_error = store
        .enqueue_queued_work(
            queued_draft("signed-write-available", "available").with_available_at_ms(too_large),
        )
        .await
        .expect_err("unrepresentable available_at_ms must refuse before insert");
    assert!(matches!(
        available_error,
        crate::StoreError::MonotonicCounterOverflow {
            counter: "queued_work_available_at_ms",
            current,
        } if current == too_large
    ));
    assert!(
        store
            .list_queued_work("signed-write-available")
            .await
            .expect("list after refused available_at_ms")
            .is_empty()
    );

    let lease_owner =
        crate::LeaseOwnerIdentity::opaque("signed-write-lease", "signed-write-lease:incarnation");
    let lease_error = store
        .try_claim_session_execution_lease("signed-write-lease", &lease_owner, u64::MAX)
        .await
        .expect_err("unrepresentable session lease expiry must refuse before insert");
    assert!(matches!(
        lease_error,
        crate::StoreError::MonotonicCounterOverflow {
            counter: "session_execution_lease_expires_at_ms",
            current: u64::MAX,
        }
    ));
    assert!(
        store
            .get_session_execution_lease("signed-write-lease")
            .await
            .expect("read after refused session lease")
            .is_none()
    );

    let generation_owner = crate::LeaseOwnerIdentity::opaque(
        "signed-write-generation",
        "signed-write-generation:incarnation",
    );
    let forged = crate::SessionExecutionLeaseAuthority {
        session_id: "signed-write-generation".to_string(),
        owner: generation_owner.clone(),
        lease_token: "forged-generation".to_string(),
        fencing_token: too_large,
    };
    let generation_error = store
        .claim_checkpoint_work(
            "signed-write-generation",
            &forged,
            &generation_owner,
            &crate::TurnId::from("signed-write-generation-turn"),
            crate::CheckpointKind::AfterWork,
            1,
            1,
        )
        .await
        .expect_err("unrepresentable session generation must refuse before query");
    assert!(matches!(
        generation_error,
        crate::StoreError::MonotonicCounterOverflow {
            counter: "session_lease_generation",
            current,
        } if current == too_large
    ));
}

fn queued_draft(session_id: &str, label: &str) -> crate::QueuedWorkBatchDraft {
    crate::QueuedWorkBatchDraft::new(
        session_id,
        crate::DeliveryPolicy::EarliestSafeBoundary,
        crate::SlotPolicy::Join,
        vec![crate::QueuedWorkPayload::agent_frame_task(
            format!("frame:{label}"),
            label,
            None,
        )],
    )
    .with_merge_key(crate::MergeKey::Group("fence-integrity".to_string()))
}

async fn claim_lease(
    store: &Arc<dyn crate::RuntimePersistence>,
    session_id: &str,
) -> (crate::LeaseOwnerIdentity, crate::SessionExecutionLease) {
    let owner = crate::LeaseOwnerIdentity::opaque("fence-owner", "fence-owner:incarnation");
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim fence-integrity session lease")
        .acquired()
        .expect("fence-integrity session lease is free");
    (owner, lease)
}

fn assert_corrupt(
    error: crate::StoreError,
    record_kind: &'static str,
    field: &'static str,
    value: i64,
) {
    match error {
        crate::StoreError::StoredDataCorrupt {
            record_kind: actual_kind,
            message,
        } => {
            assert_eq!(actual_kind, record_kind);
            assert_eq!(
                message,
                format!("{field} must be non-negative, got {value}")
            );
        }
        other => panic!("expected StoredDataCorrupt for {record_kind}.{field}, got {other:?}"),
    }
}

fn assert_overflow(error: crate::StoreError, counter: &'static str) {
    assert!(matches!(
        error,
        crate::StoreError::MonotonicCounterOverflow {
            counter: actual,
            current,
        } if actual == counter && current == i64::MAX as u64
    ));
}

async fn negative_claim_fence(handles: FenceIntegrityHandles) {
    let session_id = "fence-negative-claim";
    let batch = handles
        .runtime
        .enqueue_queued_work(queued_draft(session_id, "negative"))
        .await
        .expect("enqueue negative-fence row");
    let target = FenceIntegrityTarget::QueuedWorkClaimFence {
        batch_id: batch.batch_id,
    };
    handles.injector.inject_raw_value(&target, -1).await;
    let before = handles.injector.observe_raw_value(&target).await;
    let error = handles
        .runtime
        .list_queued_work(session_id)
        .await
        .expect_err("negative queued-work claim fence must refuse");
    assert_corrupt(error, "QueuedWorkBatch", "claim_fencing_token", -1);
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);
}

async fn negative_session_head_revision(handles: FenceIntegrityHandles) {
    let session_id = "fence-negative-head";
    handles
        .runtime
        .admit_and_bind_session(&crate::SessionBinding::root(
            session_id,
            &crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ))
        .await
        .expect("admit negative-head session");
    let state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    handles
        .runtime
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("materialize negative-head session row");
    let target = FenceIntegrityTarget::SessionHeadRevision {
        session_id: session_id.to_string(),
    };
    handles.injector.inject_raw_value(&target, -1).await;
    let before = handles.injector.observe_raw_value(&target).await;
    let error = handles
        .runtime
        .load_session()
        .await
        .expect_err("negative session-head revision must refuse");
    assert_corrupt(error, "SessionHeadMeta", "head_revision", -1);
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);
}

async fn negative_session_lease_fence(handles: FenceIntegrityHandles) {
    let session_id = "fence-negative-lease";
    let _ = claim_lease(&handles.runtime, session_id).await;
    let target = FenceIntegrityTarget::SessionLeaseFencingToken {
        session_id: session_id.to_string(),
    };
    handles.injector.inject_raw_value(&target, -1).await;
    let before = handles.injector.observe_raw_value(&target).await;
    let error = handles
        .runtime
        .get_session_execution_lease(session_id)
        .await
        .expect_err("negative session-lease fence must refuse");
    assert_corrupt(error, "SessionExecutionLease", "fencing_token", -1);
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);
}

async fn divergent_claim_fences_advance_per_row(handles: FenceIntegrityHandles) {
    let session_id = "fence-divergent-rows";
    let first = handles
        .runtime
        .enqueue_queued_work(queued_draft(session_id, "first"))
        .await
        .expect("enqueue first divergent row");
    let second = handles
        .runtime
        .enqueue_queued_work(queued_draft(session_id, "second"))
        .await
        .expect("enqueue second divergent row");
    let first_target = FenceIntegrityTarget::QueuedWorkClaimFence {
        batch_id: first.batch_id,
    };
    let second_target = FenceIntegrityTarget::QueuedWorkClaimFence {
        batch_id: second.batch_id,
    };
    handles.injector.inject_raw_value(&first_target, 5).await;
    handles.injector.inject_raw_value(&second_target, 41).await;
    let (owner, lease) = claim_lease(&handles.runtime, session_id).await;
    let claim = handles
        .runtime
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            2,
        )
        .await
        .expect("claim divergent fence rows")
        .expect("divergent fence rows are claimable");
    assert_eq!(claim.batches.len(), 2);
    assert_eq!(
        handles
            .injector
            .observe_raw_value(&first_target)
            .await
            .value,
        6
    );
    assert_eq!(
        handles
            .injector
            .observe_raw_value(&second_target)
            .await
            .value,
        42
    );
}

async fn exhausted_claim_fence(handles: FenceIntegrityHandles, exhausted_head: bool) {
    let session_id = if exhausted_head {
        "fence-exhausted-head"
    } else {
        "fence-exhausted-tail"
    };
    let first = handles
        .runtime
        .enqueue_queued_work(queued_draft(session_id, "first"))
        .await
        .expect("enqueue first exhausted fixture row");
    let second = handles
        .runtime
        .enqueue_queued_work(queued_draft(session_id, "second"))
        .await
        .expect("enqueue second exhausted fixture row");
    let first_target = FenceIntegrityTarget::QueuedWorkClaimFence {
        batch_id: first.batch_id,
    };
    let second_target = FenceIntegrityTarget::QueuedWorkClaimFence {
        batch_id: second.batch_id,
    };
    handles.injector.inject_raw_value(&first_target, 5).await;
    handles.injector.inject_raw_value(&second_target, 9).await;
    let exhausted = if exhausted_head {
        &first_target
    } else {
        &second_target
    };
    handles.injector.inject_raw_value(exhausted, i64::MAX).await;
    let first_before = handles.injector.observe_raw_value(&first_target).await;
    let second_before = handles.injector.observe_raw_value(&second_target).await;
    let (owner, lease) = claim_lease(&handles.runtime, session_id).await;
    let error = handles
        .runtime
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            2,
        )
        .await
        .expect_err("any exhausted selected row must refuse the whole claim");
    assert_overflow(error, "queued_work_claim_fencing_token");
    assert_eq!(
        handles.injector.observe_raw_value(&first_target).await,
        first_before
    );
    assert_eq!(
        handles.injector.observe_raw_value(&second_target).await,
        second_before
    );
}

async fn exhausted_trigger_revision(handles: FenceIntegrityHandles) {
    let session_id = "fence-exhausted-trigger";
    let owner_scope = crate::TriggerOwnerScope::session(session_id);
    let actor = crate::ProcessOriginator::session(crate::SessionScope::new(session_id));
    let subscription_key = "fence-trigger";
    let draft = crate::TriggerSubscriptionDraft::for_process(
        subscription_key,
        crate::ProcessExecutionEnvRef::new("fence-trigger-env"),
        "fence.event",
        "fence-source",
        crate::ProcessInput::Engine {
            kind: "fence".to_string(),
            payload: serde_json::json!({}),
        },
        crate::ProcessIdentity::new("fence"),
    );
    let registered = handles
        .triggers
        .execute_command(
            "fence-trigger-register",
            crate::TriggerCommand::Register {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                draft,
            },
        )
        .await
        .expect("register exhausted trigger")
        .expect("trigger registration succeeds");
    let crate::TriggerCommandOutcome::Mutation { receipt } = registered else {
        panic!("trigger registration must return a mutation receipt")
    };
    let target = FenceIntegrityTarget::TriggerRevision {
        subscription_id: receipt.record_snapshot.subscription_id,
    };
    handles.injector.inject_raw_value(&target, i64::MAX).await;
    let before = handles.injector.observe_raw_value(&target).await;
    let error = handles
        .triggers
        .execute_command(
            "fence-trigger-disable",
            crate::TriggerCommand::Disable {
                owner_scope,
                actor,
                subscription_key: subscription_key.to_string(),
                expected_revision: i64::MAX as u64,
            },
        )
        .await
        .expect("trigger store remains operational")
        .expect_err("exhausted trigger revision must refuse");
    assert!(matches!(
        error,
        crate::TriggerOperationError::RevisionOverflow {
            current_revision,
            ..
        } if current_revision == i64::MAX as u64
    ));
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);

    let plugin_error = handles
        .triggers
        .delete_session_subscriptions(session_id)
        .await
        .expect_err("trigger-store deletion must refuse an exhausted revision");
    assert!(matches!(
        plugin_error,
        crate::PluginError::MonotonicCounterOverflow {
            ref counter,
            current,
        } if counter == "trigger_subscription_revision" && current == i64::MAX as u64
    ));
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);
}
