//! # lash-sqlite-store
//!
//! The high-performance local **durable** persistence backend for the lash
//! agent runtime. One factory-wide SQLite durable-core database, opened in WAL journal mode
//! with a 15-second busy timeout, satisfying the full [`RuntimePersistence`] +
//! [`AttachmentManifest`] contract from `lash-core-store`.
//!
//! This crate is a drop-in replacement for `lash-sqlite-store`: it exposes the
//! same public surface (`Store`, `SqliteProcessRegistry`,
//! `SqliteSessionStoreFactory`, `SqliteEffectHost`, the option/descriptor types)
//! with identical async signatures, so a consumer swaps backends by renaming
//! the crate path only. The difference is the engine underneath: tokio-rusqlite
//! over a statically-linked SQLite with real WAL (`-wal`/`-shm` sidecars,
//! multi-process readers + single writer) instead of the prior store's experimental mvcc.
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

use lash_sansio::SessionId;
mod namespace;
mod process_key;
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
use std::sync::{Arc, Mutex, OnceLock};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use lash_core_execution::runtime::{
    QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkClaimPolicy, QueuedWorkCompletion, QueuedWorkEnqueueOutcome,
    QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, prepare_process_event_append,
    prepare_process_registration,
};
use lash_core_execution::store::queued_work::{
    ClaimCandidate, MAX_SESSION_COMMAND_BATCHES_PER_CLAIM, QueuedWorkClaimOutcome,
    QueuedWorkClaimRefusal, claim_scan_limit, derive_batch_id, select_exact_turn_work_claim_prefix,
    select_leading_session_command, select_turn_work_claim_prefix,
};
use lash_core_execution::store::{
    HydratedCheckpointComponent, HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit,
    RuntimeCommitReceipt, SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core_execution::store_backend_support::{
    SessionExecutionLeaseRow, lease_owner_from_columns, row_to_session_execution_lease,
};
use lash_core_execution::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, BlobRef, DeliveryPolicy, GcReport, LeaseOwnerIdentity,
    PersistedSegmentHandover, ProcessAwaitOutput, ProcessChange, ProcessChangeCursor,
    ProcessContinuationStore, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessIncarnation, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessListFilter, ProcessLiveReferenceView,
    ProcessObserverBy, ProcessPruneReport, ProcessRecord, ProcessRef, ProcessRegistration,
    ProcessRegistry, ProcessStartOutcome, ProcessStarted, QueuedWorkStore, RuntimePersistence,
    SessionCommitStore, SessionExecutionLease, SessionExecutionLeaseAcquisition,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore,
    SessionListFilter, SessionMeta, SessionRelationKind, SessionStoreCreateRequest,
    SessionStoreFactory, SessionSummary, StoreError, StoreMaintenance, TurnInputStore,
    VacuumReport, facade_support::ProcessStartPlan, facade_support::ProcessTransition,
    facade_support::ProcessTransitionPlan, facade_support::registry_transitions,
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
mod await_event;
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
mod effect_replay;
mod forks;
mod graph;
mod lifecycle;
mod location;
mod pending_turn_inputs;
mod persistence;
mod preflight;
mod process_definitions;
mod process_registry;
mod process_registry_change;
mod process_registry_completion;
mod queued_work;
mod release_stamp;
mod required_constraints;
mod schema;
mod schema_fragments;
mod scope_fence;
mod session_ingress;
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
pub use backend::{SqliteBackend, SqliteBackendOptions, SqliteStoreSet, SqliteStoreSetOptions};
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
pub use effect_replay::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteRuntimeEffectController,
};
pub use lash_core_execution::store_backend_support::required_constraints::{
    RequiredConstraintFinding, RequiredConstraintReport,
};
pub use preflight::{SqliteStorePreflight, verify_schema_at};
pub use required_constraints::inspect_required_constraints_at;
pub use schema::SqliteDatabase;

mod control_intent_ledger;
mod factory_reads;
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
pub struct Store {
    conn: SqliteConnection,
    /// The durable-core database this store is open on. Held so a store
    /// opened on a memory backend keeps its database alive.
    location: DatabaseLocation,
    turn_cancel_closure_owner: Option<lash_core_execution::TurnCancelClosureOwnerBinding>,
    session_id: Arc<OnceLock<SessionId>>,
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

impl Store {
    /// Replace the process-local enqueue nonce seed for deterministic fixtures.
    pub fn with_commit_count_seed_for_testing(mut self, seed: u64) -> Self {
        self.commit_count = AtomicU64::new(seed);
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

/// The `pending_turn_inputs.input_id` column is globally `UNIQUE`: a draft
/// naming an id any row already carries fails the insert, and that violation
/// is the typed id-conflict refusal, not an opaque storage failure.
fn sqlite_pending_turn_input_insert_error(
    err: rusqlite::Error,
    session_id: &SessionId,
    input_id: &str,
) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, message) = &err
        && code.code == rusqlite::ErrorCode::ConstraintViolation
        && message
            .as_deref()
            .unwrap_or_default()
            .contains("pending_turn_inputs.input_id")
    {
        return StoreError::PendingTurnInputIdConflict {
            session_id: session_id.clone(),
            input_id: input_id.into(),
        };
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

fn sqlite_conversion_error(error: StoreError) -> rusqlite::Error {
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

fn sql_session_lease_generation(value: u64) -> Result<i64, StoreError> {
    sql_counter_value("session_lease_generation", value)
}

fn plugin_sql_monotonic_counter_value(
    counter: &'static str,
    current: u64,
    value: u64,
) -> Result<i64, lash_core_execution::PluginError> {
    i64::try_from(value).map_err(
        |_| lash_core_execution::PluginError::MonotonicCounterOverflow {
            counter: counter.to_string(),
            current,
        },
    )
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

impl Store {
    fn bind_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        bind_session_lock(&self.session_id, session_id)
    }

    fn selected_session_id(&self) -> Result<SessionId, StoreError> {
        self.session_id
            .get()
            .cloned()
            .ok_or(StoreError::SessionNotBound)
    }

    async fn resolve_session_id_for_read(&self) -> Result<Option<SessionId>, StoreError> {
        if let Some(session_id) = self.session_id.get() {
            return Ok(Some(session_id.clone()));
        }
        let session_ids = self
            .conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    crate::session_sql::session_sql()
                        .head
                        .select_sole_bound_session_id
                        .sql(),
                )?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        if session_ids.is_empty() {
            return Ok(None);
        }
        if session_ids.len() > 1 {
            return Err(StoreError::SessionResolutionAmbiguous {
                session_count: session_ids.len() as u64,
            });
        }
        self.bind_session(&SessionId::from(session_ids[0].clone()))?;
        Ok(self.session_id.get().cloned())
    }
}

/// Check-or-install the handle's session binding.
///
/// The `OnceLock` makes the decision atomic against every binder, whether it
/// runs on a task thread or inside a `write_flow` closure on the connection
/// thread: a set that loses to a concurrent install re-reads the winner's id
/// and answers the mismatch.
fn bind_session_lock(lock: &OnceLock<SessionId>, session_id: &SessionId) -> Result<(), StoreError> {
    if let Some(bound_session_id) = lock.get() {
        if bound_session_id != session_id {
            return Err(StoreError::SessionBindingMismatch {
                bound_session_id: bound_session_id.clone(),
                attempted_session_id: session_id.clone(),
            });
        }
        return Ok(());
    }
    let _ = lock.set(session_id.clone());
    if lock.get().is_some_and(|bound| bound == session_id) {
        Ok(())
    } else {
        Err(StoreError::SessionBindingMismatch {
            bound_session_id: lock
                .get()
                .cloned()
                .unwrap_or_else(|| SessionId::from(String::default())),
            attempted_session_id: session_id.clone(),
        })
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

type BoundArtifactStores = (
    Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
    lash_core_execution::ProcessEngineRegistry,
);
type SharedArtifactStores = Arc<std::sync::Mutex<Option<BoundArtifactStores>>>;

/// Explicit first-party factory for one SQLite durable-core catalog.
///
/// A [`SqliteBackend`] or [`SqliteStoreSet`] opens the one a host's core
/// runs on; the factory never becomes a default: app storage and runtime
/// storage remain host-owned decisions.
#[derive(Clone)]
pub struct SqliteSessionStoreFactory {
    /// The one durable-core catalog every session of this factory lives in.
    core: DatabaseLocation,
    /// The process registry maintenance attaches beside the catalog.
    process_registry: Option<DatabaseTarget>,
    options: StoreOptions,
    clock: Arc<dyn lash_core_execution::Clock>,
    #[cfg(feature = "testing")]
    fault_injector: Option<testing::SqliteFaultInjector>,
    /// The backend's effect journal: the retained-evidence sweep attaches
    /// it to retire quiescent operation scopes whose receipt this catalog
    /// holds (ADR 0067). Fixed by the backend's location when the factory
    /// is opened; `None` when the backend journals effects elsewhere.
    effect_journal: Option<DatabaseLocation>,
    turn_cancel_closure_owner:
        Arc<std::sync::Mutex<Option<lash_core_execution::TurnCancelClosureOwnerBinding>>>,
    effect_host: Arc<std::sync::Mutex<Option<Arc<dyn lash_core_execution::EffectHost>>>>,
    artifact_stores: SharedArtifactStores,
}

impl SqliteSessionStoreFactory {
    async fn resume_artifact_owner_retirements(
        &self,
    ) -> Result<(), lash_core_execution::StoreError> {
        let effect_host = self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let artifact_stores = self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (Some(effect_host), Some((process_env_store, process_engines))) =
            (effect_host, artifact_stores)
        else {
            return Ok(());
        };
        let scopes = effect_host
            .pending_artifact_owner_retirements()
            .await
            .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        for scope in scopes {
            let owner = lash_core_execution::ArtifactOwner::execution(scope.clone());
            process_env_store
                .retire_process_execution_env_owner(&owner)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
            process_engines
                .retire_artifact_owner(&owner)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
            effect_host
                .complete_artifact_owner_retirement(&scope)
                .await
                .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        warn_process_registry_not_wired("SqliteSessionStoreFactory::new");
        Self::for_root(root.into(), StoreOptions::default(), None)
    }

    pub fn with_options(root: impl Into<PathBuf>, options: StoreOptions) -> Self {
        warn_process_registry_not_wired("SqliteSessionStoreFactory::with_options");
        Self::for_root(root.into(), options, None)
    }

    /// This is the warning-free durable constructor when the deployment uses a Lash SQLite
    /// process registry.
    pub fn new_with_process_registry(
        root: impl Into<PathBuf>,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_root(
            root.into(),
            StoreOptions::default(),
            Some(process_registry_path.into()),
        )
    }

    pub fn with_options_and_process_registry(
        root: impl Into<PathBuf>,
        options: StoreOptions,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_root(root.into(), options, Some(process_registry_path.into()))
    }

    fn for_root(root: PathBuf, options: StoreOptions, process_registry: Option<PathBuf>) -> Self {
        Self::at(
            DatabaseLocation::standalone_file(&root.join(DURABLE_CORE_DB_FILE)),
            process_registry.map(DatabaseTarget::File),
            None,
            options,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
    }

    /// The factory over `core` in one backend, with its registry and
    /// effect journal fixed by the backend's location.
    pub(crate) fn at(
        core: DatabaseLocation,
        process_registry: Option<DatabaseTarget>,
        effect_journal: Option<DatabaseLocation>,
        options: StoreOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        Self {
            core,
            process_registry,
            options,
            clock,
            #[cfg(feature = "testing")]
            fault_injector: None,
            effect_journal,
            turn_cancel_closure_owner: Arc::new(std::sync::Mutex::new(None)),
            effect_host: Arc::new(std::sync::Mutex::new(None)),
            artifact_stores: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The method and backing field do not exist without the `testing` feature.
    #[cfg(feature = "testing")]
    pub fn with_fault_injector(mut self, injector: testing::SqliteFaultInjector) -> Self {
        self.fault_injector = Some(injector);
        self
    }

    /// The URI a raw SQLite connection opens this factory's durable-core
    /// catalog through, file or memory.
    pub fn catalog_uri(&self) -> String {
        self.core.target().uri()
    }

    /// Open and project one committed session through SQLite's read-only mode.
    ///
    /// The raw SQLite handle stays private so callers receive only the
    /// canonical [`lash_core_execution::SessionReadView`], which has no mutating store
    /// operations. This path does not mutate durable session, lease, claim, or
    /// graph state. SQLite may materialize its `-wal` and `-shm` wal-index
    /// sidecars while reading a cold WAL catalog. Consequently a catalog on
    /// read-only media is inspectable only when the required sidecars already
    /// exist; otherwise the SQLite failure surfaces as
    /// [`lash_core_execution::StoreError::Backend`]. `immutable=1` is deliberately not
    /// used because another process may still hold a writer.
    pub async fn open_read_only(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::SessionReadView>, lash_core_execution::StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Store::open_bound_readonly(&self.core, session_id)
            .await
            .map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))?;
        lash_core_execution::store::load_persisted_session_read_view(&store).await
    }
}

impl SqliteSessionStoreFactory {
    /// Concrete constructor behind [`SessionStoreFactory::create_store`]; the
    /// gated conformance factory shares it.
    #[expect(
        clippy::disallowed_methods,
        reason = "the sqlite store factory ensures the host-supplied store root exists before opening (FIG-2971)"
    )]
    pub(crate) async fn create_bound_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<Store>, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        if let Some(root) = self.core.target().file_path().and_then(Path::parent) {
            std::fs::create_dir_all(root).map_err(|err| StoreError::Backend(err.to_string()))?;
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                &request.session_id,
                self.options,
                Arc::clone(&self.clock),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?,
        );
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
            pending_observer_intents: request.pending_observer_intents.clone(),
        };
        let created_at_ms = self.clock.timestamp_ms();
        store
            .conn
            .write_flow(move |tx| {
                let deleted = tx
                    .query_row(
                        crate::session_sql::session_sql()
                            .deleted_sqlite
                            .exists
                            .sql(),
                        params![meta.session_id.as_str()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if deleted {
                    return Ok(TxOutcome::Rollback(Err(
                        lash_core_execution::StoreError::SessionDeleted {
                            session_id: meta.session_id,
                        },
                    )));
                }
                session_meta::write_session_meta(
                    tx,
                    &meta,
                    session_meta::SessionMetaWrite::Insert,
                    created_at_ms,
                )
                .map_err(sqlite_conversion_error)?;
                Ok(TxOutcome::Commit(Ok(())))
            })
            .await
            .map_err(sqlite_error)??;
        Ok(store)
    }

    /// Concrete reopen behind [`SessionStoreFactory::open_existing_store`];
    /// the gated conformance factory shares it.
    pub(crate) async fn open_existing_bound_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<Store>>, String> {
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                &request.session_id,
                self.options,
                Arc::clone(&self.clock),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| err.to_string())?,
        );
        if store
            .load_session_meta()
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(store))
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SqliteSessionStoreFactory {
    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        let catalog = self.core.target().canonical_name();
        *self
            .turn_cancel_closure_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(lash_core_execution::TurnCancelClosureOwnerBinding::new(
                format!("sqlite-catalog:{catalog}"),
                Arc::clone(effect_host),
            ));
        *self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(effect_host));
    }

    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
        process_engines: lash_core_execution::ProcessEngineRegistry,
    ) {
        *self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((process_env_store, process_engines));
    }

    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core_execution::store::RetentionBound,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::store::RetentionReport> {
        let report = crate::retention::reclaim(self, bound)
            .await
            .map_err(|failure| *failure)?;
        if let Err(error) = self.resume_artifact_owner_retirements().await {
            return Err(lash_core_execution::MaintenanceFailure::failed(
                error, report,
            ));
        }
        Ok(report)
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(self.create_bound_store(request).await? as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        lash_core_execution::store::validate_session_id(&request.session_id)
            .map_err(|error| error.to_string())?;
        Ok(self
            .open_existing_bound_store(request)
            .await?
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::SessionReadView>, lash_core_execution::StoreError> {
        self.open_read_only(session_id).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        if !self.core.target().exists() {
            return Ok(Vec::new());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let filter = filter.clone();
        conn.call(move |conn| list_session_summaries(conn, &filter))
            .await
            .map_err(sqlite_error)
    }

    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core_execution::store::UnsettledTurnCounts, StoreError> {
        if !self.core.target().exists() {
            return Ok(lash_core_execution::store::UnsettledTurnCounts::default());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        conn.read(|conn| {
            let (parked, oldest_since_ms, in_flight): (i64, Option<i64>, i64) = conn
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_unsettled_turns
                        .sql(),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let mut statement = conn
                .prepare(
                    crate::turn_ingress::turn_ingress_sql()
                        .family
                        .count_parks_by_reason
                        .sql(),
                )
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
            let mut parked_by_reason = std::collections::BTreeMap::new();
            for row in rows {
                let (code, count) = row.map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(sqlite_error(err)))
                })?;
                let Some(code) = lash_core_execution::store::ParkReasonCode::from_code(&code)
                else {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::StoredDataCorrupt {
                            record_kind: "TurnPark",
                            message: format!("stored park reason code `{code}` is unknown"),
                        },
                    )));
                };
                parked_by_reason.insert(code, usize::try_from(count).unwrap_or_default());
            }
            Ok(lash_core_execution::store::UnsettledTurnCounts {
                parked_turns: usize::try_from(parked).unwrap_or_default(),
                in_flight_turns: usize::try_from(in_flight).unwrap_or_default(),
                oldest_parked_since_ms: oldest_since_ms
                    .map(|ms| u64::try_from(ms).unwrap_or_default()),
                parked_by_reason,
            })
        })
        .await
        .map_err(sqlite_error)
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core_execution::store::TurnParkQuery,
    ) -> Result<Vec<lash_core_execution::store::TurnPark>, StoreError> {
        if !self.core.target().exists() {
            return Ok(Vec::new());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let limit = i64::try_from(query.limit.get()).unwrap_or(i64::MAX);
        let session = query.session.as_ref().map(|id| id.as_str().to_string());
        let at_or_before = query
            .parked_at_or_before_ms
            .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX));
        let (after_since, after_session) = match query.after.as_ref() {
            Some((since_ms, session_id)) => (
                Some(i64::try_from(*since_ms).unwrap_or(i64::MAX)),
                Some(session_id.as_str().to_string()),
            ),
            None => (None, None),
        };
        let reasons = query
            .reasons
            .as_ref()
            .filter(|reasons| !reasons.is_empty())
            .map(|reasons| {
                serde_json::to_string(&reasons.iter().map(|code| code.as_str()).collect::<Vec<_>>())
                    .map_err(|error| StoreError::Backend(error.to_string()))
            })
            .transpose()?;
        let rows = conn
            .call(move |conn| {
                let mut statement = conn.prepare(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks_sqlite
                        .list
                        .sql(),
                )?;
                let rows = statement.query_map(
                    params![
                        limit,
                        session,
                        at_or_before,
                        after_since,
                        after_session,
                        reasons
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                        ))
                    },
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(
                |(
                    session_id,
                    turn_id,
                    park_id,
                    reason_code,
                    reason_json,
                    since_ms,
                    last_refused_ms,
                    attempts,
                )| {
                    lash_core_execution::store::TurnPark::decode(
                        SessionId::from(session_id),
                        lash_sansio::TurnId::from(turn_id),
                        lash_core_execution::store::ParkId::from_feed_sequence(
                            u64::try_from(park_id).unwrap_or_default(),
                        ),
                        &reason_code,
                        &reason_json,
                        u64::try_from(since_ms).unwrap_or_default(),
                        u64::try_from(last_refused_ms).unwrap_or_default(),
                        u32::try_from(attempts).unwrap_or(u32::MAX),
                    )
                },
            )
            .collect()
    }

    async fn turn_park_feed(
        &self,
        after: lash_core_execution::store::TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash_core_execution::store::TurnParkFeedPage, StoreError> {
        self.read_turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_sansio::TurnId,
    ) -> Result<Option<lash_core_execution::store::RootTerminal>, StoreError> {
        self.read_root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core_execution::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::store::ControlIntent>, StoreError> {
        self.read_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core_execution::store::TurnParkFeedCursor,
    ) -> Result<(), StoreError> {
        if !self.core.target().exists() {
            return Ok(());
        }
        let conn =
            SqliteConnection::open_with_policy(self.core.target(), self.options.connection_policy)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        let through_seq = i64::try_from(through.store_sequence()).unwrap_or(i64::MAX);
        conn.write_flow(move |tx| {
            let sql = crate::turn_ingress::turn_ingress_sql();
            // The write lock is held, so this read is the clock's committed
            // sequence. `through` is clamped to it: raising the horizon past
            // `current_seq` would strand events the feed has not yet
            // appended.
            let through_seq = tx
                .query_row(sql.turn_park_clock.select_current.sql(), [], |row| {
                    row.get::<_, i64>(0)
                })?
                .min(through_seq);
            tx.execute(
                sql.turn_park_events.delete_events_through.sql(),
                params![through_seq],
            )?;
            tx.execute(
                sql.turn_park_clock.raise_compaction_horizon.sql(),
                params![through_seq],
            )?;
            Ok(TxOutcome::Commit(Ok(())))
        })
        .await
        .map_err(sqlite_error)?
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        if !self.core.target().exists() {
            return Ok(None);
        }
        let store = Arc::new(
            Store::open_bound_at(
                &self.core,
                session_id,
                self.options,
                Arc::clone(&self.clock),
                self.turn_cancel_closure_owner_binding(),
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?,
        );
        if store.load_session_meta().await?.is_none() {
            return Ok(None);
        }
        Ok(Some(store as Arc<dyn RuntimePersistence>))
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        let Some(store) = self.open_existing_store_by_id(session_id).await? else {
            return Ok(Vec::new());
        };
        store.pending_turn_cancel_closure_pins().await
    }

    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &lash_core_execution::ExecutionScope,
    ) -> Result<(), StoreError> {
        let scope = scope.clone();
        let scope_id = scope
            .journal_identity()
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .key()
            .to_string();
        let store = self
            .open_catalog_for_maintenance("turn cancellation scope retirement")
            .await?;
        let inspected_scope = scope.clone();
        store
            .conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    let mut statement = tx
                        .prepare(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .list_all
                                .sql(),
                        )
                        .map_err(sqlite_error)?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(sqlite_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sqlite_error)?;
                    drop(statement);
                    for (session_id, encoded) in rows {
                        let authorization: lash_core_execution::TurnCancelClosureAuthorization =
                            serde_json::from_str(&encoded).map_err(|error| {
                                StoreError::StoredDataCorrupt {
                                    record_kind: "TurnCancelClosureAuthorization",
                                    message: error.to_string(),
                                }
                            })?;
                        if authorization.admitted_scope() == &inspected_scope {
                            return Err(StoreError::TurnCancelClosureLifecyclePinned {
                                session_id: SessionId::from(session_id),
                                pending_count: 1,
                            });
                        }
                    }
                    tx.execute(
                        crate::turn_ingress::turn_ingress_sql()
                            .retired_scopes_sqlite
                            .insert_new
                            .sql(),
                        params![scope_id],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)??;
        if let Some(owner) = self.turn_cancel_closure_owner_binding() {
            owner
                .release(&scope)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        if !self.core.target().exists() {
            return Ok(Some(false));
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let session_id = request.session_id.clone();
        conn.call(move |conn| {
            conn.query_row(
                crate::turn_ingress::turn_ingress_sql()
                    .family
                    .has_claimable_work
                    .sql(),
                params![session_id.as_str(), now_epoch_ms as i64],
                |row| row.get(0),
            )
        })
        .await
        .map(Some)
        .map_err(sqlite_error)
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(|error| error.to_string())?;
        if !self.core.target().exists() {
            return Ok(false);
        }
        let conn =
            SqliteConnection::open_with_policy(self.core.target(), self.options.connection_policy)
                .await
                .map_err(|err| err.to_string())?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
            .await
            .map_err(|err| err.to_string())?;
        let session_id = SessionId::from(session_id.to_string());
        conn.call(move |conn| {
            conn.query_row(
                crate::session_sql::session_sql()
                    .deleted_sqlite
                    .exists
                    .sql(),
                params![session_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
        })
        .await
        .map_err(|err| err.to_string())
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
        lash_core_execution::store::validate_session_id(session_id)
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)?;
        let report = delete_session_from_catalog(
            &self.core,
            session_id,
            self.options.connection_policy,
            self.clock.timestamp_ms(),
        )
        .await?;
        if let Some(process_registry) = self.process_registry.as_ref() {
            delete_wake_allocation_floors_from_process_registry(
                process_registry,
                session_id,
                self.options.connection_policy,
            )
            .await
            .map_err(|message| {
                lash_core_execution::MaintenanceFailure::failed(
                    lash_core_execution::StoreError::Backend(message),
                    report.clone(),
                )
            })?;
        }
        Ok(report)
    }

    async fn pin(
        &self,
        node_id: &str,
    ) -> Result<lash_core_execution::ForkPoint, lash_core_execution::StoreError> {
        pin_in_catalog(&self.core, node_id, self.options.connection_policy).await
    }

    async fn unpin(&self, node_id: &str) -> Result<(), lash_core_execution::StoreError> {
        unpin_in_catalog(&self.core, node_id, self.options.connection_policy).await
    }

    async fn fork_points(
        &self,
    ) -> Result<Vec<lash_core_execution::ForkPoint>, lash_core_execution::StoreError> {
        fork_points_in_catalog(&self.core, self.options.connection_policy).await
    }

    async fn fork_at(
        &self,
        request: &lash_core_execution::ForkSessionRequest,
    ) -> Result<lash_core_execution::ForkSessionReceipt, lash_core_execution::StoreError> {
        fork_at_in_catalog(
            &self.core,
            request,
            self.clock.timestamp_ms(),
            self.options.connection_policy,
        )
        .await
    }
}

fn list_session_summaries(
    conn: &Connection,
    filter: &SessionListFilter,
) -> rusqlite::Result<Vec<SessionSummary>> {
    let mut stmt = conn.prepare(
        crate::session_sql::session_sql()
            .meta_sqlite
            .select_catalog
            .sql(),
    )?;
    let rows = stmt.query_map([], |row| {
        let stored = crate::session_meta::stored_relation_from_row(row)?;
        let relation = match stored.relation_kind.as_str() {
            "root" => SessionRelationKind::Root,
            "child" => SessionRelationKind::Child,
            "fork" => SessionRelationKind::Fork,
            other => {
                return Err(sqlite_conversion_error(stored_data_corrupt(
                    "SessionSummary",
                    format!("unknown relation_kind `{other}`"),
                )));
            }
        };
        let parent_session_id = stored.parent_session_id.clone();
        let deleted = row.get::<_, i64>(20)? != 0;
        let durable_relation = if deleted {
            None
        } else {
            Some(
                crate::session_meta::decode_catalog_relation(stored, &row.get::<_, String>(21)?)
                    .map_err(sqlite_conversion_error)?,
            )
        };
        Ok(SessionSummary {
            session_id: SessionId::from(row.get::<_, String>(0)?),
            created_at_ms: u64_from_sql("SessionSummary", "created_at_ms", row.get(17)?)?,
            last_commit_at_ms: row
                .get::<_, Option<i64>>(18)?
                .map(|value| u64_from_sql("SessionSummary", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionSummary", "head_revision", row.get(19)?)?,
            relation,
            durable_relation,
            parent_session_id,
            deleted,
        })
    })?;
    let mut summaries = Vec::new();
    for row in rows {
        let summary = row?;
        if filter.matches(&summary) {
            summaries.push(summary);
        }
    }
    Ok(summaries)
}

#[async_trait::async_trait]
impl lash_core_execution::AttachmentRootSet for SqliteSessionStoreFactory {
    fn can_prove_process_owner_death(&self) -> bool {
        self.process_registry.is_some()
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<
        std::collections::BTreeSet<lash_core_execution::AttachmentId>,
        lash_core_execution::StoreError,
    > {
        let catalog = self.core.target();
        if !catalog.exists() {
            return Err(lash_core_execution::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {catalog} does not exist, so live attachment refs cannot be enumerated"
            )));
        }
        let store = Store::open_at(
            &self.core,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry.as_ref(),
            self.turn_cancel_closure_owner_binding(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core_execution::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {catalog} could not be opened: {err}"
            ))
        })?;
        lash_core_execution::AttachmentManifest::forget_aged_uncommitted_intents(
            &store,
            intent_grace_cutoff_epoch_ms,
        )
        .await?;
        Ok(
            lash_core_execution::AttachmentManifest::list_all_refs(&store)
                .await?
                .into_iter()
                .collect(),
        )
    }

    async fn list_condemnations(
        &self,
    ) -> Result<
        Vec<lash_core_execution::AttachmentCondemnationRecord>,
        lash_core_execution::StoreError,
    > {
        let store = self
            .open_catalog_for_maintenance("condemnation enumeration")
            .await?;
        store.list_attachment_condemnations().await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core_execution::StoreError> {
        let store = self.open_catalog_for_maintenance("root re-check").await?;
        lash_core_execution::AttachmentManifest::has_live_ref_for_id(
            &store,
            id,
            intent_grace_cutoff_epoch_ms,
        )
        .await
    }

    fn fence(&self) -> lash_core_execution::AttachmentGcFence {
        lash_core_execution::AttachmentGcFence::Fenced
    }

    async fn condemn_attachment(
        &self,
        id: &lash_core_execution::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core_execution::AttachmentCondemnation, lash_core_execution::StoreError> {
        let store = self.open_catalog_for_maintenance("condemnation").await?;
        store
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn arm_attachment_delete(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, lash_core_execution::StoreError> {
        let store = self.open_catalog_for_maintenance("delete arming").await?;
        store.arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("condemnation release")
            .await?;
        store.release_attachment_condemnation(id).await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("abandoned attachment write recovery")
            .await?;
        store.recover_abandoned_attachment_write(id).await
    }

    async fn retire_attachment_condemnation(
        &self,
        id: &lash_core_execution::AttachmentId,
    ) -> Result<(), lash_core_execution::StoreError> {
        let store = self
            .open_catalog_for_maintenance("condemnation retirement")
            .await?;
        store.retire_attachment_condemnation(id).await
    }
}

fn retained_artifact_refs(checkpoint: &SessionCheckpoint) -> Vec<RetainedArtifactRef> {
    checkpoint
        .components
        .values()
        .map(|descriptor| RetainedArtifactRef {
            blob_ref: descriptor.blob_ref.clone(),
            kind: PersistedArtifactKind::CheckpointComponent,
        })
        .collect()
}

#[cfg(test)]
mod graph_error_tests;
#[cfg(test)]
mod read_failure_tests;
#[cfg(test)]
mod rendered_sql_pin_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
