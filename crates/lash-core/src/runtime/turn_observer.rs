//! The turn's observation sink (ADR 0105 §1: observation never decides).
//!
//! Shift code publishes every host-facing event through a [`TurnObserver`]: a
//! synchronous hand-off onto the turn's observation queue that never waits and
//! never wakes the shift. What reaches the host sinks, and when, cannot change
//! anything the shift decides or commits: commit content comes from the
//! driver's recorded state ([`RecordedTurnAssembly`](super::RecordedTurnAssembly)),
//! never from this stream.
//!
//! # The host contract
//!
//! - **One ordered stream per lane.** Session events reach the host's
//!   `EventSink`, and turn activities its `TurnActivitySink`, in the order the
//!   turn published them.
//! - **Deltas arrive in frames.** The first delta of a stream block is
//!   published at once; the block's later deltas are coalesced into short
//!   frames, each named by the observation range it covers ([`framing`]).
//!   Each lane frames on its own, so a host listening to one lane only is
//!   bounded the same way, and a host that lags gets its deltas coalesced
//!   into the frame it has not taken yet. Framing never loses or reorders
//!   text, including across a cancellation: a stopped turn delivers every
//!   delta it queued, so a host that keeps up holds the streamed tail lash
//!   does not keep (ADR 0122). Every non-delta event is always queued and
//!   always delivered, behind every delta queued before it.
//! - **A stop publishes after its commit.** From the moment a turn records a
//!   `Stopped` terminal, everything it publishes is held
//!   ([`TurnObserver::hold_terminal`]) until the turn's commit is accepted,
//!   and is then released in order ([`TurnObserver::release_terminal`]). A
//!   commit that fails publishes none of it: the drive that held it is
//!   dropped.
//! - **Everything else publishes before its commit.** A durable turn waits
//!   for what it queued to reach the host ([`TurnObserver::published`])
//!   before it commits, so whoever reads the turn's terminal from the store
//!   finds its activity already published (FIG-5507).
//!
//! The host end is published by
//! [`work_with_observations`](super::work_with_observations), outside the
//! shift.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use lash_sansio::sync::MutexExt;

use super::RuntimeStreamEvent;
use super::observation_publisher::ObservationSource;
use crate::engine::{ObservationSink, ObservedEvent, ShiftObservation};
use crate::session_model::SessionStreamEvent;
use crate::{TurnActivity, TurnActivityId, TurnId};

mod framing;
pub(in crate::runtime) use framing::DeltaFraming;
use framing::{Frames, Queued};

/// The sending end of one logical turn's observation queue. Cloning it adds a
/// publisher to the same queue; [`for_turn`](Self::for_turn) adds one whose
/// activities are addressed to one physical turn.
#[derive(Clone)]
pub(in crate::runtime) struct TurnObserver {
    queue: Arc<ObservationQueue>,
    /// The physical turn this publisher's activities are addressed to.
    turn: Option<TurnId>,
    /// The host's session sink discards everything, so session events are
    /// not queued.
    quiet_sessions: bool,
    /// The host's activity sink discards everything, so activities are not
    /// queued.
    quiet_activities: bool,
}

/// The host end of one logical turn's observation queue, drained by the
/// publisher.
pub(in crate::runtime) struct TurnObservations {
    queue: Arc<ObservationQueue>,
}

type FrameWait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The publisher's wait for the next open frame to fall due. It lives
/// behind the queue's `Arc`, so the host end the turn's future holds stays
/// one pointer.
#[derive(Default)]
struct FrameTimer {
    /// The armed wait, and when it fires.
    armed: Option<(Instant, FrameWait)>,
    /// The wait fired: the earliest open frame is due, whatever the clock
    /// reads now.
    fired: bool,
}

/// One queued observation: the event, and for an activity the physical turn
/// it is addressed to.
pub(in crate::runtime) struct Observation {
    pub(in crate::runtime) turn: Option<TurnId>,
    pub(in crate::runtime) event: RuntimeStreamEvent,
}

struct ObservationQueue {
    state: Mutex<QueueState>,
    framing: DeltaFraming,
    /// Touched only by the publisher, never under `state`.
    timer: Mutex<FrameTimer>,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<Observation>,
    /// Each lane's open delta frame, queued after `events`.
    frames: Frames,
    /// The publisher has taken an event and not yet finished publishing it.
    publishing: bool,
    /// The shift is over: later publications are dropped.
    closed: bool,
    publisher: Option<Waker>,
    published_waiter: Option<Waker>,
    /// The host end is gone: nothing queued will be published.
    abandoned: bool,
    /// A stopped turn's terminal and everything after it, held until its
    /// commit is accepted.
    held: Option<Vec<Observation>>,
}

impl QueueState {
    fn drained(&self) -> bool {
        self.events.is_empty() && self.frames.is_empty() && !self.publishing
    }

    /// Queue one observation for the host, framing a delta. Returns whether
    /// the publisher must look: an event is ready, or a frame opened.
    fn push(&mut self, observation: Observation, framing: &DeltaFraming, due: Instant) -> bool {
        !matches!(
            self.frames
                .push(&mut self.events, observation, &framing.coalescing, due),
            Queued::Absorbed
        )
    }

    /// Queue every open frame, so nothing waits on a frame's interval.
    fn seal_frames(&mut self) -> bool {
        let sealed = !self.frames.is_empty();
        self.frames.seal_all(&mut self.events);
        sealed
    }
}

impl ObservationQueue {
    /// When a frame opened now falls due.
    fn frame_due(&self) -> Instant {
        self.framing.clock.now() + self.framing.coalescing.interval()
    }
}

impl TurnObserver {
    /// A new observation queue whose host sinks are `sessions` and
    /// `activities`, and the end its publisher drains.
    pub(in crate::runtime) fn open(
        sessions: &dyn crate::EventSink,
        activities: &dyn crate::TurnActivitySink,
        framing: DeltaFraming,
    ) -> (Self, TurnObservations) {
        Self::with_quiet_lanes(sessions.is_noop(), activities.is_noop(), framing)
    }

    /// An observer whose host sinks listen to everything, for tests that read
    /// the queue directly, framing on the system clock.
    #[cfg(test)]
    pub(in crate::runtime) fn unread() -> (Self, TurnObservations) {
        Self::with_quiet_lanes(
            false,
            false,
            DeltaFraming {
                clock: Arc::new(crate::SystemClock),
                coalescing: crate::runtime::DeltaCoalescing::recommended(),
            },
        )
    }

    fn with_quiet_lanes(
        quiet_sessions: bool,
        quiet_activities: bool,
        framing: DeltaFraming,
    ) -> (Self, TurnObservations) {
        let queue = Arc::new(ObservationQueue {
            state: Mutex::new(QueueState::default()),
            framing,
            timer: Mutex::new(FrameTimer::default()),
        });
        (
            Self {
                queue: Arc::clone(&queue),
                turn: None,
                quiet_sessions,
                quiet_activities,
            },
            TurnObservations { queue },
        )
    }

    /// A publisher onto the same queue whose activities are addressed to
    /// `turn_id`.
    pub(in crate::runtime) fn for_turn(&self, turn_id: &TurnId) -> Self {
        Self {
            turn: Some(turn_id.clone()),
            ..self.clone()
        }
    }

    /// Publish one event as it is, with no projection. After the shift is
    /// over the event is dropped: nothing depends on its delivery.
    pub(in crate::runtime) fn publish(&self, event: RuntimeStreamEvent) {
        let turn = match &event {
            RuntimeStreamEvent::Session(_) => None,
            RuntimeStreamEvent::Turn(_) => self.turn.clone(),
        };
        self.enqueue(Observation { turn, event });
    }

    fn enqueue(&self, observation: Observation) {
        let event = &observation.event;
        let quiet = match &event {
            RuntimeStreamEvent::Session(_) => self.quiet_sessions,
            RuntimeStreamEvent::Turn(_) => self.quiet_activities,
        };
        if quiet {
            return;
        }
        let mut state = self.queue.state.lock_recover();
        if state.closed {
            return;
        }
        if let Some(held) = state.held.as_mut() {
            held.push(observation);
            return;
        }
        if state.push(observation, &self.queue.framing, self.queue.frame_due())
            && let Some(publisher) = state.publisher.take()
        {
            drop(state);
            publisher.wake();
        }
    }

    /// Hold everything published from now on: the turn recorded a `Stopped`
    /// terminal, which no host may see before its commit (ADR 0122).
    pub(in crate::runtime) fn hold_terminal(&self) {
        let mut state = self.queue.state.lock_recover();
        if state.held.is_none() {
            state.held = Some(Vec::new());
        }
    }

    /// The commit was accepted: publish what was held, in order.
    pub(in crate::runtime) fn release_terminal(&self) {
        let mut state = self.queue.state.lock_recover();
        let Some(held) = state.held.take() else {
            return;
        };
        if state.closed {
            return;
        }
        let due = self.queue.frame_due();
        for observation in held {
            state.push(observation, &self.queue.framing, due);
        }
        if let Some(publisher) = state.publisher.take() {
            drop(state);
            publisher.wake();
        }
    }

    /// The turn is over: queue every open frame, so the publisher ends once
    /// it has published everything queued. Later publications are dropped,
    /// and whatever is still held stays unpublished.
    pub(in crate::runtime) fn close(&self) {
        let mut state = self.queue.state.lock_recover();
        state.seal_frames();
        state.closed = true;
        if let Some(publisher) = state.publisher.take() {
            drop(state);
            publisher.wake();
        }
    }

    /// Everything published so far has reached the host: every open frame
    /// is queued, and the publisher has published the queue. What a stop
    /// holds for its commit stays held. Answers at once when the host end
    /// is gone, since nothing will drain the queue.
    pub(in crate::runtime) async fn published(&self) {
        std::future::poll_fn(|context| {
            let mut state = self.queue.state.lock_recover();
            let sealed = state.seal_frames();
            if state.abandoned || state.drained() {
                return Poll::Ready(());
            }
            state.published_waiter = Some(context.waker().clone());
            let publisher = if sealed { state.publisher.take() } else { None };
            drop(state);
            if let Some(publisher) = publisher {
                publisher.wake();
            }
            Poll::Pending
        })
        .await;
    }

    /// Whether the shift is over and publications are dropped.
    pub(in crate::runtime) fn is_closed(&self) -> bool {
        self.queue.state.lock_recover().closed
    }
}

impl ObservationSource for TurnObservations {
    type Item = Observation;

    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<Observation>> {
        let mut timer = self.queue.timer.lock_recover();
        loop {
            let mut state = self.queue.state.lock_recover();
            let forced = std::mem::take(&mut timer.fired);
            let next = match state.events.pop_front() {
                Some(event) => Some(event),
                None => state
                    .frames
                    .take_due(self.queue.framing.clock.now(), forced),
            };
            if let Some(event) = next {
                state.publishing = true;
                return Poll::Ready(Some(event));
            }
            if state.closed {
                return Poll::Ready(None);
            }
            state.publisher = Some(context.waker().clone());
            let Some(due) = state.frames.next_due() else {
                timer.armed = None;
                return Poll::Pending;
            };
            drop(state);
            if timer.armed.as_ref().is_none_or(|(armed, _)| *armed != due) {
                let clock = Arc::clone(&self.queue.framing.clock);
                timer.armed = Some((due, Box::pin(async move { clock.sleep_until(due).await })));
            }
            let Some((_, wait)) = timer.armed.as_mut() else {
                return Poll::Pending;
            };
            if wait.as_mut().poll(context).is_pending() {
                return Poll::Pending;
            }
            timer.armed = None;
            timer.fired = true;
        }
    }

    fn published_one(&mut self) {
        let mut state = self.queue.state.lock_recover();
        state.publishing = false;
        if state.drained()
            && let Some(waiter) = state.published_waiter.take()
        {
            drop(state);
            waiter.wake();
        }
    }

    fn close(&mut self) {
        let mut state = self.queue.state.lock_recover();
        state.seal_frames();
        state.closed = true;
        if let Some(waiter) = state.published_waiter.take() {
            drop(state);
            waiter.wake();
        }
    }
}

impl Drop for TurnObservations {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock_recover();
        state.abandoned = true;
        if let Some(waiter) = state.published_waiter.take() {
            drop(state);
            waiter.wake();
        }
    }
}

#[cfg(test)]
impl TurnObservations {
    /// Take the next queued event without publishing it, an open frame as
    /// it stands once nothing is queued ahead of it.
    pub(in crate::runtime) fn try_take(&mut self) -> Option<RuntimeStreamEvent> {
        let mut state = self.queue.state.lock_recover();
        let next = match state.events.pop_front() {
            Some(event) => Some(event),
            None => state.frames.take_due(self.queue.framing.clock.now(), true),
        };
        next.map(|observation| observation.event)
    }

    /// How many events are queued, each open frame counting as one.
    pub(in crate::runtime) fn len(&self) -> usize {
        let state = self.queue.state.lock_recover();
        state.events.len() + state.frames.len()
    }
}

/// On the controller path, a keyed observation
/// is published synchronously, and every activity it yields takes its id from
/// its `(replay key, ordinal)`, never from a random source.
impl ObservationSink for TurnObserver {
    fn observe(&self, observation: ShiftObservation) {
        let ShiftObservation {
            key,
            ordinal,
            event,
        } = observation;
        let id = TurnActivityId::observed(&key, ordinal);
        match event {
            ObservedEvent::Session(event) => {
                if let Some(projected) = crate::engine::activity_projection(&event) {
                    self.publish(RuntimeStreamEvent::Turn(TurnActivity {
                        id: id.clone(),
                        correlation_id: id,
                        event: projected,
                    }));
                }
                self.publish(RuntimeStreamEvent::Session(event));
            }
            ObservedEvent::Activity {
                correlation_id,
                event,
            } => {
                self.publish(RuntimeStreamEvent::Turn(TurnActivity {
                    correlation_id: correlation_id.unwrap_or_else(|| id.clone()),
                    id,
                    event,
                }));
            }
            ObservedEvent::RecordedSession(event) => {
                self.publish(RuntimeStreamEvent::Session(event));
            }
            ObservedEvent::RecordedActivity(activity) => {
                self.publish(RuntimeStreamEvent::Turn(activity));
            }
        }
    }
}

/// Shift code that takes a host `EventSink` publishes through the observer:
/// the event is queued as it is, and the call never waits on the host.
#[async_trait::async_trait]
impl crate::EventSink for TurnObserver {
    fn is_noop(&self) -> bool {
        self.quiet_sessions
    }

    async fn emit(&self, event: SessionStreamEvent) {
        self.publish(RuntimeStreamEvent::Session(event));
    }
}

/// Shift code that takes a host `TurnActivitySink` publishes through the
/// observer. The observer belongs to one turn, whose publisher addresses
/// every activity to that turn.
#[async_trait::async_trait]
impl crate::TurnActivitySink for TurnObserver {
    fn is_noop(&self) -> bool {
        self.quiet_activities
    }

    async fn emit(&self, activity: TurnActivity) {
        self.publish(RuntimeStreamEvent::Turn(activity));
    }

    async fn emit_for_turn(&self, turn_id: &TurnId, activity: TurnActivity) {
        self.enqueue(Observation {
            turn: Some(turn_id.clone()),
            event: RuntimeStreamEvent::Turn(activity),
        });
    }
}

/// Which lane an event travels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Session = 0,
    Activity = 1,
}

fn lane(event: &RuntimeStreamEvent) -> Lane {
    match event {
        RuntimeStreamEvent::Session(_) => Lane::Session,
        RuntimeStreamEvent::Turn(_) => Lane::Activity,
    }
}

#[cfg(test)]
mod tests;
