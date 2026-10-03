//! The shift fence and its store half (ADR 0105 §2, B3).
//!
//! [`ShiftFence`] and [`AdmissionId`] live here, below the engine contract,
//! because the stores check the fence: `lash_core::engine` re-exports them
//! unchanged. The shift epoch they fence is a monotonic counter on the
//! session's `session_meta` row, next to the id of the admission that last
//! raised it. Only [`ShiftEpochStore::seal_shift_epoch`] raises it, with a
//! compare-and-set, so replay cannot mint ownership and nothing expires it.
//!
//! The same row keeps the admitted run's start marker (ADR 0105 §2, L-S8):
//! the [`RunStartNonce`] the execution that sealed the admission drew in its
//! own journal. A retry of that execution replays the same nonce and finds
//! its own seal; a fresh execution of the same admission (its journal gone)
//! draws another, and the seal answers it `ExecutionLost` instead of letting
//! the run run twice.

use serde::{Deserialize, Serialize};

use super::StoreError;
use crate::SessionId;

/// The authority of one shift over one session.
///
/// It has no public constructor. A fence comes from exactly three places:
/// the store's own seal ([`ShiftEpochStore::seal_shift_epoch`]); the store's
/// read of the current fence ([`current_shift_fence`]),
/// which a writer beside the shift presents so that it never writes over a
/// later admission; and serde decoding of a recorded step that carries one: a
/// `SealVerdict::Sealed` from the shift's journal, or the fence an
/// administrative compaction records with its base (FIG-4134).
/// `Deserialize` exists only for those recorded paths; nothing else may
/// decode a fence. A decoded fence still authorizes nothing by itself: every
/// fenced store operation checks its epoch *and* admission against the
/// session's `session_meta` row in its own transaction. It is never part of
/// an envelope hash (ADR 0105 law L-S12).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShiftFence {
    session: SessionId,
    epoch: u64,
    admission: AdmissionId,
}

impl ShiftFence {
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
    /// [`sealed_shift_fence`](crate::store_backend_support::sealed_shift_fence).
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

/// The start marker of one execution of an admitted run (ADR 0105 §2,
/// L-S8, FIG-3815).
///
/// The run draws it as its first recorded step, in its own journal, before
/// it seals its admission. Every retry of that execution replays the same
/// nonce; an execution that cannot read that journal draws a new one. The
/// seal sets it with the admission and compares it on every later seal of
/// the same admission.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunStartNonce(String);

impl RunStartNonce {
    pub fn new(nonce: impl Into<String>) -> Self {
        Self(nonce.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A session head a shift names: the state generation, the head revision, the
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
pub enum ShiftEpochSeal {
    /// The epoch was raised to the fence's epoch under this admission, now or
    /// by an earlier invocation of the same seal.
    Sealed(ShiftFence),
    /// Another admission raised the epoch past the observed one.
    Superseded { epoch: u64 },
    /// This admission was sealed by another execution of its run, which
    /// drew another start marker: this execution cannot read what that one
    /// did, so it must not run the run (L-S8).
    ExecutionLost,
    /// `run` is held by `recorded`, another executor an engine holds an execution
    /// for, and has not ended (FIG-4814): the epoch stays where that
    /// executor's seal raised it, and the sealer waits for the run's end.
    HeldByAnotherExecutor {
        run: crate::TurnId,
        recorded: Box<super::RunExecutor>,
    },
}

/// The run a seal's admission runs and the execution that runs it
/// ([`ShiftEpochStore::seal_shift_epoch`], FIG-4814).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHold {
    pub run: crate::TurnId,
    pub executor: super::RunExecutor,
}

/// What the store holds for the run a seal names, read in the seal's
/// transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRun {
    /// The executor recorded for the run: its admission's, else the one
    /// the last seal recorded for it.
    pub executor: super::RunExecutor,
    /// The run has terminal evidence.
    pub ended: bool,
}

/// Decide whether a seal that would raise the epoch at `stored_epoch` may,
/// given who holds the runs it would run beside (FIG-4814). `None` lets it
/// raise.
///
/// A run's executor is recorded by the seal that first raises the epoch
/// for it, and by its admission from then on. Another executor an engine
/// holds a run for ([`RunExecutor::excludes`](super::RunExecutor::excludes))
/// never seals over it: not while the session's unfinished run is that
/// executor's, and not for a run that executor sealed and has yet to
/// admit. The refused sealer raises nothing, so the recorded executor's
/// fence stands until its run ends. A run that already ended under
/// another executor is not this admission's to run: the seal answers
/// superseded.
#[must_use]
pub fn decide_run_hold(
    hold: &RunHold,
    stored_epoch: u64,
    held: Option<&HeldRun>,
    unfinished: Option<&super::UnfinishedRun>,
    follow_on: Option<&super::PendingFollowOn>,
) -> Option<ShiftEpochSeal> {
    if let Some(unfinished) = unfinished
        && unfinished.executor.excludes(&hold.executor)
        && !follow_on
            .is_some_and(|owed| owed.hands_off_root(&unfinished.executor, &unfinished.run, hold))
    {
        return Some(ShiftEpochSeal::HeldByAnotherExecutor {
            run: unfinished.run.clone(),
            recorded: Box::new(unfinished.executor.clone()),
        });
    }
    let held = held.filter(|held| held.executor.excludes(&hold.executor))?;
    Some(if held.ended {
        ShiftEpochSeal::Superseded {
            epoch: stored_epoch,
        }
    } else {
        ShiftEpochSeal::HeldByAnotherExecutor {
            run: hold.run.clone(),
            recorded: Box::new(held.executor.clone()),
        }
    })
}

/// What last raised a session's shift epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShiftRaise {
    /// An execution of an admitted run sealed `admission`, drawing
    /// `run_start` as its start marker.
    Sealed {
        admission: AdmissionId,
        run_start: RunStartNonce,
    },
    /// A control verb (a cancel, a fork, a session close) raised the epoch
    /// past every sealed fence under `admission`. No execution sealed it, so
    /// it has no start marker.
    Control { admission: AdmissionId },
}

impl ShiftRaise {
    /// The admission the raise recorded.
    #[must_use]
    pub fn admission(&self) -> &AdmissionId {
        match self {
            Self::Sealed { admission, .. } | Self::Control { admission } => admission,
        }
    }
}

/// The durable shift epoch of a session as its `session_meta` row stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredShiftEpoch {
    pub epoch: u64,
    /// What last raised the epoch; `None` exactly while the epoch is zero.
    pub last_raise: Option<ShiftRaise>,
    /// The `CloseSession` intent the session is closing under (FIG-3600 S7):
    /// a closing session admits nothing and seals nothing.
    pub closing: Option<super::ControlIntentId>,
    /// A cancel or fork still owes its engine half — the release of the
    /// run's old execution — and will retry it: it is pending, or failed and
    /// retryable. The session admits nothing until the release lands. A verb
    /// the engine refused for good owes nothing more and holds nothing back:
    /// its store half already ended the run and raised the epoch past the
    /// old execution's fence, so that execution can neither commit nor park.
    pub control_pending: bool,
    /// The session's standing fault (ADR 0109 §9): corrupt stored data met
    /// after a run's answer was published. Every shift is refused with it
    /// until an operator clears it.
    pub fault: Option<super::SessionFault>,
}

impl StoredShiftEpoch {
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
        self.last_raise.as_ref().map(ShiftRaise::admission)
    }

    /// Decode the `session_meta` shift columns. The columns hold exactly the
    /// states a raise writes (both backends CHECK it), so any other
    /// combination is corrupt: an unraised epoch names nothing, a raised one
    /// names its admission, and a closing session was raised by its close,
    /// which no execution sealed.
    pub fn from_stored(
        epoch: u64,
        admission: Option<String>,
        run_start: Option<String>,
        closing: Option<super::ControlIntentId>,
        control_pending: bool,
        fault: Option<super::SessionFault>,
    ) -> Result<Self, StoreError> {
        let corrupt = |message: &str| StoreError::StoredDataCorrupt {
            record_kind: "SessionMeta",
            message: message.to_string(),
        };
        let last_raise = match (epoch, admission, run_start) {
            (0, None, None) => None,
            (0, _, _) => {
                return Err(corrupt(
                    "shift_epoch 0 names an admission or a run start marker",
                ));
            }
            (_, None, _) => return Err(corrupt("a raised shift_epoch names no admission")),
            (_, Some(admission), Some(run_start)) => Some(ShiftRaise::Sealed {
                admission: AdmissionId::new(admission),
                run_start: RunStartNonce::new(run_start),
            }),
            (_, Some(admission), None) => Some(ShiftRaise::Control {
                admission: AdmissionId::new(admission),
            }),
        };
        if closing.is_some() && !matches!(last_raise, Some(ShiftRaise::Control { .. })) {
            return Err(corrupt(
                "a closing session's shift epoch was not raised by its close",
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
/// a run that already started, which is `ExecutionLost` (L-S8). A control
/// raise sealed no execution, so it answers no seal as a retry. Otherwise a
/// seal observed at the stored epoch raises it by one and stores its marker,
/// and anything else was superseded. A closing session raises nothing: its
/// close already raised the epoch past every admission.
#[must_use]
pub fn decide_shift_epoch_seal(
    session_id: &SessionId,
    stored: &StoredShiftEpoch,
    admission: &AdmissionId,
    observed_epoch: u64,
    run_start: &RunStartNonce,
) -> ShiftEpochSealDecision {
    match &stored.last_raise {
        Some(ShiftRaise::Sealed {
            admission: sealed,
            run_start: sealed_by,
        }) if sealed == admission => {
            if sealed_by != run_start {
                return ShiftEpochSealDecision::Answer(ShiftEpochSeal::ExecutionLost);
            }
            return ShiftEpochSealDecision::Answer(ShiftEpochSeal::Sealed(
                ShiftFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone()),
            ));
        }
        Some(ShiftRaise::Control { admission: raised }) if raised == admission => {
            return ShiftEpochSealDecision::Answer(ShiftEpochSeal::Superseded {
                epoch: stored.epoch,
            });
        }
        _ => {}
    }
    if stored.epoch == observed_epoch && stored.closing.is_none() && !stored.control_pending {
        return ShiftEpochSealDecision::Raise {
            next: observed_epoch.saturating_add(1),
        };
    }
    ShiftEpochSealDecision::Answer(ShiftEpochSeal::Superseded {
        epoch: stored.epoch,
    })
}

/// What a backend does for one seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShiftEpochSealDecision {
    /// Compare-and-set the epoch from the observed value to `next`, recording
    /// the admission.
    Raise { next: u64 },
    /// Answer without writing.
    Answer(ShiftEpochSeal),
}

/// Refuse `fence` unless it names `session_id` and is exactly the session's
/// current shift fence: the stored epoch *and* the admission that raised it.
/// Every fenced ingress operation calls it inside its own transaction.
pub fn require_current_shift_fence(
    session_id: &SessionId,
    fence: &ShiftFence,
    current: &StoredShiftEpoch,
) -> Result<(), StoreError> {
    if fence.session() != session_id {
        return Err(StoreError::ShiftFenceSessionMismatch {
            session_id: session_id.clone(),
            fence_session_id: fence.session().clone(),
        });
    }
    if fence.epoch() != current.epoch || current.admission() != Some(fence.admission()) {
        return Err(StoreError::StaleShiftFence {
            session_id: session_id.clone(),
            fence_epoch: fence.epoch(),
            current_epoch: current.epoch,
        });
    }
    Ok(())
}

/// The storage half of shift admission: read and raise a session's shift
/// epoch. It holds no engine logic: the engine's seal step calls it.
#[async_trait::async_trait]
pub trait ShiftEpochStore: Send + Sync {
    /// Compare-and-set the session's shift epoch from `observed_epoch` to the
    /// next value under `admission`, storing `run_start` with it; idempotent
    /// per admission and start marker ([`decide_shift_epoch_seal`]).
    ///
    /// `hold` names the run the admission runs and the execution that runs
    /// it (FIG-4814). The same transaction that raises the epoch records
    /// that executor on the run, unless the run's admission already
    /// records one, and a seal that would raise the epoch over a run
    /// another engine-held executor holds raises nothing and answers
    /// [`ShiftEpochSeal::HeldByAnotherExecutor`] ([`decide_run_hold`]). So
    /// from its first seal to its end a run has one executor, and no other
    /// supersedes that executor's fence. A seal with no `hold` runs no
    /// run: it records nothing and is refused by none.
    async fn seal_shift_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        run_start: &RunStartNonce,
        hold: Option<&RunHold>,
    ) -> Result<ShiftEpochSeal, StoreError>;

    /// The session's stored shift epoch.
    async fn shift_epoch(&self, session_id: &SessionId) -> Result<StoredShiftEpoch, StoreError>;

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
/// `store`, as a writer beside the shift presents it (ADR 0109 §7): a write
/// it fences is refused once any later admission seals. `None` before the
/// session's first seal.
///
/// # Errors
///
/// The store's shift-epoch read failed.
pub async fn current_shift_fence<S: ShiftEpochStore + ?Sized>(
    store: &S,
    session_id: &SessionId,
) -> Result<Option<ShiftFence>, StoreError> {
    let stored = store.shift_epoch(session_id).await?;
    Ok(stored.admission().map(|admission| {
        ShiftFence::sealed_by_store(session_id.clone(), stored.epoch, admission.clone())
    }))
}

/// A shift-epoch ledger held in memory, for store doubles that keep no
/// `session_meta` row. It decides every seal with
/// [`decide_shift_epoch_seal`], exactly as a SQL backend does inside its
/// transaction.
#[derive(Debug, Default)]
pub struct InMemoryShiftEpochs {
    epochs: std::sync::Mutex<std::collections::BTreeMap<SessionId, StoredShiftEpoch>>,
}

impl InMemoryShiftEpochs {
    /// [`ShiftEpochStore::seal_shift_epoch`] over this ledger.
    pub fn seal(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        run_start: &RunStartNonce,
    ) -> ShiftEpochSeal {
        let mut epochs = self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = epochs
            .entry(session_id.clone())
            .or_insert_with(StoredShiftEpoch::unraised);
        match decide_shift_epoch_seal(session_id, stored, admission, observed_epoch, run_start) {
            ShiftEpochSealDecision::Answer(seal) => seal,
            ShiftEpochSealDecision::Raise { next } => {
                *stored = StoredShiftEpoch {
                    epoch: next,
                    last_raise: Some(ShiftRaise::Sealed {
                        admission: admission.clone(),
                        run_start: run_start.clone(),
                    }),
                    closing: None,
                    control_pending: false,
                    fault: None,
                };
                ShiftEpochSeal::Sealed(ShiftFence::sealed_by_store(
                    session_id.clone(),
                    next,
                    admission.clone(),
                ))
            }
        }
    }

    /// [`ShiftEpochStore::shift_epoch`] over this ledger.
    pub fn epoch(&self, session_id: &SessionId) -> StoredShiftEpoch {
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
            .unwrap_or_else(StoredShiftEpoch::unraised)
    }

    /// Close `session_id` under `intent`: the shift-epoch half of
    /// [`ControlIntentStore::begin_session_close`](super::ControlIntentStore::begin_session_close).
    /// A session already closing keeps its first intent.
    pub fn close(&self, session_id: &SessionId, intent: super::ControlIntentId) {
        let mut epochs = self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = epochs
            .entry(session_id.clone())
            .or_insert_with(StoredShiftEpoch::unraised);
        if stored.closing.is_none() {
            *stored = StoredShiftEpoch {
                epoch: stored.epoch.saturating_add(1),
                last_raise: Some(ShiftRaise::Control {
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
/// shift epoch: `intent:{id}`.
#[must_use]
pub fn close_admission(intent: super::ControlIntentId) -> AdmissionId {
    AdmissionId::new(format!("intent:{intent}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shift_columns_decode_only_real_shift_states() {
        let intent = super::super::ControlIntentId::from_sequence(1);
        let admission = || Some("a".to_string());
        let marker = || Some("n".to_string());
        assert_eq!(
            StoredShiftEpoch::from_stored(0, None, None, None, false, None).expect("unraised"),
            StoredShiftEpoch::unraised()
        );
        assert_eq!(
            StoredShiftEpoch::from_stored(2, admission(), marker(), None, true, None)
                .expect("sealed")
                .last_raise,
            Some(ShiftRaise::Sealed {
                admission: AdmissionId::new("a"),
                run_start: RunStartNonce::new("n"),
            })
        );
        StoredShiftEpoch::from_stored(2, admission(), None, Some(intent), false, None)
            .expect("closing under its close's raise");
        for (case, decoded) in [
            (
                "an unraised epoch naming an admission",
                StoredShiftEpoch::from_stored(0, admission(), None, None, false, None),
            ),
            (
                "an unraised epoch naming a start marker",
                StoredShiftEpoch::from_stored(0, None, marker(), None, false, None),
            ),
            (
                "an unraised epoch naming a seal",
                StoredShiftEpoch::from_stored(0, admission(), marker(), None, false, None),
            ),
            (
                "a raised epoch naming no admission",
                StoredShiftEpoch::from_stored(1, None, None, None, false, None),
            ),
            (
                "a start marker without its admission",
                StoredShiftEpoch::from_stored(1, None, marker(), None, false, None),
            ),
            (
                "a closing session no close raised",
                StoredShiftEpoch::from_stored(0, None, None, Some(intent), false, None),
            ),
            (
                "a closing session an execution sealed",
                StoredShiftEpoch::from_stored(1, admission(), marker(), Some(intent), false, None),
            ),
        ] {
            assert!(
                matches!(decoded, Err(StoreError::StoredDataCorrupt { .. })),
                "{case}: {decoded:?}"
            );
        }
    }
}
