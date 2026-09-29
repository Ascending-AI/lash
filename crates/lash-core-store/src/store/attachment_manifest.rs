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
            Err(StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                    surface: "AttachmentManifest owner kind".to_string(),
                    label: unknown.to_string(),
                },
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
///   │   Free   │ ───── row for the digest is deleted ──> │ Condemned │ <─┐
///   └──────────┘ <──── spare (sweep gives it back) ──────└───────────┘   │
///       ^   ^                                                 │ arm      │ delete failed:
///       │   │                                                 v          │ attempts + 1,
///       │   │                                           ┌───────────┐    │ stalled past
///       │   └──── spare (bytes refreshed) ──────────────│ Deleting  │ ───┘ the bound
///       │                                               └───────────┘
///       │                                                     │
///       └──── delete succeeded or bytes already gone: the ────┘
///             condemnation row is retired, and no manifest
///             row survives to make the digest adoptable again
/// ```
///
/// * `Free` — the ordinary state. A writer records its intent and the digest is
///   rooted; a sweeper that finds no root may condemn it.
/// * `Condemned` — a sweeper claimed the digest for deletion but has issued no
///   physical delete yet, or its delete failed. A writer arriving here claims
///   the phase with its attempt identity and records its intent in one
///   mutation, so the sweeper's later arm CAS fails. Success clears the claimed
///   phase after bytes exist; failure releases the claim while preserving
///   `Condemned`, unless the same intent became committed while the claim was
///   held; that root returns the digest to `Free` before the old sweep can arm.
/// * `Deleting` — the physical delete is in flight. A writer arriving here
///   cannot un-issue it, so it records nothing and retries.
///
/// Every sweep-owned row names the sweep generation that owns it
/// ([`AttachmentSweepGeneration`]). Only that generation moves it, and a later
/// sweep adopts it only once the owning pass is provably dead (ADR 0067 §6):
/// a crashed sweeper's `Condemned` or `Deleting` row is finished by the next
/// sweep, never by a host.
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
    /// A condemnation for this digest already exists: a live peer sweep owns
    /// it, a restoring writer holds it, or its delete is stalled. The sweep
    /// defers the digest rather than contending for it; a condemnation left by
    /// a dead sweep is adopted at the start of the next sweep instead.
    AlreadyCondemned,
    /// This root authority implements no fence. The sweep falls back to its
    /// best-effort, unfenced path — see
    /// [`AttachmentGcFence`](crate::AttachmentGcFence).
    Unsupported,
}

/// One durable attachment-condemnation row exposed to host maintenance code.
///
/// This is the operator's listing of what is stuck right now (ADR 0067 §6): a
/// row a sweep keeps failing to delete carries its attempt count, its last
/// error and, past the retry bound, its typed stall.
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
    /// Physical deletes of this digest that failed, across every sweep.
    pub delete_attempts: u32,
    /// The most recent failed delete's error, when one failed.
    pub last_delete_error: Option<String>,
    /// Why the delete stalled. Kept while later sweeps retry it with backoff.
    pub stalled: Option<AttachmentDeleteStallReason>,
}

/// Why a condemned digest's physical delete stalled before later retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttachmentDeleteStallReason {
    /// Retryable failures reached [`MAX_ATTACHMENT_DELETE_ATTEMPTS`].
    AttemptsExhausted,
    /// The backend refused the delete because of credentials, authorization,
    /// or a terminal or contract failure. Later retries can observe recovery.
    Refused,
}

impl AttachmentDeleteStallReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 2] = [Self::AttemptsExhausted, Self::Refused];

    /// The stored label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::Refused => "refused",
        }
    }

    /// Decode a stored stall label, rejecting unknown reasons.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == label)
    }
}

/// How many failed physical deletes of one condemned digest the sweeps make,
/// one per sweep, before the condemnation stalls
/// [`AttemptsExhausted`](AttachmentDeleteStallReason::AttemptsExhausted).
pub const MAX_ATTACHMENT_DELETE_ATTEMPTS: u32 = 5;

/// One attachment sweep pass: the generation its condemnations are stamped
/// with, and the liveness that proves the pass has not died (ADR 0067 §6).
///
/// A root authority mints a fresh generation for every pass and keeps the
/// pass's liveness for as long as this value is held. Dropping it — the pass
/// returning, its task being cancelled, or its process dying — ends that
/// liveness, and from then on a later pass may adopt the rows it left. The
/// proof is structural, never a timer: an in-process registry where the
/// store runs in one process, a session-scoped lock held on a dedicated
/// connection where it does not.
pub struct AttachmentSweepGeneration {
    generation: u64,
    _liveness: Box<dyn std::any::Any + Send + Sync>,
}

impl AttachmentSweepGeneration {
    /// Generation `generation`, live for as long as `liveness` is held.
    pub fn new(generation: u64, liveness: Box<dyn std::any::Any + Send + Sync>) -> Self {
        Self {
            generation,
            _liveness: liveness,
        }
    }

    /// The generation this pass stamps on the rows it owns.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl std::fmt::Debug for AttachmentSweepGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachmentSweepGeneration")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// One condemnation a sweep adopted from a dead predecessor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedAttachmentCondemnation {
    pub digest: crate::AttachmentId,
    /// `Condemned` still needs arming; `Deleting` may already have deleted the
    /// bytes.
    pub phase: AttachmentCondemnationPhase,
    /// Failed deletes recorded before adoption.
    pub delete_attempts: u32,
    /// Existing stall, retained while its retry is in flight.
    pub stalled: Option<AttachmentDeleteStallReason>,
}

/// What adoption found among the sweep-owned condemnations of older
/// generations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachmentCondemnationAdoption {
    /// Rows whose pass is dead, now owned by the adopting generation.
    pub adopted: Vec<AdoptedAttachmentCondemnation>,
    /// Rows whose pass is still live: left to that pass.
    pub held_by_live_pass: Vec<crate::AttachmentId>,
    /// Rows whose retry is not due yet and whose delete has not stalled.
    pub backing_off: Vec<crate::AttachmentId>,
    /// Stalled rows left waiting for backoff or a live pass.
    pub stalled: Vec<crate::AttachmentId>,
}

/// How a sweep settles a condemnation its generation owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentCondemnationSettlement {
    /// The bytes are gone, deleted by this pass or already absent: retire the
    /// `Deleting` row.
    Deleted,
    /// The pass gives the digest back without deleting it: remove the
    /// unclaimed `Condemned` or `Deleting` row.
    Spared,
    /// The final `HEAD` or the physical delete failed: `Deleting ->
    /// Condemned`, one more failed attempt, and `error` recorded. With `stall`
    /// the row stays listed as stalled. Every failure schedules a later retry
    /// with capped backoff on the store clock.
    Failed {
        stall: Option<AttachmentDeleteStallReason>,
        error: String,
    },
}

/// Whether a settlement applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentSettlementOutcome {
    Applied,
    /// The row is not this generation's in the phase the settlement expects:
    /// a restoring writer claimed it, or it is already settled.
    NotOwned,
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

/// One persisted condemnation row, as a durable store reads it back.
#[derive(Clone, Debug)]
pub struct StoredAttachmentCondemnation {
    pub digest: crate::AttachmentId,
    pub phase: String,
    pub write_token_present: bool,
    pub write_session_id: Option<SessionId>,
    pub delete_attempts: i64,
    pub last_delete_error: Option<String>,
    pub stall_reason: Option<String>,
}

/// Decode a persisted condemnation row used by durable store implementations
/// without exposing the write token.
pub fn decode_attachment_condemnation_record(
    row: StoredAttachmentCondemnation,
) -> Result<AttachmentCondemnationRecord, StoreError> {
    let StoredAttachmentCondemnation {
        digest,
        phase,
        write_token_present,
        write_session_id,
        delete_attempts,
        last_delete_error,
        stall_reason,
    } = row;
    let corrupt = |message: String| StoreError::StoredDataCorrupt {
        record_kind: "attachment condemnation",
        message,
    };
    let phase = match phase.as_str() {
        "condemned" => AttachmentCondemnationPhase::Condemned,
        "deleting" => AttachmentCondemnationPhase::Deleting,
        unknown => {
            return Err(corrupt(format!(
                "attachment `{digest}` has unknown phase `{unknown}`"
            )));
        }
    };
    let provenance = match (write_token_present, write_session_id) {
        (false, None) => AttachmentCondemnationProvenance::SweepOwned,
        (true, Some(session_id)) if phase != AttachmentCondemnationPhase::Deleting => {
            super::validate_session_id(&session_id).map_err(|error| corrupt(error.to_string()))?;
            AttachmentCondemnationProvenance::RestoringWrite { session_id }
        }
        (write_token_present, write_session_id) => {
            return Err(corrupt(format!(
                "attachment `{digest}` has inconsistent phase/provenance: phase `{phase:?}`, write token present {write_token_present}, write session present {}",
                write_session_id.is_some()
            )));
        }
    };
    let delete_attempts = u32::try_from(delete_attempts).map_err(|_| {
        corrupt(format!(
            "attachment `{digest}` has delete attempt count {delete_attempts}"
        ))
    })?;
    let stalled = stall_reason
        .map(|label| {
            AttachmentDeleteStallReason::from_label(&label).ok_or_else(|| {
                corrupt(format!(
                    "attachment `{digest}` has unknown stall reason `{label}`"
                ))
            })
        })
        .transpose()?;
    if (delete_attempts == 0) != last_delete_error.is_none()
        || (stalled.is_some() && delete_attempts == 0)
    {
        return Err(corrupt(format!(
            "attachment `{digest}` has inconsistent delete failure state: phase `{phase:?}`, {delete_attempts} attempts, error present {}, stall {stalled:?}",
            last_delete_error.is_some()
        )));
    }
    Ok(AttachmentCondemnationRecord {
        digest,
        phase,
        provenance,
        delete_attempts,
        last_delete_error,
        stalled,
    })
}

#[cfg(test)]
mod condemnation_record_decode_tests {
    use super::*;

    fn row(phase: &str) -> StoredAttachmentCondemnation {
        StoredAttachmentCondemnation {
            digest: crate::AttachmentId::parse("digest").unwrap(),
            phase: phase.to_owned(),
            write_token_present: false,
            write_session_id: None,
            delete_attempts: 0,
            last_delete_error: None,
            stall_reason: None,
        }
    }

    #[test]
    fn unknown_phase_and_inconsistent_provenance_fail_closed() {
        for stored in [
            row("future-phase"),
            StoredAttachmentCondemnation {
                write_token_present: true,
                ..row("condemned")
            },
            StoredAttachmentCondemnation {
                write_token_present: true,
                write_session_id: Some(SessionId::from("session")),
                ..row("deleting")
            },
        ] {
            assert!(matches!(
                decode_attachment_condemnation_record(stored),
                Err(StoreError::StoredDataCorrupt { .. })
            ));
        }
    }

    #[test]
    fn unknown_or_inconsistent_delete_failure_state_fails_closed() {
        let failed = |stall: Option<&str>| StoredAttachmentCondemnation {
            delete_attempts: 1,
            last_delete_error: Some("backend down".to_owned()),
            stall_reason: stall.map(str::to_owned),
            ..row("condemned")
        };
        for stored in [
            failed(Some("future-reason")),
            StoredAttachmentCondemnation {
                last_delete_error: None,
                ..failed(None)
            },
            StoredAttachmentCondemnation {
                delete_attempts: -1,
                ..failed(None)
            },
            StoredAttachmentCondemnation {
                stall_reason: Some("refused".to_owned()),
                ..row("condemned")
            },
        ] {
            assert!(matches!(
                decode_attachment_condemnation_record(stored),
                Err(StoreError::StoredDataCorrupt { .. })
            ));
        }
        let retrying = decode_attachment_condemnation_record(StoredAttachmentCondemnation {
            phase: "deleting".to_owned(),
            ..failed(Some("refused"))
        })
        .expect("an armed retry retains its stall");
        assert_eq!(retrying.stalled, Some(AttachmentDeleteStallReason::Refused));
        let stalled = decode_attachment_condemnation_record(failed(Some("attempts_exhausted")))
            .expect("a stalled condemnation decodes");
        assert_eq!(
            stalled.stalled,
            Some(AttachmentDeleteStallReason::AttemptsExhausted)
        );
    }
}

/// Outcome of arming the physical delete for a condemned digest
/// ([`AttachmentRootSet::arm_attachment_delete`](crate::AttachmentRootSet::arm_attachment_delete)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentDeleteArming {
    /// `Condemned -> Deleting`: this sweeper owns the delete. Writers arriving
    /// from here on retry instead of writing bytes.
    Armed,
    /// This caller's generation no longer owns an armable condemnation. A
    /// writer may hold the existing phase with a restoration token, or the row
    /// may be absent, in another phase, or owned by another generation. The
    /// delete is not issued and this caller must not settle state it does not
    /// own.
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
