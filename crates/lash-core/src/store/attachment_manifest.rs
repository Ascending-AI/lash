//! Attachment write-ahead manifest: the synchronous attachment-tracking surface
//! required from every [`SessionCommitStore`](super::SessionCommitStore).
//!
//! Split from `store/mod.rs` to keep both modules under the file-size budget;
//! the public paths (`crate::store::AttachmentManifest`, `AttachmentIntent`,
//! `AttachmentManifestEntry`, and the `impl_noop_attachment_manifest!` macro)
//! are preserved by re-export.

use super::StoreError;
use crate::SessionId;

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

    /// Parses the stable snake-case owner class persisted by attachment-manifest stores.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "turn" => Some(Self::Turn),
            "process" => Some(Self::Process),
            _ => None,
        }
    }
}

#[cfg(test)]
mod attachment_owner_kind_tests {
    use super::AttachmentOwnerKind;

    #[test]
    fn attachment_owner_kind_wire_values_match_the_persisted_sql_encoding() {
        assert_eq!(AttachmentOwnerKind::Turn.as_str(), "turn");
        assert_eq!(AttachmentOwnerKind::Process.as_str(), "process");
    }

    #[test]
    fn attachment_owner_kind_wire_decoder_refuses_unknown_values() {
        assert_eq!(
            AttachmentOwnerKind::from_wire_str("turn"),
            Some(AttachmentOwnerKind::Turn)
        );
        assert_eq!(
            AttachmentOwnerKind::from_wire_str("process"),
            Some(AttachmentOwnerKind::Process)
        );
        assert_eq!(AttachmentOwnerKind::from_wire_str("unknown"), None);
    }
}

/// A pending attachment write recorded *before* the bytes hit the
/// [`AttachmentStore`](crate::AttachmentStore) backend.
///
/// The runtime calls [`AttachmentManifest::begin_attachment_write`] from the
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
    pub session_id: SessionId,
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
pub struct AttachmentWriteToken(u128);

impl AttachmentWriteToken {
    /// Mint an opaque identity for one attempt to restore condemned attachment
    /// bytes. Stores persist this token only while the backend `put` is in
    /// flight, so completion and rollback can affect only their own attempt.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }

    /// Stable lowercase hexadecimal encoding used by durable stores.
    pub fn as_hex(self) -> String {
        format!("{:032x}", self.0)
    }
}

impl Default for AttachmentWriteToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Authority returned with a granted attachment write fence.
///
/// Ordinary writes carry no rollback token. A write that temporarily owns a
/// `Condemned` or `Reclaimed` fact carries the token the manifest must complete
/// after the backend put succeeds or abort after it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentWritePermit {
    rollback_token: Option<AttachmentWriteToken>,
}

impl AttachmentWritePermit {
    /// A granted write that did not take ownership of a condemnation fact.
    pub const fn ordinary() -> Self {
        Self {
            rollback_token: None,
        }
    }

    /// A granted write that temporarily owns a condemnation fact.
    pub const fn restoring(token: AttachmentWriteToken) -> Self {
        Self {
            rollback_token: Some(token),
        }
    }

    /// The conditional rollback token, when this write owns one.
    pub const fn rollback_token(self) -> Option<AttachmentWriteToken> {
        self.rollback_token
    }
}

impl Default for AttachmentWritePermit {
    fn default() -> Self {
        Self::ordinary()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentWriteFence {
    /// The intent row exists and the digest is rooted: no sweep can condemn it
    /// until the intent is committed, forgotten, or reconciled. The writer may
    /// now `put` the bytes.
    Granted(AttachmentWritePermit),
    /// A sweep has already armed the physical delete, or another writer owns a
    /// condemned digest until its backend put settles. No intent was recorded;
    /// this writer retries until the current owner completes or aborts.
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
///                  writer: claim phase + record intent
///                 ┌──────────────────────────────────────────────┐
///                 │                                              v
///   ┌────────┐  condemn (no root, no row)                  ┌───────────┐
///   │  Free  │ ──────────────────────────────────────────> │ Condemned │
///   └────────┘ <──────────── release (sweep abandons)──────└───────────┘
///       ^                                                        │ arm
///       │                                                        v
///       │                                                  ┌───────────┐
///       └──────── release (delete fails/abandoned)─────────│ Deleting  │
///                                                          └───────────┘
///                                                                │ delete succeeds
///                                                                v
///                                                          ┌───────────┐
///                 └──── put succeeds: token-matched clear ─┤ Reclaimed │
///                        put fails: preserve absence fact   └───────────┘
/// ```
///
/// * `Free` — the ordinary state. A writer records its intent and the digest is
///   rooted; a sweeper that finds no root may condemn it.
/// * `Condemned` — a sweeper claimed the digest for deletion but has issued no
///   physical delete yet. A writer arriving here claims the phase with a unique
///   token and records its intent in one mutation, so the sweeper's later arm
///   CAS fails. Success clears the token-matched phase after bytes exist;
///   failure releases the token while preserving `Condemned`, unless the same
///   intent became committed while the token was held; that root returns the
///   digest to `Free` before the old sweep can arm.
/// * `Deleting` — the physical delete is in flight. A writer arriving here
///   cannot un-issue it, so it records nothing and retries.
/// * `Reclaimed` — the physical delete succeeded and the bytes are known absent.
///   Adoption refuses this digest. A fresh put claims the fact while recording
///   its write-ahead intent, restores the bytes, and only then clears the
///   token-matched fact. Failure preserves `Reclaimed`.
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
    /// This caller no longer owns an armable condemnation. A writer may hold
    /// the existing phase with a restoration token, or the row may be absent or
    /// in another phase. The delete is not issued and this caller must not
    /// release state it does not own.
    Revoked,
}

#[derive(Clone, Debug)]
pub struct AttachmentManifestEntry {
    pub attachment_id: crate::AttachmentId,
    pub session_id: SessionId,
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
    /// Record an intent without acquiring the writer-side GC fence.
    ///
    /// Fenced stores must refuse this path while a digest is condemned or
    /// reclaimed; callers that are about to put bytes use
    /// [`Self::begin_attachment_write`] and settle its permit instead. The
    /// method remains the primitive used by unfenced stores and direct writes
    /// to an otherwise free digest.
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
    /// * `Condemned` — claim the condemnation with a unique write token (so the
    ///   sweeper's arm CAS fails) and insert/refresh the intent in the *same*
    ///   transaction, return [`AttachmentWriteFence::Granted`] with a restoring
    ///   permit;
    /// * `Deleting` — record nothing and return
    ///   [`AttachmentWriteFence::ReclamationInFlight`];
    /// * `Reclaimed` — claim the byte-absence fact with a unique write token and
    ///   insert/refresh the intent in the *same* transaction, return
    ///   [`AttachmentWriteFence::Granted`] with a restoring permit.
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
            .map(|()| AttachmentWriteFence::Granted(AttachmentWritePermit::ordinary()))
    }

    /// Commit a granted attachment write after the backend put succeeds.
    ///
    /// A restoring permit removes its still-token-matched `Condemned` or
    /// `Reclaimed` fact only after the bytes exist. It must leave the write-ahead
    /// intent in place. An ordinary permit is a no-op. Implementations that
    /// return restoring permits from [`Self::begin_attachment_write`] must
    /// override this method in the same authority as the condemnation state.
    fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let _ = (intent, permit);
        Ok(())
    }

    /// Abort a granted attachment write after the backend put fails.
    ///
    /// A restoring permit conditionally removes only this attempt's uncommitted
    /// intent and releases its write token. `Reclaimed` is always preserved as
    /// durable byte-absence evidence. `Condemned` is preserved unless the same
    /// intent became a committed root while the token was held; that newer root
    /// supersedes the old unarmed condemnation before an older sweep can arm it.
    /// A stale token is a no-op: it must not clobber a newer successful writer or
    /// sweep. An ordinary permit is a no-op. Implementations that return
    /// restoring permits from [`Self::begin_attachment_write`] must override this
    /// method atomically.
    fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let _ = (intent, permit);
        Ok(())
    }

    /// Mark a set of attachment ids as committed (i.e. now referenced
    /// by a durable session-graph commit). Backends that store
    /// commits and manifest in the same database stamp this inside
    /// the commit transaction; the trait-level method is the
    /// out-of-band entry point for hosts that want to commit an id
    /// outside the normal turn-commit flow.
    ///
    /// Adoption acquires this session's own committed root, inserting a row
    /// when the bytes were put only by another session. Existing intent metadata
    /// and the first commit timestamp are preserved. A foreign owner's deletion
    /// cannot release the receiver's root.
    ///
    /// Acquisition shares the attachment GC fence: it revokes an unarmed
    /// condemnation, refuses an already armed physical delete, and returns
    /// [`StoreError::AttachmentBytesReclaimed`] when a completed delete proves
    /// the host's separate blob store no longer holds the digest. Normal runtime
    /// adoption and graph publication succeed or roll back in one transaction.
    /// The manifest never calls host blob code; its byte-existence knowledge is
    /// the durable `Reclaimed` transition recorded after a successful delete.
    fn commit_refs(
        &self,
        session_id: &SessionId,
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
    /// re-check and the delete. It exists so backends can answer with a single
    /// indexed lookup that stops at the first hit, rather than materializing
    /// the whole root set for one id.
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
    /// session's refs are dropped. FIG-653: committed rows needed by retained
    /// graph history cannot be forgotten; GC removes them after the final
    /// retained prefix disappears. Bytes die only after all roots disappear.
    fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), StoreError>;

    /// Every live attachment ref (intent or committed) this manifest instance
    /// can see, deduplicated. Both durable backends hold one manifest for every
    /// session the factory owns — a global table in Postgres, the factory-wide
    /// `durable-core.db` catalog in SQLite — so this spans all sessions and the
    /// factory-level [`AttachmentRootSet`](crate::AttachmentRootSet) takes the
    /// root set from a single call rather than by unioning per-session sources.
    /// Feeds mark-and-sweep GC.
    fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError>;
}

/// Mixin macro for [`SessionCommitStore`](super::SessionCommitStore) implementors
/// that have no attachment-write story (mock backends, in-memory test stores,
/// runtime-perf harnesses). Pastes no-op impls of every
/// [`AttachmentManifest`] method.
///
/// The no-op [`AttachmentManifest::list_all_refs`] answers with an empty root
/// set, which is only safe because these stores hold no attachment bytes to
/// lose. It is **not** a starting point for a real backend: an empty root set
/// from a store that does own bytes authorizes deleting all of them. The
/// root-set doctrine is on [`AttachmentRootSet`](crate::AttachmentRootSet) — a
/// backend that cannot enumerate its roots returns an error from
/// [`AttachmentRootSet::live_attachment_refs`](crate::AttachmentRootSet::live_attachment_refs)
/// rather than an empty set, and enumerated emptiness still does not authorize
/// deletion by itself.
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
                _session_id: &$crate::SessionId,
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
                _session_id: &$crate::SessionId,
                _attachment_id: &$crate::AttachmentId,
            ) -> ::std::result::Result<(), $crate::StoreError> {
                Ok(())
            }

            fn list_all_refs(
                &self,
            ) -> ::std::result::Result<Vec<$crate::AttachmentId>, $crate::StoreError> {
                Ok(Vec::new())
            }
        }
    };
}
