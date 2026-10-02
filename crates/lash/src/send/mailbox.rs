//! The settled-run mailbox (FIG-3600 S5b, D1 Q1): the full report of a run
//! that ran in this process, for the handle waiting on one of its inputs.
//!
//! A `SessionShifts` deposits, for every accepted input a run's recorded
//! claim drove, that run's final assembled turn. A handle takes its input's
//! entry once and answers with the report as it is today. An entry is keyed
//! by the store binding, the session and the input id — a keyed input's id
//! derives from its session and key, so two stores in one process can hold
//! the same session and input ids — and every `SessionShifts` in the process shares
//! one mailbox: a handle is answered even when another core's `SessionShifts` over
//! the same stores is the one its engine runs. The mailbox is bounded:
//! the oldest entry goes first, and a handle whose entry is gone (or whose
//! run ran in another process) rebuilds a thinner report from the store.
//!
//! A deposit wakes every waiting handle ([`deposited`]), so a handle whose
//! run settled answers the moment its report lands rather than on its next
//! poll: the run's commit is durable before its shift hands the report
//! over, which it does before the run's scope closes (FIG-3979), and a
//! handle that saw the commit first waits for this wake (FIG-3843).
//!
//! A run's execution is marked while it runs ([`running`]): a handle whose run
//! settled waits for a report only while that run's execution is under way in
//! this process. A run that ran anywhere else never deposits here, so
//! its handle answers from the store at once; the mark's release wakes the
//! waiting handles like a deposit does.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use lash_core::facade_support::{AssembledTurn, RuntimeHandle, WeakRuntimeHandle};
use lash_core::{InputId, SessionId, StoreBindingId, TurnId};
use lash_sansio::sync::MutexExt;

/// Entries kept before the oldest is evicted.
const CAPACITY: usize = 4096;

/// A run this process ran to its commit: its final physical turn, and the
/// runtime that ran it.
#[derive(Clone)]
pub(super) struct SettledRun {
    pub(super) run: TurnId,
    pub(super) turn: Arc<AssembledTurn>,
    ran_on: WeakRuntimeHandle,
}

impl SettledRun {
    /// Whether `runtime` ran the run: its resident state holds the run's
    /// commit, and its observation was published with the deposit.
    pub(super) fn ran_on(&self, runtime: &RuntimeHandle) -> bool {
        self.ran_on.names(runtime)
    }
}

type Entry = ((StoreBindingId, SessionId, InputId), SettledRun);

static SETTLED_RUNS: LazyLock<Mutex<VecDeque<Entry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

/// A run's execution under way in this process, which deposits its report when
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

/// Mark `run`'s run under way in this process, in the stores `binding`
/// names.
pub(crate) fn running(binding: &StoreBindingId, session: &SessionId, run: &TurnId) -> RunningHere {
    let mark = (binding.clone(), session.clone(), run.clone());
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

/// Whether `run`'s run is under way in this process, so its report may
/// still be deposited.
pub(super) fn may_deposit(binding: &StoreBindingId, session: &SessionId, run: &TurnId) -> bool {
    RUNNING
        .lock_recover()
        .iter()
        .any(|(entry_binding, entry_session, entry_run)| {
            entry_binding == binding && entry_session == session && entry_run == run
        })
}

/// Deposit `settled`'s final turn for every input its claim drove, in the
/// stores `binding` names, from the shift `runtime` ran it on.
pub(crate) fn deposit_settled_run(
    binding: &StoreBindingId,
    session: &SessionId,
    settled: lash_core::shift::SettledRun<'_>,
    runtime: &RuntimeHandle,
) {
    if settled.executed_inputs.is_empty() {
        return;
    }
    let entry = SettledRun {
        run: settled.run.clone(),
        turn: Arc::new(settled.turn.clone()),
        ran_on: runtime.downgrade(),
    };
    {
        let mut entries = SETTLED_RUNS.lock_recover();
        for input in settled.executed_inputs {
            if entries.len() >= CAPACITY {
                entries.pop_front();
            }
            entries.push_back((
                (binding.clone(), session.clone(), input.clone()),
                entry.clone(),
            ));
        }
    }
    DEPOSITED.notify_waiters();
}

/// Whether this process holds `input`'s entry in the stores `binding`
/// names, without taking it.
pub(super) fn holds_settled_run(
    binding: &StoreBindingId,
    session: &SessionId,
    input: &InputId,
) -> bool {
    SETTLED_RUNS
        .lock_recover()
        .iter()
        .any(|((entry_binding, entry_session, entry_input), _)| {
            entry_binding == binding && entry_session == session && entry_input == input
        })
}

/// Resolves once this process holds `input`'s entry in the stores
/// `binding` names.
pub(super) async fn settled_run_held(
    binding: &StoreBindingId,
    session: &SessionId,
    input: &InputId,
) {
    loop {
        let deposited = deposited();
        tokio::pin!(deposited);
        deposited.as_mut().enable();
        if holds_settled_run(binding, session, input) {
            return;
        }
        deposited.await;
    }
}

/// Take the first entry whose settled run is `run` in the stores
/// `binding` names, if this process holds one.
pub(super) fn take_settled_run_of(
    binding: &StoreBindingId,
    session: &SessionId,
    run: &TurnId,
) -> Option<SettledRun> {
    let mut entries = SETTLED_RUNS.lock_recover();
    let position = entries
        .iter()
        .position(|((entry_binding, entry_session, _), settled)| {
            entry_binding == binding && entry_session == session && settled.run == *run
        })?;
    entries.remove(position).map(|(_, settled)| settled)
}

/// Take `input`'s entry in the stores `binding` names, if this process
/// holds one.
pub(super) fn take_settled_run(
    binding: &StoreBindingId,
    session: &SessionId,
    input: &InputId,
) -> Option<SettledRun> {
    let mut entries = SETTLED_RUNS.lock_recover();
    let position =
        entries
            .iter()
            .position(|((entry_binding, entry_session, entry_input), _)| {
                entry_binding == binding && entry_session == session && entry_input == input
            })?;
    entries.remove(position).map(|(_, settled)| settled)
}
