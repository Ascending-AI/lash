//! # lash-sqlite-store
//!
//! The high-performance local **durable** persistence backend for the lash
//! agent runtime. One SQLite database per session, opened in WAL journal mode
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
//! [`RuntimePersistence`]: lash_core::RuntimePersistence
//! [`AttachmentManifest`]: lash_core::AttachmentManifest

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

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
    RuntimeCommitResult, SessionCheckpoint, SessionHead, SessionHeadMeta,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, BlobRef, DeliveryPolicy, DurabilityTier, GcReport, LeaseOwnerIdentity,
    LeaseOwnerLiveness, MergeKey, PROCESS_LEASE_SCHEMA_VERSION, PersistedSegmentHandover,
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
/// It is intentionally separate from [`Store`]: session databases persist one
/// conversation, while this registry persists background process state and
/// handle visibility across all sessions sharing the registry.
pub struct SqliteProcessRegistry {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core::Clock>,
    process_session_store_root: Option<PathBuf>,
}

fn sqlite_error(err: rusqlite::Error) -> StoreError {
    StoreError::Backend(err.to_string())
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

/// Explicit first-party factory for one SQLite session database per Lash
/// session.
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

    pub fn path_for_session(&self, session_id: &str) -> PathBuf {
        self.root.join(safe_session_db_file_name(session_id))
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SqliteSessionStoreFactory {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, String> {
        std::fs::create_dir_all(&self.root).map_err(|err| err.to_string())?;
        let path = self.path_for_session(&request.session_id);
        let store = Arc::new(
            Store::open_with_options_clock_and_process_registry(
                &path,
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
        let path = self.path_for_session(&request.session_id);
        if !path.exists() {
            return Ok(None);
        }
        self.create_store(request).await.map(Some)
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        delete_session_files(&self.root, session_id)
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        // Per-session-database topology: the factory owns the directory, so it
        // computes the root set by unioning each session database's refs at
        // sweep time (the ratified choice over a dual-written factory index).
        let mut refs = std::collections::BTreeSet::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // No sessions written yet: an empty root set.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
            Err(err) => {
                return Err(lash_core::StoreError::Backend(format!(
                    "read session store directory {}: {err}",
                    self.root.display()
                )));
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|err| lash_core::StoreError::Backend(err.to_string()))?
                .path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // `-wal`/`-shm` sidecars carry the `.db-wal` / `.db-shm` extension,
            // so they are not `.db` files at all — skip them.
            if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                continue;
            }
            // Per-session sidecar databases (`<primary>.effects.db`,
            // `.processes.db`, `.triggers.db`, `.artifacts.db`, `.process-env.db`)
            // ARE `.db` files, but are named on top of a primary session database
            // filename that itself ends in `.db`, so they carry an interior `.db.`
            // that a primary never does. Skip them — a sidecar (even a corrupt
            // one) is not a session database and must not abort GC, distinctly
            // from an *unreadable primary session* database, which must (below).
            if !is_primary_session_db_name(file_name) {
                tracing::warn!(
                    path = %path.display(),
                    "attachment GC: skipping sidecar database in sessions directory"
                );
                continue;
            }
            // A primary session database that fails to open might still hold live
            // refs. Aborting the whole sweep is the safe choice: treating it as
            // empty would let GC delete blobs it actually references.
            let store = Store::open_with_options_clock_and_process_registry(
                &path,
                self.options,
                Arc::clone(&self.clock),
                self.process_registry_path.as_deref(),
            )
            .await
            .map_err(|err| {
                lash_core::StoreError::Backend(format!(
                    "attachment GC aborted: session database {} could not be opened: {err}",
                    path.display()
                ))
            })?;
            // Reconcile terminal-owner intents in this session database
            // atomically: one conditional DELETE combines age with owner death
            // (no list-then-forget race against a concurrent intent refresh),
            // then union what remains.
            lash_core::AttachmentManifest::forget_aged_uncommitted_intents(
                &store,
                intent_grace_cutoff_epoch_ms,
            )?;
            refs.extend(lash_core::AttachmentManifest::list_all_refs(&store)?);
        }
        Ok(refs)
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core::StoreError> {
        // Targeted single-id probe: iterate the per-session databases only until
        // the first hit rather than unioning every session's refs. Read-only — it
        // does not reconcile aged intents (the reconciling snapshot already ran).
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(lash_core::StoreError::Backend(format!(
                    "read session store directory {}: {err}",
                    self.root.display()
                )));
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|err| lash_core::StoreError::Backend(err.to_string()))?
                .path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                continue;
            }
            if !is_primary_session_db_name(file_name) {
                continue;
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
                        "attachment GC root re-check aborted: session database {} could not be opened: {err}",
                        path.display()
                    ))
                })?;
            if lash_core::AttachmentManifest::has_live_ref_for_id(
                &store,
                id,
                intent_grace_cutoff_epoch_ms,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn warn_process_registry_not_wired() {
    tracing::warn!(
        "SQLite attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call SqliteSessionStoreFactory::new_with_process_registry(...)."
    );
}

fn delete_session_files(root: &Path, session_id: &str) -> Result<(), String> {
    let db_path = root.join(safe_session_db_file_name(session_id));
    for path in [
        db_path.clone(),
        sqlite_sidecar_path(&db_path, "-wal"),
        sqlite_sidecar_path(&db_path, "-shm"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!("remove session store {}: {err}", path.display()));
            }
        }
    }
    Ok(())
}

/// Whether `file_name` is a primary session database rather than a per-session
/// sidecar database.
///
/// A primary session database ends in `.db` with `.db` appearing *only* as the
/// final extension. Sidecar databases are named by appending a suffix on top of
/// the primary filename (which already ends in `.db`), e.g.
/// `20260710_011120.db.effects.db` or `sess-<hash>.db.processes.db`, so they
/// carry an interior `.db.` that a primary never does. This structural test is
/// deliberately independent of how the primary base name is formed — both this
/// factory's `<safe>-<hash>.db` names and the reference host's `<timestamp>.db`
/// names are recognised as primaries — because the host, not the factory, owns
/// the primary naming scheme in the shared sessions directory. (`-wal`/`-shm`
/// sidecars are excluded earlier: they are not `.db` files at all.)
fn is_primary_session_db_name(file_name: &str) -> bool {
    let Some(stem) = file_name.strip_suffix(".db") else {
        return false;
    };
    // A primary's stem is the base name with no further `.db` extension; a
    // sidecar's stem still contains the primary's trailing `.db` (`...db.effects`).
    !stem.contains(".db")
}

fn safe_session_db_file_name(session_id: &str) -> String {
    let mut safe = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    safe = safe.trim_matches('_').to_string();
    if safe.is_empty() {
        safe.push_str("session");
    }
    safe.truncate(80);
    let hash = format!("{:x}", Sha256::digest(session_id.as_bytes()));
    format!("{safe}-{}.db", &hash[..16])
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
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

fn session_head_meta(head: &SessionHead) -> SessionHeadMeta {
    SessionHeadMeta {
        schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: head.session_id.clone(),
        head_revision: 0,
        config: head.config.clone(),
        agent_frames: head.agent_frames.clone(),
        current_agent_frame_id: head.current_agent_frame_id.clone(),
        checkpoint_ref: head.checkpoint_ref.clone(),
        leaf_node_id: head.graph.leaf_node_id.clone(),
        graph_node_count: head.graph.nodes.len(),
        token_ledger: Vec::new(),
    }
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
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let row = conn
        .query_row(
            "SELECT head_json, head_revision FROM session_head WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((head_json, head_revision)) = row else {
        return Ok(None);
    };
    let mut meta: SessionHeadMeta = lash_core::store::decode_versioned_json_record(
        &head_json,
        "SessionHeadMeta",
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    )?;
    meta.head_revision = head_revision as u64;
    Ok(Some(meta))
}

fn load_session_head_meta_from_conn(conn: &Connection) -> Option<SessionHeadMeta> {
    try_load_session_head_meta_from_conn(conn).ok().flatten()
}

fn load_session_meta_from_conn(conn: &Connection) -> Option<SessionMeta> {
    conn.query_row(
        "SELECT session_id, session_name, created_at, model, cwd, relation_json
         FROM session_meta WHERE singleton = 1",
        [],
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
    fn primary_session_db_names_exclude_sidecars() {
        // Both primary naming schemes are recognised: this factory's
        // `<safe>-<hash>.db` and the reference host's `<timestamp>.db`.
        for primary in [
            safe_session_db_file_name("sess-1"),
            "20260710_011120.db".to_string(),
        ] {
            assert!(
                is_primary_session_db_name(&primary),
                "{primary} must be recognised as a primary session db"
            );
            // Sidecar databases suffixed on top of the primary `<name>.db` are not.
            for suffix in [
                "effects.db",
                "processes.db",
                "triggers.db",
                "artifacts.db",
                "process-env.db",
            ] {
                assert!(
                    !is_primary_session_db_name(&format!("{primary}.{suffix}")),
                    "sidecar {suffix} of {primary} must not be treated as a primary session db"
                );
            }
            // WAL/SHM sidecars are not `.db` files at all.
            assert!(!is_primary_session_db_name(&format!("{primary}-wal")));
        }
    }

    // Fix D: the factory's root-set discovery iterates a directory that also holds
    // per-session sidecar databases (`.effects.db`, `.processes.db`, ...). It must
    // skip those without aborting, still surfacing the primary session database's
    // committed refs.
    #[tokio::test]
    async fn live_attachment_refs_skips_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir sessions");
        let factory = SqliteSessionStoreFactory::new(&root);

        // A primary session database with a committed attachment ref.
        let primary = factory.path_for_session("sess-1");
        let attachment_id = lash_core::AttachmentId::new("a".repeat(64));
        {
            let store = Store::open(&primary).await.expect("open primary");
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

        // Sidecar databases the CLI drops next to the primary, plus a stray file.
        let primary_name = primary
            .file_name()
            .and_then(|name| name.to_str())
            .expect("primary name")
            .to_string();
        for suffix in [
            "effects.db",
            "processes.db",
            "triggers.db",
            "artifacts.db",
            "process-env.db",
        ] {
            std::fs::write(
                root.join(format!("{primary_name}.{suffix}")),
                b"not a sqlite database",
            )
            .expect("write sidecar");
        }

        // A cutoff of 0 ages out no intents; the committed ref survives. The
        // corrupt sidecars are skipped, not opened, so discovery does not abort.
        let refs = SessionStoreFactory::live_attachment_refs(&factory, 0)
            .await
            .expect("root discovery must not abort on sidecars");
        assert!(
            refs.contains(&attachment_id),
            "the primary session's committed ref must be discovered"
        );
        assert_eq!(
            refs.len(),
            1,
            "only the primary db contributes refs: {refs:?}"
        );
    }

    // Fix D safety: an *unreadable primary* session database (as opposed to a
    // sidecar) must abort the sweep — never be silently treated as empty, which
    // would drop its refs and let GC delete blobs it references.
    #[tokio::test]
    async fn live_attachment_refs_aborts_on_unreadable_primary_session_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir sessions");
        let factory = SqliteSessionStoreFactory::new(&root);

        // A file that reads as a primary session database name (no interior
        // `.db.`) but is not a valid SQLite database.
        std::fs::write(root.join("20260710_010101.db"), b"corrupt not-a-db")
            .expect("write corrupt");

        let result = SessionStoreFactory::live_attachment_refs(&factory, 0).await;
        assert!(
            result.is_err(),
            "an unreadable primary session database must abort discovery, got {result:?}"
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
