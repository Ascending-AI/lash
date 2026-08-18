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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// The durable group shape this port's backends implement, re-exported so a
/// backend imports the whole effect-journal vocabulary from one place.
pub use super::group_journal::{
    EffectFinalizeOutcome, EffectGroupColumn, EffectGroupRecord, StoredGroupSettlement,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
    ) -> Result<(), RuntimeEffectControllerError>;

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
        }
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
        scope
            .validate()
            .map_err(RuntimeEffectControllerError::from)?;
        let reconstructed_envelope = envelope.canonical_form()?;
        let replay_trace = local_executor.replay_validation_trace().cloned();
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
                    return Ok(*outcome);
                }
                PreparedEffect::ReplayError(err) => return Err(err),
                PreparedEffect::Claimed(claim) => {
                    let result = self
                        .execute_claimed_effect_with_renewal(&claim, envelope, local_executor)
                        .await;
                    let finalize = self.finalize_effect(&claim.fence, &result).await;
                    return match (result, finalize) {
                        (Ok(outcome), Ok(())) => Ok(outcome),
                        (Err(err), Ok(())) => Err(err),
                        (_, Err(err)) => Err(err),
                    };
                }
                PreparedEffect::Busy { retry_at_ms } => {
                    self.sleep_until_retry(retry_at_ms).await;
                }
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

#[cfg(test)]
mod tests;
