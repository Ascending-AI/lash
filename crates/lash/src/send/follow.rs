//! Following one input or run to its answer (FIG-3600 S5b, D1 §1.3–§1.5).
//!
//! A follower subscribes to the session's live replay from its cursor, adopts
//! the activity of the run that applies its subject, and resolves the
//! subject from the store on every wake: a commit or queue change on the
//! observation, and a bounded poll. Once it knows its run, it
//! also holds one open wait on the run's published terminal, which wakes it
//! as soon as a run that ran in another process settles; a follower with no
//! resident runtime probes for that run at the poll floor (FIG-3981). It
//! never answers from events or from the wait: a follower whose replay window
//! is gone still answers from the store, and so does one whose engine cannot
//! answer the wait (FIG-4345).
//!
//! That a run ended is the store's fact alone: its terminal. A follower that
//! reads it answers at once, with what the replay holds past its cursor, and
//! waits for no observation of the commit, which a node that died after
//! committing never publishes. The replay is whole by then: a turn's node
//! publishes the turn's activity before it commits (FIG-5507). Only a stop's
//! own terminal activity is published after its commit (ADR 0122), and the
//! outcome it carries is the terminal's.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use lash_core::facade_support::LiveReplayGap;
use lash_core::facade_support::{TurnAddress, TurnOutcome, TurnTerminal, TurnWorkDriver};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::store::PhysicalTurn;
use lash_core::{
    InputId, LiveReplayGapReason, LiveReplayOutcome, LiveReplaySubscribeOutcome,
    LiveReplaySubscription, LlmCallRecord, LlmUsage, SessionCursor, SessionObservationEvent,
    SessionObservationEventPayload, SessionRevision, TurnActivity, TurnEvent, TurnId,
};
use tokio::sync::mpsc;

use super::resolve::{self, Resolution};
use super::{SendContext, SendOutcome};
use crate::error::{EmbedError, Result, SendError};
use crate::support::TurnActivitySink;
use crate::turn::{ReportSource, TurnOutput, TurnReport};

/// A terminal wait in flight.
type AwaitedTerminal =
    BoxFuture<'static, std::result::Result<TurnTerminal, lash_core::RuntimeError>>;

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
    buffered: VecDeque<(TurnId, TurnActivity)>,
    buffer_capacity: usize,
    collected: Vec<TurnActivity>,
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
        }
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
        for (turn, activity) in buffered {
            if self.adopts(&turn) {
                self.deliver(activity, tap).await;
            }
        }
    }

    async fn deliver(&mut self, activity: TurnActivity, tap: &mut Tap<'_>) {
        tap.activity(&activity).await;
        self.collected.push(activity);
    }

    /// One observation event. Answers whether it should wake a resolve.
    async fn observe(&mut self, event: &SessionObservationEvent, tap: &mut Tap<'_>) -> bool {
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
                    self.deliver(activity.clone(), tap).await;
                } else if self.run.is_none() {
                    if self.buffered.len() >= self.buffer_capacity {
                        self.buffered.pop_front();
                    }
                    self.buffered.push_back((turn.clone(), activity.clone()));
                }
                applied
            }
            SessionObservationEventPayload::Committed { .. }
            | SessionObservationEventPayload::QueueChanged { .. } => true,
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

/// One open wait on the adopted run's published terminal (FIG-3981): a
/// wake, never an answer.
///
/// A run that ran in another process deposits no report here and publishes
/// nothing on this process's replay, so without the wait its follower learns
/// that it settled only from a store poll that backs off to a second. The
/// wait is held while a store read has just shown the run undecided. A
/// published terminal wakes the
/// follower, which reads the answer from the store: the run's commit is
/// durable before its terminal is published. The wait is registered, so it
/// holds nothing past its run: the commit's publish answers it, and a run
/// that ended without that commit is retired, which releases it. Retirement
/// never cancels a terminal the run's ending commit still publishes
/// (FIG-4025). Every follower's wait on one physical turn shares one waiter
/// in the engine, however often followers attach and drop it (FIG-4345).
struct TerminalWait {
    /// The run's physical turn waited on: a run that goes on past a
    /// committed turn (a frame switch, a follow-on) goes on in its next
    /// physical turn.
    ordinal: u64,
    wait: Option<AwaitedTerminal>,
    /// The pause before the next wait, after one that failed.
    pause: Option<Duration>,
    /// A physical turn published a failure, or its wait was released: the
    /// store reads decide what follows (a park, a redrive, or the run's
    /// end), with no wait held.
    ended: bool,
}

impl TerminalWait {
    fn new() -> Self {
        Self {
            ordinal: 0,
            wait: None,
            pause: None,
            ended: false,
        }
    }

    /// Hold the wait on `run`'s current physical turn, unless one is held
    /// or no further terminal will be published.
    fn hold(&mut self, ctx: &SendContext, run: &TurnId) {
        if self.wait.is_some() || self.ended {
            return;
        }
        let driver = TurnWorkDriver::new(ctx.parts.effect_host.backend().clone())
            .with_terminal_pacing(ctx.parts.observer_pacing.terminal);
        let address = TurnAddress::new(
            ctx.parts.session_id.clone(),
            PhysicalTurn::derive_turn_id(run, self.ordinal),
        );
        let pause = self.pause.take();
        self.wait = Some(Box::pin(async move {
            if let Some(pause) = pause {
                tokio::time::sleep(pause).await;
            }
            driver.await_terminal(&address).await
        }));
    }

    /// The held wait's answer; pending while none is held.
    async fn next(&mut self) -> std::result::Result<TurnTerminal, lash_core::RuntimeError> {
        match self.wait.as_mut() {
            Some(wait) => wait.await,
            None => std::future::pending().await,
        }
    }

    /// Take the held wait's answer. The follower resolves from the store
    /// after each: a run that settled answers there, and a read that still
    /// shows the run undecided holds the next wait.
    fn answered(
        &mut self,
        ctx: &SendContext,
        answer: std::result::Result<TurnTerminal, lash_core::RuntimeError>,
    ) {
        self.wait = None;
        match answer {
            // The turn committed: the store answers a run it ended, and a
            // run it did not end goes on in its next physical turn.
            Ok(TurnTerminal::Committed { .. }) => self.ordinal = self.ordinal.saturating_add(1),
            // A failed turn publishes no further terminal, and neither does
            // a run whose retirement (or its session's revocation) released
            // the wait.
            Err(error)
                if error.code == lash_core::RuntimeErrorCode::TurnControlUnknownOrRevoked =>
            {
                self.ended = true;
            }
            Err(error) => {
                tracing::debug!(
                    session_id = %ctx.parts.session_id,
                    error = %error,
                    "send handle's terminal wait failed; attaching again after a pause"
                );
                // A wait that cannot attach never spins: the store poll
                // still answers meanwhile.
                self.pause = Some(ctx.parts.observer_pacing.follow.maximum());
            }
        }
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
    let mut adoption = Adoption::new(subject, ctx.parts.observer_pacing.follow_buffer.get());
    let observed_before = from.observed;
    let mut observation = Observation::subscribe(ctx, from, tap).await;
    // Where this follow's observation starts: past the gap, when the cursor
    // it was handed is gone, so a later window never meets that gap again.
    let start_cursor = observation.last_cursor.clone();
    let mut terminal = TerminalWait::new();
    let pacing = ctx.parts.observer_pacing.follow;
    let mut poll = pacing.initial();
    // The store poll is due `poll` after the last resolve, and the run probe
    // ticks at the floor, whatever wakes in between: a wake that resolves
    // nothing (a probe that found no run, another run's activity) delays
    // neither.
    let mut poll_at = tokio::time::Instant::now() + poll;
    let mut probe_ticks = tokio::time::interval_at(poll_at, pacing.initial());
    probe_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut resolve_now = true;
    let mut last_pass = false;
    loop {
        if resolve_now {
            let resolution = match subject {
                Subject::Input(receipt) => resolve::resolve_input(&ctx.parts, receipt).await?,
                Subject::Run(run) => resolve::resolve_run(&ctx.parts, run).await?,
            };
            match resolution {
                Resolution::OperationSettled { run, outcome } => {
                    adoption.adopt(run.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    ctx.refresh().await?;
                    return Ok(Followed::Answered(Box::new(
                        SendOutcome::OperationSettled {
                            run,
                            outcome,
                            gaps: observation.gaps,
                        },
                    )));
                }
                Resolution::Settled { run, outcome } => {
                    adoption.adopt(run.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    ctx.refresh().await?;
                    let observed = observed_before || !adoption.collected.is_empty();
                    return finish_settled(
                        ctx,
                        subject,
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
                        run: Some(run),
                        refusal: Box::new(refusal),
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Faulted(fault) => {
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Err(EmbedError::Runtime(fault));
                }
                resolution @ (Resolution::Withdrawn | Resolution::NotAccepted) => {
                    ctx.refresh().await?;
                    let gaps = observation.gaps;
                    return Ok(Followed::Answered(Box::new(match resolution {
                        Resolution::Withdrawn => SendOutcome::Withdrawn { gaps },
                        _ => SendOutcome::NotAccepted { gaps },
                    })));
                }
                Resolution::Undecided { run } => {
                    if let Some(run) = run {
                        adoption.adopt(run, tap).await;
                    }
                    // Wait on the terminal only while a store read has just
                    // shown the run undecided: a run this read shows ended
                    // may already be retired, which releases no wait
                    // registered after it.
                    if let Some(run) = adoption.run.as_ref() {
                        terminal.hold(ctx, run);
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
        // Wait for the first wake.
        let probed = run_probe(ctx, subject, &adoption);
        let probe = async {
            match probed {
                Some(input) => {
                    probe_ticks.tick().await;
                    ctx.parts.store.run_of_input(input).await
                }
                None => std::future::pending().await,
            }
        };
        let closes = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            event = next_event(&mut observation.replay) => {
                match event {
                    Some(Ok(event)) => {
                        observation.last_cursor = event.cursor.clone();
                        resolve_now = adoption.observe(&event, tap).await;
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
            answer = terminal.next() => {
                terminal.answered(ctx, answer);
            }
            bound = probe => {
                // A run the probe finds is read at once, which holds its
                // terminal wait while the run is undecided.
                resolve_now = matches!(bound, Ok(Some(_)));
                match bound {
                    Ok(Some(run)) => adoption.adopt(run, tap).await,
                    Ok(None) => {}
                    Err(error) => tracing::debug!(
                        session_id = %ctx.parts.session_id,
                        error = %error,
                        "send handle's run probe failed; the store poll goes on"
                    ),
                }
            }
            () = tokio::time::sleep_until(poll_at) => {
                poll = pacing.next(poll);
            }
            () = closes => {
                last_pass = true;
            }
        }
    }
}

/// The input whose run a follower probes for at the poll floor: an input
/// subject's, until its run is known, on a follower with no resident
/// runtime. Such a follower reads a run that may run in another process,
/// which publishes nothing on this process's replay, so without the probe it
/// learns the run, and holds the run's terminal wait, only on a store
/// poll that backs off to a second (FIG-3981). The probe is one keyed read
/// of the input's run binding: the admission that writes the binding runs
/// in the worker's process and announces it to no other, so there is no
/// event to wait on instead.
fn run_probe<'a>(
    ctx: &SendContext,
    subject: &'a Subject,
    adoption: &Adoption,
) -> Option<&'a InputId> {
    match subject {
        Subject::Input(receipt) if adoption.run.is_none() && ctx.live.is_none() => {
            Some(&receipt.input_id)
        }
        _ => None,
    }
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

#[allow(
    clippy::too_many_arguments,
    reason = "the settled answer's parts are each read at a different point of the follow"
)]
async fn finish_settled(
    ctx: &SendContext,
    subject: &Subject,
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
    let acceptance = match subject {
        Subject::Input(receipt) => Some(receipt.clone()),
        Subject::Run(_) => None,
    };
    // The terminal is durable. Preserve the sealed calls this follower
    // actually observed beside their activities; unavailable history stays a
    // reported gap. Retain duplicates and contradictions for validation.
    let llm_calls = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ModelCallRecorded { record } => Some(record.clone()),
            _ => None,
        })
        .collect();
    let result = durable_report(ctx, &run, outcome, acceptance, llm_calls).await?;
    Ok(SendOutcome::Settled {
        run,
        output: Box::new(TurnOutput { result, activities }),
        gaps: observation.gaps,
    })
}

/// The report of a run that ran elsewhere, rebuilt from the store: honest
/// and thin (D1 §1.5 3b).
pub(super) async fn durable_report(
    ctx: &SendContext,
    run: &TurnId,
    outcome: TurnOutcome,
    acceptance: Option<TurnInputAcceptanceReceipt>,
    llm_calls: Vec<LlmCallRecord>,
) -> Result<TurnReport> {
    let usage = llm_calls
        .iter()
        .flat_map(|call| &call.attempts)
        .filter_map(|attempt| attempt.usage.as_ref())
        .try_fold(LlmUsage::default(), |total, usage| {
            total.checked_add(&LlmUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_write_input_tokens: usage.cache_write_input_tokens,
                reasoning_output_tokens: usage.reasoning_output_tokens,
            })
        })
        .map_err(|overflow| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::QueuedWork,
                format!("turn report token usage overflows {}", overflow.counter()),
            ))
        })?;
    let state = lash_core::store::load_session_window_state(
        &ctx.parts.store,
        lash_core::store::WindowSelector::Terminal(run.clone()),
    )
    .await
    .map_err(EmbedError::Store)?
    .ok_or_else(|| {
        EmbedError::Store(lash_core::StoreError::StoredDataCorrupt {
            record_kind: "RunTerminalWindow",
            message: format!("settled run `{run}` has no recorded session window"),
        })
    })?
    .state
    .to_snapshot();
    let (tool_calls, omitted) =
        lash_core::runtime::durable::services::RuntimeTurnServices::recorded_tool_calls(
            &ctx.parts.effect_host,
            &ctx.parts.session_id,
            run,
        )
        .await
        .map_err(EmbedError::Durable)?;
    Ok(TurnReport {
        state,
        outcome,
        usage,
        llm_calls,
        failure_evidence: Vec::new(),
        tool_calls,
        omitted,
        execution: Default::default(),
        errors: Vec::new(),
        acceptance,
        cancel_input_outcome: Default::default(),
        source: ReportSource::Durable,
    })
}
