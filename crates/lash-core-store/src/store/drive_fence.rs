//! The drive fence and its store half (ADR 0105 §2, B3).
//!
//! [`DriveFence`] and [`AdmissionId`] live here, below the engine contract,
//! because the stores check the fence: `lash_core::engine` re-exports them
//! unchanged. The drive epoch they fence is a monotonic counter on the
//! session's `session_meta` row, next to the id of the admission that last
//! raised it. Only [`DriveEpochStore::seal_drive_epoch`] raises it, with a
//! compare-and-set, so replay cannot mint ownership and nothing expires it.

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
}

/// The durable drive epoch of a session as its `session_meta` row stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDriveEpoch {
    pub epoch: u64,
    /// The admission that last raised the epoch; `None` before the first seal.
    pub admission: Option<AdmissionId>,
}

/// Decide one seal from the stored epoch (ADR 0105 §2).
///
/// The stored admission is checked first: when this admission is the one
/// that last raised the epoch, the seal already happened — a retry after a
/// lost reply, whatever epoch the retried body observed — and it answers the
/// stored fence without writing, so one admission makes exactly one epoch
/// transition (ADR 0105 L-S3, L-S4). Otherwise a seal observed at the stored
/// epoch raises it by one, and anything else was superseded.
#[must_use]
pub fn decide_drive_epoch_seal(
    session_id: &SessionId,
    stored: &StoredDriveEpoch,
    admission: &AdmissionId,
    observed_epoch: u64,
) -> DriveEpochSealDecision {
    if stored.admission.as_ref() == Some(admission) {
        return DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(
            DriveFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone()),
        ));
    }
    if stored.epoch == observed_epoch {
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
    /// next value under `admission`, idempotently per admission.
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
    ) -> Result<DriveEpochSeal, StoreError>;

    /// The session's stored drive epoch.
    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(epoch: u64, admission: Option<&str>) -> StoredDriveEpoch {
        StoredDriveEpoch {
            epoch,
            admission: admission.map(AdmissionId::new),
        }
    }

    #[test]
    fn a_seal_raises_once_and_a_retried_seal_answers_the_same_fence() {
        let session = SessionId::from("s");
        let admission = AdmissionId::new("a");
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(3, None), &admission, 3),
            DriveEpochSealDecision::Raise { next: 4 }
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("a")), &admission, 3),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                session.clone(),
                4,
                admission.clone()
            )))
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("a")), &admission, 4),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(DriveFence::sealed_by_store(
                session.clone(),
                4,
                admission.clone()
            ))),
            "a retry that re-read the epoch it raised does not raise again"
        );
        assert_eq!(
            decide_drive_epoch_seal(&session, &stored(4, Some("b")), &admission, 3),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded { epoch: 4 })
        );
    }
}
