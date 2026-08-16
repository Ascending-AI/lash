//! Attachment write-ahead manifest: the synchronous attachment-tracking surface
//! required from every [`SessionCommitStore`](super::SessionCommitStore).
//!
//! Split from `store/mod.rs` to keep both modules under the file-size budget;
//! the public paths (`crate::store::AttachmentManifest`, `AttachmentIntent`,
//! `AttachmentManifestEntry`, and the `impl_noop_attachment_manifest!` macro)
//! are preserved by re-export.

use super::StoreError;

/// Durable owner class for an attachment intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentOwnerKind {
    Turn,
    Process,
}

impl AttachmentOwnerKind {
    /// Exposes the stable snake-case owner class that attachment-manifest implementors persist with
    /// an intent.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Process => "process",
        }
    }
}

/// A pending attachment write recorded *before* the bytes hit the
/// [`AttachmentStore`](crate::AttachmentStore) backend.
///
/// The runtime calls [`AttachmentManifest::record_intent`] from the
/// [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// wrapper before each `put`, so the manifest is a durable record that
/// "some bytes are about to land at this URI." When the turn that
/// references the attachment commits successfully via
/// [`SessionCommitStore::commit_runtime_state`](super::SessionCommitStore::commit_runtime_state),
/// the same transaction
/// stamps `committed_at_epoch_ms`. Periodic GC sweeps manifest rows
/// whose intent has aged past a host-chosen threshold without ever
/// being committed and deletes the corresponding bytes — that's how we
/// reconcile orphaned files left behind by crashes between `put` and
/// the next turn commit.
#[derive(Clone, Debug)]
pub struct AttachmentIntent {
    pub attachment_id: crate::AttachmentId,
    pub session_id: String,
    /// Canonical, stable identity for the session-owned physical object.
    /// Backends may map this identity onto their own path/key representation.
    pub canonical_uri: String,
    pub intent_at_epoch_ms: u64,
    /// Stable durable owner that can eventually commit or release this intent.
    /// Both owner fields are absent for direct host puts made outside a runtime
    /// execution scope; those rows retain fallback timer semantics.
    pub owner_kind: Option<AttachmentOwnerKind>,
    pub owner_id: Option<String>,
}

/// Outcome of the writer-side fence acquisition
/// ([`AttachmentManifest::begin_attachment_write`]).
///
/// The writer's intent row is what roots a digest against GC, so recording it
/// and observing the digest's condemnation state must be one conditional
/// mutation. See [`AttachmentCondemnation`] for the state machine both sides
/// share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentWriteFence {
    /// The intent row exists and the digest is rooted: no sweep can condemn it
    /// until the intent is committed, forgotten, or reconciled. The writer may
    /// now `put` the bytes.
    Granted,
    /// A sweep has already armed the physical delete for this digest. No intent
    /// was recorded; the bytes are about to disappear. The writer must not write
    /// bytes into a delete that is in flight — it retries, and once the sweep
    /// releases the condemnation the retry is granted and re-puts the content.
    ReclamationInFlight,
}

/// Outcome of the GC-side condemn CAS
/// ([`AttachmentRootSet::condemn_attachment`](crate::AttachmentRootSet::condemn_attachment)).
///
/// # The digest state machine
///
/// Condemnation is per-digest state in the lash-owned root authority, held in
/// the same durable store as the manifest so the writer's intent insert and the
/// sweeper's condemn insert are one conditional mutation against each other. It
/// carries no timestamps and no TTL: every transition is a CAS, and a lost CAS
/// defers work rather than waiting for anything.
///
/// ```text
///                 writer: begin_attachment_write (revoke + record intent)
///                 ┌──────────────────────────────────────┐
///                 v                                      │
///   ┌────────┐  condemn (no root, no row)          ┌───────────┐
///   │  Free  │ ──────────────────────────────────> │ Condemned │
///   └────────┘ <────────── release (sweep abandons)└───────────┘
///       ^                                                │ arm
///       │                                                v
///       │                                          ┌───────────┐
///       └────────────── release (after delete) ────│ Deleting  │
///                                                  └───────────┘
/// ```
///
/// * `Free` — the ordinary state. A writer records its intent and the digest is
///   rooted; a sweeper that finds no root may condemn it.
/// * `Condemned` — a sweeper claimed the digest for deletion but has issued no
///   physical delete yet. A writer arriving here *revokes* the condemnation and
///   records its intent in one mutation, so the sweeper's later arm CAS fails
///   and the delete is never issued.
/// * `Deleting` — the physical delete is in flight. A writer arriving here
///   cannot un-issue it, so it records nothing and retries until the sweeper
///   releases; the retry then re-puts the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentCondemnation {
    /// The digest moved `Free -> Condemned` under this sweeper's CAS.
    Condemned,
    /// A live root (committed ref or intent) exists: the digest is not garbage.
    /// The sweep skips it and never waits.
    RootPresent,
    /// Another sweeper already holds a condemnation for this digest. The sweep
    /// defers the digest to the next sweep rather than contending for it.
    AlreadyCondemned,
    /// This root authority implements no fence. The sweep falls back to its
    /// best-effort, unfenced path — see
    /// [`AttachmentGcFence`](crate::AttachmentGcFence).
    Unsupported,
}

/// Outcome of arming the physical delete for a condemned digest
/// ([`AttachmentRootSet::arm_attachment_delete`](crate::AttachmentRootSet::arm_attachment_delete)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentDeleteArming {
    /// `Condemned -> Deleting`: this sweeper owns the delete. Writers arriving
    /// from here on retry instead of writing bytes.
    Armed,
    /// The condemnation is gone — a writer revoked it to record an intent. The
    /// delete is not issued and the digest is deferred to the next sweep.
    Revoked,
}

#[derive(Clone, Debug)]
pub struct AttachmentManifestEntry {
    pub attachment_id: crate::AttachmentId,
    pub session_id: String,
    pub canonical_uri: String,
    pub intent_at_epoch_ms: u64,
    pub committed_at_epoch_ms: Option<u64>,
    pub owner_kind: Option<AttachmentOwnerKind>,
    pub owner_id: Option<String>,
}

/// The synchronous attachment-manifest surface required from every
/// [`SessionCommitStore`](super::SessionCommitStore). Used by
/// [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// to record intent rows before `put` and by GC sweeps to reconcile
/// orphans. See the [`AttachmentIntent`] doc comment for the full
/// crash-safety story.
///
/// Backends with no attachment story (in-memory tests, mock stores)
/// paste no-op impls via [`impl_noop_attachment_manifest!`](crate::impl_noop_attachment_manifest) and
/// participate transparently — `record_intent` is a no-op, the
/// scoped wrapper still works, and GC sweeps return empty.
pub trait AttachmentManifest: Send + Sync {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError>;

    /// Record the write-ahead intent *and* resolve the digest's condemnation
    /// state in one conditional mutation — the writer half of the attachment GC
    /// fence.
    ///
    /// This is what [`SessionAttachmentStore`](crate::SessionAttachmentStore)
    /// calls before every `put`. It must be a single durable transaction over
    /// the manifest row and the digest's condemnation state:
    ///
    /// * no condemnation — insert/refresh the intent, return
    ///   [`AttachmentWriteFence::Granted`];
    /// * `Condemned` — delete the condemnation row (revoking it, so the
    ///   sweeper's arm CAS fails) and insert/refresh the intent in the *same*
    ///   transaction, return [`AttachmentWriteFence::Granted`];
    /// * `Deleting` — record nothing and return
    ///   [`AttachmentWriteFence::ReclamationInFlight`].
    ///
    /// Splitting the condemnation read from the intent insert reopens exactly
    /// the window the fence closes, so a backend that cannot express both in one
    /// transaction must not override this method.
    ///
    /// The default implementation is the unfenced legacy behaviour: it records
    /// the intent and always grants. A root authority whose manifest does not
    /// override this must also report
    /// [`AttachmentGcFence::BestEffort`](crate::AttachmentGcFence), which is the
    /// default on that side too, so the two halves cannot disagree by omission.
    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, StoreError> {
        self.record_intent(intent)
            .map(|()| AttachmentWriteFence::Granted)
    }

    /// Mark a set of attachment ids as committed (i.e. now referenced
    /// by a durable session-graph commit). Backends that store
    /// commits and manifest in the same database stamp this inside
    /// the commit transaction; the trait-level method is the
    /// out-of-band entry point for hosts that want to commit an id
    /// outside the normal turn-commit flow.
    ///
    /// Commit is an *update in place* of an existing intent row, never an
    /// insert: it stamps `committed_at_epoch_ms` on rows that already exist for
    /// `(session_id, attachment_id)` and no-ops on ids with no row. This is
    /// deliberate and sound in both edge cases:
    ///
    /// * An id with no row in *this* session (e.g. an attachment carried in from
    ///   conversation history or a parent session) is already rooted by the
    ///   session that recorded its intent — this session needs no row of its own.
    /// * An id whose intent was *reconciled away* by GC (after its durable owner
    ///   was proven dead and the retention window elapsed) also no-ops:
    ///   because commit never re-inserts, it cannot resurrect a committed ref to
    ///   bytes GC may already have collected. The read side surfaces the missing
    ///   bytes as `NotFound` rather than through a dangling root.
    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), StoreError>;

    /// Return manifest entries whose intent has aged past
    /// `older_than_epoch_ms` without ever being committed. Hosts run
    /// this periodically to find orphans left by crashes between
    /// `record_intent` and the next turn commit.
    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError>;

    /// Atomically forget every *uncommitted* intent whose `intent_at_epoch_ms`
    /// is at or before `intent_grace_cutoff_epoch_ms` and whose durable owner
    /// is proven dead. Unscoped host puts have no owner proof and retain the
    /// legacy age-only fallback.
    ///
    /// This MUST be a single conditional operation, not a `list_uncommitted`
    /// read followed by per-row `forget` calls. A concurrent `record_intent` for
    /// the same `(session, attachment)` refreshes the intent's timestamp; a
    /// read-then-forget can delete that freshly-refreshed *live* intent in the
    /// window between the read and the forget, dropping the blob's only root and
    /// letting GC collect bytes a session is about to commit. Expressing the age
    /// predicate inside one delete closes that race: a refresh that bumps
    /// `intent_at` past the cutoff no longer matches, so the intent survives.
    ///
    /// The default implementation is conservative and forgets nothing. Durable
    /// backends must implement the owner-death predicate in the same conditional
    /// mutation as the age predicate; a read-then-forget implementation is not
    /// sound.
    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        let _ = intent_grace_cutoff_epoch_ms;
        Ok(())
    }

    /// Whether this manifest currently holds a *GC-live* ref for `attachment_id`
    /// — a committed ref, or an uncommitted intent that is not both aged and
    /// owner-dead. The cutoff is retention policy after terminal proof, never a
    /// liveness oracle for turn/process owners.
    ///
    /// This is the single-id counterpart to [`Self::list_all_refs`], used by the
    /// GC lever's delete-time root re-check to spare (and, post-delete, to alarm
    /// on) a blob that was re-referenced in the narrow window between the freshness
    /// re-check and the delete. Per-session-database backends answer by checking
    /// only until the first hit rather than materializing the whole root set.
    ///
    /// The default is conservative — it treats *any* manifest row for the id as
    /// live (sparing more than strictly necessary, never deleting a referenced
    /// blob). Backends override it with the precise cutoff-aware predicate.
    fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        let _ = intent_grace_cutoff_epoch_ms;
        Ok(self
            .list_all_refs()?
            .iter()
            .any(|ref_id| ref_id == attachment_id))
    }

    /// Remove one session's manifest row. Called by the session facade when a
    /// turn releases an attachment, and by `delete_session` when a whole
    /// session's refs are dropped. The bytes themselves die later via GC once
    /// no session references them.
    fn forget(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), StoreError>;

    /// Whether this manifest holds a live ref — an intent *or* a commit — for
    /// `(session_id, attachment_id)`.
    ///
    /// The session-boundary guard: the
    /// [`SessionAttachmentStore`](crate::SessionAttachmentStore) facade calls
    /// it before every backend `get`, so a turn in session A can never resolve
    /// session B's content-addressed blob by guessing its hash. Backends with
    /// no attachment story answer `true` (they impose no guard).
    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<bool, StoreError>;

    /// Every live attachment ref (intent or committed) this manifest instance
    /// can see, deduplicated. For a global manifest table (Postgres) this spans
    /// all sessions; for a per-session database (SQLite) it is that session's
    /// refs, which the factory-level
    /// [`AttachmentRootSet`](crate::AttachmentRootSet) unions across every
    /// session database at sweep time. Feeds mark-and-sweep GC.
    fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError>;
}

/// Mixin macro for [`SessionCommitStore`](super::SessionCommitStore) implementors
/// that have no attachment-write story (mock backends, in-memory test stores,
/// runtime-perf harnesses). Pastes no-op impls of every
/// [`AttachmentManifest`] method.
#[macro_export]
macro_rules! impl_noop_attachment_manifest {
    ($ty:ty) => {
        impl $crate::AttachmentManifest for $ty {
            fn record_intent(
                &self,
                _intent: $crate::AttachmentIntent,
            ) -> ::std::result::Result<(), $crate::StoreError> {
                Ok(())
            }

            fn commit_refs(
                &self,
                _session_id: &str,
                _attachment_ids: &[$crate::AttachmentId],
            ) -> ::std::result::Result<(), $crate::StoreError> {
                Ok(())
            }

            fn list_uncommitted(
                &self,
                _older_than_epoch_ms: u64,
            ) -> ::std::result::Result<Vec<$crate::AttachmentManifestEntry>, $crate::StoreError>
            {
                Ok(Vec::new())
            }

            fn forget(
                &self,
                _session_id: &str,
                _attachment_id: &$crate::AttachmentId,
            ) -> ::std::result::Result<(), $crate::StoreError> {
                Ok(())
            }

            fn holds_ref(
                &self,
                _session_id: &str,
                _attachment_id: &$crate::AttachmentId,
            ) -> ::std::result::Result<bool, $crate::StoreError> {
                Ok(true)
            }

            fn list_all_refs(
                &self,
            ) -> ::std::result::Result<Vec<$crate::AttachmentId>, $crate::StoreError> {
                Ok(Vec::new())
            }
        }
    };
}
