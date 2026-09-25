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

use crate::durable_wait::RestateTurnCancelRaceOutcome;
use lash_core::facade_support::effect_replay_driver::{
    EffectGroupChildCommitOutcome, GroupChildFinalCommit,
};
use lash_core::{ExecutionScope, RuntimeEffectControllerError};

use crate::effect_group::{
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse,
    EffectGroupDrainBlockersResponse, drained_wait_lifted, drained_wait_request, group_shape_error,
};

use super::{RestateControllerContext, effect_group_engine_error};

/// The §4 boundary commit for one group child on the Restate tier.
pub(super) async fn commit_group_child_final<'ctx, C>(
    context: &C,
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
        .scope_group_child_membership(index_key, commit.replay_key.clone())
        .await
        .map_err(|error| {
            effect_group_engine_error("LashDurableWaitIndex/group_child_membership", error)
        })?
    else {
        return Ok(Outcome::Ungrouped);
    };
    let response = context
        .effect_group_commit_child(
            group_key.clone(),
            EffectGroupCommitChildRequest {
                replay_key: commit.replay_key.clone(),
            },
        )
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/commit_child", error))?;
    Ok(match response {
        EffectGroupCommitChildResponse::Committed { commit_seq, .. } => Outcome::Committed {
            group_key,
            commit_seq,
        },
        EffectGroupCommitChildResponse::AlreadyCommitted { commit_seq, .. } => {
            Outcome::AlreadyCommitted {
                group_key,
                commit_seq,
                drain_input: None,
            }
        }
        EffectGroupCommitChildResponse::CancelDecided { rank } => Outcome::CancelDecided {
            group_key,
            commit_seq: rank,
        },
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

/// The §5 barrier on the engine's own wake: the index names the lower-commit
/// siblings still owed a seat, and the drain parks on each one's durable
/// drained wake — the same wake the dispatch workflow's settlement parks on —
/// rather than polling the index. The set was fixed when this child's commit
/// position was allocated, so waiting out each member once lifts the barrier.
pub(super) async fn await_group_child_drain_admission<'ctx, C>(
    context: &C,
    group_key: &str,
    commit_seq: u64,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let (wait_scope, positions) = match context
        .effect_group_drain_blockers(group_key.to_string(), commit_seq)
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/drain_blockers", error))?
    {
        EffectGroupDrainBlockersResponse::Admitted => return Ok(()),
        EffectGroupDrainBlockersResponse::Blocked {
            wait_scope,
            positions,
        } => (wait_scope, positions),
    };
    for position in positions {
        let request = drained_wait_request(&wait_scope, group_key, position)?;
        let replay_key = request.key.key_id.clone();
        let resolution = match context
            .await_effect_group_wait(request, replay_key, None)
            .await
            .map_err(|error| {
                effect_group_engine_error(
                    "LashDurableWaitWorkflow/await_resolution(DRAINED)",
                    error,
                )
            })? {
            RestateTurnCancelRaceOutcome::Completed(resolution) => resolution,
            RestateTurnCancelRaceOutcome::TurnCancelled
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
