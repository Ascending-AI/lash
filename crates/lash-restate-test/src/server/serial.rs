//! Serial scheduling: one attempt runs at a time.
//!
//! Under [`Scheduling::Serial`](super::Scheduling::Serial) the server hands a
//! single turn from attempt to attempt. The attempt that holds the turn runs.
//! Every other live attempt is gated: frames the server delivers to it, and a
//! close of its input, wait until it holds the turn again, and a new attempt
//! does not start polling its handler until it first gets the turn.
//!
//! The holder gives up the turn when it can make no progress without the
//! server:
//!
//! * it is blocked on its input with everything it wrote applied and no
//!   `ctx.run` closure in flight;
//! * its stream ended (it completed, suspended, failed or crashed);
//! * it is inside a `ctx.run` closure that waits on an ingress request the
//!   closure issued. The request is attributed to the attempt through a
//!   task-local set around the attempt's task, so the server can yield to
//!   the invocation the closure waits on, and the closure resumes only once
//!   the attempt holds the turn again. A request issued outside a closure
//!   (a watch the handler polls beside its work, say) frees the turn only
//!   when the handler is blocked on its input, as any other holder;
//! * another attempt is ready, or an outside request waits to land, and the
//!   holder has applied no frame for [`STALL`] of wall time while not
//!   blocked on the server: the fallback for waits the server cannot see,
//!   such as an ingress request issued from a task the handler spawned (a
//!   task does not inherit the task-local). A waiting outside request lands
//!   before the stalled holder gets the turn back, since the holder may be
//!   waiting on it.
//!
//! The next turn goes to the attempt that became ready first: attempts become
//! ready in the order the server started them or delivered to them, under
//! the server lock. With one attempt running, that order follows from the
//! seed and the handlers' own code.
//!
//! Ingress requests from outside every attempt — test code, and tasks lash
//! spawns beside its handlers (a watcher, a worker loop) — land only between
//! turns: a request waits until no attempt can move without the server,
//! then the waiting requests land one at a time in the order of their URL
//! and body, each once the one before it has reached the server. So a
//! request issued while a turn is in progress lands at the same point on
//! every run, however soon its task got to issue it.
//!
//! What Serial does not order: work outside the server. SQLite completes on
//! its own threads; the holder keeps the turn while it awaits a store call,
//! so store calls of different attempts do not interleave, but when a store
//! call completes is the store's timing. A task lash spawns runs whenever
//! Tokio polls it, and only its ingress requests are sequenced. On a
//! multi-threaded runtime those tasks race the holder, so one seed's grant
//! order repeats only on a current-thread runtime: there every task the
//! handlers spawn runs between the holder's awaits, in one order. When a
//! holder waits on something the server cannot see for [`STALL`] (an
//! in-process channel fed by a task whose request waits to land, say), the
//! turn moves on or the request lands anyway, and
//! [`Stats::stall_preemptions`](super::Stats::stall_preemptions) counts it:
//! a run with none was fully sequenced.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Shared;
use super::model::{InvKey, Status};
use super::processor::State;

/// How long the holder may go without applying a frame, while not blocked
/// on the server, before the turn moves on anyway.
pub const STALL: Duration = Duration::from_millis(100);

/// What the holder does with the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Yield {
    Keep,
    /// Blocked on the server, waiting on its handler's ingress request, or
    /// no longer live.
    Done,
    /// Preempted after [`STALL`] without progress.
    Stalled,
}

/// One attempt of one invocation.
pub type Turn = (InvKey, u32);

/// The serial scheduler's state, under the server lock.
#[derive(Debug)]
pub struct Serial {
    holder: Option<Turn>,
    /// Wall time the holder last got the turn or applied a frame.
    progress: Instant,
    ready: VecDeque<Turn>,
    /// In-flight ingress requests each attempt's handler issued, by
    /// ticket: the `ctx.run` whose closure issued it, if one was in flight.
    ingress: BTreeMap<u64, (Turn, Option<u32>)>,
    next_ticket: u64,
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
        serial.ingress.insert(ticket, (turn, run));
        ticket
    }

    /// An ingress request answered: its handler goes on once the attempt
    /// holds the turn again.
    pub(super) fn ingress_ended(&mut self, ticket: u64) {
        let Some(serial) = &mut self.serial else {
            return;
        };
        if let Some((turn, _)) = serial.ingress.remove(&ticket) {
            self.make_ready(turn);
        }
    }

    /// Whether `turn` is inside a `ctx.run` closure waiting on an ingress
    /// request the closure issued.
    fn waits_in_run(&self, serial: &Serial, turn: Turn) -> bool {
        let pending = &self.invocations[turn.0.0].pending_runs;
        serial
            .ingress
            .values()
            .any(|(waiting, run)| *waiting == turn && run.is_some_and(|run| pending.contains(&run)))
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

    /// Whether no live attempt can move without the server.
    fn attempts_blocked(&self) -> bool {
        self.invocations
            .iter()
            .all(|invocation| match &invocation.status {
                Status::Running(attempt) => {
                    attempt.probe.is_idle()
                        && !attempt.has_held_work()
                        && invocation.pending_runs.is_empty()
                }
                _ => true,
            })
    }

    /// Whether the holder must give the turn up now.
    fn holder_yields(&self, serial: &Serial, turn: Turn) -> Yield {
        let (key, number) = turn;
        let invocation = &self.invocations[key.0];
        let Status::Running(attempt) = &invocation.status else {
            return Yield::Done;
        };
        if attempt.number != number || self.waits_in_run(serial, turn) {
            return Yield::Done;
        }
        let blocked_on_server = attempt.probe.is_idle()
            && invocation.pending_runs.is_empty()
            && !attempt.has_held_work();
        // A stalled holder is preempted only for someone else — another
        // ready attempt, or an outside request waiting to land (the holder
        // may wait on work that request unblocks): alone, it keeps the turn
        // and the trace does not depend on how long it took.
        let others_ready = !serial.external.is_empty()
            || serial
                .ready
                .iter()
                .any(|ready| *ready != turn && self.is_live(*ready));
        if blocked_on_server {
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
        // A holder stalled while an outside request waits gives the turn up
        // to that request first: it lands before any attempt is granted.
        let stalled_for_outside =
            matches!(verdict, Some((_, Yield::Stalled))) && !serial.external.is_empty();
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
                // A stalled holder still has its own work: it queues for
                // the turn behind everything already ready.
                let blocked = attempt.probe.is_idle() && !attempt.has_held_work();
                if verdict == Yield::Stalled && !blocked {
                    serial.ready.push_back(turn);
                }
            }
        }
        let mut granted = false;
        if serial.holder.is_none()
            && !serial.landing
            && !serial.external.is_empty()
            && (stalled_for_outside || !serial.ready.iter().any(|turn| self.is_live(*turn)))
        {
            // Between turns: let the next outside request in once nothing
            // can move without it — or once nothing has moved for a stall.
            let blocked = self.attempts_blocked();
            let stalled = serial.progress.elapsed() >= STALL;
            if (blocked || stalled)
                && let Some((_, admit)) = serial.external.pop_first()
            {
                if !blocked && !stalled_for_outside {
                    self.stats.stall_preemptions += 1;
                }
                if admit.send(()).is_ok() {
                    serial.landing = true;
                    granted = true;
                }
            }
        }
        if serial.holder.is_none() && !(stalled_for_outside && serial.landing) {
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
        drop(strong);
        if granted {
            activity.notify_waiters();
            continue;
        }
        let _ = tokio::time::timeout(Duration::from_millis(2), notified).await;
    }
}
