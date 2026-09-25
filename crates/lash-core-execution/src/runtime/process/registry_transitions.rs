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

use crate::ProcessId;
use crate::plugin::PluginError;
use crate::store::LeaseOwnerIdentity;

use super::events::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessWakeDelivery};
use super::model::{PROCESS_LEASE_SCHEMA_VERSION, ProcessLease, ProcessRef};
use super::registry::{
    WakeDelivery, WakeDeliveryDisposition, WakeDeliveryState, WakeDiscardReason,
};

/// Failure text for a persisted registry payload that will not decode.
///
/// One vocabulary for both backends: every process-registry row body that fails
/// `serde_json` reports this, so a corrupt payload reads the same whichever
/// substrate stored it.
fn registry_row_decode_error(err: serde_json::Error) -> PluginError {
    PluginError::Session(format!("failed to decode process registry row: {err}"))
}

#[derive(serde::Deserialize)]
struct ProcessWakeDeliveryFormatVersionProbe {
    version: Option<u32>,
}

fn decode_process_wake_delivery(delivery_json: &str) -> Result<ProcessWakeDelivery, PluginError> {
    let probe: ProcessWakeDeliveryFormatVersionProbe =
        serde_json::from_str(delivery_json).map_err(registry_row_decode_error)?;
    let found = probe
        .version
        .unwrap_or(PROCESS_WAKE_DELIVERY_FORMAT_VERSION - 1);
    if found != PROCESS_WAKE_DELIVERY_FORMAT_VERSION {
        return Err(PluginError::ProcessWakeDeliveryFormatVersionMismatch {
            expected: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            found,
        });
    }
    serde_json::from_str(delivery_json).map_err(registry_row_decode_error)
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
    /// The verdict's view of this row for
    /// [`process_lease_verdict`](crate::store::fencing::process_lease_verdict)
    /// (FIG-3388): the holder columns exactly as stored, so the verdict can
    /// distinguish absent, released, superseded and expired rows itself.
    /// Columns that do not fit `u64` read as zero rather than trusted.
    pub fn facts(&self) -> crate::store::fencing::ProcessLeaseFacts<'_> {
        crate::store::fencing::ProcessLeaseFacts {
            lease_owner_id: self.owner_id.as_deref(),
            lease_token: self.lease_token.as_deref(),
            lease_fencing_token: u64::try_from(self.fencing_token).unwrap_or(0),
            lease_expires_at_ms: u64::try_from(self.expires_at_ms).unwrap_or(0),
        }
    }

    /// Project the row into the lease it records, or `None` when it records no
    /// holder.
    ///
    /// Completing or releasing a lease NULLs `lease_owner_id` and `lease_token`
    /// while retaining the monotonic `lease_fencing_token`. So for eligibility a
    /// released lease is indistinguishable from one that never existed — which
    /// is the point — yet the retained counter still fences the next holder
    /// against a stale writer that predates the release
    /// (see [`next_process_lease_fencing_token`]).
    pub fn project(self, process_id: &ProcessId) -> Option<ProcessLease> {
        let (Some(owner_id), Some(lease_token)) = (self.owner_id, self.lease_token) else {
            return None;
        };
        Some(ProcessLease {
            schema_version: PROCESS_LEASE_SCHEMA_VERSION,
            process_id: ProcessId::from(process_id.to_string()),
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
    process_id: &ProcessId,
    claimed: &ProcessLease,
    stored: Option<&ProcessLease>,
    now_ms: u64,
) -> Result<(), PluginError> {
    if claimed.process_id != process_id || !process_lease_still_holds(stored, claimed, now_ms) {
        return Err(PluginError::ProcessLeaseSuperseded {
            process_id: ProcessId::from(process_id.to_string()),
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
/// `blake3("{process_id}:{owner_id}:{incarnation_id}:{claimed_at}:{fencing_token}")`,
/// rendered lower-case hex. A token is minted exactly once at claim time and
/// only ever compared afterwards — no backend re-derives it — so single-homing
/// the mint here (rather than beside each `INSERT`) is about one definition of
/// the durable format, not cross-backend recomputation. The raw concatenated
/// preimage is a deliberate exemption from the `stable_identity` framing
/// doctrine: like `WorkClaimLease::derive` and `LeaseClaimNonce`, the token is
/// an opaque minted capability compared for equality, never a projection of
/// live structure, so serde drift cannot reach it.
pub fn acquired_process_lease(
    process_id: &ProcessId,
    owner: &LeaseOwnerIdentity,
    fencing_token: u64,
    now_ms: u64,
    lease_ttl_ms: u64,
) -> ProcessLease {
    ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: ProcessId::from(process_id.to_string()),
        owner: owner.clone(),
        lease_token: crate::stable_hash::blake3_hex(
            "lash-process-lease/v2",
            format!(
                "{process_id}:{}:{}:{now_ms}:{fencing_token}",
                owner.owner_id, owner.incarnation_id
            )
            .as_bytes(),
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

/// Refusal for a durable reference whose reusable name now identifies another
/// process lifetime.
pub fn process_incarnation_superseded(
    requested: &ProcessRef,
    current_incarnation: super::model::ProcessIncarnation,
) -> PluginError {
    PluginError::ProcessIncarnationSuperseded {
        process_id: requested.process_id.clone(),
        requested_incarnation: requested.incarnation,
        current_incarnation,
    }
}

/// Refusal for a process id no registry ever knew.
pub fn unknown_process(process_id: &ProcessId) -> PluginError {
    PluginError::ProcessUnknown {
        process_id: ProcessId::from(process_id.to_string()),
    }
}

/// Classify a lookup that found no live process row.
///
/// The three-way split — retained row, retained tombstone, never known — is
/// what lets a host tell "this finished and was reaped" from "this id is
/// wrong", so the two absent cases must never collapse into one error.
pub fn absent_process_error(
    process_id: &ProcessId,
    tombstone: Option<ProcessTombstoneStamp>,
) -> PluginError {
    match tombstone {
        Some(stamp) => process_no_longer_retained(stamp),
        None => unknown_process(process_id),
    }
}

/// The `status` column labels a process row carries while it is still live —
/// that is, while lash may still act on it.
///
/// Both registries' retention SQL is written against exactly this set —
/// `status IN ('running', 'waiting')` selects live rows, `status NOT IN (…)`
/// selects retention candidates. The set is a constant rather than a query
/// fragment on purpose: the registries keep their literal SQL, and
/// `process_status_labels_partition_live_from_retired` is what fails if a new
/// variant would silently land on the wrong side.
///
/// Live is **not** the complement of terminal.
/// [`ProcessStatus::CallerDeparted`](crate::ProcessStatus::CallerDeparted) is
/// neither: lash may never act on such a row and may never assert an outcome
/// for it, so it is excluded here (recovery must not pick it up) and included
/// in [`RETIRED_PROCESS_STATUS_LABELS`] (retention may reclaim it).
pub const LIVE_PROCESS_STATUS_LABELS: [&str; 2] = ["running", "waiting"];

/// The `status` column labels retention may reclaim, i.e. the exact complement
/// of [`LIVE_PROCESS_STATUS_LABELS`] that both registries' prune SQL selects
/// with `status NOT IN ('running', 'waiting')`.
///
/// Reclaiming a row is a retention act, never an outcome claim, which is why
/// the non-terminal `caller_departed` label belongs here: nothing may ever
/// honestly terminalize such a row, so excluding it would let a host
/// accumulate unresolvable rows without bound.
pub const RETIRED_PROCESS_STATUS_LABELS: [&str; 5] = [
    "completed",
    "failed",
    "cancelled",
    "abandoned",
    "caller_departed",
];

// ---------------------------------------------------------------------------
// Wake reconciliation vocabulary
// ---------------------------------------------------------------------------

/// Refusal for a wake-delivery id with no row.
pub fn unknown_wake_delivery(delivery_id: &str) -> PluginError {
    PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
}

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
    pub state_label: String,
    /// `claim_token`, the ownership fence of the current `enqueuing` claim.
    pub claim_token: Option<String>,
    pub attempts: i64,
    /// `first_attempt_ms`.
    pub first_attempt_ms: Option<i64>,
    /// `next_attempt_at_ms`.
    pub next_attempt_at_ms: i64,
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
        let disposition = match (state, self.claim_token) {
            (WakeDeliveryState::Pending, _) => WakeDeliveryDisposition::Pending,
            (WakeDeliveryState::Enqueuing, Some(claim_token)) => {
                WakeDeliveryDisposition::Enqueuing { claim_token }
            }
            (WakeDeliveryState::Enqueuing, None) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is enqueuing without a claim token",
                    self.delivery_id
                )));
            }
            (WakeDeliveryState::Enqueued, _) => WakeDeliveryDisposition::Enqueued,
            (WakeDeliveryState::Discarded, _) => match discard_reason {
                Some(reason) => WakeDeliveryDisposition::Discarded { reason },
                None => WakeDeliveryDisposition::DiscardedUnattributed,
            },
        };
        let wake = decode_process_wake_delivery(&self.delivery_json)?;
        Ok(WakeDelivery {
            delivery_id: self.delivery_id,
            wake,
            disposition,
            attempts: self.attempts as u64,
            first_attempt_ms: self.first_attempt_ms.map(|value| value as u64),
            next_attempt_at_ms: self.next_attempt_at_ms as u64,
            expires_at_ms: self.expires_at_ms as u64,
        })
    }
}
