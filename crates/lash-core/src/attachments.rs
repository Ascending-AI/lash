use lash_sansio::sync::MutexExt;
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
    #[error(
        "attachment GC refused an empty live root set with a deletion-eligible blob; set `AttachmentReclamationPolicy::empty_root_set` to `EmptyRootSetPolicy::AuthorizeDeleteAll` only when deleting every unreferenced blob is intended"
    )]
    EmptyRootSetRefused,
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
///
/// Implementors that map an id into a namespaced storage path or object key
/// must reject malformed ids *before* constructing that path or key. A storage
/// id is 1 to 128 bytes of printable ASCII, contains no `/` or `\\`, is not `.`
/// or `..`, and has no absolute or platform-prefix form. This keeps lookup and
/// deletion inside the backend namespace even when an id came from an
/// untrusted protocol. Such backends return their typed invalid-id error, or
/// [`AttachmentStoreError::NotFound`] when they have no separate invalid-id
/// variant.
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

    /// Fetch one blob.
    ///
    /// Namespaced-storage implementors must apply the trait-level id-shape
    /// guard before deriving any path or key from `id`.
    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError>;

    /// Remove one blob. Idempotent: deleting an absent blob is a no-op. This is
    /// the primitive mark-and-sweep GC uses to reclaim unreferenced content;
    /// per-session lifecycle is expressed by dropping manifest refs, never by
    /// calling this directly for a live session.
    ///
    /// Namespaced-storage implementors must apply the trait-level id-shape
    /// guard before deriving any path or key from `id`.
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
    ///
    /// Overrides that derive a namespaced path or key from `id` must apply the
    /// trait-level id-shape guard first.
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
/// sweep time (SQLite); an in-memory factory answers from its live stores. If
/// the implementor cannot enumerate its roots, it must return an error from
/// [`Self::live_attachment_refs`]. The sweep then lists the backend only to
/// determine whether a deletion-eligible blob exists: it propagates the error
/// before deleting anything when one does, or returns a report carrying the
/// failure when every blob is still protected by the grace window. Even a
/// successfully enumerated empty set cannot authorize deletion by itself;
/// [`AttachmentReclamationPolicy`] controls that separate destructive
/// assertion.
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
    /// indexed query / first-hit scan rather than materializing the whole set.
    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError>;
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
    /// Root-enumeration failure suppressed because the backend contained no
    /// deletion-eligible blobs. The sweep deleted nothing. Hosts should expose
    /// this diagnostic separately from a healthy empty sweep.
    pub root_enumeration_failure: Option<String>,
    /// Blobs that were deleted but a live root re-appeared for in the residual
    /// window between the pre-delete root re-check and the delete itself. The
    /// bytes are already gone and cannot be restored, but a put-always-writes
    /// backend self-heals on the referencing session's next `put` (the intent's
    /// write-ahead ordering guarantees a retry rewrites the bytes). Recorded and
    /// logged at error level so an operator sees the (single-digit-millisecond,
    /// self-healing) event rather than a silent data loss.
    pub deleted_while_referenced: Vec<AttachmentId>,
}

/// Host authorization for interpreting an empty attachment root set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmptyRootSetPolicy {
    /// Refuse a sweep when an empty root set would authorize deletion.
    #[default]
    Refuse,
    /// Assert that deleting every unreferenced, deletion-eligible blob is intended.
    AuthorizeDeleteAll,
}

/// Host-owned policy for one attachment reclamation sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentReclamationPolicy {
    /// Post-terminal retention window and delete-time freshness window.
    pub grace_period_ms: u64,
    /// How the sweep may interpret an empty live root set.
    pub empty_root_set: EmptyRootSetPolicy,
}

/// Mark-and-sweep GC for attachment blobs — the host-invocable counterpart to
/// [`StoreMaintenance::gc_unreachable`](crate::StoreMaintenance::gc_unreachable)
/// for attachment payloads.
///
/// Enumerates every blob in `backend`, computes the live root set from
/// `root_set` (committed refs plus intents whose durable owners can still
/// commit), and deletes every blob no session references. The policy's
/// `grace_period_ms` retention window delays reclamation after owner death and
/// protects freshly written blobs even if they currently look unreferenced.
/// An empty live root set refuses deletion by default; the host must explicitly
/// authorize the destructive interpretation through [`EmptyRootSetPolicy`].
/// This valve catches an empty root set, not a wrong store whose unrelated data
/// makes its root set non-empty. A refusal returns an error and therefore does
/// not return the partial report accumulated before the first eligible blob.
/// Per-blob delete failures are collected into
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
/// This is a lever, not a scheduler: the host passes an
/// [`AttachmentReclamationPolicy`] choosing `grace_period_ms` as a post-terminal
/// retention policy, explicitly decides whether an empty root set may authorize
/// deletion, and chooses when to run it. The window is not a correctness bound
/// on replay duration. The lever does no background work.
pub async fn reclaim_unreferenced_attachments<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    policy: AttachmentReclamationPolicy,
) -> Result<AttachmentReclamationReport, AttachmentStoreError>
where
    R: AttachmentRootSet + ?Sized,
{
    let now = now_epoch_ms();
    let grace_period_ms = policy.grace_period_ms;
    let intent_grace_cutoff = now.saturating_sub(grace_period_ms);
    let live = root_set.live_attachment_refs(intent_grace_cutoff).await;
    let blobs = backend.list().await?;
    let mut report = AttachmentReclamationReport {
        scanned_blob_count: blobs.len(),
        ..AttachmentReclamationReport::default()
    };
    let live = match live {
        Ok(live) => live,
        Err(err) => {
            let failure = format!("failed to enumerate live attachment refs: {err}");
            if blobs
                .iter()
                .any(|blob| !within_grace(blob.last_modified_epoch_ms, now, grace_period_ms))
            {
                return Err(AttachmentStoreError::Backend(failure));
            }
            tracing::warn!(
                scanned_blob_count = report.scanned_blob_count,
                grace_period_ms,
                root_enumeration_failure = %failure,
                "attachment GC could not enumerate live roots but found no deletion-eligible blobs"
            );
            report.root_enumeration_failure = Some(failure);
            return Ok(report);
        }
    };
    for blob in blobs {
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
        if live.is_empty() && policy.empty_root_set != EmptyRootSetPolicy::AuthorizeDeleteAll {
            tracing::warn!(
                live_root_count = live.len(),
                scanned_blob_count = report.scanned_blob_count,
                deletion_candidate_id = %blob.id,
                grace_period_ms,
                empty_root_set_policy = ?policy.empty_root_set,
                "attachment GC refused an empty live root set with a deletion-eligible blob"
            );
            return Err(AttachmentStoreError::EmptyRootSetRefused);
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

    /// Snapshot concrete blob bytes without calling the `AttachmentStore`
    /// read path. Intended for cross-backend durable-state differentials.
    #[doc(hidden)]
    #[cfg(any(test, feature = "testing"))]
    pub fn raw_blobs_for_testing(&self) -> Vec<(AttachmentId, Vec<u8>)> {
        let mut rows = self
            .attachments
            .lock_recover()
            .iter()
            .map(|(id, blob)| (id.clone(), blob.stored.bytes.clone()))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
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
        let mut attachments = self.attachments.lock_recover();
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
            .lock_recover()
            .get(id)
            .map(|blob| blob.stored.clone())
            .ok_or_else(|| AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.attachments.lock_recover().remove(id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(self
            .attachments
            .lock_recover()
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
            .lock_recover()
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
        let previous = self.owner.lock_recover().replace((kind, owner_id.clone()));
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
        let mut owner = self.owner.lock_recover();
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
        let owner = self.owner.lock_recover().clone();
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
#[path = "attachments/fail_closed_tests.rs"]
mod fail_closed_tests;

#[cfg(test)]
#[path = "attachments/tests.rs"]
mod tests;
