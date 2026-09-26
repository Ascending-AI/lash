//! The settled-root mailbox (FIG-3600 S5b, D1 Q1): the full report of a root
//! that ran in this process, for the handle waiting on one of its inputs.
//!
//! A session driver deposits, for every accepted input a root's recorded
//! claim drove, that root's final assembled turn. A handle takes its input's
//! entry once and answers with the report as it is today. An entry is keyed
//! by the session and the input id, and input ids are minted unique, so every
//! driver in the process shares one mailbox: a handle is answered even when
//! another core's driver is the one its engine runs. The mailbox is bounded:
//! the oldest entry goes first, and a handle whose entry is gone (or whose
//! root ran in another process) rebuilds a thinner report from the store.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use lash_core::facade_support::AssembledTurn;
use lash_core::{InputId, SessionId, TurnId};
use lash_sansio::sync::MutexExt;

/// Entries kept before the oldest is evicted.
const CAPACITY: usize = 4096;

/// A root this process ran to its commit, and its final physical turn.
pub(super) type SettledRoot = (TurnId, Arc<AssembledTurn>);

type Entry = ((SessionId, InputId), SettledRoot);

static SETTLED_ROOTS: LazyLock<Mutex<VecDeque<Entry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Deposit `report`'s final turn for every input its claim drove.
pub(crate) fn deposit_settled_root(session: &SessionId, report: lash_core::drive::RootReport) {
    let lash_core::drive::RootReport {
        outcome,
        run,
        driven_inputs,
    } = report;
    let lash_core::engine::RootOutcome::Committed { root, .. } = outcome else {
        return;
    };
    let Some(turn) = run.and_then(|run| run.into_final_turn()) else {
        return;
    };
    let turn = Arc::new(turn);
    let mut entries = SETTLED_ROOTS.lock_recover();
    for input in driven_inputs {
        if entries.len() >= CAPACITY {
            entries.pop_front();
        }
        entries.push_back(((session.clone(), input), (root.clone(), Arc::clone(&turn))));
    }
}

/// Take `input`'s entry, if this process holds one.
pub(super) fn take_settled_root(session: &SessionId, input: &InputId) -> Option<SettledRoot> {
    let mut entries = SETTLED_ROOTS.lock_recover();
    let position = entries
        .iter()
        .position(|((entry_session, entry_input), _)| {
            entry_session == session && entry_input == input
        })?;
    entries.remove(position).map(|(_, turn)| turn)
}
