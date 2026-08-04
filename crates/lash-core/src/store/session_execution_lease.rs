//! Session-execution-lease vocabulary: holder identity, the durable row, the
//! fences derived from it, and what a claim reports about the holder it displaced.

/// Stable identity for a lease holder.
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

    /// Stable owner identity for one Restate process execution invocation.
    ///
    /// Construction and recognition share this single representation so a
    /// formatting drift cannot silently turn a continuation into a fresh
    /// execution.
    pub fn restate_process_execution(
        process_id: &str,
        execution_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        Self::opaque(format!("restate:{process_id}"), execution_id)
    }

    /// Return the Restate execution id when this owner belongs to `process_id`.
    pub fn restate_process_execution_id(&self, process_id: &str) -> Option<&str> {
        let expected = Self::restate_process_execution(process_id, &self.incarnation_id);
        self.same_incarnation(&expected)
            .then_some(self.incarnation_id.as_str())
    }

    /// Reports the same lease incarnation to store implementors only when both owner ID and
    /// incarnation ID match exactly.
    pub fn same_incarnation(&self, other: &LeaseOwnerIdentity) -> bool {
        self.owner_id == other.owner_id && self.incarnation_id == other.incarnation_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLease {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    pub claimed_at_epoch_ms: u64,
    pub expires_at_epoch_ms: u64,
}

/// Shared authority presented at every session-execution-lease fence and
/// completion seam.
///
/// Fence checks and release used to accept field-identical record types. That
/// allowed one role to gain an authority field without making the other role a
/// compile error. A single record keeps both paths structurally identical.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLeaseAuthority {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
}

impl SessionExecutionLease {
    /// Captures session, owner, lease token, and fencing generation that store implementors must
    /// verify before accepting execution writes.
    pub fn authority(&self) -> SessionExecutionLeaseAuthority {
        SessionExecutionLeaseAuthority {
            session_id: self.session_id.clone(),
            owner: self.owner.clone(),
            lease_token: self.lease_token.clone(),
            fencing_token: self.fencing_token,
        }
    }

    /// Captures the shared authority for a session-execution fence check.
    pub fn fence(&self) -> SessionExecutionLeaseAuthority {
        self.authority()
    }

    /// Captures the same shared authority for idempotent lease release.
    pub fn completion(&self) -> SessionExecutionLeaseAuthority {
        self.authority()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionExecutionLeaseClaimOutcome {
    Acquired(SessionExecutionLeaseAcquisition),
    Busy { holder: SessionExecutionLease },
}

/// A granted session-execution-lease claim, together with whatever it displaced.
///
/// The displacement rides on the claim because the claim is the only moment at
/// which a takeover is atomically observable, and the winner is the only party
/// guaranteed to be alive to report it: the displaced runner may be dead, frozen,
/// or already replaced, in which case nothing it would have logged ever happens.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLeaseAcquisition {
    pub lease: SessionExecutionLease,
    /// The lapsed holder this claim took the lane from, observed inside the same
    /// atomic claim.
    ///
    /// Backends must report `Some` exactly when the row named a *different* owner
    /// incarnation immediately before this claim, and `None` otherwise: a first
    /// claim, a reclaim of a row whose holder released it (nothing was taken from
    /// anyone), or same-incarnation reentry (which advances no generation).
    /// Boxed for the same reason `ProcessRecord`'s optional facts are: a claim
    /// outcome is awaited on the turn path, so an inline copy grows every turn
    /// future, and this field is `None` on the common claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displaced: Option<Box<SessionExecutionLeaseDisplacement>>,
}

/// The prior durable holder a claim displaced.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLeaseDisplacement {
    pub owner: LeaseOwnerIdentity,
    /// The fencing token the displaced holder held, which ADR 0029 calls its
    /// generation. Strictly below the token the displacing claim received.
    pub fencing_token: u64,
    /// When the displaced holder's lease had lapsed. A live holder is reported
    /// [`SessionExecutionLeaseClaimOutcome::Busy`] instead of being displaced, so
    /// this is always at or before the claim.
    pub expired_at_epoch_ms: u64,
}

impl SessionExecutionLeaseAcquisition {
    /// A claim that took the lane from nobody: unheld, released, or reentered.
    pub fn fresh(lease: SessionExecutionLease) -> Self {
        Self {
            lease,
            displaced: None,
        }
    }

    /// Record the exact lapsed row a backend read before claiming, rather than
    /// deriving the displaced generation and expiry from the new lease.
    pub fn displacing_observed(
        lease: SessionExecutionLease,
        displaced: LeaseOwnerIdentity,
        displaced_fencing_token: u64,
        displaced_expired_at_epoch_ms: u64,
    ) -> Self {
        Self {
            lease,
            displaced: Some(Box::new(SessionExecutionLeaseDisplacement {
                owner: displaced,
                fencing_token: displaced_fencing_token,
                expired_at_epoch_ms: displaced_expired_at_epoch_ms,
            })),
        }
    }
}

impl SessionExecutionLeaseClaimOutcome {
    /// Returns the newly acquired session lease to store implementors and `None` when another
    /// holder remains busy; the observed busy holder and any displacement are discarded only by
    /// this projection.
    pub fn acquired(self) -> Option<SessionExecutionLease> {
        self.acquisition().map(|acquisition| acquisition.lease)
    }

    /// Returns the granted claim with its displacement evidence intact. Callers
    /// that report takeovers must use this rather than [`Self::acquired`].
    pub fn acquisition(self) -> Option<SessionExecutionLeaseAcquisition> {
        match self {
            Self::Acquired(acquisition) => Some(acquisition),
            Self::Busy { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_authority_json_keeps_the_fence_and_completion_shape() {
        let authority = SessionExecutionLeaseAuthority {
            session_id: "session".to_string(),
            owner: LeaseOwnerIdentity::opaque("owner", "incarnation"),
            lease_token: "lease".to_string(),
            fencing_token: 7,
        };
        assert_eq!(
            serde_json::to_string(&authority).expect("serialize lease authority"),
            r#"{"session_id":"session","owner":{"owner_id":"owner","incarnation_id":"incarnation"},"lease_token":"lease","fencing_token":7}"#
        );
    }
}
