//! Following one input or run to its answer (FIG-3600 S5b, D1 §1.3–§1.5).
//!
//! A follower subscribes to its session's committed changes and to the
//! session's live replay from its cursor, adopts the activity of the run
//! that applies its subject, and resolves the subject from the store on
//! every wake. Answers always come from the store, even when the replay
//! window is gone or another node executes the run (FIG-4345).
//!
//! Where a listener of this process hears every process's commits (a node
//! this process serves, or the store set's observer, FIG-7055), the
//! session's change hints are what wake a resolution: every owner commit of
//! the session, on whichever node, names the session to every listener. Each
//! drained hint costs one fresh authoritative read, and a hint that arrives
//! during a read leaves the session dirty for the next. A timed read at the
//! follow poll's ceiling stays, for a hint that was lost.
//!
//! Where none listens (a store that lives in one process, or a host that
//! publishes no node wakes), the replay's commit and queue changes and a
//! bounded poll wake the resolutions. Events the replay already holds wake
//! one resolution between them, not one each: a follower that replays a
//! session's retained history reads the store once for all of it (FIG-5915).
//! Once it knows its run, the terminal polling schedule also wakes that
//! resolution path. Both schedules share the same durable read: no separate
//! SQL waiter races resolution and gets abandoned when the answer arrives.
//! Until adoption, a follower with no resident runtime resolves its input at
//! the poll floor (FIG-3981).
//!
//! That a run ended is the store's fact alone: its terminal. A follower that
//! reads it answers at once, with what the replay holds past its cursor, and
//! waits for no observation of the commit, which a node that died after
//! committing never publishes. The replay is whole by then: a turn's node
//! publishes the turn's activity before it commits (FIG-5507). Only the
//! turn's outcome activity is published after its commit (ADR 0122,
//! FIG-5800), and the outcome it carries is the terminal's. A follower that
//! has been receiving a run's activity reads on for it, until the commit's
//! own observation that follows it, for at most the follow poll's ceiling
//! (FIG-5793).

use lash_protocol_types::core_support::RunIdCoreSupport;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt as _, StreamExt};
use lash_core::facade_support::TurnOutcome;
use lash_core::facade_support::{FollowRead, LiveReplayGap};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::store::PhysicalTurn;
use lash_core::{
    LiveReplayGapReason, LiveReplayOutcome, LiveReplaySubscribeOutcome, LiveReplaySubscription,
    SessionCursor, SessionObservationEvent, SessionObservationEventPayload, SessionRevision,
    TurnActivity, TurnEvent, TurnId,
};
use lash_durable::runner::KeyWatch;
use tokio::sync::mpsc;

use super::resolve::{self, Resolution};
use super::{SendContext, SendOutcome};
use crate::error::{EmbedError, Result, SendError};
use crate::support::TurnActivitySink;
use crate::turn::TurnOutput;

/// What a follower follows.
#[derive(Clone, Debug)]
pub(super) enum Subject {
    /// An accepted input: its run is the one whose turn applies it.
    Input(TurnInputAcceptanceReceipt),
    /// A logical run, by id.
    Run(TurnId),
}

/// Where a follower forwards the activity it adopts.
pub(super) enum Tap<'a> {
    /// Collect only.
    Quiet,
    /// Forward to a host sink as it arrives.
    Sink(&'a dyn TurnActivitySink),
    /// Forward to an events stream; a gap ends the stream with one error.
    Channel(mpsc::Sender<Result<TurnActivity>>),
}

impl Tap<'_> {
    pub(super) async fn activity(&mut self, activity: &TurnActivity) {
        match self {
            Self::Quiet => {}
            Self::Sink(sink) => sink.emit(activity.clone()).await,
            Self::Channel(tx) => {
                if tx.send(Ok(activity.clone())).await.is_err() {
                    // The stream was dropped: nothing listens any more, and
                    // dropping a stream stops nothing.
                    *self = Self::Quiet;
                }
            }
        }
    }

    /// Report `gap` in-stream. A stream keeps delivering what follows the
    /// gap: the follower resubscribes past it.
    async fn gap(&mut self, gap: &LiveReplayGap) {
        if let Self::Channel(tx) = self
            && tx
                .send(Err(EmbedError::from(SendError::ObservationGap(
                    gap.clone(),
                ))))
                .await
                .is_err()
        {
            *self = Self::Quiet;
        }
    }
}

/// The adoption filter: which physical turns' activity is the subject's.
struct Adoption {
    subject: Subject,
    run: Option<TurnId>,
    // Before the durable binding is known, most replayed activities can
    // belong to earlier runs. Share their records until adoption selects
    // the ones to deliver, rather than copying every retained payload.
    buffered: VecDeque<Arc<SessionObservationEvent>>,
    buffer_capacity: usize,
    collected: Vec<TurnActivity>,
    /// The turn of the last activity delivered.
    last_turn: Option<TurnId>,
    /// Every turn whose commit this follower observed.
    committed: HashSet<TurnId>,
}

impl Adoption {
    fn new(subject: &Subject, buffer_capacity: usize) -> Self {
        Self {
            run: match subject {
                Subject::Run(run) => Some(run.clone()),
                Subject::Input(_) => None,
            },
            subject: subject.clone(),
            buffered: VecDeque::new(),
            buffer_capacity,
            collected: Vec::new(),
            last_turn: None,
            committed: HashSet::new(),
        }
    }

    /// Whether the turn this follower last heard the run from has not yet
    /// been observed committing.
    fn awaits_commit(&self) -> bool {
        self.last_turn
            .as_ref()
            .is_some_and(|turn| !self.committed.contains(turn))
    }

    fn adopts(&self, turn: &TurnId) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| PhysicalTurn::physical_ordinal_of(run, turn).is_some())
    }

    /// Adopt `run`, releasing the buffered activity of its turns.
    async fn adopt(&mut self, run: TurnId, tap: &mut Tap<'_>) {
        if self.run.is_some() {
            return;
        }
        self.run = Some(run);
        let buffered = std::mem::take(&mut self.buffered);
        for event in buffered {
            if let (Some(turn), SessionObservationEventPayload::TurnActivity(activity)) =
                (&event.turn_id, &event.payload)
                && self.adopts(turn)
            {
                self.deliver(turn.clone(), activity.clone(), tap).await;
            }
        }
    }

    async fn deliver(&mut self, turn: TurnId, activity: TurnActivity, tap: &mut Tap<'_>) {
        tap.activity(&activity).await;
        self.collected.push(activity);
        self.last_turn = Some(turn);
    }

    /// One observation event. Answers whether it should wake a resolve.
    async fn observe(&mut self, event: &Arc<SessionObservationEvent>, tap: &mut Tap<'_>) -> bool {
        match &event.payload {
            SessionObservationEventPayload::TurnActivity(activity) => {
                let Some(turn) = event.turn_id.as_ref() else {
                    return false;
                };
                // An application wakes resolution through the input's durable
                // binding. Its physical turn's spelling cannot name the run.
                let applied = self.run.is_none()
                    && matches!((&self.subject, &activity.event),
                        (Subject::Input(receipt), TurnEvent::QueuedInputAccepted { applications })
                        if applications.iter().any(|application| application.input_id == receipt.input_id));
                if self.adopts(turn) {
                    self.deliver(turn.clone(), activity.clone(), tap).await;
                } else if self.run.is_none() {
                    if self.buffered.len() >= self.buffer_capacity {
                        self.buffered.pop_front();
                    }
                    self.buffered.push_back(Arc::clone(event));
                }
                applied
            }
            SessionObservationEventPayload::Committed { .. } => {
                if let Some(turn) = event.turn_id.as_ref() {
                    self.committed.insert(turn.clone());
                }
                true
            }
            SessionObservationEventPayload::QueueChanged { .. } => true,
            _ => false,
        }
    }
}

/// The live replay subscription, or why there is none.
enum Replay {
    Live(LiveReplaySubscription),
    Ended,
}

fn replay_gap(
    ctx: &SendContext,
    cursor: &SessionCursor,
    reason: LiveReplayGapReason,
) -> LiveReplayGap {
    let revision = cursor
        .parse_for_session(&ctx.parts.session_id)
        .map(|parsed| parsed.revision)
        .unwrap_or(SessionRevision::new(0));
    LiveReplayGap {
        session_id: ctx.parts.session_id.clone(),
        requested_cursor: cursor.clone(),
        latest_cursor: ctx
            .parts
            .live_replay_store
            .current_cursor(&ctx.parts.session_id, revision),
        latest_revision: revision,
        reason,
    }
}

/// Where a follower stands in its subject's live activity: the replay
/// cursor to go on from, and whether it has observed any of the run's
/// activity. A windowed follower hands its position to the next window, which
/// persists it between probes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Position {
    pub(crate) cursor: SessionCursor,
    pub(crate) observed: bool,
}

impl Position {
    pub(crate) fn at(cursor: SessionCursor) -> Self {
        Self {
            cursor,
            observed: false,
        }
    }
}

/// What one follow answered.
pub(super) enum Followed {
    /// The subject stopped moving. Its gaps are the ones this follow met.
    Answered(Box<SendOutcome>),
    /// The window closed first: go on from `position`. `gaps` are the ones
    /// this window met, and only those: the caller keeps what earlier
    /// windows met, so a journaled window never records them again.
    Pending {
        position: Position,
        gaps: Vec<LiveReplayGap>,
    },
}

/// A follower's live replay: its subscription, the cursor it last read, and
/// the gaps it met in this follow. A gap is reported, then the follower
/// resubscribes before everything the replay still retains, so a gap is
/// bounded to what the replay lost: what a restarted stream published before
/// the follower resubscribed is delivered, not skipped (FIG-5486).
struct Observation {
    replay: Replay,
    last_cursor: SessionCursor,
    gaps: Vec<LiveReplayGap>,
}

impl Observation {
    async fn subscribe(ctx: &SendContext, from: Position, tap: &mut Tap<'_>) -> Self {
        let mut observation = Self {
            replay: Replay::Ended,
            last_cursor: from.cursor,
            gaps: Vec::new(),
        };
        observation.resubscribe(ctx, tap).await;
        observation
    }

    /// Subscribe after `last_cursor`, or past a gap from the start of what
    /// the replay retains: a trimmed window, a restarted stream, or a cursor
    /// this process's replay cannot place (one taken in another process,
    /// before a restart).
    async fn resubscribe(&mut self, ctx: &SendContext, tap: &mut Tap<'_>) {
        let store = &ctx.parts.live_replay_store;
        let reason = match store.subscribe_after_cursor(&self.last_cursor).await {
            Ok(LiveReplaySubscribeOutcome::Subscribed(subscription)) => {
                self.replay = Replay::Live(subscription);
                return;
            }
            Ok(LiveReplaySubscribeOutcome::Gap(reason)) => reason,
            Err(error) => {
                tracing::debug!(
                    session_id = %ctx.parts.session_id,
                    error = %error,
                    "send handle's cursor is not this process's live replay; observing from its head"
                );
                LiveReplayGapReason::Unavailable
            }
        };
        let gap = replay_gap(ctx, &self.last_cursor, reason);
        self.last_cursor = store.earliest_cursor(&ctx.parts.session_id, gap.latest_revision);
        self.report(gap, tap).await;
        self.replay = match store.subscribe_after_cursor(&self.last_cursor).await {
            Ok(LiveReplaySubscribeOutcome::Subscribed(subscription)) => Replay::Live(subscription),
            _ => Replay::Ended,
        };
    }

    /// Observe every event the replay already holds, without waiting for
    /// another: what a resolution about to read the store would otherwise
    /// be woken by, one event at a time. A lost subscription is resubscribed
    /// once, and its replay is left to the next wake.
    async fn take_ready(&mut self, ctx: &SendContext, adoption: &mut Adoption, tap: &mut Tap<'_>) {
        loop {
            let Replay::Live(subscription) = &mut self.replay else {
                return;
            };
            match subscription.next().now_or_never() {
                Some(Some(Ok(event))) => {
                    self.last_cursor = event.cursor.clone();
                    let _ = adoption.observe(&event, tap).await;
                }
                Some(Some(Err(error))) => {
                    tracing::debug!(
                        session_id = %ctx.parts.session_id,
                        error = %error,
                        "send handle's live replay lost events; resubscribing past the gap"
                    );
                    self.lost(ctx, tap).await;
                    return;
                }
                Some(None) => {
                    self.replay = Replay::Ended;
                    return;
                }
                None => return,
            }
        }
    }

    async fn report(&mut self, gap: LiveReplayGap, tap: &mut Tap<'_>) {
        tap.gap(&gap).await;
        self.gaps.push(gap);
    }

    /// The subscription ended under the follower: resubscribe from its
    /// cursor. A follower that only lagged loses nothing; one whose cursor
    /// the replay no longer holds reports the gap.
    async fn lost(&mut self, ctx: &SendContext, tap: &mut Tap<'_>) {
        self.replay = Replay::Ended;
        self.resubscribe(ctx, tap).await;
    }
}

/// Follow `subject` from `from` until it answers, or until `window` closes.
///
/// A follower answers from the store, never from events. Activity is only
/// what this process's live replay published: a run that ran in another
/// process is observed through its durable record alone, and a settled run
/// whose activity this follower never saw answers with a reported
/// [`LiveReplayGapReason::Unavailable`] gap, so a collected activity list is
/// never mistaken for the run's whole history.
pub(super) async fn follow(
    ctx: &SendContext,
    subject: &Subject,
    from: Position,
    tap: &mut Tap<'_>,
    window: Option<Duration>,
) -> Result<Followed> {
    let deadline = window.map(|window| tokio::time::Instant::now() + window);
    // Subscribed before the first read, so no change after it is missed.
    let mut changes = watch_session(ctx).await;
    let mut adoption = Adoption::new(subject, ctx.parts.observer_pacing.follow_buffer.get());
    let observed_before = from.observed;
    let mut observation = Observation::subscribe(ctx, from, tap).await;
    // Where this follow's observation starts: past the gap, when the cursor
    // it was handed is gone, so a later window never meets that gap again.
    let start_cursor = observation.last_cursor.clone();
    let terminal_pacing = ctx.parts.observer_pacing.terminal;
    let mut terminal_poll = terminal_pacing.initial();
    let mut terminal_at = None;
    let pacing = ctx.parts.observer_pacing.follow;
    // A hinted follower's timed read is loss recovery alone: at the
    // ceiling, from its last read.
    let mut poll = if changes.is_some() {
        pacing.maximum()
    } else {
        pacing.initial()
    };
    // The store poll is due `poll` after the last resolve, and the floor
    // read ticks at the floor, whatever wakes in between: a wake that
    // resolves nothing (another run's activity) delays neither.
    let mut poll_at = tokio::time::Instant::now() + poll;
    let mut floor_ticks = tokio::time::interval_at(poll_at, pacing.initial());
    floor_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut resolve_now = true;
    let mut read = FollowRead::First;
    let mut last_pass = false;
    loop {
        if resolve_now {
            observation.take_ready(ctx, &mut adoption, tap).await;
            if let Some(changes) = &mut changes {
                lash_core::facade_support::witness_follow_heard(changes.seen());
            }
            let resolution = match changes
                .as_ref()
                .and_then(|watch| resolve::committed(watch, subject))
            {
                Some(committed) => committed,
                None => {
                    lash_core::facade_support::witness_follow_read(read);
                    resolve::resolve(&ctx.parts, subject).await?
                }
            };
            match resolution {
                Resolution::OperationSettled { run, outcome } => {
                    adoption.adopt(run.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Ok(Followed::Answered(Box::new(
                        SendOutcome::OperationSettled {
                            run: crate::RunId::from_turn_id(run),
                            outcome,
                            gaps: observation.gaps,
                        },
                    )));
                }
                Resolution::Settled { run, outcome } => {
                    adoption.adopt(run.clone(), tap).await;
                    await_outcome_activity(ctx, &mut adoption, &mut observation, tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    let observed = observed_before || !adoption.collected.is_empty();
                    return finish_settled(
                        ctx,
                        run,
                        outcome,
                        adoption.collected,
                        observation,
                        observed,
                        tap,
                    )
                    .await
                    .map(|outcome| Followed::Answered(Box::new(outcome)));
                }
                Resolution::Refused { run, refusal } => {
                    adoption.adopt(run.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Ok(Followed::Answered(Box::new(SendOutcome::Refused {
                        run: Some(crate::RunId::from_turn_id(run)),
                        refusal: Box::new(refusal),
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Parked { run, reason } => {
                    adoption.adopt(run.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Ok(Followed::Answered(Box::new(SendOutcome::Parked {
                        parked: crate::ParkedTurn {
                            session_id: ctx.parts.session_id.clone(),
                            run: crate::RunId::from_turn_id(run),
                            reason,
                        },
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Faulted(fault) => {
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Err(EmbedError::Runtime(fault));
                }
                resolution @ (Resolution::Withdrawn
                | Resolution::Unknown
                | Resolution::Forsaken) => {
                    let gaps = observation.gaps;
                    return Ok(Followed::Answered(Box::new(match resolution {
                        Resolution::Withdrawn => SendOutcome::Withdrawn { gaps },
                        Resolution::Forsaken => SendOutcome::Forsaken { gaps },
                        _ => SendOutcome::Unknown { gaps },
                    })));
                }
                Resolution::Undecided { run } => {
                    if let Some(run) = run {
                        adoption.adopt(run, tap).await;
                    }
                    // A fresh authoritative read satisfies both schedules.
                    // The terminal schedule wakes this same path instead of
                    // starting a competing SQL read inside the select below.
                    // A hinted follower hears its run's terminal commit.
                    if adoption.run.is_some() && changes.is_none() {
                        terminal_at = Some(tokio::time::Instant::now() + terminal_poll);
                    }
                }
            }
            if last_pass {
                // The window closed on an undecided subject. A follower that
                // adopted no run yet goes on from where it started, so the
                // activity it buffered is read again.
                let cursor = if adoption.run.is_some() {
                    observation.last_cursor
                } else {
                    start_cursor
                };
                return Ok(Followed::Pending {
                    position: Position {
                        cursor,
                        observed: observed_before || !adoption.collected.is_empty(),
                    },
                    gaps: observation.gaps,
                });
            }
            poll_at = tokio::time::Instant::now() + poll;
        }
        resolve_now = true;
        // Wait for the first wake. A follower that resolves at the floor
        // has no slower poll to wait for.
        let floor = changes.is_none() && at_floor(ctx, subject, &adoption);
        let tick = async {
            if floor {
                floor_ticks.tick().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let polled = async {
            if floor {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep_until(poll_at).await;
            }
        };
        let closes = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let terminal_tick = async {
            match terminal_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = changed(&mut changes) => read = FollowRead::Hint,
            event = next_event(&mut observation.replay) => {
                read = FollowRead::Event;
                // A hinted follower's events are activity alone: the
                // commits they report are hinted.
                resolve_now = changes.is_none();
                match event {
                    Some(Ok(event)) => {
                        observation.last_cursor = event.cursor.clone();
                        let wakes = adoption.observe(&event, tap).await;
                        resolve_now &= wakes;
                        if resolve_now {
                            poll = pacing.initial();
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!(
                            session_id = %ctx.parts.session_id,
                            error = %error,
                            "send handle's live replay lost events; resubscribing past the gap"
                        );
                        observation.lost(ctx, tap).await;
                    }
                    None => observation.replay = Replay::Ended,
                }
            }
            () = terminal_tick => {
                read = FollowRead::Timer;
                terminal_poll = terminal_pacing.next(terminal_poll);
            }
            () = tick => read = FollowRead::Timer,
            () = polled => {
                read = FollowRead::Timer;
                if changes.is_none() {
                    poll = pacing.next(poll);
                }
            }
            () = closes => {
                read = FollowRead::Timer;
                last_pass = true;
            }
        }
    }
}

/// Watch `ctx`'s session for committed changes, when a listener of this
/// process hears every process's: a node it serves, or the store set's
/// observer, opened through the node wakes the engine publishes through.
/// `None` when none listens: the follower then reads on the replay's events
/// and its polls.
async fn watch_session(ctx: &SendContext) -> Option<KeyWatch> {
    let actor = lash_durable::ActorKey::session(ctx.parts.session_id.as_str()).ok()?;
    let backend = ctx.parts.effect_host.backend();
    let stores = backend.stores();
    let transport = (backend.config().settings().notifier == lash_durable::Notifier::AfterCommit)
        .then(|| stores.node_wakes())
        .flatten();
    let watch = stores.wake_sink().observe(actor, transport).await;
    watch.hears_every_process().then_some(watch)
}

/// The next change `changes` hears; never, without a watch.
async fn changed(changes: &mut Option<KeyWatch>) {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}

/// Whether a follower resolves at the poll floor: an input subject, until
/// its run is known, on a follower with no resident runtime. Such a
/// follower reads a run that may run in another process, which publishes
/// nothing on this process's replay, so on a poll that backs off to a second
/// it would learn the run and start its terminal polling schedule that late
/// (FIG-3981). The admission that writes the binding runs in the worker's
/// process and announces it to no other, so there is no event to wait on
/// instead. Each floor read is one keyed snapshot of the input's facts, which
/// answers its binding, its withdrawal and its session's fault at once.
fn at_floor(ctx: &SendContext, subject: &Subject, adoption: &Adoption) -> bool {
    matches!(subject, Subject::Input(_)) && adoption.run.is_none() && !ctx.live
}

async fn next_event(
    replay: &mut Replay,
) -> Option<Result<std::sync::Arc<SessionObservationEvent>>> {
    match replay {
        Replay::Live(subscription) => subscription.next().await.map(|event| {
            event.map_err(|error| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::QueuedWork,
                    error.to_string(),
                ))
            })
        }),
        Replay::Ended => std::future::pending().await,
    }
}

/// Read on for a settled run's outcome activity. Its node holds it for the
/// run's commit and publishes it after (ADR 0122, FIG-5800), and then the
/// commit's own observation, so the store can show the run ended before the
/// replay holds its outcome: a finish or a reported failure the host's feed
/// would otherwise miss. A
/// follower that has been receiving the run's activity reads until it
/// observes the commit of the turn it last heard the run from, for at most
/// the follow poll's ceiling: a node that died after committing publishes
/// neither (FIG-5793).
async fn await_outcome_activity(
    ctx: &SendContext,
    adoption: &mut Adoption,
    observation: &mut Observation,
    tap: &mut Tap<'_>,
) {
    let deadline = tokio::time::Instant::now() + ctx.parts.observer_pacing.follow.maximum();
    while adoption.awaits_commit() {
        tokio::select! {
            event = next_event(&mut observation.replay) => match event {
                Some(Ok(event)) => {
                    observation.last_cursor = event.cursor.clone();
                    let _ = adoption.observe(&event, tap).await;
                }
                Some(Err(_)) => observation.lost(ctx, tap).await,
                None => {
                    observation.replay = Replay::Ended;
                    return;
                }
            },
            () = tokio::time::sleep_until(deadline) => return,
        }
    }
}

/// Deliver what the live replay already holds past the last cursor read: the
/// subject's run ended in the store, and its node published the run's
/// activity before the commit that ended it.
async fn drain(
    ctx: &SendContext,
    adoption: &mut Adoption,
    observation: &mut Observation,
    tap: &mut Tap<'_>,
) {
    if matches!(observation.replay, Replay::Ended) {
        return;
    }
    observation.replay = Replay::Ended;
    if let Ok(LiveReplayOutcome::Replayed(events)) = ctx
        .parts
        .live_replay_store
        .replay_after_cursor(&observation.last_cursor)
        .await
    {
        for event in events {
            observation.last_cursor = event.cursor.clone();
            let _ = adoption.observe(&event, tap).await;
        }
    }
}

async fn finish_settled(
    ctx: &SendContext,
    run: TurnId,
    outcome: TurnOutcome,
    activities: Vec<TurnActivity>,
    mut observation: Observation,
    observed: bool,
    tap: &mut Tap<'_>,
) -> Result<SendOutcome> {
    if !observed {
        // The run settled, yet none of its activity reached this follower:
        // it ran in another process, or before this follower's cursor.
        let gap = replay_gap(
            ctx,
            &observation.last_cursor,
            LiveReplayGapReason::Unavailable,
        );
        observation.report(gap, tap).await;
    }
    Ok(SendOutcome::Settled {
        run: crate::RunId::from_turn_id(run),
        output: Box::new(TurnOutput {
            outcome,
            activities,
        }),
        gaps: observation.gaps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-6386: an unbound follower must not copy the session's retained
    /// activity payloads before it knows which run consumed its input.
    #[tokio::test]
    async fn unbound_adoption_shares_replay_payloads_until_the_run_is_known() {
        let session = crate::SessionId::fixture("adoption-sharing");
        let subject = Subject::Input(TurnInputAcceptanceReceipt {
            input_id: crate::InputId::fixture("input"),
            session_id: session.clone(),
            source_key: None,
            ingress: lash_core::TurnInputIngress::next_turn(),
        });
        let mut adoption = Adoption::new(&subject, 2);
        let mut tap = Tap::Quiet;
        let activity = |turn: &str, position| {
            Arc::new(
                SessionObservationEvent::new(
                    Some(TurnId::fixture(turn)),
                    SessionCursor::new(
                        "incarnation",
                        session.as_str(),
                        SessionRevision::new(0),
                        position,
                    ),
                    SessionObservationEventPayload::TurnActivity(TurnActivity::independent(
                        TurnEvent::CodeBlockCompleted {
                            language: "typescript".to_owned(),
                            prints: Some(vec!["retained output".repeat(1024).into()]),
                            prints_retained: None,
                            result: lash_core::CellOutcome::Completed,
                            duration_ms: Some(1),
                            tool_call_ids: Some(Vec::new()),
                            graph_key: None,
                        },
                    )),
                )
                .expect("valid replay cursor"),
            )
        };
        let evicted = activity("earlier-run", 1);
        let unrelated = activity("another-run", 2);
        let selected = activity("consuming-run", 3);
        for event in [&evicted, &unrelated, &selected] {
            adoption.observe(event, &mut tap).await;
            assert_eq!(
                Arc::strong_count(event),
                2,
                "unadopted activity must share the replay record instead of copying its payload"
            );
        }
        assert_eq!(Arc::strong_count(&evicted), 1, "the buffer remains bounded");
        adoption
            .adopt(TurnId::fixture("consuming-run"), &mut tap)
            .await;
        assert_eq!(Arc::strong_count(&unrelated), 1);
        assert_eq!(Arc::strong_count(&selected), 1);
        assert_eq!(adoption.collected.len(), 1);
        let SessionObservationEventPayload::TurnActivity(expected) = &selected.payload else {
            panic!("fixture is an activity")
        };
        assert_eq!(adoption.collected[0].id, expected.id);
    }
}
