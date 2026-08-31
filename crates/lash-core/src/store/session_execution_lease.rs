//! Session-execution-lease vocabulary: holder identity, the durable row, the
//! fences derived from it, and what a claim reports about the holder it displaced.

use super::StoreError;

/// Unforgeable proposal that identifies one logical lease-claim attempt.
///
/// Construct a fresh nonce for every distinct claim and borrow that same nonce
/// when retrying an ambiguous outcome from that claim. The inner value is
/// intentionally readable by store implementors but cannot be supplied by a
/// host, formatted through `Display`, or reconstructed from durable text.
#[derive(PartialEq, Eq)]
pub struct LeaseClaimNonce(String);

impl LeaseClaimNonce {
    /// Mints a globally unique nonce for one logical claim attempt.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Constructs a fixed nonce for deterministic durable-store fixtures.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn for_testing(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the opaque bytes a backend persists and compares on retry.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LeaseClaimNonce {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LeaseClaimNonce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("LeaseClaimNonce")
            .field(&"opaque")
            .finish()
    }
}

/// The token-scoped lease operation that refused stale authority.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionExecutionLeaseRefusalOperation {
    ExecutionFence,
    Renewal,
    RenewalInstall,
    Release,
}

impl SessionExecutionLeaseRefusalOperation {
    fn label(self) -> &'static str {
        match self {
            Self::ExecutionFence => "execution_fence",
            Self::Renewal => "renewal",
            Self::RenewalInstall => "renewal_install",
            Self::Release => "release",
        }
    }

    fn event(self) -> &'static str {
        match self {
            Self::ExecutionFence => "session_execution_lease.execution_fence_refused",
            Self::Renewal => "session_execution_lease.renewal_refused",
            Self::RenewalInstall => "session_execution_lease.renewal_install_refused",
            Self::Release => "session_execution_lease.release_refused",
        }
    }
}

/// Durable facts consulted while diagnosing a lease refusal.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionExecutionLeaseRefusalFacts<'a> {
    pub(crate) current_owner: Option<&'a LeaseOwnerIdentity>,
    pub(crate) current_executor_id: Option<&'a str>,
    pub(crate) current_token: Option<&'a str>,
    pub(crate) current_fencing_token: Option<u64>,
    pub(crate) current_expires_at_epoch_ms: Option<u64>,
    pub(crate) observed_at_epoch_ms: Option<u64>,
    pub(crate) minimum_expires_at_epoch_ms: Option<u64>,
    pub(crate) requested_session_id: Option<&'a str>,
    pub(crate) refusal_cause: Option<&'static str>,
}

impl<'a> SessionExecutionLeaseRefusalFacts<'a> {
    /// Capture the owner-and-token subset consulted by renewal and release.
    pub fn lifecycle(
        current_owner: Option<&'a LeaseOwnerIdentity>,
        current_executor_id: Option<&'a str>,
        current_token: Option<&'a str>,
    ) -> Self {
        Self {
            current_owner,
            current_executor_id,
            current_token,
            ..Self::default()
        }
    }
}

/// Emit the decision evidence required whenever a store returns a named
/// session-execution-lease refusal.
///
/// Token values remain opaque: the event carries only SHA-256 identities so an
/// operator can compare the presented and durable tokens without learning
/// either nonce.
/// For `renewal_install`, `current_*` describes the returned response rather
/// than the durable row, and `expiry_matched` means that response did not regress.
#[doc(hidden)]
pub fn trace_session_execution_lease_refusal(
    operation: SessionExecutionLeaseRefusalOperation,
    decision_basis: &'static str,
    observation_freshness: &'static str,
    presented: &SessionExecutionLeaseAuthority,
    facts: SessionExecutionLeaseRefusalFacts<'_>,
) {
    let current_owner_id = facts
        .current_owner
        .map(|owner| owner.owner_id.as_str())
        .unwrap_or("none");
    let current_incarnation_id = facts
        .current_owner
        .map(|owner| owner.incarnation_id.as_str())
        .unwrap_or("none");
    let current_executor_id = facts.current_executor_id.unwrap_or("none");
    let owner_matched = facts
        .current_owner
        .is_some_and(|owner| owner.same_incarnation(&presented.owner));
    let executor_matched = facts.current_executor_id == Some(presented.executor_id.as_str());
    let token_matched = facts.current_token == Some(presented.lease_token.as_str());
    let session_matched = facts
        .requested_session_id
        .map(|session_id| presented.session_id == session_id);
    let generation_matched = facts
        .current_fencing_token
        .map(|token| token == presented.fencing_token);
    let expiry_matched = facts.current_expires_at_epoch_ms.and_then(|expires_at| {
        facts
            .minimum_expires_at_epoch_ms
            .map(|minimum| expires_at >= minimum)
            .or_else(|| {
                facts
                    .observed_at_epoch_ms
                    .map(|observed_at| expires_at > observed_at)
            })
    });
    let refusal_cause = if operation == SessionExecutionLeaseRefusalOperation::RenewalInstall {
        facts.refusal_cause.unwrap_or(decision_basis)
    } else if operation == SessionExecutionLeaseRefusalOperation::Renewal
        || operation == SessionExecutionLeaseRefusalOperation::Release
    {
        decision_basis
    } else if session_matched == Some(false) {
        "session_binding_mismatch"
    } else if facts.current_fencing_token.is_none() {
        "missing_current_lease"
    } else if !owner_matched {
        "owner_mismatch"
    } else if !executor_matched {
        "executor_mismatch"
    } else if generation_matched == Some(false) {
        "generation_mismatch"
    } else if expiry_matched == Some(false) {
        "expired"
    } else if !token_matched {
        "token_mismatch"
    } else {
        decision_basis
    };
    let current_token_identity = facts
        .current_token
        .map(|token| {
            format!(
                "sha256:{}",
                crate::stable_hash::sha256_hex(token.as_bytes())
            )
        })
        .unwrap_or_else(|| "none".to_string());
    let presented_token_identity = format!(
        "sha256:{}",
        crate::stable_hash::sha256_hex(presented.lease_token.as_bytes())
    );
    let consulted_state = if operation == SessionExecutionLeaseRefusalOperation::RenewalInstall {
        "backend_renewal_response"
    } else {
        "session_execution_lease_row"
    };

    tracing::warn!(
        target: "lash_core::session_execution_lease",
        event = operation.event(),
        operation = operation.label(),
        decision_basis,
        session_id = presented.session_id.as_str(),
        presented_owner_id = presented.owner.owner_id.as_str(),
        presented_incarnation_id = presented.owner.incarnation_id.as_str(),
        presented_executor_id = presented.executor_id.as_str(),
        current_owner_id,
        current_incarnation_id,
        current_executor_id,
        session_matched = ?session_matched,
        owner_matched,
        executor_matched,
        token_matched,
        presented_fencing_token = presented.fencing_token,
        current_fencing_token = ?facts.current_fencing_token,
        generation_matched = ?generation_matched,
        current_expires_at_epoch_ms = ?facts.current_expires_at_epoch_ms,
        observed_at_epoch_ms = ?facts.observed_at_epoch_ms,
        minimum_expires_at_epoch_ms = ?facts.minimum_expires_at_epoch_ms,
        expiry_matched = ?expiry_matched,
        refusal_cause,
        current_token_identity = current_token_identity.as_str(),
        presented_token_identity = presented_token_identity.as_str(),
        consulted_state,
        observation_freshness,
        outcome = "refused",
    );
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
    /// Runtime-minted identity for one executor open under the host owner.
    pub executor_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
    pub claimed_at_epoch_ms: u64,
    /// Store-authored duration installed by the most recent claim, reentry, or renewal.
    pub lease_term_ms: u64,
    pub expires_at_epoch_ms: u64,
}

/// One diagnostic observation of a session-execution-lease row.
///
/// `observed_at_epoch_ms` is sampled from the same clock domain that authored
/// the lease timestamps. PostgreSQL reads it from the transaction clock in the
/// same transaction as `lease`; embedded stores return their injected clock.
/// Callers can therefore compare the two without mixing host and store clocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionExecutionLeaseObservation {
    /// Store-clock instant paired with the row read.
    pub observed_at_epoch_ms: u64,
    /// Held lease facts, or `None` for an absent, unleased, or released row.
    pub lease: Option<SessionExecutionLease>,
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
pub struct SessionExecutionLeaseAuthority {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub executor_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
}

/// The caller-supplied identity one session-execution-lease claim installs.
///
/// Backends take these four values as one value because they *are* one fact -
/// the row the claim writes. As loose parameters, the executor id and the lease
/// token are two adjacent `&str`s that a backend could silently transpose,
/// which would make one host's executor another's lease authority.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct SessionExecutionLeaseClaimIdentity<'a> {
    pub session_id: &'a str,
    pub owner: &'a LeaseOwnerIdentity,
    pub executor_id: &'a str,
    pub lease_token: &'a str,
}

/// Raw durable session-execution-lease facts shared by every SQL backend.
///
/// Released rows carry no owner, executor, or lease token while retaining their
/// fencing generation. A live row carries all three. The conversion into
/// [`SessionExecutionLease`] rejects every partial identity.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionExecutionLeaseRow {
    pub owner: Option<LeaseOwnerIdentity>,
    pub executor_id: Option<String>,
    pub lease_token: Option<String>,
    pub fencing_token: u64,
    pub claimed_at_ms: u64,
    pub lease_term_ms: u64,
    pub expires_at_ms: u64,
}

/// Decode the strictly paired columns that identify a lease owner.
#[doc(hidden)]
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

/// Convert a raw durable row into the live lease it represents.
#[doc(hidden)]
pub fn row_to_session_execution_lease(
    session_id: &str,
    row: SessionExecutionLeaseRow,
) -> Result<SessionExecutionLease, StoreError> {
    let corrupt = |field| StoreError::StoredDataCorrupt {
        record_kind: "SessionExecutionLease",
        message: format!("live row has NULL `{field}`"),
    };
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: row.owner.ok_or_else(|| corrupt("lease_owner_id"))?,
        executor_id: row
            .executor_id
            .ok_or_else(|| corrupt("lease_executor_id"))?,
        lease_token: row.lease_token.ok_or_else(|| corrupt("lease_token"))?,
        fencing_token: row.fencing_token,
        claimed_at_epoch_ms: row.claimed_at_ms,
        lease_term_ms: row.lease_term_ms,
        expires_at_epoch_ms: row.expires_at_ms,
    })
}

/// The lease-row facts every backend loads before deciding whether a retained
/// execution guard still has authority.
///
/// Storage implementations own how these facts are read and locked. Core owns
/// their meaning so an execution fence cannot silently differ by backend.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct SessionExecutionLeaseFenceFacts<'a> {
    pub owner: Option<&'a LeaseOwnerIdentity>,
    pub executor_id: Option<&'a str>,
    pub lease_token: Option<&'a str>,
    pub fencing_token: u64,
    pub expires_at_epoch_ms: u64,
}

/// Require the one canonical session-execution-lease fence predicate.
///
/// A retained guard has authority only while it names the requested session,
/// the current holder executor (owner, incarnation, and executor id), the current fencing generation, the current
/// lease token, and a lease whose expiry is strictly after `now_epoch_ms`.
/// Every refusal uses the same typed error construction from this function.
#[doc(hidden)]
pub fn require_current_session_execution_lease(
    session_id: &str,
    current: Option<SessionExecutionLeaseFenceFacts<'_>>,
    presented: &SessionExecutionLeaseAuthority,
    now_epoch_ms: u64,
) -> Result<(), StoreError> {
    let authorized = presented.session_id == session_id
        && current.is_some_and(|current| {
            current
                .owner
                .is_some_and(|owner| owner.same_incarnation(&presented.owner))
                && current.executor_id == Some(presented.executor_id.as_str())
                && current.fencing_token == presented.fencing_token
                && current.expires_at_epoch_ms > now_epoch_ms
                && current.lease_token == Some(presented.lease_token.as_str())
        });
    if authorized {
        Ok(())
    } else {
        trace_session_execution_lease_refusal(
            SessionExecutionLeaseRefusalOperation::ExecutionFence,
            "core_execution_fence_predicate",
            "backend_execution_fence_snapshot",
            presented,
            SessionExecutionLeaseRefusalFacts {
                current_owner: current.and_then(|current| current.owner),
                current_executor_id: current.and_then(|current| current.executor_id),
                current_token: current.and_then(|current| current.lease_token),
                current_fencing_token: current.map(|current| current.fencing_token),
                current_expires_at_epoch_ms: current.map(|current| current.expires_at_epoch_ms),
                observed_at_epoch_ms: Some(now_epoch_ms),
                requested_session_id: Some(session_id),
                ..Default::default()
            },
        );
        Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        })
    }
}

impl SessionExecutionLease {
    /// Captures session, owner, lease token, and fencing generation so each
    /// operation can consult its contracted subset.
    pub fn authority(&self) -> SessionExecutionLeaseAuthority {
        SessionExecutionLeaseAuthority {
            session_id: self.session_id.clone(),
            owner: self.owner.clone(),
            executor_id: self.executor_id.clone(),
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
    /// anyone), or exact owner/incarnation/executor reentry (which advances no
    /// generation).
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
    pub executor_id: String,
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
        displaced_executor_id: String,
        displaced_fencing_token: u64,
        displaced_expired_at_epoch_ms: u64,
    ) -> Self {
        Self {
            lease,
            displaced: Some(Box::new(SessionExecutionLeaseDisplacement {
                owner: displaced,
                executor_id: displaced_executor_id,
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
    fn lease_row_conversion_rejects_partial_identity() {
        let owner_error = lease_owner_from_columns(Some("owner".to_string()), None)
            .expect_err("an owner without an incarnation must be corrupt");
        assert!(matches!(
            owner_error,
            StoreError::StoredDataCorrupt {
                record_kind: "LeaseOwnerIdentity",
                ..
            }
        ));

        let row_error = row_to_session_execution_lease(
            "session",
            SessionExecutionLeaseRow {
                owner: Some(LeaseOwnerIdentity::opaque("owner", "incarnation")),
                executor_id: None,
                lease_token: Some("token".to_string()),
                fencing_token: 1,
                claimed_at_ms: 2,
                lease_term_ms: 3,
                expires_at_ms: 5,
            },
        )
        .expect_err("a partial live identity must be corrupt");
        assert!(matches!(
            row_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionExecutionLease",
                ..
            }
        ));
    }

    #[test]
    fn shared_authority_json_keeps_the_fence_and_completion_shape() {
        let authority = SessionExecutionLeaseAuthority {
            session_id: "session".to_string(),
            owner: LeaseOwnerIdentity::opaque("owner", "incarnation"),
            executor_id: "executor".to_string(),
            lease_token: "lease".to_string(),
            fencing_token: 7,
        };
        assert_eq!(
            serde_json::to_string(&authority).expect("serialize lease authority"),
            r#"{"session_id":"session","owner":{"owner_id":"owner","incarnation_id":"incarnation"},"executor_id":"executor","lease_token":"lease","fencing_token":7}"#
        );
    }

    #[test]
    fn fence_predicate_requires_every_ruled_authority_fact() {
        let owner = LeaseOwnerIdentity::opaque("owner", "incarnation");
        let authority = SessionExecutionLeaseAuthority {
            session_id: "session".to_string(),
            owner: owner.clone(),
            executor_id: "executor".to_string(),
            lease_token: "current-token".to_string(),
            fencing_token: 7,
        };
        let current = SessionExecutionLeaseFenceFacts {
            owner: Some(&owner),
            executor_id: Some("executor"),
            lease_token: Some("current-token"),
            fencing_token: 7,
            expires_at_epoch_ms: 101,
        };
        require_current_session_execution_lease("session", Some(current), &authority, 100)
            .expect("all current authority facts match");

        for rejected in [
            SessionExecutionLeaseFenceFacts {
                lease_token: Some("rotated-token"),
                ..current
            },
            SessionExecutionLeaseFenceFacts {
                fencing_token: 8,
                ..current
            },
            SessionExecutionLeaseFenceFacts {
                expires_at_epoch_ms: 100,
                ..current
            },
        ] {
            assert!(matches!(
                require_current_session_execution_lease(
                    "session",
                    Some(rejected),
                    &authority,
                    100
                ),
                Err(StoreError::SessionExecutionLeaseExpired { session_id })
                    if session_id == "session"
            ));
        }
        let stale_owner = LeaseOwnerIdentity::opaque("owner", "stale-incarnation");
        assert!(matches!(
            require_current_session_execution_lease(
                "session",
                Some(SessionExecutionLeaseFenceFacts {
                    owner: Some(&stale_owner),
                    ..current
                }),
                &authority,
                100
            ),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
        ));
        let mut wrong_session = authority.clone();
        wrong_session.session_id = "other-session".to_string();
        assert!(matches!(
            require_current_session_execution_lease("session", Some(current), &wrong_session, 100),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
        ));
        assert!(matches!(
            require_current_session_execution_lease("session", None, &authority, 100),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
        ));
    }
}
