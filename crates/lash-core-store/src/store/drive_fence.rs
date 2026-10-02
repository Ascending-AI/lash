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
/// It has no public constructor. A fence comes from exactly three places:
/// the store's own seal ([`DriveEpochStore::seal_drive_epoch`]); the store's
/// read of the current fence ([`current_drive_fence`]),
/// which a writer beside the drive presents so that it never writes over a
/// later admission; and serde decoding of a recorded step that carries one: a
/// `SealVerdict::Sealed` from the drive's journal, or the fence an
/// administrative compaction records with its base (FIG-4134).
/// `Deserialize` exists only for those recorded paths; nothing else may
/// decode a fence. A decoded fence still authorizes nothing by itself: every
/// fenced store operation checks its epoch *and* admission against the
/// session's `session_meta` row in its own transaction. It is never part of
/// an envelope hash (ADR 0105 law L-S12).
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
    /// `root` is held by `recorded`, another executor an engine holds a run
    /// for, and has not ended (FIG-4814): the epoch stays where that
    /// executor's seal raised it, and the sealer waits for the root's end.
    HeldByAnotherExecutor {
        root: crate::TurnId,
        recorded: Box<super::RootExecutor>,
    },
}

/// The root a seal's admission runs and the execution that runs it
/// ([`DriveEpochStore::seal_drive_epoch`], FIG-4814).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootHold {
    pub root: crate::TurnId,
    pub executor: super::RootExecutor,
}

/// What the store holds for the root a seal names, read in the seal's
/// transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRoot {
    /// The executor recorded for the root: its admission's, else the one
    /// the last seal recorded for it.
    pub executor: super::RootExecutor,
    /// The root has terminal evidence.
    pub ended: bool,
}

/// Decide whether a seal that would raise the epoch at `stored_epoch` may,
/// given who holds the roots it would run beside (FIG-4814). `None` lets it
/// raise.
///
/// A root's executor is recorded by the seal that first raises the epoch
/// for it, and by its admission from then on. Another executor an engine
/// holds a run for ([`RootExecutor::excludes`](super::RootExecutor::excludes))
/// never seals over it: not while the session's unfinished root is that
/// executor's, and not for a root that executor sealed and has yet to
/// admit. The refused sealer raises nothing, so the recorded executor's
/// fence stands until its root ends. A root that already ended under
/// another executor is not this admission's to run: the seal answers
/// superseded.
#[must_use]
pub fn decide_root_hold(
    hold: &RootHold,
    stored_epoch: u64,
    held: Option<&HeldRoot>,
    unfinished: Option<&super::UnfinishedRoot>,
) -> Option<DriveEpochSeal> {
    if let Some(unfinished) = unfinished
        && unfinished.executor.excludes(&hold.executor)
    {
        return Some(DriveEpochSeal::HeldByAnotherExecutor {
            root: unfinished.root.clone(),
            recorded: Box::new(unfinished.executor.clone()),
        });
    }
    let held = held.filter(|held| held.executor.excludes(&hold.executor))?;
    Some(if held.ended {
        DriveEpochSeal::Superseded {
            epoch: stored_epoch,
        }
    } else {
        DriveEpochSeal::HeldByAnotherExecutor {
            root: hold.root.clone(),
            recorded: Box::new(held.executor.clone()),
        }
    })
}

/// What last raised a session's drive epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveRaise {
    /// An execution of an admitted root sealed `admission`, drawing
    /// `root_start` as its start marker.
    Sealed {
        admission: AdmissionId,
        root_start: RootStartNonce,
    },
    /// A control verb (a cancel, a fork, a session close) raised the epoch
    /// past every sealed fence under `admission`. No execution sealed it, so
    /// it has no start marker.
    Control { admission: AdmissionId },
}

impl DriveRaise {
    /// The admission the raise recorded.
    #[must_use]
    pub fn admission(&self) -> &AdmissionId {
        match self {
            Self::Sealed { admission, .. } | Self::Control { admission } => admission,
        }
    }
}

/// The durable drive epoch of a session as its `session_meta` row stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDriveEpoch {
    pub epoch: u64,
    /// What last raised the epoch; `None` exactly while the epoch is zero.
    pub last_raise: Option<DriveRaise>,
    /// The `CloseSession` intent the session is closing under (FIG-3600 S7):
    /// a closing session admits nothing and seals nothing.
    pub closing: Option<super::ControlIntentId>,
    /// A cancel or fork still owes its engine half — the release of the
    /// root's old execution — and will retry it: it is pending, or failed and
    /// retryable. The session admits nothing until the release lands. A verb
    /// the engine refused for good owes nothing more and holds nothing back:
    /// its store half already ended the root and raised the epoch past the
    /// old execution's fence, so that execution can neither commit nor park.
    pub control_pending: bool,
    /// The session's standing fault (ADR 0109 §9): corrupt stored data met
    /// after a root's answer was published. Every drive is refused with it
    /// until an operator clears it.
    pub fault: Option<super::SessionFault>,
}

impl StoredDriveEpoch {
    /// A session no admission and no control verb has raised yet.
    #[must_use]
    pub fn unraised() -> Self {
        Self {
            epoch: 0,
            last_raise: None,
            closing: None,
            control_pending: false,
            fault: None,
        }
    }

    /// The admission that last raised the epoch; `None` before the first
    /// raise.
    #[must_use]
    pub fn admission(&self) -> Option<&AdmissionId> {
        self.last_raise.as_ref().map(DriveRaise::admission)
    }

    /// Decode the `session_meta` drive columns. The columns hold exactly the
    /// states a raise writes (both backends CHECK it), so any other
    /// combination is corrupt: an unraised epoch names nothing, a raised one
    /// names its admission, and a closing session was raised by its close,
    /// which no execution sealed.
    pub fn from_stored(
        epoch: u64,
        admission: Option<String>,
        root_start: Option<String>,
        closing: Option<super::ControlIntentId>,
        control_pending: bool,
        fault: Option<super::SessionFault>,
    ) -> Result<Self, StoreError> {
        let corrupt = |message: &str| StoreError::StoredDataCorrupt {
            record_kind: "SessionMeta",
            message: message.to_string(),
        };
        let last_raise = match (epoch, admission, root_start) {
            (0, None, None) => None,
            (0, _, _) => {
                return Err(corrupt(
                    "drive_epoch 0 names an admission or a root start marker",
                ));
            }
            (_, None, _) => return Err(corrupt("a raised drive_epoch names no admission")),
            (_, Some(admission), Some(root_start)) => Some(DriveRaise::Sealed {
                admission: AdmissionId::new(admission),
                root_start: RootStartNonce::new(root_start),
            }),
            (_, Some(admission), None) => Some(DriveRaise::Control {
                admission: AdmissionId::new(admission),
            }),
        };
        if closing.is_some() && !matches!(last_raise, Some(DriveRaise::Control { .. })) {
            return Err(corrupt(
                "a closing session's drive epoch was not raised by its close",
            ));
        }
        Ok(Self {
            epoch,
            last_raise,
            closing,
            control_pending,
            fault,
        })
    }
}

/// Decide one seal from the stored epoch (ADR 0105 §2).
///
/// The stored raise is checked first: when this admission is the one whose
/// seal last raised the epoch, the seal already happened, and one admission
/// makes exactly one epoch transition (ADR 0105 L-S3, L-S4). Its start marker
/// then says by whom: the same marker is a retry of the execution that sealed
/// it (a lost reply, whatever epoch the retried body observed) and answers
/// the stored fence without writing; another marker is a fresh execution of
/// a root that already started, which is `ExecutionLost` (L-S8). A control
/// raise sealed no execution, so it answers no seal as a retry. Otherwise a
/// seal observed at the stored epoch raises it by one and stores its marker,
/// and anything else was superseded. A closing session raises nothing: its
/// close already raised the epoch past every admission.
#[must_use]
pub fn decide_drive_epoch_seal(
    session_id: &SessionId,
    stored: &StoredDriveEpoch,
    admission: &AdmissionId,
    observed_epoch: u64,
    root_start: &RootStartNonce,
) -> DriveEpochSealDecision {
    match &stored.last_raise {
        Some(DriveRaise::Sealed {
            admission: sealed,
            root_start: sealed_by,
        }) if sealed == admission => {
            if sealed_by != root_start {
                return DriveEpochSealDecision::Answer(DriveEpochSeal::ExecutionLost);
            }
            return DriveEpochSealDecision::Answer(DriveEpochSeal::Sealed(
                DriveFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone()),
            ));
        }
        Some(DriveRaise::Control { admission: raised }) if raised == admission => {
            return DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded {
                epoch: stored.epoch,
            });
        }
        _ => {}
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
    if fence.epoch() != current.epoch || current.admission() != Some(fence.admission()) {
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
    ///
    /// `hold` names the root the admission runs and the execution that runs
    /// it (FIG-4814). The same transaction that raises the epoch records
    /// that executor on the root, unless the root's admission already
    /// records one, and a seal that would raise the epoch over a root
    /// another engine-held executor holds raises nothing and answers
    /// [`DriveEpochSeal::HeldByAnotherExecutor`] ([`decide_root_hold`]). So
    /// from its first seal to its end a root has one executor, and no other
    /// supersedes that executor's fence. A seal with no `hold` runs no
    /// root: it records nothing and is refused by none.
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
        hold: Option<&RootHold>,
    ) -> Result<DriveEpochSeal, StoreError>;

    /// The session's stored drive epoch.
    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError>;

    /// Record `record` as `session_id`'s fault at `at_ms` (ADR 0109 §9) and
    /// answer the fault that now stands: a session already faulted keeps its
    /// first. `None` when the session has no `session_meta` row.
    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &super::SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<super::SessionFault>, StoreError>;

    /// `session_id`'s standing fault, read by itself.
    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<super::SessionFault>, StoreError>;

    /// The standing session faults after session `after`, in session-id
    /// order, at most `limit`.
    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<super::SessionFault>, StoreError>;

    /// Clear `session_id`'s fault, an operator's verb: the session admits
    /// again. `false` when it had none.
    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError>;
}

/// The fence of the admission that last raised `session_id`'s epoch in
/// `store`, as a writer beside the drive presents it (ADR 0109 §7): a write
/// it fences is refused once any later admission seals. `None` before the
/// session's first seal.
///
/// # Errors
///
/// The store's drive-epoch read failed.
pub async fn current_drive_fence<S: DriveEpochStore + ?Sized>(
    store: &S,
    session_id: &SessionId,
) -> Result<Option<DriveFence>, StoreError> {
    let stored = store.drive_epoch(session_id).await?;
    Ok(stored.admission().map(|admission| {
        DriveFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone())
    }))
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
            .or_insert_with(StoredDriveEpoch::unraised);
        match decide_drive_epoch_seal(session_id, stored, admission, observed_epoch, root_start) {
            DriveEpochSealDecision::Answer(seal) => seal,
            DriveEpochSealDecision::Raise { next } => {
                *stored = StoredDriveEpoch {
                    epoch: next,
                    last_raise: Some(DriveRaise::Sealed {
                        admission: admission.clone(),
                        root_start: root_start.clone(),
                    }),
                    closing: None,
                    control_pending: false,
                    fault: None,
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
            .unwrap_or_else(StoredDriveEpoch::unraised)
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
            .or_insert_with(StoredDriveEpoch::unraised);
        if stored.closing.is_none() {
            *stored = StoredDriveEpoch {
                epoch: stored.epoch.saturating_add(1),
                last_raise: Some(DriveRaise::Control {
                    admission: close_admission(intent),
                }),
                closing: Some(intent),
                control_pending: false,
                fault: None,
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
            last_raise: admission.map(|admission| DriveRaise::Sealed {
                admission: AdmissionId::new(admission),
                root_start: RootStartNonce::new("n"),
            }),
            closing: None,
            control_pending: false,
            fault: None,
        }
    }

    fn nonce() -> RootStartNonce {
        RootStartNonce::new("n")
    }

    #[test]
    fn a_root_another_engine_held_executor_holds_is_never_sealed_over() {
        use super::super::{AdmittedHead, RootExecutor, UnfinishedRoot};
        let acceptor = RootExecutor::Acceptor {
            scope: crate::ExecutionScope::turn("s", "r"),
        };
        let drain = RootExecutor::Inline {
            scope: crate::ExecutionScope::session_operation("s", "drain"),
        };
        let hold = |executor: &RootExecutor| RootHold {
            root: crate::TurnId::from("r"),
            executor: executor.clone(),
        };
        let held = |executor: &RootExecutor, ended| HeldRoot {
            executor: executor.clone(),
            ended,
        };
        let refused = Some(DriveEpochSeal::HeldByAnotherExecutor {
            root: crate::TurnId::from("r"),
            recorded: Box::new(acceptor.clone()),
        });
        // Sealed or admitted, the acceptor's root is closed to the root's
        // own run until it ends, and open to itself and to a drive no
        // engine holds.
        assert_eq!(
            decide_root_hold(
                &hold(&RootExecutor::Root),
                4,
                Some(&held(&acceptor, false)),
                None
            ),
            refused
        );
        let unfinished = UnfinishedRoot {
            root: crate::TurnId::from("r"),
            head: AdmittedHead::Input(crate::InputId::from("i")),
            executor: acceptor.clone(),
        };
        assert_eq!(
            decide_root_hold(&hold(&RootExecutor::Root), 4, None, Some(&unfinished)),
            refused
        );
        for sealer in [&acceptor, &drain] {
            assert_eq!(
                decide_root_hold(
                    &hold(sealer),
                    4,
                    Some(&held(&acceptor, false)),
                    Some(&unfinished)
                ),
                None
            );
        }
        assert_eq!(
            decide_root_hold(&hold(&acceptor), 4, Some(&held(&drain, false)), None),
            None
        );
        // A root that ended under the acceptor is not the run's to seal.
        assert_eq!(
            decide_root_hold(
                &hold(&RootExecutor::Root),
                4,
                Some(&held(&acceptor, true)),
                None
            ),
            Some(DriveEpochSeal::Superseded { epoch: 4 })
        );
    }

    #[test]
    fn a_closing_session_seals_nothing() {
        let session = SessionId::from("s");
        let closing = StoredDriveEpoch {
            epoch: 5,
            last_raise: Some(DriveRaise::Control {
                admission: AdmissionId::new("intent:1"),
            }),
            closing: Some(super::super::ControlIntentId::from_sequence(1)),
            control_pending: false,
            fault: None,
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
            decide_drive_epoch_seal(&session, &stored(4, Some("b")), &admission, 3, &nonce()),
            DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded { epoch: 4 })
        );
    }

    /// F44: a raise that stored no start marker is a control verb's, which
    /// sealed no execution. A seal presenting its admission is never
    /// answered `Sealed`: the old pre-marker branch failed open to a fresh
    /// execution of a root that already started.
    #[test]
    fn a_raise_without_a_start_marker_answers_no_seal_as_sealed() {
        let session = SessionId::from("s");
        let admission = AdmissionId::new("a");
        let control =
            StoredDriveEpoch::from_stored(4, Some("a".to_string()), None, None, false, None)
                .expect("a control raise");
        assert_eq!(
            control.last_raise,
            Some(DriveRaise::Control {
                admission: admission.clone()
            })
        );
        for observed in [3, 4] {
            assert_eq!(
                decide_drive_epoch_seal(&session, &control, &admission, observed, &nonce()),
                DriveEpochSealDecision::Answer(DriveEpochSeal::Superseded { epoch: 4 }),
                "observed epoch {observed}"
            );
        }
    }

    #[test]
    fn the_drive_columns_decode_only_real_drive_states() {
        let intent = super::super::ControlIntentId::from_sequence(1);
        let admission = || Some("a".to_string());
        let marker = || Some("n".to_string());
        assert_eq!(
            StoredDriveEpoch::from_stored(0, None, None, None, false, None).expect("unraised"),
            StoredDriveEpoch::unraised()
        );
        assert_eq!(
            StoredDriveEpoch::from_stored(2, admission(), marker(), None, true, None)
                .expect("sealed")
                .last_raise,
            Some(DriveRaise::Sealed {
                admission: AdmissionId::new("a"),
                root_start: RootStartNonce::new("n"),
            })
        );
        StoredDriveEpoch::from_stored(2, admission(), None, Some(intent), false, None)
            .expect("closing under its close's raise");
        for (case, decoded) in [
            (
                "an unraised epoch naming an admission",
                StoredDriveEpoch::from_stored(0, admission(), None, None, false, None),
            ),
            (
                "an unraised epoch naming a start marker",
                StoredDriveEpoch::from_stored(0, None, marker(), None, false, None),
            ),
            (
                "an unraised epoch naming a seal",
                StoredDriveEpoch::from_stored(0, admission(), marker(), None, false, None),
            ),
            (
                "a raised epoch naming no admission",
                StoredDriveEpoch::from_stored(1, None, None, None, false, None),
            ),
            (
                "a start marker without its admission",
                StoredDriveEpoch::from_stored(1, None, marker(), None, false, None),
            ),
            (
                "a closing session no close raised",
                StoredDriveEpoch::from_stored(0, None, None, Some(intent), false, None),
            ),
            (
                "a closing session an execution sealed",
                StoredDriveEpoch::from_stored(1, admission(), marker(), Some(intent), false, None),
            ),
        ] {
            assert!(
                matches!(decoded, Err(StoreError::StoredDataCorrupt { .. })),
                "{case}: {decoded:?}"
            );
        }
    }
}
