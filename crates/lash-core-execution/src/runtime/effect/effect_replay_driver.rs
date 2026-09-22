//! The durable effect-replay state machine shared by every SQL backend.
//!
//! # Where this sits: one contract, two implementations
//!
//! The outer seam is the *substrate contract* — the ports
//! [`EffectHost`](super::executor::EffectHost),
//! the effect-group surface it hands out
//! ([`EffectGroupHandle`](super::group::EffectGroupHandle)),
//! [`QueuedWorkSubstrate`](crate::runtime::QueuedWorkSubstrate) and
//! [`ProcessWorkSubstrate`](crate::runtime::ProcessWorkSubstrate). Every
//! substrate answers those ports, and the conformance laws that say what an
//! answer must mean live at that level, not here.
//!
//! There are two implementations of that contract:
//!
//! * the **store-backed driver** — this module. It absorbs *all* of the
//!   semantics: leases, claim arbitration, replay decisions, journal payload
//!   encoding, group membership, and the loser drain. Backends plug into it
//!   through [`EffectReplayRowStore`], which is dumb row storage plus the
//!   fixed fact in [`EffectReplayCapabilities`], and nothing more; PostgreSQL
//!   and SQLite are two sets of rows under one state machine. The
//!   [`EffectHost`](super::executor::EffectHost) and
//!   [`RuntimeEffectController`](super::executor::RuntimeEffectController)
//!   surface over the driver is shared too — one [`StoreReplayAdapter`]
//!   family, implemented once in this module — so a store contributes its
//!   rows, its capabilities, and its constructors, and nothing else.
//! * the **engine-backed host** — `lash-restate`, which implements the same
//!   ports directly against the engine's own journal. It has no replay ledger,
//!   no Lash lease, and no drain, because the engine already owns retention and
//!   exactly-once execution; it proves the same substrate laws live.
//!
//! Everything store-scoped in this module is therefore *driver-internal
//! machinery*, not part of the substrate contract, and its names say so:
//! [`StoreEffectReplayDriver`], [`EffectReplayRowStore`],
//! [`StoreEffectGroupDrain`](super::group_drain::StoreEffectGroupDrain), and
//! the `store_effect_group_drain_conformance` laws. A reader who wants "what
//! must every substrate do" should be reading the ports; a reader who wants
//! "how does the SQL tier do it" is in the right file.
//!
//! Runtime effects are journaled: the first worker to reach a
//! `(scope_id, replay_key)` pair claims it under a fenced lease, executes it
//! once, and records the terminal outcome; every later arrival replays that
//! record instead of executing again. That is the exactly-once contract, and
//! the two SQL stores were each carrying a full copy of it.
//!
//! This module owns the copy. [`StoreEffectReplayDriver`] runs the whole
//! claim/execute/renew/finalize loop, decodes and encodes the journal payloads,
//! maps controller errors, sleeps for `Sleep` effects and busy retries, and
//! forwards the [`AwaitEventResolver`](super::executor::AwaitEventResolver)
//! surface to the shared [`AwaitEventCoordinator`]. Backends implement only
//! [`EffectReplayRowStore`]: four atomic row operations plus whatever
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
//! [`EffectReplayRowStore::claim`]'s to read, and each backend reads its own
//! (SQLite: the host's injected [`Clock`](crate::Clock), the same domain its
//! rows already live in; PostgreSQL: `transaction_timestamp()`, per the
//! [`Clock`](crate::Clock) contract's database-authoritative lease boundary,
//! pinned by `postgres_clock_contract`).
//! The driver's own [`Clock`](crate::Clock) never stamps a row and never
//! decides a lease: it only sleeps — `Sleep` effect due times, busy-retry
//! backoff, and the lease renewal interval.

use crate::SessionId;
use crate::runtime::effect::ProcessCommand;
mod adapter;
pub use adapter::{
    StoreReplayAdapter, StoreReplayController, StoreReplayHost, store_replay_capabilities,
};
#[doc(hidden)]
pub use tokio_util::sync::CancellationToken as ReplayCancellationToken;

#[cfg(feature = "testing")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::await_event_coordinator::{AwaitEventBackend, AwaitEventCoordinator};
use super::envelope::{
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectOutcome, SleepSpec,
};

use super::executor::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectJournalRetirement, ExecutionScope, Resolution,
    ResolveOutcome, RuntimeEffectControllerError, RuntimeEffectLocalExecutor,
};
use super::group::child_cancelled_error;
use super::group_drain::GroupExecutors;
/// The durable group shape this port's backends implement, re-exported so a
/// backend imports the whole effect-journal vocabulary from one place.
pub use super::group_journal::{
    AcceptedGroupChild, EffectCancelOutcome, EffectCancelRequest, EffectCommitState,
    EffectDischargeOutcome, EffectDischargeRequest, EffectFinalizeOutcome,
    EffectGroupChildCommitOutcome, EffectGroupChildCommitRequest, EffectGroupColumn,
    EffectGroupLifecycle, EffectGroupLifecyclePhase, EffectGroupRecord, FinalizationStep,
    GroupChildFinalCommit, StoredChildArbitration, StoredGroupSettlement, UnsettledGroupChild,
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

/// The fact about a backend that the shared [`EffectHost`] /
/// [`RuntimeEffectController`] adapter (see [`StoreReplayAdapter`]) needs and
/// cannot derive from the row operations.
///
/// Before the adapter was shared, each store carried its own copy of the
/// adapter to encode its answers; now a backend states them once, here, and
/// the one adapter reads them. They are fixed at construction: none can change
/// while a driver is alive.
///
/// [`EffectHost`]: super::executor::EffectHost
/// [`RuntimeEffectController`]: super::executor::RuntimeEffectController
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectReplayCapabilities {
    pub completion_keys: CompletionKeys,
}

/// Whether a backend's await-event rows can back a completion key handed out
/// of the process — the answer
/// [`AwaitEventResolver::prepare_completion_key`](super::executor::AwaitEventResolver::prepare_completion_key)
/// gives when the caller may defer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKeys {
    /// Promise rows outlive the process, so a key minted here stays routable:
    /// the preparation is [`Issued`](super::executor::CompletionKeyPreparation::Issued).
    Issued,
    /// Promise rows die with the process (SQLite's testing-only memory
    /// backing), so no key is handed out: the preparation is
    /// [`Unsupported`](super::executor::CompletionKeyPreparation::Unsupported).
    Unsupported,
}

/// What a caller of the shared claim loop wants a live competing claim to mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusyPolicy {
    /// What an effect's own caller wants: it needs this outcome, and the other executor is
    /// producing it.
    Queue,
    /// What the drain wants: it has a queue, and a child someone else owns right now is the
    /// one it should move past rather than sleep against.
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
/// The request carries the *sleep intent* rather than a due timestamp: the
/// due time is derived from the same authoritative instant that stamps the
/// lease, which the backend reads inside its own transaction. An absolute
/// `Until` deadline needs no derivation and is recorded as-is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectClaimRequest {
    /// Durable journal identity of the executing scope.
    pub scope_id: String,
    /// Owning session, when the scope has one. `NULL` rows are session-free.
    pub session_id: Option<SessionId>,
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
    /// `Some` only for `Sleep` effects: the journaled sleep intent.
    pub sleep: Option<SleepSpec>,
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
    /// The journal address of the effect this admission is minted under —
    /// the envelope's `caused_by` lineage — when the cause is an effect.
    ///
    /// The claim consults it on the **insert path only**, and only for §4's
    /// fence: when the minting effect is a group child whose cancel
    /// disposition already committed, a new semantic admission under it is
    /// refused with
    /// [`MintingChildCancelled`](EffectClaimObservation::MintingChildCancelled)
    /// and no row is written. A replay row that already exists — completed,
    /// failed, or reclaimed by takeover — is an *admitted* command, which §4
    /// protects exactly: cancellation fences new admission, it never undoes
    /// what was admitted before it.
    pub minting_effect: Option<MintingEffectRef>,
    /// Strict replay: a missing row is an error instead of a fresh claim.
    pub strict_replay: bool,
}

/// The journal identity of the effect a fresh admission is minted under —
/// what [`EffectClaimRequest::minting_effect`] carries so the claim can fence
/// the insert on the minting child's §4 decision without re-deriving scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintingEffectRef {
    /// The minting effect's journal scope key.
    pub scope_id: String,
    /// The minting effect's replay key.
    pub replay_key: String,
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
    /// The status and payload columns, decoded once by the store.
    pub state: EffectRowState,
    /// The §4 commit-protocol phase, when the row belongs to a group child.
    /// `None` is the honest answer for an ungrouped row: it never contests a
    /// §4 point, so it carries no phase to read.
    pub commit_state: Option<EffectCommitState>,
    /// The drain input a boundary-committed child recorded — what a recovery
    /// drains instead of re-executing the attempt.
    pub drain_input: Option<String>,
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
    Report(EffectClaimObservation),
}

/// What a claim attempt observed. Backends return this from
/// [`EffectReplayRowStore::claim`].
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
    /// The scope carries a permanent retirement tombstone: its journal was
    /// deleted as unreachable and nothing may re-admit an effect under it.
    ScopeRetired,
    /// The minting parent named by [`EffectClaimRequest::minting_effect`] is a
    /// group child whose cancel disposition already committed. §4 forbids a
    /// new semantic admission under a cancelled invocation, so the claim
    /// wrote nothing — no row, no lease, no decision of its own.
    MintingChildCancelled,
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
    /// A known status carries a payload column it does not own.
    UnexpectedPayloads {
        /// The recognized status column.
        status: EffectRowStatus,
        outcome_json_present: bool,
        error_json_present: bool,
    },
    /// `status` holds a value no version of this runtime writes.
    UnknownStatus {
        /// The unrecognized column value.
        status: String,
    },
    /// `commit_state` is `committed` but `drain_input` is `NULL`: only the
    /// §4 boundary writes that state, and it always writes the input with it.
    MissingDrainInput,
    /// `commit_state` is `drained` or `cancel_decided` while `status` is still
    /// `in_progress`: both states journal their terminal in the same write,
    /// so the pair cannot represent a row any writer produced.
    CommitStateWithoutTerminal {
        /// The recorded commit state.
        commit_state: String,
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
            Self::UnexpectedPayloads {
                status,
                outcome_json_present,
                error_json_present,
            } => format!(
                "runtime effect replay status `{}` contradicts its payload columns: \
                 outcome_json present = {outcome_json_present}, error_json present = \
                 {error_json_present}",
                status.column()
            ),
            Self::UnknownStatus { status } => {
                format!("unknown runtime effect replay status `{status}`")
            }
            Self::MissingDrainInput => {
                "committed runtime effect row is missing drain_input".to_string()
            }
            Self::CommitStateWithoutTerminal { commit_state } => {
                format!(
                    "runtime effect row is `{commit_state}` but still `in_progress`; \
                     a decided or drained row journals its terminal in the same write"
                )
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

/// A replay row's status and payload columns decoded at the store boundary.
///
/// `Corrupt` is a state rather than a conversion error so reads that promise
/// to surface bad rows, notably the group drain, cannot accidentally filter
/// corruption while collecting backend rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectRowState {
    /// A lease is held and neither terminal payload exists.
    InProgress,
    /// Exactly one terminal payload agrees with the terminal status.
    Settled(EffectTerminal),
    /// The three columns do not describe a state any runtime writes.
    Corrupt(EffectRowDefect),
}

impl EffectRowState {
    /// Decode the persisted status and payload columns without discarding a
    /// corrupt row.
    pub fn from_columns(
        status: String,
        outcome_json: Option<String>,
        error_json: Option<String>,
    ) -> Self {
        let Some(status) = EffectRowStatus::parse(&status) else {
            return Self::Corrupt(EffectRowDefect::UnknownStatus { status });
        };
        match (status, outcome_json, error_json) {
            (EffectRowStatus::InProgress, None, None) => Self::InProgress,
            (EffectRowStatus::Completed, Some(outcome_json), None) => {
                Self::Settled(EffectTerminal::Completed { outcome_json })
            }
            (EffectRowStatus::Failed, None, Some(error_json)) => {
                Self::Settled(EffectTerminal::Failed { error_json })
            }
            (EffectRowStatus::Completed, None, _) => Self::Corrupt(EffectRowDefect::MissingOutcome),
            (EffectRowStatus::Failed, _, None) => Self::Corrupt(EffectRowDefect::MissingError),
            (status, outcome_json, error_json) => {
                Self::Corrupt(EffectRowDefect::UnexpectedPayloads {
                    status,
                    outcome_json_present: outcome_json.is_some(),
                    error_json_present: error_json.is_some(),
                })
            }
        }
    }

    fn status_column(&self) -> &str {
        match self {
            Self::InProgress => EffectRowStatus::InProgress.column(),
            Self::Settled(terminal) => terminal.status().column(),
            Self::Corrupt(EffectRowDefect::MissingOutcome) => EffectRowStatus::Completed.column(),
            Self::Corrupt(EffectRowDefect::MissingError) => EffectRowStatus::Failed.column(),
            Self::Corrupt(EffectRowDefect::UnexpectedPayloads { status, .. }) => status.column(),
            Self::Corrupt(EffectRowDefect::UnknownStatus { status }) => status,
            Self::Corrupt(EffectRowDefect::VanishedUnderClaim) => "vanished",
            // Both defects describe `in_progress` rows: a boundary commit
            // leaves the status column alone, so a row missing its drain
            // input or ahead of its terminal still reads `in_progress`.
            Self::Corrupt(
                EffectRowDefect::MissingDrainInput
                | EffectRowDefect::CommitStateWithoutTerminal { .. },
            ) => EffectRowStatus::InProgress.column(),
        }
    }
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
    let fresh_due_at_ms = request.sleep.map(|spec| match spec {
        SleepSpec::For { duration_ms } => now_ms.saturating_add(duration_ms),
        SleepSpec::Until { deadline_ms } => deadline_ms,
    });
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

    match &row.state {
        EffectRowState::Settled(EffectTerminal::Completed { outcome_json }) => {
            EffectClaimDecision::Report(EffectClaimObservation::Completed {
                outcome_json: outcome_json.clone(),
                due_at_ms: row.due_at_ms,
            })
        }
        EffectRowState::Settled(EffectTerminal::Failed { error_json }) => {
            EffectClaimDecision::Report(EffectClaimObservation::Failed {
                error_json: error_json.clone(),
            })
        }
        EffectRowState::InProgress if row.lease_expires_at_ms > now_ms => {
            EffectClaimDecision::Report(EffectClaimObservation::Busy {
                retry_at_ms: row.lease_expires_at_ms,
            })
        }
        EffectRowState::InProgress => match row.commit_state {
            // A boundary-committed child is taken over exactly like any
            // expired claim: the executor re-runs, its journaled inner rows
            // replay, and the settle path's `commit_group_child` read-back
            // lands `AlreadyCommitted` so the drain resumes rather than the
            // sealed attempt re-executing. The drain input the commit
            // recorded is what makes that resume honest — a committed row
            // without one is corruption, not a recovery candidate.
            Some(EffectCommitState::Committed) => match row.drain_input {
                Some(_) => EffectClaimDecision::TakeOver(stamp(row.due_at_ms.or(fresh_due_at_ms))),
                None => EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
                    defect: EffectRowDefect::MissingDrainInput,
                }),
            },
            Some(state @ (EffectCommitState::Drained | EffectCommitState::CancelDecided)) => {
                EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
                    defect: EffectRowDefect::CommitStateWithoutTerminal {
                        commit_state: state.column().to_string(),
                    },
                })
            }
            Some(EffectCommitState::Pending) | None => {
                EffectClaimDecision::TakeOver(stamp(row.due_at_ms.or(fresh_due_at_ms)))
            }
        },
        EffectRowState::Corrupt(defect) => {
            EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
                defect: defect.clone(),
            })
        }
    }
}

/// The seal on [`EffectReplayRowStore`].
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
pub mod sealed;

/// Atomic row operations a durable substrate must provide to journal effects.
///
/// # Implementing this trait inherits the whole driver
///
/// This is the plug-in seam of the store-backed tier, not a second place to
/// write effect semantics. A backend that supplies these row operations gets
/// claim/execute/renew/finalize, payload encoding, group membership, and the
/// loser drain from [`StoreEffectReplayDriver`] for free — and gets no say in
/// any of them. Nobody hand-rolls replay or drain: there is one copy, above
/// this trait, and adding a SQL tier means answering these rows and nothing
/// else.
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
pub trait EffectReplayRowStore: sealed::EffectReplayBackend + Send + Sync {
    /// The error vocabulary hosts already match on for this backend.
    fn vocabulary(&self) -> EffectReplayVocabulary;

    /// The fixed facts about this backend the shared host and controller
    /// adapter answers from. See [`EffectReplayCapabilities`].
    fn capabilities(&self) -> EffectReplayCapabilities;

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

    /// Read whether an exact replay row exists without claiming or mutating it.
    /// Used only after a strict v2 miss to distinguish an absent continuation
    /// from a pre-cutover v1 tool-intent row that must be refused loudly.
    async fn replay_row_exists(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<bool, RuntimeEffectControllerError>;

    /// Expire an ungrouped pending derivation claim without sealing an error.
    /// Match all five fence columns and the live lease at write time. Retain
    /// the canonical envelope and pending row; a subsequent claim rotates its
    /// owner and token. Refuse committed, cancelled, grouped or expired rows.
    async fn release_uncommitted_derivation(
        &self,
        fence: &EffectLeaseFence,
    ) -> Result<bool, RuntimeEffectControllerError>;

    /// Write `terminal` and release the lease, guarded by `fence`; for a
    /// grouped child, contest the group's §4 linearization point in the same
    /// transaction.
    ///
    /// Atomically, and only while the row still matches all five fence columns,
    /// is `in_progress`, and has not expired against the substrate's lease
    /// clock: set the terminal's status and payload column, clear the lease
    /// owner and token, and zero the lease expiry. Report
    /// [`EffectFinalizeOutcome::FenceMoved`] when the guarded write matched no
    /// row and no cancel disposition explains the miss — the fence moved and
    /// this driver no longer owns the effect.
    ///
    /// # Normative ordering (N1, extended by ADR 0099 §4)
    ///
    /// One transaction, in this order — **replay row, then group row**, the
    /// one lock order every arbitration path in this contract shares. The §4
    /// point itself is the replay row's `commit_state`: the CAS that moves it
    /// `pending → committed` runs under the same row lock the fenced write
    /// already holds, and the group's counter bump is what lets that CAS
    /// write its `commit_seq` in the same statement:
    ///
    /// 1. Perform the fenced `UPDATE` above.
    /// 2. **If its rowcount is 0:** read the row's `group_key` and
    ///    `commit_state`. `cancel_decided` means the cancel disposition
    ///    already won the linearization point: roll back and report
    ///    [`EffectFinalizeOutcome::CancelDecided`]. Anything else is an
    ///    ordinary fence miss: roll back and report `FenceMoved`. **No
    ///    counter bump either way.**
    /// 3. On rowcount 1 with a `group_key`: bump `next_commit_seq` on the
    ///    group row, then CAS the replay row `commit_state = 'committed'`,
    ///    `commit_seq` = the returned position — guarded on
    ///    `commit_state = 'pending'`. A CAS miss means a cancel disposition
    ///    committed between the claim and now: roll back — the terminal, the
    ///    bump and the state all go away together — and report
    ///    `CancelDecided`. A `committed` state cannot cause the miss — it is
    ///    written only with a terminal, which the fenced `UPDATE` would not
    ///    have matched — so any other miss is corruption, not an outcome arm.
    /// 4. Commit, reporting [`EffectFinalizeOutcome::Written`] with the
    ///    commit-order position.
    ///
    /// Because every contestant for a child's commit state must first pass
    /// the replay row's lock, holding it makes the CAS at step 3 serialize
    /// behind one writer: no concurrent `decide_cancel` can be inside the
    /// row while this transaction holds it.
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

    /// Commit the cancel disposition for one group child at the group's §4
    /// linearization point, or report the decision that already holds.
    ///
    /// This is the durable half of cancellation: the body signal and the local
    /// grace that follow it are *signalling*, and this is the *decision* —
    /// journaled, fenced, and irreversible once committed (ADR 0099 §4).
    ///
    /// # Normative ordering
    ///
    /// One transaction, in the shared lock order — replay row, then group
    /// row. The terminal write deliberately precedes the commit-state CAS:
    /// writing first is what keeps this path from deadlocking against a
    /// concurrent `finalize`, which holds the replay row's lock while it
    /// reaches for the group row. A CAS that loses rolls the speculative
    /// write back with the rest.
    ///
    /// 1. Read the replay row's `commit_state`, joining the row to
    ///    `request`'s `group_key`. `cancel_decided` → report
    ///    [`EffectCancelOutcome::AlreadyDecided`] with the rank the first
    ///    decision seated the child at. `committed` or `drained` → report
    ///    [`EffectCancelOutcome::FinalCommitted`]; the child's final record
    ///    holds the point and the cancel may not commit. No replay row for
    ///    the pair means an accepted-but-never-claimed child — the membership
    ///    row is the admission, so proceed to insert one.
    /// 2. Write the cancelled terminal into the child's replay row — an
    ///    `UPDATE` for a claimed child, an `INSERT` for an unclaimed one,
    ///    sourcing `scope_id` from the group row and
    ///    `envelope_json`/`envelope_hash` from the request, which carries the
    ///    canonical envelope form the caller captured from retained
    ///    membership.
    /// 3. Bump `next_seq` on the group row: the rank the cancelled terminal is
    ///    about to take. Bumping before the CAS keeps this path in the shared
    ///    lock order, and a CAS miss rolls the bump back with the terminal.
    /// 4. CAS the replay row `commit_state = 'cancel_decided'`, guarded on
    ///    `commit_state = 'pending'`. A miss means a contestant won between
    ///    the read and the CAS: roll back and answer with the state that
    ///    committed.
    /// 5. Write the bumped rank onto the replay row's `settlement_seq`: a
    ///    cancel-decided child is rankable immediately, because it has no
    ///    protected admission left to discharge — new admission is exactly
    ///    what the decision fences out. Commit, reporting
    ///    [`EffectCancelOutcome::Decided`].
    ///
    /// Idempotent end to end: a retried decision observes step 1's
    /// `AlreadyDecided` and writes nothing.
    async fn decide_cancel(
        &self,
        request: &EffectCancelRequest,
    ) -> Result<EffectCancelOutcome, RuntimeEffectControllerError>;

    /// Discharge one committed child's §5 drain: record the durable drain
    /// stamp and allocate its settlement rank, in commit order.
    ///
    /// Rank is allocated *here* and not at finalize, because a rank a consumer
    /// could observe before the child's declared intents landed would be a
    /// settlement published ahead of its own effects (ADR 0099 §5). Separating
    /// commit order (`commit_seq`, assigned at finalize) from rank
    /// (`settlement_seq`, assigned here) is what makes "intent drains are
    /// admitted in final-commit order" a durable fact rather than a scheduler
    /// convention.
    ///
    /// # Normative ordering
    ///
    /// One transaction, in the shared lock order — replay row, then group
    /// row:
    ///
    /// 1. Take the child's replay row lock (a write; the row must exist and
    ///    carry `request`'s `group_key` — a missing or foreign row is
    ///    corruption).
    /// 2. Read the row's `commit_state`. `drained` → report
    ///    [`EffectDischargeOutcome::AlreadyDischarged`] with the rank the
    ///    first discharge seated, and write nothing. Anything other than
    ///    `committed` is corruption: only a committed child has a drain to
    ///    discharge.
    /// 3. The commit-order barrier: if any sibling replay row holds
    ///    `commit_state = 'committed'` with `commit_seq` below this child's,
    ///    roll back and report [`EffectDischargeOutcome::Blocked`]. The set
    ///    of lower commit positions is fixed at commit time, so a blocked
    ///    child becomes dischargeable exactly when the siblings ahead of it
    ///    drain — never by the barrier loosening.
    /// 4. Bump `next_seq` on the group row, write the returned rank onto the
    ///    replay row's `settlement_seq`, and CAS `commit_state` to `drained`
    ///    — rank and state in one commit, so a recovered reader never sees a
    ///    rankable child that is not drained nor a drained child without a
    ///    rank.
    /// 5. Commit, reporting [`EffectDischargeOutcome::Discharged`].
    async fn discharge_child(
        &self,
        request: &EffectDischargeRequest,
    ) -> Result<EffectDischargeOutcome, RuntimeEffectControllerError>;

    /// Whether any sibling replay row of `group_key` holds `commit_state =
    /// 'committed'` with `commit_seq` below `commit_seq` — the durable gate a
    /// child's intent drain waits behind so drains are admitted in
    /// final-commit order (ADR 0099 §5).
    ///
    /// Monotonic in the caller's favour: the set of commit positions below
    /// `commit_seq` was fixed when it was allocated, so once this answers
    /// `false` it cannot become `true` again, and a driver may poll it without
    /// a notification channel.
    async fn drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, RuntimeEffectControllerError>;

    /// Commit one group child's final record at the §4 point — the
    /// final-attempt boundary's durable half.
    ///
    /// In one transaction, in the shared lock order:
    ///
    /// 1. Read the child's replay row (its group is the row's own
    ///    `group_key`, never the caller's): `committed`/`drained` answers
    ///    [`EffectGroupChildCommitOutcome::AlreadyCommitted`] with the
    ///    recorded `commit_seq` and `drain_input`, `cancel_decided` answers
    ///    [`EffectGroupChildCommitOutcome::CancelDecided`], and anything
    ///    other than `pending` is corruption the CAS makes unwritable.
    /// 2. Bump `next_commit_seq` on the group row and CAS the child's
    ///    `commit_state` to `committed`, writing the returned position and
    ///    the drain input — guarded on `pending` *and* on the row's lease
    ///    still belonging to `request.owner_id`, so a stale executor whose
    ///    claim was reclaimed commits nothing. A miss re-reads: a committed
    ///    cancel decides the race; a committed sibling CAS is idempotent;
    ///    a still-pending row under a moved lease is a fence loss.
    /// 3. Commit, reporting [`EffectGroupChildCommitOutcome::Committed`].
    ///
    /// What the commit deliberately does **not** write is the terminal: a
    /// boundary-committed row stays `in_progress` holding only the decision,
    /// the position, and the drain input — the projected outcome lands at
    /// discharge, when the drain it records has actually finished.
    async fn commit_group_child(
        &self,
        request: &EffectGroupChildCommitRequest,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError>;

    /// The same arbitration state, reached through the child's replay row
    /// instead of its group: `(scope_id, replay_key)` is the journal address
    /// a caller already holds, and the join resolves the group from the row
    /// itself rather than trusting the caller's word for it.
    ///
    /// `None` means the replay row exists but belongs to no group child — or
    /// no row exists at all; the fence callers cannot and need not tell the
    /// two apart.
    async fn read_group_child_arbitration(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<Option<StoredChildArbitration>, RuntimeEffectControllerError>;

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
        membership: &[AcceptedGroupChild],
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError>;

    /// The read half of [`open_group`](Self::open_group)'s membership write,
    /// and the reason §3's retention is worth anything: a host that reopens a
    /// journaled group rebuilds its children from this rather than from
    /// whatever the caller happened to pass. Empty for a group the journal does
    /// not hold; a recorded group always has complete membership, because the
    /// two are written in one transaction.
    async fn read_group_membership(
        &self,
        group_key: &str,
    ) -> Result<Vec<AcceptedGroupChild>, RuntimeEffectControllerError>;

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

    /// The settlement notifier for `group_key`: one shared `Arc<Notify>` per
    /// (store, group) that every committed rank write —
    /// [`discharge_child`](Self::discharge_child),
    /// [`decide_cancel`](Self::decide_cancel), and a grouped
    /// [`finalize`](Self::finalize) — wakes after its commit lands, and that a
    /// peer driver over the same database wakes the same way.
    ///
    /// The caller enables [`Notify::notified`] *before* its journal read and
    /// parks on it afterwards, so a settlement committed between the read and
    /// the park is caught rather than slept through. Acquiring the notifier is
    /// async so a backend whose wake-up rides an external subscription
    /// (PostgreSQL `LISTEN`) can await the subscription's installation before
    /// the caller's first read — the ordering the same guarantee needs across
    /// processes.
    async fn settlement_notifier(
        &self,
        group_key: &str,
    ) -> Result<Arc<Notify>, RuntimeEffectControllerError>;

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

    /// Advance the group's durable lifecycle if it currently holds one of
    /// `from` phases, in a single guarded write (ADR 0099 §7).
    ///
    /// This is the close/finalization CAS: close writes `Closing` before any
    /// `decide_cancel`, each finalization step advances the recorded cursor,
    /// and step 4 turns `closing` into `settled`. Returns the lifecycle now
    /// durable on the row — `to` on a hit, the existing value on a guard miss —
    /// so a caller distinguishes "I wrote this" from "someone else moved it"
    /// without a second round-trip. An unknown `group_key` is an error: the
    /// group row must exist.
    async fn transition_group_lifecycle(
        &self,
        group_key: &str,
        from: &[EffectGroupLifecyclePhase],
        to: &EffectGroupLifecycle,
    ) -> Result<EffectGroupLifecycle, RuntimeEffectControllerError>;

    /// Every group recorded under `scope_id` whose lifecycle is `closing` —
    /// the resumable finalization set a redriven opener drains
    /// (ADR 0099 §7, `resume_closing_groups`).
    async fn read_closing_groups(
        &self,
        scope_id: &str,
    ) -> Result<Vec<EffectGroupRecord>, RuntimeEffectControllerError>;

    /// `(group_key, lifecycle)` for every group owned by `session_id` whose
    /// lifecycle is not `settled` — the pins a session deletion must refuse
    /// before it deletes anything (ADR 0099 §7).
    async fn read_session_group_lifecycle_pins(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, EffectGroupLifecycle)>, RuntimeEffectControllerError>;

    /// Extend the lease by `lease_ttl_ms`, guarded by `fence`.
    ///
    /// Same guard as [`finalize`](EffectReplayRowStore::finalize); the new expiry is the
    /// substrate's lease clock plus `lease_ttl_ms`.
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
    ///
    /// **Scope-exact retirements fence** (N4): for a `Process` or
    /// `RuntimeOperation` retirement the same transaction also deletes the
    /// scope's await-event promise rows and inserts the scope's permanent
    /// retirement tombstone, keyed by its journal identity. Every admission
    /// path — [`claim`](Self::claim), [`open_group`](Self::open_group), and the
    /// await-event backend's mint/ensure/store/inspect atoms — reads that
    /// tombstone under the same lock the retirement writes it under, so a
    /// retired scope reports [`EffectClaimObservation::ScopeRetired`] or
    /// `await_event_unknown_or_revoked` rather than re-executing under an
    /// empty journal. Session retirements keep their shipped shape: rows go,
    /// no tombstone is written here (session promise revocation owns that).
    /// A scope-exact retirement gated [`EffectRetirementGate::WhenQuiescent`]
    /// first proves, inside the same transaction and under the same lock,
    /// that the scope is quiescent: no `in_progress` effect row (a running
    /// child or a draining loser) and no open group still waiting for a child
    /// that has not been journaled yet. A live scope is left untouched and
    /// the retirement fails with [`RuntimeErrorCode::EffectScopeNotQuiescent`].
    /// [`EffectRetirementGate::OwnerTerminal`] skips the proof: the caller
    /// holds it (the registry pruned the owner), so in-flight rows go too.
    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError>;

    /// Read committed scope fences whose artifact-owner cleanup is pending.
    async fn pending_artifact_owner_retirements(&self)
    -> Result<Vec<ExecutionScope>, RuntimeError>;

    /// Mark one committed fence's artifact-owner cleanup complete.
    async fn complete_artifact_owner_retirement(&self, scope_id: &str) -> Result<(), RuntimeError>;

    /// Delete the scope-retirement fence row of `scope_id`, if any, under the
    /// same lock retirement writes it: the scope's owner is being registered
    /// again (a pruned process id reused by the host, ADR 0049). Nothing else
    /// is written; the re-registered scope starts with the empty journal its
    /// prune left.
    async fn reinstate_scope(&self, scope_id: &str) -> Result<(), RuntimeError>;

    /// The `WhenQuiescent` retirement proof as a standalone read: `true` when
    /// `scope` carries no `in_progress` effect row, no group still short of a
    /// journaled child, and no unresolved promise. Unlike the gate inside
    /// [`retire_journal`](Self::retire_journal) this read takes no scope lock:
    /// the caller is deciding whether to write an owner's end fact (FIG-3419),
    /// not deleting the scope, so it needs the answer without the exclusion.
    async fn scope_is_quiescent(&self, scope: &ExecutionScope) -> Result<bool, RuntimeError>;
}

/// The refusal a quiescence-gated retirement reports for a scope that still
/// has live work: nothing was deleted or fenced, and the caller retries once
/// the work settles.
pub fn scope_not_quiescent(scope_id: &str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::EffectScopeNotQuiescent,
        format!(
            "effect scope `{scope_id}` still has in-progress effects or an open group; retirement deferred until it is quiescent"
        ),
    )
}

/// The refusal every admission path reports for a scope whose retirement
/// tombstone exists: the journal under it was deleted as unreachable, so a
/// late redrive must fail closed rather than re-execute under an empty journal.
pub fn scope_retired(scope_id: &str) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::EffectScopeRetired,
        format!(
            "effect scope `{scope_id}` has been retired: its journal was deleted as unreachable and the scope cannot be re-admitted"
        ),
    )
}

pub fn tool_intent_replay_key_format_cutover(
    recorded_replay_key: &str,
    requested_replay_key: &str,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::ToolIntentReplayKeyFormatCutover,
        format!(
            "continuation replay refused at the tool-intent replay-key format cutover: journaled row uses `{recorded_replay_key}` from `tool-intent:v1:`, but this build requested `{requested_replay_key}` from `tool-intent:v2:`; start a fresh post-cutover invocation instead of re-executing the pre-cutover command"
        ),
    )
}

/// The typed refusal a late final record earns when the cancel disposition
/// already owns the §4 linearization point: no terminal, no rank, no journal
/// write from the loser.
fn group_child_cancel_decided(replay_key: &str) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
        format!(
            "the cancel disposition won the durable linearization point for \
             group child `{replay_key}` before its final record could commit; the \
             terminal was refused and nothing was journaled",
        ),
    )
}

/// A claim this driver holds: the fence plus the due time it recorded.
struct ClaimedEffect {
    fence: EffectLeaseFence,
    due_at_ms: Option<u64>,
    /// The group this claim belongs to, when it does — carried from the claim
    /// request so the post-commit discharge can name it without re-reading the
    /// row.
    group_key: Option<String>,
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
pub struct StoreEffectReplayDriver<P, A> {
    row_store: P,
    await_events: AwaitEventCoordinator<A>,
    clock: Arc<dyn crate::Clock>,
    owner_id: String,
    lease_counter: AtomicU64,
    replay_mode: AtomicBool,
    lease_timings: LeaseTimings,
    /// The bound step 1 of group finalization waits on a cancel-decided
    /// child's attempt body after its decision commits (ADR 0099 §7).
    drain_budget: super::group::EffectGroupDrainBudget,
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
    /// [`register_group_executors`](StoreEffectReplayDriver::register_group_executors).
    group_executors: OnceLock<Arc<dyn GroupExecutors>>,
    /// This driver's one tool-child wiring (ADR 0099 §2), shared by both SQL
    /// tiers because both reach their group seam through this driver.
    ///
    /// Beside `group_executors` rather than inside it, because the two answer
    /// different questions: that cell holds whatever resolver was registered,
    /// and this one holds the live-opener registry a turn must register its
    /// opener in. A get-or-init, so a host backing several runtimes hands them
    /// all the same registry — two registries on one host would mean a turn
    /// registering in one while the resolver read the other.
    tool_children: OnceLock<Arc<super::ToolChildHost>>,
    /// Testing seam (FIG-3429): which offered-child executor the group-open
    /// selector may reuse. `RetainedEnvelope` in every real deployment; the
    /// two-opener differential flips one host to `KeyOnly` to prove its
    /// assertions kill the replay-key-only leak the envelope check exists to
    /// stop.
    #[cfg(feature = "testing")]
    offered_child_selection: AtomicUsize,
    /// Error-return injector (FIG-3524) over `claim`, `finalize` and `renew`,
    /// consulted by `take_journal_fault` at each row-store call.
    #[cfg(feature = "testing")]
    journal_faults: EffectJournalFaults,
}

/// How a group open decides whether an executor the caller staged for its own
/// offered child may run the retained child of the same replay key (ADR 0099
/// §3, W1).
///
/// [`OfferedChildSelection::RetainedEnvelope`] is the only correct answer: an
/// offered runner is bound to the *offered* envelope's authority and may be
/// reused only when the offered envelope is byte-identical to the retained
/// row — which is exactly the honest-reopen case the staging fast-path exists
/// for. [`OfferedChildSelection::KeyOnly`] is the injected leak the two-opener
/// differential is red-proved against: a replay key is the child's durable
/// identity, not its request's, and matching on it alone hands retained
/// authority to whatever a reoffering successor staged under that key.
#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OfferedChildSelection {
    /// Reuse the staged runner only when the offered envelope is
    /// byte-identical to the retained row.
    #[default]
    RetainedEnvelope = 0,
    /// Reuse the staged runner on a replay-key match alone, whatever
    /// authority the retained envelope records.
    KeyOnly = 1,
}

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// `clock` is the driver's *sleep* clock: it times `Sleep` effects, the
    /// busy-retry backoff, and the lease renewal interval, and it never stamps
    /// a row or decides a lease — the substrate's own lease clock does that
    /// inside [`EffectReplayRowStore::claim`]. Pass the host's injected
    /// clock when the substrate shares the host's clock domain (SQLite), and an
    /// explicit [`SystemClock`](crate::facade_support::SystemClock) when it does
    /// not (PostgreSQL, whose lease decisions are server-side per the
    /// [`Clock`](crate::Clock) contract, pinned by `postgres_clock_contract`).
    pub fn new(
        row_store: P,
        await_events: AwaitEventCoordinator<A>,
        clock: Arc<dyn crate::Clock>,
        lease_timings: LeaseTimings,
        drain_budget: super::group::EffectGroupDrainBudget,
    ) -> Self {
        let sequence = EFFECT_OWNER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let owner_id = format!(
            "pid{}-{sequence}-{}",
            std::process::id(),
            clock.timestamp_ms()
        );
        #[cfg(feature = "testing")]
        let journal_faults = EffectJournalFaults::new(row_store.vocabulary().store_code());
        Self {
            #[cfg(feature = "testing")]
            journal_faults,
            row_store,
            await_events,
            clock,
            owner_id,
            lease_counter: AtomicU64::new(1),
            replay_mode: AtomicBool::new(false),
            lease_timings,
            drain_budget,
            groups: groups::DurableEffectGroups::default(),
            group_executors: OnceLock::new(),
            tool_children: OnceLock::new(),
            #[cfg(feature = "testing")]
            offered_child_selection: AtomicUsize::new(
                OfferedChildSelection::RetainedEnvelope as usize,
            ),
        }
    }

    /// Testing seam: install this host's offered-child selection strategy.
    ///
    /// Per driver, never global: a test flips exactly the host it means to
    /// poison, and a sibling test's host keeps the honest answer.
    #[cfg(feature = "testing")]
    pub fn set_offered_child_selection(&self, selection: OfferedChildSelection) {
        self.offered_child_selection
            .store(selection as usize, Ordering::SeqCst);
    }

    /// This driver's tool-child wiring cell. See the field.
    pub fn tool_child_host(&self) -> &OnceLock<Arc<super::ToolChildHost>> {
        &self.tool_children
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
    /// nowhere to come from — so all three group methods refuse with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// rather than journaling a group nothing can run.
    ///
    /// [`OnceLock::set`] is the arbiter rather than a preceding `get`: a
    /// get-then-set pair leaves a window in which two threads both read `None`,
    /// both write, and the loser is told `Ok` while its resolver was dropped on
    /// the floor — the exact drift this refusal exists to prevent. `set` decides,
    /// and its `Err` hands back the rejected resolver so the same-resolver case
    /// stays a no-op.
    #[expect(
        clippy::expect_used,
        reason = "a rejected set means the cell is already initialized"
    )]
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

    /// The registered resolver, or the refusal that says this host does not
    /// implement groups at all.
    ///
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// not a shape refusal: an unwired host is not a host with a bad group, it is
    /// a host that does no groups at all, and it answers so through all three
    /// methods alike. A *per-child* resolver miss on a wired host is the other
    /// fact and keeps its typed routing refusal.
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
        self.row_store.vocabulary()
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

    pub async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.await_events.peek(key).await
    }

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
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.await_events.revoke_session(session_id).await
    }

    /// Sweep a session's unresolved non-turn-control promises to `Cancelled`.
    pub async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.await_events.cancel_session(session_id).await
    }

    /// List the registered, unresolved promise keys of one session.
    pub async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        self.await_events.outstanding_for_session(session_id).await
    }

    /// The promise half of scope retirement, answered from the whole: a
    /// non-session scope's promises go with its journal in one transaction
    /// (N4), so this lever is [`retire_effect_journal`](Self::retire_effect_journal)
    /// for the scope's exact retirement.
    pub async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let Some(retirement) = EffectJournalRetirement::for_scope(scope) else {
            return Err(super::executor::await_event_scope_not_retirable(scope));
        };
        self.retire_effect_journal(retirement).await.map(|_| ())
    }

    /// Delete the journal rows `retirement` names, reporting how many went.
    ///
    /// Group-atomic: a retired group's row and its children go together, so no
    /// partially-retired group exists for a settlement rank to be computed over.
    pub async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.row_store.retire_journal(&retirement).await
    }

    pub async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<ExecutionScope>, RuntimeError> {
        self.row_store.pending_artifact_owner_retirements().await
    }

    pub async fn complete_artifact_owner_retirement(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let identity = scope.journal_identity()?;
        self.row_store
            .complete_artifact_owner_retirement(identity.key())
            .await
    }

    /// Lift the scope-retirement fence of a non-session `scope` whose owner is
    /// registered again (ADR 0049). Session scopes are refused as on the
    /// retirement lever.
    pub async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        if EffectJournalRetirement::for_scope(scope).is_none() {
            return Err(super::executor::await_event_scope_not_retirable(scope));
        }
        let identity = scope.journal_identity()?;
        self.row_store.reinstate_scope(identity.key()).await
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
        binding: Option<&crate::GroupChildBinding>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // Not boxed here: `execute_effect_cancellable` boxes the claim loop
        // itself, which is where the large future is, so a second box on the way
        // in would only add an allocation and an indirection to the same call.
        self.execute_effect_cancellable(scope, envelope, local_executor, None, binding)
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
            None,
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
    /// [`LoserPolicy::Cancel`](super::group::LoserPolicy::Cancel)
    /// promises.
    async fn execute_effect_cancellable(
        &self,
        scope: &ExecutionScope,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
        cancel: Option<&CancellationToken>,
        binding: Option<&crate::GroupChildBinding>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match Box::pin(self.execute_effect_with_policy(
            scope,
            envelope,
            local_executor,
            cancel,
            BusyPolicy::Queue,
            binding,
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
        binding: Option<&crate::GroupChildBinding>,
    ) -> Result<EffectRun, RuntimeEffectControllerError> {
        envelope.invocation.validate_execution_scope(scope)?;
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
        // The wait a cancellable `AwaitEvent` child is parked on: dropping the
        // execution future never polls the waiter's own release arm, so the
        // cancel path below resolves the promise itself (ADR 0099 §12 — the
        // cancelled child terminal releases the wait, not the process).
        let cancel_wait_key = cancel.and_then(|_| match &envelope.command {
            RuntimeEffectCommand::AwaitEvent { key } => Some(key.clone()),
            _ => None,
        });
        loop {
            match self
                .prepare_effect(scope, &envelope, &reconstructed_envelope, binding)
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
                    let command_kind = envelope.command.kind();
                    let execution =
                        self.execute_claimed_effect_with_renewal(&claim, envelope, local_executor);
                    let result = match cancel {
                        None => execution.await,
                        Some(cancel) => {
                            tokio::pin!(execution);
                            tokio::select! {
                                biased;
                                () = cancel.cancelled() => {
                                    // Cancellation tokens are physical-stop
                                    // signals for uncommitted children; a
                                    // committed final retains authority to
                                    // finish its drain (§4), so the token is
                                    // not authorization once the boundary ran.
                                    match self
                                        .row_store
                                        .read_group_child_arbitration(
                                            &claim.fence.scope_id,
                                            &claim.fence.replay_key,
                                        )
                                        .await
                                    {
                                        Err(err) => Err(err),
                                        Ok(Some(arbitration))
                                            if matches!(
                                                arbitration.commit_state,
                                                EffectCommitState::Committed
                                                    | EffectCommitState::Drained
                                            ) =>
                                        {
                                            execution.await
                                        }
                                        _ => {
                                            if let Some(key) = &cancel_wait_key {
                                                let _ = self
                                                    .await_events
                                                    .resolve(
                                                        key,
                                                        crate::Resolution::Cancelled,
                                                    )
                                                    .await;
                                            }
                                            Err(child_cancelled_error(
                                                cancel_membership
                                                    .as_deref()
                                                    .map_or("<ungrouped>", |membership| {
                                                        membership.group_key.as_str()
                                                    }),
                                                cancel_membership
                                                    .as_deref()
                                                    .map_or(0, |membership| membership.position),
                                            ))
                                        }
                                    }
                                }
                                result = &mut execution => result,
                            }
                        }
                    };
                    let finalize = self.finalize_effect(&claim, command_kind, &result).await;
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
        binding: Option<&crate::GroupChildBinding>,
    ) -> Result<PreparedEffect, RuntimeEffectControllerError> {
        let vocabulary = self.vocabulary();
        let replay_key = envelope.invocation.replay_key().to_string();
        let envelope_json = serde_json::to_string(reconstructed_envelope)
            .map_err(|err| vocabulary.encode_error(err))?;
        let journal_identity = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        let request = EffectClaimRequest {
            scope_id: journal_identity.key().to_string(),
            session_id: journal_identity.session_id().cloned(),
            replay_key,
            envelope_hash: reconstructed_envelope.hash().to_string(),
            envelope_json,
            owner_id: self.owner_id.clone(),
            lease_token: self.next_lease_token(),
            lease_ttl_ms: self.lease_timings.ttl_ms(),
            sleep: sleep_spec(envelope),
            group_key: envelope
                .group
                .as_deref()
                .map(|membership| membership.group_key.clone()),
            minting_effect: match binding {
                Some(binding) => Some(MintingEffectRef {
                    scope_id: binding
                        .child
                        .execution_scope
                        .journal_identity()
                        .map_err(RuntimeEffectControllerError::from)?
                        .key()
                        .to_string(),
                    replay_key: binding.child.replay_key.clone(),
                }),
                None => None,
            },
            strict_replay: self.replay_mode.load(Ordering::SeqCst),
        };

        #[cfg(feature = "testing")]
        if let Some(err) =
            self.take_journal_fault(EffectJournalFaultPoint::Claim, &request.replay_key)
        {
            return Err(err);
        }
        match self.row_store.claim(&request).await? {
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
                    group_key: request.group_key,
                }))
            }
            EffectClaimObservation::ReplayMismatch {
                recorded_envelope_json,
                stored_envelope_hash,
            } => {
                let recorded_envelope =
                    CanonicalRuntimeEffectEnvelope::decode(&recorded_envelope_json)?;
                Ok(PreparedEffect::ReplayMismatch {
                    recorded_envelope: Box::new(recorded_envelope),
                    stored_envelope_hash,
                })
            }
            EffectClaimObservation::Completed {
                outcome_json,
                due_at_ms,
            } => {
                let outcome = decode_runtime_effect_outcome(&outcome_json)
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
            EffectClaimObservation::StrictReplayMiss => {
                if let Some(legacy_replay_key) =
                    crate::tool_intent::legacy_tool_intent_v1_lookup_key(&envelope.invocation)
                    && self
                        .row_store
                        .replay_row_exists(&request.scope_id, &legacy_replay_key)
                        .await?
                {
                    return Err(tool_intent_replay_key_format_cutover(
                        &legacy_replay_key,
                        &request.replay_key,
                    ));
                }
                Err(vocabulary.error(
                    EffectReplayFailure::Missing,
                    format!(
                        "no recorded runtime effect for scope `{}` and replay key `{}`",
                        request.scope_id, request.replay_key
                    ),
                ))
            }
            EffectClaimObservation::CorruptRow { defect } => {
                Err(vocabulary.error(EffectReplayFailure::CorruptRow, defect.message()))
            }
            EffectClaimObservation::ScopeRetired => Err(scope_retired(&request.scope_id)),
            // The §4 fence, raised inside the claim transaction: the minting
            // child's cancel disposition is already durable, so this admission
            // writes nothing and the nested command surfaces as the typed
            // refusal, not a journaled failure of its own.
            EffectClaimObservation::MintingChildCancelled => {
                Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
                    format!(
                        "the group child that minted replay key `{}` is cancel-decided; \
                         ADR 0099 §4 forbids a new semantic admission under it",
                        request.replay_key
                    ),
                ))
            }
        }
    }

    async fn finalize_effect(
        &self,
        claim: &ClaimedEffect,
        command_kind: crate::RuntimeEffectKind,
        outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let fence = &claim.fence;
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
        // A group child whose final-attempt boundary already ran arrives here
        // `committed` with no terminal: the commit journaled the decision, the
        // position and the drain input while the lease was held, and the
        // intents drained underneath it. Its discharge is the write that seats
        // this execution's projected terminal and marks the row `drained`. A
        // `pending` row still owes its §4 decision — taken below — and
        // `cancel_decided` is the typed refusal either way.
        if let Some(group_key) = &claim.group_key
            && let Some(arbitration) = self
                .row_store
                .read_group_child_arbitration(&fence.scope_id, &fence.replay_key)
                .await?
            && arbitration.group_key == *group_key
        {
            match arbitration.commit_state {
                EffectCommitState::Committed | EffectCommitState::Drained => {
                    // A failed execution of a committed child seats nothing:
                    // the row stays `committed`+`in_progress` so the recovery
                    // drain re-executes it, rather than discharging a
                    // terminal the protected final never produced (§4, §14).
                    if outcome.is_err() {
                        return Ok(());
                    }
                    return self
                        .discharge_committed_claim(
                            &fence.scope_id,
                            &fence.replay_key,
                            group_key,
                            Some(terminal),
                        )
                        .await;
                }
                EffectCommitState::CancelDecided => {
                    return Err(group_child_cancel_decided(&fence.replay_key));
                }
                EffectCommitState::Pending => {}
            }
        }
        if derivation::release_derivation(self, claim, command_kind, outcome).await? {
            return Ok(());
        }
        #[cfg(feature = "testing")]
        if let Some(err) =
            self.take_journal_fault(EffectJournalFaultPoint::Finalize, &fence.replay_key)
        {
            return Err(err);
        }
        match self.row_store.finalize(fence, &terminal).await? {
            EffectFinalizeOutcome::Written { commit_seq: _ } => {
                // A `pending` grouped row reaching finalize never ran the
                // attempt boundary — a non-tool child, or a tool child that
                // failed before its terminal attempt — so it has no drain to
                // owe: finalize is its §4 point and its discharge is
                // immediate (§5: a child with no remaining intent admission
                // is admitted and discharged at once). The barrier still
                // holds it behind lower-commit siblings; a sibling whose
                // host died mid-drain is finished by the next drain pass,
                // which this poll is the fallback for, not the driver of.
                let Some(group_key) = &claim.group_key else {
                    return Ok(());
                };
                self.discharge_committed_claim(
                    &fence.scope_id,
                    &fence.replay_key,
                    group_key,
                    None,
                )
                .await
            }
            EffectFinalizeOutcome::CancelDecided => {
                Err(group_child_cancel_decided(&fence.replay_key))
            }
            EffectFinalizeOutcome::FenceMoved => Err(vocabulary.error(
                EffectReplayFailure::LeaseLost,
                format!(
                    "runtime effect replay lease was lost before finalizing scope `{}` replay key `{}`",
                    fence.scope_id, fence.replay_key
                ),
            )),
        }
    }

    /// The §4 boundary commit the scoped controller forwards: the lease
    /// owner is this driver's identity, so the CAS is fenced on the claim
    /// this host holds, and the group resolves from the durable row rather
    /// than anything the caller asserts.
    async fn commit_group_child_final(
        &self,
        commit: GroupChildFinalCommit,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        self.row_store
            .commit_group_child(&EffectGroupChildCommitRequest {
                group_key: None,
                scope_id: commit.scope_id,
                replay_key: commit.replay_key,
                drain_input: commit.drain_input,
                owner_id: self.owner_id.clone(),
            })
            .await
    }

    /// Discharge one committed group child: the barrier wait, the rank write,
    /// and — for a boundary-committed row — the projected terminal its §4
    /// commit deliberately did not carry.
    async fn discharge_committed_claim(
        &self,
        scope_id: &str,
        replay_key: &str,
        group_key: &str,
        terminal: Option<EffectTerminal>,
    ) -> Result<(), RuntimeEffectControllerError> {
        loop {
            match self
                .row_store
                .discharge_child(&EffectDischargeRequest {
                    group_key: group_key.to_string(),
                    scope_id: scope_id.to_string(),
                    replay_key: replay_key.to_string(),
                    terminal: terminal.clone(),
                })
                .await?
            {
                EffectDischargeOutcome::Discharged { .. }
                | EffectDischargeOutcome::AlreadyDischarged { .. } => return Ok(()),
                EffectDischargeOutcome::Blocked => {
                    self.clock.sleep(BUSY_POLL).await;
                }
            }
        }
    }

    async fn renew_effect_lease(
        &self,
        fence: &EffectLeaseFence,
    ) -> Result<(), RuntimeEffectControllerError> {
        #[cfg(feature = "testing")]
        if let Some(err) =
            self.take_journal_fault(EffectJournalFaultPoint::Renew, &fence.replay_key)
        {
            return Err(err);
        }
        if self
            .row_store
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
                let result =
                    if matches!(command.as_ref(), ProcessCommand::RegisterDefinition { .. }) {
                        local_executor
                            .into_process_definitions()?
                            .execute(envelope.invocation.replay_key(), *command)
                            .await?
                    } else {
                        local_executor.into_process()?.execute(*command).await?
                    };
                Ok(RuntimeEffectOutcome::Process { result })
            }
            RuntimeEffectCommand::Trigger { command } => {
                local_executor
                    .execute_trigger(envelope.invocation, *command)
                    .await
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

/// Decode one completed effect outcome, upgrading only the pre-cutover
/// `LlmResponse.full_text` representation at the durable journal boundary.
///
/// Live responses never pass through this function. A legacy text value is
/// materialized as a response part only when the response's existing parts
/// project no visible assistant prose; otherwise the parts remain untouched.
fn decode_runtime_effect_outcome(
    outcome_json: &str,
) -> Result<RuntimeEffectOutcome, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(outcome_json)?;
    if let Some(response) = journaled_llm_response_mut(&mut value) {
        upgrade_legacy_journaled_llm_response(response);
    }
    serde_json::from_value(value)
}

fn journaled_llm_response_mut(value: &mut serde_json::Value) -> Option<&mut serde_json::Value> {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("llm_call" | "direct") => value.get_mut("result")?.get_mut("Ok"),
        Some("assistant_response_hooks") => value.get_mut("response"),
        _ => None,
    }
}

#[expect(clippy::expect_used, reason = "a crate-owned output part serializes")]
fn upgrade_legacy_journaled_llm_response(response: &mut serde_json::Value) {
    let Some(response) = response.as_object_mut() else {
        return;
    };
    let Some(serde_json::Value::String(full_text)) = response.remove("full_text") else {
        return;
    };
    if full_text.is_empty() {
        return;
    }

    let parts_project_no_text = response
        .get("parts")
        .cloned()
        .and_then(|parts| serde_json::from_value::<Vec<crate::LlmOutputPart>>(parts).ok())
        .is_some_and(|parts| crate::visible_response_text_from_parts(&parts).is_empty());
    if !parts_project_no_text {
        return;
    }

    let Some(parts) = response
        .get_mut("parts")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    parts.push(
        serde_json::to_value(crate::LlmOutputPart::Text {
            text: full_text,
            response_meta: None,
        })
        .expect("LlmOutputPart serialization is infallible"),
    );
}

fn sleep_spec(envelope: &RuntimeEffectEnvelope) -> Option<SleepSpec> {
    match envelope.command {
        RuntimeEffectCommand::Sleep { spec } => Some(spec),
        _ => None,
    }
}

mod closing;
mod derivation;
mod drain;
mod groups;
#[cfg(feature = "testing")]
mod journal_faults;
#[cfg(feature = "testing")]
pub use journal_faults::{EffectJournalFaultPoint, EffectJournalFaults};

#[cfg(test)]
mod tests;
