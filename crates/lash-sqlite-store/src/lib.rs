//! # lash-sqlite-store
//!
//! The high-performance local **durable** persistence backend for the lash
//! agent runtime. One factory-wide SQLite durable-core database, opened in WAL journal mode
//! with a 15-second busy timeout, satisfying the full [`RuntimePersistence`] +
//! [`AttachmentManifest`] contract from `lash-core`.
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
//! ## Schema cutover, not migrations
//!
//! There is exactly one supported schema (see [`schema::SCHEMA`]). Older
//! databases must be deleted before opening — we do not carry migration code.
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
//! [`RuntimePersistence`]: lash_core::RuntimePersistence
//! [`AttachmentManifest`]: lash_core::AttachmentManifest

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use lash_core::runtime::{
    QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkClaimPolicy, QueuedWorkCompletion, QueuedWorkEnqueueOutcome,
    QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, prepare_process_event_append,
    prepare_process_registration,
};
use lash_core::store::queued_work::{
    ClaimCandidate, WorkClaimLease, claim_scan_limit, derive_batch_id,
    select_exact_turn_work_claim_prefix, select_leading_session_command,
    select_turn_work_claim_prefix,
};
use lash_core::store::{
    HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit, RuntimeCommitResult,
    SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, BlobRef, DeliveryPolicy, GcReport, LeaseOwnerIdentity,
    PersistedSegmentHandover, ProcessAwaitOutput, ProcessChange, ProcessChangeCursor,
    ProcessContinuationStore, ProcessEvent, ProcessEventAppendRequest, ProcessEventAppendResult,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessLiveReferenceSummary, ProcessObserverBy,
    ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome,
    ProcessStarted, QueuedWorkStore, RuntimePersistence, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionMeta,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, StoreMaintenance, TurnInputStore,
    VacuumReport, facade_support::ProcessStartPlan, facade_support::registry_transitions,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use conn::SqliteConnection;

mod attachments;
mod await_event;
mod blobs;
mod conn;

fn commit_count_entropy_seed() -> u64 {
    let (high, low) = uuid::Uuid::new_v4().as_u64_pair();
    (high ^ low) & (u64::MAX >> 1)
}
mod effect_replay;
mod forks;
mod graph;
mod lifecycle;
mod pending_turn_inputs;
mod persistence;
mod process_registry;
mod process_registry_change;
mod process_registry_completion;
mod queued_work;
mod schema;
mod session_meta;
#[cfg(feature = "testing")]
pub mod testing;
mod triggers;

use conn::TxOutcome;
pub use effect_replay::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteRuntimeEffectController,
};
use forks::*;
use pending_turn_inputs::*;
use queued_work::*;
use schema::{
    StoreBacking, apply_pragmas, ensure_effect_schema, ensure_process_schema, ensure_schema,
    ensure_trigger_schema,
};
pub use triggers::SqliteTriggerStore;

/// SQLite-backed store for checkpoint blobs, runtime session state, and
/// Lashlang artifacts.
///
/// This is the first-party local implementation of the runtime store traits.
/// Internally it holds a single cloneable [`SqliteConnection`] (a
/// tokio-rusqlite handle to one database thread).
pub struct Store {
    conn: SqliteConnection,
    session_id: OnceLock<String>,
    clock: Arc<dyn lash_core::Clock>,
    artifact_cache: Mutex<BTreeMap<lashlang::ModuleRef, Arc<lashlang::ModuleArtifact>>>,
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
    #[doc(hidden)]
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
    clock: Arc<dyn lash_core::Clock>,
    process_session_store_root: Option<PathBuf>,
    wake_delivery_config: lash_core::WakeDeliveryConfig,
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

fn sqlite_graph_node_insert_error(
    err: rusqlite::Error,
    session_id: &str,
    generation: u64,
    node_id: &str,
) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, message) = &err
        && code.code == rusqlite::ErrorCode::ConstraintViolation
    {
        let message = message.as_deref().unwrap_or_default();
        if message.contains("graph_nodes.session_id, graph_nodes.generation") {
            return StoreError::GraphGenerationCollision {
                session_id: session_id.to_string(),
                generation,
            };
        }
        if message.contains("graph_nodes.node_id") {
            return StoreError::NodeIdCollision {
                node_id: node_id.to_string(),
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
) -> Result<u64, lash_core::PluginError> {
    u64::try_from(value).map_err(|_| lash_core::PluginError::StoredDataCorrupt {
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

fn sql_claim_fencing_tokens(
    counter: &'static str,
    currents: impl IntoIterator<Item = u64>,
) -> Result<Vec<i64>, StoreError> {
    currents
        .into_iter()
        .map(|current| {
            let next = StoreError::checked_monotonic_increment(counter, current)?;
            sql_monotonic_counter_value(counter, current, next)
        })
        .collect()
}

fn plugin_sql_monotonic_counter_value(
    counter: &'static str,
    current: u64,
    value: u64,
) -> Result<i64, lash_core::PluginError> {
    i64::try_from(value).map_err(|_| lash_core::PluginError::MonotonicCounterOverflow {
        counter: counter.to_string(),
        current,
    })
}

fn plugin_sql_counter_value(
    counter: &'static str,
    value: u64,
) -> Result<i64, lash_core::PluginError> {
    i64::try_from(value).map_err(|_| lash_core::PluginError::MonotonicCounterOverflow {
        counter: counter.to_string(),
        current: value,
    })
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
    fn bind_session(&self, session_id: &str) -> Result<(), StoreError> {
        if let Some(bound_session_id) = self.session_id.get() {
            if bound_session_id != session_id {
                return Err(StoreError::SessionBindingMismatch {
                    bound_session_id: bound_session_id.clone(),
                    attempted_session_id: session_id.to_string(),
                });
            }
            return Ok(());
        }
        let _ = self.session_id.set(session_id.to_string());
        if self
            .session_id
            .get()
            .is_some_and(|bound| bound == session_id)
        {
            Ok(())
        } else {
            Err(StoreError::SessionBindingMismatch {
                bound_session_id: self.session_id.get().cloned().unwrap_or_default(),
                attempted_session_id: session_id.to_string(),
            })
        }
    }

    fn selected_session_id(&self) -> Result<String, StoreError> {
        self.session_id
            .get()
            .cloned()
            .ok_or(StoreError::SessionNotBound)
    }

    async fn resolve_session_id_for_read(&self) -> Result<Option<String>, StoreError> {
        if let Some(session_id) = self.session_id.get() {
            return Ok(Some(session_id.clone()));
        }
        let session_ids = self
            .conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT session_id FROM (
                         SELECT session_id FROM (
                             SELECT session_id FROM session_head
                             LIMIT 2
                         )
                         UNION
                         SELECT session_id FROM (
                             SELECT session_id FROM session_meta
                             LIMIT 2
                         )
                     )
                     LIMIT 2",
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
        self.bind_session(&session_ids[0])?;
        Ok(self.session_id.get().cloned())
    }
}

fn process_sqlite_error(err: rusqlite::Error) -> lash_core::PluginError {
    lash_core::PluginError::Session(err.to_string())
}

fn process_decode_error(err: serde_json::Error) -> lash_core::PluginError {
    lash_core::PluginError::Session(format!("failed to decode process registry row: {err}"))
}

fn process_encode_json<T: serde::Serialize>(value: &T) -> Result<String, lash_core::PluginError> {
    serde_json::to_string(value).map_err(|err| {
        lash_core::PluginError::Session(format!("failed to encode process row: {err}"))
    })
}

fn block_on_store<T>(future: impl std::future::Future<Output = T>) -> T {
    futures_executor::block_on(future)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PersistedArtifactKind {
    GenericBlob,
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlobArtifactDescriptor {
    pub kind: PersistedArtifactKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<BlobStorageHint>,
}

impl BlobArtifactDescriptor {
    pub fn new(kind: PersistedArtifactKind, hints: impl Into<Vec<BlobStorageHint>>) -> Self {
        Self {
            kind,
            hints: hints.into(),
        }
    }

    pub fn checkpoint_manifest() -> Self {
        Self::new(
            PersistedArtifactKind::CheckpointManifest,
            vec![BlobStorageHint::Compressible],
        )
    }

    pub fn checkpoint_component() -> Self {
        Self::new(
            PersistedArtifactKind::CheckpointComponent,
            vec![BlobStorageHint::Compressible, BlobStorageHint::LargePayload],
        )
    }

    pub fn lashlang_module() -> Self {
        Self::new(
            PersistedArtifactKind::LashlangModule,
            vec![BlobStorageHint::Compressible, BlobStorageHint::LargePayload],
        )
    }

    pub fn process_execution_env() -> Self {
        Self::new(
            PersistedArtifactKind::ProcessExecutionEnv,
            vec![BlobStorageHint::Compressible],
        )
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
pub struct StoreGcPolicy {
    pub auto_run_every_commits: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreOptions {
    pub blob_profile: BuiltinBlobProfile,
    pub gc_policy: StoreGcPolicy,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredBlobEnvelope {
    descriptor: BlobArtifactDescriptor,
    compression: BlobCompression,
    #[serde(with = "serde_bytes")]
    content: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct StoredSessionCheckpoint {
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
}

/// Explicit first-party factory for one SQLite durable-core catalog.
///
/// Hosts opt into this by passing it to `lash::LashCoreBuilder::store_factory`.
/// The factory never becomes a default: app storage and runtime storage remain
/// host-owned decisions.
#[derive(Clone, Debug)]
pub struct SqliteSessionStoreFactory {
    root: PathBuf,
    process_registry_path: Option<PathBuf>,
    options: StoreOptions,
    clock: Arc<dyn lash_core::Clock>,
    #[cfg(feature = "testing")]
    fault_injector: Option<testing::SqliteFaultInjector>,
}

impl SqliteSessionStoreFactory {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        warn_process_registry_not_wired();
        Self {
            root,
            process_registry_path: None,
            options: StoreOptions::default(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            #[cfg(feature = "testing")]
            fault_injector: None,
        }
    }

    pub fn with_options(root: impl Into<PathBuf>, options: StoreOptions) -> Self {
        let root = root.into();
        warn_process_registry_not_wired();
        Self {
            root,
            process_registry_path: None,
            options,
            clock: Arc::new(lash_core::facade_support::SystemClock),
            #[cfg(feature = "testing")]
            fault_injector: None,
        }
    }

    /// Construct a factory with explicit process-owner liveness wiring for
    /// attachment GC. This is the warning-free durable constructor when the
    /// deployment uses a Lash SQLite process registry.
    pub fn new_with_process_registry(
        root: impl Into<PathBuf>,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: root.into(),
            process_registry_path: Some(process_registry_path.into()),
            options: StoreOptions::default(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            #[cfg(feature = "testing")]
            fault_injector: None,
        }
    }

    pub fn with_options_and_process_registry(
        root: impl Into<PathBuf>,
        options: StoreOptions,
        process_registry_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: root.into(),
            process_registry_path: Some(process_registry_path.into()),
            options,
            clock: Arc::new(lash_core::facade_support::SystemClock),
            #[cfg(feature = "testing")]
            fault_injector: None,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Install a per-factory substrate fault injector for simulation/tests.
    ///
    /// The method and backing field do not exist without the `testing` feature.
    #[cfg(feature = "testing")]
    pub fn with_fault_injector(mut self, injector: testing::SqliteFaultInjector) -> Self {
        self.fault_injector = Some(injector);
        self
    }

    /// Path to the one durable-core database shared by every session created
    /// through this factory.
    pub fn catalog_path(&self) -> PathBuf {
        self.root.join("durable-core.db")
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SqliteSessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        lash_core::store::validate_session_id(&request.session_id)?;
        std::fs::create_dir_all(&self.root).map_err(|err| StoreError::Backend(err.to_string()))?;
        let path = self.catalog_path();
        let store = Arc::new(
            Store::open_bound_with_options_clock_and_process_registry(
                &path,
                &request.session_id,
                self.options,
                Arc::clone(&self.clock),
                None,
                #[cfg(feature = "testing")]
                self.fault_injector.clone(),
            )
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?,
        );
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
        };
        store
            .conn
            .write_flow(move |tx| {
                let deleted = tx
                    .query_row(
                        "SELECT 1 FROM deleted_sessions WHERE session_id = ?1",
                        params![meta.session_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if deleted {
                    return Ok(TxOutcome::Rollback(Err(
                        lash_core::StoreError::SessionDeleted {
                            session_id: meta.session_id,
                        },
                    )));
                }
                session_meta::write_session_meta(tx, &meta, session_meta::SessionMetaWrite::Insert)
                    .map_err(sqlite_conversion_error)?;
                Ok(TxOutcome::Commit(Ok(())))
            })
            .await
            .map_err(sqlite_error)??;
        Ok(store as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(None);
        }
        let store = Arc::new(
            Store::open_bound_with_options_clock_and_process_registry(
                &path,
                &request.session_id,
                self.options,
                Arc::clone(&self.clock),
                None,
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
        Ok(Some(store as Arc<dyn RuntimePersistence>))
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(Some(false));
        }
        let conn = SqliteConnection::open_readonly(&path)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let session_id = request.session_id.clone();
        conn.call(move |conn| {
            conn.query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM queued_work_batches qwb
                    WHERE qwb.session_id = ?1
                      AND qwb.available_at_ms <= ?2
                ) OR EXISTS(
                    SELECT 1
                    FROM pending_turn_inputs pti
                    WHERE pti.session_id = ?1
                      AND pti.state = ?3
                )",
                params![
                    session_id,
                    now_epoch_ms as i64,
                    lash_core::TurnInputState::DeferredNextTurn.as_str()
                ],
                |row| row.get(0),
            )
        })
        .await
        .map(Some)
        .map_err(sqlite_error)
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(false);
        }
        let conn = SqliteConnection::open(&path)
            .await
            .map_err(|err| err.to_string())?;
        ensure_schema(&conn).await.map_err(|err| err.to_string())?;
        let session_id = session_id.to_string();
        conn.call(move |conn| {
            conn.query_row(
                "SELECT 1 FROM deleted_sessions WHERE session_id = ?1",
                params![session_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
        })
        .await
        .map_err(|err| err.to_string())
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        delete_session_from_catalog(&self.root, session_id, true).await?;
        if let Some(process_registry_path) = self.process_registry_path.as_deref() {
            delete_wake_allocation_floors_from_process_registry(process_registry_path, session_id)
                .await?;
        }
        Ok(())
    }

    async fn pin(&self, node_id: &str) -> Result<lash_core::ForkPoint, lash_core::StoreError> {
        pin_in_catalog(&self.root, node_id).await
    }

    async fn unpin(&self, node_id: &str) -> Result<(), lash_core::StoreError> {
        unpin_in_catalog(&self.root, node_id).await
    }

    async fn fork_points(&self) -> Result<Vec<lash_core::ForkPoint>, lash_core::StoreError> {
        fork_points_in_catalog(&self.root).await
    }

    async fn fork_at(
        &self,
        request: &lash_core::ForkSessionRequest,
    ) -> Result<lash_core::ForkSessionResult, lash_core::StoreError> {
        fork_at_in_catalog(&self.root, request).await
    }
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for SqliteSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Err(lash_core::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {} does not exist, so live attachment refs cannot be enumerated",
                path.display()
            )));
        }
        let store = Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path.as_deref(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core::StoreError::Backend(format!(
                "attachment GC aborted: durable-core catalog {} could not be opened: {err}",
                path.display()
            ))
        })?;
        lash_core::AttachmentManifest::forget_aged_uncommitted_intents(
            &store,
            intent_grace_cutoff_epoch_ms,
        )?;
        Ok(lash_core::AttachmentManifest::list_all_refs(&store)?
            .into_iter()
            .collect())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core::StoreError> {
        let store = self.open_catalog_for_attachment_gc("root re-check").await?;
        lash_core::AttachmentManifest::has_live_ref_for_id(&store, id, intent_grace_cutoff_epoch_ms)
    }

    fn fence(&self) -> lash_core::AttachmentGcFence {
        lash_core::AttachmentGcFence::Fenced
    }

    async fn condemn_attachment(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core::AttachmentCondemnation, lash_core::StoreError> {
        let store = self.open_catalog_for_attachment_gc("condemnation").await?;
        store
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn arm_attachment_delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::AttachmentDeleteArming, lash_core::StoreError> {
        let store = self.open_catalog_for_attachment_gc("delete arming").await?;
        store.arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::StoreError> {
        let store = self
            .open_catalog_for_attachment_gc("condemnation release")
            .await?;
        store.release_attachment_condemnation(id).await
    }
}

impl SqliteSessionStoreFactory {
    /// Open the factory-wide catalog that owns both halves of the attachment GC
    /// fence — the manifest rows and the condemnation state.
    async fn open_catalog_for_attachment_gc(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core::StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Err(lash_core::StoreError::Backend(format!(
                "attachment GC {operation} aborted: durable-core catalog {} does not exist",
                path.display()
            )));
        }
        Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path.as_deref(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core::StoreError::Backend(format!(
                "attachment GC {operation} aborted: durable-core catalog {} could not be opened: {err}",
                path.display()
            ))
        })
    }
}

fn warn_process_registry_not_wired() {
    tracing::warn!(
        "SQLite attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call SqliteSessionStoreFactory::new_with_process_registry(...)."
    );
}

async fn delete_session_from_catalog(
    root: &Path,
    session_id: &str,
    tombstone_host_facing_id: bool,
) -> Result<(), String> {
    let path = root.join("durable-core.db");
    if !path.exists() {
        return Ok(());
    }
    let session_id = session_id.to_string();
    let conn = SqliteConnection::open(&path)
        .await
        .map_err(|err| err.to_string())?;
    ensure_schema(&conn).await.map_err(|err| err.to_string())?;
    conn.write_flow(move |tx| {
        let outcome: Result<(), lash_core::StoreError> = (|| {
            let existed = tx
                .query_row(
                    "SELECT 1 FROM session_meta WHERE session_id = ?1
                     UNION ALL
                     SELECT 1 FROM session_head WHERE session_id = ?1
                     LIMIT 1",
                    params![session_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if tombstone_host_facing_id && existed {
                // Permanent identity evidence for host-facing ids only.
                // Runtime-internal process session ids are lash-minted and
                // reclaimed without a tombstone.
                tx.execute(
                    "INSERT OR IGNORE INTO deleted_sessions (session_id) VALUES (?1)",
                    params![session_id],
                )
                .map_err(sqlite_error)?;
            }
            let leaf_node_id = tx
                .query_row(
                    "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .flatten();
            tx.execute(
                "DELETE FROM session_head WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(sqlite_error)?;
            if let Some(leaf_node_id) = leaf_node_id {
                persistence::retire_unreachable_ancestry_conn(tx, &leaf_node_id)?;
            }
            let unreachable_candidates = {
                let mut stmt = tx
                    .prepare(
                        "SELECT g.node_id FROM graph_nodes AS g
                         WHERE g.session_id = ?1 AND g.tombstoned = 0
                           AND NOT EXISTS (
                               SELECT 1 FROM graph_nodes AS child
                               WHERE child.parent_node_id = g.node_id
                                 AND child.tombstoned = 0
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM session_head AS head
                               WHERE head.leaf_node_id = g.node_id
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM node_anchors AS anchor
                               WHERE anchor.node_id = g.node_id
                           )
                         ORDER BY g.seq DESC",
                    )
                    .map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(params![session_id], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
            };
            for node_id in unreachable_candidates {
                persistence::retire_unreachable_ancestry_conn(tx, &node_id)?;
            }
            // Delete-time reclaim covers this session's tombstoned rows plus any
            // tombstoned row owned by an already-deleted session. A node can be
            // tombstoned *after* its owner is gone (unpin of a pinned leaf whose
            // session was deleted, or ancestry retired at a fork child's delete),
            // and no session-scoped vacuum could ever reach it: the owning id is
            // permanently unbindable. Live sessions' rows stay resident for their
            // own vacuum, so this is not a catalog-wide sweep.
            tx.execute(
                "DELETE FROM graph_nodes
                 WHERE tombstoned = 1
                   AND (session_id = ?1
                        OR session_id IN (SELECT session_id FROM deleted_sessions))",
                params![session_id],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM fork_lineage WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM queued_work_batches WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM wake_redelivery_fences WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(sqlite_error)?;
            for table in [
                "pending_turn_inputs",
                "attachment_manifest",
                "runtime_turn_commits",
                "session_execution_leases",
                "usage_deltas",
                "session_meta",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    params![session_id],
                )
                .map_err(sqlite_error)?;
            }
            // Trigger manifests are the one artifact-ref namespace with an exact
            // session owner. Module, raw-artifact, and process-environment refs are
            // content-addressed factory services with no safe session attribution;
            // their lifecycle remains owned by the host-facing artifact APIs.
            tx.execute(
                "DELETE FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2",
                params![
                    attachments::CURRENT_TRIGGER_MANIFEST_NAMESPACE,
                    lash_core::TriggerOwnerScope::session(session_id).namespace()
                ],
            )
            .map_err(sqlite_error)?;
            // Session deletion used to unlink the whole per-session file. Preserve
            // that reclaiming behavior for rows whose ownership is now explicit;
            // global artifact refs above remain roots.
            Store::gc_unreachable_in_tx(tx)
                .map_err(|err| lash_core::StoreError::Backend(err.to_string()))?;
            Ok(())
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => TxOutcome::Rollback(Err(err)),
        })
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(|err| err.to_string())
}

async fn delete_wake_allocation_floors_from_process_registry(
    process_registry_path: &Path,
    target_session_id: &str,
) -> Result<(), String> {
    if !process_registry_path.exists() {
        return Ok(());
    }
    let conn = SqliteConnection::open(process_registry_path)
        .await
        .map_err(|err| err.to_string())?;
    ensure_process_schema(&conn)
        .await
        .map_err(|err| err.to_string())?;
    let target_session_id = target_session_id.to_string();
    conn.write_flow(move |tx| {
        let outcome = tx
            .execute(
                "DELETE FROM wake_allocation_floors WHERE target_session_id = ?1",
                params![target_session_id],
            )
            .map(|_| ())
            .map_err(sqlite_error);
        Ok(match outcome {
            Ok(()) => TxOutcome::Commit(Ok(())),
            Err(error) => TxOutcome::Rollback(Err(error)),
        })
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(|err| err.to_string())
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

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: "persisted JSON record".to_string(),
        message: error.to_string(),
    })
}

fn should_compress_blob(
    profile: BuiltinBlobProfile,
    descriptor: &BlobArtifactDescriptor,
    len: usize,
) -> bool {
    if !descriptor.hints.contains(&BlobStorageHint::Compressible) {
        return false;
    }
    match profile {
        BuiltinBlobProfile::LowLatency => false,
        BuiltinBlobProfile::Balanced => len >= 4 * 1024,
        BuiltinBlobProfile::Compact => len >= 1024,
    }
}

fn compress_blob(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, content).map_err(|error| {
        StoreError::RecordEncodingFailed {
            record_kind: "compressed artifact blob".to_string(),
            message: error.to_string(),
        }
    })?;
    encoder
        .finish()
        .map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "compressed artifact blob".to_string(),
            message: error.to_string(),
        })
}

fn decompress_blob(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut decoder = ZlibDecoder::new(content);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out)
        .map_err(|error| stored_data_corrupt("compressed artifact blob", error))?;
    Ok(out)
}

fn encode_artifact_blob(
    descriptor: &BlobArtifactDescriptor,
    profile: BuiltinBlobProfile,
    content: &[u8],
) -> Result<Vec<u8>, StoreError> {
    let (compression, stored_content) = if should_compress_blob(profile, descriptor, content.len())
    {
        (BlobCompression::Zlib, compress_blob(content)?)
    } else {
        (BlobCompression::None, content.to_vec())
    };
    encode_msgpack(
        &StoredBlobEnvelope {
            descriptor: descriptor.clone(),
            compression,
            content: stored_content,
        },
        "SQLite stored blob envelope",
    )
}

fn decode_artifact_blob(bytes: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
    let Some(envelope) = decode_msgpack::<StoredBlobEnvelope>(bytes) else {
        return Ok(None);
    };
    match envelope.compression {
        BlobCompression::None => Ok(Some(envelope.content)),
        BlobCompression::Zlib => decompress_blob(&envelope.content).map(Some),
    }
}

/// Read the session head meta off a raw connection. Synchronous because it runs
/// inside a `conn.call`/`conn.write` closure on the connection thread.
fn try_load_session_head_meta_from_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let row = conn
        .query_row(
            "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref
             FROM session_head WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((head_json, head_revision, leaf_node_id, checkpoint_ref)) = row else {
        return Ok(None);
    };
    let payload: SessionHeadPayload = lash_core::store::decode_versioned_json_record(
        &head_json,
        "SessionHeadMeta",
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .map_err(|error| map_record_decode_error("SessionHeadMeta", error))?;
    Ok(Some(SessionHeadMeta::assemble(
        payload,
        u64::try_from(head_revision).map_err(|_| {
            stored_data_corrupt(
                "SessionHeadMeta",
                format!("head_revision must be non-negative, got {head_revision}"),
            )
        })?,
        checkpoint_ref.map(Into::into),
        leaf_node_id,
    )))
}

fn decode_checkpoint(bytes: &[u8]) -> Result<SessionCheckpoint, StoreError> {
    let value: serde_json::Value = rmp_serde::from_slice(bytes)
        .map_err(|err| stored_data_corrupt("SessionCheckpoint", err))?;
    lash_core::store::ensure_supported_record_schema_version(
        "SessionCheckpoint",
        &value,
        lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
    )?;
    rmp_serde::from_slice(bytes).map_err(|err| stored_data_corrupt("SessionCheckpoint", err))
}

fn encode_msgpack<T: serde::Serialize>(
    value: &T,
    record_kind: &str,
) -> Result<Vec<u8>, StoreError> {
    // Pre-size the buffer so the per-byte writes inside rmp_serde don't
    // walk the Vec through 0→4→8→16→32… reallocations on every call.
    let mut buf = Vec::with_capacity(1024);
    rmp_serde::encode::write_named(&mut buf, value).map_err(|error| {
        StoreError::RecordEncodingFailed {
            record_kind: record_kind.to_string(),
            message: error.to_string(),
        }
    })?;
    Ok(buf)
}

fn decode_msgpack<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    rmp_serde::from_slice(bytes).ok()
}

#[cfg(test)]
mod graph_error_tests;
#[cfg(test)]
mod read_failure_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
