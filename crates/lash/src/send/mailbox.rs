//! The settled-root mailbox (FIG-3600 S5b, D1 Q1): the full report of a root
//! that ran in this process, for the handle waiting on one of its inputs.
//!
//! A session driver deposits, for every accepted input a root's recorded
//! claim drove, that root's final assembled turn. A handle takes its input's
//! entry once and answers with the report as it is today. An entry is keyed
//! by the store binding, the session and the input id — a keyed input's id
//! derives from its session and key, so two stores in one process can hold
//! the same session and input ids — and every driver in the process shares
//! one mailbox: a handle is answered even when another core's driver over
//! the same stores is the one its engine runs. The mailbox is bounded:
//! the oldest entry goes first, and a handle whose entry is gone (or whose
//! root ran in another process) rebuilds a thinner report from the store.
//!
//! A deposit wakes every waiting handle ([`deposited`]), so a handle whose
//! root settled answers the moment its report lands rather than on its next
//! poll: the root's commit is durable before its run returns the report, and
//! a handle that saw the commit first waits for this wake (FIG-3843).
//!
//! A root's run is marked while it runs ([`running`]): a handle whose root
//! settled waits for a report only while that root's run is under way in
//! this process. A root that ran anywhere else never deposits here, so
//! its handle answers from the store at once; the mark's release wakes the
//! waiting handles like a deposit does.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use lash_core::facade_support::AssembledTurn;
use lash_core::{InputId, SessionId, StoreBindingId, TurnId};
use lash_sansio::sync::MutexExt;

/// Entries kept before the oldest is evicted.
const CAPACITY: usize = 4096;

/// A root this process ran to its commit, and its final physical turn.
pub(super) type SettledRoot = (TurnId, Arc<AssembledTurn>);

type Entry = ((StoreBindingId, SessionId, InputId), SettledRoot);

static SETTLED_ROOTS: LazyLock<Mutex<VecDeque<Entry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

/// A root's run under way in this process, which deposits its report when
/// it returns, in the stores its binding names.
type Running = (StoreBindingId, SessionId, TurnId);

static RUNNING: LazyLock<Mutex<Vec<Running>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Woken once per deposit, after its entries are in, and once per released
/// [`RunningHere`] mark.
static DEPOSITED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// The next deposit's wake. Enable it before looking in the mailbox, so a
/// deposit between the look and the wait is not missed.
pub(super) fn deposited() -> tokio::sync::futures::Notified<'static> {
    DEPOSITED.notified()
}

/// Marks a run under way in this process until dropped. Take it before the
/// run can commit anything, and deposit before it drops: a handle that saw
/// the commit then finds either the mark or the deposit.
pub(crate) struct RunningHere(Option<Running>);

/// Mark `root`'s run under way in this process, in the stores `binding`
/// names.
pub(crate) fn running(binding: &StoreBindingId, session: &SessionId, root: &TurnId) -> RunningHere {
    let mark = (binding.clone(), session.clone(), root.clone());
    RUNNING.lock_recover().push(mark.clone());
    RunningHere(Some(mark))
}

impl Drop for RunningHere {
    fn drop(&mut self) {
        let Some(mark) = self.0.take() else {
            return;
        };
        {
            let mut running = RUNNING.lock_recover();
            if let Some(position) = running.iter().position(|entry| *entry == mark) {
                running.swap_remove(position);
            }
        }
        DEPOSITED.notify_waiters();
    }
}

/// Whether `root`'s run is under way in this process, so its report may
/// still be deposited.
pub(super) fn may_deposit(binding: &StoreBindingId, session: &SessionId, root: &TurnId) -> bool {
    RUNNING
        .lock_recover()
        .iter()
        .any(|(entry_binding, entry_session, entry_root)| {
            entry_binding == binding && entry_session == session && entry_root == root
        })
}

/// Deposit `report`'s final turn for every input its claim drove, in the
/// stores `binding` names.
pub(crate) fn deposit_settled_root(
    binding: &StoreBindingId,
    session: &SessionId,
    report: lash_core::drive::RootReport,
) {
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
    {
        let mut entries = SETTLED_ROOTS.lock_recover();
        for input in driven_inputs {
            if entries.len() >= CAPACITY {
                entries.pop_front();
            }
            entries.push_back((
                (binding.clone(), session.clone(), input),
                (root.clone(), Arc::clone(&turn)),
            ));
        }
    }
    DEPOSITED.notify_waiters();
}

/// Whether this process holds `input`'s entry in the stores `binding`
/// names, without taking it.
pub(super) fn holds_settled_root(
    binding: &StoreBindingId,
    session: &SessionId,
    input: &InputId,
) -> bool {
    SETTLED_ROOTS
        .lock_recover()
        .iter()
        .any(|((entry_binding, entry_session, entry_input), _)| {
            entry_binding == binding && entry_session == session && entry_input == input
        })
}

/// Take `input`'s entry in the stores `binding` names, if this process
/// holds one.
pub(super) fn take_settled_root(
    binding: &StoreBindingId,
    session: &SessionId,
    input: &InputId,
) -> Option<SettledRoot> {
    let mut entries = SETTLED_ROOTS.lock_recover();
    let position =
        entries
            .iter()
            .position(|((entry_binding, entry_session, entry_input), _)| {
                entry_binding == binding && entry_session == session && entry_input == input
            })?;
    entries.remove(position).map(|(_, turn)| turn)
}
