#[cfg(test)]
use crate::SessionId;
#[cfg(test)]
use crate::store::{
    AttachmentReferrers, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit,
};
#[cfg(test)]
use lash_sansio::sync::MutexExt;
#[cfg(test)]
use std::sync::Mutex;
mod root_enumeration;
pub use root_enumeration::{AttachmentRootPage, AttachmentRootSource, CompleteAttachmentRoots};

use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;

use lash_sansio::llm::attachment_delivery::{Delivery, DeliveryLimits, ProviderAccepts};
use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef, MediaType};

use crate::store::{
    AttachmentCondemnation, AttachmentCondemnationAdoption, AttachmentCondemnationPhase,
    AttachmentCondemnationSettlement, AttachmentDeleteArming, AttachmentDeleteStallReason,
    AttachmentSettlementOutcome, AttachmentSweepGeneration, MAX_ATTACHMENT_DELETE_ATTEMPTS,
    StoreError,
};

/// Why an attachment-store backend operation failed, as a property a caller can
/// act on without parsing the message.
///
/// The retry and operator verdicts are derived from the class, never stored
/// separately, so a class and its verdicts cannot contradict each other.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
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

/// The content claim that failed verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContentMismatchDetail {
    #[error("expected {expected} bytes, found {actual}")]
    Length { expected: u64, actual: u64 },
    #[error("content digest differs")]
    Digest,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttachmentStoreError {
    #[error("attachment `{0}` was not found")]
    NotFound(AttachmentId),
    #[error("attachment `{id}` ({media_type}) cannot be delivered in any accepted form")]
    DeliveryUnsupported {
        id: AttachmentId,
        media_type: MediaType,
    },
    #[error("attachment `{id}` does not match its reference: {detail}")]
    ContentMismatch {
        id: AttachmentId,
        detail: ContentMismatchDetail,
    },
    /// A session put exceeded the host's configured attachment byte limit.
    #[error(
        "attachment is {byte_len} bytes, exceeding the configured {max_bytes}-byte attachment limit"
    )]
    SizeLimitExceeded { byte_len: u64, max_bytes: u64 },
    #[error("attachment read reached {byte_len} bytes, exceeding the {max_bytes}-byte read limit")]
    ReadLimitExceeded { byte_len: u64, max_bytes: u64 },
    #[error("attachment delivery exceeds the {max_bytes}-byte request budget")]
    RequestBudgetExceeded { max_bytes: u64 },
    #[error("attachment store I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A referrers operation failed for one attachment. The store cause keeps
    /// its classification and structured refusal fields.
    #[error("attachment referrers {operation} for `{attachment_id}` failed: {source}")]
    ReferrersOperationFailed {
        operation: &'static str,
        attachment_id: AttachmentId,
        #[source]
        source: Box<StoreError>,
    },
    /// A blob write or its returned-id contract failed, then aborting the
    /// referrers write failed too. The source chain follows the original write
    /// failure; `abort_error` retains the separate rollback cause.
    #[error(
        "attachment write for `{attachment_id}` failed: {write_error}; referrers abort also failed: {abort_error}"
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
    /// blob backend, referrers or root-set failure is retryable when its source is
    /// transient, as is a write refused by an in-flight reclamation: the
    /// retry re-puts the bytes once the delete settles.
    /// A contract violation or a terminal backend failure retries to the same
    /// refusal. A failed rollback is retryable only if both causes are
    /// retryable. This verdict does not authorize replaying an executed tool.
    pub fn is_retryable(&self) -> bool {
        self.retention_failure().is_retryable()
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

/// A bounded page ordered by content id. `next` is the exclusive content
/// cursor for the following page; a full final page may lead to an empty page.
/// Concurrent puts/deletes are weakly consistent: ids inserted behind the
/// cursor wait for the next sweep, and deleted ids may disappear between pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentBlobPage {
    pub blobs: Vec<StoredBlobRef>,
    pub next: Option<AttachmentId>,
}
impl AttachmentBlobPage {
    /// Build a page from at most `limit` blobs, already ordered by content id.
    pub fn from_ordered(blobs: Vec<StoredBlobRef>, limit: std::num::NonZeroU32) -> Self {
        let next = if blobs.len() == limit.get() as usize {
            blobs.last().map(|blob| blob.id.clone())
        } else {
            None
        };
        Self { blobs, next }
    }
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
/// [`RuntimeAttachmentStore`] and the [`AttachmentReferrers`]; lifecycle
/// (which blobs may be deleted) lives above that in the host, via
/// [`reclaim_unreferenced_attachments`].
///
/// Conventions every backend upholds: `put` is content-idempotent (identical
/// bytes return the same ref without creating a second blob) but refreshes any
/// freshness signal the backend exposes, `delete` is idempotent, and a missing
/// blob maps to [`AttachmentStoreError::NotFound`].
///
/// Every id is a 64-character lowercase content digest, validated at construction.
/// Backends preserve that content address through bounded reads and deliveries.
#[async_trait::async_trait]
pub trait AttachmentStore: Send + Sync {
    /// Whether delivery requires materializing bytes under the host's per-blob
    /// budget. A store configured to deliver immutable URLs returns false.
    /// This is a deployment capability, not a promise about a provider's acceptance.
    fn byte_delivery_budget_applies(&self) -> bool {
        true
    }

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
    /// Enforce `max_bytes` against actual content before allocating retained
    /// bytes. Metadata may reject early but cannot establish this bound.
    async fn get(
        &self,
        id: &AttachmentId,
        max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError>;

    /// Deliver immutable content in an accepted form, under the byte and validity bounds.
    /// Delivered secrets must never appear in backend diagnostics.
    async fn deliver(
        &self,
        reference: &AttachmentRef,
        accepts: &ProviderAccepts,
        limits: &DeliveryLimits,
    ) -> Result<Delivery, AttachmentStoreError> {
        if !accepts.bytes {
            return Err(AttachmentStoreError::DeliveryUnsupported {
                id: reference.id.clone(),
                media_type: reference.media_type.clone(),
            });
        }
        let stored = self.get(&reference.id, limits.max_bytes).await?;
        validate_attachment_bytes(
            reference,
            &stored.bytes,
            stored.bytes.capacity() as u64,
            limits.max_bytes,
        )?;
        Ok(Delivery::Bytes(stored.bytes))
    }

    /// Forget only the cached delivery equal to the rejected value.
    async fn invalidate_delivery(
        &self,
        _reference: &AttachmentRef,
        _rejected: &Delivery,
    ) -> Result<(), AttachmentStoreError> {
        Ok(())
    }

    /// Idempotent: deleting an absent blob is a no-op.
    /// This is the primitive mark-and-sweep GC uses to reclaim unreferenced content;
    /// per-session lifecycle is expressed by dropping referrers refs, never by calling this
    /// directly for a live session.
    ///
    /// Namespaced-storage implementors must apply the trait-level id-shape
    /// guard before deriving any path or key from `id`.
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError>;

    /// Enumerate every blob currently held, materializing the namespace.
    /// Prefer [`Self::list_page`] for bounded listing and reclamation.
    /// Order is unspecified.
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError>;

    /// List at most `limit` blobs ordered by content id, strictly after `after`.
    /// A cursor progresses exclusively; concurrent changes are weakly consistent.
    /// This fallback collects `list` for external implementors and is NOT a
    /// bounded backend implementation. SQLite and S3 override it with bounded
    /// blob listing; root enumeration and report ids have separate memory limits.
    async fn list_page(
        &self,
        after: Option<&AttachmentId>,
        limit: std::num::NonZeroU32,
    ) -> Result<AttachmentBlobPage, AttachmentStoreError> {
        let mut blobs = self.list().await?;
        blobs.retain(|blob| after.is_none_or(|after| &blob.id > after));
        blobs.sort_by(|a, b| a.id.cmp(&b.id));
        blobs.truncate(limit.get() as usize);
        Ok(AttachmentBlobPage::from_ordered(blobs, limit))
    }

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
/// The collector visits every referrer source and exhausts its pages before
/// it mints a witness. If the implementor cannot enumerate its roots, it
/// must return an error from
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
    async fn live_attachment_refs(&self) -> Result<CompleteAttachmentRoots, StoreError> {
        root_enumeration::enumerate(self).await
    }

    /// Read one source in digest order, strictly after `after`. An unsupported
    /// source or an unfinished continuation must return an error, never an
    /// empty terminal page. The shared collector owns source and page coverage.
    async fn attachment_root_page(
        &self,
        _source: AttachmentRootSource,
        _after: Option<&AttachmentId>,
    ) -> Result<AttachmentRootPage, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "AttachmentRootSet::attachment_root_page",
        })
    }

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
    /// — it does not mutate referrer edges or pending writes. Backends answer with a single
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
    /// 1. [`AttachmentReferrers::begin_attachment_write`] — the **writer half**,
    ///    on the referrers trait, not this one. Overriding the methods below
    ///    while leaving this at its default means writers record intents without
    ///    consulting the condemnation, so a sweep deletes bytes behind a live
    ///    intent while this method reports `Fenced`. There is no fence without
    ///    it.
    /// 2. [`AttachmentReferrers::complete_attachment_write`] and
    ///    [`AttachmentReferrers::abort_attachment_write`] — fenced and matched on
    ///    the attempt identity the writer half minted.
    /// 3. [`Self::begin_attachment_sweep`] — mint a pass generation and hold
    ///    the liveness that proves the pass has not died.
    /// 4. [`Self::adopt_attachment_condemnations`] — claim a dead pass's rows.
    /// 5. [`Self::condemn_attachment`] — `Free -> Condemned`, conditional on
    ///    the root predicate, clearing every referrers row for the digest.
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
    /// The same transaction removes upload evidence. The root predicate is
    /// the one [`Self::has_live_attachment_ref`] answers: an edge or pending
    /// write is sufficient, regardless of age. Checking that predicate and
    /// inserting the condemnation must serialize with writers on the digest.
    /// A writer that wins first makes this return `RootPresent`; a writer
    /// that follows may claim the unarmed condemnation.
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
    /// evidence, because condemnation already removed its upload record.
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
    /// that no restoring writer for this digest is running. Recovery applies
    /// the same transition as abort to the pending attempt named by the
    /// condemnation's foreign key. Other referrer edges or pending attempts
    /// retire the condemnation; otherwise it remains available to a fresh
    /// writer. A stale completion from the
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
    /// Failed deletes before the row is reported stalled. The standard preset
    /// is 5 attempts, a historical value without workload measurements.
    delete_attempt_limit: std::num::NonZeroU32,
}

impl AttachmentReclamationPolicy {
    /// Choose retention and empty-root authority explicitly, with the standard
    /// retry preset of 5 failed physical deletes. The retry count is a
    /// historical value without supporting workload measurements.
    pub fn new(grace_period_ms: u64, empty_root_set: EmptyRootSetPolicy) -> Self {
        Self {
            grace_period_ms,
            empty_root_set,
            delete_attempt_limit: std::num::NonZeroU32::MIN
                .saturating_add(MAX_ATTACHMENT_DELETE_ATTEMPTS - 1),
        }
    }
    /// Override the optional standard delete-attempt limit.
    pub fn with_delete_attempt_limit(mut self, limit: std::num::NonZeroU32) -> Self {
        self.delete_attempt_limit = limit;
        self
    }
}

mod reclamation;
pub use reclamation::reclaim_unreferenced_attachments;

/// No attachment port: a runtime or fixture with nowhere to keep attachment
/// bytes. Every write is refused with a terminal backend failure, and reads
/// find nothing, as they would in a store that never accepted a write.
///
/// It keeps nothing, so it is not a persistence store: a fixture that only
/// needs *an* attachment facade takes [`RuntimeAttachmentStore::unavailable`],
/// and one that stores attachments takes its backend's port.
mod runtime_store;
#[cfg(any(test, feature = "testing"))]
pub use runtime_store::UnavailableAttachmentStore;
pub use runtime_store::{
    AttachmentByteDelivery, AttachmentExecutionBinding, AttachmentHolder, AttachmentPolicy,
    AttachmentPutReceipt, AttachmentReadPolicy, AttachmentReclamationRetryPolicy,
    AttachmentReclamationRetryPolicyError, NoopAttachmentReferrers, PersistenceReferrersAdapter,
    RuntimeAttachmentStore, STANDARD_ATTACHMENT_UPLOAD_EXPIRY_MS,
};

pub use lash_sansio::attachment::content_id;

/// Verify actual content and retained allocation before a read or delivery escapes.
pub fn validate_attachment_bytes(
    reference: &AttachmentRef,
    bytes: &[u8],
    capacity: u64,
    max_bytes: u64,
) -> Result<(), AttachmentStoreError> {
    let actual = bytes.len() as u64;
    if actual > max_bytes || capacity > max_bytes {
        return Err(AttachmentStoreError::ReadLimitExceeded {
            byte_len: actual.max(capacity),
            max_bytes,
        });
    }
    if actual != reference.byte_len {
        return Err(AttachmentStoreError::ContentMismatch {
            id: reference.id.clone(),
            detail: ContentMismatchDetail::Length {
                expected: reference.byte_len,
                actual,
            },
        });
    }
    if content_id(bytes) != reference.id {
        return Err(AttachmentStoreError::ContentMismatch {
            id: reference.id.clone(),
            detail: ContentMismatchDetail::Digest,
        });
    }
    Ok(())
}

fn now_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
#[path = "attachments/delivery.rs"]
mod delivery;
pub mod provider_files;
pub use provider_files::{
    ProviderFileCacheLimits, ProviderFileDelivery, ProviderFileUploader, UploadedProviderFile,
};

mod degradation;
pub use degradation::degrade_unmaterializable_request_attachments;

#[cfg(test)]
#[path = "attachments/referrer_failure_tests.rs"]
mod referrer_failure_tests;

#[cfg(any(test, feature = "testing"))]
#[path = "attachments/test_capability.rs"]
pub mod test_capability;
#[cfg(any(test, feature = "testing"))]
pub use test_capability::attachment_test_acceptance;

#[cfg(test)]
#[path = "attachments/paging_tests.rs"]
mod tests;
