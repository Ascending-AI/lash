//! The §5 barrier as the index sees it: which lower-ranked committed siblings
//! still owe their seats. A held drain subscribes to the barrier's
//! `Drained` notice, which the index answers once none is left (FIG-4344).
//! Split from the index handlers so the handler file keeps its line budget.

use super::*;

/// Every committed sibling ranked below `below` that still owes its seat —
/// the §5 barrier as the index sees it (FIG-4308).
///
/// Every one is named, not only the last: a child that declared no intent and
/// won its own commit seats without waiting on anyone, so its seat covers no
/// lower sibling, and a barrier that named only the last blocker would lift
/// while a lower one was still unseated. The barrier lifts once all of them
/// have seated, or retirement releases the wait. Once empty it stays empty: a
/// child's barrier is asked at its own reserved rank, and a later commit
/// reserves a higher one; the closing barrier past the last rank is asked
/// after the close decided every undecided child. A cancel-decided sibling
/// never blocks: its decision seats it.
pub(super) fn blocking_positions(live: &EffectGroupStateLiveRecord, below: u64) -> Vec<usize> {
    let mut blockers = live
        .commit_states
        .iter()
        .filter_map(|(position, state)| match state {
            EffectGroupChildCommitState::Committed { rank }
                if *rank < below && !live.settled_positions.contains_key(position) =>
            {
                Some((*rank, *position))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    blockers.sort_unstable();
    blockers.into_iter().map(|(_, position)| position).collect()
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
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
            next_rank: 1,
            commit_states: commits
                .iter()
                .map(|&(position, rank)| {
                    (position, EffectGroupChildCommitState::Committed { rank })
                })
                .collect(),
            settlements: BTreeMap::new(),
            settled_positions: seated
                .iter()
                .map(|&position| {
                    let rank = commits
                        .iter()
                        .find(|(committed, _)| *committed == position)
                        .map_or(0, |(_, rank)| *rank);
                    (position, rank)
                })
                .collect(),
        }
    }

    #[test]
    fn the_barrier_is_every_unseated_committed_sibling_below_in_rank_order() {
        // Positions and ranks differ: position 4 committed first, position 1
        // last among those below rank 5.
        let live = live_record(&[(4, 1), (0, 2), (3, 3), (1, 4), (2, 5)], &[]);
        assert_eq!(blocking_positions(&live, 5), vec![4, 0, 3, 1]);
        assert_eq!(blocking_positions(&live, 2), vec![4]);
        assert_eq!(blocking_positions(&live, 1), Vec::<usize>::new());
    }

    #[test]
    fn a_seated_sibling_no_longer_blocks_even_out_of_rank_order() {
        // Rank 4 seated before ranks 2 and 3: the lower unseated ones still
        // block, because a seat no longer waits on the siblings below it.
        let live = live_record(&[(4, 1), (0, 2), (3, 3), (1, 4), (2, 5)], &[1, 4]);
        assert_eq!(blocking_positions(&live, 5), vec![0, 3]);
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
