//! An in-memory turn capture segment for store doubles that keep no SQL rows
//! (ADR 0114 §3.2). It decides every operation as the SQL backends do inside
//! their transactions: epochs fence and retract per invocation, a seal is
//! first-writer-wins and fences the whole turn, and a turn's commit publishes
//! its sealed partial and drops the turn's staging.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use lash_sansio::{CaptureBase, CaptureCoverage, StoppedPartial, StoppedPartialId};

use super::capture::{
    CaptureAck, CaptureAttemptReset, CaptureBaseAdvance, CaptureBatch, CaptureFrame,
    CaptureFrameKey, CaptureInvocationKey, CaptureWriterLease, CaptureWriterLeaseRef,
    OpenCaptureWriter, SealTurnCapture, SealedCapture, StoppedPartialRead,
    StoppedPartialReadRequest, TurnCaptureStore,
};
use super::{RuntimeCommit, StoreError};
use crate::{SessionId, TurnAddress, TurnId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterState {
    Live,
    Fenced,
    Retracted,
}

#[derive(Clone, Debug)]
struct StagedFrame {
    key: CaptureFrameKey,
    batch_ordinal: u64,
    frame: CaptureFrame,
}

#[derive(Debug)]
struct CaptureTurn {
    root: TurnId,
    base: CaptureBase,
    next_sequence: u64,
    recovered: bool,
    writers: BTreeMap<(CaptureInvocationKey, u32), WriterState>,
    frames: Vec<StagedFrame>,
}

impl CaptureTurn {
    fn new(root: TurnId) -> Self {
        Self {
            root,
            base: CaptureBase(0),
            next_sequence: 1,
            recovered: false,
            writers: BTreeMap::new(),
            frames: Vec::new(),
        }
    }

    fn latest(&self, invocation: &CaptureInvocationKey) -> Option<(u32, WriterState)> {
        self.writers
            .iter()
            .filter(|((key, _), _)| key == invocation)
            .map(|((_, epoch), state)| (*epoch, *state))
            .max_by_key(|(epoch, _)| *epoch)
    }

    fn state(&self, invocation: &CaptureInvocationKey, epoch: u32) -> Option<WriterState> {
        self.writers.get(&(invocation.clone(), epoch)).copied()
    }

    /// Frames of `invocation` under the current base whose epoch was never
    /// retracted: the prefix a successor inherits.
    fn inherited(&self, invocation: &CaptureInvocationKey) -> Vec<(CaptureFrameKey, CaptureFrame)> {
        self.frames
            .iter()
            .filter(|staged| {
                staged.key.invocation == *invocation
                    && staged.key.base == self.base
                    && self.state(invocation, staged.key.attempt_epoch)
                        != Some(WriterState::Retracted)
            })
            .map(|staged| (staged.key.clone(), staged.frame.clone()))
            .collect()
    }

    fn lease(
        &self,
        turn: &TurnAddress,
        invocation: &CaptureInvocationKey,
        epoch: u32,
    ) -> CaptureWriterLease {
        CaptureWriterLease {
            turn: turn.clone(),
            invocation: invocation.clone(),
            attempt_epoch: epoch,
            base: self.base,
            inherited: self.inherited(invocation),
        }
    }

    fn require_writer(&self, lease: &CaptureWriterLeaseRef) -> Result<(), StoreError> {
        let latest = self.latest(&lease.invocation);
        let current_epoch = latest.map_or(0, |(epoch, _)| epoch);
        if latest != Some((lease.attempt_epoch, WriterState::Live)) {
            return Err(StoreError::CaptureWriterFenced {
                session_id: lease.turn.session_id.clone(),
                turn_id: lease.turn.turn_id.clone(),
                invocation: lease.invocation.0.clone(),
                attempt_epoch: lease.attempt_epoch,
                current_epoch,
            });
        }
        if self.base != lease.base {
            return Err(StoreError::CaptureBaseStale {
                session_id: lease.turn.session_id.clone(),
                turn_id: lease.turn.turn_id.clone(),
                offered: lease.base.0,
                current: self.base.0,
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
struct SealedRow {
    partial: StoppedPartial,
    committed: bool,
}

#[derive(Debug, Default)]
struct InMemoryCaptures {
    turns: HashMap<(SessionId, TurnId), CaptureTurn>,
    partials: HashMap<(SessionId, TurnId), SealedRow>,
    committed_turns: BTreeSet<(SessionId, TurnId)>,
}

impl InMemoryCaptures {
    fn reject_sealed(&self, turn: &TurnAddress) -> Result<(), StoreError> {
        match self
            .partials
            .get(&(turn.session_id.clone(), turn.turn_id.clone()))
        {
            Some(row) => Err(StoreError::CaptureSealed {
                session_id: turn.session_id.clone(),
                turn_id: turn.turn_id.clone(),
                sealed_through: row.partial.id.sealed_through,
            }),
            None => Ok(()),
        }
    }

    fn turn_mut(&mut self, turn: &TurnAddress) -> Option<&mut CaptureTurn> {
        self.turns
            .get_mut(&(turn.session_id.clone(), turn.turn_id.clone()))
    }
}

/// The turn capture segment of a store double that keeps no SQL rows.
#[derive(Debug, Default)]
pub struct InMemoryTurnCapture {
    state: std::sync::Mutex<InMemoryCaptures>,
}

impl InMemoryTurnCapture {
    fn lock(&self) -> std::sync::MutexGuard<'_, InMemoryCaptures> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The commit's capture block (ADR 0114 §3.3), for a double to run inside
    /// its `commit_runtime_state`: a named partial must be the turn's seal,
    /// and the turn's staging goes with the commit.
    pub fn commit(&self, commit: &RuntimeCommit) -> Result<(), StoreError> {
        let Some(turn) = commit.turn_commit.operation.turn_id() else {
            return Ok(());
        };
        let session = &commit.session_id;
        let key = (session.clone(), turn.clone());
        let mut state = self.lock();
        match (&commit.stopped_partial, state.partials.get_mut(&key)) {
            (Some(reference), Some(row)) => {
                if row.partial.id != reference.id || row.partial.digest != reference.digest {
                    return Err(StoreError::StoppedPartialConflict {
                        session_id: session.clone(),
                        turn_id: turn.clone(),
                        existing: Box::new(row.partial.digest),
                        offered: Box::new(reference.digest),
                    });
                }
                row.committed = true;
            }
            (Some(_), None) => {
                return Err(StoreError::StoppedPartialNotSealed {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                });
            }
            (None, Some(row)) => {
                return Err(StoreError::StoppedPartialConflict {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                    existing: Box::new(row.partial.digest),
                    offered: Box::new(row.partial.digest),
                });
            }
            (None, None) => {}
        }
        state.turns.remove(&key);
        state.committed_turns.insert(key);
        Ok(())
    }
}

#[async_trait::async_trait]
impl TurnCaptureStore for InMemoryTurnCapture {
    async fn open_capture_writer(
        &self,
        request: &OpenCaptureWriter,
    ) -> Result<CaptureWriterLease, StoreError> {
        let mut state = self.lock();
        state.reject_sealed(&request.turn)?;
        let turn = state
            .turns
            .entry((
                request.turn.session_id.clone(),
                request.turn.turn_id.clone(),
            ))
            .or_insert_with(|| CaptureTurn::new(request.root.clone()));
        if turn.root != request.root {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "TurnCapture",
                message: "root mismatch".to_string(),
            });
        }
        let epoch = match turn.latest(&request.invocation) {
            Some((previous, _)) => {
                if !turn.inherited(&request.invocation).is_empty() {
                    turn.recovered = true;
                }
                for ((invocation, _), writer) in &mut turn.writers {
                    if *invocation == request.invocation && *writer == WriterState::Live {
                        *writer = WriterState::Fenced;
                    }
                }
                previous
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Backend("capture epoch overflow".into()))?
            }
            None => 0,
        };
        turn.writers
            .insert((request.invocation.clone(), epoch), WriterState::Live);
        Ok(turn.lease(&request.turn, &request.invocation, epoch))
    }

    async fn append_capture_batch(&self, batch: &CaptureBatch) -> Result<CaptureAck, StoreError> {
        batch.validate_bounds()?;
        let lease = &batch.lease;
        let mut state = self.lock();
        state.reject_sealed(&lease.turn)?;
        let Some(turn) = state.turn_mut(&lease.turn) else {
            return Err(StoreError::CaptureWriterFenced {
                session_id: lease.turn.session_id.clone(),
                turn_id: lease.turn.turn_id.clone(),
                invocation: lease.invocation.0.clone(),
                attempt_epoch: lease.attempt_epoch,
                current_epoch: 0,
            });
        };
        turn.require_writer(lease)?;
        let existing: Vec<&StagedFrame> = turn
            .frames
            .iter()
            .filter(|staged| {
                staged.key.invocation == lease.invocation
                    && staged.key.attempt_epoch == lease.attempt_epoch
                    && staged.batch_ordinal == batch.batch_ordinal
            })
            .collect();
        if let (Some(first), Some(last)) = (existing.first(), existing.last()) {
            if existing
                .iter()
                .map(|staged| &staged.frame)
                .ne(batch.frames.iter())
            {
                return Err(StoreError::CaptureBatchConflict {
                    session_id: lease.turn.session_id.clone(),
                    turn_id: lease.turn.turn_id.clone(),
                    invocation: lease.invocation.0.clone(),
                    attempt_epoch: lease.attempt_epoch,
                    batch_ordinal: batch.batch_ordinal,
                });
            }
            return Ok(CaptureAck {
                first_sequence: first.key.sequence,
                last_sequence: last.key.sequence,
            });
        }
        let first = turn.next_sequence;
        for (index, frame) in batch.frames.iter().enumerate() {
            turn.frames.push(StagedFrame {
                key: CaptureFrameKey {
                    turn: lease.turn.clone(),
                    base: lease.base,
                    invocation: lease.invocation.clone(),
                    attempt_epoch: lease.attempt_epoch,
                    sequence: first + index as u64,
                },
                batch_ordinal: batch.batch_ordinal,
                frame: frame.clone(),
            });
        }
        turn.next_sequence = first + batch.frames.len() as u64;
        Ok(CaptureAck {
            first_sequence: first,
            last_sequence: turn.next_sequence - 1,
        })
    }

    async fn persist_attempt_reset(
        &self,
        reset: &CaptureAttemptReset,
    ) -> Result<CaptureWriterLease, StoreError> {
        let lease = &reset.lease;
        let mut state = self.lock();
        state.reject_sealed(&lease.turn)?;
        let Some(turn) = state.turn_mut(&lease.turn) else {
            return Err(StoreError::CaptureWriterFenced {
                session_id: lease.turn.session_id.clone(),
                turn_id: lease.turn.turn_id.clone(),
                invocation: lease.invocation.0.clone(),
                attempt_epoch: lease.attempt_epoch,
                current_epoch: 0,
            });
        };
        let prior = turn.state(&lease.invocation, lease.attempt_epoch);
        let latest = turn.latest(&lease.invocation);
        // A successor retracts the inherited epoch it fenced, and keeps its
        // own live lease.
        if let Some((latest_epoch, WriterState::Live)) = latest
            && latest_epoch > lease.attempt_epoch
            && matches!(prior, Some(WriterState::Fenced | WriterState::Retracted))
        {
            if turn.base != lease.base {
                return Err(StoreError::CaptureBaseStale {
                    session_id: lease.turn.session_id.clone(),
                    turn_id: lease.turn.turn_id.clone(),
                    offered: lease.base.0,
                    current: turn.base.0,
                });
            }
            turn.writers.insert(
                (lease.invocation.clone(), lease.attempt_epoch),
                WriterState::Retracted,
            );
            return Ok(turn.lease(&lease.turn, &lease.invocation, latest_epoch));
        }
        let next = lease
            .attempt_epoch
            .checked_add(1)
            .ok_or_else(|| StoreError::Backend("capture epoch overflow".into()))?;
        // A replayed reset answers the lease it minted.
        if latest.is_some_and(|(epoch, _)| epoch == next) && prior == Some(WriterState::Retracted) {
            return Ok(turn.lease(&lease.turn, &lease.invocation, next));
        }
        turn.require_writer(lease)?;
        turn.writers.insert(
            (lease.invocation.clone(), lease.attempt_epoch),
            WriterState::Retracted,
        );
        turn.writers
            .insert((lease.invocation.clone(), next), WriterState::Live);
        Ok(turn.lease(&lease.turn, &lease.invocation, next))
    }

    async fn advance_capture_base(&self, advance: &CaptureBaseAdvance) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.reject_sealed(&advance.turn)?;
        let Some(turn) = state.turn_mut(&advance.turn) else {
            return Err(StoreError::CaptureBaseStale {
                session_id: advance.turn.session_id.clone(),
                turn_id: advance.turn.turn_id.clone(),
                offered: advance.to.0,
                current: 0,
            });
        };
        if advance.to == turn.base {
            return Ok(());
        }
        if Some(advance.to.0) != turn.base.0.checked_add(1) {
            return Err(StoreError::CaptureBaseStale {
                session_id: advance.turn.session_id.clone(),
                turn_id: advance.turn.turn_id.clone(),
                offered: advance.to.0,
                current: turn.base.0,
            });
        }
        turn.base = advance.to;
        turn.frames.retain(|staged| staged.key.base >= advance.to);
        Ok(())
    }

    async fn seal_turn_capture(
        &self,
        request: &SealTurnCapture,
    ) -> Result<SealedCapture, StoreError> {
        let session = &request.turn.session_id;
        let turn_id = &request.turn.turn_id;
        let key = (session.clone(), turn_id.clone());
        let mut state = self.lock();
        if let Some(row) = state.partials.get(&key) {
            return Ok(if row.committed {
                SealedCapture::Committed(row.partial.clone())
            } else {
                SealedCapture::Sealed(row.partial.clone())
            });
        }
        let turn = state
            .turns
            .entry(key.clone())
            .or_insert_with(|| CaptureTurn::new(request.root.clone()));
        if turn.root != request.root {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "TurnCapture",
                message: "root mismatch".to_string(),
            });
        }
        let through = turn.next_sequence.saturating_sub(1);
        if let Some(recorded) = request.recorded_watermark
            && through < recorded
        {
            return Err(StoreError::CaptureSealBelowWatermark {
                session_id: session.clone(),
                turn_id: turn_id.clone(),
                sealed_through: through,
                recorded,
            });
        }
        for writer in turn.writers.values_mut() {
            if *writer == WriterState::Live {
                *writer = WriterState::Fenced;
            }
        }
        let retracted = turn
            .writers
            .iter()
            .filter(|(_, writer)| **writer == WriterState::Retracted)
            .map(|(key, _)| key.clone())
            .collect::<BTreeSet<_>>();
        let frames = turn
            .frames
            .iter()
            .map(|staged| (staged.key.clone(), staged.frame.clone()))
            .collect::<Vec<_>>();
        let coverage = if turn.recovered {
            CaptureCoverage::AcknowledgedPrefix
        } else {
            CaptureCoverage::Complete
        };
        let partial = crate::capture::reduce_capture(
            StoppedPartialId {
                session_id: session.clone(),
                root: request.root.clone(),
                turn_id: turn_id.clone(),
                base: turn.base,
                sealed_through: through,
            },
            request.reason.clone(),
            turn.recovered,
            coverage,
            &frames,
            &retracted,
        )
        .map_err(|violation| StoreError::CaptureCorrupt {
            session_id: session.clone(),
            turn_id: turn_id.clone(),
            violation,
        })?;
        state.partials.insert(
            key,
            SealedRow {
                partial: partial.clone(),
                committed: false,
            },
        );
        Ok(SealedCapture::Sealed(partial))
    }

    async fn read_stopped_partial(
        &self,
        request: &StoppedPartialReadRequest,
    ) -> Result<StoppedPartialRead, StoreError> {
        let state = self.lock();
        let sealed = state.partials.iter().find(|((session, turn), row)| {
            *session == request.session_id
                && (*turn == request.turn || row.partial.id.root == request.turn)
        });
        if let Some((_, row)) = sealed {
            return Ok(if row.committed {
                StoppedPartialRead::Available(row.partial.clone())
            } else {
                StoppedPartialRead::Pending
            });
        }
        let key = (request.session_id.clone(), request.turn.clone());
        if state.turns.contains_key(&key) {
            return Ok(StoppedPartialRead::Pending);
        }
        Ok(if state.committed_turns.contains(&key) {
            StoppedPartialRead::NotStopped
        } else {
            StoppedPartialRead::Unknown
        })
    }
}
