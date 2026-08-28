use lash_sansio::sync::MutexExt;
mod file_store;

pub use file_store::FileAttachmentStore;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentMeta, AttachmentRef};

use crate::store::{
    AttachmentCondemnation, AttachmentDeleteArming, AttachmentIntent, AttachmentManifest,
    AttachmentWriteFence, StoreError,
};

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
        "attachment `{attachment_id}` is being reclaimed: a sweep armed its physical delete before this write recorded an intent, and the condemnation was still held after {attempts} fence attempts. The sweep may simply be slow — a large or remote delete can outlast the retry window — so retrying the put is the normal response; the fence clears when the sweep releases the digest. If it never clears and no sweep is running, the condemnation was abandoned by a sweeper that died mid-delete, and the host clears it with `AttachmentRootSet::release_attachment_condemnation`."
    )]
    ReclamationInFlight {
        attachment_id: AttachmentId,
        attempts: u32,
    },
}

#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub bytes: Vec<u8>,
}

/// One blob enumerated by [`AttachmentStore::list`]. Feeds mark-and-sweep GC:
/// the sweeper pairs each blob's `id` against the live root set and uses
/// `last_modified_epoch_ms` to apply the write grace period. Backends that
/// cannot report a modification time leave it `None`, and the sweep treats
/// such blobs as always past the grace window. `None` therefore defeats *both*
/// freshness checks in the same sweep — the snapshot's and the delete-time
/// re-stat — so such a backend has no write-grace protection at all and rests
/// entirely on the root set and, depending on the authority, the condemnation
/// fence or the legacy targeted re-check.
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
/// Conventions every backend upholds: `put` is content-idempotent (identical
/// bytes return the same ref without creating a second blob) but refreshes any
/// freshness signal the backend exposes, `delete` is idempotent, and a missing
/// blob maps to [`AttachmentStoreError::NotFound`].
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

    /// Store bytes and return their content-addressed reference.
    ///
    /// Repeating a `put` for bytes already held must refresh the freshness
    /// signal returned by [`Self::head`] when the backend exposes one. The GC
    /// relies on that restamp to distinguish a newly referenced blob from the
    /// older snapshot it is considering for deletion.
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
    /// content id) touched *after* the snapshot must be spared.
    ///
    /// Required, with no default: a backend whose freshness answer is a
    /// `list` scan it never chose is a delete guard nobody wrote. Backends with
    /// a cheap stat/`HEAD` use it; backends without one state the scan
    /// explicitly (`Ok(self.list().await?.into_iter().find(|blob| &blob.id ==
    /// id))`).
    ///
    /// Neither answer risks a delete — the sweep skips the candidate on
    /// `Ok(None)` (read as a concurrent delete) and on `Err` alike. They differ
    /// in what the host learns: an `Err` lands in
    /// [`AttachmentReclamationReport::failed_ids`], while `Ok(None)` passes
    /// silently. A backend that cannot answer should therefore return an error
    /// rather than `None`, so a store that has stopped being able to vouch for
    /// its blobs is visible instead of looking like a steadily empty sweep.
    ///
    /// Implementors that derive a namespaced path or key from `id` must apply
    /// the trait-level id-shape guard first.
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError>;
}

/// A source of the live attachment root set across every session a store
/// factory owns. Committed refs and intents with owners that can still commit
/// are roots; terminal-owner intents remain roots through their retention
/// window. Unscoped host puts use the legacy age-only fallback.
///
/// Implemented by session-store factories, which own the full set of sessions.
/// Every durable backend answers from one factory-wide manifest in a single
/// pass: a global manifest table for Postgres, and for SQLite the single
/// factory-wide `durable-core.db` catalog, which the factory opens for the
/// root-set pass and asks for [`AttachmentManifest::list_all_refs`].
/// An in-memory factory answers from its live stores. If
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

    /// Whether this root authority implements the condemn/intent CAS fence.
    ///
    /// The default is [`AttachmentGcFence::BestEffort`]: an authority that does
    /// not override both this and the condemnation transitions below runs the
    /// sweep's unfenced path, which cannot exclude a concurrent writer from the
    /// query/delete window. See [`reclaim_unreferenced_attachments`].
    ///
    /// # Answering `Fenced` is a five-method claim, across two traits
    ///
    /// The fence is only real when *all* of the following are implemented
    /// against the same durable store, with each one a single conditional
    /// mutation:
    ///
    /// 1. [`AttachmentManifest::begin_attachment_write`] — the **writer half**,
    ///    on the manifest trait, not this one. Overriding the four methods below
    ///    while leaving this at its default means writers record intents without
    ///    consulting the condemnation, so a sweep deletes bytes behind a live
    ///    intent while this method reports `Fenced`. There is no fence without
    ///    it.
    /// 2. [`Self::condemn_attachment`] — `Free -> Condemned`, conditional on the
    ///    root predicate.
    /// 3. [`Self::arm_attachment_delete`] — `Condemned -> Deleting`, conditional
    ///    on the condemnation still being held.
    /// 4. [`Self::release_attachment_condemnation`] — back to `Free`.
    /// 5. This method, answering [`AttachmentGcFence::Fenced`].
    ///
    /// A partial implementation is worse than none: it silences the sweep's
    /// best-effort warning while keeping the loss. As a backstop the sweep
    /// downgrades its own report to [`AttachmentGcFence::BestEffort`] whenever a
    /// self-declared `Fenced` authority answers
    /// [`AttachmentCondemnation::Unsupported`], but it cannot detect a missing
    /// writer half — that one is on the implementer.
    fn fence(&self) -> AttachmentGcFence {
        AttachmentGcFence::BestEffort
    }

    /// `Free -> Condemned` for one digest, conditional on there being no live
    /// root — the GC half of the fence.
    ///
    /// This MUST be one conditional mutation in the same durable store as the
    /// manifest: the root predicate (the same one
    /// [`Self::has_live_attachment_ref`] answers, with the same cutoff) and the
    /// condemnation insert are evaluated together, so a writer's
    /// [`AttachmentManifest::begin_attachment_write`] either lands first (and
    /// this returns [`AttachmentCondemnation::RootPresent`]) or lands after (and
    /// revokes the condemnation this call created). A read-then-insert
    /// implementation is not a fence.
    ///
    /// It never blocks: an existing condemnation returns
    /// [`AttachmentCondemnation::AlreadyCondemned`] and the digest is deferred
    /// to the next sweep.
    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<AttachmentCondemnation, StoreError> {
        let _ = (id, intent_grace_cutoff_epoch_ms);
        Ok(AttachmentCondemnation::Unsupported)
    }

    /// `Condemned -> Deleting` for one digest: the CAS that authorizes the
    /// physical backend delete.
    ///
    /// Returns [`AttachmentDeleteArming::Revoked`] when the condemnation is no
    /// longer there, which is exactly the case where a writer took the digest
    /// back. The sweep then issues no delete at all, so the physical delete is
    /// only ever issued for a digest this call armed — no SQL/blob-store
    /// atomicity required. `Revoked` MUST mean the caller does not own this
    /// digest's delete and must not release it: either a writer already
    /// returned it to `Free`, or the transition is owned elsewhere (a digest
    /// already in `Deleting` also answers `Revoked`, which the sweep cannot
    /// reach — arming requires having won the condemnation first). An
    /// implementation that answers `Revoked` for a condemnation this caller
    /// still owns strands that digest, since the sweep will not hand it back.
    ///
    /// The default is an error, not `Revoked`: it is only ever reached by an
    /// authority that implemented [`Self::condemn_attachment`] without this,
    /// which would otherwise condemn digests it can never arm or hand back —
    /// blocking writers permanently. Failing loudly makes the sweep release the
    /// condemnation and report the digest in
    /// [`AttachmentReclamationReport::failed_ids`] instead.
    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        let _ = id;
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::arm_attachment_delete (required alongside \
                        condemn_attachment)",
        })
    }

    /// Drop the digest's condemnation, returning it to `Free`.
    ///
    /// The sweep calls this after the physical delete lands, and on every path
    /// where it abandons a digest it had condemned. It is also the host-owned
    /// recovery lever (ADR 0014) for a condemnation left behind by a sweeper
    /// that died between arming and releasing: the host asserts that no sweep is
    /// running and clears the digest, unblocking writers. lash never expires a
    /// condemnation on its own — there is no clock in this protocol.
    async fn release_attachment_condemnation(&self, id: &AttachmentId) -> Result<(), StoreError> {
        let _ = id;
        Ok(())
    }
}

/// Whether a sweep's deletes were fenced against concurrent writers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AttachmentGcFence {
    /// The root authority implements the condemn/intent CAS fence: every delete
    /// this sweep issued was for a digest that was proven rootless and held
    /// against writers for the whole query/delete window.
    Fenced,
    /// The root authority implements no fence, so the sweep ran its unfenced
    /// path: a same-content write can still land inside the query/delete window
    /// and lose its bytes. The operation reports itself best-effort rather than
    /// claiming a guarantee it cannot make. Deployments that must stay on an
    /// unfenced authority should point the backend at recoverable deletion —
    /// object-store versioning, a soft-delete/quarantine prefix, or a bucket
    /// lifecycle rule — so a lost write is restorable from backend coordinates
    /// rather than gone.
    #[default]
    BestEffort,
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
    /// Why the live root set could not be enumerated. The sweep deleted
    /// nothing: no blob was deletion-eligible, so nothing was at risk.
    ///
    /// It rides on an `Ok` report only when the backend itself held no blobs at
    /// all — a fresh deployment or an operator reset, where no blob's fate ever
    /// depended on the root set. With blobs present, the sweep refuses with
    /// [`MaintenanceRefusal::UnwitnessedScope`](crate::store::MaintenanceRefusal::UnwitnessedScope)
    /// and this diagnostic rides on the partial report instead: an unwitnessed
    /// root set is never reported as a healthy empty sweep.
    pub root_enumeration_failure: Option<String>,
    /// Detection telemetry: blobs this sweep deleted that a live root existed
    /// for when the sweep probed again immediately afterwards.
    ///
    /// This is a *detector*, not a remedy. Under
    /// [`AttachmentGcFence::Fenced`] it should stay empty — a root cannot appear
    /// for a digest whose delete was armed, because recording a root and arming
    /// a delete are the same CAS — so a non-empty list is evidence that the
    /// fence is not holding (a store implementing the transitions
    /// non-atomically, or two authorities over one backend) and is logged at
    /// error level. Under [`AttachmentGcFence::BestEffort`] it is the only
    /// signal the unfenced window produced a loss; the bytes are gone and lash
    /// cannot restore them.
    pub deleted_while_referenced: Vec<AttachmentId>,
    /// Whether this sweep's deletes were fenced against concurrent writers.
    pub fence: AttachmentGcFence,
    /// Digests this sweep deferred on contention rather than waiting: either a
    /// peer sweeper already held the condemnation, or a writer revoked it before
    /// the delete could be armed. Both are ordinary outcomes — the digest is
    /// simply left for the next sweep.
    ///
    /// A digest that keeps appearing here across sweeps with no other sweeper
    /// and no writer is a condemnation left behind by a sweeper that died
    /// mid-delete; see
    /// [`AttachmentRootSet::release_attachment_condemnation`].
    pub condemn_deferred_ids: Vec<AttachmentId>,
}

impl crate::store::MaintenanceReport for AttachmentReclamationReport {
    fn reclaimed_count(&self) -> usize {
        self.reclaimed_count
    }

    fn sweep(&self) -> crate::store::MaintenanceSweep {
        if !self.failed_ids.is_empty() || !self.condemn_deferred_ids.is_empty() {
            crate::store::MaintenanceSweep::Incomplete
        } else if self.reclaimed_count > 0 {
            crate::store::MaintenanceSweep::Swept
        } else {
            crate::store::MaintenanceSweep::NothingToDo
        }
    }
}

/// An attachment sweep that stopped before completing its scope, carrying the
/// partial report accumulated before the stop (ADR 0067 §4).
pub type AttachmentReclamationFailure =
    crate::store::MaintenanceFailure<AttachmentReclamationReport, AttachmentStoreError>;

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
/// makes its root set non-empty. A refusal is an
/// [`AttachmentReclamationFailure`] carrying
/// [`MaintenanceRefusal::EmptyRootSetUnauthorized`](crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized)
/// and the partial report accumulated before the first eligible blob.
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
/// # The delete window is fenced, not raced
///
/// A snapshot-then-delete sweep has a window: between deciding a digest is
/// unreferenced and physically deleting it, a session can write the same content
/// and lose its bytes. No re-check closes that window, because a re-check is
/// still a read. The sweep closes it with a CAS state machine in the lash-owned
/// root authority instead — no locks, no leases, no TTLs, no wall clock:
///
/// 1. **Condemn before delete.** For each candidate the sweep runs
///    [`AttachmentRootSet::condemn_attachment`], one conditional mutation that
///    inserts a per-digest condemnation *only if no root or intent exists*. A
///    live root ([`AttachmentCondemnation::RootPresent`]) means the digest is
///    not garbage: skip it.
/// 2. **The writer's intent is the fence on the other side.** Every `put`
///    records its intent through
///    [`AttachmentManifest::begin_attachment_write`] *before* the bytes land, in
///    the same conditional mutation that reads the condemnation. So a writer
///    either records first (and the condemn CAS fails) or arrives to a condemned
///    digest and *revokes* the condemnation (and the sweep's arm CAS fails).
///    Whoever loses the CAS yields; nobody waits.
/// 3. **Only an armed digest is deleted.** The physical backend delete is issued
///    exclusively for a digest [`AttachmentRootSet::arm_attachment_delete`]
///    moved to `Deleting`, and the condemnation is released afterwards. A writer
///    that arrives while the delete is in flight records nothing and retries; it
///    is granted once the sweep releases the digest, and re-puts the content.
///    That is why no SQL/blob-store atomicity is needed: the authority's state
///    machine, not the backend, decides whether bytes may die.
/// 4. **Skip on contention.** A digest another sweeper has already condemned is
///    recorded in [`AttachmentReclamationReport::condemn_deferred_ids`] and left
///    for the next sweep. The sweep never blocks a writer and never waits on a
///    peer.
///
/// Reclamation is still driven by the host: the sweep condemns only what it was
/// about to delete anyway, and a condemnation lives no longer than the sweep's
/// own candidate handling. Clearing one left behind by a sweeper that died
/// mid-delete is host policy (ADR 0014), exposed as
/// [`AttachmentRootSet::release_attachment_condemnation`] — lash expires nothing
/// on a timer.
///
/// # Unfenced authorities are best-effort
///
/// A root authority that does not implement the transitions reports
/// [`AttachmentGcFence::BestEffort`] (the default), and the sweep says so in
/// [`AttachmentReclamationReport::fence`] and in a warning. It then runs the
/// legacy path: a targeted root re-check
/// ([`AttachmentRootSet::has_live_attachment_ref`]) before the delete, and the
/// same probe again after it, recording any late root in
/// [`AttachmentReclamationReport::deleted_while_referenced`] as detection
/// telemetry. That path detects the loss; it cannot prevent it. Such deployments
/// should point the backend at recoverable deletion — object-store versioning or
/// a quarantine/soft-delete prefix — so the report's coordinates lead to bytes
/// that can be restored.
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
#[allow(
    clippy::result_large_err,
    reason = "boxing AttachmentReclamationFailure would change this public maintenance API"
)]
pub async fn reclaim_unreferenced_attachments<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    policy: AttachmentReclamationPolicy,
) -> Result<AttachmentReclamationReport, AttachmentReclamationFailure>
where
    R: AttachmentRootSet + ?Sized,
{
    let now = now_epoch_ms();
    let grace_period_ms = policy.grace_period_ms;
    let intent_grace_cutoff = now.saturating_sub(grace_period_ms);
    let live = root_set.live_attachment_refs(intent_grace_cutoff).await;
    let blobs = backend
        .list()
        .await
        .map_err(AttachmentReclamationFailure::failed_before_any_work)?;
    let mut fence = root_set.fence();
    let mut report = AttachmentReclamationReport {
        scanned_blob_count: blobs.len(),
        fence,
        ..AttachmentReclamationReport::default()
    };
    let live = match live {
        Ok(live) => live,
        Err(err) => {
            let failure = format!("failed to enumerate live attachment refs: {err}");
            // Enumeration failure deliberately splits by destructive scope. An
            // eligible blob needed the unavailable roots to decide its fate,
            // so that is a backend failure (`Failed`). Grace-protected blobs
            // require no destructive step, but their scope remains unwitnessed,
            // so that is a policy refusal (`Refused(UnwitnessedScope)`).
            if blobs
                .iter()
                .any(|blob| !within_grace(blob.last_modified_epoch_ms, now, grace_period_ms))
            {
                return Err(AttachmentReclamationFailure::failed(
                    AttachmentStoreError::Backend(failure),
                    report,
                ));
            }
            tracing::warn!(
                scanned_blob_count = report.scanned_blob_count,
                grace_period_ms,
                root_enumeration_failure = %failure,
                "attachment GC could not enumerate live roots but found no deletion-eligible blobs"
            );
            report.root_enumeration_failure = Some(failure);
            if blobs.is_empty() {
                // The backend itself enumerated completely and held nothing, so
                // no blob's fate ever depended on the root set. That is a
                // witnessed nothing-to-do — the case a fresh deployment or an
                // operator reset lands in — and the enumeration diagnostic
                // rides along on a report that is provably empty.
                return Ok(report);
            }
            // Blobs exist and only the grace window spared them. Their liveness
            // *would* have been decided by a root set nobody could enumerate, so
            // this sweep refuses rather than reporting itself healthy; the
            // diagnostic rides in the partial report (ADR 0067 §5).
            return Err(AttachmentReclamationFailure::refused(
                crate::store::MaintenanceRefusal::UnwitnessedScope {
                    scope: "live attachment root set",
                },
                report,
            ));
        }
    };
    if fence == AttachmentGcFence::BestEffort {
        tracing::warn!(
            scanned_blob_count = report.scanned_blob_count,
            "attachment GC is running against an unfenced root authority: deletes are \
             best-effort and a same-content write landing in the query/delete window can \
             lose its bytes. Point the backend at recoverable deletion (object-store \
             versioning or a quarantine prefix) or use a root authority that implements \
             the condemnation CAS."
        );
    }
    for blob in blobs {
        if live.contains(&blob.id) {
            continue;
        }
        if within_grace(blob.last_modified_epoch_ms, now, grace_period_ms) {
            // Fresh write or in-flight intent per the (possibly stale) snapshot.
            continue;
        }
        // (a) Condemn before delete. One conditional mutation against the same
        // authority a writer records its intent in: from here on, a concurrent
        // writer for this digest either loses this CAS or revokes what it
        // created, and either way the physical delete below is never issued
        // behind a live intent. An unfenced authority answers `Unsupported` and
        // the legacy re-check path runs instead.
        let condemned = match root_set
            .condemn_attachment(&blob.id, intent_grace_cutoff)
            .await
        {
            Ok(AttachmentCondemnation::Condemned) => true,
            // A root appeared since the snapshot: not garbage.
            Ok(AttachmentCondemnation::RootPresent) => continue,
            // Skip-on-contention: a peer sweeper owns this digest.
            Ok(AttachmentCondemnation::AlreadyCondemned) => {
                report.condemn_deferred_ids.push(blob.id);
                continue;
            }
            Ok(AttachmentCondemnation::Unsupported) => {
                // A self-declared `Fenced` authority that cannot actually
                // condemn is a partial implementation. Believe the transition,
                // not the flag: this sweep's deletes ran the unfenced path, so
                // it reports itself best-effort like any other unfenced sweep.
                if fence == AttachmentGcFence::Fenced {
                    tracing::error!(
                        attachment_id = %blob.id,
                        "attachment root authority reported `Fenced` but answered \
                         `Unsupported` to condemn_attachment; this sweep's deletes are \
                         NOT fenced and are reported best-effort. Implement all five \
                         fence methods (including AttachmentManifest::begin_attachment_write) \
                         or report AttachmentGcFence::BestEffort"
                    );
                    fence = AttachmentGcFence::BestEffort;
                    report.fence = AttachmentGcFence::BestEffort;
                }
                false
            }
            // Could not reach the authority: do not delete a blob we cannot
            // fence.
            Err(_) => {
                report.failed_ids.push(blob.id);
                continue;
            }
        };
        // (b) Delete-time freshness re-check: the snapshot's freshness is stale, so
        // re-stat the blob immediately before deleting. A concurrent
        // new-intent-plus-`put` of the same content id — landed after the root
        // snapshot — refreshes the blob's modification time; spare it so a
        // newly-referenced blob is never reclaimed out from under its intent.
        let freshness = backend.head(&blob.id).await;
        // Every abandonment from here on must hand the digest back, or a
        // condemnation this sweep created would outlive its candidate and block
        // writers for good.
        match freshness {
            Ok(Some(fresh)) => {
                if within_grace(
                    fresh.last_modified_epoch_ms,
                    now_epoch_ms(),
                    grace_period_ms,
                ) {
                    release_condemnation(root_set, condemned, &blob.id).await;
                    continue;
                }
            }
            // Already gone (a concurrent delete): nothing to reclaim.
            Ok(None) => {
                release_condemnation(root_set, condemned, &blob.id).await;
                continue;
            }
            // Could not re-stat: treat as a per-blob failure rather than risk
            // deleting a blob we can no longer vouch for.
            Err(_) => {
                release_condemnation(root_set, condemned, &blob.id).await;
                report.failed_ids.push(blob.id);
                continue;
            }
        }
        if !condemned {
            // Unfenced fallback: a targeted root re-check for THIS id. The `live`
            // snapshot was taken before the per-blob loop began; a session may have
            // recorded a fresh intent for this content id since. It observes an
            // intent recorded before the delete because the facade's `put` records
            // the write-ahead intent BEFORE the backend `put`. It cannot observe one
            // recorded after it, which is the window only the fence closes.
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
            release_condemnation(root_set, condemned, &blob.id).await;
            // The refusal carries the work already done — including any
            // `deleted_while_referenced` detections — instead of discarding it.
            return Err(AttachmentReclamationFailure::refused(
                crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized,
                report,
            ));
        }
        // (c) Arm the delete: `Condemned -> Deleting`. A writer that took the
        // digest back while we were re-stating the blob has already removed the
        // condemnation, so this fails and no delete is issued at all.
        if condemned {
            match root_set.arm_attachment_delete(&blob.id).await {
                Ok(AttachmentDeleteArming::Armed) => {}
                // No release here, deliberately: `Revoked` means the
                // condemnation row is already gone (a writer took the digest
                // back), and it may since have been re-condemned by a peer
                // sweeper that now owns it. Releasing would clear *their*
                // condemnation. The contract on `arm_attachment_delete` is
                // therefore that `Revoked` implies the row no longer stands.
                Ok(AttachmentDeleteArming::Revoked) => {
                    report.condemn_deferred_ids.push(blob.id);
                    continue;
                }
                Err(_) => {
                    release_condemnation(root_set, condemned, &blob.id).await;
                    report.failed_ids.push(blob.id);
                    continue;
                }
            }
        }
        // (d) Delete.
        let deleted = backend.delete(&blob.id).await;
        match deleted {
            Ok(()) => {
                report.reclaimed_count += 1;
                // (e) Post-delete detection probe, taken while the digest is
                // still `Deleting`. Under a fenced authority this must never
                // answer `true`: no writer can record a root for an armed digest,
                // so a root here means the fence is not holding (transitions that
                // are not atomic with intent recording, or two authorities over
                // one backend). Under an unfenced authority it is the loss
                // detector for the window the legacy path cannot close. Either
                // way the bytes are already gone; a failed probe cannot un-delete
                // them.
                if let Ok(true) = root_set
                    .has_live_attachment_ref(&blob.id, intent_grace_cutoff)
                    .await
                {
                    tracing::error!(
                        attachment_id = %blob.id,
                        fence = ?fence,
                        "attachment GC deleted a blob that a live root exists for; the \
                         bytes are unrecoverable from lash. Under a fenced root authority \
                         this indicates the condemnation CAS is not atomic with intent \
                         recording; under an unfenced one it is the delete-window race \
                         the fence exists to close"
                    );
                    report.deleted_while_referenced.push(blob.id.clone());
                }
            }
            Err(_) => report.failed_ids.push(blob.id.clone()),
        }
        // (f) Hand the digest back so a writer that arrived during the delete can
        // record its intent and re-put the content.
        release_condemnation(root_set, condemned, &blob.id).await;
    }
    Ok(report)
}

/// Hand a condemned digest back to `Free`. A no-op for an unfenced sweep, which
/// never created one.
async fn release_condemnation<R>(root_set: &R, condemned: bool, id: &AttachmentId)
where
    R: AttachmentRootSet + ?Sized,
{
    if condemned {
        let _ = root_set.release_attachment_condemnation(id).await;
    }
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

/// How many times a `put` re-acquires the write fence before giving the digest
/// back to the caller as [`AttachmentStoreError::ReclamationInFlight`].
///
/// The bound exists so a condemnation abandoned by a sweeper that died
/// mid-delete surfaces as a typed, actionable error instead of spinning
/// forever; clearing such a condemnation is host policy — see
/// [`AttachmentRootSet::release_attachment_condemnation`].
const RECLAMATION_FENCE_ATTEMPTS: u32 = 64;

/// The first few re-acquires are pure yields, for the common case where the
/// sweep's delete is a local unlink already in flight.
const RECLAMATION_FENCE_YIELD_ATTEMPTS: u32 = 8;

/// Backoff floor and ceiling for the remaining re-acquires.
///
/// These delays pace a retry loop; they do not bound anyone's liveness and
/// nothing expires because of them. The protocol stays clockless in the sense
/// that matters: no state transition, and in particular no reclamation, is ever
/// authorized by elapsed time. Sleeping between CAS attempts is a politeness to
/// the store, not a lease.
///
/// The ceiling is chosen so the total wait comfortably outlasts a remote
/// object-store delete (tens to hundreds of milliseconds) rather than a
/// scheduler quantum: the previous yield-only loop could exhaust itself in
/// microseconds against a perfectly healthy sweeper.
const RECLAMATION_FENCE_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_millis(2);
const RECLAMATION_FENCE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(250);

/// Wait before the `attempt`-th re-acquire of the write fence.
async fn reclamation_fence_backoff(clock: &dyn crate::Clock, attempt: u32) {
    if attempt <= RECLAMATION_FENCE_YIELD_ATTEMPTS {
        clock.sleep(std::time::Duration::ZERO).await;
        return;
    }
    let doublings = (attempt - RECLAMATION_FENCE_YIELD_ATTEMPTS - 1).min(16);
    let delay = RECLAMATION_FENCE_BACKOFF_MIN
        .saturating_mul(1u32 << doublings)
        .min(RECLAMATION_FENCE_BACKOFF_MAX);
    clock.sleep(delay).await;
}

pub fn content_id(bytes: &[u8]) -> AttachmentId {
    // A BLAKE3 hex digest is 64 lowercase hex characters — statically within
    // every attachment-id rule, so this cannot fail.
    AttachmentId::parse(crate::stable_hash::blake3_hex("lash-attachment/v2", bytes))
        .expect("BLAKE3 hex digest is a valid attachment id")
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
    owner: Mutex<Option<AttachmentOwner>>,
    clock: Arc<dyn crate::Clock>,
}

#[derive(Clone)]
struct AttachmentOwner {
    kind: crate::AttachmentOwnerKind,
    id: String,
    recorded_intent_ids: Arc<Mutex<BTreeSet<AttachmentId>>>,
}

pub(crate) struct AttachmentOwnerBinding {
    store: Arc<SessionAttachmentStore>,
    owner: AttachmentOwner,
    previous: Option<AttachmentOwner>,
}

impl Drop for AttachmentOwnerBinding {
    fn drop(&mut self) {
        self.store.restore_owner(&self.owner, self.previous.take());
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
        let owner = AttachmentOwner {
            kind,
            id: owner_id,
            recorded_intent_ids: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let previous = self.owner.lock_recover().replace(owner.clone());
        AttachmentOwnerBinding {
            store: Arc::clone(self),
            owner,
            previous,
        }
    }

    fn restore_owner(&self, completed: &AttachmentOwner, previous: Option<AttachmentOwner>) {
        let mut owner = self.owner.lock_recover();
        if owner
            .as_ref()
            .is_some_and(|current| current.kind == completed.kind && current.id == completed.id)
        {
            *owner = previous;
        }
    }

    /// Returns the unique attachment intents recorded by the active durable
    /// turn. The runtime uses this turn-side evidence while assembling the
    /// commit budget; stores are never queried during admission.
    pub(crate) fn recorded_turn_intent_ids(&self, turn_id: &str) -> BTreeSet<AttachmentId> {
        let recorded = self.owner.lock_recover().as_ref().and_then(|owner| {
            (owner.kind == crate::AttachmentOwnerKind::Turn && owner.id == turn_id)
                .then(|| Arc::clone(&owner.recorded_intent_ids))
        });
        recorded
            .map(|ids| ids.lock_recover().clone())
            .unwrap_or_default()
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
            owner_kind: owner.as_ref().map(|owner| owner.kind),
            owner_id: owner.as_ref().map(|owner| owner.id.clone()),
        };
        // Acquire the write fence first: the intent is recorded before any bytes
        // land (the write-ahead guarantee) and, in the same mutation, the digest
        // is taken back from a sweep that condemned it. If this fails the bytes
        // never land.
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let fence = self
                .manifest
                .begin_attachment_write(intent.clone())
                .map_err(|err| {
                    AttachmentStoreError::ManifestRecordFailed(format!(
                        "failed to record attachment intent for `{attachment_id}`: {err}"
                    ))
                })?;
            match fence {
                AttachmentWriteFence::Granted => {
                    if let Some(owner) = &owner {
                        owner
                            .recorded_intent_ids
                            .lock_recover()
                            .insert(attachment_id.clone());
                    }
                    break;
                }
                // A sweep armed this digest's delete before we recorded an
                // intent. Writing bytes into an in-flight delete would lose
                // them, so back off and re-acquire: the sweep releases the
                // digest as soon as the delete lands, and the granted retry
                // re-puts the content. The delay paces the retry; it authorizes
                // nothing and expires nothing.
                AttachmentWriteFence::ReclamationInFlight => {
                    if attempts >= RECLAMATION_FENCE_ATTEMPTS {
                        return Err(AttachmentStoreError::ReclamationInFlight {
                            attachment_id,
                            attempts,
                        });
                    }
                    reclamation_fence_backoff(self.clock.as_ref(), attempts).await;
                }
            }
        }
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
    format!("lash-attachment://blake3/{attachment_id}")
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

    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, crate::StoreError> {
        AttachmentManifest::begin_attachment_write(&*self.0, intent)
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
