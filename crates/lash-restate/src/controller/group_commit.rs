//! The §4 boundary for one group child's final record, and the §5 barrier its
//! drain waits at, routed through the durable index objects: the barrier is
//! the group index's own `Drained` notice (FIG-4344).
//!
//! Split out of `mod.rs` only for the production file-size budget: this is
//! the routing half of
//! [`RuntimeEffectController::commit_group_child_final`](lash_core::RuntimeEffectController::commit_group_child_final)
//! and
//! [`RuntimeEffectController::await_group_child_drain_admission`](lash_core::RuntimeEffectController::await_group_child_drain_admission)
//! for the Restate tier. The child's own scope index answers which group owns
//! its replay key, and that group's index takes the commit. The serialized
//! object handler — not any state the controller holds — is the linearization
//! point, so a cancel decision racing the commit is fenced inside the index.
//! The index retains the commit's `drain_input` as the child's committed
//! final, and `AlreadyCommitted` answers the one the winner sealed: a later
//! invocation of the child drains exactly that final (ADR 0099 §5, W7).
//!
//! The controller keeps, per child, the rank its own commit was answered
//! (FIG-4308): the child's dispatch handler publishes that rank at the seat
//! without committing again. The receipt is filled only from a journaled
//! `Committed` or `AlreadyCommitted` answer this controller received — never
//! from `Ungrouped` or a cancel — so a replay of the invocation restores it
//! from the same journaled answer, and a fresh invocation starts without one.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::durable_wait::RestateTurnCancelRaceOutcome;
use lash_core::facade_support::{EffectGroupChildCommitOutcome, GroupChildFinalCommit};
use lash_core::{ExecutionScope, RuntimeEffectControllerError};

use crate::effect_group::{
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupCommittedFinal,
    EffectGroupNotice, EffectGroupNotification, group_shape_error,
};

use super::{RestateControllerContext, effect_group_engine_error};

/// The §4 boundary commit for one group child on the Restate tier.
pub(super) async fn commit_group_child_final<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    commit: GroupChildFinalCommit,
) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    use EffectGroupChildCommitOutcome as Outcome;
    let scope = ExecutionScope::from_journal_key(&commit.scope_id).ok_or_else(|| {
        group_shape_error(format!(
            "group-child commit scope id `{}` does not decode to an execution scope",
            commit.scope_id
        ))
    })?;
    let index_key = crate::durable_wait::durable_wait_index_key_for_scope(&scope);
    let Some(group_key) = context
        .scope_group_child_membership(namespace, index_key, commit.replay_key.clone())
        .await
        .map_err(|error| {
            effect_group_engine_error("LashDurableWaitIndex/group_child_membership", error)
        })?
    else {
        return Ok(Outcome::Ungrouped);
    };
    let response = context
        .effect_group_commit_child(
            namespace,
            group_key.clone(),
            EffectGroupCommitChildRequest {
                replay_key: commit.replay_key.clone(),
                committed: EffectGroupCommittedFinal::Tool {
                    drain_input: commit.drain_input,
                },
            },
        )
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/commit_child", error))?;
    Ok(match response {
        EffectGroupCommitChildResponse::Committed { rank } => {
            Outcome::Committed { group_key, rank }
        }
        EffectGroupCommitChildResponse::AlreadyCommitted {
            rank,
            committed: EffectGroupCommittedFinal::Tool { drain_input },
        } => Outcome::AlreadyCommitted {
            group_key,
            rank,
            drain_input,
        },
        EffectGroupCommitChildResponse::AlreadyCommitted { rank, committed } => {
            return Err(committed_final_is_not_a_tool_terminal(
                &group_key,
                &commit.replay_key,
                rank,
                &committed,
            ));
        }
        EffectGroupCommitChildResponse::CancelDecided { rank } => {
            Outcome::CancelDecided { group_key, rank }
        }
        EffectGroupCommitChildResponse::UnknownChild => {
            return Err(group_shape_error(format!(
                "effect group {group_key} membership names replay key `{}` but its \
                 index holds no such child; the two durable records disagree",
                commit.replay_key
            )));
        }
        EffectGroupCommitChildResponse::UnknownGroup | EffectGroupCommitChildResponse::Retired => {
            return Err(group_shape_error(format!(
                "effect group {group_key} carries membership for replay key `{}` but \
                 its index is gone or retired; the two durable records disagree",
                commit.replay_key
            )));
        }
    })
}

/// A tool child's commit that found the point holding a final that is not a
/// tool terminal: one only an invocation that never drove the child commits,
/// which exists only once the executing invocation is gone. The committed final
/// wins, so this shift's own final is refused rather than drained over it.
pub(crate) fn committed_final_is_not_a_tool_terminal(
    group_key: &str,
    replay_key: &str,
    rank: u64,
    committed: &EffectGroupCommittedFinal,
) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "effect group {group_key} child `{replay_key}` drove to a tool terminal, but \
         the §4 point already holds its final at rank {rank} ({committed:?}), \
         committed by an invocation that did not shift it"
    ))
}

/// The §5 barrier on the group index's own notice (FIG-4344): one
/// subscription to the barrier for `rank`, which the index answers at once
/// when no committed sibling ranked below it still owes its seat, and
/// otherwise completes from the seat that lifts it. The barrier lifts once all
/// lower committed siblings have seated, or retirement releases the wait — a
/// release, not proof of seating: the semantic-admission fence still refuses
/// any intent under a retired group.
///
/// The subscription is one call, awaited alone. Several SDK call futures must
/// never be polled together here: one that has blocked on the invocation's
/// input reads input again before it re-checks its own completion, so a
/// sibling can take that completion off the input and leave the drain parked
/// until the stream's inactivity timeout (FIG-4431).
pub(super) async fn await_group_child_drain_admission<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    rank: u64,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let notification = match context
        .await_effect_group_notice(
            namespace,
            group_key.to_string(),
            EffectGroupNotice::Drained { rank },
            None,
            super::context::ProcessCancelRace::NotRaced,
        )
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/subscribe(Drained)", error))?
    {
        RestateTurnCancelRaceOutcome::Completed(notification) => notification,
        RestateTurnCancelRaceOutcome::TurnCancelled
        | RestateTurnCancelRaceOutcome::ProcessCancelled
        | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
            return Err(group_shape_error(format!(
                "effect group {group_key} barrier at rank {rank} ended without an answer \
                 though it races no turn gate"
            )));
        }
    };
    match notification {
        EffectGroupNotification::Drained
        | EffectGroupNotification::Retired
        | EffectGroupNotification::Absent => Ok(()),
        other => Err(group_shape_error(format!(
            "effect group {group_key} barrier at rank {rank} answered {other:?}"
        ))),
    }
}

/// The exact child a commit receipt is for: its group, its journal scope and
/// its replay key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct GroupCommitReceiptKey {
    group_key: String,
    scope_id: String,
    replay_key: String,
}

/// The ranks this controller's own commits reserved, by exact child.
#[derive(Default)]
pub(super) struct GroupCommitReceipts {
    ranks: Mutex<BTreeMap<GroupCommitReceiptKey, u64>>,
}

impl GroupCommitReceipts {
    /// Keeps the rank a `Committed` or `AlreadyCommitted` answer reserved for
    /// the child at `scope_id`/`replay_key`; any other answer keeps nothing.
    pub(super) fn keep(
        &self,
        scope_id: String,
        replay_key: String,
        outcome: &EffectGroupChildCommitOutcome,
    ) {
        let (group_key, rank) = match outcome {
            EffectGroupChildCommitOutcome::Committed { group_key, rank }
            | EffectGroupChildCommitOutcome::AlreadyCommitted {
                group_key, rank, ..
            } => (group_key.clone(), *rank),
            EffectGroupChildCommitOutcome::Ungrouped
            | EffectGroupChildCommitOutcome::CancelDecided { .. } => return,
        };
        self.ranks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                GroupCommitReceiptKey {
                    group_key,
                    scope_id,
                    replay_key,
                },
                rank,
            );
    }

    pub(super) fn rank(&self, group_key: &str, scope_id: &str, replay_key: &str) -> Option<u64> {
        self.ranks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&GroupCommitReceiptKey {
                group_key: group_key.to_owned(),
                scope_id: scope_id.to_owned(),
                replay_key: replay_key.to_owned(),
            })
            .copied()
    }
}
