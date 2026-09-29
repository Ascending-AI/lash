//! # lash-sqlite-store
//!
//! The high-performance local **durable** persistence backend for the lash
//! agent runtime. One factory-wide SQLite durable-core database, opened in WAL journal mode
//! with a 15-second busy timeout, satisfying the full [`RuntimePersistence`] +
//! [`AttachmentManifest`] contract from `lash-core-store`.
//!
//! It provides a `SqliteStoreSet` and storage ports for an effect engine such
//! as Restate. SQLite uses WAL (`-wal`/`-shm` sidecars) for concurrent
//! readers and a single writer.
//!
//! ## Why this is "the durable backend" not just "an option"
//!
//! Lash's runtime layer treats persistence as a first-class boundary, not a
//! debug-only convenience. Every primitive that lets the runtime survive a
//! crash — head-revision CAS, final turn-commit idempotency, attachment
//! write-ahead manifests, blob content-addressing with optional compression —
//! is implemented in this crate against SQLite for one reason: SQLite is the
//! simplest backend that gives us *atomic multi-statement transactions on a
//! single file* with durability guarantees we can reason about.
//!
//! ## Schema cutover
//!
//! There is exactly one supported schema (see [`schema::SCHEMA`]). Older
//! databases must be deleted before opening; schema changes are explicit
//! reject-and-recreate boundaries.
//!
//! ## Catalog contention
//!
//! Every store handle from one [`SqliteSessionStoreFactory`] writes the same
//! durable-core database. SQLite WAL permits concurrent readers but has one
//! writer, so commits for different sessions serialize. This is an accepted
//! embedded/single-host trade-off: catalog granularity can be tuned later
//! without weakening crash atomicity. Runtime commits are preflighted against a
//! measured node-and-byte budget for graph, checkpoint, and attachment-adoption
//! payloads before entering the catalog write transaction.
//!
//! [`RuntimePersistence`]: lash_core_execution::RuntimePersistence
//! [`AttachmentManifest`]: lash_core_execution::AttachmentManifest

use lash_core_execution::FleetFormatStore;
use lash_sansio::SessionId;
mod namespace;
#[cfg(test)]
mod process_lifecycle_sql_tests;
#[cfg(test)]
mod rendered_statement_sets_tests;
mod session_deletion;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use lash_core_execution::runtime::{
    AdmissionBoundary, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload,
    TurnLaneAdmissionPolicy, prepare_process_event_append, prepare_process_registration,
};
use lash_core_execution::store::queued_work::{
    MAX_SESSION_COMMAND_BATCHES_PER_RUN, TurnLaneCandidate, admission_scan_limit, derive_batch_id,
    select_leading_session_command, select_turn_work_prefix,
};
use lash_core_execution::store::{
    HydratedCheckpointComponent, HydratedSessionCheckpoint, RuntimeCommit, RuntimeCommitReceipt,
    SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core_execution::{
    AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, BlobRef, DeliveryPolicy, GcReport, PersistedSegmentHandover,
    ProcessAwaitOutput, ProcessChange, ProcessChangeCursor, ProcessContinuationStore, ProcessEvent,
    ProcessEventAppendReceipt, ProcessEventAppendRequest, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessListFilter, ProcessLiveReferenceView, ProcessObserverBy,
    ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome,
    ProcessStarted, SessionCommitStore, SessionListFilter, SessionMeta, SessionStoreCreateRequest,
    SessionSummary, StoreError, StoreMaintenance, VacuumReport, facade_support::ProcessStartPlan,
    facade_support::ProcessTransition, facade_support::ProcessTransitionPlan,
    facade_support::registry_transitions,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use conn::SqliteConnection;
use session_deletion::{
    delete_session_from_catalog, delete_wake_allocation_floors_from_process_registry,
    warn_process_registry_not_wired,
};

mod artifact_store;
mod attachment_store;
mod attachments;
mod blobs;
mod codec;
mod conn;
mod connection_sql;
mod retention;
pub(crate) use codec::*;

fn commit_count_entropy_seed() -> u64 {
    let (high, low) = uuid::Uuid::new_v4().as_u64_pair();
    (high ^ low) & (u64::MAX >> 1)
}
mod backend;
mod catalog;
mod fleet_format;
mod forks;
mod generation_drain;
mod graph;
mod history;
mod ingress_obligation;
mod lifecycle;
mod location;
mod obligation_ledger;
mod pending_turn_inputs;
mod persistence;
mod preflight;
mod process_definitions;
mod process_registry;
mod process_registry_change;
mod process_registry_completion;
mod queued_work;
mod recovery_leader;
mod release_stamp;
mod root_verbs;
mod schema;
mod schema_fragments;
mod schema_layout;
mod session_delete_ledger;
mod session_ingress;
mod session_listing;
mod session_meta;
mod session_roots;
mod session_sql;
#[cfg(test)]
mod session_sql_tests;
#[cfg(any(test, feature = "testing"))]
mod test_support;
#[cfg(feature = "testing")]
pub mod testing;
mod triggers;
mod turn_ingress;

pub use attachment_store::SqliteAttachmentStore;
pub use backend::{SqliteStoreSet, SqliteStoreSetOptions};
pub use conn::{SqliteConnectionPolicy, SqliteSynchronous};
pub use location::SqliteLocation;
use location::{DatabaseLocation, DatabaseTarget};

/// File name of the one durable-core database under a session-store root.
///
/// Named once so the factory that creates it and the preflight that reads it
/// cannot drift onto different files.
pub(crate) const DURABLE_CORE_DB_FILE: &str = "durable-core.db";

/// Backend name this store reports in shared fencing diagnostics.
///
/// Every fenced write names its backend so
/// [`StoreError::FencedWriteVerdictDisagreed`](lash_core_execution::StoreError::FencedWriteVerdictDisagreed)
/// says which store's locked read and backstop predicate disagreed.
pub(crate) const SQLITE_BACKEND: &str = "sqlite";

use conn::TxOutcome;
pub use preflight::{SqliteStorePreflight, verify_schema_at};
pub use schema::SqliteDatabase;

mod control_intent_ledger;
use forks::*;
use pending_turn_inputs::*;
use queued_work::*;
use schema::{apply_pragmas, ensure_versioned_schema};

/// The SQLite durable-core session schema version stamped in `PRAGMA user_version`.
///
/// Hosts can use this constant for compatibility stamps. It moves whenever the
/// SQLite session-store format changes; it does not cover the process, trigger,
/// or effect schemas.
pub const SESSION_SCHEMA_VERSION: i32 = schema::SCHEMA_VERSION;

pub use process_definitions::SqliteProcessDefinitionRegistry;
pub use triggers::SqliteTriggerStore;

/// SQLite-backed store for checkpoint blobs, runtime session state, and
/// Lashlang artifacts.
///
/// This is the first-party local implementation of the runtime store traits.
/// Internally it holds a single cloneable [`SqliteConnection`] (a
/// tokio-rusqlite handle to one database thread).
pub struct SqliteStore {
    conn: SqliteConnection,
    /// The durable-format generation this store's writers emit — the
    /// fleet-format row (ADR 0106 §1 `F`) as the open transaction recorded it.
    /// Writers consult it through
    /// [`lash_core_execution::FleetFormat::writer_version`] instead of binding
    /// the build's constants directly.
    pub(crate) fleet_format: lash_core_execution::FleetFormat,
    /// The durable-core database this store is open on. Held so a store
    /// opened on a memory backend keeps its database alive.
    location: DatabaseLocation,
    turn_cancel_closure_owner: Mutex<Option<lash_core_execution::TurnCancelClosureOwnerBinding>>,
    effect_host: Mutex<Option<Arc<dyn lash_core_execution::EffectHost>>>,
    artifact_stores: Mutex<
        Option<(
            Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
            lash_core_execution::ProcessEngineRegistry,
        )>,
    >,
    process_registry: Option<DatabaseTarget>,
    readers: Vec<SqliteConnection>,
    next_reader: AtomicU64,
    decoded_graph_node_bodies: Arc<AtomicU64>,
    decoded_usage_rows: Arc<AtomicU64>,
    decoded_usage_holes: Arc<AtomicU64>,
    decoded_turn_receipt_bodies: Arc<AtomicU64>,
    clock: Arc<dyn lash_core_execution::Clock>,
    artifact_publication_pause: Mutex<Option<lash_core_execution::ArtifactPublicationPause>>,
    options: StoreOptions,
    commit_count: AtomicU64,
    process_registry_attached: bool,
    #[cfg(test)]
    checkpoint_probe_count: AtomicUsize,
    #[cfg(test)]
    checkpoint_write_transaction_count: AtomicUsize,
}

impl SqliteStore {
    /// Replace the process-local enqueue nonce seed for deterministic fixtures.
    pub fn with_commit_count_seed_for_testing(mut self, seed: u64) -> Self {
        self.commit_count = AtomicU64::new(seed);
        self
    }

    /// Stand this store's writers up on `fleet_format` — typically one
    /// carrying a pin table via [`lash_core_execution::FleetFormat::with_writer_pins`]
    /// — so a test proves they emit the version `F` assigns rather than the
    /// build constant they would stamp anyway (FIG-3796).
    #[cfg(any(test, feature = "testing"))]
    pub fn with_fleet_format_for_testing(
        mut self,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Self {
        self.fleet_format = fleet_format;
        self
    }
}

/// SQLite-backed process registry for one configured runtime deployment.
///
/// It is intentionally separate from [`Store`]: the durable-core catalog
/// persists conversations, while this registry persists background process
/// state and handle visibility across all sessions sharing the registry.
pub struct SqliteProcessRegistry {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core_execution::Clock>,
    /// The durable-core catalog holding the two process-owned sessions of
    /// each process, which the terminal-retention prune deletes before the
    /// process row.
    process_session_catalog: DatabaseLocation,
    wake_delivery_config: lash_core_execution::WakeDeliveryConfig,
    /// Effect hosts whose scope fence registration lifts (ADR 0049).
    scope_fence_hosts: lash_core_execution::ProcessScopeFenceHosts,
    /// This registry's database: bound effect hosts attach it and keep their
    /// process-scope fences in it, beside the process rows (ADR 0049).
    location: DatabaseLocation,
    /// Where registration mints process ids (ADR 0107).
    process_id_mint: lash_core_execution::ProcessIdMint,
    /// `F` from the fleet's durable-core catalog: wake-delivery, lease and
    /// parent-end payloads the registry stamps go through `writer_version`,
    /// never a bare build constant (FIG-3796).
    fleet_format: lash_core_execution::FleetFormat,
}

fn sqlite_error(err: rusqlite::Error) -> StoreError {
    match err {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            StoreError::Contended
        }
        rusqlite::Error::ToSqlConversionFailure(error) => match error.downcast::<StoreError>() {
            Ok(error) => *error,
            Err(error) => StoreError::StorageFailure {
                backend: "sqlite",
                message: error.to_string(),
            },
        },
        err => StoreError::StorageFailure {
            backend: "sqlite",
            message: err.to_string(),
        },
    }
}

/// The `conn.call`/`conn.write` boundary returns [`tokio_rusqlite::Error`];
/// the synchronous arm unwraps to the rusqlite error [`sqlite_error`] already
/// reads, and anything else is the async driver failing.
fn sqlite_async_error(error: tokio_rusqlite::Error) -> StoreError {
    match error {
        tokio_rusqlite::Error::Error(error) => sqlite_error(error),
        error => StoreError::StorageFailure {
            backend: "sqlite",
            message: error.to_string(),
        },
    }
}

/// The `pending_turn_inputs.input_id` column is globally `UNIQUE`: a draft
/// naming an id any row already carries fails the insert, and that violation
/// is the typed id-conflict refusal, not an opaque storage failure. The
/// `(session_id, source_key)` unique constraint surfaces the same way: the
/// admission verdict reads both names first, so reaching the constraint means
/// the row materialized after that read — a cross-handle race or a skipped
/// verdict — and the refused draft's provisioned id is the identity that
/// cannot be filed. Either violation is the id-conflict refusal PostgreSQL's
/// `23505` already produces, not a swallowed retry.
fn sqlite_pending_turn_input_insert_error(
    err: rusqlite::Error,
    session_id: &SessionId,
    input_id: &str,
) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, message) = &err
        && code.code == rusqlite::ErrorCode::ConstraintViolation
    {
        let message = message.as_deref().unwrap_or_default();
        if message.contains("pending_turn_inputs.input_id")
            || message.contains("pending_turn_inputs.session_id, pending_turn_inputs.source_key")
        {
            return StoreError::PendingTurnInputIdConflict {
                session_id: session_id.clone(),
                input_id: input_id.into(),
            };
        }
    }
    sqlite_error(err)
}

fn sqlite_graph_node_insert_error(
    err: rusqlite::Error,
    session_id: &SessionId,
    generation: u64,
    node_id: &str,
) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, message) = &err
        && code.code == rusqlite::ErrorCode::ConstraintViolation
    {
        let message = message.as_deref().unwrap_or_default();
        if message.contains("graph_nodes.session_id, graph_nodes.generation") {
            return StoreError::GraphGenerationCollision {
                session_id: SessionId::from(session_id.to_string()),
                generation,
            };
        }
        if message.contains("graph_nodes.node_id") {
            return StoreError::NodeIdCollision {
                node_id: node_id.to_string().into(),
            };
        }
    }
    sqlite_error(err)
}

pub(crate) fn sqlite_conversion_error(error: StoreError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn stored_data_corrupt(record_kind: &'static str, error: impl std::fmt::Display) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind,
        message: error.to_string(),
    }
}

/// Rebuild a stored attachment id, refusing a row that no longer satisfies the
/// id rule. A malformed stored id is corrupt data, not an id: it must surface
/// as a read failure rather than travel on as a well-formed-looking value.
fn attachment_id_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: String,
) -> rusqlite::Result<lash_core_execution::AttachmentId> {
    lash_core_execution::AttachmentId::parse(&value).map_err(|err| {
        sqlite_conversion_error(stored_data_corrupt(
            record_kind,
            format!("{field} is not a valid attachment id: {err}"),
        ))
    })
}

fn u64_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: i64,
) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| {
        sqlite_conversion_error(stored_data_corrupt(
            record_kind,
            format!("{field} must be non-negative, got {value}"),
        ))
    })
}

fn plugin_u64_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: i64,
) -> Result<u64, lash_core_execution::PluginError> {
    u64::try_from(value).map_err(|_| lash_core_execution::PluginError::StoredDataCorrupt {
        record_kind: record_kind.to_string(),
        message: format!("{field} must be non-negative, got {value}"),
    })
}

fn sql_monotonic_counter_value(
    counter: &'static str,
    current: u64,
    next: u64,
) -> Result<i64, StoreError> {
    i64::try_from(next).map_err(|_| StoreError::MonotonicCounterOverflow { counter, current })
}

fn sql_counter_value(counter: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::MonotonicCounterOverflow {
        counter,
        current: value,
    })
}

fn plugin_sql_counter_value(
    counter: &'static str,
    value: u64,
) -> Result<i64, lash_core_execution::PluginError> {
    i64::try_from(value).map_err(
        |_| lash_core_execution::PluginError::MonotonicCounterOverflow {
            counter: counter.to_string(),
            current: value,
        },
    )
}

fn map_record_decode_error(record_kind: &'static str, error: StoreError) -> StoreError {
    match error {
        StoreError::UnsupportedRecordSchemaVersion { .. }
        | StoreError::MissingRecordSchemaVersion { .. }
        | StoreError::InvalidRecordSchemaVersion { .. }
        | StoreError::StoredDataCorrupt { .. } => error,
        error => stored_data_corrupt(record_kind, error),
    }
}

impl SqliteStore {
    fn read_connection(&self) -> &SqliteConnection {
        let index = self.next_reader.fetch_add(1, AtomicOrdering::Relaxed) as usize;
        &self.readers[index % self.readers.len()]
    }

    fn turn_cancel_closure_owner_binding(
        &self,
    ) -> Option<lash_core_execution::TurnCancelClosureOwnerBinding> {
        self.turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// Clamps an epoch-milliseconds bound to the `i64` range of the SQL time columns.
///
/// Every stored `*_at_ms` column is an `i64`, so a `u64` bound above `i64::MAX`
/// is outside the representable range. Saturating keeps SQL comparisons ordered
/// the way the in-memory predicates order them; a raw `as i64` cast wraps
/// (`u64::MAX as i64 == -1`) and inverts every comparison, so a host-supplied
/// huge cutoff would silently select the opposite row set from the in-memory
/// backend.
///
/// Bounds are compared against stored `i64` timestamps, so saturating is exact
/// for every timestamp below `i64::MAX`.
fn clamp_epoch_ms(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Clamps a caller-supplied sequence, rank, or offset bound to the `i64` range
/// of the SQL sequence columns.
///
/// Every stored sequence is a signed 64-bit integer, so a bound at or above
/// `i64::MAX` already means "through every stored row". Saturating keeps that
/// meaning for the rest of the unsigned range; a raw `as i64` cast wraps
/// (`u64::MAX as i64 == -1`), so a "through the end" count counted nothing and
/// an offset past the end read the first row (FIG-3601).
fn clamp_sequence_bound(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn process_sqlite_error(err: rusqlite::Error) -> lash_core_execution::PluginError {
    lash_core_execution::PluginError::Session(err.to_string())
}

fn process_decode_error(err: serde_json::Error) -> lash_core_execution::PluginError {
    lash_core_execution::PluginError::Session(format!(
        "failed to decode process registry row: {err}"
    ))
}

fn process_encode_json<T: serde::Serialize>(
    value: &T,
) -> Result<String, lash_core_execution::PluginError> {
    serde_json::to_string(value).map_err(|err| {
        lash_core_execution::PluginError::Session(format!("failed to encode process row: {err}"))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PersistedArtifactKind {
    CheckpointManifest,
    CheckpointComponent,
    LashlangModule,
    ProcessExecutionEnv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BlobStorageHint {
    Compressible,
    InlinePreferred,
    LargePayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum BlobCompression {
    None,
    Zlib,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobArtifactDescriptor {
    pub hints: Vec<BlobStorageHint>,
}

impl BlobArtifactDescriptor {
    pub fn new(hints: impl Into<Vec<BlobStorageHint>>) -> Self {
        Self {
            hints: hints.into(),
        }
    }

    pub fn checkpoint_manifest() -> Self {
        Self::new(vec![BlobStorageHint::Compressible])
    }

    pub fn checkpoint_component() -> Self {
        Self::new(vec![
            BlobStorageHint::Compressible,
            BlobStorageHint::LargePayload,
        ])
    }

    pub fn lashlang_module() -> Self {
        Self::new(vec![
            BlobStorageHint::Compressible,
            BlobStorageHint::LargePayload,
        ])
    }

    pub fn process_execution_env() -> Self {
        Self::new(vec![BlobStorageHint::Compressible])
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RetainedArtifactRef {
    pub blob_ref: BlobRef,
    pub kind: PersistedArtifactKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuiltinBlobProfile {
    LowLatency,
    #[default]
    Balanced,
    Compact,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreOptions {
    /// Blob compression profile. This controls storage size and CPU use for
    /// persisted payloads independently of the connection policy.
    pub blob_profile: BuiltinBlobProfile,
    /// SQLite connection behavior for stores opened with these options.
    /// Defaults preserve the current 15-second, normal-synchronous, WAL
    /// autocheckpoint, and cache-size behavior; change it for a deployment
    /// with different lock, durability, WAL-growth, or memory constraints.
    pub connection_policy: SqliteConnectionPolicy,
}

/// The durable artifact-blob envelope. It carries no payload-family field:
/// the pointer table's namespace key owns that fact, so a stored envelope can
/// never disagree with the row that names it (FIG-1949).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredBlobEnvelope {
    compression: BlobCompression,
    #[serde(with = "serde_bytes")]
    content: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct StoredSessionCheckpoint {
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
}

#[cfg(test)]
mod graph_error_tests;
#[cfg(test)]
mod read_failure_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
