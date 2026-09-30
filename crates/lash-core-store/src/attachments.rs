use crate::SessionId;
use lash_sansio::sync::MutexExt;
mod file_store;

pub use file_store::FileAttachmentStore;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef};

use crate::store::{
    AttachmentCondemnation, AttachmentCondemnationAdoption, AttachmentCondemnationPhase,
    AttachmentCondemnationSettlement, AttachmentDeleteArming, AttachmentDeleteStallReason,
    AttachmentManifest, AttachmentSettlementOutcome, AttachmentSweepGeneration, AttachmentWrite,
    AttachmentWriteFence, AttachmentWritePermit, MAX_ATTACHMENT_DELETE_ATTEMPTS, StoreError,
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

/// Why an attachment-store backend operation failed, as a property a caller can
/// act on without parsing the message.
///
/// The retry and operator verdicts are derived from the class, never stored
/// separately, so a class and its verdicts cannot contradict each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AttachmentStoreFailureClass {
    /// The operation may succeed if retried: a transport fault, timeout,
    /// throttling, or an unclassified backend failure.
    #[error("transient failure")]
    Transient,
    /// Credentials or authorization must be corrected before a retry can
    /// succeed; the failure is operator-actionable.
    #[error("credentials or authorization failure")]
    Credentials,
    /// The request, its configuration, or the backend's support for it cannot
    /// succeed as written. Retrying changes nothing.
    #[error("terminal failure")]
    Terminal,
}

impl AttachmentStoreFailureClass {
    /// Whether retrying the identical operation may succeed.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient)
    }

    /// Whether an operator must change credentials or authorization before the
    /// operation can succeed.
    pub const fn is_operator_actionable(self) -> bool {
        matches!(self, Self::Credentials)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttachmentStoreError {
    #[error("attachment `{0}` was not found")]
    NotFound(AttachmentId),
    /// A session put exceeded the host's configured attachment byte limit.
    #[error(
        "attachment is {byte_len} bytes, exceeding the configured {max_bytes}-byte attachment limit"
    )]
    SizeLimitExceeded { byte_len: u64, max_bytes: u64 },
    #[error("attachment store I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A manifest operation failed for one attachment. The store cause keeps
    /// its classification and structured refusal fields.
    #[error("attachment manifest {operation} for `{attachment_id}` failed: {source}")]
    ManifestOperationFailed {
        operation: &'static str,
        attachment_id: AttachmentId,
        #[source]
        source: Box<StoreError>,
    },
    /// A blob write or its returned-id contract failed, then aborting the
    /// manifest write failed too. The source chain follows the original write
    /// failure; `abort_error` retains the separate rollback cause.
    #[error(
        "attachment write for `{attachment_id}` failed: {write_error}; manifest abort also failed: {abort_error}"
    )]
    WriteRollbackFailed {
        attachment_id: AttachmentId,
        #[source]
        write_error: Box<AttachmentStoreError>,
        abort_error: Box<StoreError>,
    },
    /// The blob backend failed an operation. `operation` names the failed
    /// request, `class` is the actionable verdict, and `source` preserves the
    /// underlying cause for operators.
    #[error("attachment store backend {operation} failed ({class}): {source}")]
    Backend {
        operation: &'static str,
        class: AttachmentStoreFailureClass,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// The attachment layer observed a state its own contract forbids: a stored
    /// key that is not a valid [`AttachmentId`], an unreadable stored filename,
    /// or a backend that returned an id other than the one requested. Always a
    /// defect or foreign/corrupt data, never a transport fault.
    #[error("attachment store contract violation: {0}")]
    Contract(String),
    /// The live root set could not be enumerated, so a sweep's destructive
    /// scope is unwitnessed. `source` is the root-set (session store) failure;
    /// this is not a blob-backend failure.
    #[error("failed to enumerate live attachment refs: {source}")]
    RootSetEnumerationFailed {
        #[source]
        source: Box<StoreError>,
    },
    /// A root-set operation failed during reclamation: opening the sweep's
    /// pass, adopting a dead predecessor's condemnations, or a per-blob
    /// condemnation transition or root probe.
    #[error("attachment root set {operation} failed: {source}")]
    RootSetOperationFailed {
        operation: &'static str,
        #[source]
        source: Box<StoreError>,
    },
    #[error(
        "attachment `{attachment_id}` is being reclaimed: a sweep armed its physical delete before this write recorded an intent, and the condemnation was still held after {attempts} fence attempts. The sweep may simply be slow — a large or remote delete can outlast the retry window — so retrying the put is the normal response; a completed or failed delete settles the condemnation and the retry then re-puts the bytes. A condemnation left by a sweeper that died mid-delete is adopted and finished by the next sweep."
    )]
    ReclamationInFlight {
        attachment_id: AttachmentId,
        attempts: u32,
    },
}

impl AttachmentStoreError {
    /// Whether retrying the identical operation may succeed. A transient
    /// blob backend, manifest or root-set failure is retryable when its source is
    /// transient, as is a write refused by an in-flight reclamation: the
    /// retry re-puts the bytes once the delete settles.
    /// A contract violation or a terminal backend failure retries to the same
    /// refusal. A failed rollback is retryable only if both causes are
    /// retryable. This verdict does not authorize replaying an executed tool.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Backend { class, .. } => class.is_retryable(),
            Self::RootSetOperationFailed { source, .. }
            | Self::ManifestOperationFailed { source, .. } => source.is_transient(),
            Self::WriteRollbackFailed {
                write_error,
                abort_error,
                ..
            } => write_error.is_retryable() && abort_error.is_transient(),
            Self::ReclamationInFlight { .. } => true,
            _ => false,
        }
    }

    /// Whether an operator must correct credentials or authorization before the
    /// operation can succeed.
    pub fn is_operator_actionable(&self) -> bool {
        matches!(self, Self::Backend { class, .. } if class.is_operator_actionable())
    }

    /// The backend failure class, when this error is a backend failure.
    pub fn failure_class(&self) -> Option<AttachmentStoreFailureClass> {
        match self {
            Self::Backend { class, .. } => Some(*class),
            _ => None,
        }
    }
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

    /// Idempotent: deleting an absent blob is a no-op.
    /// This is the primitive mark-and-sweep GC uses to reclaim unreferenced content;
    /// per-session lifecycle is expressed by dropping manifest refs, never by calling this
    /// directly for a live session.
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
    /// Under a fenced root authority, `Ok(None)` retires the already armed
    /// digest's condemnation without issuing a delete, exactly as a completed
    /// delete would; `Err` releases the arm and lands in
    /// [`AttachmentReclamationReport::failed_ids`]. Under an unfenced authority
    /// both answers skip deletion, while only `Err` is reported. A backend that
    /// cannot answer should therefore return an error rather than `None`.
    ///
    /// Implementors that derive a namespaced path or key from `id` must apply
    /// the trait-level id-shape guard first.
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError>;
}

/// The deployment-wide attachment roots: every explicit referrer edge and
/// every pending write. Root enumeration never ends a referrer or uses age
/// to infer that its writer died.
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
    /// Every digest held by an edge or a pending write.
    ///
    /// A digest is live exactly while it has an edge or pending write.
    async fn live_attachment_refs(&self) -> Result<BTreeSet<AttachmentId>, StoreError>;

    /// Enumerate the factory's durable condemnation authority in digest order.
    ///
    /// This is the operator's listing of what is stuck right now (ADR 0067 §6).
    /// The result includes sweep-owned and restoring-write-owned rows in every
    /// non-free phase, with each row's failed-delete count, last error and
    /// typed stall. Restoring ownership exposes its session identity but never
    /// the opaque write token. Implementations must fail closed when a
    /// persisted phase, provenance or failure combination is unknown or
    /// inconsistent. Inspection only: it neither adopts nor mutates rows.
    async fn list_condemnations(
        &self,
    ) -> Result<Vec<crate::store::AttachmentCondemnationRecord>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::list_condemnations",
        })
    }

    /// Targeted counterpart to [`Self::live_attachment_refs`] for the GC lever's
    /// delete-time root re-check (see [`reclaim_unreferenced_attachments`]): the
    /// full root set is snapshotted once, but a candidate blob can be re-referenced
    /// in the narrow window between the freshness re-check and the delete, so the
    /// sweep re-probes just that id. Unlike the snapshot, this is a read-only probe
    /// — it must NOT reconcile (forget) aged intents. Backends answer with a single
    /// indexed query / first-hit scan rather than materializing the whole set.
    async fn has_live_attachment_ref(&self, id: &AttachmentId) -> Result<bool, StoreError>;

    /// Whether this root authority implements the condemn/intent CAS fence.
    ///
    /// The default is [`AttachmentGcFence::BestEffort`]: an authority that does
    /// not override both this and the condemnation transitions below runs the
    /// sweep's unfenced path, which cannot exclude a concurrent writer from the
    /// query/delete window. See [`reclaim_unreferenced_attachments`].
    ///
    /// # Answering `Fenced` is a ten-method claim, across two traits
    ///
    /// The fence is only real when *all* of the following are implemented
    /// against the same durable store, with each transition a single
    /// conditional mutation:
    ///
    /// 1. [`AttachmentManifest::begin_attachment_write`] — the **writer half**,
    ///    on the manifest trait, not this one. Overriding the methods below
    ///    while leaving this at its default means writers record intents without
    ///    consulting the condemnation, so a sweep deletes bytes behind a live
    ///    intent while this method reports `Fenced`. There is no fence without
    ///    it.
    /// 2. [`AttachmentManifest::complete_attachment_write`] and
    ///    [`AttachmentManifest::abort_attachment_write`] — fenced and matched on
    ///    the attempt identity the writer half minted.
    /// 3. [`Self::begin_attachment_sweep`] — mint a pass generation and hold
    ///    the liveness that proves the pass has not died.
    /// 4. [`Self::adopt_attachment_condemnations`] — claim a dead pass's rows.
    /// 5. [`Self::condemn_attachment`] — `Free -> Condemned`, conditional on
    ///    the root predicate, clearing every manifest row for the digest.
    /// 6. [`Self::arm_attachment_delete`] — `Condemned -> Deleting`,
    ///    conditional on the pass still owning the condemnation.
    /// 7. [`Self::settle_attachment_condemnation`] — retire, spare, or record
    ///    a failed delete, conditional on the pass owning the row.
    /// 8. [`Self::recover_abandoned_attachment_write`] — a host-authorized,
    ///    quiescent recovery that clears one restoring writer's claim and
    ///    intent, retiring `Condemned` only when the associated intent became
    ///    a committed root while the claim was held.
    /// 9. This method, answering [`AttachmentGcFence::Fenced`].
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

    /// Open one sweep pass: mint a generation newer than every earlier pass's
    /// and hold the liveness that proves this pass is running.
    ///
    /// The returned value owns that liveness. While it is held no other pass
    /// adopts the rows it stamps; once it is dropped — the pass ends, its task
    /// is cancelled, or its process dies — a later pass may. The proof must
    /// not be a timer: an authority whose store runs in one process keeps an
    /// in-process registry, one shared across processes holds a lock that the
    /// database releases when the holder's connection goes away.
    ///
    /// Unfenced authorities never condemn, so the default refuses.
    async fn begin_attachment_sweep(&self) -> Result<AttachmentSweepGeneration, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::begin_attachment_sweep",
        })
    }

    /// Adopt every sweep-owned condemnation an older, dead generation left
    /// (ADR 0067 §6): the first thing a fenced sweep does.
    ///
    /// Each adoption is one conditional mutation per row — stamp `generation`
    /// where the row still carries the older generation it was read with, no
    /// restoring writer holds it, its retry backoff has elapsed, and the owning
    /// pass is proven dead — so two sweepers adopting at once claim each row
    /// exactly once. A row whose pass is still live is reported in
    /// [`AttachmentCondemnationAdoption::held_by_live_pass`] and left alone; a
    /// stalled row is reported in [`AttachmentCondemnationAdoption::stalled`]
    /// until its retry is due. A restoring writer's row belongs to that writer
    /// and is not reported.
    async fn adopt_attachment_condemnations(
        &self,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnationAdoption, StoreError> {
        let _ = generation;
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::adopt_attachment_condemnations",
        })
    }

    /// `Free -> Condemned` for one digest, stamped with `generation`,
    /// conditional on there being no live root — the GC half of the fence.
    ///
    /// The same mutation clears every manifest row for the digest. A digest with
    /// no live root is one whose remaining rows are aged, owner-dead intents;
    /// leaving them behind would leave upload evidence for bytes this sweep is
    /// about to delete. Clearing them is what removes the need for a durable
    /// byte-absence tombstone: adoption is gated on positive evidence, and after
    /// condemnation there is none.
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
    /// [`AttachmentCondemnation::AlreadyCondemned`] and the digest is deferred.
    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnation, StoreError> {
        let _ = (id, generation);
        Ok(AttachmentCondemnation::Unsupported)
    }

    /// `Condemned -> Deleting` for one digest `generation` owns: the CAS that
    /// authorizes the physical backend delete.
    ///
    /// Returns [`AttachmentDeleteArming::Revoked`] when the condemnation is no
    /// longer this generation's unclaimed `Condemned` row, which covers exactly
    /// the case where a writer took the digest back. The sweep then issues no
    /// delete at all, so the physical delete is only ever issued for a digest
    /// this call armed — no SQL/blob-store atomicity required.
    ///
    /// The default is an error, not `Revoked`: it is only ever reached by an
    /// authority that implemented [`Self::condemn_attachment`] without this,
    /// which would otherwise condemn digests it can never arm — blocking
    /// writers permanently. Failing loudly makes the sweep report the digest in
    /// [`AttachmentReclamationReport::failed_ids`].
    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        let _ = (id, generation);
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::arm_attachment_delete (required alongside \
                        condemn_attachment)",
        })
    }

    /// Settle one condemnation `generation` owns.
    ///
    /// [`Deleted`](AttachmentCondemnationSettlement::Deleted) retires the
    /// `Deleting` row: the digest returns to `Free` holding no upload
    /// evidence, because condemning it already cleared every manifest row.
    /// No byte-absence fact is retained, and none is needed: adoption finds no
    /// upload evidence and refuses with
    /// [`StoreError::UnknownAttachment`](crate::StoreError::UnknownAttachment)
    /// until someone actually puts the bytes again.
    /// [`Spared`](AttachmentCondemnationSettlement::Spared) removes an
    /// unclaimed `Condemned` or `Deleting` row without a delete.
    /// [`Failed`](AttachmentCondemnationSettlement::Failed) returns `Deleting`
    /// to `Condemned` with one more failed attempt, so writers can reclaim the
    /// digest. Later sweeps retry once backoff on the store clock has elapsed,
    /// retaining any typed stall until success.
    ///
    /// Every settlement is conditional on `generation` still owning the row: a
    /// settlement that finds a restoring writer's claim, another generation,
    /// or no row answers [`AttachmentSettlementOutcome::NotOwned`] and changes
    /// nothing. Unfenced authorities never create a condemnation, so the
    /// default is a no-op.
    async fn settle_attachment_condemnation(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
        settlement: AttachmentCondemnationSettlement,
    ) -> Result<AttachmentSettlementOutcome, StoreError> {
        let _ = (id, generation, settlement);
        Ok(AttachmentSettlementOutcome::NotOwned)
    }

    /// Recover one abandoned restoring write without a timer or stale-writer race.
    ///
    /// This is an explicit host-policy lever under ADR 0014 for a *writer* the
    /// host has stopped; a sweeper's own condemnations are recovered by the next
    /// sweep, never by a host. Before calling it, the host MUST establish
    /// that no restoring writer for this digest is running. The operation clears
    /// the claim and the precisely associated unstamped, uncommitted manifest
    /// intent in one mutation; an intent already carrying upload evidence is
    /// left alone. It preserves `Condemned` unless the associated intent became
    /// a committed root while the claim was held; that newer root supersedes the
    /// old unarmed condemnation, which returns to `Free` before an older sweep
    /// can arm it. A fresh [`AttachmentManifest::begin_attachment_write`] can
    /// claim a retained phase and re-put the bytes; a stale completion from the
    /// recovered attempt fails with
    /// [`StoreError::StaleWritePermit`](crate::StoreError::StaleWritePermit) and
    /// a stale abort is a no-op. There is no TTL and no elapsed-time authority.
    ///
    /// Fenced authorities must override this method in the same durable store as
    /// the writer half. The default fails loudly so a host never mistakes an
    /// unimplemented recovery path for success.
    async fn recover_abandoned_attachment_write(
        &self,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let _ = id;
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::recover_abandoned_attachment_write",
        })
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
    /// Blobs whose per-candidate handling failed: final `HEAD`, physical
    /// delete, or condemnation-state settlement. The sweep continues past
    /// per-blob failures and reports them after attempting the remaining
    /// candidates. These failures make a completed pass incomplete.
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
    /// Digests deferred because a live peer or restoring writer holds the
    /// condemnation, a writer revoked it before arming, or a failed delete's
    /// backoff has not elapsed. A due condemnation whose pass died is adopted
    /// and finished before new work (ADR 0067 §6).
    pub condemn_deferred_ids: Vec<AttachmentId>,
    /// Condemned digests whose delete is stalled
    /// ([`AttachmentDeleteStallReason`]), waiting for backoff or a live pass,
    /// or whose retry in this sweep failed again.
    /// [`AttachmentRootSet::list_condemnations`] names each one's reason,
    /// attempt count and last error.
    pub stalled_ids: Vec<AttachmentId>,
    /// Condemnations this sweep adopted from a dead predecessor, whether it
    /// finished them or not.
    pub adopted_count: usize,
}

impl crate::store::MaintenanceReport for AttachmentReclamationReport {
    fn reclaimed_count(&self) -> usize {
        self.reclaimed_count
    }

    fn sweep(&self) -> crate::store::MaintenanceSweep {
        if !self.failed_ids.is_empty()
            || !self.condemn_deferred_ids.is_empty()
            || !self.stalled_ids.is_empty()
        {
            crate::store::MaintenanceSweep::Incomplete
        } else if self.reclaimed_count > 0 {
            crate::store::MaintenanceSweep::Swept
        } else {
            crate::store::MaintenanceSweep::NothingToDo
        }
    }
}

/// An attachment sweep that stopped before completing its scope, carrying
/// the report accumulated before that refusal or sweep-level failure
/// (ADR 0067 §4).
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
/// Per-blob final-`HEAD`, delete, and condemnation-settlement failures are collected into
/// [`AttachmentReclamationReport::failed_ids`]; the sweep attempts the remaining
/// candidates, then returns `Ok(report)` classified as
/// [`MaintenanceSweep::Incomplete`](crate::store::MaintenanceSweep::Incomplete).
/// Failed or stalled deletes are completed sweep outcomes. `Err` is reserved
/// for a refusal or a failure that prevents the sweep from completing.
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
///    digest and claims it with a write token (and the sweep's arm CAS fails).
///    Whoever loses the CAS yields; nobody waits.
/// 3. **Only an armed digest is deleted.** The physical backend delete is issued
///    exclusively for a digest [`AttachmentRootSet::arm_attachment_delete`]
///    moved to `Deleting`. A successful delete, or bytes already gone, retires
///    the condemnation row outright, and condemnation has already cleared every
///    manifest row for the digest, so the bytes are unadoptable until somebody
///    puts them again. A writer that arrives while the delete is in flight
///    records nothing and retries. A failed put preserves `Condemned` unless
///    its intent became a committed root while the claim was held, in which
///    case that root returns it to `Free`.
///    That is why no SQL/blob-store atomicity is needed: the authority's state
///    machine, not the backend, decides whether bytes may die.
/// 4. **Skip on contention.** A digest a live peer sweep or a restoring writer
///    already holds is recorded in
///    [`AttachmentReclamationReport::condemn_deferred_ids`] and left for later.
///    The sweep never blocks a writer and never waits on a peer.
///
/// # Adoption first: a crashed sweep is finished by the next one
///
/// Every pass runs under its own generation
/// ([`AttachmentRootSet::begin_attachment_sweep`]), stamped on every
/// condemnation it creates, and the authority keeps that generation's
/// liveness for as long as the pass runs. Before it condemns anything new, a
/// pass adopts every condemnation an older generation left whose pass is
/// provably dead ([`AttachmentRootSet::adopt_attachment_condemnations`]) and
/// completes it: it arms a `Condemned` row, re-stats the bytes, deletes them
/// if present, and retires the row. Bytes already gone count as success, and a
/// row already settled is a no-op. Adoption is one CAS per row, so two
/// sweepers adopting at once claim each row exactly once and delete it once.
///
/// A failed final `HEAD` or physical delete keeps the row: it returns to
/// `Condemned`, so a writer can still reclaim the digest, with one more
/// failed attempt recorded. Later sweeps retry after a delay starting at one
/// second and doubling to a fifteen-minute cap, using the store clock. A
/// refusal (credentials, authorization, a terminal or contract failure) stalls
/// the row at once, and a retryable one stalls once
/// [`MAX_ATTACHMENT_DELETE_ATTEMPTS`] deletes have failed. A stalled row is
/// retried by later sweeps with capped backoff and stays listed until success
/// in [`AttachmentReclamationReport::stalled_ids`] and, with its typed reason,
/// in [`AttachmentRootSet::list_condemnations`].
///
/// Reclamation is still driven by the host: the sweep condemns only what it
/// was about to delete anyway, and lash expires nothing on a timer. Clearing a
/// condemnation is never host policy; there is no host lever for it.
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
    let grace_period_ms = policy.grace_period_ms;
    let mut fence = root_set.fence();
    let mut report = AttachmentReclamationReport {
        fence,
        ..AttachmentReclamationReport::default()
    };
    // (0) Adoption first (ADR 0067 §6). A fenced pass opens its generation and
    // finishes every condemnation a dead predecessor left before it looks at a
    // single new candidate, so a crash between condemn and delete strands
    // neither the row nor the bytes.
    let generation = if fence == AttachmentGcFence::Fenced {
        Some(root_set.begin_attachment_sweep().await.map_err(|source| {
            AttachmentReclamationFailure::failed_before_any_work(
                AttachmentStoreError::RootSetOperationFailed {
                    operation: "begin sweep",
                    source: Box::new(source),
                },
            )
        })?)
    } else {
        None
    };
    let mut settled = HashSet::new();
    if let Some(generation) = &generation {
        let adoption = match root_set.adopt_attachment_condemnations(generation).await {
            Ok(adoption) => adoption,
            Err(source) => {
                return Err(AttachmentReclamationFailure::failed(
                    AttachmentStoreError::RootSetOperationFailed {
                        operation: "adopt condemnations",
                        source: Box::new(source),
                    },
                    report,
                ));
            }
        };
        let AttachmentCondemnationAdoption {
            adopted,
            held_by_live_pass,
            backing_off,
            stalled,
        } = adoption;
        settled.extend(held_by_live_pass.iter().cloned());
        settled.extend(backing_off.iter().cloned());
        settled.extend(stalled.iter().cloned());
        report.condemn_deferred_ids.extend(held_by_live_pass);
        report.condemn_deferred_ids.extend(backing_off);
        report.stalled_ids.extend(stalled);
        report.adopted_count = adopted.len();
        for condemnation in adopted {
            settled.insert(condemnation.digest.clone());
            complete_condemnation(
                root_set,
                backend,
                generation,
                Candidate {
                    id: &condemnation.digest,
                    phase: condemnation.phase,
                    delete_attempts: condemnation.delete_attempts,
                    stalled: condemnation.stalled,
                },
                grace_period_ms,
                &mut report,
            )
            .await;
        }
    }
    let now = now_epoch_ms();
    let live = root_set.live_attachment_refs().await;
    let blobs = match backend.list().await {
        Ok(blobs) => blobs,
        Err(error) if report.adopted_count == 0 => {
            return Err(AttachmentReclamationFailure::failed_before_any_work(error));
        }
        Err(error) => return Err(AttachmentReclamationFailure::failed(error, report)),
    };
    report.scanned_blob_count = blobs.len();
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
                    AttachmentStoreError::RootSetEnumerationFailed {
                        source: Box::new(err),
                    },
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
        if settled.contains(&blob.id) || live.contains(&blob.id) {
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
        if let Some(generation) = generation
            .as_ref()
            .filter(|_| fence == AttachmentGcFence::Fenced)
        {
            match root_set.condemn_attachment(&blob.id, generation).await {
                Ok(AttachmentCondemnation::Condemned) => {
                    if live.is_empty()
                        && policy.empty_root_set != EmptyRootSetPolicy::AuthorizeDeleteAll
                    {
                        warn_empty_root_set_refused(&report, &blob.id, policy);
                        let _ = root_set
                            .settle_attachment_condemnation(
                                &blob.id,
                                generation,
                                AttachmentCondemnationSettlement::Spared,
                            )
                            .await;
                        return Err(AttachmentReclamationFailure::refused(
                            crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized,
                            report,
                        ));
                    }
                    complete_condemnation(
                        root_set,
                        backend,
                        generation,
                        Candidate {
                            id: &blob.id,
                            phase: AttachmentCondemnationPhase::Condemned,
                            delete_attempts: 0,
                            stalled: None,
                        },
                        grace_period_ms,
                        &mut report,
                    )
                    .await;
                    continue;
                }
                // A root appeared since the snapshot: not garbage.
                Ok(AttachmentCondemnation::RootPresent) => continue,
                // Skip-on-contention: a live peer sweep or a writer holds it.
                Ok(AttachmentCondemnation::AlreadyCondemned) => {
                    report.condemn_deferred_ids.push(blob.id);
                    continue;
                }
                Ok(AttachmentCondemnation::Unsupported) => {
                    // A self-declared `Fenced` authority that cannot actually
                    // condemn is a partial implementation. Believe the
                    // transition, not the flag: this sweep's deletes run the
                    // unfenced path, so it reports itself best-effort like any
                    // other unfenced sweep.
                    tracing::error!(
                        attachment_id = %blob.id,
                        "attachment root authority reported `Fenced` but answered \
                         `Unsupported` to condemn_attachment; this sweep's deletes are \
                         NOT fenced and are reported best-effort. Implement the complete \
                         fence methods (including AttachmentManifest::begin_attachment_write) \
                         or report AttachmentGcFence::BestEffort"
                    );
                    fence = AttachmentGcFence::BestEffort;
                    report.fence = AttachmentGcFence::BestEffort;
                }
                // Could not reach the authority: do not delete a blob we cannot
                // fence.
                Err(error) => {
                    record_reclamation_failure(
                        &mut report,
                        blob.id,
                        AttachmentStoreError::RootSetOperationFailed {
                            operation: "condemn",
                            source: Box::new(error),
                        },
                    );
                    continue;
                }
            }
        }
        // (b) Unfenced path. Delete-time freshness re-check: the snapshot's
        // freshness is stale, so re-stat the blob immediately before deleting.
        // A concurrent new-intent-plus-`put` of the same content id — landed
        // after the root snapshot — refreshes the blob's modification time;
        // spare it so a newly-referenced blob is never reclaimed out from
        // under its intent.
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
            // Already gone (a lifecycle expiry or concurrent delete).
            Ok(None) => continue,
            // Could not re-stat: treat as a per-blob failure rather than risk
            // deleting a blob we can no longer vouch for.
            Err(error) => {
                record_reclamation_failure(&mut report, blob.id, error);
                continue;
            }
        }
        // A targeted root re-check for THIS id. The `live` snapshot was taken
        // before the per-blob loop began; a session may have recorded a fresh
        // intent for this content id since. It observes an intent recorded
        // before the delete because the facade's `put` records the write-ahead
        // intent BEFORE the backend `put`. It cannot observe one recorded after
        // it, which is the window only the fence closes.
        match root_set.has_live_attachment_ref(&blob.id).await {
            Ok(true) => continue,
            Ok(false) => {}
            // Could not probe the root set: do not delete a blob we can no longer
            // prove is unreferenced.
            Err(error) => {
                record_reclamation_failure(
                    &mut report,
                    blob.id,
                    AttachmentStoreError::RootSetOperationFailed {
                        operation: "probe live reference",
                        source: Box::new(error),
                    },
                );
                continue;
            }
        }
        if live.is_empty() && policy.empty_root_set != EmptyRootSetPolicy::AuthorizeDeleteAll {
            warn_empty_root_set_refused(&report, &blob.id, policy);
            // The refusal carries the work already done — including any
            // `deleted_while_referenced` detections — instead of discarding it.
            return Err(AttachmentReclamationFailure::refused(
                crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized,
                report,
            ));
        }
        match backend.delete(&blob.id).await {
            Ok(()) => {
                report.reclaimed_count += 1;
                detect_deleted_while_referenced(root_set, &blob.id, fence, &mut report).await;
            }
            Err(error) => {
                record_reclamation_failure(&mut report, blob.id, error);
            }
        }
    }
    Ok(report)
}

fn record_reclamation_failure(
    report: &mut AttachmentReclamationReport,
    id: AttachmentId,
    error: AttachmentStoreError,
) {
    tracing::warn!(attachment_id = %id, error = %error, retryable = error.is_retryable(),
        "attachment sweep item failed; the completed pass reports incomplete work");
    report.failed_ids.push(id);
}

fn warn_empty_root_set_refused(
    report: &AttachmentReclamationReport,
    id: &AttachmentId,
    policy: AttachmentReclamationPolicy,
) {
    tracing::warn!(
        live_root_count = 0,
        scanned_blob_count = report.scanned_blob_count,
        deletion_candidate_id = %id,
        grace_period_ms = policy.grace_period_ms,
        empty_root_set_policy = ?policy.empty_root_set,
        "attachment GC refused an empty live root set with a deletion-eligible blob"
    );
}

/// One condemnation a fenced pass owns and is about to complete.
struct Candidate<'a> {
    id: &'a AttachmentId,
    phase: AttachmentCondemnationPhase,
    /// Failed deletes recorded before this pass.
    delete_attempts: u32,
    stalled: Option<AttachmentDeleteStallReason>,
}

/// Arm (when still `Condemned`), re-stat, delete, and settle one condemnation
/// `generation` owns. Every outcome settles the row: bytes gone retire it,
/// refreshed bytes spare it, and a failure keeps it for the next sweep or
/// stalls it — the row is never dropped while the bytes may remain.
async fn complete_condemnation<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    generation: &AttachmentSweepGeneration,
    candidate: Candidate<'_>,
    grace_period_ms: u64,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    let id = candidate.id;
    if candidate.phase == AttachmentCondemnationPhase::Condemned {
        // Arm before the final HEAD. Once `Deleting` is recorded, a writer
        // cannot revoke the condemnation between observing absent bytes and
        // retiring the condemnation row.
        match root_set.arm_attachment_delete(id, generation).await {
            Ok(AttachmentDeleteArming::Armed) => {}
            // This pass no longer owns the transition: a writer took the
            // digest back. It must not settle a writer's claim.
            Ok(AttachmentDeleteArming::Revoked) => {
                report.condemn_deferred_ids.push(id.clone());
                return;
            }
            // The row stays `Condemned` under this generation; the next sweep
            // adopts it.
            Err(error) => {
                record_reclamation_failure(
                    report,
                    id.clone(),
                    AttachmentStoreError::RootSetOperationFailed {
                        operation: "arm delete",
                        source: Box::new(error),
                    },
                );
                return;
            }
        }
    }
    // Delete-time freshness re-check. Under the fence this is a cheap
    // pre-filter: bytes refreshed within the window are spared.
    match backend.head(id).await {
        Ok(Some(fresh))
            if within_grace(
                fresh.last_modified_epoch_ms,
                now_epoch_ms(),
                grace_period_ms,
            ) =>
        {
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Spared,
                report,
            )
            .await;
            return;
        }
        Ok(Some(_)) => {}
        // Already gone (a lifecycle expiry, a concurrent delete, or a crashed
        // predecessor's delete that landed): exactly as a completed delete.
        Ok(None) => {
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Deleted,
                report,
            )
            .await;
            return;
        }
        Err(error) => {
            record_failed_delete(root_set, &candidate, generation, error, report).await;
            return;
        }
    }
    match backend.delete(id).await {
        Ok(()) => {
            report.reclaimed_count += 1;
            detect_deleted_while_referenced(root_set, id, AttachmentGcFence::Fenced, report).await;
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Deleted,
                report,
            )
            .await;
        }
        Err(error) => {
            record_failed_delete(root_set, &candidate, generation, error, report).await;
        }
    }
}

/// Keep a condemnation whose final `HEAD` or delete failed: it returns to
/// `Condemned` with one more attempt, and stalls when retrying cannot help or
/// the attempts reach the bound.
async fn record_failed_delete<R>(
    root_set: &R,
    candidate: &Candidate<'_>,
    generation: &AttachmentSweepGeneration,
    error: AttachmentStoreError,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    let attempts = candidate.delete_attempts.saturating_add(1);
    let stall = candidate.stalled.or_else(|| {
        if !error.is_retryable() {
            Some(AttachmentDeleteStallReason::Refused)
        } else if attempts >= MAX_ATTACHMENT_DELETE_ATTEMPTS {
            Some(AttachmentDeleteStallReason::AttemptsExhausted)
        } else {
            None
        }
    });
    let message = error.to_string();
    record_reclamation_failure(report, candidate.id.clone(), error);
    match root_set
        .settle_attachment_condemnation(
            candidate.id,
            generation,
            AttachmentCondemnationSettlement::Failed {
                stall,
                error: message.clone(),
            },
        )
        .await
    {
        Ok(AttachmentSettlementOutcome::Applied) => {
            if let Some(reason) = stall {
                tracing::error!(
                    attachment_id = %candidate.id,
                    attempts,
                    reason = reason.as_str(),
                    error = %message,
                    "attachment GC stalled a condemned digest whose delete keeps failing; \
                     later sweeps retry it with capped backoff and its bytes remain"
                );
                report.stalled_ids.push(candidate.id.clone());
            }
        }
        // Unreachable while this pass holds its generation; the row is
        // whoever owns it now.
        Ok(AttachmentSettlementOutcome::NotOwned) => {}
        // The row stays `Deleting` under this generation; the next sweep
        // adopts it and re-stats the bytes.
        Err(_) => {}
    }
}

/// Settle one owned condemnation, reporting a failed settlement. A row left
/// unsettled stays with this generation and the next sweep adopts it.
async fn settle<R>(
    root_set: &R,
    id: &AttachmentId,
    generation: &AttachmentSweepGeneration,
    settlement: AttachmentCondemnationSettlement,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    if let Err(error) = root_set
        .settle_attachment_condemnation(id, generation, settlement)
        .await
    {
        record_reclamation_failure(
            report,
            id.clone(),
            AttachmentStoreError::RootSetOperationFailed {
                operation: "settle condemnation",
                source: Box::new(error),
            },
        );
    }
}

/// Post-delete detection probe, taken while the digest is still `Deleting`
/// under a fenced authority. There it must never answer `true`: no writer can
/// record a root for an armed digest, so a root here means the fence is not
/// holding (transitions that are not atomic with intent recording, or two
/// authorities over one backend). Under an unfenced authority it is the loss
/// detector for the window the legacy path cannot close. Either way the bytes
/// are already gone; a failed probe cannot un-delete them.
async fn detect_deleted_while_referenced<R>(
    root_set: &R,
    id: &AttachmentId,
    fence: AttachmentGcFence,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    if let Ok(true) = root_set.has_live_attachment_ref(id).await {
        tracing::error!(
            attachment_id = %id,
            fence = ?fence,
            "attachment GC deleted a blob that a live root exists for; the \
             bytes are unrecoverable from lash. Under a fenced root authority \
             this indicates the condemnation CAS is not atomic with intent \
             recording; under an unfenced one it is the delete-window race \
             the fence exists to close"
        );
        report.deleted_while_referenced.push(id.clone());
    }
}

/// A backend that cannot report a modification time (`None`) is treated as past the window,
/// matching [`StoredBlobRef`].
fn within_grace(last_modified_epoch_ms: Option<u64>, now: u64, grace_period_ms: u64) -> bool {
    last_modified_epoch_ms.is_some_and(|modified| now.saturating_sub(modified) < grace_period_ms)
}

/// No attachment port: a runtime or fixture with nowhere to keep attachment
/// bytes. Every write is refused with a terminal backend failure, and reads
/// find nothing, as they would in a store that never accepted a write.
///
/// It keeps nothing, so it is not a persistence store: a fixture that only
/// needs *an* attachment facade takes [`SessionAttachmentStore::unavailable`],
/// and one that stores attachments takes its backend's port.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
pub struct UnavailableAttachmentStore;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl AttachmentStore for UnavailableAttachmentStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        Err(AttachmentStoreError::Backend {
            operation: "put",
            class: AttachmentStoreFailureClass::Terminal,
            source: "this context has no attachment port".into(),
        })
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(Vec::new())
    }

    async fn head(
        &self,
        _id: &AttachmentId,
    ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Ok(None)
    }
}

/// How many times a `put` re-acquires the write fence before giving the digest
/// back to the caller as [`AttachmentStoreError::ReclamationInFlight`].
///
/// The bound exists so a condemnation abandoned by a sweeper that died
/// mid-delete surfaces as a typed, retryable error instead of spinning
/// forever; the next sweep adopts and finishes that condemnation.
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

#[expect(
    clippy::expect_used,
    reason = "a BLAKE3 hex digest is 64 lowercase hex characters, which satisfies every attachment-id rule"
)]
pub fn content_id(bytes: &[u8]) -> AttachmentId {
    // A BLAKE3 hex digest is 64 lowercase hex characters — statically within
    // every attachment-id rule, so this cannot fail.
    AttachmentId::parse(crate::stable_hash::blake3_hex("lash-attachment/v2", bytes))
        .expect("BLAKE3 hex digest is a valid attachment id")
}

/// The default lifetime of an unbound session upload's staging referrer.
pub const DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS: u64 = 86_400_000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentHolder {
    Ephemeral,
    Runtime(crate::runtime_owner::RuntimeOwner),
}
/// Attachment bytes held by a session execution, a process record, or one
/// session upload. Pending writes precede byte publication; completion records
/// upload evidence. Reads resolve content addresses; deletion releases only
/// the holder's lasting referrer edge. Reclamation alone deletes bytes.
pub struct SessionAttachmentStore {
    backend: Arc<dyn AttachmentStore>,
    manifest: Arc<dyn AttachmentManifest>,
    holder: AttachmentHolder,
    max_attachment_bytes: Option<u64>,
    upload_expiry_ms: u64,
    execution: Mutex<Option<BoundAttachmentExecution>>,
    clock: Arc<dyn crate::Clock>,
}
#[derive(Clone)]
struct BoundAttachmentExecution {
    journal: lash_sansio::EffectJournalIdentity,
    recorded_puts: Arc<Mutex<BTreeSet<AttachmentId>>>,
}
pub struct AttachmentExecutionBinding {
    store: Arc<SessionAttachmentStore>,
    execution: BoundAttachmentExecution,
    previous: Option<BoundAttachmentExecution>,
}
impl Drop for AttachmentExecutionBinding {
    fn drop(&mut self) {
        let mut current = self.store.execution.lock_recover();
        if current
            .as_ref()
            .is_some_and(|bound| Arc::ptr_eq(&bound.recorded_puts, &self.execution.recorded_puts))
        {
            *current = self.previous.take();
        }
    }
}
impl SessionAttachmentStore {
    pub fn new(
        backend: Arc<dyn AttachmentStore>,
        manifest: Arc<dyn AttachmentManifest>,
        owner: crate::runtime_owner::RuntimeOwner,
    ) -> Self {
        Self::new_with_clock(backend, manifest, owner, Arc::new(crate::SystemClock))
    }
    pub fn new_with_clock(
        backend: Arc<dyn AttachmentStore>,
        manifest: Arc<dyn AttachmentManifest>,
        owner: crate::runtime_owner::RuntimeOwner,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            backend,
            manifest,
            holder: AttachmentHolder::Runtime(owner),
            max_attachment_bytes: None,
            upload_expiry_ms: DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS,
            execution: Mutex::new(None),
            clock,
        }
    }
    pub fn ephemeral(backend: Arc<dyn AttachmentStore>) -> Self {
        Self {
            backend,
            manifest: Arc::new(NoopAttachmentManifest),
            holder: AttachmentHolder::Ephemeral,
            max_attachment_bytes: None,
            upload_expiry_ms: DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS,
            execution: Mutex::new(None),
            clock: Arc::new(crate::SystemClock),
        }
    }
    #[cfg(any(test, feature = "testing"))]
    pub fn unavailable() -> Self {
        Self::ephemeral(Arc::new(UnavailableAttachmentStore))
    }
    pub fn backend(&self) -> &Arc<dyn AttachmentStore> {
        &self.backend
    }
    pub fn manifest(&self) -> &Arc<dyn AttachmentManifest> {
        &self.manifest
    }
    pub fn holder(&self) -> &AttachmentHolder {
        &self.holder
    }
    pub fn with_max_attachment_bytes(mut self, max_attachment_bytes: Option<u64>) -> Self {
        self.max_attachment_bytes = max_attachment_bytes;
        self
    }
    pub fn max_attachment_bytes(&self) -> Option<u64> {
        self.max_attachment_bytes
    }
    pub fn with_upload_expiry_ms(mut self, upload_expiry_ms: u64) -> Self {
        self.upload_expiry_ms = upload_expiry_ms;
        self
    }
    pub fn upload_expiry_ms(&self) -> u64 {
        self.upload_expiry_ms
    }
    pub fn reconfigured_max_attachment_bytes(&self, max_attachment_bytes: Option<u64>) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            manifest: Arc::clone(&self.manifest),
            holder: self.holder.clone(),
            max_attachment_bytes,
            upload_expiry_ms: self.upload_expiry_ms,
            execution: Mutex::new(self.execution.lock_recover().clone()),
            clock: Arc::clone(&self.clock),
        }
    }
    pub fn persistence(&self) -> AttachmentStorePersistence {
        self.backend.persistence()
    }
    pub fn bind_execution_scoped(
        self: &Arc<Self>,
        journal: lash_sansio::EffectJournalIdentity,
    ) -> Result<AttachmentExecutionBinding, AttachmentStoreError> {
        if !matches!(
            self.holder,
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(_))
        ) {
            return Err(AttachmentStoreError::Contract(
                "only a session runtime can bind an attachment execution".into(),
            ));
        }
        let execution = BoundAttachmentExecution {
            journal,
            recorded_puts: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let previous = self.execution.lock_recover().replace(execution.clone());
        Ok(AttachmentExecutionBinding {
            store: Arc::clone(self),
            execution,
            previous,
        })
    }
    pub fn recorded_execution_puts(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> BTreeSet<AttachmentId> {
        let recorded = self
            .execution
            .lock_recover()
            .as_ref()
            .filter(|bound| &bound.journal == journal)
            .map(|bound| Arc::clone(&bound.recorded_puts));
        recorded
            .map(|puts| puts.lock_recover().clone())
            .unwrap_or_default()
    }
    pub async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if let Some(max_bytes) = self.max_attachment_bytes
            && byte_len > max_bytes
        {
            return Err(AttachmentStoreError::SizeLimitExceeded {
                byte_len,
                max_bytes,
            });
        }
        let attachment_id = content_id(&bytes);
        let execution = self.execution.lock_recover().clone();
        let claim = match &self.holder {
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Process(id)) => {
                crate::artifact_referrer::ReferrerClaim::unguarded(
                    crate::artifact_referrer::ArtifactReferrer::ProcessRecord(id.clone()),
                )
            }
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(id)) => {
                match &execution {
                    Some(bound) => crate::artifact_referrer::ReferrerClaim::guarded(
                        crate::artifact_referrer::ArtifactReferrer::Execution(
                            bound.journal.clone(),
                        ),
                        crate::artifact_referrer::ArtifactCleanupPlan::AwaitJournal,
                    ),
                    None => crate::artifact_referrer::ReferrerClaim::guarded(
                        crate::artifact_referrer::ArtifactReferrer::Upload(
                            crate::artifact_referrer::UploadReferrerId::mint(id.clone()),
                        ),
                        crate::artifact_referrer::ArtifactCleanupPlan::AwaitUploadExpiry {
                            expires_at_ms: self
                                .clock
                                .timestamp_ms()
                                .saturating_add(self.upload_expiry_ms),
                        },
                    ),
                }
            }
            AttachmentHolder::Ephemeral => crate::artifact_referrer::ReferrerClaim::unguarded(
                crate::artifact_referrer::ArtifactReferrer::Session(SessionId::from("ephemeral")),
            ),
        }
        .map_err(|error| AttachmentStoreError::Contract(error.to_string()))?;
        let write = AttachmentWrite {
            attachment_id: attachment_id.clone(),
            claim,
        };
        // Acquire the write fence first: the intent is recorded before any bytes
        // land (the write-ahead guarantee) and, in the same mutation, the digest
        // is taken back from a sweep that condemned it. If this fails the bytes
        // never land.
        let mut attempts: u32 = 0;
        let reference = loop {
            attempts += 1;
            let fence = self
                .manifest
                .begin_attachment_write(&write)
                .await
                .map_err(|source| AttachmentStoreError::ManifestOperationFailed {
                    operation: "begin_attachment_write",
                    attachment_id: attachment_id.clone(),
                    source: Box::new(source),
                })?;
            let permit = match fence {
                AttachmentWriteFence::Granted(permit) => permit,
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
                    continue;
                }
            };
            // Cloned per attempt because a retry has to re-put: a sweep that
            // condemned the digest in the window below deletes both this row and
            // the bytes, so the recovering attempt is a full re-put, not a bare
            // re-stamp.
            let reference = match self.backend.put(bytes.clone(), meta.clone()).await {
                Ok(reference) => reference,
                Err(backend_error) => {
                    if let Err(rollback_error) =
                        self.manifest.abort_attachment_write(&write, permit).await
                    {
                        return Err(AttachmentStoreError::WriteRollbackFailed {
                            attachment_id,
                            write_error: Box::new(backend_error),
                            abort_error: Box::new(rollback_error),
                        });
                    }
                    return Err(backend_error);
                }
            };
            if reference.id != attachment_id {
                let backend_error = AttachmentStoreError::Contract(format!(
                    "attachment store returned id `{}` after manifest intent for `{attachment_id}`",
                    reference.id
                ));
                if let Err(rollback_error) =
                    self.manifest.abort_attachment_write(&write, permit).await
                {
                    return Err(AttachmentStoreError::WriteRollbackFailed {
                        attachment_id,
                        write_error: Box::new(backend_error),
                        abort_error: Box::new(rollback_error),
                    });
                }
                return Err(backend_error);
            }
            match self
                .manifest
                .complete_attachment_write(&write, permit)
                .await
            {
                Ok(()) => break reference,
                // The manifest has no TTL and no elapsed-time authority, so an
                // unstamped intent that is already past the sweep's grace cutoff
                // reads as an abandoned attempt: a sweep that condemns this
                // digest between the grant and the stamp deletes the row this
                // permit names, and the stamp then certifies nothing. That is
                // the documented recovery point for the writer, not a failure —
                // re-acquire the fence (which claims the condemnation and stops
                // the delete from arming) and re-put. The bytes are only ever
                // written behind a granted permit, so nothing lands inside an
                // armed delete.
                Err(StoreError::StaleWritePermit { .. })
                    if attempts < RECLAMATION_FENCE_ATTEMPTS =>
                {
                    reclamation_fence_backoff(self.clock.as_ref(), attempts).await;
                    continue;
                }
                Err(source) => {
                    return Err(AttachmentStoreError::ManifestOperationFailed {
                        operation: "complete_attachment_write",
                        attachment_id,
                        source: Box::new(source),
                    });
                }
            }
        };
        if let Some(bound) = &execution {
            bound.recorded_puts.lock_recover().insert(attachment_id);
        }
        Ok(reference)
    }

    pub async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.backend.get(id).await
    }
    pub async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        let referrer = match &self.holder {
            AttachmentHolder::Ephemeral => return Ok(()),
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(id)) => {
                crate::artifact_referrer::ArtifactReferrer::Session(id.clone())
            }
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Process(id)) => {
                crate::artifact_referrer::ArtifactReferrer::ProcessRecord(id.clone())
            }
        };
        self.manifest
            .forget_attachment_ref(&referrer, id)
            .await
            .map_err(|source| AttachmentStoreError::ManifestOperationFailed {
                operation: "forget_attachment_ref",
                attachment_id: id.clone(),
                source: Box::new(source),
            })
    }
}
pub struct NoopAttachmentManifest;
crate::impl_noop_attachment_manifest!(NoopAttachmentManifest);
fn now_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
/// The attachment port of a runtime store.
pub struct PersistenceManifestAdapter(pub Arc<dyn crate::RuntimeStore>);
#[async_trait::async_trait]
impl AttachmentManifest for PersistenceManifestAdapter {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, StoreError> {
        self.0.begin_attachment_write(write).await
    }
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.0.complete_attachment_write(write, permit).await
    }
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.0.abort_attachment_write(write, permit).await
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &crate::artifact_referrer::ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        self.0.acquire_attachment_refs(claim, ids).await
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        self.0.forget_attachment_ref(referrer, id).await
    }
    async fn end_attachment_referrer(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
    ) -> Result<(), StoreError> {
        self.0.end_attachment_referrer(referrer).await
    }
    async fn session_referrer_state(
        &self,
        session: &SessionId,
    ) -> Result<crate::store::SessionReferrerState, StoreError> {
        self.0.session_referrer_state(session).await
    }
    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<crate::artifact_referrer::ArtifactReferrer>, StoreError> {
        self.0.attachment_referrers(id).await
    }
}

pub async fn resolve_llm_request_attachments(
    mut request: crate::llm::types::LlmRequest,
    store: &SessionAttachmentStore,
) -> Result<crate::llm::types::LlmRequest, AttachmentStoreError> {
    for attachment in request
        .attachments()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>()
    {
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

pub fn attachment_materialization_notice(
    snapshot: &crate::provider::AttachmentCapabilitySnapshot,
    source: &crate::AttachmentSource,
) -> Option<crate::AttachmentMaterializationNotice> {
    crate::llm::transport::known_attachment_acceptors(snapshot, source)
        .is_empty()
        .then(|| crate::AttachmentMaterializationNotice::no_provider_accepts(source))
}

/// Replace attachments accepted by no provider with deterministic text blocks.
///
/// The fast path neither clones nor mutates the request, keeping accepted
/// attachment envelopes byte-for-byte identical. On the degradation path the
/// effect outcome is journaled under the turn-effect invocation key, so its
/// recorded response wins on replay instead of dispatch running again.
pub fn degrade_unmaterializable_request_attachments(
    request: &mut Arc<crate::llm::types::LlmRequest>,
) -> Vec<crate::AttachmentMaterializationNotice> {
    let notices = request
        .attachments()
        .into_iter()
        .filter_map(|source| {
            attachment_materialization_notice(
                &request.model_capability.attachment_acceptance,
                source,
            )
        })
        .collect::<Vec<_>>();
    if notices.is_empty() {
        return notices;
    }
    let request = Arc::make_mut(request);
    let snapshot = &request.model_capability.attachment_acceptance;
    for message in &mut request.messages {
        use crate::llm::types::LlmContentBlock;
        if !message
            .blocks
            .iter()
            .flat_map(LlmContentBlock::attachment_sources)
            .any(|source| attachment_materialization_notice(snapshot, source).is_some())
        {
            continue;
        }
        // A placeholder already in the message's text (tool execution
        // appends the same notice to the result it degrades) is not repeated.
        let existing_placeholders = message
            .blocks
            .iter()
            .flat_map(|block| match block {
                LlmContentBlock::Text { text, .. } => vec![text.to_string()],
                LlmContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|part| match part {
                        lash_sansio::ModelToolReturnPart::Text { text } => Some(text.clone()),
                        lash_sansio::ModelToolReturnPart::Attachment(_) => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<HashSet<_>>();
        let degrade = |source: &crate::AttachmentSource| {
            attachment_materialization_notice(snapshot, source)
                .map(|notice| notice.model_placeholder())
        };
        Arc::make_mut(&mut message.blocks).retain_mut(|block| match block {
            LlmContentBlock::Attachment { source } => {
                let Some(placeholder) = degrade(source) else {
                    return true;
                };
                if existing_placeholders.contains(&placeholder) {
                    return false;
                }
                *block = LlmContentBlock::Text {
                    text: placeholder.into(),
                    response_meta: None,
                    cache_breakpoint: false,
                };
                true
            }
            // Inside a tool result the placeholder takes the attachment's
            // place, so the result stays one block in its original order.
            LlmContentBlock::ToolResult { content, .. } => {
                content.retain_mut(|part| {
                    let Some(placeholder) = part.attachment().and_then(degrade) else {
                        return true;
                    };
                    if existing_placeholders.contains(&placeholder) {
                        return false;
                    }
                    *part = lash_sansio::ModelToolReturnPart::text(placeholder);
                    true
                });
                true
            }
            _ => true,
        });
    }
    let retained = request
        .attachments()
        .into_iter()
        .filter_map(|source| source.stored_ref().map(|r| r.id.clone()))
        .collect::<HashSet<_>>();
    request
        .resolved_stored
        .retain(|id, _| retained.contains(id));
    notices
}

#[cfg(test)]
#[path = "attachments/fail_closed_tests.rs"]
mod fail_closed_tests;

#[cfg(test)]
#[path = "attachments/manifest_failure_tests.rs"]
mod manifest_failure_tests;

#[cfg(any(test, feature = "testing"))]
#[path = "attachments/test_capability.rs"]
pub mod test_capability;
#[cfg(any(test, feature = "testing"))]
pub use test_capability::attachment_test_capability;
