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
use lash_core::runtime::ProcessHandleGrantEntry;
use lash_core::runtime::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkCompletion, QueuedWorkItem, QueuedWorkPayload, prepare_process_event_append,
    prepare_process_registration,
};
use lash_core::store::queued_work::{
    ClaimCandidate, QueuedWorkClaimLease, claim_scan_limit, derive_batch_id,
    ensure_completion_owns_all_batches, select_leading_session_command,
    select_turn_work_claim_prefix,
};
use lash_core::store::{
    GraphCommitDelta, HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit,
    RuntimeCommitResult, SessionCheckpoint, SessionHeadMeta,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, BlobRef, DeliveryPolicy, GcReport, LeaseOwnerIdentity, LeaseOwnerLiveness,
    MergeKey, NodeRefcountVerification, PROCESS_LEASE_SCHEMA_VERSION, PersistedSegmentHandover,
    ProcessAwaitOutput, ProcessChangeCursor, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessHandleDescriptor, ProcessHandleGrant, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessLiveReferenceSummary, ProcessPruneReport,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome, ProcessStartPlan,
    ProcessStarted, QueuedWorkStore, RuntimePersistence, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion, SessionExecutionLeaseFence,
    SessionExecutionLeaseStore, SessionMeta, SessionPickerInfo, SessionReadScope, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy, StoreError, StoreMaintenance,
    TurnInputStore, VacuumReport,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use conn::SqliteConnection;

mod attachments;
mod await_event;
mod blobs;
mod conn;
mod effect_replay;
mod forks;
mod graph;
mod leases;
mod lifecycle;
mod pending_turn_inputs;
mod persistence;
mod process_registry;
mod process_registry_change;
mod process_registry_completion;
mod queued_work;
mod schema;
mod triggers;

use conn::TxOutcome;
pub use effect_replay::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteRuntimeEffectController,
};
use forks::*;
use leases::*;
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

/// SQLite-backed process registry for one configured runtime deployment.
///
/// It is intentionally separate from [`Store`]: the durable-core catalog
/// persists conversations, while this registry persists background process
/// state and handle visibility across all sessions sharing the registry.
pub struct SqliteProcessRegistry {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core::Clock>,
    process_session_store_root: Option<PathBuf>,
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
            Err(error) => StoreError::Backend(error.to_string()),
        },
        err => StoreError::Backend(err.to_string()),
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
        self.session_id.get().cloned().ok_or_else(|| {
            StoreError::Backend(
                "SQLite durable-core store is not bound to a session; use SqliteSessionStoreFactory"
                    .to_string(),
            )
        })
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
                         SELECT session_id FROM session_head
                         UNION
                         SELECT session_id FROM session_meta
                     )
                     ORDER BY session_id ASC
                     LIMIT 2",
                )?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        if session_ids.len() != 1 {
            return Ok(None);
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
    ToolState,
    PluginSessionSnapshot,
    ExecutionStateSnapshot,
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

    pub fn tool_state_snapshot() -> Self {
        Self::new(
            PersistedArtifactKind::ToolState,
            vec![BlobStorageHint::Compressible, BlobStorageHint::LargePayload],
        )
    }

    pub fn plugin_session_snapshot() -> Self {
        Self::new(
            PersistedArtifactKind::PluginSessionSnapshot,
            vec![BlobStorageHint::Compressible, BlobStorageHint::LargePayload],
        )
    }

    pub fn execution_state_snapshot() -> Self {
        Self::new(
            PersistedArtifactKind::ExecutionStateSnapshot,
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
}

impl SqliteSessionStoreFactory {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        warn_process_registry_not_wired();
        Self {
            root,
            process_registry_path: None,
            options: StoreOptions::default(),
            clock: Arc::new(lash_core::SystemClock),
        }
    }

    pub fn with_options(root: impl Into<PathBuf>, options: StoreOptions) -> Self {
        let root = root.into();
        warn_process_registry_not_wired();
        Self {
            root,
            process_registry_path: None,
            options,
            clock: Arc::new(lash_core::SystemClock),
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
            clock: Arc::new(lash_core::SystemClock),
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
            clock: Arc::new(lash_core::SystemClock),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
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
    ) -> Result<Arc<dyn RuntimePersistence>, String> {
        std::fs::create_dir_all(&self.root).map_err(|err| err.to_string())?;
        let path = self.catalog_path();
        let store = Arc::new(
            Store::open_bound_with_options_clock_and_process_registry(
                &path,
                &request.session_id,
                self.options,
                Arc::clone(&self.clock),
                None,
            )
            .await
            .map_err(|err| err.to_string())?,
        );
        if store.load_session_meta().await.is_none() {
            store
                .save_session_meta(SessionMeta {
                    session_id: request.session_id.clone(),
                    session_name: request.session_id.clone(),
                    created_at: self.clock.timestamp_rfc3339(),
                    model: request.policy.model.id.clone(),
                    cwd: std::env::current_dir()
                        .ok()
                        .and_then(|path| path.to_str().map(str::to_string)),
                    relation: request.relation.clone(),
                })
                .await;
        }
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
            )
            .await
            .map_err(|err| err.to_string())?,
        );
        if store.load_session_meta().await.is_none() {
            return Ok(None);
        }
        Ok(Some(store as Arc<dyn RuntimePersistence>))
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        delete_session_from_catalog(&self.root, session_id).await
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

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(std::collections::BTreeSet::new());
        }
        let store = Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path.as_deref(),
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
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(false);
        }
        let store = Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path.as_deref(),
        )
        .await
        .map_err(|err| {
            lash_core::StoreError::Backend(format!(
                "attachment GC root re-check aborted: durable-core catalog {} could not be opened: {err}",
                path.display()
            ))
        })?;
        lash_core::AttachmentManifest::has_live_ref_for_id(&store, id, intent_grace_cutoff_epoch_ms)
    }
}

fn warn_process_registry_not_wired() {
    tracing::warn!(
        "SQLite attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call SqliteSessionStoreFactory::new_with_process_registry(...)."
    );
}

async fn delete_session_from_catalog(root: &Path, session_id: &str) -> Result<(), String> {
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
                persistence::decrement_node_ref_conn(tx, &leaf_node_id)?;
            }
            let zero_ref_nodes = {
                let mut stmt = tx
                    .prepare(
                        "SELECT node_id, parent_node_id FROM graph_nodes
                     WHERE session_id = ?1 AND tombstoned = 0 AND incoming_refs = 0
                     ORDER BY seq DESC",
                    )
                    .map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(params![session_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })
                    .map_err(sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
            };
            for (node_id, parent_node_id) in zero_ref_nodes {
                let cached = tx
                    .query_row(
                        "SELECT incoming_refs FROM graph_nodes
                     WHERE node_id = ?1 AND tombstoned = 0",
                        params![node_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(sqlite_error)?;
                if cached != Some(0) {
                    continue;
                }
                let derived = persistence::derived_node_refcount_conn(tx, &node_id)?;
                if derived != 0 {
                    return Err(lash_core::StoreError::NodeRefcountDrift {
                        node_id,
                        cached: 0,
                        derived,
                    });
                }
                tx.execute(
                    "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                    params![node_id],
                )
                .map_err(sqlite_error)?;
                if let Some(parent_node_id) = parent_node_id {
                    persistence::decrement_node_ref_conn(tx, &parent_node_id)?;
                }
            }
            tx.execute("DELETE FROM graph_nodes WHERE tombstoned = 1", [])
                .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM queued_work_batches WHERE session_id = ?1",
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
                    format!("session:{session_id}")
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

fn retained_artifact_refs(checkpoint: &SessionCheckpoint) -> Vec<RetainedArtifactRef> {
    let mut refs = Vec::new();
    if let Some(blob_ref) = &checkpoint.tool_state_ref {
        refs.push(RetainedArtifactRef {
            blob_ref: blob_ref.clone(),
            kind: PersistedArtifactKind::ToolState,
        });
    }
    if let Some(blob_ref) = &checkpoint.plugin_snapshot_ref {
        refs.push(RetainedArtifactRef {
            blob_ref: blob_ref.clone(),
            kind: PersistedArtifactKind::PluginSessionSnapshot,
        });
    }
    if let Some(blob_ref) = &checkpoint.execution_state_ref {
        refs.push(RetainedArtifactRef {
            blob_ref: blob_ref.clone(),
            kind: PersistedArtifactKind::ExecutionStateSnapshot,
        });
    }
    refs
}

fn encode_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("persisted state should serialize")
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

fn compress_blob(content: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, content).expect("compress blob");
    encoder.finish().expect("submit blob compression")
}

fn decompress_blob(content: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(content);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out).ok()?;
    Some(out)
}

fn encode_artifact_blob(
    descriptor: &BlobArtifactDescriptor,
    profile: BuiltinBlobProfile,
    content: &[u8],
) -> Vec<u8> {
    let (compression, stored_content) = if should_compress_blob(profile, descriptor, content.len())
    {
        (BlobCompression::Zlib, compress_blob(content))
    } else {
        (BlobCompression::None, content.to_vec())
    };
    encode_msgpack(&StoredBlobEnvelope {
        descriptor: descriptor.clone(),
        compression,
        content: stored_content,
    })
}

fn decode_artifact_blob(bytes: &[u8]) -> Option<Vec<u8>> {
    let envelope = decode_msgpack::<StoredBlobEnvelope>(bytes)?;
    match envelope.compression {
        BlobCompression::None => Some(envelope.content),
        BlobCompression::Zlib => decompress_blob(&envelope.content),
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
    let mut meta: SessionHeadMeta = lash_core::store::decode_versioned_json_record(
        &head_json,
        "SessionHeadMeta",
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    )?;
    meta.head_revision = head_revision as u64;
    meta.leaf_node_id = leaf_node_id;
    meta.checkpoint_ref = checkpoint_ref.map(Into::into);
    Ok(Some(meta))
}

fn load_session_head_meta_from_conn(
    conn: &Connection,
    session_id: &str,
) -> Option<SessionHeadMeta> {
    try_load_session_head_meta_from_conn(conn, session_id)
        .ok()
        .flatten()
}

fn load_session_meta_from_conn(conn: &Connection, session_id: &str) -> Option<SessionMeta> {
    conn.query_row(
        "SELECT session_id, session_name, created_at, model, cwd, relation_json
         FROM session_meta WHERE session_id = ?1",
        params![session_id],
        |row| {
            let relation_json: Option<String> = row.get(5)?;
            let relation = relation_json
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            Ok(SessionMeta {
                session_id: row.get(0)?,
                session_name: row.get(1)?,
                created_at: row.get(2)?,
                model: row.get(3)?,
                cwd: row.get(4)?,
                relation,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn decode_checkpoint(bytes: &[u8]) -> Result<SessionCheckpoint, StoreError> {
    let value: serde_json::Value = rmp_serde::from_slice(bytes)
        .map_err(|err| StoreError::Backend(format!("failed to decode SessionCheckpoint: {err}")))?;
    lash_core::store::ensure_supported_record_schema_version(
        "SessionCheckpoint",
        &value,
        lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
    )?;
    rmp_serde::from_slice(bytes)
        .map_err(|err| StoreError::Backend(format!("failed to decode SessionCheckpoint: {err}")))
}

fn encode_msgpack<T: serde::Serialize>(value: &T) -> Vec<u8> {
    // Pre-size the buffer so the per-byte writes inside rmp_serde don't
    // walk the Vec through 0→4→8→16→32… reallocations on every call.
    let mut buf = Vec::with_capacity(1024);
    rmp_serde::encode::write_named(&mut buf, value).expect("value should serialize");
    buf
}

fn decode_msgpack<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    rmp_serde::from_slice(bytes).ok()
}

fn merge_token_ledger_entries(
    entries: Vec<lash_core::TokenLedgerEntry>,
) -> Vec<lash_core::TokenLedgerEntry> {
    let mut merged: Vec<lash_core::TokenLedgerEntry> = Vec::new();
    for entry in entries {
        if entry.usage.total() == 0 {
            continue;
        }
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.source == entry.source && existing.model == entry.model)
        {
            existing.usage.input_tokens += entry.usage.input_tokens;
            existing.usage.output_tokens += entry.usage.output_tokens;
            existing.usage.cache_read_input_tokens += entry.usage.cache_read_input_tokens;
            existing.usage.cache_write_input_tokens += entry.usage.cache_write_input_tokens;
            existing.usage.reasoning_output_tokens += entry.usage.reasoning_output_tokens;
        } else {
            merged.push(entry);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::ProcessInput;
    use lashlang::LashlangArtifactStore;

    #[tokio::test]
    async fn checkpoint_probe_skips_writes_for_deferred_head() {
        let store = Arc::new(Store::memory().await.expect("open counter store"));
        lash_core::testing::conformance::checkpoint_claim_probe_transaction_counts(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            "sqlite-checkpoint-counter",
            || store.checkpoint_claim_counts(),
        )
        .await;
    }

    fn registration(id: &str) -> ProcessRegistration {
        ProcessRegistration::new(
            id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryDisposition::ExternallyOwned,
            lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
        )
    }

    #[test]
    fn sqlite_busy_and_locked_errors_are_typed_as_contention() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
        ] {
            let error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("synthetic contention".to_string()),
            );
            assert!(matches!(sqlite_error(error), StoreError::Contended));
        }
    }

    #[tokio::test]
    async fn real_locked_catalog_surfaces_typed_contention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("contended.db");
        let store = Store::open(&path).await.expect("open store");
        store.bind_session("contended").expect("bind store");
        store
            .conn
            .call(|conn| {
                conn.busy_timeout(std::time::Duration::ZERO)?;
                Ok(())
            })
            .await
            .expect("disable busy wait");

        let locker = rusqlite::Connection::open(&path).expect("open lock holder");
        locker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold catalog writer lock");
        let result = store
            .commit_runtime_state(RuntimeCommit::persisted_state(
                &lash_core::RuntimeSessionState {
                    session_id: "contended".to_string(),
                    ..Default::default()
                },
                &[],
            ))
            .await;
        locker
            .execute_batch("ROLLBACK")
            .expect("release writer lock");

        assert!(matches!(result, Err(StoreError::Contended)));
    }

    #[tokio::test]
    async fn zero_confirmation_aborts_a_corrupt_low_count_transaction() {
        let store = Store::memory().await.expect("open store");
        store.bind_session("refcount-drift").expect("bind store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: "refcount-drift".to_string(),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("commit root frame");
        let frame_node_id = state.current_frame_node_id.clone().expect("frame node");
        store
            .conn
            .write({
                let frame_node_id = frame_node_id.clone();
                move |tx| {
                    tx.execute(
                        "UPDATE graph_nodes SET incoming_refs = 0 WHERE node_id = ?1",
                        params![frame_node_id],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("corrupt cached count");
        let child_node_id = "refcount-drift-child".to_string();
        let child = lash_core::SessionNodeRecord {
            node_id: child_node_id.clone(),
            parent_node_id: Some(frame_node_id.clone()),
            timestamp: "2026-07-27T00:00:00Z".to_string(),
            payload: lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(
                    lash_core::ProtocolEvent::typed("refcount-drift", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.expected_head_revision = 1;
        commit.graph = GraphCommitDelta::Append {
            nodes: vec![child],
            leaf_node_id: Some(child_node_id.clone()),
        };

        let error = store
            .commit_runtime_state(commit)
            .await
            .expect_err("zero-confirmation must abort");

        assert!(
            matches!(
                &error,
                StoreError::NodeRefcountDrift {
                    node_id,
                    cached: 0,
                    derived: 1,
                } if node_id == &frame_node_id
            ),
            "unexpected zero-confirmation error: {error:?}"
        );
        let persisted = store
            .load_session(SessionReadScope::FullGraph)
            .await
            .expect("load after abort")
            .expect("session remains live");
        assert_eq!(persisted.head_revision, 1);
        assert_eq!(
            persisted.graph.leaf_node_id.as_deref(),
            Some(frame_node_id.as_str())
        );
        assert!(
            store
                .load_node(&child_node_id)
                .await
                .expect("load aborted child")
                .is_none(),
            "the transaction must roll back the child insert"
        );
    }

    #[tokio::test]
    async fn refcount_scrub_detects_corrupt_cached_count() {
        let store = Store::memory().await.expect("open store");
        store
            .bind_session("scrub-refcount-drift")
            .expect("bind store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: "scrub-refcount-drift".to_string(),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("commit root frame");
        let frame_node_id = state.current_frame_node_id.expect("frame node");
        store
            .conn
            .write({
                let frame_node_id = frame_node_id.clone();
                move |tx| {
                    tx.execute(
                        "UPDATE graph_nodes SET incoming_refs = 2 WHERE node_id = ?1",
                        params![frame_node_id],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("corrupt cached count");

        let error = store
            .verify_node_refcounts()
            .await
            .expect_err("scrub must detect cached count drift");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                node_id,
                cached: 2,
                derived: 1,
            } if node_id == frame_node_id
        ));
    }

    #[tokio::test]
    async fn maintenance_preserves_typed_refcount_drift() {
        let store = Store::memory().await.expect("open store");
        store.bind_session("maintenance-drift").expect("bind store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: "maintenance-drift".to_string(),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("commit root frame");
        let frame_node_id = state.current_frame_node_id.clone().expect("frame node");
        store
            .conn
            .write({
                let frame_node_id = frame_node_id.clone();
                move |tx| {
                    tx.execute(
                        "UPDATE graph_nodes SET incoming_refs = 0 WHERE node_id = ?1",
                        params![frame_node_id],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("corrupt cached count");

        let error = store
            .tombstone_nodes(std::slice::from_ref(&frame_node_id))
            .await
            .expect_err("maintenance drift must stay typed");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                node_id,
                cached: 0,
                derived: 1,
            } if node_id == frame_node_id
        ));
    }

    #[tokio::test]
    async fn live_attachment_refs_reads_the_factory_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir sessions");
        let factory = SqliteSessionStoreFactory::new(&root);

        let catalog = factory.catalog_path();
        let attachment_id = lash_core::AttachmentId::new("a".repeat(64));
        {
            let store = Store::open(&catalog).await.expect("open catalog");
            lash_core::AttachmentManifest::record_intent(
                &store,
                lash_core::AttachmentIntent {
                    attachment_id: attachment_id.clone(),
                    session_id: "sess-1".to_string(),
                    canonical_uri: format!("lash-attachment://sha256/{attachment_id}"),
                    intent_at_epoch_ms: 1_000,
                    owner_kind: None,
                    owner_id: None,
                },
            )
            .expect("record intent");
            lash_core::AttachmentManifest::commit_refs(
                &store,
                "sess-1",
                std::slice::from_ref(&attachment_id),
            )
            .expect("commit ref");
        }

        let refs = SessionStoreFactory::live_attachment_refs(&factory, 0)
            .await
            .expect("root discovery");
        assert!(
            refs.contains(&attachment_id),
            "the catalog's committed ref must be discovered"
        );
        assert_eq!(refs.len(), 1, "only the catalog contributes refs: {refs:?}");
    }

    #[tokio::test]
    async fn live_attachment_refs_aborts_on_unreadable_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir sessions");
        let factory = SqliteSessionStoreFactory::new(&root);

        std::fs::write(factory.catalog_path(), b"corrupt not-a-db").expect("write corrupt");

        let result = SessionStoreFactory::live_attachment_refs(&factory, 0).await;
        assert!(
            result.is_err(),
            "an unreadable durable-core catalog must abort discovery, got {result:?}"
        );
    }

    #[tokio::test]
    async fn segment_handover_persist_keeps_current_input_for_crash_replay() {
        let registry = SqliteProcessRegistry::memory()
            .await
            .expect("memory registry");
        registry
            .register_process(registration("segment-crash"))
            .await
            .expect("register");
        let handover = |segment_ordinal| PersistedSegmentHandover {
            segment_ordinal,
            program_hash: "program-v1".to_string(),
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("program-v1".to_string()),
                engine_state: vec![segment_ordinal as u8],
            },
        };
        registry
            .put_segment_handover("segment-crash", handover(1))
            .await
            .expect("persist current segment input");
        registry
            .put_segment_handover("segment-crash", handover(2))
            .await
            .expect("persist successor before send");

        assert_eq!(
            registry
                .get_segment_handover("segment-crash", 1)
                .await
                .expect("replay read"),
            Some(handover(1)),
            "a crash before successor send must leave segment 1 replayable"
        );
        assert_eq!(
            registry
                .latest_segment_handover("segment-crash")
                .await
                .expect("latest handover"),
            Some(handover(2))
        );
    }

    #[tokio::test]
    async fn terminal_segment_handover_cleanup_removes_continuation_state() {
        let registry = SqliteProcessRegistry::memory()
            .await
            .expect("memory registry");
        registry
            .register_process(registration("segment-terminal"))
            .await
            .expect("register");
        registry
            .put_segment_handover(
                "segment-terminal",
                PersistedSegmentHandover {
                    segment_ordinal: 1,
                    program_hash: "program-v1".to_string(),
                    handover: lash_core::SegmentHandover {
                        reason: lash_core::BoundaryReason::JournalBudget,
                        program_hash: Some("program-v1".to_string()),
                        engine_state: vec![7],
                    },
                },
            )
            .await
            .expect("persist handover");
        registry
            .delete_segment_handovers("segment-terminal")
            .await
            .expect("terminal cleanup");
        assert!(
            registry
                .latest_segment_handover("segment-terminal")
                .await
                .expect("latest handover")
                .is_none()
        );
    }

    #[tokio::test]
    async fn sqlite_lashlang_artifact_store_round_trips_verified_module_artifacts() {
        let store = Store::memory().await.expect("memory store");
        let module =
            lashlang::parse("process scan(root: str) { finish root }").expect("parse module");
        let linked = lashlang::LinkedModule::link(
            module,
            lashlang::LashlangHostEnvironment::new(
                lashlang::LashlangHostCatalog::new(),
                lashlang::LashlangAbilities::all(),
            ),
        )
        .expect("link module");

        store
            .put_module_artifact(&linked.artifact)
            .await
            .expect("put artifact");
        let restored = store
            .get_module_artifact(&linked.module_ref)
            .await
            .expect("get artifact")
            .expect("artifact exists");

        assert_eq!(restored.module_ref, linked.module_ref);
        assert_eq!(
            restored.process_ref("scan"),
            linked.artifact.process_ref("scan")
        );
    }

    #[tokio::test]
    async fn sqlite_process_registry_persists_rows_after_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("processes.db");
        {
            let registry = SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
                .await
                .expect("open registry");
            let session_scope = lash_core::SessionScope::new("session");
            registry
                .register_process(registration("proc-persist"))
                .await
                .expect("register");
            registry
                .grant_handle(
                    &session_scope,
                    "proc-persist",
                    ProcessHandleDescriptor::new(Some("tool"), Some("demo")),
                )
                .await
                .expect("grant");
            registry
                .complete_process(
                    "proc-persist",
                    ProcessAwaitOutput::Success {
                        value: serde_json::json!({"ok": true}),
                        control: None,
                    },
                    lash_core::ProcessCompletionAuthority::external_owner(),
                )
                .await
                .expect("complete");
        }

        let registry = Arc::new(
            SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
                .await
                .expect("reopen registry"),
        ) as Arc<dyn lash_core::ProcessRegistry>;
        let session_scope = lash_core::SessionScope::new("session");
        let record = registry
            .get_process("proc-persist")
            .await
            .expect("persisted process");

        assert_eq!(record.originator_scope_id(), session_scope.id().as_str());
        assert_eq!(
            record.provenance.originator,
            lash_core::ProcessOriginator::session(session_scope.clone())
        );
        assert_eq!(
            lash_core::ProcessAwaiter::polling(Arc::clone(&registry))
                .await_terminal("proc-persist")
                .await
                .expect("await persisted"),
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"ok": true}),
                control: None,
            }
        );
        assert_eq!(
            registry
                .list_handle_grants(&session_scope)
                .await
                .expect("grants")
                .len(),
            1
        );
    }
}
