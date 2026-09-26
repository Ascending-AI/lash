//! The §5 barrier on the engine's own wake: which lower-commit siblings still
//! hold a drain, the drained wake each resolves when it seats, and what that
//! wake's resolution means to the drain parked on it. A held drain parks on
//! those wakes instead of polling the index. Split from the index handlers so
//! the handler file keeps its line budget.

use super::*;

/// The durable wake the child at `position` resolves when it seats its
/// settlement: what a §5 barrier parks on while that lower-commit sibling
/// finishes its drain.
pub(crate) fn drained_wait_request(
    scope: &ExecutionScope,
    group_key: &str,
    position: usize,
) -> Result<RestateDurableWaitAwaitRequest, RuntimeEffectControllerError> {
    group_wait_key(scope, group_key, EffectGroupWaitKind::Drained(position))
        .map(|key| RestateDurableWaitAwaitRequest {
            key,
            deadline: None,
        })
        .map_err(|error| group_shape_error(error.to_string()))
}

/// Check what a §5 barrier's drained wake resolved to: the blocking sibling
/// seated its settlement, or the group retired, which lifts every barrier.
pub(crate) fn drained_wait_lifted(
    group_key: &str,
    position: usize,
    resolution: Resolution,
) -> Result<(), RuntimeEffectControllerError> {
    match decode_wait_resolution(resolution)? {
        EffectGroupWaitResolution::Drained | EffectGroupWaitResolution::Retired => Ok(()),
        other => Err(group_shape_error(format!(
            "effect group {group_key} drained wake for child {position} resolved as {other:?}"
        ))),
    }
}

/// Every committed sibling below `below` that still owes its seat — the §5
/// barrier as the index sees it.
pub(super) fn blocking_positions(live: &EffectGroupStateLiveRecord, below: u64) -> Vec<usize> {
    live.commit_states
        .iter()
        .filter_map(|(position, state)| match state {
            EffectGroupChildCommitState::Committed { commit_seq }
                if *commit_seq < below && !live.settled_positions.contains_key(position) =>
            {
                Some(*position)
            }
            _ => None,
        })
        .collect()
}
