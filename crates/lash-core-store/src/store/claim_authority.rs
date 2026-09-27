//! Claim authority carried by a sealed drive admission.

use crate::{ProcessId, SessionId, StoreError};

/// Decode the paired owner columns retained by queued-work and turn-input claims.
pub fn lease_owner_from_columns(
    owner_id: Option<String>,
    incarnation_id: Option<String>,
) -> Result<Option<LeaseOwnerIdentity>, StoreError> {
    match (owner_id, incarnation_id) {
        (None, None) => Ok(None),
        (Some(owner_id), Some(incarnation_id)) => Ok(Some(LeaseOwnerIdentity {
            owner_id,
            incarnation_id,
        })),
        fields => Err(StoreError::StoredDataCorrupt {
            record_kind: "LeaseOwnerIdentity",
            message: format!(
                "owner id and incarnation id must both be NULL or both be present, got {fields:?}"
            ),
        }),
    }
}

/// Stable identity for a lease holder.
///
/// Hosts keep `owner_id` stable for one worker or process, never one turn, and
/// assign a new `incarnation_id` on every process boot. The slack-clone example
/// is the reference shape: one constant worker owner id plus its boot-specific
/// incarnation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseOwnerIdentity {
    pub owner_id: String,
    pub incarnation_id: String,
}

impl LeaseOwnerIdentity {
    /// Constructs explicit owner and incarnation identity for store implementors; equality and
    /// fencing depend on both components, not a display-form concatenation.
    pub fn opaque(
        owner_id: impl Into<String>,
        incarnation_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.into(),
            incarnation_id: incarnation_id.into(),
        }
    }

    /// Stable owner identity for one engine process execution.
    ///
    /// Construction and recognition share this single representation so a
    /// formatting drift cannot silently turn a continuation into a fresh
    /// execution. The `restate:` owner-id spelling is durable: it is already
    /// written into lease rows and process start records, so it stays even
    /// though the constructor name no longer names an engine.
    pub fn engine_process_execution(
        process_id: &ProcessId,
        execution_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        Self::opaque(format!("restate:{process_id}"), execution_id)
    }

    pub fn engine_process_execution_id(&self, process_id: &ProcessId) -> Option<&str> {
        let expected = Self::engine_process_execution(process_id, &self.incarnation_id);
        self.same_incarnation(&expected)
            .then_some(self.incarnation_id.as_str())
    }

    /// Reports the same lease incarnation to store implementors only when both owner ID and
    /// incarnation ID match exactly.
    pub fn same_incarnation(&self, other: &LeaseOwnerIdentity) -> bool {
        self.owner_id == other.owner_id && self.incarnation_id == other.incarnation_id
    }
}

/// Shared evidence presented at every session-execution-lease seam.
///
/// Fence checks and release used to accept field-identical record types. That
/// allowed one role to gain an authority field without making the other role a
/// compile error. A single record keeps both paths structurally identical while
/// each operation consults its authority fields: execution claims validate the
/// owner, fencing generation, live expiry, and current lease token; renewal and
/// release validate owner plus lease token. The lease token does not become
/// session-head commit authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaimAuthority {
    pub session_id: SessionId,
    pub owner: LeaseOwnerIdentity,
    pub executor_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
}

impl ClaimAuthority {
    /// Evidence presented to a claim or queued run operation.
    pub fn fence(&self) -> Self {
        self.clone()
    }

    /// Evidence presented to a queued run operation.
    pub fn authority(&self) -> Self {
        self.clone()
    }

    /// Evidence carried by a settlement under this admission.
    pub fn completion(&self) -> Self {
        self.clone()
    }

    /// Derive claim authority from a sealed drive admission. The admission id
    /// supplies the old claim token field until claim storage is replaced.
    pub fn from_drive_fence(fence: &super::DriveFence) -> Self {
        let admission = fence.admission().as_str();
        Self {
            session_id: fence.session().clone(),
            owner: LeaseOwnerIdentity::opaque(format!("drive:{}", fence.session()), admission),
            executor_id: admission.to_owned(),
            lease_token: admission.to_owned(),
            fencing_token: fence.epoch(),
        }
    }

    /// The sealed admission represented by this claim authority.
    pub fn drive_fence(&self) -> super::DriveFence {
        super::DriveFence::sealed_by_store(
            self.session_id.clone(),
            self.fencing_token,
            super::AdmissionId::new(self.lease_token.clone()),
        )
    }
}
