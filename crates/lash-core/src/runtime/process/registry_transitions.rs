//! Pure transition tables shared by every durable process registry.
//!
//! A durable registry backend reads rows, asks this table what the observation
//! means, and applies the write the table prescribes. The backend never decides
//! a lease eligible, never classifies a missing process, and never parses a
//! persisted label: those are the decisions that must be identical on SQLite,
//! on PostgreSQL, and on anything else that implements
//! [`ProcessRegistry`](crate::ProcessRegistry), and duplicating them is how
//! they drift.
//!
//! The lease functions are ADR 0029's CAS-on-commit fence expressed as data
//! rather than as hand-written `WHERE` clauses that could diverge: the stored
//! lease authorizes a write only when it *is* the lease the caller claims to
//! hold. Two backends previously carried six copies of that three-part
//! predicate.
//!
//! There is no SQL, no clock, no I/O and nothing `async` here. `now_ms` is
//! always an input, because which clock is authoritative is the backend's
//! question: SQLite's process leases are stamped and compared against the
//! host's injected [`Clock`](crate::Clock), while PostgreSQL reads
//! `clock_timestamp()` inside the claim transaction so worker skew cannot steal
//! or spuriously expire a lease across hosts. Both hand the instant they trust
//! to the same table.

use sha2::{Digest, Sha256};

use crate::plugin::PluginError;
use crate::store::session_execution_lease::LeaseOwnerIdentity;

#[cfg(test)]
use super::model::ProcessStatus;
use super::model::{PROCESS_LEASE_SCHEMA_VERSION, ProcessLease};
use super::registry::{WakeDelivery, WakeDeliveryState, WakeDiscardReason};

/// Failure text for a persisted registry payload that will not decode.
///
/// One vocabulary for both backends: every process-registry row body that fails
/// `serde_json` reports this, so a corrupt payload reads the same whichever
/// substrate stored it.
fn registry_row_decode_error(err: serde_json::Error) -> PluginError {
    PluginError::Session(format!("failed to decode process registry row: {err}"))
}

// ---------------------------------------------------------------------------
// Lease eligibility
// ---------------------------------------------------------------------------

/// Columns of a persisted process-lease row, before projection.
///
/// Backends bind their own column reads into this shape and call
/// [`ProcessLeaseRow::project`]; the NULL handling and the integer widening are
/// the table's, not the SQL dialect's.
#[derive(Clone, Debug)]
pub struct ProcessLeaseRow {
    /// `lease_owner_id`; NULL once the lease is released or completed.
    pub owner_id: Option<String>,
    /// `lease_owner_incarnation_id`; absent on rows written before the
    /// incarnation column existed.
    pub incarnation_id: Option<String>,
    /// `lease_token`; NULL once the lease is released or completed.
    pub lease_token: Option<String>,
    /// `lease_fencing_token`, retained across release so succession is
    /// monotonic.
    pub fencing_token: i64,
    /// `lease_claimed_at_ms`.
    pub claimed_at_ms: i64,
    /// `lease_expires_at_ms`.
    pub expires_at_ms: i64,
}

impl ProcessLeaseRow {
    /// Project the row into the lease it records, or `None` when it records no
    /// holder.
    ///
    /// Completing or releasing a lease NULLs `lease_owner_id` and `lease_token`
    /// while retaining the monotonic `lease_fencing_token`. So for eligibility a
    /// released lease is indistinguishable from one that never existed — which
    /// is the point — yet the retained counter still fences the next holder
    /// against a stale writer that predates the release
    /// (see [`next_process_lease_fencing_token`]).
    pub fn project(self, process_id: &str) -> Option<ProcessLease> {
        let (Some(owner_id), Some(lease_token)) = (self.owner_id, self.lease_token) else {
            return None;
        };
        Some(ProcessLease {
            schema_version: PROCESS_LEASE_SCHEMA_VERSION,
            process_id: process_id.to_string(),
            owner: LeaseOwnerIdentity {
                incarnation_id: self.incarnation_id.unwrap_or_else(|| owner_id.clone()),
                owner_id,
            },
            lease_token,
            fencing_token: self.fencing_token as u64,
            claimed_at_epoch_ms: self.claimed_at_ms as u64,
            expires_at_epoch_ms: self.expires_at_ms as u64,
        })
    }
}

/// ADR 0029's fence, as a predicate.
///
/// The `stored` lease authorizes a `claimed` holder's write only when it *is*
/// that holder's lease — same `lease_token`, same owner incarnation, same
/// `fencing_token` — and has not expired at `now_ms`. A lease expiring exactly
/// at `now_ms` is expired, not live.
///
/// All three identity conjuncts are load-bearing and none subsumes another: the
/// token proves which claim, the incarnation proves which run of which owner,
/// and the fencing token proves which generation. A token-only check would let a
/// writer that was fenced out by a reclaim keep committing.
pub fn process_lease_still_holds(
    stored: Option<&ProcessLease>,
    claimed: &ProcessLease,
    now_ms: u64,
) -> bool {
    stored.is_some_and(|stored| {
        stored.lease_token == claimed.lease_token
            && stored.expires_at_epoch_ms > now_ms
            && stored.owner.same_incarnation(&claimed.owner)
            && stored.fencing_token == claimed.fencing_token
    })
}

/// [`process_lease_still_holds`] plus the process-id half of the fence,
/// reporting the canonical refusal.
///
/// A lease is only ever authority over its own process, so presenting one
/// process's lease for another's write is refused before the row is consulted at
/// all. Every lease-guarded registry write — renew, leased completion, and every
/// `*_with_authority` mutation — goes through here.
pub fn authorize_process_lease_write(
    process_id: &str,
    claimed: &ProcessLease,
    stored: Option<&ProcessLease>,
    now_ms: u64,
) -> Result<(), PluginError> {
    if claimed.process_id != process_id || !process_lease_still_holds(stored, claimed, now_ms) {
        return Err(PluginError::ProcessLeaseSuperseded {
            process_id: process_id.to_string(),
        });
    }
    Ok(())
}

/// What a fresh claim should do with the lease row it observed.
///
/// A claim has no `AcquireOnObservedFence` arm: it always re-reads the retained
/// `lease_fencing_token` column, even when it observed an expired lease. That is
/// one read more than [`ProcessLeaseReclaimDecision`] needs and it is deliberate
/// — the two operations are separate types so neither can be handed an arm it
/// cannot mean.
#[derive(Clone, Debug)]
pub enum ProcessLeaseClaimDecision {
    /// The caller's own incarnation already holds a live lease: persist this
    /// lease's expiry and report it acquired. The lease token and the fencing
    /// token are the ones already stored, so the caller's in-flight fenced
    /// writes stay valid.
    ExtendHeldLease {
        /// The stored lease, carrying its extended expiry.
        lease: ProcessLease,
    },
    /// A different incarnation holds a live lease. The holder travels with the
    /// decision so the caller can assess its liveness and reclaim exactly the
    /// lease it observed.
    ReportBusy {
        /// The live lease that blocks this claim.
        holder: ProcessLease,
    },
    /// No live lease: read the retained `lease_fencing_token` column and acquire
    /// on [`next_process_lease_fencing_token`] of it. The column outlives a
    /// released lease, so a re-claim never reuses a stale writer's token.
    AcquireOnRetainedFence,
}

/// What a fenced reclaim should do with the lease row it observed.
///
/// A reclaim has no extend arm: it exists for a claimant that already observed a
/// busy holder and proved it dead, so a still-live lease is reported busy
/// whoever holds it. It also never re-reads the retained column when it observed
/// a lease, because the observed lease's `fencing_token` *is* that column.
#[derive(Clone, Debug)]
pub enum ProcessLeaseReclaimDecision {
    /// A live lease blocks the reclaim.
    ReportBusy {
        /// The live lease that blocks this reclaim.
        holder: ProcessLease,
    },
    /// No lease row records a holder: acquire on the retained column's successor,
    /// exactly as a claim would.
    AcquireOnRetainedFence,
    /// The observed expired lease is the predecessor, so no second read is
    /// needed.
    AcquireOnObservedFence {
        /// The successor token to acquire on.
        fencing_token: u64,
    },
}

/// Decide a fresh claim against the lease row observed for the process.
pub fn decide_process_lease_claim(
    stored: Option<&ProcessLease>,
    owner: &LeaseOwnerIdentity,
    now_ms: u64,
    lease_ttl_ms: u64,
) -> ProcessLeaseClaimDecision {
    match stored {
        Some(stored) if stored.expires_at_epoch_ms > now_ms => {
            if stored.owner.same_incarnation(owner) {
                ProcessLeaseClaimDecision::ExtendHeldLease {
                    lease: ProcessLease {
                        expires_at_epoch_ms: now_ms.saturating_add(lease_ttl_ms),
                        ..stored.clone()
                    },
                }
            } else {
                ProcessLeaseClaimDecision::ReportBusy {
                    holder: stored.clone(),
                }
            }
        }
        _ => ProcessLeaseClaimDecision::AcquireOnRetainedFence,
    }
}

/// Decide a fenced reclaim against the lease row observed for the process.
pub fn decide_process_lease_reclaim(
    stored: Option<&ProcessLease>,
    now_ms: u64,
) -> Result<ProcessLeaseReclaimDecision, PluginError> {
    Ok(match stored {
        None => ProcessLeaseReclaimDecision::AcquireOnRetainedFence,
        Some(stored) if stored.expires_at_epoch_ms <= now_ms => {
            ProcessLeaseReclaimDecision::AcquireOnObservedFence {
                fencing_token: next_process_lease_fencing_token(stored.fencing_token)?,
            }
        }
        Some(stored) => ProcessLeaseReclaimDecision::ReportBusy {
            holder: stored.clone(),
        },
    })
}

/// Successor of a retained fencing token; `0` (no row ever written) yields `1`.
/// Returns a typed monotonic-counter overflow once the signed durable-store
/// ceiling (`i64::MAX`) has been reached.
pub fn next_process_lease_fencing_token(retained: u64) -> Result<u64, PluginError> {
    if retained >= i64::MAX as u64 {
        return Err(PluginError::MonotonicCounterOverflow {
            counter: "process_lease_fencing_token".to_string(),
            current: retained,
        });
    }
    Ok(retained + 1)
}

/// Mint the lease a successful claim acquires.
///
/// The `lease_token` is a durable value and this preimage is its contract:
/// `sha256("{process_id}:{owner_id}:{incarnation_id}:{claimed_at}:{fencing_token}")`,
/// rendered lower-case hex. A token is minted exactly once at claim time and
/// only ever compared afterwards — no backend re-derives it — so single-homing
/// the mint here (rather than beside each `INSERT`) is about one definition of
/// the durable format, not cross-backend recomputation. The raw concatenated
/// preimage is a deliberate exemption from the `stable_identity` framing
/// doctrine: like `WorkClaimLease::derive` and `LeaseClaimNonce`, the token is
/// an opaque minted capability compared for equality, never a projection of
/// live structure, so serde drift cannot reach it.
pub fn acquired_process_lease(
    process_id: &str,
    owner: &LeaseOwnerIdentity,
    fencing_token: u64,
    now_ms: u64,
    lease_ttl_ms: u64,
) -> ProcessLease {
    ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.to_string(),
        owner: owner.clone(),
        lease_token: format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{process_id}:{}:{}:{now_ms}:{fencing_token}",
                    owner.owner_id, owner.incarnation_id
                )
                .as_bytes()
            )
        ),
        fencing_token,
        claimed_at_epoch_ms: now_ms,
        expires_at_epoch_ms: now_ms.saturating_add(lease_ttl_ms),
    }
}

// ---------------------------------------------------------------------------
// Terminal / tombstone classification
// ---------------------------------------------------------------------------

/// The stamp a pruned process leaves behind in its tombstone row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTombstoneStamp {
    /// The terminal `status` label the row carried when it was pruned.
    pub terminal_label: String,
    /// When the prune removed the process row.
    pub pruned_at_ms: u64,
}

/// Refusal for a process whose row was pruned but whose tombstone is retained.
pub fn process_no_longer_retained(stamp: ProcessTombstoneStamp) -> PluginError {
    PluginError::ProcessNoLongerRetained {
        terminal_label: stamp.terminal_label,
        pruned_at_ms: stamp.pruned_at_ms,
    }
}

/// Refusal for a process id no registry ever knew.
pub fn unknown_process(process_id: &str) -> PluginError {
    PluginError::Session(format!("unknown process `{process_id}`"))
}

/// Classify a lookup that found no live process row.
///
/// The three-way split — retained row, retained tombstone, never known — is
/// what lets a host tell "this finished and was reaped" from "this id is
/// wrong", so the two absent cases must never collapse into one error.
pub fn absent_process_error(
    process_id: &str,
    tombstone: Option<ProcessTombstoneStamp>,
) -> PluginError {
    match tombstone {
        Some(stamp) => process_no_longer_retained(stamp),
        None => unknown_process(process_id),
    }
}

/// The `status` column labels a process row carries while it is still live.
///
/// Both registries' retention SQL is written against exactly this set —
/// `status IN ('running', 'waiting')` selects live rows, `status NOT IN (…)`
/// selects prune candidates — so every non-terminal
/// [`ProcessStatus`](crate::ProcessStatus) variant must appear here. The set is
/// a constant rather than a query fragment on purpose: the registries keep
/// their literal SQL, and
/// `non_terminal_status_labels_match_the_process_status_partition` is what fails
/// if a new variant would silently become prunable.
pub const NON_TERMINAL_PROCESS_STATUS_LABELS: [&str; 2] = ["running", "waiting"];

/// Every [`ProcessStatus`] variant, exhaustively, so the retention-label law can
/// partition the enum instead of a hand-kept sample of it.
///
/// The `match` in this function has no wildcard arm: adding a variant to
/// `ProcessStatus` stops the crate compiling until the new variant is
/// classified here, and the law test then decides whether
/// [`NON_TERMINAL_PROCESS_STATUS_LABELS`] must grow with it.
#[cfg(test)]
fn all_process_statuses() -> Vec<ProcessStatus> {
    let statuses = vec![
        ProcessStatus::Running,
        ProcessStatus::Waiting,
        ProcessStatus::Completed,
        ProcessStatus::Failed,
        ProcessStatus::Cancelled,
        ProcessStatus::Abandoned,
    ];
    for status in &statuses {
        // Exhaustive, wildcard-free: a new variant fails to compile here.
        match status {
            ProcessStatus::Running
            | ProcessStatus::Waiting
            | ProcessStatus::Completed
            | ProcessStatus::Failed
            | ProcessStatus::Cancelled
            | ProcessStatus::Abandoned => {}
        }
    }
    statuses
}

// ---------------------------------------------------------------------------
// Wake reconciliation vocabulary
// ---------------------------------------------------------------------------

/// Refusal for a wake-delivery id with no row.
pub fn unknown_wake_delivery(delivery_id: &str) -> PluginError {
    PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
}

/// Parse a persisted `state` label, the inverse of
/// [`WakeDeliveryState::as_str`].
///
/// The labels are durable values, so this is the only reader: an unrecognised
/// one is a refusal, never a default.
pub fn wake_delivery_state_from_label(
    delivery_id: &str,
    label: &str,
) -> Result<WakeDeliveryState, PluginError> {
    match label {
        "pending" => Ok(WakeDeliveryState::Pending),
        "enqueuing" => Ok(WakeDeliveryState::Enqueuing),
        "enqueued" => Ok(WakeDeliveryState::Enqueued),
        "discarded" => Ok(WakeDeliveryState::Discarded),
        state => Err(PluginError::Session(format!(
            "wake delivery `{delivery_id}` has unknown state `{state}`"
        ))),
    }
}

/// Parse a persisted `discard_reason` label, the inverse of
/// [`WakeDiscardReason::as_str`].
///
/// `None` stays `None`: a delivery that was never discarded carries no reason.
/// [`WakeDiscardReason`] is `#[non_exhaustive]`, so this single reader is also
/// the single place a new reason has to be taught.
pub fn wake_discard_reason_from_label(
    delivery_id: &str,
    label: Option<&str>,
) -> Result<Option<WakeDiscardReason>, PluginError> {
    match label {
        None => Ok(None),
        Some("expired") => Ok(Some(WakeDiscardReason::Expired)),
        Some("target_gone") => Ok(Some(WakeDiscardReason::TargetGone)),
        Some("retargeted") => Ok(Some(WakeDiscardReason::Retargeted)),
        Some("sequence_rewound") => Ok(Some(WakeDiscardReason::SequenceRewound)),
        Some(reason) => Err(PluginError::Session(format!(
            "wake delivery `{delivery_id}` has unknown discard reason `{reason}`"
        ))),
    }
}

/// Columns of a persisted wake-delivery row, before projection.
#[derive(Clone, Debug)]
pub struct WakeDeliveryRow {
    /// `delivery_id`, the structural wake identity.
    pub delivery_id: String,
    /// `state`.
    pub state_label: String,
    /// `claim_token`, the ownership fence of the current `enqueuing` claim.
    pub claim_token: Option<String>,
    /// `attempts`.
    pub attempts: i64,
    /// `first_attempt_ms`.
    pub first_attempt_ms: Option<i64>,
    /// `next_attempt_at_ms`.
    pub next_attempt_at_ms: i64,
    /// `expires_at_ms`.
    pub expires_at_ms: i64,
    /// `discard_reason`.
    pub discard_reason_label: Option<String>,
    /// `delivery_json`, the encoded [`ProcessWakeDelivery`](crate::ProcessWakeDelivery).
    pub delivery_json: String,
}

impl WakeDeliveryRow {
    /// Project the row into a [`WakeDelivery`].
    ///
    /// The two label columns are parsed before the payload is decoded, so a row
    /// with an unrecognised state reports the state refusal rather than a decode
    /// failure.
    pub fn project(self) -> Result<WakeDelivery, PluginError> {
        let state = wake_delivery_state_from_label(&self.delivery_id, &self.state_label)?;
        let discard_reason = wake_discard_reason_from_label(
            &self.delivery_id,
            self.discard_reason_label.as_deref(),
        )?;
        Ok(WakeDelivery {
            delivery_id: self.delivery_id,
            wake: serde_json::from_str(&self.delivery_json).map_err(registry_row_decode_error)?,
            state,
            claim_token: self.claim_token,
            attempts: self.attempts as u64,
            first_attempt_ms: self.first_attempt_ms.map(|value| value as u64),
            next_attempt_at_ms: self.next_attempt_at_ms as u64,
            expires_at_ms: self.expires_at_ms as u64,
            discard_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(owner_id: &str, incarnation_id: &str) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.to_string(),
            incarnation_id: incarnation_id.to_string(),
        }
    }

    fn lease(now_ms: u64, ttl_ms: u64, fencing_token: u64) -> ProcessLease {
        acquired_process_lease(
            "process",
            &owner("worker", "incarnation"),
            fencing_token,
            now_ms,
            ttl_ms,
        )
    }

    // --- A1 / A7: the fence -------------------------------------------------

    #[test]
    fn fence_holds_for_the_exact_stored_lease_before_expiry() {
        let stored = lease(1_000, 5_000, 3);
        assert!(process_lease_still_holds(
            Some(&stored),
            &stored.clone(),
            5_999
        ));
    }

    #[test]
    fn fence_fails_on_a_different_lease_token() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            lease_token: "other".to_string(),
            ..stored.clone()
        };
        assert!(!process_lease_still_holds(Some(&stored), &claimed, 2_000));
    }

    #[test]
    fn fence_fails_on_a_different_owner_incarnation() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            owner: owner("worker", "restarted"),
            ..stored.clone()
        };
        assert!(
            !process_lease_still_holds(Some(&stored), &claimed, 2_000),
            "a restarted incarnation of the same owner id is not the holder"
        );
    }

    #[test]
    fn fence_fails_on_a_different_owner_id() {
        // `same_incarnation` compares both halves of the identity; this pins
        // the owner-id half, which the incarnation-variation law above cannot.
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            owner: owner("other-worker", "incarnation"),
            ..stored.clone()
        };
        assert!(
            !process_lease_still_holds(Some(&stored), &claimed, 2_000),
            "a different owner id with the stored token and incarnation label is not the holder"
        );
    }

    #[test]
    fn fence_fails_on_a_different_fencing_token() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            fencing_token: 4,
            ..stored.clone()
        };
        assert!(!process_lease_still_holds(Some(&stored), &claimed, 2_000));
    }

    #[test]
    fn fence_fails_at_the_exact_expiry_instant() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(stored.expires_at_epoch_ms, 6_000);
        assert!(process_lease_still_holds(
            Some(&stored),
            &stored.clone(),
            5_999
        ));
        assert!(
            !process_lease_still_holds(Some(&stored), &stored.clone(), 6_000),
            "a lease expiring at `now` is expired, not live"
        );
    }

    #[test]
    fn fence_fails_when_no_lease_is_stored() {
        let claimed = lease(1_000, 5_000, 3);
        assert!(!process_lease_still_holds(None, &claimed, 2_000));
    }

    #[test]
    fn authorization_refuses_a_lease_for_another_process() {
        let stored = lease(1_000, 5_000, 3);
        let error = authorize_process_lease_write("other-process", &stored, Some(&stored), 2_000)
            .expect_err("a lease is only authority over its own process");
        assert!(
            matches!(
                &error,
                PluginError::ProcessLeaseSuperseded { process_id } if process_id == "other-process"
            ),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn authorization_succeeds_exactly_when_the_fence_holds() {
        let stored = lease(1_000, 5_000, 3);
        assert!(
            authorize_process_lease_write("process", &stored, Some(&stored), 2_000).is_ok(),
            "the holder's own live lease authorizes its write"
        );
        let error = authorize_process_lease_write("process", &stored, None, 2_000)
            .expect_err("a released lease authorizes nothing");
        assert!(
            matches!(&error, PluginError::ProcessLeaseSuperseded { process_id } if process_id == "process"),
            "unexpected refusal: {error}"
        );
        assert!(
            authorize_process_lease_write("process", &stored, Some(&stored), 6_000).is_err(),
            "an expired lease authorizes nothing"
        );
    }

    // --- A2 / A3 / A4: claim and reclaim -----------------------------------

    /// Compact rendering of a claim decision, so the arm laws read as equalities.
    fn arm(decision: &ProcessLeaseClaimDecision) -> String {
        match decision {
            ProcessLeaseClaimDecision::ExtendHeldLease { lease } => format!(
                "extend(token={}, fence={}, expires={})",
                lease.lease_token, lease.fencing_token, lease.expires_at_epoch_ms
            ),
            ProcessLeaseClaimDecision::ReportBusy { holder } => {
                format!("busy(token={})", holder.lease_token)
            }
            ProcessLeaseClaimDecision::AcquireOnRetainedFence => "acquire(retained)".to_string(),
        }
    }

    /// The same rendering for a reclaim decision.
    fn reclaim_arm(decision: &ProcessLeaseReclaimDecision) -> String {
        match decision {
            ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                format!("busy(token={})", holder.lease_token)
            }
            ProcessLeaseReclaimDecision::AcquireOnRetainedFence => "acquire(retained)".to_string(),
            ProcessLeaseReclaimDecision::AcquireOnObservedFence { fencing_token } => {
                format!("acquire(observed={fencing_token})")
            }
        }
    }

    #[test]
    fn claim_extends_its_own_live_lease_without_rotating_the_fence() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &stored.owner,
                2_000,
                5_000
            )),
            format!(
                "extend(token={}, fence=3, expires=7000)",
                stored.lease_token
            ),
            "a re-entering incarnation keeps its token and fencing token"
        );
    }

    #[test]
    fn claim_reports_busy_for_another_incarnation_on_a_live_lease() {
        let stored = lease(1_000, 5_000, 3);
        let busy = format!("busy(token={})", stored.lease_token);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("worker", "restarted"),
                2_000,
                5_000
            )),
            busy,
            "a restarted incarnation of the same owner id must not steal the lease"
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                2_000,
                5_000
            )),
            busy
        );
    }

    #[test]
    fn claim_on_an_absent_lease_acquires_on_the_retained_fence() {
        assert_eq!(
            arm(&decide_process_lease_claim(
                None,
                &owner("worker", "incarnation"),
                2_000,
                5_000
            )),
            "acquire(retained)"
        );
    }

    #[test]
    fn claim_on_an_expired_lease_still_acquires_on_the_retained_fence() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                6_000,
                5_000
            )),
            "acquire(retained)",
            "claim re-reads the retained column; only reclaim uses the observed lease"
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &stored.owner,
                6_000,
                5_000
            )),
            "acquire(retained)",
            "an expired lease is not extended, even by its own incarnation"
        );
    }

    #[test]
    fn reclaim_on_an_absent_lease_acquires_on_the_retained_fence() {
        assert_eq!(
            reclaim_arm(&decide_process_lease_reclaim(None, 2_000).expect("decide reclaim")),
            "acquire(retained)"
        );
    }

    #[test]
    fn reclaim_on_an_expired_lease_acquires_on_the_observed_successor() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 6_000).expect("decide reclaim"),
            ),
            "acquire(observed=4)"
        );
    }

    #[test]
    fn reclaim_reports_busy_on_a_live_lease_whoever_holds_it() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 2_000).expect("decide reclaim"),
            ),
            format!("busy(token={})", stored.lease_token),
            "reclaim has no same-incarnation extend arm"
        );
    }

    #[test]
    fn the_expiry_boundary_is_the_same_for_claim_and_reclaim() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(stored.expires_at_epoch_ms, 6_000);
        let busy = format!("busy(token={})", stored.lease_token);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                5_999,
                5_000
            )),
            busy
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                6_000,
                5_000
            )),
            "acquire(retained)"
        );
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 5_999).expect("decide reclaim"),
            ),
            busy
        );
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 6_000).expect("decide reclaim"),
            ),
            "acquire(observed=4)"
        );
    }

    #[test]
    fn fencing_token_succession_starts_at_one_and_refuses_overflow() {
        assert_eq!(next_process_lease_fencing_token(0).expect("advance"), 1);
        assert_eq!(next_process_lease_fencing_token(41).expect("advance"), 42);
        assert!(matches!(
            next_process_lease_fencing_token(i64::MAX as u64),
            Err(PluginError::MonotonicCounterOverflow {
                ref counter,
                current,
            }) if counter == "process_lease_fencing_token" && current == i64::MAX as u64
        ));
    }

    // --- A5: lease-row projection ------------------------------------------

    fn populated_row() -> ProcessLeaseRow {
        ProcessLeaseRow {
            owner_id: Some("worker".to_string()),
            incarnation_id: Some("incarnation".to_string()),
            lease_token: Some("token".to_string()),
            fencing_token: 7,
            claimed_at_ms: 1_000,
            expires_at_ms: 6_000,
        }
    }

    #[test]
    fn a_populated_lease_row_projects() {
        let lease = populated_row()
            .project("process")
            .expect("a row with an owner and a token records a holder");
        assert_eq!(lease.schema_version, PROCESS_LEASE_SCHEMA_VERSION);
        assert_eq!(lease.process_id, "process");
        assert_eq!(lease.owner, owner("worker", "incarnation"));
        assert_eq!(lease.lease_token, "token");
        assert_eq!(lease.fencing_token, 7);
        assert_eq!(lease.claimed_at_epoch_ms, 1_000);
        assert_eq!(lease.expires_at_epoch_ms, 6_000);
    }

    #[test]
    fn a_null_owner_id_records_no_holder() {
        assert!(
            ProcessLeaseRow {
                owner_id: None,
                ..populated_row()
            }
            .project("process")
            .is_none()
        );
    }

    #[test]
    fn a_null_lease_token_records_no_holder() {
        assert!(
            ProcessLeaseRow {
                lease_token: None,
                ..populated_row()
            }
            .project("process")
            .is_none()
        );
    }

    #[test]
    fn a_null_incarnation_id_defaults_to_the_owner_id() {
        let lease = ProcessLeaseRow {
            incarnation_id: None,
            ..populated_row()
        }
        .project("process")
        .expect("pre-incarnation rows still record a holder");
        assert_eq!(lease.owner, owner("worker", "worker"));
    }

    #[test]
    fn a_released_row_records_no_holder_but_retains_its_fence() {
        let released = ProcessLeaseRow {
            owner_id: None,
            incarnation_id: None,
            lease_token: None,
            fencing_token: 9,
            claimed_at_ms: 0,
            expires_at_ms: 0,
        };
        let retained = released.fencing_token as u64;
        assert!(
            released.project("process").is_none(),
            "a released lease is not a holder"
        );
        assert_eq!(
            next_process_lease_fencing_token(retained).expect("advance retained fence"),
            10,
            "the retained counter still fences the next holder"
        );
    }

    // --- A6: lease minting -------------------------------------------------

    #[test]
    fn the_lease_token_preimage_is_pinned() {
        let minted = acquired_process_lease(
            "process-a",
            &owner("owner-a", "incarnation-a"),
            5,
            1_700_000_000_000,
            30_000,
        );
        // sha256("process-a:owner-a:incarnation-a:1700000000000:5"). Durable
        // value: changing this literal changes every backend's lease identity.
        assert_eq!(
            minted.lease_token,
            "9f4707d653c3ba3578026558af3108694967b2583443ebbac5b80d86c2c626e3"
        );
        assert_eq!(minted.process_id, "process-a");
        assert_eq!(minted.fencing_token, 5);
        assert_eq!(minted.claimed_at_epoch_ms, 1_700_000_000_000);
        assert_eq!(minted.expires_at_epoch_ms, 1_700_000_030_000);
        assert_eq!(minted.schema_version, PROCESS_LEASE_SCHEMA_VERSION);
    }

    #[test]
    fn the_lease_token_preimage_is_field_ordered() {
        let straight = acquired_process_lease("p", &owner("a", "b"), 1, 10, 10);
        let swapped = acquired_process_lease("p", &owner("b", "a"), 1, 10, 10);
        assert_ne!(
            straight.lease_token, swapped.lease_token,
            "owner id and incarnation id occupy distinct preimage positions"
        );
        for (left, right) in [
            (
                acquired_process_lease("p", &owner("a", "b"), 2, 10, 10).lease_token,
                straight.lease_token.clone(),
            ),
            (
                acquired_process_lease("p", &owner("a", "b"), 1, 11, 10).lease_token,
                straight.lease_token.clone(),
            ),
            (
                acquired_process_lease("q", &owner("a", "b"), 1, 10, 10).lease_token,
                straight.lease_token.clone(),
            ),
        ] {
            assert_ne!(left, right, "every preimage field must move the token");
        }
    }

    #[test]
    fn the_minted_expiry_saturates() {
        let minted = acquired_process_lease("p", &owner("a", "b"), 1, u64::MAX - 1, 10);
        assert_eq!(minted.expires_at_epoch_ms, u64::MAX);
    }

    // --- B1 / B2: terminal and tombstone classification -------------------

    #[test]
    fn a_retained_tombstone_reports_the_pruned_refusal() {
        let error = absent_process_error(
            "process",
            Some(ProcessTombstoneStamp {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 4_242,
            }),
        );
        match error {
            PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => {
                assert_eq!(terminal_label, "completed");
                assert_eq!(pruned_at_ms, 4_242);
            }
            other => panic!("unexpected refusal: {other}"),
        }
    }

    #[test]
    fn an_unknown_process_id_reports_the_unknown_refusal() {
        let error = absent_process_error("process", None);
        assert!(
            matches!(&error, PluginError::Session(message) if message == "unknown process `process`"),
            "unexpected refusal: {error}"
        );
        assert!(
            matches!(unknown_process("other"), PluginError::Session(message) if message == "unknown process `other`")
        );
    }

    #[test]
    fn non_terminal_status_labels_match_the_process_status_partition() {
        let statuses = all_process_statuses();
        assert_eq!(
            statuses.len(),
            6,
            "every ProcessStatus variant must be listed for the partition to be a law"
        );
        let live: Vec<&'static str> = statuses
            .iter()
            .filter(|status| !status.is_terminal())
            .map(ProcessStatus::label)
            .collect();
        assert_eq!(
            live,
            NON_TERMINAL_PROCESS_STATUS_LABELS.to_vec(),
            "the registries' retention SQL is written against exactly these labels"
        );
        let terminal: Vec<&'static str> = statuses
            .iter()
            .filter(|status| status.is_terminal())
            .map(ProcessStatus::label)
            .collect();
        for label in &terminal {
            assert!(
                !NON_TERMINAL_PROCESS_STATUS_LABELS.contains(label),
                "terminal label `{label}` must not be treated as live"
            );
        }
        assert_eq!(
            live.len() + terminal.len(),
            statuses.len(),
            "the partition must be total"
        );
    }

    // --- C1 / C2 / C3: wake reconciliation vocabulary ---------------------

    #[test]
    fn every_wake_delivery_state_round_trips_its_label() {
        for state in [
            WakeDeliveryState::Pending,
            WakeDeliveryState::Enqueuing,
            WakeDeliveryState::Enqueued,
            WakeDeliveryState::Discarded,
        ] {
            assert_eq!(
                wake_delivery_state_from_label("delivery", state.as_str())
                    .expect("a label this crate wrote must parse"),
                state
            );
        }
    }

    #[test]
    fn an_unknown_state_label_is_refused() {
        let error = wake_delivery_state_from_label("delivery", "settled")
            .expect_err("an unrecognised state is never defaulted");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `delivery` has unknown state `settled`"),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn every_discard_reason_round_trips_its_label() {
        for reason in [
            WakeDiscardReason::Expired,
            WakeDiscardReason::TargetGone,
            WakeDiscardReason::Retargeted,
            WakeDiscardReason::SequenceRewound,
        ] {
            assert_eq!(
                wake_discard_reason_from_label("delivery", Some(reason.as_str()))
                    .expect("a label this crate wrote must parse"),
                Some(reason)
            );
        }
        assert_eq!(
            wake_discard_reason_from_label("delivery", None).expect("no reason is not an error"),
            None,
            "a delivery that was never discarded carries no reason"
        );
    }

    #[test]
    fn an_unknown_discard_reason_label_is_refused() {
        let error = wake_discard_reason_from_label("delivery", Some("bored"))
            .expect_err("an unrecognised reason is never defaulted");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `delivery` has unknown discard reason `bored`"),
            "unexpected refusal: {error}"
        );
    }

    fn wake_delivery_json() -> String {
        let wake = super::super::events::ProcessWakeDelivery {
            wake_id: format!("wake:v1:sha256:{}", "a".repeat(64)),
            target_session_id: "session".to_string(),
            process_id: "process".to_string(),
            sequence: 4,
            event_type: "process.wake".to_string(),
            event_invocation: crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new("session"),
                "effect",
                crate::RuntimeEffectKind::Process,
                "replay",
            ),
            process_caused_by: None,
            authority: crate::QueuedWorkAuthority::default(),
            input: "wake".to_string(),
            created_at_ms: 10,
        };
        serde_json::to_string(&wake).expect("encode wake delivery")
    }

    fn wake_row() -> WakeDeliveryRow {
        WakeDeliveryRow {
            delivery_id: format!("wake:v1:sha256:{}", "a".repeat(64)),
            state_label: "enqueuing".to_string(),
            claim_token: Some("claim".to_string()),
            attempts: 2,
            first_attempt_ms: Some(11),
            next_attempt_at_ms: 12,
            expires_at_ms: 13,
            discard_reason_label: None,
            delivery_json: wake_delivery_json(),
        }
    }

    #[test]
    fn a_populated_wake_delivery_row_projects() {
        let row = wake_row();
        let delivery_id = row.delivery_id.clone();
        let delivery = row.project().expect("a well-formed row projects");
        assert_eq!(delivery.delivery_id, delivery_id);
        assert_eq!(delivery.state, WakeDeliveryState::Enqueuing);
        assert_eq!(delivery.claim_token.as_deref(), Some("claim"));
        assert_eq!(delivery.attempts, 2);
        assert_eq!(delivery.first_attempt_ms, Some(11));
        assert_eq!(delivery.next_attempt_at_ms, 12);
        assert_eq!(delivery.expires_at_ms, 13);
        assert_eq!(delivery.discard_reason, None);
        assert_eq!(delivery.wake.sequence, 4);
        assert_eq!(delivery.wake.target_session_id, "session");
    }

    #[test]
    fn a_discarded_wake_delivery_row_projects_its_reason() {
        let delivery = WakeDeliveryRow {
            state_label: "discarded".to_string(),
            discard_reason_label: Some("retargeted".to_string()),
            claim_token: None,
            ..wake_row()
        }
        .project()
        .expect("a discarded row projects");
        assert_eq!(delivery.state, WakeDeliveryState::Discarded);
        assert_eq!(delivery.discard_reason, Some(WakeDiscardReason::Retargeted));
    }

    #[test]
    fn a_corrupt_wake_payload_reports_the_registry_decode_vocabulary() {
        let error = WakeDeliveryRow {
            delivery_json: "{".to_string(),
            ..wake_row()
        }
        .project()
        .expect_err("a corrupt payload is a decode failure");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message.starts_with("failed to decode process registry row: ")),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn an_unknown_state_label_outranks_a_corrupt_payload() {
        let error = WakeDeliveryRow {
            state_label: "settled".to_string(),
            delivery_json: "{".to_string(),
            ..wake_row()
        }
        .project()
        .expect_err("the state label is parsed before the payload is decoded");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `wake:v1:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` has unknown state `settled`"),
            "unexpected refusal: {error}"
        );
    }
}
