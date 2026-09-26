//! The drive fence and its store half (ADR 0105 §2, B3).
//!
//! [`DriveFence`] and [`AdmissionId`] live here, below the engine contract,
//! because the stores check the fence: `lash_core::engine` re-exports them
//! unchanged. The drive epoch they fence is a monotonic counter on the
//! session's `session_meta` row, next to the id of the admission that last
//! raised it. Only [`DriveEpochStore::seal_drive_epoch`] raises it, with a
//! compare-and-set, so replay cannot mint ownership and nothing expires it.
//!
//! The same row keeps the admitted root's start marker (ADR 0105 §2, L-S8):
//! the [`RootStartNonce`] the execution that sealed the admission drew in its
//! own journal. A retry of that execution replays the same nonce and finds
//! its own seal; a fresh execution of the same admission (its journal gone)
//! draws another, and the seal answers it `ExecutionLost` instead of letting
//! the root run twice.

use serde::{Deserialize, Serialize};

use super::StoreError;
use crate::SessionId;

/// The authority of one drive over one session.
///
/// It has no public constructor. A fence comes from exactly two places: the
/// store's own seal ([`DriveEpochStore::seal_drive_epoch`]), and serde
/// decoding of a recorded `SealVerdict::Sealed` or `InheritVerdict::Valid`
/// from the drive's journal. `Deserialize` exists only for that
/// recorded-verdict path; nothing else may decode a fence. A decoded fence
/// still authorizes nothing by itself: every fenced store operation checks
/// its epoch *and* admission against the session's `session_meta` row in its
/// own transaction. It is never part of an envelope hash (ADR 0105 law
/// L-S12).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DriveFence {
    session: SessionId,
    epoch: u64,
    admission: AdmissionId,
}

impl DriveFence {
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn admission(&self) -> &AdmissionId {
        &self.admission
    }

    /// The fence a store's seal returns for the epoch it just raised, or the
    /// one a seal retried under the same admission finds. Backends reach it
    /// only through the backend-support
    /// [`sealed_drive_fence`](crate::store_backend_support::sealed_drive_fence).
    #[must_use]
    pub(crate) fn sealed_by_store(session: SessionId, epoch: u64, admission: AdmissionId) -> Self {
        Self {
            session,
            epoch,
            admission,
        }
    }
}

/// The nonce one admission is keyed by. Retried seal bodies with the same
/// nonce are idempotent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdmissionId(String);

impl AdmissionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The start marker of one execution of an admitted root (ADR 0105 §2,
/// L-S8, FIG-3815).
///
/// The root draws it as its first recorded step, in its own journal, before
/// it seals its admission. Every retry of that execution replays the same
/// nonce; an execution that cannot read that journal draws a new one. The
/// seal sets it with the admission and compares it on every later seal of
/// the same admission.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RootStartNonce(String);

impl RootStartNonce {
    pub fn new(nonce: impl Into<String>) -> Self {
        Self(nonce.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A session head a drive names: the state generation, the head revision, the
/// leaf of its graph and its checkpoint (ADR 0105 §2, §9).
///
/// A commit names the head it expects. An admission names the head it was
/// admitted on, its base: a replay of the admitted turn rebuilds the turn's
/// input state from this reference, never from the live head, which the turn's
/// own commit or a lane service may have advanced since (FIG-3682). `leaf` and
/// `checkpoint` are `None` for a session with no committed graph or checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionHeadRef {
    pub generation: u32,
    pub revision: u64,
    pub leaf: Option<crate::NodeId>,
    pub checkpoint: Option<super::BlobRef>,
}

impl SessionHeadRef {
    /// Whether `head` is this head: the same revision, leaf and checkpoint.
    /// The generation is the store's, not the head row's, so it is compared
    /// by the caller that read it.
    #[must_use]
    pub fn names_head(&self, head: &super::SessionHeadMeta) -> bool {
        self.revision == head.head_revision
            && self.leaf == head.leaf_node_id
            && self.checkpoint == head.checkpoint_ref
    }
}

/// What the store's seal answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveEpochSeal {
    /// The epoch was raised to the fence's epoch under this admission, now or
    /// by an earlier invocation of the same seal.
    Sealed(DriveFence),
    /// Another admission raised the epoch past the observed one.
    Superseded { epoch: u64 },
    /// This admission was sealed by another execution of its root, which
    /// drew another start marker: this execution cannot read what that one
    /// did, so it must not run the root (L-S8).
    ExecutionLost,
}

/// The durable drive epoch of a session as its `session_meta` row stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDriveEpoch {
    pub epoch: u64,
    /// The admission that last raised the epoch; `None` before the first seal.
    pub admission: Option<AdmissionId>,
    /// The start marker of the execution that sealed `admission`; `None`
    /// before the first seal, and for a seal written before markers existed.
    pub root_start: Option<RootStartNonce>,
    /// The `CloseSession` intent the session is closing under (FIG-3600 S7):
    /// a closing session admits nothing and seals nothing.
    pub closing: Option<super::ControlIntentId>,
    /// An unacknowledged cancel or fork still owns release of the old execution.
    pub control_pending: bool,
}

/// Decide one seal from the stored epoch (ADR 0105 §2).
///
/// The stored admission is checked first: when this admission is the one
/// that last raised the epoch, the seal already happened, and one admission
/// makes exactly one epoch transition (ADR 0105 L-S3, L-S4). Its start marker
/// then says by whom: the same marker is a retry of the execution that sealed
/// it (a lost reply, whatever epoch the retried body observed) and answers
/// the stored fence without writing; another marker is a fresh execution of
/// a root that already started, which is `ExecutionLost` (L-S8). A seal
/// stored without a marker predates markers and is answered as a retry.
/// Otherwise a seal observed at the stored epoch raises it by one and stores
/// its marker, and anything else was superseded. A closing session raises
/// nothing: its close already raised the epoch past every admission.
#[must_use]
pub fn decide_drive_epoch_seal(
    session_id: &SessionId,
    stored: &StoredDriveEpoch,
    admission: &AdmissionId,
    observed_epoch: u64,
    root_start: &RootStartNonce,
) -> DriveEpochSealDecision {
    if stored.admission.as_ref() == Some(admission) {
        if stored
            .root_start
            .as_ref()
            .is_some_and(|sealed_by| sealed_by != root_start)
        {
            return DriveEpochSealDecision::Answer(DriveEpochSeal::ExecutionLost);
        }
        return DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(
            DriveFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone()),
        ));
    }
    if stored.epoch == observed_epoch && stored.closing.is_none() && !stored.control_pending {
        return DriveEpochSealDecision::Raise {
            next: observed_epoch.saturating_add(1),
        };
    }
    DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded {
        epoch: stored.epoch,
    })
}

/// What a backend does for one seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveEpochSealDecision {
    /// Compare-and-set the epoch from the observed value to `next`, recording
    /// the admission.
    Raise { next: u64 },
    /// Answer without writing.
    Answer(DriveEpochSeal),
}

/// Refuse `fence` unless it names `session_id` and is exactly the session's
/// current drive fence: the stored epoch *and* the admission that raised it.
/// Every fenced ingress operation calls it inside its own transaction.
pub fn require_current_drive_fence(
    session_id: &SessionId,
    fence: &DriveFence,
    current: &StoredDriveEpoch,
) -> Result<(), StoreError> {
    if fence.session() != session_id {
        return Err(StoreError::DriveFenceSessionMismatch {
            session_id: session_id.clone(),
            fence_session_id: fence.session().clone(),
        });
    }
    if fence.epoch() != current.epoch || current.admission.as_ref() != Some(fence.admission()) {
        return Err(StoreError::StaleDriveFence {
            session_id: session_id.clone(),
            fence_epoch: fence.epoch(),
            current_epoch: current.epoch,
        });
    }
    Ok(())
}

/// The storage half of drive admission: read and raise a session's drive
/// epoch. It holds no engine logic: the engine's seal step calls it.
#[async_trait::async_trait]
pub trait DriveEpochStore: Send + Sync {
    /// Compare-and-set the session's drive epoch from `observed_epoch` to the
    /// next value under `admission`, storing `root_start` with it; idempotent
    /// per admission and start marker ([`decide_drive_epoch_seal`]).
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
    ) -> Result<DriveEpochSeal, StoreError>;

    /// The session's stored drive epoch.
    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError>;
}

/// A drive-epoch ledger held in memory, for store doubles that keep no
/// `session_meta` row. It decides every seal with
/// [`decide_drive_epoch_seal`], exactly as a SQL backend does inside its
/// transaction.
#[derive(Debug, Default)]
pub struct InMemoryDriveEpochs {
    epochs: std::sync::Mutex<std::collections::BTreeMap<SessionId, StoredDriveEpoch>>,
}

impl InMemoryDriveEpochs {
    /// [`DriveEpochStore::seal_drive_epoch`] over this ledger.
    pub fn seal(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
    ) -> DriveEpochSeal {
        let mut epochs = self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = epochs
            .entry(session_id.clone())
            .or_insert(StoredDriveEpoch {
                epoch: 0,
                admission: None,
                root_start: None,
                closing: None,
                control_pending: false,
            });
        match decide_drive_epoch_seal(session_id, stored, admission, observed_epoch, root_start) {
            DriveEpochSealDecision::Answer(seal) => seal,
            DriveEpochSealDecision::Raise { next } => {
                *stored = StoredDriveEpoch {
                    epoch: next,
                    admission: Some(admission.clone()),
                    root_start: Some(root_start.clone()),
                    closing: None,
                    control_pending: false,
                };
                DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                    session_id.clone(),
                    next,
                    admission.clone(),
                ))
            }
        }
    }

    /// [`DriveEpochStore::drive_epoch`] over this ledger.
    pub fn epoch(&self, session_id: &SessionId) -> StoredDriveEpoch {
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
            .unwrap_or(StoredDriveEpoch {
                epoch: 0,
                admission: None,
                root_start: None,
                closing: None,
                control_pending: false,
            })
    }

    /// Close `session_id` under `intent`: the drive-epoch half of
    /// [`ControlIntentStore::begin_session_close`](super::ControlIntentStore::begin_session_close).
    /// A session already closing keeps its first intent.
    pub fn close(&self, session_id: &SessionId, intent: super::ControlIntentId) {
        let mut epochs = self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = epochs
            .entry(session_id.clone())
            .or_insert(StoredDriveEpoch {
                epoch: 0,
                admission: None,
                root_start: None,
                closing: None,
                control_pending: false,
            });
        if stored.closing.is_none() {
            *stored = StoredDriveEpoch {
                epoch: stored.epoch.saturating_add(1),
                admission: Some(close_admission(intent)),
                root_start: None,
                closing: Some(intent),
                control_pending: false,
            };
        }
    }
}

/// The admission a session close records as the one that last raised the
/// drive epoch: `intent:{id}`.
#[must_use]
pub fn close_admission(intent: super::ControlIntentId) -> AdmissionId {
    AdmissionId::new(format!("intent:{intent}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(epoch: u64, admission: Option<&str>) -> StoredDriveEpoch {
        StoredDriveEpoch {
            epoch,
            admission: admission.map(AdmissionId::new),
            root_start: admission.map(|_| RootStartNonce::new("n")),
            closing: None,
            control_pending: false,
        }
    }

    fn nonce() -> RootStartNonce {
        RootStartNonce::new("n")
    }

    #[test]
    fn a_closing_session_seals_nothing() {
        let session = SessionId::from("s");
        let closing = StoredDriveEpoch {
            epoch: 5,
            admission: Some(AdmissionId::new("intent:1")),
            root_start: None,
            closing: Some(super::super::ControlIntentId::from_sequence(1)),
            control_pending: false,
        };
        assert_eq!(
            decide_drive_epoch_seal(&session, &closing, &AdmissionId::new("a"), 5, &nonce()),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded { epoch: 5 })
        );
    }

    #[test]
    fn a_seal_raises_once_and_a_retried_seal_answers_the_same_fence() {
        let session = SessionId::from("s");
        let admission = AdmissionId::new("a");
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(3, None), &admission, 3, &nonce()),
            DriveEpochSealDecision::Raise { next: 4 }
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("a")), &admission, 3, &nonce()),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                session.clone(),
                4,
                admission.clone()
            )))
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("a")), &admission, 4, &nonce()),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                session.clone(),
                4,
                admission.clone()
            ))),
            "a retry that re-read the epoch it raised does not raise again"
        );
        assert_eq!(
            decide_drive_epoch_seal(
                &session,
                &stored(4, Some("a")),
                &admission,
                3,
                &RootStartNonce::new("another execution")
            ),
            DriveEpochSealDecision::Answer(DriveEpochSeal::ExecutionLost),
            "a fresh execution of the sealed admission is lost, whatever it observed"
        );
        assert_eq!(
            decide_drive_epoch_seal(
                &session,
                &StoredDriveEpoch {
                    root_start: None,
                    ..stored(4, Some("a"))
                },
                &admission,
                3,
                &nonce()
            ),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                session.clone(),
                4,
                admission.clone()
            ))),
            "a seal stored before markers is answered as a retry"
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("b")), &admission, 3, &nonce()),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded { epoch: 4 })
        );
    }
}
