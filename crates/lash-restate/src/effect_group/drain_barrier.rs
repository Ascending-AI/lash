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

/// The committed sibling below `below` that still owes its seat and
/// committed last — the §5 barrier as the index sees it, reduced to the one
/// wake it needs (FIG-4088).
///
/// The barrier is transitive. Commit sequences are allocated in order, so every
/// sibling that committed below the last blocker had committed when that
/// blocker's own barrier was read, and the blocker seats only once each of
/// them has seated or the group retired, which lifts every barrier. Its
/// drained wake therefore resolves exactly when the whole barrier lifts.
/// Parking on every blocker instead cost each drain a wait per lower sibling,
/// quadratic in the width.
pub(super) fn blocking_positions(live: &EffectGroupStateLiveRecord, below: u64) -> Vec<usize> {
    live.commit_states
        .iter()
        .filter_map(|(position, state)| match state {
            EffectGroupChildCommitState::Committed { commit_seq }
                if *commit_seq < below && !live.settled_positions.contains_key(position) =>
            {
                Some((*commit_seq, *position))
            }
            _ => None,
        })
        .max()
        .map(|(_, position)| position)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_record(commits: &[(usize, u64)], seated: &[usize]) -> EffectGroupStateLiveRecord {
        EffectGroupStateLiveRecord {
            shape: EffectGroupShape {
                wake: lash_core::GroupWakePolicy::All,
                loser_disposition: LoserPolicy::RunToCompletion,
                replay_keys: (0..6).map(|position| format!("child-{position}")).collect(),
                wait_scope: ExecutionScope::runtime_operation("group"),
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
            next_rank: 1,
            next_commit_seq: 1,
            commit_states: commits
                .iter()
                .map(|&(position, commit_seq)| {
                    (
                        position,
                        EffectGroupChildCommitState::Committed { commit_seq },
                    )
                })
                .collect(),
            settlements: BTreeMap::new(),
            settled_positions: seated
                .iter()
                .enumerate()
                .map(|(rank, &position)| (position, rank as u64 + 1))
                .collect(),
        }
    }

    #[test]
    fn the_barrier_is_the_last_committed_unseated_sibling_below() {
        // Positions and commit order differ: position 4 committed first,
        // position 1 last among those below sequence 5.
        let live = live_record(&[(4, 1), (0, 2), (3, 3), (1, 4), (2, 5)], &[]);
        assert_eq!(blocking_positions(&live, 5), vec![1]);
        assert_eq!(blocking_positions(&live, 2), vec![4]);
        assert_eq!(blocking_positions(&live, 1), Vec::<usize>::new());
    }

    #[test]
    fn a_seated_sibling_no_longer_blocks() {
        let live = live_record(&[(4, 1), (0, 2), (3, 3), (1, 4), (2, 5)], &[1, 3]);
        assert_eq!(blocking_positions(&live, 5), vec![0]);
        let live = live_record(&[(4, 1), (0, 2)], &[4, 0]);
        assert_eq!(blocking_positions(&live, 3), Vec::<usize>::new());
    }

    #[test]
    fn a_cancel_decided_sibling_does_not_block() {
        let mut live = live_record(&[(0, 1)], &[]);
        live.commit_states
            .insert(1, EffectGroupChildCommitState::CancelDecided);
        assert_eq!(blocking_positions(&live, 5), vec![0]);
    }
}
