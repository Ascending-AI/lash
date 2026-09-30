//! The attachment referrer and external-byte write contract.
use super::StoreError;
use crate::SessionId;

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
/// (the `attachment_uploads` row), not on a negative
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
    /// A restoring pending write owns the phase for its referrer.
    RestoringWrite {
        referrer: crate::artifact_referrer::ArtifactReferrer,
    },
}

/// One persisted condemnation row, as a durable store reads it back.
#[derive(Clone, Debug)]
pub struct StoredAttachmentCondemnation {
    pub digest: crate::AttachmentId,
    pub phase: String,
    pub write_token_present: bool,
    pub write_referrer: Option<(String, String)>,
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
        write_referrer,
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
    let provenance = match (write_token_present, write_referrer) {
        (false, None) => AttachmentCondemnationProvenance::SweepOwned,
        (true, Some((kind, id))) if phase != AttachmentCondemnationPhase::Deleting => {
            let referrer = crate::artifact_referrer::ArtifactReferrer::decode(&kind, &id)
                .map_err(|error| error.into_store_error("attachment condemnation"))?;
            AttachmentCondemnationProvenance::RestoringWrite { referrer }
        }
        (write_token_present, write_referrer) => {
            return Err(corrupt(format!(
                "attachment `{digest}` has inconsistent phase/provenance: phase `{phase:?}`, write token present {write_token_present}, write session present {}",
                write_referrer.is_some()
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
            write_referrer: None,
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
                write_referrer: Some(("session".into(), "session".into())),
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

/// A write attempt's digest and durable referrer claim.
#[derive(Clone, Debug)]
pub struct AttachmentWrite {
    pub attachment_id: crate::AttachmentId,
    pub claim: crate::artifact_referrer::ReferrerClaim,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionReferrerState {
    Live,
    Absent,
    DeletedRetained,
    DeletedRetired,
}
/// Attachment edges, pending writes, and upload evidence of one durable core.
#[async_trait::async_trait]
pub trait AttachmentManifest: Send + Sync {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, StoreError>;
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError>;
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError>;
    async fn acquire_attachment_refs(
        &self,
        claim: &crate::artifact_referrer::ReferrerClaim,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), StoreError>;
    async fn forget_attachment_ref(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), StoreError>;
    async fn end_attachment_referrer(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
    ) -> Result<(), StoreError>;
    async fn session_referrer_state(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReferrerState, StoreError>;
    /// Strictly decoded, ordered by kind and canonical id.
    async fn attachment_referrers(
        &self,
        attachment_id: &crate::AttachmentId,
    ) -> Result<Vec<crate::artifact_referrer::ArtifactReferrer>, StoreError>;
}
/// Attachment-free stores grant writes and retain no durable edges.
#[macro_export]
macro_rules! impl_noop_attachment_manifest {
    ($ty:ty) => {
        #[$crate::async_trait]
        impl $crate::store::AttachmentManifest for $ty {
            async fn begin_attachment_write(
                &self,
                _: &$crate::store::AttachmentWrite,
            ) -> Result<$crate::store::AttachmentWriteFence, $crate::store::StoreError> {
                Ok($crate::store::AttachmentWriteFence::Granted(
                    $crate::store::AttachmentWritePermit::new(
                        $crate::store::AttachmentWriteToken::new(),
                    ),
                ))
            }
            async fn complete_attachment_write(
                &self,
                _: &$crate::store::AttachmentWrite,
                _: $crate::store::AttachmentWritePermit,
            ) -> Result<(), $crate::store::StoreError> {
                Ok(())
            }
            async fn abort_attachment_write(
                &self,
                _: &$crate::store::AttachmentWrite,
                _: $crate::store::AttachmentWritePermit,
            ) -> Result<(), $crate::store::StoreError> {
                Ok(())
            }
            async fn acquire_attachment_refs(
                &self,
                _: &$crate::artifact_referrer::ReferrerClaim,
                _: &[$crate::sansio::AttachmentId],
            ) -> Result<(), $crate::store::StoreError> {
                Ok(())
            }
            async fn forget_attachment_ref(
                &self,
                _: &$crate::artifact_referrer::ArtifactReferrer,
                _: &$crate::sansio::AttachmentId,
            ) -> Result<(), $crate::store::StoreError> {
                Ok(())
            }
            async fn end_attachment_referrer(
                &self,
                _: &$crate::artifact_referrer::ArtifactReferrer,
            ) -> Result<(), $crate::store::StoreError> {
                Ok(())
            }
            async fn session_referrer_state(
                &self,
                _: &$crate::sansio::SessionId,
            ) -> Result<$crate::store::SessionReferrerState, $crate::store::StoreError> {
                Ok($crate::store::SessionReferrerState::Live)
            }
            async fn attachment_referrers(
                &self,
                _: &$crate::sansio::AttachmentId,
            ) -> Result<Vec<$crate::artifact_referrer::ArtifactReferrer>, $crate::store::StoreError>
            {
                Ok(Vec::new())
            }
        }
    };
}
