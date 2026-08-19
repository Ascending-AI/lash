//! The durable effect-replay state machine shared by every SQL backend.
//!
//! Runtime effects are journaled: the first worker to reach a
//! `(scope_id, replay_key)` pair claims it under a fenced lease, executes it
//! once, and records the terminal outcome; every later arrival replays that
//! record instead of executing again. That is the exactly-once contract, and
//! the two SQL stores were each carrying a full copy of it.
//!
//! This module owns the copy. [`EffectReplayDriver`] runs the whole
//! claim/execute/renew/finalize loop, decodes and encodes the journal payloads,
//! maps controller errors, sleeps for `Sleep` effects and busy retries, and
//! forwards the [`AwaitEventResolver`](super::executor::AwaitEventResolver)
//! surface to the shared [`AwaitEventCoordinator`]. Backends implement only
//! [`EffectReplayPersistence`]: four atomic row operations plus whatever
//! transaction and locking mechanics their substrate needs to make each one
//! atomic.
//!
//! # Transition authority
//!
//! No backend decides whether a row is claimable. [`decide_effect_claim`] is a
//! pure function over the observed row, and it is the *only* place that reads a
//! status column, compares an envelope hash, or judges a lease expired. A
//! backend reads the row, asks the table, and applies the write the table
//! prescribes — so changing the state machine changes one function, and the
//! law tests below are what break if a backend stops honoring it.
//!
//! # Two clocks, on purpose
//!
//! Effect leases fence work across hosts, so the instant that stamps and
//! compares a lease must be authoritative for the substrate, not for whichever
//! host happens to run the claim. That instant is
//! [`EffectReplayPersistence::claim`]'s to read, and each backend reads its own
//! (SQLite: the host's injected [`Clock`](crate::Clock), the same domain its
//! rows already live in; PostgreSQL: `transaction_timestamp()`, per the
//! [`Clock`](crate::Clock) contract's database-authoritative lease boundary,
//! pinned by `postgres_clock_contract`).
//! The driver's own [`Clock`](crate::Clock) never stamps a row and never
//! decides a lease: it only sleeps — `Sleep` effect due times, busy-retry
//! backoff, and the lease renewal interval.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::await_event_coordinator::{AwaitEventBackend, AwaitEventCoordinator};
use super::envelope::{RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectOutcome};

use super::executor::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectJournalRetirement, ExecutionScope, Resolution,
    ResolveOutcome, RuntimeEffectControllerError, RuntimeEffectLocalExecutor,
};
use super::group_drain::GroupExecutors;
/// The durable group shape this port's backends implement, re-exported so a
/// backend imports the whole effect-journal vocabulary from one place.
pub use super::group_journal::{
    EffectFinalizeOutcome, EffectGroupColumn, EffectGroupRecord, StoredGroupSettlement,
    UnsettledGroupChild,
};
use super::validation::{CanonicalRuntimeEffectEnvelope, validate_replayed_effect_envelope};
use crate::store::LeaseTimings;

/// Delay between polls while another owner holds a live claim.
const BUSY_POLL: Duration = Duration::from_millis(25);

/// Process-wide sequence making each driver's owner id distinct.
static EFFECT_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Backend-specific error vocabulary for driver-owned failures.
///
/// Hosts match on `RuntimeEffectControllerError::code`, so each backend keeps
/// the codes it shipped: `{code_prefix}_effect_replay_{suffix}`. Substrate
/// failures stay in the backend, which owns its own `_store` mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectReplayVocabulary {
    backend: EffectReplayBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectReplayBackend {
    Sqlite,
    Postgres,
}

/// What a caller of the shared claim loop wants a live competing claim to mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusyPolicy {
    /// Wait the competing claim out. What an effect's own caller wants: it
    /// needs this outcome, and the other executor is producing it.
    Queue,
    /// Report the busy claim and hand the decision back. What the drain wants:
    /// it has a queue, and a child someone else owns right now is the one it
    /// should move past rather than sleep against.
    Yield,
}

/// How one trip through the claim loop ended.
enum EffectRun {
    /// The effect reached a terminal — replayed, or executed and finalized.
    Terminal(RuntimeEffectOutcome),
    /// Another executor holds a live claim, and the caller asked to be told
    /// rather than made to wait. Nothing was written.
    Busy,
}

enum EffectReplayFailure {
    CorruptRow,
    Decode,
    Encode,
    HashConflict,
    KeyMissing,
    LeaseLost,
    Missing,
    Store,
}

impl EffectReplayVocabulary {
    pub const fn sqlite() -> Self {
        Self {
            backend: EffectReplayBackend::Sqlite,
        }
    }

    pub const fn postgres() -> Self {
        Self {
            backend: EffectReplayBackend::Postgres,
        }
    }

    pub fn store_code(&self) -> RuntimeErrorCode {
        self.code(EffectReplayFailure::Store)
    }

    fn code(&self, failure: EffectReplayFailure) -> RuntimeErrorCode {
        match (self.backend, failure) {
            (EffectReplayBackend::Sqlite, EffectReplayFailure::CorruptRow) => {
                RuntimeErrorCode::SqliteEffectReplayCorruptRow
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::Decode) => {
                RuntimeErrorCode::SqliteEffectReplayDecode
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::Encode) => {
                RuntimeErrorCode::SqliteEffectReplayEncode
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::HashConflict) => {
                RuntimeErrorCode::SqliteEffectReplayHashConflict
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::KeyMissing) => {
                RuntimeErrorCode::SqliteEffectReplayKeyMissing
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::LeaseLost) => {
                RuntimeErrorCode::SqliteEffectReplayLeaseLost
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::Missing) => {
                RuntimeErrorCode::SqliteEffectReplayMissing
            }
            (EffectReplayBackend::Sqlite, EffectReplayFailure::Store) => {
                RuntimeErrorCode::SqliteEffectReplayStore
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::CorruptRow) => {
                RuntimeErrorCode::PostgresEffectReplayCorruptRow
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::Decode) => {
                RuntimeErrorCode::PostgresEffectReplayDecode
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::Encode) => {
                RuntimeErrorCode::PostgresEffectReplayEncode
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::HashConflict) => {
                RuntimeErrorCode::PostgresEffectReplayHashConflict
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::KeyMissing) => {
                RuntimeErrorCode::PostgresEffectReplayKeyMissing
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::LeaseLost) => {
                RuntimeErrorCode::PostgresEffectReplayLeaseLost
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::Missing) => {
                RuntimeErrorCode::PostgresEffectReplayMissing
            }
            (EffectReplayBackend::Postgres, EffectReplayFailure::Store) => {
                RuntimeErrorCode::PostgresEffectReplayStore
            }
        }
    }

    fn error(
        &self,
        failure: EffectReplayFailure,
        message: impl Into<String>,
    ) -> RuntimeEffectControllerError {
        RuntimeEffectControllerError::new(self.code(failure), message)
    }

    fn encode_error(&self, err: serde_json::Error) -> RuntimeEffectControllerError {
        self.error(
            EffectReplayFailure::Encode,
            format!("failed to encode runtime effect replay row: {err}"),
        )
    }

    fn decode_error(&self, err: serde_json::Error) -> RuntimeEffectControllerError {
        self.error(
            EffectReplayFailure::Decode,
            format!("failed to decode runtime effect replay row: {err}"),
        )
    }
}

/// The persisted `status` column of an effect-replay row.
///
/// The column stores these exact strings; they are journal bytes, so the
/// mapping lives here once rather than as literals in each backend's SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectRowStatus {
    /// A lease is held and the effect has not produced a terminal yet.
    InProgress,
    /// The effect completed and `outcome_json` is authoritative.
    Completed,
    /// The effect failed and `error_json` is authoritative.
    Failed,
}

impl EffectRowStatus {
    /// The persisted column value.
    pub fn column(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(status: &str) -> Option<Self> {
        match status {
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Everything a backend needs to claim `(scope_id, replay_key)`.
///
/// The request carries the *sleep duration* rather than a due timestamp: the
/// due time is derived from the same authoritative instant that stamps the
/// lease, which the backend reads inside its own transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectClaimRequest {
    /// Durable journal identity of the executing scope.
    pub scope_id: String,
    /// Owning session, when the scope has one. `NULL` rows are session-free.
    pub session_id: Option<String>,
    /// Replay key, unique within `scope_id`.
    pub replay_key: String,
    /// Canonical envelope hash, the replay identity of this effect.
    pub envelope_hash: String,
    /// Canonical envelope JSON, persisted so a mismatch can be diagnosed.
    pub envelope_json: String,
    /// This driver's owner id, one half of the lease fence.
    pub owner_id: String,
    /// A fresh lease token, the other half of the lease fence.
    pub lease_token: String,
    /// Lease lifetime, added to the claim instant to form the expiry.
    pub lease_ttl_ms: u64,
    /// `Some` only for `Sleep` effects: how long past the claim instant the
    /// effect is due.
    pub sleep_duration_ms: Option<u64>,
    /// `Some` only for a child of a durable effect group: the group whose
    /// counter this child's finalize will allocate a settlement rank from
    /// (FIG-1416).
    ///
    /// Derived from the envelope's own
    /// [`EffectGroupMembership`](super::group::EffectGroupMembership), never
    /// passed alongside it, so a child's row cannot record a group its
    /// canonical envelope does not hash. It needs no separate claim-time check:
    /// membership is inside the hash, so a row whose `envelope_hash` matches
    /// necessarily agrees about the group, and a row whose hash disagrees is
    /// already refused as a [replay
    /// mismatch](EffectClaimObservation::ReplayMismatch) before any status is
    /// read.
    pub group_key: Option<String>,
    /// Strict replay: a missing row is an error instead of a fresh claim.
    pub strict_replay: bool,
}

/// The effect-replay row as persisted, projected for [`decide_effect_claim`].
///
/// The projection deliberately omits `lease_owner_id` and `lease_token`: who
/// holds a lease is not a claimability input, only *whether* it is still live
/// is. Identity is enforced where it belongs, on the compare-and-set that every
/// guarded write performs against [`EffectLeaseFence`] (ADR 0029).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEffectRow {
    /// Recorded canonical envelope hash.
    pub envelope_hash: String,
    /// Recorded canonical envelope JSON.
    pub envelope_json: String,
    /// Raw `status` column; an unrecognized value is a corrupt row.
    pub status: String,
    /// Recorded success outcome, present when `status` is `completed`.
    pub outcome_json: Option<String>,
    /// Recorded failure, present when `status` is `failed`.
    pub error_json: Option<String>,
    /// Lease expiry of the current claim, `0` once finalized.
    pub lease_expires_at_ms: u64,
    /// Recorded due time for a `Sleep` effect.
    pub due_at_ms: Option<u64>,
}

/// The lease write [`decide_effect_claim`] prescribes for a claim attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectLeaseStamp {
    /// Lease expiry to persist: the claim instant plus the requested TTL.
    pub lease_expires_at_ms: u64,
    /// Due time to persist. A takeover keeps the recorded due time so a
    /// half-slept `Sleep` effect is not restarted from zero.
    pub due_at_ms: Option<u64>,
    /// The claim instant itself, for the row's `created_at_ms`/`updated_at_ms`.
    pub now_ms: u64,
}

/// What a backend must do with the row it just read under its claim fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectClaimDecision {
    /// No row exists: insert a fresh `in_progress` row from the request and
    /// this stamp, then report [`EffectClaimObservation::Claimed`].
    Insert(EffectLeaseStamp),
    /// An `in_progress` row's lease has expired: overwrite its lease owner,
    /// token, expiry and due time from the request and this stamp, then report
    /// [`EffectClaimObservation::Claimed`].
    TakeOver(EffectLeaseStamp),
    /// Write nothing; report this observation to the driver.
    Report(EffectClaimObservation),
}

/// What a claim attempt observed. Backends return this from
/// [`EffectReplayPersistence::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectClaimObservation {
    /// This driver now holds the lease.
    Claimed {
        /// The due time persisted for the claim, `None` for non-`Sleep`
        /// effects.
        due_at_ms: Option<u64>,
    },
    /// A row exists for this replay key under a different canonical envelope.
    ReplayMismatch {
        /// The recorded canonical envelope JSON, for diagnosis.
        recorded_envelope_json: String,
        /// The recorded hash that disagreed.
        stored_envelope_hash: String,
    },
    /// The effect already completed; replay its outcome.
    Completed {
        /// The recorded success outcome.
        outcome_json: String,
        /// The recorded due time, so a replayed `Sleep` still sleeps.
        due_at_ms: Option<u64>,
    },
    /// The effect already failed; replay its error.
    Failed {
        /// The recorded failure.
        error_json: String,
    },
    /// Another owner holds a live lease.
    Busy {
        /// When that lease expires; the driver retries no sooner.
        retry_at_ms: u64,
    },
    /// Strict replay found no recorded effect.
    StrictReplayMiss,
    /// The row cannot be interpreted.
    CorruptRow {
        /// Which invariant the row broke.
        defect: EffectRowDefect,
    },
}

/// Why an effect-replay row could not be interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectRowDefect {
    /// `status` is `completed` but `outcome_json` is `NULL`.
    MissingOutcome,
    /// `status` is `failed` but `error_json` is `NULL`.
    MissingError,
    /// `status` holds a value no version of this runtime writes.
    UnknownStatus {
        /// The unrecognized column value.
        status: String,
    },
    /// The backend's claim mechanics saw the row appear and then vanish.
    ///
    /// Reachable only on substrates whose claim is not a single serialized
    /// write (PostgreSQL's insert-on-conflict retry); SQLite's
    /// `BEGIN IMMEDIATE` cannot produce it.
    VanishedUnderClaim,
}

impl EffectRowDefect {
    fn message(&self) -> String {
        match self {
            Self::MissingOutcome => {
                "completed runtime effect row is missing outcome_json".to_string()
            }
            Self::MissingError => "failed runtime effect row is missing error_json".to_string(),
            Self::UnknownStatus { status } => {
                format!("unknown runtime effect replay status `{status}`")
            }
            Self::VanishedUnderClaim => {
                "effect replay insert conflicted but no row could be selected".to_string()
            }
        }
    }
}

/// The five columns that fence a claim.
///
/// Every guarded write matches all five: a lease is this driver's only if the
/// scope, replay key, canonical envelope hash, owner id and lease token all
/// still agree with what the claim recorded (ADR 0029 — the compare-and-set on
/// commit is the authority, the lease row is only its record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectLeaseFence {
    /// Durable journal identity of the executing scope.
    pub scope_id: String,
    /// Replay key, unique within `scope_id`.
    pub replay_key: String,
    /// Canonical envelope hash recorded by the claim.
    pub envelope_hash: String,
    /// Owner id recorded by the claim.
    pub owner_id: String,
    /// Lease token recorded by the claim.
    pub lease_token: String,
}

/// The terminal an effect produced, ready to journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectTerminal {
    /// The effect succeeded.
    Completed {
        /// Encoded [`RuntimeEffectOutcome`].
        outcome_json: String,
    },
    /// The effect failed.
    Failed {
        /// Encoded [`RuntimeEffectControllerError`].
        error_json: String,
    },
}

impl EffectTerminal {
    /// The `status` column this terminal writes.
    pub fn status(&self) -> EffectRowStatus {
        match self {
            Self::Completed { .. } => EffectRowStatus::Completed,
            Self::Failed { .. } => EffectRowStatus::Failed,
        }
    }

    /// The `outcome_json` column this terminal writes.
    pub fn outcome_json(&self) -> Option<&str> {
        match self {
            Self::Completed { outcome_json } => Some(outcome_json),
            Self::Failed { .. } => None,
        }
    }

    /// The `error_json` column this terminal writes.
    pub fn error_json(&self) -> Option<&str> {
        match self {
            Self::Completed { .. } => None,
            Self::Failed { error_json } => Some(error_json),
        }
    }
}

/// Decide what a claim attempt should do with the row it observed.
///
/// This is the effect-replay transition table: pure, backend-independent, and
/// the single authority over claimability. `now_ms` is the substrate's
/// authoritative claim instant — the same instant that will stamp the row.
pub fn decide_effect_claim(
    row: Option<&StoredEffectRow>,
    request: &EffectClaimRequest,
    now_ms: u64,
) -> EffectClaimDecision {
    let fresh_due_at_ms = request
        .sleep_duration_ms
        .map(|duration_ms| now_ms.saturating_add(duration_ms));
    let stamp = |due_at_ms: Option<u64>| EffectLeaseStamp {
        lease_expires_at_ms: now_ms.saturating_add(request.lease_ttl_ms),
        due_at_ms,
        now_ms,
    };

    let Some(row) = row else {
        if request.strict_replay {
            return EffectClaimDecision::Report(EffectClaimObservation::StrictReplayMiss);
        }
        return EffectClaimDecision::Insert(stamp(fresh_due_at_ms));
    };

    if row.envelope_hash != request.envelope_hash {
        return EffectClaimDecision::Report(EffectClaimObservation::ReplayMismatch {
            recorded_envelope_json: row.envelope_json.clone(),
            stored_envelope_hash: row.envelope_hash.clone(),
        });
    }

    match EffectRowStatus::parse(&row.status) {
        Some(EffectRowStatus::Completed) => {
            let Some(outcome_json) = row.outcome_json.clone() else {
                return EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
                    defect: EffectRowDefect::MissingOutcome,
                });
            };
            EffectClaimDecision::Report(EffectClaimObservation::Completed {
                outcome_json,
                due_at_ms: row.due_at_ms,
            })
        }
        Some(EffectRowStatus::Failed) => {
            let Some(error_json) = row.error_json.clone() else {
                return EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
                    defect: EffectRowDefect::MissingError,
                });
            };
            EffectClaimDecision::Report(EffectClaimObservation::Failed { error_json })
        }
        Some(EffectRowStatus::InProgress) if row.lease_expires_at_ms > now_ms => {
            EffectClaimDecision::Report(EffectClaimObservation::Busy {
                retry_at_ms: row.lease_expires_at_ms,
            })
        }
        Some(EffectRowStatus::InProgress) => {
            EffectClaimDecision::TakeOver(stamp(row.due_at_ms.or(fresh_due_at_ms)))
        }
        None => EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
            defect: EffectRowDefect::UnknownStatus {
                status: row.status.clone(),
            },
        }),
    }
}

/// The seal on [`EffectReplayPersistence`].
///
/// Effect journaling is not an extension point. lash's own SQL stores are the
/// only intended implementors of the port; a durable substrate that owns its
/// own journal (Restate, Temporal) implements the *effect-host contract*
/// instead and never sees this trait. So the port carries a supertrait whose
/// only purpose is to be named: nothing outside lash's stores has a reason to
/// write [`EffectReplayBackend`](sealed::EffectReplayBackend) for its type, and
/// writing it is the acknowledgement that the resulting exactly-once behavior
/// is unsupported and unrefereed.
///
/// The seal is a marker rather than a wall, because Rust has no visibility that
/// admits a sibling crate and excludes a foreign one — the two adapters live in
/// `lash-sqlite-store` and `lash-postgres-store`, so a crate-private supertrait
/// would exclude them too. What the seal buys is that the backends-only intent
/// is in the type system instead of only in prose.
#[doc(hidden)]
pub mod sealed {
    /// Marker every [`EffectReplayPersistence`](super::EffectReplayPersistence)
    /// implementation must also carry. See [the seal](super::sealed).
    pub trait EffectReplayBackend {}
}

/// Atomic row operations a durable substrate must provide to journal effects.
///
/// Each method is one atomic unit: the backend takes whatever transaction and
/// lock it needs (SQLite's `BEGIN IMMEDIATE` write lock, PostgreSQL's
/// `SELECT … FOR UPDATE` in a server transaction) so the read, the decision,
/// and the write it guards cannot interleave with a competing claimant.
///
/// No method decides claimability, encodes or decodes a journal payload, or
/// sleeps.
///
/// The trait is sealed behind [`sealed::EffectReplayBackend`]: only lash's own
/// SQL stores implement it, and the seal says so.
#[async_trait]
pub trait EffectReplayPersistence: sealed::EffectReplayBackend + Send + Sync {
    /// The error vocabulary hosts already match on for this backend.
    fn vocabulary(&self) -> EffectReplayVocabulary;

    /// Claim `(scope_id, replay_key)`, or report why it could not be claimed.
    ///
    /// Atomically: read the row for the request's scope and replay key; read
    /// `now_ms` from the substrate's authoritative lease clock; ask
    /// [`decide_effect_claim`]; apply the prescribed write for
    /// [`EffectClaimDecision::Insert`] / [`EffectClaimDecision::TakeOver`] and
    /// report [`EffectClaimObservation::Claimed`] with the stamp's due time;
    /// write nothing for [`EffectClaimDecision::Report`] and return its
    /// observation. A committed transaction that reports an observation is
    /// correct: the observations describe reads, not failures.
    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError>;

    /// Write `terminal` and release the lease, guarded by `fence`, allocating a
    /// settlement rank when the row belongs to a durable effect group.
    ///
    /// Atomically, and only while the row still matches all five fence columns,
    /// is `in_progress`, and has not expired against the substrate's lease
    /// clock: set the terminal's status and payload column, clear the lease
    /// owner and token, and zero the lease expiry. Report
    /// [`EffectFinalizeOutcome::FenceMoved`] when the guarded write matched no
    /// row — the fence moved and this driver no longer owns the effect.
    ///
    /// # Normative ordering (N1)
    ///
    /// One transaction, in this order:
    ///
    /// 1. Perform the fenced `UPDATE` above.
    /// 2. **If its rowcount is 0: roll back and report `FenceMoved`.** No
    ///    counter bump.
    /// 3. Only on rowcount 1, and only when the row records a `group_key`:
    ///    `UPDATE lash_runtime_effect_group SET next_seq = next_seq + 1 WHERE
    ///    group_key = $g RETURNING next_seq`, and write the returned value into
    ///    this child's `settlement_seq`.
    /// 4. Commit, reporting [`EffectFinalizeOutcome::Written`] with the rank.
    ///
    /// The group is read from the child's own row rather than passed in, so a
    /// finalize cannot bump a group the row does not belong to. Bumping before
    /// the fenced write, or bumping unconditionally and committing while
    /// reporting the miss, lets a driver whose lease was taken over permanently
    /// advance a live group's counter — and the `UNIQUE (group_key,
    /// settlement_seq)` index cannot catch it, because the burned number never
    /// reaches a child row. See [`EffectFinalizeOutcome`] for the full argument,
    /// and the `fence-miss-allocates-nothing` conformance test in each store
    /// crate for what holds a backend to it.
    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<EffectFinalizeOutcome, RuntimeEffectControllerError>;

    /// Record `record` as an open durable effect group, idempotently.
    ///
    /// **In its own transaction, committed before any of the group's children
    /// claim** (N2): the open path must never hold a group-row lock while
    /// acquiring a child-row lock, because [`finalize`](Self::finalize) takes
    /// them the other way round and the two together would be an ABBA deadlock.
    ///
    /// Idempotent because open is replayed: a redriven caller reopens the group
    /// it already opened, and re-inserting must neither fail nor reset
    /// `next_seq` — resetting it would re-seat already-recorded children at
    /// ranks another caller has consumed. An existing row for the same key is
    /// left exactly as it is.
    ///
    /// Returns the record **as it stands durably** after the idempotent write:
    /// the row that was already there when the key existed, and `record` itself
    /// when it did not. That is what makes the reopen fence the group host owes
    /// (`open_effect_group`'s "refuse rather than reopen a group whose child
    /// count or wake rule differs") a *durable* check rather than an in-process
    /// one. The per-child envelope-hash fence already refuses a drifted wake
    /// rule or disposition, since both are folded into every child's hash — but
    /// it cannot see a **shrunk child vec**, which silently renumbers every rank
    /// above the truncation, and it cannot see anything at all from a process
    /// that never opened the group before.
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError>;

    /// Read the group row under `group_key`, or `None` when no group is
    /// recorded there.
    ///
    /// The read half of [`open_group`](Self::open_group), for the one reader
    /// that must not write: the group drain takes its queue from the journal
    /// rather than from a caller, and the disposition it applies is the one the
    /// group declared. Asking `open_group` for it would *insert* a group row for
    /// a key that has none — inventing the very fact the drain is forbidden to
    /// invent.
    ///
    /// `None` is a real answer, not an error: a group whose row was retired
    /// (N3) is gone whole, children included, and a drain that finds no row has
    /// nothing to drain.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError>;

    /// Read the group's settled child at `rank`, counting from 1.
    ///
    /// Rank is the position of a child's `settlement_seq` in the ascending order
    /// of the group's recorded sequences — never a lookup by literal sequence
    /// value, which gaps would break. `None` means fewer than `rank` children
    /// have settled yet.
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: usize,
    ) -> Result<Option<StoredGroupSettlement>, RuntimeEffectControllerError>;

    /// Read every child of `group_key` that holds no settlement rank.
    ///
    /// The exact complement of [`read_group_settlement`](Self::read_group_settlement):
    /// that read filters `settlement_seq IS NOT NULL`, this one
    /// `settlement_seq IS NULL`. Both predicates over one table, so a child is
    /// in exactly one of the two answers and "the group is complete" is
    /// decidable in a single query rather than by walking ranks until one comes
    /// back `None`.
    ///
    /// Order is unspecified beyond being stable for one journal state: these
    /// rows have no rank — that is what makes them unsettled — and imposing one
    /// would invent an ordering fact the journal does not hold.
    async fn read_unsettled_group_children(
        &self,
        group_key: &str,
    ) -> Result<Vec<UnsettledGroupChild>, RuntimeEffectControllerError>;

    /// Extend the lease by `lease_ttl_ms`, guarded by `fence`.
    ///
    /// Same guard as [`finalize`](EffectReplayPersistence::finalize); the new
    /// expiry is the substrate's lease clock plus `lease_ttl_ms`. Report
    /// `false` when the guarded write matched no row.
    async fn renew(
        &self,
        fence: &EffectLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<bool, RuntimeEffectControllerError>;

    /// Delete the journal rows `retirement` names, reporting how many went.
    ///
    /// **Group-atomic** (N3): a group's own row and every one of its children go
    /// in the same transaction, so no partially-retired group is ever visible.
    /// Rank counts a group's recorded children, and it is stable only because
    /// allocation is monotonic and therefore appends *above* any consumed rank;
    /// a deletion *below* a consumed rank would shift ranks even though
    /// allocation never does. The count reports children, matching what the
    /// method has always reported.
    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError>;
}

/// A claim this driver holds: the fence plus the due time it recorded.
struct ClaimedEffect {
    fence: EffectLeaseFence,
    due_at_ms: Option<u64>,
}

/// What a prepared claim attempt resolved to, after decoding.
enum PreparedEffect {
    ReplayMismatch {
        recorded_envelope: Box<CanonicalRuntimeEffectEnvelope>,
        stored_envelope_hash: String,
    },
    ReplayOutcome {
        outcome: Box<RuntimeEffectOutcome>,
        due_at_ms: Option<u64>,
    },
    ReplayError(RuntimeEffectControllerError),
    Claimed(ClaimedEffect),
    Busy {
        retry_at_ms: u64,
    },
}

/// The durable effect-replay state machine, shared by every SQL backend.
///
/// One driver instance is one host object: it owns that host's owner id, lease
/// counter, replay mode, and the [`AwaitEventCoordinator`] its effect commands
/// resolve promises through. Stores wrap it in an `Arc` and hand the same
/// driver to their effect host and to every scoped controller the host mints,
/// so all of them share one lease identity.
pub struct EffectReplayDriver<P, A> {
    persistence: P,
    await_events: AwaitEventCoordinator<A>,
    clock: Arc<dyn crate::Clock>,
    owner_id: String,
    lease_counter: AtomicU64,
    replay_mode: AtomicBool,
    lease_timings: LeaseTimings,
    /// The groups this driver has open, and the host-owned task set their
    /// children run on. Process-local by design: every durable fact about a
    /// group lives in the journal, and this map holds only what a process that
    /// opened the group knows — which child is at which position, and the
    /// cancellation token its own children select on.
    groups: groups::DurableEffectGroups,
    /// This host's one answer to "what code runs a journaled grouped child".
    ///
    /// Registered once, by the host that owns the runners, and read by every
    /// path that has to execute a child: the open, a retry, and the loser drain.
    /// Absent until a host registers one, which is the same thing as this host
    /// not supporting effect groups — see
    /// [`register_group_executors`](EffectReplayDriver::register_group_executors).
    group_executors: OnceLock<Arc<dyn GroupExecutors>>,
}

impl<P: EffectReplayPersistence, A: AwaitEventBackend> EffectReplayDriver<P, A> {
    /// Build a driver over `persistence`.
    ///
    /// `clock` is the driver's *sleep* clock: it times `Sleep` effects, the
    /// busy-retry backoff, and the lease renewal interval, and it never stamps
    /// a row or decides a lease — the substrate's own lease clock does that
    /// inside [`EffectReplayPersistence::claim`]. Pass the host's injected
    /// clock when the substrate shares the host's clock domain (SQLite), and an
    /// explicit [`SystemClock`](crate::facade_support::SystemClock) when it does
    /// not (PostgreSQL, whose lease decisions are server-side per the
    /// [`Clock`](crate::Clock) contract, pinned by `postgres_clock_contract`).
    pub fn new(
        persistence: P,
        await_events: AwaitEventCoordinator<A>,
        clock: Arc<dyn crate::Clock>,
        lease_timings: LeaseTimings,
    ) -> Self {
        let sequence = EFFECT_OWNER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let owner_id = format!(
            "pid{}-{sequence}-{}",
            std::process::id(),
            clock.timestamp_ms()
        );
        Self {
            persistence,
            await_events,
            clock,
            owner_id,
            lease_counter: AtomicU64::new(1),
            replay_mode: AtomicBool::new(false),
            lease_timings,
            groups: groups::DurableEffectGroups::default(),
            group_executors: OnceLock::new(),
        }
    }

    /// Register this host's envelope→executor resolver, once.
    ///
    /// One host has one answer to "what code runs this journaled grouped child",
    /// so this is set once and then read by the open, by a retry, and by the
    /// loser drain alike. A second registration of a *different* resolver is
    /// refused rather than allowed to win: two resolvers on one journal means two
    /// answers for one child, and which one a given path got would depend on when
    /// it asked. Re-registering the resolver already held is a no-op, so a host
    /// handed out repeatedly need not track whether it has been wired yet.
    ///
    /// Until it is called, this host does not support effect groups — the
    /// `'static` executors a grouped child needs to outlive its caller have
    /// nowhere to come from — so
    /// [`supports_effect_groups`](EffectReplayDriver::supports_effect_groups)
    /// answers `false` and all three group methods refuse with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// rather than journaling a group nothing can run.
    ///
    /// [`OnceLock::set`] is the arbiter rather than a preceding `get`: a
    /// get-then-set pair leaves a window in which two threads both read `None`,
    /// both write, and the loser is told `Ok` while its resolver was dropped on
    /// the floor — the exact drift this refusal exists to prevent. `set` decides,
    /// and its `Err` hands back the rejected resolver so the same-resolver case
    /// stays a no-op.
    pub fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Err(rejected) = self.group_executors.set(executors) else {
            return Ok(());
        };
        let held = self
            .group_executors
            .get()
            .expect("a rejected set means the lock is initialized");
        if Arc::ptr_eq(held, &rejected) {
            Ok(())
        } else {
            Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "this effect host already has a different registered group \
                 executor resolver; one host has one answer to what runs a \
                 journaled grouped child, and a second answer would make which \
                 one a path got depend on when it asked",
            ))
        }
    }

    /// Whether a resolver has been registered, which is exactly whether this
    /// host supports effect groups.
    ///
    /// A **per-deployment** fact established at wiring time: deployment
    /// validation reads it after the registration, and before wiring it reads
    /// `false` and the three group methods refuse coherently with it.
    pub fn supports_effect_groups(&self) -> bool {
        self.group_executors.get().is_some()
    }

    /// The registered resolver, or the refusal that says this host does not
    /// implement groups at all.
    ///
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// not a shape refusal: an unwired host is not a host with a bad group, it is
    /// the host `supports_effect_groups` answers `false` for, and the coherence
    /// law binds the flag to all three methods. A *per-child* resolver miss on a
    /// wired host is the other fact and keeps its typed routing refusal.
    pub(super) fn group_executors(
        &self,
    ) -> Result<&Arc<dyn GroupExecutors>, RuntimeEffectControllerError> {
        self.group_executors.get().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EffectGroupUnsupported,
                "this effect host has no registered group executor resolver, so \
                 it does not implement durable effect groups; register one with \
                 register_group_executors at wiring time",
            )
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.replay_mode.store(true, Ordering::SeqCst);
    }

    fn vocabulary(&self) -> EffectReplayVocabulary {
        self.persistence.vocabulary()
    }

    fn next_lease_token(&self) -> String {
        let sequence = self.lease_counter.fetch_add(1, Ordering::SeqCst);
        format!("{}:{sequence}", self.owner_id)
    }

    /// Mint the authenticated await-event key for `scope`/`wait`.
    pub async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        self.await_events.key_for(scope, wait).await
    }

    /// Publish `resolution` as the promise's terminal, first writer wins.
    pub async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.await_events.resolve(key, resolution).await
    }

    /// Read a promise's terminal without registering a waiter.
    pub async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.await_events.peek(key).await
    }

    /// Wait for a promise's terminal on this host's clock.
    pub async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.await_events
            .await_resolution(key, cancel, deadline)
            .await
    }

    /// Tombstone a session and drop its promise rows.
    pub async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), RuntimeError> {
        self.await_events.revoke_session(session_id).await
    }

    /// Sweep a session's unresolved non-turn-control promises to `Cancelled`.
    pub async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), RuntimeError> {
        self.await_events.cancel_session(session_id).await
    }

    /// Delete the journal rows `retirement` names, reporting how many went.
    ///
    /// Group-atomic: a retired group's row and its children go together, so no
    /// partially-retired group exists for a settlement rank to be computed over.
    pub async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.persistence.retire_journal(&retirement).await
    }

    /// Run `envelope` for `scope` exactly once, replaying any recorded terminal.
    ///
    /// Loops until the effect is either replayed, claimed and finalized, or
    /// refused: a live competing claim only makes this wait.
    pub async fn execute_effect(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // Not boxed here: `execute_effect_cancellable` boxes the claim loop
        // itself, which is where the large future is, so a second box on the way
        // in would only add an allocation and an indirection to the same call.
        self.execute_effect_cancellable(scope, envelope, local_executor, None)
            .await
    }

    /// Run `envelope` for `scope`, yielding rather than queueing behind a live
    /// competing claim, and stopping if `cancel` fires.
    ///
    /// The drain's entry point, and the only caller that wants either bound.
    /// [`execute_effect`](Self::execute_effect) queues on a busy claim because
    /// its caller *needs this effect's outcome* and has nowhere else to be; a
    /// drain pass is the opposite — it holds a queue of children and a child
    /// another executor is running right now is the one child it should not be
    /// waiting on. Queueing there turns a pass into an unbounded sleep against
    /// a live renewer while the rest of the queue goes untouched.
    ///
    /// `Ok(None)` means the claim was busy and nothing was written. Dropping
    /// the execution on cancellation is safe for exactly the reason the drain
    /// exists: an abandoned claim leaves an `in_progress` row whose lease stops
    /// being renewed, which is the same state a crashed executor leaves and
    /// which the next pass reclaims.
    pub(super) async fn execute_effect_yielding(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<Option<RuntimeEffectOutcome>, RuntimeEffectControllerError> {
        match Box::pin(self.execute_effect_with_policy(
            scope,
            envelope,
            local_executor,
            None,
            BusyPolicy::Yield,
        ))
        .await?
        {
            EffectRun::Terminal(outcome) => Ok(Some(outcome)),
            EffectRun::Busy => Ok(None),
        }
    }

    /// [`execute_effect`](Self::execute_effect) with an optional cancellation
    /// token, which is how a group child's disposition reaches its execution.
    ///
    /// Cancellation is deliberately applied *inside* the claim rather than by
    /// dropping this future: a dropped execution leaves an `in_progress` row
    /// under a live lease and no terminal, so the child holds no rank and its
    /// caller can never observe it settle. Racing the token against the
    /// execution instead makes the cancellation the child's *terminal*, written
    /// through the same fence and allocating the same rank any other outcome
    /// would — which is what
    /// [`LoserDisposition::Cancel`](super::group::LoserDisposition::Cancel)
    /// promises.
    async fn execute_effect_cancellable(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
        cancel: Option<&CancellationToken>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match Box::pin(self.execute_effect_with_policy(
            scope,
            envelope,
            local_executor,
            cancel,
            BusyPolicy::Queue,
        ))
        .await?
        {
            EffectRun::Terminal(outcome) => Ok(outcome),
            // Unreachable under `BusyPolicy::Queue`, which loops rather than
            // reporting a busy claim. Answered as a refusal rather than a
            // panic so that a future caller passing the wrong policy loses an
            // effect's outcome instead of the process.
            EffectRun::Busy => Err(self.vocabulary().error(
                EffectReplayFailure::LeaseLost,
                format!(
                    "a queueing runtime effect execution for scope `{}` reported a busy claim, \
                     which only a yielding execution may do",
                    scope.id()
                ),
            )),
        }
    }

    /// The claim loop both execution entry points share, parameterised by what
    /// a busy claim means to the caller.
    async fn execute_effect_with_policy(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
        cancel: Option<&CancellationToken>,
        busy: BusyPolicy,
    ) -> Result<EffectRun, RuntimeEffectControllerError> {
        scope
            .validate()
            .map_err(RuntimeEffectControllerError::from)?;
        let reconstructed_envelope = envelope.canonical_form()?;
        let replay_trace = local_executor.replay_validation_trace().cloned();
        // Kept before the claim loop, while the envelope still names the group
        // this child belongs to — the terminal a cancellation writes has to say
        // which group's disposition ended the child, and the claim consumes the
        // envelope. Only a cancellable call clones it, so an ordinary effect
        // pays nothing: every effect this driver runs passes through here, and
        // the overwhelming majority can never be cancelled.
        let cancel_membership = cancel.and_then(|_| envelope.group.clone());
        loop {
            match self
                .prepare_effect(scope, &envelope, &reconstructed_envelope)
                .await?
            {
                PreparedEffect::ReplayMismatch {
                    recorded_envelope,
                    stored_envelope_hash,
                } => {
                    validate_replayed_effect_envelope(
                        recorded_envelope.as_ref(),
                        &reconstructed_envelope,
                        self.vocabulary().code(EffectReplayFailure::HashConflict),
                        replay_trace.as_ref(),
                    )?;
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
                        format!(
                            "stored envelope_hash {stored_envelope_hash} did not match the persisted canonical envelope hash {}",
                            recorded_envelope.hash()
                        ),
                    ));
                }
                PreparedEffect::ReplayOutcome { outcome, due_at_ms } => {
                    self.sleep_until_due(due_at_ms).await;
                    return Ok(EffectRun::Terminal(*outcome));
                }
                PreparedEffect::ReplayError(err) => return Err(err),
                PreparedEffect::Claimed(claim) => {
                    let execution =
                        self.execute_claimed_effect_with_renewal(&claim, envelope, local_executor);
                    let result = match cancel {
                        None => execution.await,
                        Some(cancel) => {
                            tokio::pin!(execution);
                            tokio::select! {
                                biased;
                                () = cancel.cancelled() => Err(groups::child_cancelled_error(
                                    cancel_membership.as_deref(),
                                )),
                                result = &mut execution => result,
                            }
                        }
                    };
                    let finalize = self.finalize_effect(&claim.fence, &result).await;
                    return match (result, finalize) {
                        (Ok(outcome), Ok(())) => Ok(EffectRun::Terminal(outcome)),
                        (Err(err), Ok(())) => Err(err),
                        (_, Err(err)) => Err(err),
                    };
                }
                PreparedEffect::Busy { retry_at_ms } => match busy {
                    BusyPolicy::Queue => self.sleep_until_retry(retry_at_ms).await,
                    BusyPolicy::Yield => return Ok(EffectRun::Busy),
                },
            }
        }
    }

    async fn prepare_effect(
        &self,
        scope: &ExecutionScope,
        envelope: &RuntimeEffectEnvelope,
        reconstructed_envelope: &CanonicalRuntimeEffectEnvelope,
    ) -> Result<PreparedEffect, RuntimeEffectControllerError> {
        let vocabulary = self.vocabulary();
        let replay_key = envelope
            .invocation
            .replay_key()
            .ok_or_else(|| {
                vocabulary.error(
                    EffectReplayFailure::KeyMissing,
                    "runtime effect envelope requires replay.key",
                )
            })?
            .to_string();
        let envelope_json = serde_json::to_string(reconstructed_envelope)
            .map_err(|err| vocabulary.encode_error(err))?;
        let journal_identity = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        let request = EffectClaimRequest {
            scope_id: journal_identity.key().to_string(),
            session_id: journal_identity.session_id().map(str::to_string),
            replay_key,
            envelope_hash: reconstructed_envelope.hash().to_string(),
            envelope_json,
            owner_id: self.owner_id.clone(),
            lease_token: self.next_lease_token(),
            lease_ttl_ms: self.lease_timings.ttl_ms(),
            sleep_duration_ms: sleep_duration_ms(envelope),
            group_key: envelope
                .group
                .as_deref()
                .map(|membership| membership.group_key.clone()),
            strict_replay: self.replay_mode.load(Ordering::SeqCst),
        };

        match self.persistence.claim(&request).await? {
            EffectClaimObservation::Claimed { due_at_ms } => {
                Ok(PreparedEffect::Claimed(ClaimedEffect {
                    fence: EffectLeaseFence {
                        scope_id: request.scope_id,
                        replay_key: request.replay_key,
                        envelope_hash: request.envelope_hash,
                        owner_id: request.owner_id,
                        lease_token: request.lease_token,
                    },
                    due_at_ms,
                }))
            }
            EffectClaimObservation::ReplayMismatch {
                recorded_envelope_json,
                stored_envelope_hash,
            } => {
                let recorded_envelope = serde_json::from_str(&recorded_envelope_json)
                    .map_err(|err| vocabulary.decode_error(err))?;
                Ok(PreparedEffect::ReplayMismatch {
                    recorded_envelope: Box::new(recorded_envelope),
                    stored_envelope_hash,
                })
            }
            EffectClaimObservation::Completed {
                outcome_json,
                due_at_ms,
            } => {
                let outcome = serde_json::from_str(&outcome_json)
                    .map_err(|err| vocabulary.decode_error(err))?;
                Ok(PreparedEffect::ReplayOutcome {
                    outcome: Box::new(outcome),
                    due_at_ms,
                })
            }
            EffectClaimObservation::Failed { error_json } => {
                let err = serde_json::from_str(&error_json)
                    .map_err(|err| vocabulary.decode_error(err))?;
                Ok(PreparedEffect::ReplayError(err))
            }
            EffectClaimObservation::Busy { retry_at_ms } => {
                Ok(PreparedEffect::Busy { retry_at_ms })
            }
            EffectClaimObservation::StrictReplayMiss => Err(vocabulary.error(
                EffectReplayFailure::Missing,
                format!(
                    "no recorded runtime effect for scope `{}` and replay key `{}`",
                    request.scope_id, request.replay_key
                ),
            )),
            EffectClaimObservation::CorruptRow { defect } => {
                Err(vocabulary.error(EffectReplayFailure::CorruptRow, defect.message()))
            }
        }
    }

    async fn finalize_effect(
        &self,
        fence: &EffectLeaseFence,
        outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let vocabulary = self.vocabulary();
        let terminal = match outcome {
            Ok(outcome) => EffectTerminal::Completed {
                outcome_json: serde_json::to_string(outcome)
                    .map_err(|err| vocabulary.encode_error(err))?,
            },
            Err(err) => EffectTerminal::Failed {
                error_json: serde_json::to_string(err)
                    .map_err(|err| vocabulary.encode_error(err))?,
            },
        };
        if matches!(
            self.persistence.finalize(fence, &terminal).await?,
            EffectFinalizeOutcome::Written { .. }
        ) {
            return Ok(());
        }
        Err(vocabulary.error(
            EffectReplayFailure::LeaseLost,
            format!(
                "runtime effect replay lease was lost before finalizing scope `{}` replay key `{}`",
                fence.scope_id, fence.replay_key
            ),
        ))
    }

    async fn renew_effect_lease(
        &self,
        fence: &EffectLeaseFence,
    ) -> Result<(), RuntimeEffectControllerError> {
        if self
            .persistence
            .renew(fence, self.lease_timings.ttl_ms())
            .await?
        {
            return Ok(());
        }
        Err(self.vocabulary().error(
            EffectReplayFailure::LeaseLost,
            format!(
                "runtime effect replay lease was lost while executing scope `{}` replay key `{}`",
                fence.scope_id, fence.replay_key
            ),
        ))
    }

    async fn execute_claimed_effect_with_renewal(
        &self,
        claim: &ClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let renew_every = self.lease_timings.renew_interval();
        let effect = self.execute_claimed_effect(claim, envelope, local_executor);
        tokio::pin!(effect);

        loop {
            tokio::select! {
                result = &mut effect => return result,
                _ = self.clock.sleep(renew_every) => {
                    self.renew_effect_lease(&claim.fence).await?;
                }
            }
        }
    }

    async fn execute_claimed_effect(
        &self,
        claim: &ClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(envelope.command, RuntimeEffectCommand::Sleep { .. }) {
            self.sleep_until_due(claim.due_at_ms).await;
            return Ok(RuntimeEffectOutcome::Sleep);
        }
        match envelope.command {
            RuntimeEffectCommand::PeekAwaitEvent { key } => {
                let resolution = self
                    .peek_await_event(&key)
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution })
            }
            RuntimeEffectCommand::AwaitEvent { key } => {
                let super::executor::RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    ..
                } = local_executor.into_await_event_options()?;
                let resolution = self
                    .await_events
                    .await_resolution_with_clock(&key, cancellation, deadline, clock.as_ref())
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
            }
            RuntimeEffectCommand::Process { command } => {
                let result = local_executor.into_process()?.execute(*command).await?;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            _ => local_executor.execute(envelope).await,
        }
    }

    async fn sleep_until_due(&self, due_at_ms: Option<u64>) {
        let Some(due_at_ms) = due_at_ms else {
            return;
        };
        let now = self.clock.timestamp_ms();
        if due_at_ms > now {
            self.clock
                .sleep(Duration::from_millis(due_at_ms - now))
                .await;
        }
    }

    async fn sleep_until_retry(&self, retry_at_ms: u64) {
        let now = self.clock.timestamp_ms();
        let delay = if retry_at_ms > now {
            Duration::from_millis(retry_at_ms - now).min(BUSY_POLL)
        } else {
            BUSY_POLL
        };
        self.clock.sleep(delay).await;
    }
}

fn sleep_duration_ms(envelope: &RuntimeEffectEnvelope) -> Option<u64> {
    match envelope.command {
        RuntimeEffectCommand::Sleep { duration_ms } => Some(duration_ms),
        _ => None,
    }
}

mod drain;
mod groups;

#[cfg(test)]
mod tests;
