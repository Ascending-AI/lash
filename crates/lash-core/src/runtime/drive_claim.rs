//! Claim authority derived from the current sealed drive admission.

use std::sync::Arc;

use crate::store::{ClaimAuthority, DriveFence, RuntimeCommit, RuntimeCommitReceipt};
use crate::{Clock, LeaseOwnerIdentity, SessionId, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveClaimContinuity;

#[derive(Clone)]
pub struct BorrowedDriveAuthority(ClaimAuthority);

impl BorrowedDriveAuthority {
    pub fn fence(&self) -> ClaimAuthority {
        self.0.clone()
    }
}

pub struct DriveClaimGuard(ClaimAuthority);

impl DriveClaimGuard {
    pub fn from_drive_fence(
        fence: &DriveFence,
        owner: LeaseOwnerIdentity,
        executor_id: String,
    ) -> Self {
        let mut authority = ClaimAuthority::from_drive_fence(fence);
        authority.owner = owner;
        authority.executor_id = executor_id;
        Self(authority)
    }

    pub async fn try_acquire_for_executor(
        store: Arc<dyn crate::RuntimePersistence>,
        session_id: &SessionId,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        _timings: crate::store::LeaseTimings,
        _clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, StoreError> {
        let fence = crate::store::current_drive_fence(store.as_ref(), session_id).await?;
        Ok(
            fence
                .map(|fence| Self::from_drive_fence(&fence, owner.clone(), executor_id.to_owned())),
        )
    }

    pub fn borrowed_authority(&self) -> BorrowedDriveAuthority {
        BorrowedDriveAuthority(self.0.clone())
    }

    pub fn fence(&self) -> ClaimAuthority {
        self.0.clone()
    }

    pub fn mark_released(&self) {}

    pub fn is_lost(&self) -> bool {
        false
    }

    pub fn continuity(&self) -> Option<DriveClaimContinuity> {
        // A drive fence does not prevent a host service from changing the head.
        None
    }

    pub async fn release_if_live(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

pub async fn commit_runtime_state_without_session_lease(
    store: Arc<dyn crate::RuntimePersistence>,
    commit: RuntimeCommit,
    _owner: &LeaseOwnerIdentity,
    _executor_id: &str,
    _timings: crate::store::LeaseTimings,
    _clock: Arc<dyn Clock>,
) -> Result<RuntimeCommitReceipt, StoreError> {
    crate::store::commit_runtime_state_verified(store.as_ref(), commit).await
}

pub async fn commit_runtime_state_with_borrowed_drive(
    authority: &BorrowedDriveAuthority,
    store: Arc<dyn crate::RuntimePersistence>,
    mut commit: RuntimeCommit,
    _owner: &LeaseOwnerIdentity,
) -> Result<RuntimeCommitReceipt, StoreError> {
    commit.drive_fence = Some(Box::new(authority.fence().drive_fence()));
    crate::store::commit_runtime_state_verified(store.as_ref(), commit).await
}

pub fn trace_commit_cas_rejected(
    session_id: &SessionId,
    authority: Option<&ClaimAuthority>,
    claimant: &LeaseOwnerIdentity,
    claimant_executor_id: &str,
    error: &StoreError,
) {
    let StoreError::HeadRevisionConflict { expected, actual } = error else {
        return;
    };
    let owner = authority.map_or(claimant, |authority| &authority.owner);
    let executor_id = authority.map_or(claimant_executor_id, |authority| {
        authority.executor_id.as_str()
    });
    tracing::warn!(
        session_id = %session_id,
        owner_id = %owner.owner_id,
        incarnation_id = %owner.incarnation_id,
        executor_id,
        expected_head_revision = expected,
        actual_head_revision = actual,
        event = "session_head.commit_cas_rejected",
        "the commit's head compare-and-set was rejected"
    );
}
