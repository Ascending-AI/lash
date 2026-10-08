//! Shared durable-counter corruption and exhaustion conformance.

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Arc;

/// A raw durable counter selected by the shared fence-integrity fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceIntegrityTarget {
    SessionHeadRevision { session_id: SessionId },
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
    pub injector: Arc<dyn FenceIntegrityInjector>,
}

pub async fn fence_integrity_conformance<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: Future<Output = FenceIntegrityHandles>,
{
    negative_session_head_revision(make("fence-negative-head").await).await;
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
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &SessionId::from(session_id),
        ))
        .await
        .expect("admit negative-head session");
    let state = crate::RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    handles
        .runtime
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("materialize negative-head session row");
    let target = FenceIntegrityTarget::SessionHeadRevision {
        session_id: SessionId::fixture(session_id.to_string()),
    };
    handles.injector.inject_raw_value(&target, -1).await;
    let before = handles.injector.observe_raw_value(&target).await;
    let error = handles
        .runtime
        .load_session_window(
            &SessionId::from(session_id),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect_err("negative session-head revision must refuse");
    assert_corrupt(error, "SessionHeadMeta", "head_revision", -1);
    assert_eq!(handles.injector.observe_raw_value(&target).await, before);
}
