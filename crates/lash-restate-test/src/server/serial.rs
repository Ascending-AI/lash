//! Serial scheduling: one attempt runs at a time.
//!
//! Under [`Scheduling::Serial`](super::Scheduling::Serial) the server hands a
//! single turn from attempt to attempt. The attempt that holds the turn runs.
//! Every other live attempt is gated: frames the server delivers to it, a
//! close of its input, the answer to a request its handler issued, and any
//! frame its handler writes wait until it holds the turn again, and a new
//! attempt does not start polling its handler until it first gets the turn.
//!
//! The holder gives up the turn when its handler parks (its poll returns
//! with nothing to write; the turn is decided at the park itself) waiting on
//! the server:
//!
//! * blocked on its input with no `ctx.run` closure in flight;
//! * its stream ended (it completed, suspended, failed or crashed);
//! * inside a `ctx.run` closure, on an ingress request the closure issued.
//!   A handler's requests are attributed to its attempt through a task-local
//!   set around the attempt's task;
//! * on a request of its own whose target — or an invocation that target
//!   waits on, through a call or a request — is ready to run. It queues for
//!   the turn again behind that target: a request whose target cannot move
//!   (a watch on an unresolved promise) is no reason to yield, since the
//!   handler may be parked on work of its own beside it;
//! * inside a `ctx.run` closure on a gate the test declared
//!   ([`OutsideGates`](super::OutsideGates)), while outside work waits and
//!   every other attempt waits on the server or on a declared gate.
//!
//! A request's answer readies its handler's attempt as the server answers
//! it, not when the handler reads it; a request the handler drops unanswered
//! stops counting as one it waits on.
//!
//! The next turn goes to the attempt that became ready first: attempts become
//! ready in the order the server started them, delivered to them or answered
//! their requests, under the server lock. With one attempt running, that
//! order follows from the seed and the handlers' own code.
//!
//! Outside work goes on only between turns, once no attempt is ready and
//! every live attempt waits on the server or on a declared gate: ingress
//! requests from outside every attempt (test code, tasks lash spawns beside
//! its handlers), in the order of their URL and body, each once the one
//! before it has reached the server; then attempts woken by something
//! outside the server (a declared gate opening, a store call completing)
//! with a frame to write, in invocation order. So outside work lands at the
//! same point on every run, however soon its task got to it.
//!
//! What Serial does not order: work outside the server that no one waits
//! for. SQLite completes on its own threads; the holder keeps the turn while
//! it awaits a store call, so store calls of different attempts do not
//! interleave, but when a store call completes is the store's timing. A task
//! lash spawns runs whenever Tokio polls it, and only its ingress requests
//! are sequenced. On a multi-threaded runtime those tasks race the holder,
//! so one seed's grant order repeats only on a current-thread runtime: there
//! every task the handlers spawn runs between the holder's awaits, in one
//! order. A holder that waits, undeclared, on something the server cannot
//! see for [`STALL`] while an attempt or outside work waits gives the turn
//! up anyway, and
//! [`Stats::stall_preemptions`](super::Stats::stall_preemptions) counts it:
//! a run with none was fully sequenced.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Shared;
use super::model::{InvKey, Invocation, LiveAttempt, Status};
use super::processor::State;

/// How long the holder may go without applying a frame, while not blocked
/// on the server, before the turn moves on anyway. Only a wait nobody
/// declared reaches it, so it is long enough that a store call on a loaded
/// host does not.
pub const STALL: Duration = Duration::from_secs(1);

/// What the holder does with the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Yield {
    Keep,
    /// Waits on the server or on a declared gate, or no longer live.
    Done,
    /// Parked while a request of its own waits on a ready attempt: it
    /// queues for the turn again behind that attempt.
    Waiting,
    /// Preempted after [`STALL`] without progress.
    Stalled,
}

/// One attempt of one invocation.
pub type Turn = (InvKey, u32);

/// An ingress request an attempt's handler issued, until it answers.
#[derive(Debug)]
struct IngressWait {
    turn: Turn,
    /// The `ctx.run` whose closure issued it, if one was in flight.
    run: Option<u32>,
    /// The invocation whose result it waits for, once it reached the
    /// server as a call or an attach.
    target: Option<InvKey>,
}

/// The serial scheduler's state, under the server lock.
#[derive(Debug)]
pub struct Serial {
    holder: Option<Turn>,
    /// Wall time the holder last got the turn or applied a frame.
    progress: Instant,
    ready: VecDeque<Turn>,
    /// In-flight ingress requests each attempt's handler issued, by
    /// ticket.
    ingress: BTreeMap<u64, IngressWait>,
    next_ticket: u64,
    /// Attempts that do not hold the turn and were woken by something
    /// outside the server — a gate their `ctx.run` closure awaited, a
    /// store call — with a frame to write. Like outside requests, they
    /// go on only between turns.
    woken: BTreeSet<Turn>,
    /// Outside gates a test declared handlers wait on (see
    /// [`OutsideGates`](super::OutsideGates)).
    gates: usize,
    /// Outside requests waiting to land, by URL, body and arrival.
    external: BTreeMap<(String, bytes::Bytes, u64), tokio::sync::oneshot::Sender<()>>,
    /// A request was let in and has not reached the server yet.
    landing: bool,
    /// Every grant of the turn, in order.
    trace: Vec<Turn>,
}

impl Default for Serial {
    fn default() -> Self {
        Self {
            holder: None,
            progress: Instant::now(),
            ready: VecDeque::new(),
            ingress: BTreeMap::new(),
            next_ticket: 0,
            woken: BTreeSet::new(),
            gates: 0,
            external: BTreeMap::new(),
            landing: false,
            trace: Vec::new(),
        }
    }
}

impl Serial {
    pub fn trace(&self) -> &[Turn] {
        &self.trace
    }

    /// Drop every waiting outside request's admission: each goes on, to
    /// the 503 a shut server answers.
    pub(super) fn shut_down(&mut self) {
        self.external.clear();
        self.woken.clear();
        self.ready.clear();
        self.holder = None;
    }
}

impl State {
    /// `turn` has something to act on once it holds the turn: queue it
    /// behind the attempts that became ready before it.
    pub(super) fn make_ready(&mut self, turn: Turn) {
        let Some(serial) = &mut self.serial else {
            return;
        };
        if serial.holder != Some(turn) && !serial.ready.contains(&turn) {
            serial.ready.push_back(turn);
        }
    }

    /// `turn` does not hold the turn and something outside the server woke
    /// it with a frame to write: it goes on between turns, as an outside
    /// request lands.
    pub(super) fn woke(&mut self, turn: Turn) {
        let Some(serial) = &mut self.serial else {
            return;
        };
        if serial.holder != Some(turn) && !serial.ready.contains(&turn) {
            serial.woken.insert(turn);
        }
    }

    /// A handler is about to wait on a gate the test opens.
    pub(super) fn gate_entered(&mut self) {
        if let Some(serial) = &mut self.serial {
            serial.gates += 1;
        }
    }

    /// A gate wait ended.
    pub(super) fn gate_left(&mut self) {
        if let Some(serial) = &mut self.serial {
            serial.gates = serial.gates.saturating_sub(1);
        }
    }

    /// `key`'s live attempt was just delivered to or closed: if it waits
    /// for the turn, queue it.
    pub(super) fn delivered(&mut self, key: InvKey) {
        if self.serial.is_none() {
            return;
        }
        if let Status::Running(attempt) = &self.invocations[key.0].status
            && attempt.has_held_work()
        {
            let turn = (key, attempt.number);
            self.make_ready(turn);
        }
    }

    /// An attempt applied a frame: the server moved.
    pub(super) fn progressed(&mut self) {
        if let Some(serial) = &mut self.serial {
            serial.progress = Instant::now();
        }
    }

    /// The handler of `turn` issued an ingress request. Returns the ticket
    /// that ends it.
    pub(super) fn ingress_began(&mut self, turn: Turn) -> u64 {
        // The closure that issued it: the latest `ctx.run` still in flight.
        let run = self.invocations[turn.0.0].pending_runs.last().copied();
        let Some(serial) = &mut self.serial else {
            return 0;
        };
        let ticket = serial.next_ticket;
        serial.next_ticket += 1;
        serial.ingress.insert(
            ticket,
            IngressWait {
                turn,
                run,
                target: None,
            },
        );
        ticket
    }

    /// The ingress request `ticket` waits for `target`'s result.
    pub(super) fn ingress_awaits(&mut self, ticket: u64, target: InvKey) {
        if let Some(wait) = self
            .serial
            .as_mut()
            .and_then(|serial| serial.ingress.get_mut(&ticket))
        {
            wait.target = Some(target);
        }
    }

    /// An ingress request answered: its handler goes on once the attempt
    /// holds the turn again.
    pub(super) fn ingress_ended(&mut self, ticket: u64) {
        let Some(serial) = &mut self.serial else {
            return;
        };
        if let Some(wait) = serial.ingress.remove(&ticket) {
            self.make_ready(wait.turn);
        }
    }

    /// An ingress request a handler issued was dropped unanswered: the
    /// handler does not wait on it.
    pub(super) fn ingress_abandoned(&mut self, ticket: u64) {
        if let Some(serial) = &mut self.serial {
            serial.ingress.remove(&ticket);
        }
    }

    /// Whether `turn` is inside a `ctx.run` closure waiting on an ingress
    /// request the closure issued.
    fn waits_in_run(&self, serial: &Serial, turn: Turn) -> bool {
        let pending = &self.invocations[turn.0.0].pending_runs;
        serial
            .ingress
            .values()
            .any(|wait| wait.turn == turn && wait.run.is_some_and(|run| pending.contains(&run)))
    }

    /// Whether the handler of `turn` has an ingress request of its own in
    /// flight.
    fn waits_on_ingress(serial: &Serial, turn: Turn) -> bool {
        serial.ingress.values().any(|wait| wait.turn == turn)
    }

    /// Whether a request `turn`'s handler issued waits, directly or through
    /// the calls and requests of the invocations it waits on, for an
    /// attempt that is ready to run. A request whose target cannot move —
    /// a watch on a promise nobody has resolved, say — is not what a parked
    /// handler waits on: the handler may be waiting on work of its own
    /// outside the server beside it.
    fn waits_on_ready(&self, serial: &Serial, turn: Turn) -> bool {
        let mut seen = BTreeSet::new();
        let mut next: Vec<InvKey> = serial
            .ingress
            .values()
            .filter(|wait| wait.turn == turn)
            .filter_map(|wait| wait.target)
            .collect();
        while let Some(key) = next.pop() {
            if !seen.insert(key) {
                continue;
            }
            let Status::Running(attempt) = &self.invocations[key.0].status else {
                continue;
            };
            let target = (key, attempt.number);
            if serial.ready.contains(&target) {
                return true;
            }
            next.extend(
                serial
                    .ingress
                    .values()
                    .filter(|wait| wait.turn == target)
                    .filter_map(|wait| wait.target),
            );
            next.extend(self.invocations[key.0].children.iter().copied());
        }
        false
    }

    /// Whether `invocation`'s live attempt waits on the server: nothing is
    /// held for it, and it is blocked on its input with no `ctx.run`
    /// closure in flight or parked on a request it issued — or it was woken
    /// and waits to go on between turns.
    fn is_quiet(
        serial: &Serial,
        invocation: &Invocation,
        attempt: &LiveAttempt,
        key: InvKey,
    ) -> bool {
        let turn = (key, attempt.number);
        serial.woken.contains(&turn)
            || (!attempt.has_held_work()
                && ((attempt.probe.is_idle() && invocation.pending_runs.is_empty())
                    || (attempt.probe.is_parked() && Self::waits_on_ingress(serial, turn))))
    }

    /// Whether `invocation`'s live attempt is parked inside a `ctx.run`
    /// closure on something outside the server: not quiet, with nothing
    /// held for it and no input it waits for.
    fn parked_in_run(
        serial: &Serial,
        invocation: &Invocation,
        attempt: &LiveAttempt,
        key: InvKey,
    ) -> bool {
        !invocation.pending_runs.is_empty()
            && attempt.probe.is_parked()
            && !attempt.probe.is_starved()
            && !Self::is_quiet(serial, invocation, attempt, key)
    }

    /// Whether `turn` may run: it holds the turn, it is no longer live, or
    /// the server schedules concurrently.
    pub(super) fn may_run(&self, turn: Turn) -> bool {
        let Some(serial) = &self.serial else {
            return true;
        };
        serial.holder == Some(turn) || !self.is_live(turn)
    }

    fn is_live(&self, (key, number): Turn) -> bool {
        matches!(
            &self.invocations[key.0].status,
            Status::Running(attempt) if attempt.number == number
        )
    }

    /// Whether the serial scheduler has work queued: a live attempt
    /// waiting for the turn, or an ingress request waiting to land.
    pub(super) fn serial_pending(&self) -> bool {
        self.serial.as_ref().is_some_and(|serial| {
            serial.landing
                || !serial.external.is_empty()
                || !serial.woken.is_empty()
                || serial.ready.iter().any(|turn| self.is_live(*turn))
        })
    }

    /// An ingress request from outside every attempt wants to land; `admit`
    /// fires when it may.
    pub(super) fn wait_to_land(
        &mut self,
        url: String,
        body: bytes::Bytes,
        admit: tokio::sync::oneshot::Sender<()>,
    ) {
        let Some(serial) = &mut self.serial else {
            let _ = admit.send(());
            return;
        };
        let arrival = serial.next_ticket;
        serial.next_ticket += 1;
        serial.external.insert((url, body, arrival), admit);
    }

    /// The request let in last has reached the server.
    pub(super) fn landed(&mut self) {
        if let Some(serial) = &mut self.serial {
            serial.landing = false;
            serial.progress = Instant::now();
        }
    }

    /// Whether every live attempt waits on the server or on the test:
    /// each is quiet (see [`Self::is_quiet`]) or parked inside a `ctx.run`
    /// closure, and those parked in closures are no more than the gates
    /// the test declared entered — so each such closure waits on one. A
    /// closure parked beyond them waits on work that goes on by itself.
    fn attempts_quiet(&self, serial: &Serial) -> bool {
        let mut in_runs = 0;
        for (index, invocation) in self.invocations.iter().enumerate() {
            let Status::Running(attempt) = &invocation.status else {
                continue;
            };
            let key = InvKey(index);
            if Self::is_quiet(serial, invocation, attempt, key) {
                continue;
            }
            if !Self::parked_in_run(serial, invocation, attempt, key) {
                return false;
            }
            in_runs += 1;
        }
        in_runs <= serial.gates
    }

    /// Whether the holder must give the turn up now.
    fn holder_yields(&self, serial: &Serial, turn: Turn) -> Yield {
        let (key, number) = turn;
        let invocation = &self.invocations[key.0];
        let Status::Running(attempt) = &invocation.status else {
            return Yield::Done;
        };
        if attempt.number != number {
            return Yield::Done;
        }
        // Parked inside a `ctx.run` closure on a request the closure
        // issued: what it waits on is the server. (Not before it parks: a
        // holder granted the turn to write what it has goes on first.)
        if attempt.probe.is_parked() && self.waits_in_run(serial, turn) {
            return Yield::Done;
        }
        let blocked_on_server = attempt.probe.is_idle()
            && invocation.pending_runs.is_empty()
            && !attempt.has_held_work();
        // A stalled holder is preempted only for someone else, an attempt
        // or an outside request: alone, it keeps the turn and the trace
        // does not depend on how long it took.
        let others_ready = !serial.external.is_empty()
            || !serial.woken.is_empty()
            || serial
                .ready
                .iter()
                .any(|ready| *ready != turn && self.is_live(*ready));
        if blocked_on_server {
            Yield::Done
        } else if attempt.probe.is_parked() && self.waits_on_ready(serial, turn) {
            Yield::Waiting
        } else if (!serial.external.is_empty() || !serial.woken.is_empty())
            && Self::parked_in_run(serial, invocation, attempt, key)
            && self.attempts_quiet(serial)
        {
            // Parked inside a `ctx.run` closure on a gate the test opens,
            // with everyone else quiet, while outside work waits: that work
            // goes on between turns, and may be what the gate waits on.
            Yield::Done
        } else if others_ready && serial.progress.elapsed() >= STALL {
            Yield::Stalled
        } else {
            Yield::Keep
        }
    }

    /// Move the turn on if its holder is done with it, and grant it to the
    /// first ready attempt. Returns whether a grant was made.
    pub(super) fn schedule(&mut self) -> bool {
        let Some(serial) = self.serial.take() else {
            return false;
        };
        let mut serial = serial;
        let verdict = serial
            .holder
            .map(|turn| (turn, self.holder_yields(&serial, turn)));
        if let Some((turn, verdict)) = verdict
            && verdict != Yield::Keep
        {
            if verdict == Yield::Stalled {
                self.stats.stall_preemptions += 1;
            }
            serial.holder = None;
            if let Status::Running(attempt) = &mut self.invocations[turn.0.0].status
                && attempt.number == turn.1
            {
                attempt.gate();
                // A waiting holder may still have work of its own: it queues
                // for the turn behind everything already ready. A stalled
                // one goes on between turns, as outside work does.
                let blocked = attempt.probe.is_idle() && !attempt.has_held_work();
                if !blocked {
                    match verdict {
                        Yield::Waiting => serial.ready.push_back(turn),
                        Yield::Stalled => {
                            serial.woken.insert(turn);
                        }
                        Yield::Keep | Yield::Done => {}
                    }
                }
            }
        }
        let mut granted = false;
        serial.woken.retain(|turn| self.is_live(*turn));
        if serial.holder.is_none()
            && !serial.landing
            && (!serial.external.is_empty() || !serial.woken.is_empty())
            && !serial.ready.iter().any(|turn| self.is_live(*turn))
        {
            // Between turns: let the next outside work in once every
            // attempt is quiet — or once nothing has moved for a stall.
            // Outside requests go first, in the order of their URL and
            // body, then woken attempts in invocation order.
            let blocked = self.attempts_quiet(&serial);
            let stalled = serial.progress.elapsed() >= STALL;
            if blocked || stalled {
                if !blocked {
                    self.stats.stall_preemptions += 1;
                }
                if let Some((_, admit)) = serial.external.pop_first() {
                    if admit.send(()).is_ok() {
                        serial.landing = true;
                        granted = true;
                    }
                } else if let Some(turn) = serial.woken.pop_first() {
                    serial.ready.push_back(turn);
                }
            }
        }
        if serial.holder.is_none() {
            while let Some(turn) = serial.ready.pop_front() {
                let (key, number) = turn;
                let Status::Running(attempt) = &mut self.invocations[key.0].status else {
                    continue;
                };
                if attempt.number != number {
                    continue;
                }
                attempt.release();
                serial.holder = Some(turn);
                serial.progress = Instant::now();
                serial.trace.push(turn);
                granted = true;
                break;
            }
        }
        self.serial = Some(serial);
        granted
    }
}

/// The driver of serial scheduling: moves the turn whenever an attempt
/// changes state.
pub(super) async fn drive(shared: std::sync::Weak<Shared>) {
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let activity = Arc::clone(&strong.activity);
        let notified = activity.notified();
        let granted = strong.lock().schedule();
        if granted {
            strong.turn_granted();
            continue;
        }
        drop(strong);
        let _ = tokio::time::timeout(Duration::from_millis(2), notified).await;
    }
}
