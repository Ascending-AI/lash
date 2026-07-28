mod file_store;

pub use file_store::FileAttachmentStore;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentMeta, AttachmentRef};
use sha2::{Digest, Sha256};

use crate::store::{AttachmentIntent, AttachmentManifest, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentProducer {
    Host,
    TurnIngress,
    Tool { tool_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("attachment source policy denied {producer:?}: {reason}")]
pub struct AttachmentSourcePolicyError {
    pub producer: AttachmentProducer,
    pub reason: String,
}

pub trait AttachmentSourcePolicy: Send + Sync {
    fn authorize(
        &self,
        producer: &AttachmentProducer,
        source: &crate::AttachmentSource,
    ) -> Result<(), AttachmentSourcePolicyError>;
}

#[derive(Debug, Default)]
pub struct OpenAttachmentSourcePolicy;

impl AttachmentSourcePolicy for OpenAttachmentSourcePolicy {
    fn authorize(
        &self,
        _producer: &AttachmentProducer,
        _source: &crate::AttachmentSource,
    ) -> Result<(), AttachmentSourcePolicyError> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AttachmentStoreError {
    #[error("attachment `{0}` was not found")]
    NotFound(AttachmentId),
    #[error("attachment store I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("attachment manifest write failed: {0}")]
    ManifestRecordFailed(String),
    #[error("attachment store backend failed: {0}")]
    Backend(String),
}

#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub bytes: Vec<u8>,
}

/// One blob enumerated by [`AttachmentStore::list`]. Feeds mark-and-sweep GC:
/// the sweeper pairs each blob's `id` against the live root set and uses
/// `last_modified_epoch_ms` to apply the write grace period. Backends that
/// cannot report a modification time leave it `None`, and the sweep treats
/// such blobs as always past the grace window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBlobRef {
    pub id: AttachmentId,
    pub last_modified_epoch_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentStorePersistence {
    Ephemeral,
    Durable,
}

/// A flat, content-addressed blob store: host-supplied dumb infrastructure.
///
/// The store maps a content hash to its bytes and nothing more. It has no
/// notion of sessions — identical bytes written by any number of sessions
/// resolve to one physical blob, and that dedup is intended. Reference
/// tracking and the session boundary live one layer up in
/// [`SessionAttachmentStore`] and the [`AttachmentManifest`]; lifecycle
/// (which blobs may be deleted) lives above that in the host, via
/// [`reclaim_unreferenced_attachments`].
///
/// Conventions every backend upholds: `put` is idempotent (identical bytes are
/// a no-op returning the same ref), `delete` is idempotent, and a missing blob
/// maps to [`AttachmentStoreError::NotFound`].
#[async_trait::async_trait]
pub trait AttachmentStore: Send + Sync {
    fn persistence(&self) -> AttachmentStorePersistence {
        AttachmentStorePersistence::Ephemeral
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError>;

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError>;

    /// Remove one blob. Idempotent: deleting an absent blob is a no-op. This is
    /// the primitive mark-and-sweep GC uses to reclaim unreferenced content;
    /// per-session lifecycle is expressed by dropping manifest refs, never by
    /// calling this directly for a live session.
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError>;

    /// Enumerate every blob currently held. Used only by mark-and-sweep GC.
    /// Large deployments may hold many blobs; backends should stream/batch
    /// internally where possible. Order is unspecified.
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError>;

    /// Re-fetch one blob's current freshness signal, or `None` if it is absent.
    ///
    /// The mark-and-sweep GC calls this immediately before deleting a candidate:
    /// the `last_modified_epoch_ms` captured by the `list` snapshot is stale by
    /// delete time, so a blob that a fresh `put` (a new intent for the same
    /// content id) touched *after* the snapshot must be spared. The default
    /// implementation scans `list`; backends override it with a cheap
    /// stat/`HEAD`.
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Ok(self.list().await?.into_iter().find(|blob| &blob.id == id))
    }
}

/// A source of the live attachment root set across every session a store
/// factory owns. Committed refs and intents with owners that can still commit
/// are roots; terminal-owner intents remain roots through their retention
/// window. Unscoped host puts use the legacy age-only fallback.
///
/// Implemented by session-store factories, which own the full set of sessions:
/// a global manifest table answers in one query (Postgres); a per-session
/// database topology answers by iterating the factory's session databases at
/// sweep time (SQLite); an in-memory factory answers from its live stores.
#[async_trait::async_trait]
pub trait AttachmentRootSet: Send + Sync {
    /// The live root set, reconciled against `intent_grace_cutoff_epoch_ms`.
    ///
    /// A committed ref is always a root. An uncommitted intent remains a root
    /// until both the cutoff has elapsed and its durable owner is proven unable
    /// to commit. A turn owner is dead only after a superseding turn commit for
    /// the session; a process owner is dead only after its durable process row
    /// is pruned. An ownerless host intent retains the legacy age-only rule.
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, StoreError>;

    /// Whether a single id currently has a live root under the same age plus
    /// owner-reachability rule as [`Self::live_attachment_refs`].
    ///
    /// Targeted counterpart to [`Self::live_attachment_refs`] for the GC lever's
    /// delete-time root re-check (see [`reclaim_unreferenced_attachments`]): the
    /// full root set is snapshotted once, but a candidate blob can be re-referenced
    /// in the narrow window between the freshness re-check and the delete, so the
    /// sweep re-probes just that id. Unlike the snapshot, this is a read-only probe
    /// — it must NOT reconcile (forget) aged intents. Backends answer with a single
    /// indexed query / first-hit scan rather than materializing the whole set. The
    /// default re-materializes the root set and tests membership.
    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await?
            .contains(id))
    }
}

/// Every session-store factory is a root set: it owns all sessions, so it can
/// enumerate their refs. The concrete factories override
/// [`SessionStoreFactory::live_attachment_refs`](crate::SessionStoreFactory::live_attachment_refs);
/// this blanket makes any of them (including `dyn SessionStoreFactory`) usable
/// as the GC lever's `root_set`.
#[async_trait::async_trait]
impl<T: crate::SessionStoreFactory + ?Sized> AttachmentRootSet for T {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, StoreError> {
        crate::SessionStoreFactory::live_attachment_refs(self, intent_grace_cutoff_epoch_ms).await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        crate::SessionStoreFactory::has_live_attachment_ref(self, id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

/// Outcome of a host-invoked unreferenced-attachment reclamation sweep.
///
/// See [`reclaim_unreferenced_attachments`] for the full contract. Returned so
/// hosts can emit metrics the same way [`GcReport`](crate::GcReport) and
/// [`VacuumReport`](crate::VacuumReport) do for the store-side levers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachmentReclamationReport {
    /// Blobs enumerated from the backend and considered by the sweep.
    pub scanned_blob_count: usize,
    /// Blobs deleted: unreferenced by any session and past the grace window.
    pub reclaimed_count: usize,
    /// Blobs the sweep tried but failed to delete. The sweep continues past
    /// per-blob failures and reports them here rather than aborting.
    pub failed_ids: Vec<AttachmentId>,
    /// Blobs that were deleted but a live root re-appeared for in the residual
    /// window between the pre-delete root re-check and the delete itself. The
    /// bytes are already gone and cannot be restored, but a put-always-writes
    /// backend self-heals on the referencing session's next `put` (the intent's
    /// write-ahead ordering guarantees a retry rewrites the bytes). Recorded and
    /// logged at error level so an operator sees the (single-digit-millisecond,
    /// self-healing) event rather than a silent data loss.
    pub deleted_while_referenced: Vec<AttachmentId>,
}

/// Mark-and-sweep GC for attachment blobs — the host-invocable counterpart to
/// [`StoreMaintenance::gc_unreachable`](crate::StoreMaintenance::gc_unreachable)
/// for attachment payloads.
///
/// Enumerates every blob in `backend`, computes the live root set from
/// `root_set` (committed refs plus intents whose durable owners can still
/// commit), and deletes every blob no session references. A `grace_period_ms`
/// retention window delays reclamation after owner death and protects freshly
/// written blobs even if they currently look unreferenced. Per-blob delete
/// failures are collected into
/// [`AttachmentReclamationReport::failed_ids`]; the sweep does not abort on the
/// first failure.
///
/// # Two reconciliation windows, one grace period
///
/// `grace_period_ms` gates two independent hazards, both keyed off the same
/// value:
///
/// * *Terminal-owner retention.* An uncommitted intent older than
///   `now - grace_period_ms` is forgotten only when its turn has been
///   superseded, its process row has been pruned, or it has no durable owner.
///   Age never proves a turn or process dead.
/// * *Delete-time freshness race.* The `list` snapshot's `last_modified` is
///   stale by the time the sweep reaches a candidate. Before deleting, the sweep
///   re-fetches the blob's freshness with [`AttachmentStore::head`] and spares
///   any blob touched within the window — covering the interleaving where a new
///   intent plus a `put` of the same content id lands after the root snapshot
///   was taken (the `put` refreshes the blob's modification time).
///
/// # The delete window and its residual
///
/// After the freshness re-check the sweep does a *targeted root re-check* for the
/// single candidate id ([`AttachmentRootSet::has_live_attachment_ref`]) and skips
/// any blob a session has re-referenced since the root snapshot. This probe is
/// what the write-ahead intent ordering makes reliable: the facade records the
/// manifest intent *before* the backend `put`, so a root exists no later than the
/// bytes. A ref can still appear in the residual window between that probe and the
/// physical delete — bounded to the single-digit milliseconds of one probe plus
/// one delete. When it does, the bytes are already unrecoverable, but every
/// backend `put` physically rewrites absent content, so the referencing session's
/// next `put` self-heals; the sweep records the id in
/// [`AttachmentReclamationReport::deleted_while_referenced`] and logs at error
/// level so the (rare, self-healing) event is never silent.
///
/// # Deployment assumption
///
/// The `backend` instance is assumed exclusive to this lash deployment: every
/// blob it holds was written by this deployment's sessions, so a blob with no
/// live ref is genuinely garbage. Sharing a bucket/directory across
/// deployments would let this sweep delete another deployment's live content.
///
/// # Policy is the host's (ADR-0014)
///
/// This is a lever, not a scheduler: the host chooses `grace_period_ms` as a
/// post-terminal retention policy and chooses when to run it. The window is not
/// a correctness bound on replay duration. The lever does no background work.
pub async fn reclaim_unreferenced_attachments<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    grace_period_ms: u64,
) -> Result<AttachmentReclamationReport, AttachmentStoreError>
where
    R: AttachmentRootSet + ?Sized,
{
    let now = now_epoch_ms();
    let intent_grace_cutoff = now.saturating_sub(grace_period_ms);
    let live = root_set
        .live_attachment_refs(intent_grace_cutoff)
        .await
        .map_err(|err| {
            AttachmentStoreError::Backend(format!(
                "failed to enumerate live attachment refs: {err}"
            ))
        })?;
    let blobs = backend.list().await?;
    let mut report = AttachmentReclamationReport::default();
    for blob in blobs {
        report.scanned_blob_count += 1;
        if live.contains(&blob.id) {
            continue;
        }
        if within_grace(blob.last_modified_epoch_ms, now, grace_period_ms) {
            // Fresh write or in-flight intent per the (possibly stale) snapshot.
            continue;
        }
        // (a) Delete-time freshness re-check: the snapshot's freshness is stale, so
        // re-stat the blob immediately before deleting. A concurrent
        // new-intent-plus-`put` of the same content id — landed after the root
        // snapshot — refreshes the blob's modification time; spare it so a
        // newly-referenced blob is never reclaimed out from under its intent.
        match backend.head(&blob.id).await {
            Ok(Some(fresh)) => {
                if within_grace(
                    fresh.last_modified_epoch_ms,
                    now_epoch_ms(),
                    grace_period_ms,
                ) {
                    continue;
                }
            }
            // Already gone (a concurrent delete): nothing to reclaim.
            Ok(None) => continue,
            // Could not re-stat: treat as a per-blob failure rather than risk
            // deleting a blob we can no longer vouch for.
            Err(_) => {
                report.failed_ids.push(blob.id);
                continue;
            }
        }
        // (b) Targeted root re-check for THIS id. The `live` snapshot was taken
        // before the per-blob loop began; a session may have recorded a fresh
        // intent for this content id since. This is effective because the facade's
        // `put` records the write-ahead intent BEFORE the backend `put` refreshes
        // the bytes (`SessionAttachmentStore::put`): by the time bytes exist to be
        // reclaimed, the intent row that roots them already does, so this probe
        // observes it. A live root here means we must not delete.
        match root_set
            .has_live_attachment_ref(&blob.id, intent_grace_cutoff)
            .await
        {
            Ok(true) => continue,
            Ok(false) => {}
            // Could not probe the root set: do not delete a blob we can no longer
            // prove is unreferenced.
            Err(_) => {
                report.failed_ids.push(blob.id);
                continue;
            }
        }
        // (c) Delete.
        match backend.delete(&blob.id).await {
            Ok(()) => {
                report.reclaimed_count += 1;
                // (d) Post-delete root re-check. A ref can still appear in the
                // residual window between (b) and (c) — bounded to the single-digit
                // milliseconds of one root-set probe plus one backend delete. The
                // bytes are already gone and cannot be restored, but every backend
                // `put` physically rewrites content when it is absent (file store
                // rewrites on a missing path, S3 PUTs unconditionally, the
                // in-memory store re-inserts), so the referencing session's next
                // `put` self-heals. Record and log loudly so an operator sees the
                // (rare, self-healing) event.
                // A late ref (probe answers true) is recorded and alarmed. No late
                // ref, or a failed probe, needs nothing more — a failed probe here
                // cannot un-delete the blob.
                if let Ok(true) = root_set
                    .has_live_attachment_ref(&blob.id, intent_grace_cutoff)
                    .await
                {
                    tracing::error!(
                        attachment_id = %blob.id,
                        "attachment GC deleted a blob that was re-referenced in the \
                         delete window; bytes are unrecoverable but a subsequent put \
                         self-heals"
                    );
                    report.deleted_while_referenced.push(blob.id);
                }
            }
            Err(_) => report.failed_ids.push(blob.id),
        }
    }
    Ok(report)
}

/// Whether a blob modified at `last_modified_epoch_ms` is within the write grace
/// window relative to `now`. A backend that cannot report a modification time
/// (`None`) is treated as past the window, matching [`StoredBlobRef`].
fn within_grace(last_modified_epoch_ms: Option<u64>, now: u64, grace_period_ms: u64) -> bool {
    last_modified_epoch_ms.is_some_and(|modified| now.saturating_sub(modified) < grace_period_ms)
}

struct InMemoryBlob {
    stored: StoredAttachment,
    stored_at_epoch_ms: u64,
}

#[derive(Default)]
pub struct InMemoryAttachmentStore {
    attachments: Mutex<HashMap<AttachmentId, InMemoryBlob>>,
}

impl InMemoryAttachmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl AttachmentStore for InMemoryAttachmentStore {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let meta = stored_meta(&bytes, meta);
        let reference = meta.as_ref();
        let now = now_epoch_ms();
        let mut attachments = self.attachments.lock().expect("attachment store lock");
        match attachments.entry(reference.id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut existing) => {
                // Dedup hit: refresh the freshness signal so a GC sweep that
                // snapshotted the roots before this put cannot reclaim the
                // now-freshly-referenced blob.
                existing.get_mut().stored_at_epoch_ms = now;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(InMemoryBlob {
                    stored: StoredAttachment { bytes },
                    stored_at_epoch_ms: now,
                });
            }
        }
        Ok(reference)
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.attachments
            .lock()
            .expect("attachment store lock")
            .get(id)
            .map(|blob| blob.stored.clone())
            .ok_or_else(|| AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.attachments
            .lock()
            .expect("attachment store lock")
            .remove(id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(self
            .attachments
            .lock()
            .expect("attachment store lock")
            .iter()
            .map(|(id, blob)| StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms: Some(blob.stored_at_epoch_ms),
            })
            .collect())
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Ok(self
            .attachments
            .lock()
            .expect("attachment store lock")
            .get(id)
            .map(|blob| StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms: Some(blob.stored_at_epoch_ms),
            }))
    }
}

pub fn content_id(bytes: &[u8]) -> AttachmentId {
    AttachmentId::new(format!("{:x}", Sha256::digest(bytes)))
}

/// The concrete, session-bound facade over a flat [`AttachmentStore`] backend —
/// the only attachment surface the runtime and its consumers ever see.
///
/// It binds a flat blob `backend`, an [`AttachmentManifest`] that tracks
/// `(session_id, attachment_id)` refs, and a `session_id`. Every `put` records
/// a write-ahead intent in the manifest *before* the bytes hit the backend, so
/// a crash between `put` and the next durable commit surfaces as an uncommitted
/// manifest row that GC reconciles. Every `get` first checks the manifest holds
/// a ref for this session — the session-boundary guard that replaces physical
/// per-session isolation: a turn in one session can never resolve another
/// session's content-addressed blob by guessing its hash. `delete` drops the
/// session's manifest ref and leaves the blob in place; the bytes die later via
/// [`reclaim_unreferenced_attachments`] once no session references them.
///
/// Ephemeral runtimes (no durable reference store) wrap their backend with a
/// [`NoopAttachmentManifest`] via [`SessionAttachmentStore::ephemeral`], so
/// consumers still see exactly one type. A no-op manifest imposes no boundary
/// guard (reads pass straight through) and records nothing.
pub struct SessionAttachmentStore {
    backend: Arc<dyn AttachmentStore>,
    manifest: Arc<dyn AttachmentManifest>,
    session_id: String,
    owner: Mutex<Option<(crate::AttachmentOwnerKind, String)>>,
    clock: Arc<dyn crate::Clock>,
}

pub(crate) struct AttachmentOwnerBinding {
    store: Arc<SessionAttachmentStore>,
    kind: crate::AttachmentOwnerKind,
    owner_id: String,
    previous: Option<(crate::AttachmentOwnerKind, String)>,
}

impl Drop for AttachmentOwnerBinding {
    fn drop(&mut self) {
        self.store
            .restore_owner(self.kind, &self.owner_id, self.previous.take());
    }
}

impl SessionAttachmentStore {
    pub fn new(
        backend: Arc<dyn AttachmentStore>,
        manifest: Arc<dyn AttachmentManifest>,
        session_id: impl Into<String>,
    ) -> Self {
        Self::new_with_clock(backend, manifest, session_id, Arc::new(crate::SystemClock))
    }

    pub(crate) fn new_with_clock(
        backend: Arc<dyn AttachmentStore>,
        manifest: Arc<dyn AttachmentManifest>,
        session_id: impl Into<String>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            backend,
            manifest,
            session_id: session_id.into(),
            owner: Mutex::new(None),
            clock,
        }
    }

    /// Ephemeral facade: wrap `backend` with a no-op manifest and an empty
    /// session id. No boundary guard, no reference tracking — used by ephemeral
    /// runtimes and tests with no durable reference store.
    pub fn ephemeral(backend: Arc<dyn AttachmentStore>) -> Self {
        Self::new(backend, Arc::new(NoopAttachmentManifest), String::new())
    }

    /// Ephemeral facade over a fresh in-memory backend.
    pub fn in_memory() -> Self {
        Self::ephemeral(Arc::new(InMemoryAttachmentStore::new()))
    }

    pub fn backend(&self) -> &Arc<dyn AttachmentStore> {
        &self.backend
    }

    pub fn manifest(&self) -> &Arc<dyn AttachmentManifest> {
        &self.manifest
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn persistence(&self) -> AttachmentStorePersistence {
        self.backend.persistence()
    }

    /// Bind puts for the lifetime of a durable turn execution.
    pub(crate) fn bind_turn_scoped(
        self: &Arc<Self>,
        turn_id: impl Into<String>,
    ) -> AttachmentOwnerBinding {
        self.bind_owner_scoped(crate::AttachmentOwnerKind::Turn, turn_id.into())
    }

    /// Bind puts for the lifetime of a recovered ToolCall or Engine process.
    pub(crate) fn bind_process_scoped(
        self: &Arc<Self>,
        process_id: impl Into<String>,
    ) -> AttachmentOwnerBinding {
        self.bind_owner_scoped(crate::AttachmentOwnerKind::Process, process_id.into())
    }

    fn bind_owner_scoped(
        self: &Arc<Self>,
        kind: crate::AttachmentOwnerKind,
        owner_id: String,
    ) -> AttachmentOwnerBinding {
        let previous = self
            .owner
            .lock()
            .expect("attachment owner binding lock")
            .replace((kind, owner_id.clone()));
        AttachmentOwnerBinding {
            store: Arc::clone(self),
            kind,
            owner_id,
            previous,
        }
    }

    fn restore_owner(
        &self,
        kind: crate::AttachmentOwnerKind,
        owner_id: &str,
        previous: Option<(crate::AttachmentOwnerKind, String)>,
    ) {
        let mut owner = self.owner.lock().expect("attachment owner binding lock");
        if owner
            .as_ref()
            .is_some_and(|(current_kind, id)| *current_kind == kind && id == owner_id)
        {
            *owner = previous;
        }
    }

    pub async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let attachment_id = content_id(&bytes);
        let owner = self
            .owner
            .lock()
            .expect("attachment owner binding lock")
            .clone();
        let intent = AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: self.session_id.clone(),
            canonical_uri: attachment_uri(&attachment_id),
            intent_at_epoch_ms: self.clock.timestamp_ms(),
            owner_kind: owner.as_ref().map(|(kind, _)| *kind),
            owner_id: owner.map(|(_, id)| id),
        };
        // Record intent first. If this fails the bytes never land, matching the
        // write-ahead guarantee.
        self.manifest.record_intent(intent).map_err(|err| {
            AttachmentStoreError::ManifestRecordFailed(format!(
                "failed to record attachment intent for `{attachment_id}`: {err}"
            ))
        })?;
        let reference = self.backend.put(bytes, meta).await?;
        if reference.id != attachment_id {
            return Err(AttachmentStoreError::Backend(format!(
                "attachment store returned id `{}` after manifest intent for `{attachment_id}`",
                reference.id
            )));
        }
        Ok(reference)
    }

    pub async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        // Session-boundary guard: refuse to resolve a blob this session never
        // referenced, even if the backend physically holds identical bytes for
        // another session.
        let holds_ref = self
            .manifest
            .holds_ref(&self.session_id, id)
            .map_err(|err| {
                AttachmentStoreError::Backend(format!(
                    "failed to check attachment manifest for `{id}`: {err}"
                ))
            })?;
        if !holds_ref {
            return Err(AttachmentStoreError::NotFound(id.clone()));
        }
        self.backend.get(id).await
    }

    pub async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        // Drop this session's manifest ref. Backend bytes stay put; they are
        // reclaimed by GC once no session references them.
        self.manifest.forget(&self.session_id, id).map_err(|err| {
            AttachmentStoreError::ManifestRecordFailed(format!(
                "failed to forget attachment ref for `{id}`: {err}"
            ))
        })?;
        Ok(())
    }
}

/// No-op [`AttachmentManifest`] for ephemeral facades: records nothing, imposes
/// no boundary guard (`holds_ref` returns `true`), and exposes no refs. The
/// backend is the sole source of truth for these runtimes.
pub struct NoopAttachmentManifest;

impl AttachmentManifest for NoopAttachmentManifest {
    fn record_intent(&self, _intent: AttachmentIntent) -> Result<(), StoreError> {
        Ok(())
    }

    fn commit_refs(
        &self,
        _session_id: &str,
        _attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn list_uncommitted(
        &self,
        _older_than_epoch_ms: u64,
    ) -> Result<Vec<crate::AttachmentManifestEntry>, StoreError> {
        Ok(Vec::new())
    }

    fn forget(&self, _session_id: &str, _attachment_id: &AttachmentId) -> Result<(), StoreError> {
        Ok(())
    }

    fn holds_ref(
        &self,
        _session_id: &str,
        _attachment_id: &AttachmentId,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        Ok(Vec::new())
    }
}

fn attachment_uri(attachment_id: &AttachmentId) -> String {
    format!("lash-attachment://sha256/{attachment_id}")
}

fn now_epoch_ms() -> u64 {
    <crate::SystemClock as crate::Clock>::timestamp_ms(&crate::SystemClock)
}

/// Adapter that exposes the [`AttachmentManifest`] supertrait of an
/// `Arc<dyn RuntimePersistence>` as an `Arc<dyn AttachmentManifest>`.
/// Rust's trait-object upcasting does not yet allow direct coercion
/// between the two; this thin forwarder is the bridge.
pub(crate) struct PersistenceManifestAdapter(pub Arc<dyn crate::RuntimePersistence>);

impl AttachmentManifest for PersistenceManifestAdapter {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), crate::StoreError> {
        AttachmentManifest::record_intent(&*self.0, intent)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        AttachmentManifest::commit_refs(&*self.0, session_id, attachment_ids)
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<crate::AttachmentManifestEntry>, crate::StoreError> {
        AttachmentManifest::list_uncommitted(&*self.0, older_than_epoch_ms)
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        AttachmentManifest::forget(&*self.0, session_id, attachment_id)
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, crate::StoreError> {
        AttachmentManifest::holds_ref(&*self.0, session_id, attachment_id)
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, crate::StoreError> {
        AttachmentManifest::list_all_refs(&*self.0)
    }
}

fn stored_meta(bytes: &[u8], meta: AttachmentCreateMeta) -> AttachmentMeta {
    AttachmentMeta::new(
        content_id(bytes),
        meta.media_type,
        bytes.len() as u64,
        meta.type_metadata,
        meta.label,
    )
}

pub async fn resolve_llm_request_attachments(
    mut request: crate::llm::types::LlmRequest,
    store: &SessionAttachmentStore,
) -> Result<crate::llm::types::LlmRequest, AttachmentStoreError> {
    for attachment in &request.attachments {
        let crate::AttachmentSource::Stored { attachment_ref } = attachment else {
            continue;
        };
        if request.resolved_stored.contains_key(&attachment_ref.id) {
            continue;
        }
        let stored = store.get(&attachment_ref.id).await?;
        request
            .resolved_stored
            .insert(attachment_ref.id.clone(), stored.bytes);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::{AttachmentTypeMetadata, MediaType};

    #[derive(Default)]
    struct RecordingManifest {
        entries: Mutex<HashMap<(String, AttachmentId), crate::AttachmentManifestEntry>>,
    }

    impl AttachmentManifest for RecordingManifest {
        fn record_intent(&self, intent: AttachmentIntent) -> Result<(), crate::StoreError> {
            let key = (intent.session_id.clone(), intent.attachment_id.clone());
            self.entries
                .lock()
                .expect("lock entries")
                .entry(key)
                .or_insert(crate::AttachmentManifestEntry {
                    attachment_id: intent.attachment_id,
                    session_id: intent.session_id,
                    canonical_uri: intent.canonical_uri,
                    intent_at_epoch_ms: intent.intent_at_epoch_ms,
                    committed_at_epoch_ms: None,
                    owner_kind: intent.owner_kind,
                    owner_id: intent.owner_id,
                });
            Ok(())
        }

        fn commit_refs(
            &self,
            session_id: &str,
            attachment_ids: &[AttachmentId],
        ) -> Result<(), crate::StoreError> {
            let mut entries = self.entries.lock().expect("lock entries");
            for attachment_id in attachment_ids {
                if let Some(entry) =
                    entries.get_mut(&(session_id.to_string(), attachment_id.clone()))
                {
                    entry.committed_at_epoch_ms.get_or_insert(1);
                }
            }
            Ok(())
        }

        fn list_uncommitted(
            &self,
            older_than_epoch_ms: u64,
        ) -> Result<Vec<crate::AttachmentManifestEntry>, crate::StoreError> {
            Ok(self
                .entries
                .lock()
                .expect("lock entries")
                .values()
                .filter(|entry| {
                    entry.committed_at_epoch_ms.is_none()
                        && entry.intent_at_epoch_ms <= older_than_epoch_ms
                })
                .cloned()
                .collect())
        }

        fn forget(
            &self,
            session_id: &str,
            attachment_id: &AttachmentId,
        ) -> Result<(), crate::StoreError> {
            self.entries
                .lock()
                .expect("lock entries")
                .remove(&(session_id.to_string(), attachment_id.clone()));
            Ok(())
        }

        fn holds_ref(
            &self,
            session_id: &str,
            attachment_id: &AttachmentId,
        ) -> Result<bool, crate::StoreError> {
            Ok(self
                .entries
                .lock()
                .expect("lock entries")
                .contains_key(&(session_id.to_string(), attachment_id.clone())))
        }

        fn list_all_refs(&self) -> Result<Vec<AttachmentId>, crate::StoreError> {
            Ok(self
                .entries
                .lock()
                .expect("lock entries")
                .values()
                .map(|entry| entry.attachment_id.clone())
                .collect())
        }
    }

    /// Root set backed by a set of [`RecordingManifest`]s — the in-memory
    /// analogue of a factory unioning its sessions' refs.
    struct RecordingRootSet {
        manifests: Vec<Arc<RecordingManifest>>,
    }

    #[async_trait::async_trait]
    impl AttachmentRootSet for RecordingRootSet {
        async fn live_attachment_refs(
            &self,
            intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
            let mut refs = BTreeSet::new();
            for manifest in &self.manifests {
                // This test root set contains only ownerless host puts, whose
                // documented fallback remains age-only reconciliation.
                for aged in manifest.list_uncommitted(intent_grace_cutoff_epoch_ms)? {
                    manifest.forget(&aged.session_id, &aged.attachment_id)?;
                }
                refs.extend(manifest.list_all_refs()?);
            }
            Ok(refs)
        }
    }

    fn meta() -> AttachmentCreateMeta {
        AttachmentCreateMeta::new(
            MediaType::parse("image/png").unwrap(),
            Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("pixel".to_string()),
        )
    }

    #[tokio::test]
    async fn memory_store_dedupes_by_bytes() {
        let store = InMemoryAttachmentStore::new();
        let a = store.put(vec![1, 2, 3], meta()).await.expect("put a");
        let b = store.put(vec![1, 2, 3], meta()).await.expect("put b");
        assert_eq!(a.id, b.id);
        assert_eq!(a.byte_len, 3);
        assert_eq!(store.get(&a.id).await.expect("get").bytes, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn memory_store_assigns_identity_and_byte_len_from_bytes() {
        let store = InMemoryAttachmentStore::new();
        let reference = store.put(vec![4, 5, 6, 7], meta()).await.expect("put");

        assert_eq!(reference.id, content_id(&[4, 5, 6, 7]));
        assert_eq!(reference.byte_len, 4);
    }

    #[tokio::test]
    async fn memory_store_lists_stored_blobs() {
        let store = InMemoryAttachmentStore::new();
        let a = store.put(vec![1], meta()).await.expect("put a");
        let b = store.put(vec![2], meta()).await.expect("put b");
        let listed: BTreeSet<AttachmentId> = store
            .list()
            .await
            .expect("list")
            .into_iter()
            .map(|blob| blob.id)
            .collect();
        assert!(listed.contains(&a.id));
        assert!(listed.contains(&b.id));
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn facade_get_is_gated_by_manifest_ownership() {
        let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let manifest: Arc<dyn AttachmentManifest> = Arc::new(RecordingManifest::default());
        let session_a = SessionAttachmentStore::new(backend.clone(), manifest.clone(), "session-a");
        let session_b = SessionAttachmentStore::new(backend.clone(), manifest.clone(), "session-b");

        let reference = session_a.put(vec![7, 7, 7], meta()).await.expect("put a");
        // Session A holds the ref and resolves the blob.
        assert_eq!(
            session_a.get(&reference.id).await.expect("a reads").bytes,
            vec![7, 7, 7]
        );
        // Session B shares the backend blob but never referenced it: NotFound.
        assert!(matches!(
            session_b.get(&reference.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn facade_delete_drops_ref_but_keeps_backend_bytes() {
        let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let manifest: Arc<dyn AttachmentManifest> = Arc::new(RecordingManifest::default());
        let session = SessionAttachmentStore::new(backend.clone(), manifest, "session-1");

        let reference = session.put(vec![9, 9], meta()).await.expect("put");
        session.delete(&reference.id).await.expect("delete ref");

        // Facade no longer resolves it (ref dropped)...
        assert!(matches!(
            session.get(&reference.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ));
        // ...but the backend still physically holds the bytes (GC's job).
        assert_eq!(
            backend
                .get(&reference.id)
                .await
                .expect("bytes remain")
                .bytes,
            vec![9, 9]
        );
    }

    #[tokio::test]
    async fn shared_bytes_survive_until_all_refs_released_then_gc_collects() {
        let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let manifest_a = Arc::new(RecordingManifest::default());
        let manifest_b = Arc::new(RecordingManifest::default());
        let session_a = SessionAttachmentStore::new(
            backend.clone(),
            manifest_a.clone() as Arc<dyn AttachmentManifest>,
            "session-a",
        );
        let session_b = SessionAttachmentStore::new(
            backend.clone(),
            manifest_b.clone() as Arc<dyn AttachmentManifest>,
            "session-b",
        );

        // Two sessions put identical bytes: ONE physical blob. Commit both refs
        // so they are stable committed roots (not grace-gated intents), letting
        // this test exercise ref-driven collection independently of intent aging.
        let ref_a = session_a.put(vec![5, 5, 5], meta()).await.expect("put a");
        let ref_b = session_b.put(vec![5, 5, 5], meta()).await.expect("put b");
        assert_eq!(ref_a.id, ref_b.id);
        manifest_a
            .commit_refs("session-a", std::slice::from_ref(&ref_a.id))
            .expect("commit a");
        manifest_b
            .commit_refs("session-b", std::slice::from_ref(&ref_b.id))
            .expect("commit b");
        assert_eq!(backend.list().await.expect("list").len(), 1);

        let root_set = RecordingRootSet {
            manifests: vec![manifest_a.clone(), manifest_b.clone()],
        };

        // Session A releases its ref. Blob is still referenced by B: spared.
        session_a.delete(&ref_a.id).await.expect("a releases");
        let report = reclaim_unreferenced_attachments(&root_set, &*backend, 0)
            .await
            .expect("sweep with b holding a ref");
        assert_eq!(report.reclaimed_count, 0, "b still references the blob");
        assert_eq!(
            backend.get(&ref_b.id).await.expect("blob alive").bytes,
            vec![5, 5, 5]
        );

        // Session B releases too. Now unreferenced: GC collects it.
        session_b.delete(&ref_b.id).await.expect("b releases");
        let report = reclaim_unreferenced_attachments(&root_set, &*backend, 0)
            .await
            .expect("sweep with no refs");
        assert_eq!(report.reclaimed_count, 1);
        assert!(matches!(
            backend.get(&ref_b.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn gc_spares_fresh_in_flight_intents_as_refs() {
        let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let manifest = Arc::new(RecordingManifest::default());
        let session = SessionAttachmentStore::new(
            backend.clone(),
            manifest.clone() as Arc<dyn AttachmentManifest>,
            "session-1",
        );

        // Fresh ownerless host intent: the legacy fallback retains it through
        // the grace window.
        let reference = session.put(vec![3, 1, 4], meta()).await.expect("put");
        let root_set = RecordingRootSet {
            manifests: vec![manifest.clone()],
        };
        const GRACE_MS: u64 = 60 * 60 * 1000;
        let report = reclaim_unreferenced_attachments(&root_set, &*backend, GRACE_MS)
            .await
            .expect("sweep");
        assert_eq!(report.reclaimed_count, 0, "a fresh intent is a live ref");
        assert_eq!(
            backend.get(&reference.id).await.expect("kept").bytes,
            vec![3, 1, 4]
        );
    }

    // Ownerless host puts have no durable liveness proof, so their fallback is
    // age-only: an old intent is reconciled and a fresh one survives.
    #[tokio::test]
    async fn gc_collects_aged_uncommitted_intent_orphan() {
        let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
        let manifest = Arc::new(RecordingManifest::default());
        let session = SessionAttachmentStore::new(
            backend.clone(),
            manifest.clone() as Arc<dyn AttachmentManifest>,
            "session-1",
        );

        // With a zero grace window the ownerless intent is already eligible.
        let orphan = session
            .put(vec![9, 9, 9], meta())
            .await
            .expect("put orphan");
        let root_set = RecordingRootSet {
            manifests: vec![manifest.clone()],
        };
        let report = reclaim_unreferenced_attachments(&root_set, &*backend, 0)
            .await
            .expect("sweep");
        assert_eq!(
            report.reclaimed_count, 1,
            "an aged, never-committed intent is a collectable orphan"
        );
        assert!(matches!(
            backend.get(&orphan.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ));
        // The intent row was reconciled away too, so it is no longer a root.
        assert!(manifest.list_all_refs().expect("refs").is_empty());
    }

    // Fix C: the GC delete-time re-check. A blob looks unreferenced and stale in
    // the `list` snapshot, but a new intent + `put` of the same content id landed
    // after the snapshot, refreshing the blob. The sweep must re-stat the blob via
    // `head` before deleting and spare it. Modelled sequentially with a backend
    // whose `list` reports a stale mtime and whose `head` reports a fresh one.
    #[tokio::test]
    async fn gc_delete_recheck_spares_blob_refreshed_after_snapshot() {
        struct StaleSnapshotStore {
            id: AttachmentId,
            list_mtime: u64,
            head_mtime: u64,
            deleted: Mutex<bool>,
        }

        #[async_trait::async_trait]
        impl AttachmentStore for StaleSnapshotStore {
            async fn put(
                &self,
                _bytes: Vec<u8>,
                _meta: AttachmentCreateMeta,
            ) -> Result<AttachmentRef, AttachmentStoreError> {
                unreachable!("test does not put through this store")
            }
            async fn get(
                &self,
                id: &AttachmentId,
            ) -> Result<StoredAttachment, AttachmentStoreError> {
                Err(AttachmentStoreError::NotFound(id.clone()))
            }
            async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
                *self.deleted.lock().expect("lock") = true;
                Ok(())
            }
            async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
                // Stale snapshot: the blob looks old and unreferenced.
                Ok(vec![StoredBlobRef {
                    id: self.id.clone(),
                    last_modified_epoch_ms: Some(self.list_mtime),
                }])
            }
            async fn head(
                &self,
                id: &AttachmentId,
            ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
                // Fresh re-stat: the new intent's `put` refreshed the blob.
                Ok((id == &self.id).then(|| StoredBlobRef {
                    id: id.clone(),
                    last_modified_epoch_ms: Some(self.head_mtime),
                }))
            }
        }

        let now = now_epoch_ms();
        const GRACE_MS: u64 = 60 * 60 * 1000;
        let backend = StaleSnapshotStore {
            id: AttachmentId::new("recheck"),
            // Stale mtime well past the grace window: the first check would delete.
            list_mtime: now.saturating_sub(GRACE_MS * 2),
            // Fresh mtime inside the window: the re-check must spare it.
            head_mtime: now,
            deleted: Mutex::new(false),
        };
        // Empty root set: the snapshot did not see the new intent.
        let root_set = RecordingRootSet { manifests: vec![] };
        let report = reclaim_unreferenced_attachments(&root_set, &backend, GRACE_MS)
            .await
            .expect("sweep");
        assert_eq!(
            report.reclaimed_count, 0,
            "the delete-time re-check must spare a blob refreshed after the snapshot"
        );
        assert!(
            !*backend.deleted.lock().expect("lock"),
            "the freshly-refreshed blob must not be deleted"
        );
    }

    // Fix C, checks (b) and (d): the delete-window root re-check. A candidate blob
    // is stale in both the `list` snapshot AND the `head` re-stat (so the freshness
    // gate does not spare it), but a session records a fresh intent for the same
    // content id in the delete window. A backend whose `head` reports a stale mtime
    // and a root set scripted to answer the single-id probe drive the two branches:
    // (b) a ref present before delete spares the blob; (d) a ref that appears only
    // after delete is detected and alarmed via `deleted_while_referenced`.
    struct StaleHeadStore {
        id: AttachmentId,
        mtime: u64,
        deleted: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl AttachmentStore for StaleHeadStore {
        async fn put(
            &self,
            _bytes: Vec<u8>,
            _meta: AttachmentCreateMeta,
        ) -> Result<AttachmentRef, AttachmentStoreError> {
            unreachable!("test does not put through this store")
        }
        async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
            Err(AttachmentStoreError::NotFound(id.clone()))
        }
        async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
            *self.deleted.lock().expect("lock") = true;
            Ok(())
        }
        async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
            Ok(vec![StoredBlobRef {
                id: self.id.clone(),
                last_modified_epoch_ms: Some(self.mtime),
            }])
        }
        async fn head(
            &self,
            id: &AttachmentId,
        ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
            // Stale re-stat: the freshness gate does not spare the blob, so the
            // targeted root re-check is what must decide its fate.
            Ok((id == &self.id).then(|| StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms: Some(self.mtime),
            }))
        }
    }

    /// Root set whose single-id probe returns scripted answers in order — models
    /// a ref appearing at a chosen point in the delete window. `live_attachment_refs`
    /// is empty (the snapshot never saw the late ref).
    struct ScriptedRootSet {
        answers: Mutex<std::collections::VecDeque<bool>>,
    }

    #[async_trait::async_trait]
    impl AttachmentRootSet for ScriptedRootSet {
        async fn live_attachment_refs(
            &self,
            _intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
            Ok(BTreeSet::new())
        }

        async fn has_live_attachment_ref(
            &self,
            _id: &AttachmentId,
            _intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<bool, crate::StoreError> {
            Ok(self
                .answers
                .lock()
                .expect("lock answers")
                .pop_front()
                .unwrap_or(false))
        }
    }

    #[tokio::test]
    async fn gc_pre_delete_root_recheck_spares_reappeared_ref() {
        let now = now_epoch_ms();
        const GRACE_MS: u64 = 60 * 60 * 1000;
        let backend = StaleHeadStore {
            id: AttachmentId::new("reappeared"),
            // Well past the grace window: neither the snapshot nor the head re-stat
            // spares it, so the single-id root re-check is the only guard left.
            mtime: now.saturating_sub(GRACE_MS * 2),
            deleted: Mutex::new(false),
        };
        // (b) sees a live ref: the probe answers true before the delete.
        let root_set = ScriptedRootSet {
            answers: Mutex::new([true].into_iter().collect()),
        };
        let report = reclaim_unreferenced_attachments(&root_set, &backend, GRACE_MS)
            .await
            .expect("sweep");
        assert_eq!(
            report.reclaimed_count, 0,
            "pre-delete root re-check must spare the blob"
        );
        assert!(report.deleted_while_referenced.is_empty());
        assert!(
            !*backend.deleted.lock().expect("lock"),
            "a blob re-referenced before delete must not be deleted"
        );
    }

    #[tokio::test]
    async fn gc_post_delete_root_recheck_alarms_on_window_ref() {
        let now = now_epoch_ms();
        const GRACE_MS: u64 = 60 * 60 * 1000;
        let id = AttachmentId::new("window-ref");
        let backend = StaleHeadStore {
            id: id.clone(),
            mtime: now.saturating_sub(GRACE_MS * 2),
            deleted: Mutex::new(false),
        };
        // (b) sees no ref (delete proceeds); (d) sees a ref that appeared in the
        // (b)->(c) window: detected and alarmed.
        let root_set = ScriptedRootSet {
            answers: Mutex::new([false, true].into_iter().collect()),
        };
        let report = reclaim_unreferenced_attachments(&root_set, &backend, GRACE_MS)
            .await
            .expect("sweep");
        assert_eq!(report.reclaimed_count, 1, "the blob is deleted");
        assert!(*backend.deleted.lock().expect("lock"), "delete happened");
        assert_eq!(
            report.deleted_while_referenced,
            vec![id],
            "a ref appearing in the delete window is recorded for the operator alarm"
        );
    }

    #[tokio::test]
    async fn session_facade_records_bound_owner_on_put() {
        let manifest = Arc::new(RecordingManifest::default());
        let store = Arc::new(SessionAttachmentStore::new(
            Arc::new(InMemoryAttachmentStore::new()),
            manifest.clone(),
            "session-1",
        ));
        let binding = store.bind_turn_scoped("turn-1");

        let reference = store.put(vec![8, 9, 10], meta()).await.expect("put");
        {
            let entries = manifest.entries.lock().expect("lock entries");
            let entry = entries
                .get(&("session-1".to_string(), reference.id))
                .expect("manifest entry");
            assert_eq!(entry.owner_kind, Some(crate::AttachmentOwnerKind::Turn));
            assert_eq!(entry.owner_id.as_deref(), Some("turn-1"));
        }

        drop(binding);
        let host_reference = store.put(vec![11, 12], meta()).await.expect("host put");
        let entries = manifest.entries.lock().expect("lock entries");
        let host_entry = entries
            .get(&("session-1".to_string(), host_reference.id))
            .expect("host manifest entry");
        assert_eq!(host_entry.owner_kind, None);
        assert_eq!(host_entry.owner_id, None);
    }

    #[tokio::test]
    async fn nested_owner_binding_restores_the_previous_owner() {
        let manifest = Arc::new(RecordingManifest::default());
        let store = Arc::new(SessionAttachmentStore::new(
            Arc::new(InMemoryAttachmentStore::new()),
            manifest.clone(),
            "session-1",
        ));
        let process_binding = store.bind_process_scoped("process-1");
        let turn_binding = store.bind_turn_scoped("turn-1");

        let turn_ref = store.put(vec![1], meta()).await.expect("turn put");
        drop(turn_binding);
        let process_ref = store.put(vec![2], meta()).await.expect("process put");
        drop(process_binding);
        let host_ref = store.put(vec![3], meta()).await.expect("host put");

        let entries = manifest.entries.lock().expect("lock entries");
        let turn = entries
            .get(&("session-1".to_string(), turn_ref.id))
            .expect("turn entry");
        assert_eq!(turn.owner_kind, Some(crate::AttachmentOwnerKind::Turn));
        assert_eq!(turn.owner_id.as_deref(), Some("turn-1"));
        let process = entries
            .get(&("session-1".to_string(), process_ref.id))
            .expect("process entry");
        assert_eq!(
            process.owner_kind,
            Some(crate::AttachmentOwnerKind::Process)
        );
        assert_eq!(process.owner_id.as_deref(), Some("process-1"));
        let host = entries
            .get(&("session-1".to_string(), host_ref.id))
            .expect("host entry");
        assert_eq!(host.owner_kind, None);
        assert_eq!(host.owner_id, None);
    }

    #[tokio::test]
    async fn ephemeral_facade_passes_reads_through_without_a_guard() {
        let store = SessionAttachmentStore::in_memory();
        let reference = store.put(vec![1, 2, 3], meta()).await.expect("put");
        assert_eq!(
            store.get(&reference.id).await.expect("get").bytes,
            vec![1, 2, 3]
        );
    }

    #[test]
    fn persistence_manifest_adapter_forwards_holds_ref() {
        let runtime: Arc<dyn crate::RuntimePersistence> =
            Arc::new(crate::InMemorySessionStore::new());
        let adapter = PersistenceManifestAdapter(runtime);
        let attachment_id = AttachmentId::new("adapter-forwarding");
        let intent = AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: "adapter-session".to_string(),
            canonical_uri: attachment_uri(&attachment_id),
            intent_at_epoch_ms: 10,
            owner_kind: None,
            owner_id: None,
        };
        adapter.record_intent(intent).expect("record intent");
        assert!(
            adapter
                .holds_ref("adapter-session", &attachment_id)
                .expect("holds ref")
        );
        assert!(
            !adapter
                .holds_ref("other-session", &attachment_id)
                .expect("no ref for other session")
        );
        assert_eq!(
            adapter.list_all_refs().expect("list all refs"),
            vec![attachment_id]
        );
    }
}
