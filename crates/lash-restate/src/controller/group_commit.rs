//! The generic group drain barrier on the durable index notice.

use super::{RestateControllerContext, effect_group_engine_error};
use crate::durable_wait::RestateTurnCancelRaceOutcome;
use crate::effect_group::{EffectGroupNotice, EffectGroupNotification, group_shape_error};
use lash_core::RuntimeEffectControllerError;

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
