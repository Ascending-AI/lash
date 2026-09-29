//! Shared durable-counter corruption and exhaustion conformance.

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Arc;

/// A raw durable counter selected by the shared fence-integrity fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceIntegrityTarget {
    SessionHeadRevision { session_id: SessionId },
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
    pub runtime: Arc<dyn crate::RuntimeStore>,
    pub triggers: Arc<dyn crate::TriggerStore>,
    pub injector: Arc<dyn FenceIntegrityInjector>,
}

pub async fn fence_integrity_conformance<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: Future<Output = FenceIntegrityHandles>,
{
    negative_session_head_revision(make("fence-negative-head").await).await;
    exhausted_trigger_revision(make("fence-exhausted-trigger").await).await;
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn negative_session_head_revision(handles: FenceIntegrityHandles) {
    let session_id = "fence-negative-head";
    handles
        .runtime
        .admit_and_bind_session(&crate::SessionBinding::root(session_id))
        .await
        .expect("admit negative-head session");
    let state = crate::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    handles
        .runtime
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("materialize negative-head session row");
    let target = FenceIntegrityTarget::SessionHeadRevision {
        session_id: SessionId::from(session_id.to_string()),
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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
        .delete_session_subscriptions(&SessionId::from(session_id))
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
