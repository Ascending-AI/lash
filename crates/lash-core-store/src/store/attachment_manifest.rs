//! Attachment write-ahead manifest: the async attachment-tracking surface
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Process => "process",
        }
    }

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
    /// `None` for direct host puts made outside a runtime execution scope;
    /// those rows retain fallback timer semantics.
    pub owner: Option<AttachmentOwner>,
}

/// The durable owner that can eventually commit or release an attachment
/// intent.
///
/// Owner identity is one value, not a bag of independently nullable columns.
/// The two shapes the durable surfaces accept are the two variants here, so
/// the pairing rules the SQL `CHECK` constraints enforce — an owner kind
/// without an id, an id without a kind — are unrepresentable rather than
/// validated. A process owner is its minted process id, which is never reused
/// (ADR 0107), so attachments can never bind to a later process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentOwner {
    /// A durable turn, identified by its operation storage key.
    Turn { id: String },
    /// One durable process.
    Process { process_id: crate::ProcessId },
}

impl AttachmentOwner {
    /// The persisted owner-kind discriminant for this owner.
    pub const fn kind(&self) -> AttachmentOwnerKind {
        match self {
            Self::Turn { .. } => AttachmentOwnerKind::Turn,
            Self::Process { .. } => AttachmentOwnerKind::Process,
        }
    }

    /// The persisted owner id: a turn's operation storage key, or a process
    /// id.
    pub fn id(&self) -> &str {
        match self {
            Self::Turn { id } => id,
            Self::Process { process_id } => process_id.as_str(),
        }
    }
}

/// Strictly decode the two durable attachment-owner columns into one owner.
///
/// Every column combination the [`AttachmentOwner`] variants cannot express —
/// including a process owner whose id is not a minted process id — is corrupt
/// stored data.
pub fn decode_attachment_owner(
    owner_kind: Option<&str>,
    owner_id: Option<String>,
) -> Result<Option<AttachmentOwner>, StoreError> {
    let corrupt = |message: String| StoreError::StoredDataCorrupt {
        record_kind: "AttachmentManifest owner",
        message,
    };
    match (owner_kind, owner_id) {
        (None, None) => Ok(None),
        (Some("turn"), Some(id)) => Ok(Some(AttachmentOwner::Turn { id })),
        (Some("process"), Some(id)) => crate::ProcessId::parse(&id)
            .map(|process_id| Some(AttachmentOwner::Process { process_id }))
            .map_err(|error| corrupt(error.to_string())),
        (Some(unknown), _) if AttachmentOwnerKind::from_wire_str(unknown).is_none() => {
            Err(StoreError::StoredDataCorrupt {
                record_kind: "AttachmentManifest owner kind",
                message: format!("unknown attachment owner kind `{unknown}`"),
            })
        }
        (kind, id) => Err(corrupt(format!(
            "inconsistent attachment owner fields: kind {kind:?}, id present {}",
            id.is_some()
        ))),
    }
}

/// Identity of one attempt to write an attachment's bytes, minted by
/// [`AttachmentManifest::begin_attachment_write`].
///
/// The id is persisted on the manifest row for the duration of the attempt and is carried back
/// in the attempt's [`AttachmentWritePermit`].
/// While the attempt holds a `Condemned` digest it is also the claim token on the condemnation
/// row, so the sweeper's arm CAS fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentWriteToken(u128);

impl AttachmentWriteToken {
    /// Mint an opaque identity for one attachment write attempt.
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
/// Every granted write carries the attempt identity its manifest row was
/// stamped with. There is no tokenless permit: an attempt that cannot name
/// itself cannot be fenced against a concurrent attempt for the same digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentWritePermit {
    write_id: AttachmentWriteToken,
}

impl AttachmentWritePermit {
    /// A granted write identified by `write_id`.
    pub const fn new(write_id: AttachmentWriteToken) -> Self {
        Self { write_id }
    }

    /// The attempt identity this permit certifies.
    pub const fn write_id(self) -> AttachmentWriteToken {
        self.write_id
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
///             writer: claim phase + record a fresh attempt
///            ┌───────────────────────────────────────────────┐
///            │                                               v
///   ┌────────┴─┐  condemn: no root, and every manifest  ┌───────────┐
///   │   Free   │ ───── row for the digest is deleted ──> │ Condemned │
///   └──────────┘ <──── release (sweep abandons) ─────────└───────────┘
///       ^   ^                                                 │ arm
///       │   │                                                 v
///       │   │                                           ┌───────────┐
///       │   └──── release (delete failed/abandoned) ────│ Deleting  │
///       │                                               └───────────┘
///       │                                                     │
///       └──── delete succeeded: the condemnation row is ───────┘
///             retired, and no manifest row survives to
///             make the digest adoptable again
/// ```
///
/// * `Free` — the ordinary state. A writer records its intent and the digest is
///   rooted; a sweeper that finds no root may condemn it.
/// * `Condemned` — a sweeper claimed the digest for deletion but has issued no
///   physical delete yet. A writer arriving here claims the phase with its
///   attempt identity and records its intent in one mutation, so the sweeper's
///   later arm CAS fails. Success clears the claimed phase after bytes exist;
///   failure releases the claim while preserving `Condemned`, unless the same
///   intent became committed while the claim was held; that root returns the
///   digest to `Free` before the old sweep can arm.
/// * `Deleting` — the physical delete is in flight. A writer arriving here
///   cannot un-issue it, so it records nothing and retries.
///
/// There is no terminal post-delete phase. Condemnation deletes every manifest
/// row for the digest, and a completed delete deletes the condemnation row, so
/// the digest returns to `Free` holding no upload evidence. Adoption is gated
/// on that positive evidence
/// ([`AttachmentManifestEntry::written_at_epoch_ms`]), not on a negative
/// tombstone that nothing would ever clear.
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

/// One durable attachment-condemnation row exposed to host maintenance code.
///
/// The restoring write's attempt identity remains private to the store
/// implementation. A host needs the owning session to establish quiescence
/// before recovery, but must never be able to present or settle the write's
/// fence identity itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentCondemnationRecord {
    /// Content digest whose physical bytes are fenced by this row.
    pub digest: crate::AttachmentId,
    /// Current public phase of the condemnation state machine.
    pub phase: AttachmentCondemnationPhase,
    /// Authority that currently owns the persisted phase.
    pub provenance: AttachmentCondemnationProvenance,
}

/// Public projection of a persisted attachment-condemnation phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentCondemnationPhase {
    /// A sweep selected the digest but has not armed physical deletion.
    Condemned,
    /// Physical deletion was armed and may already have completed.
    Deleting,
}

/// Public ownership projection for a persisted condemnation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentCondemnationProvenance {
    /// Tokenless state owned by a reclamation sweep.
    SweepOwned,
    /// A restoring write owns the phase for this session.
    RestoringWrite { session_id: SessionId },
}

/// Decode the persisted phase and token/session presence used by durable store
/// implementations without exposing the token itself.
pub fn decode_attachment_condemnation_record(
    digest: crate::AttachmentId,
    phase: &str,
    write_token_present: bool,
    write_session_id: Option<SessionId>,
) -> Result<AttachmentCondemnationRecord, StoreError> {
    let phase = match phase {
        "condemned" => AttachmentCondemnationPhase::Condemned,
        "deleting" => AttachmentCondemnationPhase::Deleting,
        unknown => {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "attachment condemnation",
                message: format!("attachment `{digest}` has unknown phase `{unknown}`"),
            });
        }
    };
    let provenance = match (write_token_present, write_session_id) {
        (false, None) => AttachmentCondemnationProvenance::SweepOwned,
        (true, Some(session_id)) if phase != AttachmentCondemnationPhase::Deleting => {
            super::validate_session_id(&session_id).map_err(|error| {
                StoreError::StoredDataCorrupt {
                    record_kind: "attachment condemnation",
                    message: error.to_string(),
                }
            })?;
            AttachmentCondemnationProvenance::RestoringWrite { session_id }
        }
        (write_token_present, write_session_id) => {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "attachment condemnation",
                message: format!(
                    "attachment `{digest}` has inconsistent phase/provenance: phase `{phase:?}`, write token present {write_token_present}, write session present {}",
                    write_session_id.is_some()
                ),
            });
        }
    };
    Ok(AttachmentCondemnationRecord {
        digest,
        phase,
        provenance,
    })
}

#[cfg(test)]
mod condemnation_record_decode_tests {
    use super::*;

    #[test]
    fn unknown_phase_and_inconsistent_provenance_fail_closed() {
        let digest = || crate::AttachmentId::parse("digest").unwrap();
        for result in [
            decode_attachment_condemnation_record(digest(), "future-phase", false, None),
            decode_attachment_condemnation_record(digest(), "condemned", true, None),
            decode_attachment_condemnation_record(
                digest(),
                "deleting",
                true,
                Some(SessionId::from("session")),
            ),
        ] {
            assert!(matches!(result, Err(StoreError::StoredDataCorrupt { .. })));
        }
    }
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
    /// Upload evidence: when the attempt that owns this row reported that the
    /// backend `put` succeeded. `None` means no upload has ever been proven for
    /// this row, and adoption of the digest cannot be certified by it.
    ///
    /// The attempt identity that stamped it stays inside the store: a host can
    /// see *that* bytes landed, never present the fence identity that proves it.
    pub written_at_epoch_ms: Option<u64>,
    pub committed_at_epoch_ms: Option<u64>,
    /// The row's durable owner, or `None` for an unowned direct host put.
    pub owner: Option<AttachmentOwner>,
}

/// The async attachment-manifest surface required from every
/// [`SessionCommitStore`](super::SessionCommitStore). Used by
/// [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// to record intent rows before `put` and by GC sweeps to reconcile
/// orphans. See the [`AttachmentIntent`] doc comment for the full
/// crash-safety story.
///
/// Backends with no attachment story (in-memory tests, mock stores)
/// paste no-op impls via [`impl_noop_attachment_manifest!`](crate::impl_noop_attachment_manifest) and
/// participate transparently — the fence methods are no-ops, the
/// scoped wrapper still works, and GC sweeps return empty.
#[async_trait::async_trait]
pub trait AttachmentManifest: Send + Sync {
    /// Record the write-ahead intent *and* resolve the digest's condemnation
    /// state in one conditional mutation — the writer half of the attachment GC
    /// fence, and the only way a manifest row is ever created by a writer.
    ///
    /// This is what [`SessionAttachmentStore`](crate::SessionAttachmentStore)
    /// calls before every `put`. It must be a single durable transaction over
    /// the manifest row and the digest's condemnation state:
    ///
    /// * no condemnation — insert/refresh the intent, return
    ///   [`AttachmentWriteFence::Granted`];
    /// * `Condemned` — claim the condemnation with the fresh attempt identity
    ///   (so the sweeper's arm CAS fails) and insert/refresh the intent in the
    ///   *same* transaction, return [`AttachmentWriteFence::Granted`];
    /// * `Deleting` — record nothing and return
    ///   [`AttachmentWriteFence::ReclamationInFlight`].
    ///
    /// Splitting the condemnation read from the intent insert reopens exactly
    /// the window the fence closes, so a backend that cannot express both in one
    /// transaction must not implement this method as two.
    ///
    /// The row records the minted attempt identity and no upload stamp: this
    /// attempt has proven nothing yet. Any `written_at_epoch_ms` or
    /// `committed_at_epoch_ms` already on the row is preserved — a repeat put of
    /// an already-uploaded or already-committed digest must not retract the
    /// evidence a previous attempt earned.
    ///
    /// An authority whose manifest cannot fence must also report
    /// [`AttachmentGcFence::BestEffort`](crate::AttachmentGcFence).
    async fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, StoreError>;

    /// Stamp upload evidence on a granted attachment write after the backend
    /// put succeeds.
    ///
    /// Fenced on every backend and matched on the permit's attempt identity:
    /// only the row still carrying this `write_id` is stamped, and a permit
    /// whose attempt has been superseded certifies nothing and fails with
    /// [`StoreError::StaleWritePermit`]. An existing stamp is preserved — the
    /// first proven upload is the evidence.
    ///
    /// The write-ahead intent stays in place, and a `Condemned` fact this
    /// attempt claimed is cleared now that the bytes exist.
    async fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError>;

    /// Abort a granted attachment write after the backend put fails.
    ///
    /// Deletes only this attempt's row: matching `write_id`, never stamped with
    /// upload evidence, and never committed. A stale permit deletes nothing, so
    /// it cannot clobber a newer attempt's row or another session's adoption.
    /// A `Condemned` fact this attempt claimed is released, unless the same
    /// intent became a committed root while the claim was held; that newer root
    /// supersedes the old unarmed condemnation before an older sweep can arm it.
    async fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError>;

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
    /// # Adoption requires upload evidence
    ///
    /// A digest is adoptable iff some manifest row for it — in *any* session —
    /// carries [`AttachmentManifestEntry::written_at_epoch_ms`], and no
    /// `Deleting` condemnation is in flight for it. Anything else is
    /// [`StoreError::UnknownAttachment`]: the store has never been shown these
    /// bytes, so it must not mint a root for them.
    ///
    /// Every digest in the batch is validated before *anything* is written, so a
    /// batch containing one unknown digest writes no rows at all.
    ///
    /// The adopter's row copies `written_at_epoch_ms` from the evidenced row
    /// alongside its own `committed_at_epoch_ms`. The evidence therefore
    /// survives the uploader's intent being forgotten, and the adopter can be
    /// adopted from in turn.
    ///
    /// Acquisition shares the attachment GC fence: it takes the same per-digest
    /// fence as put and condemn, and revokes an unarmed, unclaimed condemnation
    /// — the fresh committed root supersedes it. The manifest never calls host
    /// blob code; its byte-existence knowledge is the durable upload stamp.
    async fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), StoreError>;

    /// Hosts run this periodically to find orphans left by crashes between
    /// `begin_attachment_write` and the next turn commit.
    async fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError>;

    /// Atomically forget every *uncommitted* intent whose `intent_at_epoch_ms`
    /// is at or before `intent_grace_cutoff_epoch_ms` and whose durable owner
    /// is proven dead. Unscoped host puts have no owner proof and retain the
    /// legacy age-only fallback.
    ///
    /// This MUST be a single conditional operation, not a `list_uncommitted`
    /// read followed by per-row `forget` calls. A concurrent
    /// `begin_attachment_write` for the same `(session, attachment)` refreshes
    /// the intent's timestamp; a
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
    async fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        let _ = intent_grace_cutoff_epoch_ms;
        Ok(())
    }

    /// The cutoff is retention policy after terminal proof, never a liveness oracle for
    /// turn/process owners.
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
    async fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        let _ = intent_grace_cutoff_epoch_ms;
        Ok(self
            .list_all_refs()
            .await?
            .iter()
            .any(|ref_id| ref_id == attachment_id))
    }

    /// Called by the session facade when a turn releases an attachment, and by
    /// `delete_session` when a whole session's refs are dropped.
    /// FIG-653: committed rows needed by retained graph history cannot be forgotten; GC
    /// removes them after the final retained prefix disappears.
    /// Bytes die only after all roots disappear.
    async fn forget(
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
    async fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError>;
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
        #[$crate::async_trait]
        impl $crate::store::attachment_manifest::AttachmentManifest for $ty {
            async fn begin_attachment_write(
                &self,
                _intent: $crate::store::attachment_manifest::AttachmentIntent,
            ) -> ::std::result::Result<
                $crate::store::attachment_manifest::AttachmentWriteFence,
                $crate::store::StoreError,
            > {
                ::std::result::Result::Ok(
                    $crate::store::attachment_manifest::AttachmentWriteFence::Granted(
                        $crate::store::attachment_manifest::AttachmentWritePermit::new(
                            $crate::store::attachment_manifest::AttachmentWriteToken::new(),
                        ),
                    ),
                )
            }

            async fn complete_attachment_write(
                &self,
                _intent: &$crate::store::attachment_manifest::AttachmentIntent,
                _permit: $crate::store::attachment_manifest::AttachmentWritePermit,
            ) -> ::std::result::Result<(), $crate::store::StoreError> {
                Ok(())
            }

            async fn abort_attachment_write(
                &self,
                _intent: &$crate::store::attachment_manifest::AttachmentIntent,
                _permit: $crate::store::attachment_manifest::AttachmentWritePermit,
            ) -> ::std::result::Result<(), $crate::store::StoreError> {
                Ok(())
            }

            async fn commit_refs(
                &self,
                _session_id: &$crate::sansio::SessionId,
                _attachment_ids: &[$crate::sansio::AttachmentId],
            ) -> ::std::result::Result<(), $crate::store::StoreError> {
                Ok(())
            }

            async fn list_uncommitted(
                &self,
                _older_than_epoch_ms: u64,
            ) -> ::std::result::Result<
                Vec<$crate::store::attachment_manifest::AttachmentManifestEntry>,
                $crate::store::StoreError,
            > {
                Ok(Vec::new())
            }

            async fn forget(
                &self,
                _session_id: &$crate::sansio::SessionId,
                _attachment_id: &$crate::sansio::AttachmentId,
            ) -> ::std::result::Result<(), $crate::store::StoreError> {
                Ok(())
            }

            async fn list_all_refs(
                &self,
            ) -> ::std::result::Result<Vec<$crate::sansio::AttachmentId>, $crate::store::StoreError>
            {
                Ok(Vec::new())
            }
        }
    };
}
