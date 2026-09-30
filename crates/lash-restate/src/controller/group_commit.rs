//! The §4 boundary for one group child's final record, and the §5 barrier its
//! drain waits at, routed through the durable index objects.
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
//! The Restate index does not retain `drain_input`: the durable publication
//! obligation is the committed-but-unseated child plus the dispatch workflow's
//! own redrive, so `AlreadyCommitted` reports it `None`.
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
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse,
    EffectGroupDrainBlockersResponse, drained_wait_lifted, drained_wait_request, group_shape_error,
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
            },
        )
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/commit_child", error))?;
    Ok(match response {
        EffectGroupCommitChildResponse::Committed { rank } => {
            Outcome::Committed { group_key, rank }
        }
        EffectGroupCommitChildResponse::AlreadyCommitted { rank, .. } => {
            Outcome::AlreadyCommitted {
                group_key,
                rank,
                drain_input: None,
            }
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

/// The §5 barrier on the engine's own wake: the index names every committed
/// sibling ranked below `rank` that has not seated, and the waits on their
/// drained wakes are issued together, so one round trip covers them all.
/// The barrier lifts once all lower committed siblings have seated, or
/// retirement releases the wait — a release, not proof of seating: the
/// semantic-admission fence still refuses any intent under a retired group.
pub(super) async fn await_group_child_drain_admission<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    rank: u64,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let (wait_scope, positions) = match context
        .effect_group_drain_blockers(namespace, group_key.to_string(), rank)
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/drain_blockers", error))?
    {
        EffectGroupDrainBlockersResponse::Admitted => return Ok(()),
        EffectGroupDrainBlockersResponse::Blocked {
            wait_scope,
            positions,
        } => (wait_scope, positions),
    };
    let mut waits = Vec::with_capacity(positions.len());
    for &position in &positions {
        let request = drained_wait_request(&wait_scope, group_key, position)?;
        let replay_key = request.key.key_id.clone();
        waits.push(context.await_effect_group_wait(
            namespace,
            request,
            replay_key,
            None,
            super::context::ProcessCancelRace::NotRaced,
        ));
    }
    let resolved = join_in_order(waits).await;
    for (position, resolved) in positions.into_iter().zip(resolved) {
        let resolution = match resolved.map_err(|error| {
            effect_group_engine_error("LashDurableWaitWorkflow/await_resolution(DRAINED)", error)
        })? {
            RestateTurnCancelRaceOutcome::Completed(resolution) => resolution,
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::ProcessCancelled
            | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} drained wake for child {position} ended without \
                     a resolution though it races no turn gate"
                )));
            }
        };
        drained_wait_lifted(group_key, position, resolution)?;
    }
    Ok(())
}

/// Drives every future to completion, polling them in their order on each
/// wake, and returns their outputs in that order.
///
/// A journaled wait emits its call when it is first polled, so the first poll
/// issues every call, in order, before any completes: one round trip covers
/// them all, and the journal is the same on every replay.
async fn join_in_order<F: std::future::Future + Unpin>(mut futures: Vec<F>) -> Vec<F::Output> {
    let mut outputs = futures.iter().map(|_| None).collect::<Vec<_>>();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_some() {
                continue;
            }
            match std::pin::Pin::new(future).poll(cx) {
                std::task::Poll::Ready(value) => *output = Some(value),
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
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
