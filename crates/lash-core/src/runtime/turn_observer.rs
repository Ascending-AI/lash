//! The turn's observation sink (ADR 0105 §1: observation never decides).
//!
//! Drive code publishes every host-facing event through a [`TurnObserver`]: a
//! synchronous hand-off onto the turn's observation queue that never waits and
//! never wakes the drive. What reaches the host sinks, and when, cannot change
//! anything the drive decides or commits: commit content comes from the
//! driver's recorded state ([`RecordedTurnAssembly`](super::RecordedTurnAssembly)),
//! never from this stream.
//!
//! # The host contract
//!
//! - **One ordered stream per lane.** Session events reach the host's
//!   `EventSink`, and turn activities its `TurnActivitySink`, in the order the
//!   turn published them.
//! - **Exact deltas unless the host lags.** A host that keeps up receives the
//!   provider's delta sequence exactly. Once more than [`LAG_BUDGET`] events
//!   are queued ahead of the host, a new stream delta merges into the last
//!   queued event of its own lane when that is a delta of the same block and
//!   kind. Each lane merges on its own, so a host listening to one lane only is
//!   bounded the same way. Merging never loses text, including across a
//!   cancellation: a stopped turn delivers every delta it queued, merged, so a
//!   host that keeps up holds the streamed tail lash does not keep (ADR 0122).
//!   Every non-delta event is always queued and always delivered.
//! - **A stop publishes after its commit.** From the moment a turn records a
//!   `Stopped` terminal, everything it publishes is held
//!   ([`TurnObserver::hold_terminal`]) until the turn's commit is accepted,
//!   and is then released in order ([`TurnObserver::release_terminal`]). A
//!   commit that fails publishes none of it
//!   ([`TurnObserver::abandon_terminal`]).
//! - **"Finished" comes after the last event.** The turn commits inside the
//!   drive and never waits on the host to do so. The turn's terminal
//!   publication to waiters (turn attach, await-event resolution) and the turn
//!   call's return both happen only after every event the turn queued,
//!   including its final deltas and `Done`, has been published to the host
//!   ([`TurnObserver::published`]).
//!
//! The host end is published by
//! [`drive_with_observations`](super::drive_with_observations), outside the
//! drive.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use lash_sansio::sync::MutexExt;

use super::RuntimeStreamEvent;
use super::observation_publisher::ObservationSource;
use crate::engine::{DriveObservation, ObservationSink, ObservedEvent};
use crate::session_model::SessionStreamEvent;
use crate::{TurnActivity, TurnActivityId, TurnEvent, TurnId};

/// How many events may queue ahead of the host before new stream deltas merge.
/// It is the capacity of the bounded host channel this queue replaces.
pub(in crate::runtime) const LAG_BUDGET: usize = 100;

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

/// One queued observation: the event, and for an activity the physical turn
/// it is addressed to.
pub(in crate::runtime) struct Observation {
    pub(in crate::runtime) turn: Option<TurnId>,
    pub(in crate::runtime) event: RuntimeStreamEvent,
}

struct ObservationQueue {
    state: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<Observation>,
    /// The publisher has taken an event and not yet finished publishing it.
    publishing: bool,
    /// The drive is over: later publications are dropped.
    closed: bool,
    publisher: Option<Waker>,
    published_waiter: Option<Waker>,
    /// A stopped turn's terminal and everything after it, held until its
    /// commit is accepted.
    held: Option<Vec<Observation>>,
}

impl QueueState {
    fn drained(&self) -> bool {
        self.events.is_empty() && !self.publishing
    }
}

impl TurnObserver {
    /// A new observation queue whose host sinks are `sessions` and
    /// `activities`, and the end its publisher drains.
    pub(in crate::runtime) fn open(
        sessions: &dyn crate::EventSink,
        activities: &dyn crate::TurnActivitySink,
    ) -> (Self, TurnObservations) {
        Self::with_quiet_lanes(sessions.is_noop(), activities.is_noop())
    }

    /// An observer whose host sinks listen to everything, for tests that read
    /// the queue directly.
    #[cfg(test)]
    pub(in crate::runtime) fn unread() -> (Self, TurnObservations) {
        Self::with_quiet_lanes(false, false)
    }

    fn with_quiet_lanes(quiet_sessions: bool, quiet_activities: bool) -> (Self, TurnObservations) {
        let queue = Arc::new(ObservationQueue {
            state: Mutex::new(QueueState::default()),
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

    /// Publish one event as it is, with no projection. After the drive is
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
        push_observation(&mut state, observation);
        if let Some(publisher) = state.publisher.take() {
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
        for observation in held {
            push_observation(&mut state, observation);
        }
        if let Some(publisher) = state.publisher.take() {
            drop(state);
            publisher.wake();
        }
    }

    /// The commit failed: nothing held is ever published.
    pub(in crate::runtime) fn abandon_terminal(&self) {
        self.queue.state.lock_recover().held = None;
    }

    /// Whether the drive is over and publications are dropped.
    pub(in crate::runtime) fn is_closed(&self) -> bool {
        self.queue.state.lock_recover().closed
    }

    /// Resolves once every event queued so far has been published to the
    /// host. The turn awaits it after its commit and before it announces the
    /// turn finished, so no waiter sees "finished" before the host has seen
    /// the turn's last event.
    pub(in crate::runtime) fn published(&self) -> Published<'_> {
        Published { observer: self }
    }
}

/// The future [`TurnObserver::published`] returns.
pub(in crate::runtime) struct Published<'a> {
    observer: &'a TurnObserver,
}

impl Future for Published<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut state = self.observer.queue.state.lock_recover();
        if state.closed || state.drained() {
            return Poll::Ready(());
        }
        state.published_waiter = Some(context.waker().clone());
        Poll::Pending
    }
}

impl ObservationSource for TurnObservations {
    type Item = Observation;

    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<Observation>> {
        let mut state = self.queue.state.lock_recover();
        if let Some(event) = state.events.pop_front() {
            state.publishing = true;
            return Poll::Ready(Some(event));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.publisher = Some(context.waker().clone());
        Poll::Pending
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
        state.closed = true;
        if let Some(waiter) = state.published_waiter.take() {
            drop(state);
            waiter.wake();
        }
    }
}

#[cfg(test)]
impl TurnObservations {
    /// Take the next queued event without publishing it.
    pub(in crate::runtime) fn try_take(&mut self) -> Option<RuntimeStreamEvent> {
        self.queue
            .state
            .lock_recover()
            .events
            .pop_front()
            .map(|observation| observation.event)
    }

    /// How many events are queued.
    pub(in crate::runtime) fn len(&self) -> usize {
        self.queue.state.lock_recover().events.len()
    }
}

/// On the controller path, a keyed observation
/// is published synchronously, and every activity it yields takes its id from
/// its `(replay key, ordinal)`, never from a random source.
impl ObservationSink for TurnObserver {
    fn observe(&self, observation: DriveObservation) {
        let DriveObservation {
            key,
            ordinal,
            event,
        } = observation;
        let id = TurnActivityId::new(format!("{key}#{ordinal}"));
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

/// Drive code that takes a host `EventSink` publishes through the observer:
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

/// Drive code that takes a host `TurnActivitySink` publishes through the
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

/// Which lane an event travels, and whether it is a stream delta of a block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Session,
    Activity,
}

/// A stream delta's lane, kind and block: two deltas merge only when all
/// three match.
#[derive(PartialEq, Eq)]
struct DeltaKey<'a> {
    lane: Lane,
    reasoning: bool,
    block_id: &'a str,
}

fn lane(event: &RuntimeStreamEvent) -> Lane {
    match event {
        RuntimeStreamEvent::Session(_) => Lane::Session,
        RuntimeStreamEvent::Turn(_) => Lane::Activity,
    }
}

fn delta_key(event: &RuntimeStreamEvent) -> Option<DeltaKey<'_>> {
    let (lane, reasoning, block_id) = match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { block, .. }) => {
            (Lane::Session, false, block.id.as_str())
        }
        RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { block, .. }) => {
            (Lane::Session, true, block.id.as_str())
        }
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::AssistantProseDelta { block, .. },
            ..
        }) => (Lane::Activity, false, block.id.as_str()),
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::ReasoningDelta { block, .. },
            ..
        }) => (Lane::Activity, true, block.id.as_str()),
        _ => return None,
    };
    Some(DeltaKey {
        lane,
        reasoning,
        block_id,
    })
}

/// Merge `incoming` into the last queued event of its lane when the host lags
/// more than [`LAG_BUDGET`] events behind and that event is a delta of the
/// same block and kind. Returns whether it merged.
fn merge_lagging_delta(events: &mut VecDeque<Observation>, incoming: &Observation) -> bool {
    if events.len() <= LAG_BUDGET {
        return false;
    }
    let Some(incoming_key) = delta_key(&incoming.event) else {
        return false;
    };
    let incoming_lane = incoming_key.lane;
    let Some(position) = events
        .iter()
        .rposition(|queued| lane(&queued.event) == incoming_lane)
    else {
        return false;
    };
    // Only the lagging tail may absorb a delta: an event within the budget is
    // next in line for the host and is published as it is.
    if position < LAG_BUDGET {
        return false;
    }
    let Some(last) = events.get_mut(position) else {
        return false;
    };
    if last.turn != incoming.turn || delta_key(&last.event) != Some(incoming_key) {
        return false;
    }
    match (&mut last.event, &incoming.event) {
        (
            RuntimeStreamEvent::Session(
                SessionStreamEvent::TextDelta { content, .. }
                | SessionStreamEvent::ReasoningDelta { content, .. },
            ),
            RuntimeStreamEvent::Session(
                SessionStreamEvent::TextDelta { content: more, .. }
                | SessionStreamEvent::ReasoningDelta { content: more, .. },
            ),
        ) => {
            content.push_str(more);
            true
        }
        (
            RuntimeStreamEvent::Turn(TurnActivity {
                event:
                    TurnEvent::AssistantProseDelta { text, .. } | TurnEvent::ReasoningDelta { text, .. },
                ..
            }),
            RuntimeStreamEvent::Turn(TurnActivity {
                event:
                    TurnEvent::AssistantProseDelta { text: more, .. }
                    | TurnEvent::ReasoningDelta { text: more, .. },
                ..
            }),
        ) => {
            let mut merged = String::with_capacity(text.len() + more.len());
            merged.push_str(text);
            merged.push_str(more);
            *text = merged.into();
            true
        }
        _ => false,
    }
}

/// Queue one observation for the host: a lagging delta merges into the tail.
fn push_observation(state: &mut QueueState, observation: Observation) {
    if !merge_lagging_delta(&mut state.events, &observation) {
        state.events.push_back(observation);
    }
}

#[cfg(test)]
mod tests;
