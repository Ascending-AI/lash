//! The retained answer of a root's atomic admission.

use super::*;
use crate::{SessionId, TurnId};
use serde::{Deserialize, Serialize};

/// What an admitted run executes. Decided by admission and recorded with it,
/// so the run's execution never re-reads the store to learn its own shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "work", rename_all = "snake_case")]
pub enum AdmittedWork {
    /// The prefix of accepted next-turn input headed by `head`.
    Input { head: crate::InputId },
    /// The prefix of ready queued work headed by `head`: a run like an
    /// input run, admitted and executed the same way (FIG-3927).
    Queued { head: crate::BatchId },
    /// The session's open command run, applied at this boundary before any
    /// turn-lane work (ADR 0101 §4). It admits no turn: the run names the
    /// application and ends when the command lane is empty. `head` is the
    /// `enqueue_seq` of the leading open command the admission saw, so a
    /// later admission naming the same head shows the lane made no progress.
    Commands { head: u64 },
    /// A tool-bearing host operation at the head of the command lane: a
    /// host's plugin task, executed as its own logical Run (K8, binding Q2),
    /// named by the operation ([`OperationRun::run_id`]) so every admission
    /// of it names the same run. The run's invocation owns every effect the
    /// task issues until the task returned and its owned work drained.
    ///
    /// [`OperationRun::run_id`]: lash_core_store::tool_run::OperationRun::run_id
    Operation { operation: crate::BatchId },
    /// The follow-on the session head owes (ADR 0101 §3): its recovery, as
    /// recovery number `attempts + 1`. The recorded count is what the
    /// recovery raises from, so a redrive of the run never raises it twice.
    FollowOn { follow_on: TurnId, attempts: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShiftAdmissionSelection {
    pub run: TurnId,
    pub work: AdmittedWork,
    pub observed_epoch: u64,
}

#[derive(Clone, Debug)]
pub struct ShiftAdmissionPreparation {
    pub epoch: StoredShiftEpoch,
    pub park: Option<TurnPark>,
    pub head: Option<SessionHeadMeta>,
    pub selection: Option<ShiftAdmissionSelection>,
    pub prospective_fence: ShiftFence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShiftAdmissionReceipt {
    pub selection: ShiftAdmissionSelection,
    pub run_start: RunStartNonce,
    pub seal: ShiftEpochSeal,
    pub cancel_intent: crate::TurnCancelIntentSnapshot,
    pub run_admission: Option<RunAdmissionAnswer>,
}

#[derive(Clone, Debug)]
pub struct ShiftAdmissionWrite {
    pub session_id: SessionId,
    pub admission: AdmissionId,
    pub run_start: RunStartNonce,
    pub executor: RunExecutor,
    pub preparation: ShiftAdmissionPreparation,
    pub run: Option<PreparedRunAdmission>,
}

impl ShiftAdmissionWrite {
    /// The trace proposal must name exactly the root selected for this seal.
    pub fn validate(&self) -> Result<(), StoreError> {
        let Some(selection) = &self.preparation.selection else {
            return Err(StoreError::Backend(
                "root admission lacks a selection".into(),
            ));
        };
        let expected_head = match &selection.work {
            AdmittedWork::Input { head } => Some(AdmittedHead::Input(head.clone())),
            AdmittedWork::Queued { head } => Some(AdmittedHead::Batch(head.clone())),
            _ => None,
        };
        let fence = &self.preparation.prospective_fence;
        let valid = fence.session() == self.session_id
            && fence.admission() == &self.admission
            && self.preparation.epoch.epoch.checked_add(1) == Some(fence.epoch())
            && selection.observed_epoch == self.preparation.epoch.epoch
            && match (&self.run, expected_head) {
                (Some(run), Some(head)) => {
                    run.request.fence == *fence
                        && run.request.run == selection.run
                        && run.request.head == head
                        && run.request.executor == self.executor
                        && run.request.unsealed_epoch == Some(selection.observed_epoch)
                }
                (None, None) => true,
                _ => false,
            };
        if valid {
            Ok(())
        } else {
            Err(StoreError::PreparedRunAdmissionStale {
                session_id: self.session_id.clone(),
                run: selection.run.clone(),
            })
        }
    }
}

/// Selection is repeated under the write transaction before the seal. The
/// prepared composition separately fences the payloads and retained base.
pub struct ShiftAdmissionQueue<'a> {
    pub follow_on: Option<&'a PendingFollowOn>,
    pub unfinished: Option<&'a UnfinishedRun>,
    pub command_seq: Option<u64>,
    pub queued: &'a [crate::QueuedWorkBatch],
    pub inputs: &'a [crate::PendingTurnInputRead],
    pub bound_head: Option<TurnId>,
}

pub fn select_shift_work(
    session: &SessionId,
    admission: &AdmissionId,
    executor: &RunExecutor,
    queue: ShiftAdmissionQueue<'_>,
) -> Result<Option<(TurnId, AdmittedWork)>, StoreError> {
    let ShiftAdmissionQueue {
        follow_on,
        unfinished,
        command_seq,
        queued,
        inputs,
        bound_head,
    } = queue;
    let held_elsewhere = || unfinished.is_some_and(|held| held.executor.excludes(executor));
    if let Some(owed) = follow_on {
        let successor = RunHold {
            run: owed.recovery_run(),
            executor: executor.clone(),
        };
        if let Some(held) = unfinished.filter(|held| {
            held.executor.excludes(executor)
                && !owed.hands_off_root(&held.executor, &held.run, &successor)
        }) {
            return Err(StoreError::RunHeldByAnotherExecutor {
                session_id: session.clone(),
                run: held.run.clone(),
                recorded: Box::new(held.executor.clone()),
                admitting: Box::new(executor.clone()),
            });
        }
        return Ok(Some((
            owed.recovery_run(),
            AdmittedWork::FollowOn {
                follow_on: owed.follow_on_turn_id.clone(),
                attempts: owed.attempts,
            },
        )));
    }
    if let Some(held) = unfinished {
        if held_elsewhere() {
            return Err(StoreError::RunHeldByAnotherExecutor {
                session_id: session.clone(),
                run: held.run.clone(),
                recorded: Box::new(held.executor.clone()),
                admitting: Box::new(executor.clone()),
            });
        }
        return Ok(Some((
            held.run.clone(),
            match &held.head {
                AdmittedHead::Input(head) => AdmittedWork::Input { head: head.clone() },
                AdmittedHead::Batch(head) => AdmittedWork::Queued { head: head.clone() },
            },
        )));
    }
    if let Some(sequence) = command_seq {
        if let Some(command) = queued.iter().find(|batch| batch.enqueue_seq == sequence)
            && matches!(&command.payload, crate::QueuedWorkPayload::SessionCommand { command } if matches!(**command, crate::queued_work_vocabulary::SessionCommand::RunPluginTask { .. }))
        {
            return Ok(Some((
                crate::tool_run::OperationRun {
                    session_id: session.clone(),
                    operation_id: command.batch_id.to_string(),
                }
                .run_id(),
                AdmittedWork::Operation {
                    operation: command.batch_id.clone(),
                },
            )));
        }
        return Ok(Some((
            TurnId::prefixed("shift-commands:", admission.as_str()),
            AdmittedWork::Commands { head: sequence },
        )));
    }
    // The lane selector is store vocabulary: active-turn rows and control
    // batches do not enter the next-turn lane.
    let input = inputs
        .iter()
        .filter(|read| read.input.state.is_next_turn_input(None))
        .min_by_key(|read| read.input.enqueue_seq);
    let batch = queued
        .iter()
        .filter(|batch| batch.terminal.is_none() && batch.kind() == crate::QueuedWorkKind::Turn)
        .min_by_key(|batch| batch.enqueue_seq);
    match (input, batch) {
        (Some(input), batch)
            if batch.is_none_or(|batch| input.input.enqueue_seq <= batch.enqueue_seq) =>
        {
            Ok(Some((
                bound_head
                    .or_else(|| {
                        input.input.source_key.as_ref().map(|key| {
                            TurnId::parse(key)
                                .ok()
                                .unwrap_or_else(|| TurnId::from(&input.input.input_id))
                        })
                    })
                    .unwrap_or_else(|| TurnId::from(&input.input.input_id)),
                AdmittedWork::Input {
                    head: input.input.input_id.clone(),
                },
            )))
        }
        (_, Some(batch)) => Ok(Some((
            TurnId::prefixed("shift-run:", admission.as_str()),
            AdmittedWork::Queued {
                head: batch.batch_id.clone(),
            },
        ))),
        _ => Ok(None),
    }
}

pub fn inspect_shift_admitted_head(
    base: &SessionHeadRef,
    live: Option<&SessionHeadMeta>,
    committed: bool,
) -> AdmittedHeadVerdict {
    let Some(live) = live else {
        return AdmittedHeadVerdict::Diverged { live_revision: 0 };
    };
    if base.names_head(live) || committed {
        return AdmittedHeadVerdict::Ready;
    }
    if live.head_revision <= base.revision {
        return AdmittedHeadVerdict::Diverged {
            live_revision: live.head_revision,
        };
    }
    if live.published_by_shift {
        AdmittedHeadVerdict::Advanced {
            head: SessionHeadRef {
                generation: base.generation,
                revision: live.head_revision,
                leaf: live.leaf_node_id.clone(),
                checkpoint: live.checkpoint_ref.clone(),
            },
        }
    } else {
        AdmittedHeadVerdict::Overtaken {
            live_revision: live.head_revision,
        }
    }
}
