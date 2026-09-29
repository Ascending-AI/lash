//! Following one input or root to its answer (FIG-3600 S5b, D1 §1.3–§1.5).
//!
//! A follower subscribes to the session's live replay from its cursor, adopts
//! the activity of the root that applies its subject, and resolves the
//! subject from the store on every wake: the engine's drive barrier, a
//! commit or queue change on the observation, a settled root's report landing
//! in this process's mailbox, and a bounded poll. Once it knows its root, it
//! also holds one open wait on the root's published terminal, which answers
//! a root that ran in another process as soon as it settles; a follower with
//! no resident runtime probes for that root at the poll floor (FIG-3981). It
//! never answers from events: a follower whose replay window is gone still
//! answers from the store.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use lash_core::drive::{physical_turn_of, root_of_physical_turn};
use lash_core::engine::{DriveAbort, DriveOutcome, DriveRequestId};
use lash_core::facade_support::LiveReplayGap;
use lash_core::facade_support::{TurnAddress, TurnOutcome, TurnTerminal, TurnWorkDriver};
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::{
    InputId, LiveReplayGapReason, LiveReplayOutcome, LiveReplaySubscribeOutcome,
    LiveReplaySubscription, SessionCursor, SessionObservationEvent, SessionObservationEventPayload,
    SessionRevision, SessionWorkEngine, TurnActivity, TurnEvent, TurnId,
};
use tokio::sync::mpsc;

use super::resolve::{self, Resolution};
use super::{SendContext, SendOutcome, TurnStatus, mailbox, status_of_outcome};
use crate::error::{EmbedError, Result, SendError};
use crate::support::TurnActivitySink;
use crate::turn::{ReportSource, TurnOutput, TurnReport};

/// The first wait between store reads when nothing else wakes a follower.
const POLL_FLOOR: Duration = Duration::from_millis(25);
/// The longest wait between store reads.
const POLL_CEILING: Duration = Duration::from_secs(1);
/// The longest a follower waits for a live report once the store shows the
/// root settled, while a run in this process may still deposit it and the
/// engine's drive has not stopped. A root no run here can report answers
/// from the store at once.
const LIVE_REPORT_GRACE: Duration = Duration::from_secs(5);
/// How long an applied input's terminal may stay unreadable after its drive
/// stopped before the follower answers [`SendError::Unresolved`].
const UNRESOLVED_CEILING: Duration = Duration::from_secs(30);
/// The pause before re-asking an engine whose drive attempt failed.
const RETRY_PAUSE: Duration = Duration::from_millis(50);
/// Unadopted activities a follower buffers before dropping the oldest.
const BUFFER_CAPACITY: usize = 4096;

/// A terminal wait in flight.
type AwaitedTerminal =
    BoxFuture<'static, std::result::Result<TurnTerminal, lash_core::RuntimeError>>;

/// What a follower follows.
#[derive(Clone, Debug)]
pub(super) enum Subject {
    /// An accepted input: its root is the one whose turn applies it.
    Input(TurnInputAcceptanceReceipt),
    /// A logical root, by id.
    Root(TurnId),
}

impl Subject {
    /// The drive request a follower first waits on: an accepted row's
    /// request is the one its ingress obligation delivers (ADR 0109 §3), so
    /// waiting attaches to that drive, or starts it when no delivery reached
    /// the engine yet. A follower moves on to the relay's later ask when the
    /// engine lost this one ([`moved_ask`]).
    fn drive_request(&self) -> DriveRequestId {
        match self {
            Self::Input(receipt) => lash_core::drive::ingress_drive_request(
                receipt.input_id.as_str(),
                lash_core::drive::FIRST_INGRESS_ATTEMPT,
            ),
            Self::Root(root) => DriveRequestId::new(format!("root:{root}")),
        }
    }
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
    root: Option<TurnId>,
    buffered: VecDeque<(TurnId, TurnActivity)>,
    collected: Vec<TurnActivity>,
}

impl Adoption {
    fn new(subject: &Subject) -> Self {
        Self {
            root: match subject {
                Subject::Root(root) => Some(root.clone()),
                Subject::Input(_) => None,
            },
            subject: subject.clone(),
            buffered: VecDeque::new(),
            collected: Vec::new(),
        }
    }

    fn adopts(&self, turn: &TurnId) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root_of_physical_turn(turn).0 == *root)
    }

    /// Adopt `root`, releasing the buffered activity of its turns.
    async fn adopt(&mut self, root: TurnId, tap: &mut Tap<'_>) {
        if self.root.is_some() {
            return;
        }
        self.root = Some(root);
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
                if self.root.is_none()
                    && let Subject::Input(receipt) = &self.subject
                    && let TurnEvent::QueuedInputAccepted { applications } = &activity.event
                    && applications
                        .iter()
                        .any(|application| application.input_id == receipt.input_id)
                {
                    self.adopt(root_of_physical_turn(turn).0, tap).await;
                }
                if self.adopts(turn) {
                    self.deliver(activity.clone(), tap).await;
                } else if self.root.is_none() {
                    if self.buffered.len() >= BUFFER_CAPACITY {
                        self.buffered.pop_front();
                    }
                    self.buffered.push_back((turn.clone(), activity.clone()));
                }
                false
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

/// Wait on the engine for `request`'s drive, as a `'static` future.
fn await_drive(
    ctx: &SendContext,
    request: &DriveRequestId,
    pause: Option<Duration>,
) -> BoxFuture<'static, std::result::Result<DriveOutcome, DriveAbort>> {
    let work = ctx.parts.work.clone();
    let session = ctx.parts.session_id.clone();
    let request = request.clone();
    Box::pin(async move {
        if let Some(pause) = pause {
            tokio::time::sleep(pause).await;
        }
        work.await_drive(&session, &request).await
    })
}

/// The relay's last ask for an input subject's drive, when it moved past
/// `request`: the engine lost the ask the follower waited on (an operator
/// kill) before it admitted the input, so the relay asked again under the
/// next attempt, `ingress:{input}:{attempt}` — whether or not that drive has
/// admitted the input yet. The follower attaches to that drive rather than
/// wait on the lost one. A root subject has no ask.
async fn moved_ask(
    ctx: &SendContext,
    subject: &Subject,
    request: &DriveRequestId,
) -> Option<DriveRequestId> {
    let Subject::Input(receipt) = subject else {
        return None;
    };
    match ctx
        .parts
        .ops
        .current_ingress_ask(receipt.input_id.as_str())
        .await
    {
        Ok(Some(ask)) if ask != *request => Some(ask),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(
                session_id = %ctx.parts.session_id,
                input_id = %receipt.input_id.as_str(),
                error = %error,
                "send handle could not read its input's current drive ask; waiting on the one it holds"
            );
            None
        }
    }
}

/// One open wait on the adopted root's published terminal (FIG-3981).
///
/// A root that ran in another process deposits no report here and publishes
/// nothing on this process's replay, so without the wait its follower learns
/// that it settled only from a store poll that backs off to a second. The
/// wait is held while a store read has just shown the root undecided and no
/// run in this process may still deposit the root's report: that deposit
/// answers first, with the full report. The wait is registered, so it holds nothing past its root: the
/// commit's publish answers it, and a root that ended without that commit
/// is retired, which releases it. Retirement never cancels a terminal the
/// root's ending commit still publishes (FIG-4025).
struct TerminalWait {
    /// The root's physical turn waited on: a frame switch continues the root
    /// in its next physical turn.
    ordinal: u64,
    wait: Option<AwaitedTerminal>,
    /// The pause before the next wait, after one that failed.
    pause: Option<Duration>,
    /// The outcome the root's final physical turn committed with.
    settled: Option<TurnOutcome>,
    /// A physical turn published a failure, or its wait was released: the
    /// store reads decide what follows (a park, a redrive, or the root's
    /// end), with no wait held.
    ended: bool,
}

impl TerminalWait {
    fn new() -> Self {
        Self {
            ordinal: 0,
            wait: None,
            pause: None,
            settled: None,
            ended: false,
        }
    }

    /// Hold the wait on `root`'s current physical turn, unless one is held,
    /// the root's terminal is already known, or a run in this process may
    /// still deposit the root's report.
    fn hold(&mut self, ctx: &SendContext, root: &TurnId) {
        if self.wait.is_some()
            || self.settled.is_some()
            || self.ended
            || mailbox::may_deposit(ctx.parts.work.store_binding(), &ctx.parts.session_id, root)
        {
            return;
        }
        let driver = TurnWorkDriver::for_session(
            std::sync::Arc::clone(&ctx.parts.effect_host),
            ctx.parts.session_id.to_string(),
            std::sync::Arc::clone(&ctx.parts.store),
        );
        let address = TurnAddress::new(
            ctx.parts.session_id.clone(),
            physical_turn_of(root, self.ordinal),
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

    /// Take the held wait's answer. The follower resolves after each: the
    /// read holds the next wait while the root is undecided.
    fn answered(
        &mut self,
        ctx: &SendContext,
        answer: std::result::Result<TurnTerminal, lash_core::RuntimeError>,
    ) {
        self.wait = None;
        match answer {
            Ok(TurnTerminal::Committed {
                outcome: TurnOutcome::AgentFrameSwitch { .. },
                ..
            }) => self.ordinal = self.ordinal.saturating_add(1),
            Ok(TurnTerminal::Committed { outcome, .. }) => self.settled = Some(outcome),
            // A failed turn publishes no further terminal, and neither does
            // a root whose retirement (or its session's revocation) released
            // the wait.
            Ok(TurnTerminal::Failed { .. }) => self.ended = true,
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
                self.pause = Some(POLL_CEILING);
            }
        }
    }
}

/// Where a follower stands in its subject's live activity: the replay
/// cursor to go on from, whether it has observed any of the root's activity,
/// and the gaps it has met so far. A windowed follower hands its position to
/// the next window; the host's Restate wait journals it between probes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Position {
    pub(crate) cursor: SessionCursor,
    pub(crate) observed: bool,
    pub(crate) gaps: Vec<LiveReplayGap>,
}

impl Position {
    pub(crate) fn at(cursor: SessionCursor) -> Self {
        Self {
            cursor,
            observed: false,
            gaps: Vec::new(),
        }
    }
}

/// What one follow answered.
pub(super) enum Followed {
    /// The subject stopped moving.
    Answered(Box<SendOutcome>),
    /// The window closed first: go on from here.
    Pending(Position),
}

/// A follower's live replay: its subscription, the cursor it last read, and
/// the gaps it met. A gap is reported, then the follower resubscribes at the
/// replay's current head, so a gap is bounded to what the replay lost.
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
            gaps: from.gaps,
        };
        observation.resubscribe(ctx, tap).await;
        observation
    }

    /// Subscribe after `last_cursor`, or past a gap at the replay's head: a
    /// trimmed window, or a cursor this process's replay cannot place (one
    /// taken in another process, before a restart).
    async fn resubscribe(&mut self, ctx: &SendContext, tap: &mut Tap<'_>) {
        let store = &ctx.parts.live_replay_store;
        let reason = match store.subscribe_after_cursor(&self.last_cursor) {
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
        self.last_cursor = gap.latest_cursor.clone();
        self.report(gap, tap).await;
        self.replay = match store.subscribe_after_cursor(&self.last_cursor) {
            Ok(LiveReplaySubscribeOutcome::Subscribed(subscription)) => Replay::Live(subscription),
            _ => Replay::Ended,
        };
    }

    async fn report(&mut self, gap: LiveReplayGap, tap: &mut Tap<'_>) {
        tap.gap(&gap).await;
        self.gaps.push(gap);
    }

    /// The subscription ended under the follower: report the loss, then
    /// resubscribe at the replay's head.
    async fn lost(&mut self, ctx: &SendContext, tap: &mut Tap<'_>) {
        let gap = replay_gap(ctx, &self.last_cursor, LiveReplayGapReason::Unavailable);
        self.last_cursor = gap.latest_cursor.clone();
        self.report(gap, tap).await;
        self.replay = Replay::Ended;
        self.resubscribe(ctx, tap).await;
    }
}

/// Follow `subject` from `from` until it answers, or until `window` closes.
///
/// A follower answers from the store, never from events. Activity is only
/// what this process's live replay published: a root that ran in another
/// process is observed through its durable record alone, and a settled root
/// whose activity this follower never saw answers with a reported
/// [`LiveReplayGapReason::Unavailable`] gap, so a collected activity list is
/// never mistaken for the root's whole history.
pub(super) async fn follow(
    ctx: &SendContext,
    subject: &Subject,
    from: Position,
    tap: &mut Tap<'_>,
    window: Option<Duration>,
) -> Result<Followed> {
    let deadline = window.map(|window| tokio::time::Instant::now() + window);
    let start_cursor = from.cursor.clone();
    let mut adoption = Adoption::new(subject);
    let observed_before = from.observed;
    let mut observation = Observation::subscribe(ctx, from, tap).await;
    let mut request = subject.drive_request();
    let mut drive = Some(await_drive(ctx, &request, None));
    let mut drive_stopped: Option<tokio::time::Instant> = None;
    let mut refused: Option<lash_core::RuntimeError> = None;
    let mut settled_at: Option<tokio::time::Instant> = None;
    let mut terminal = TerminalWait::new();
    let mut poll = POLL_FLOOR;
    // The store poll is due `poll` after the last resolve, and the root probe
    // ticks at the floor, whatever wakes in between: a wake that resolves
    // nothing (a probe that found no root, another root's activity) delays
    // neither.
    let mut poll_at = tokio::time::Instant::now() + poll;
    let mut probe_ticks = tokio::time::interval_at(poll_at, POLL_FLOOR);
    probe_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut resolve_now = true;
    let mut last_pass = false;
    loop {
        // Armed before this pass looks in the mailbox: a report deposited
        // after the look still wakes the wait below.
        let deposited = mailbox::deposited();
        tokio::pin!(deposited);
        deposited.as_mut().enable();
        if resolve_now {
            // A root this process ran to its commit deposited its final turn
            // for every input it drove: the report as it ran, and the
            // evidence that the input settled.
            if let Subject::Input(receipt) = subject
                && let Some(settled) = mailbox::take_settled_root(
                    ctx.parts.work.store_binding(),
                    &ctx.parts.session_id,
                    &receipt.input_id,
                )
            {
                let root = settled.root.clone();
                adoption.adopt(root.clone(), tap).await;
                drain(ctx, &mut adoption, &mut observation, tap).await;
                ctx.refresh_unless_ran_on(Some(&settled)).await?;
                let outcome = settled.turn.outcome.clone();
                let turn = settled.turn;
                let observed = observed_before || !adoption.collected.is_empty();
                return finish_settled(
                    ctx,
                    subject,
                    root,
                    outcome,
                    Some(turn),
                    adoption.collected,
                    observation,
                    observed,
                    tap,
                )
                .await
                .map(|outcome| Followed::Answered(Box::new(outcome)));
            }
            let resolution = match (&adoption.root, &terminal.settled, subject) {
                // The open wait saw the root's final terminal published: the
                // same terminal the store read would wait for.
                (Some(root), Some(outcome), _) => Resolution::Settled {
                    root: root.clone(),
                    outcome: outcome.clone(),
                },
                // The input's report landing answers at once, whatever the
                // store read is waiting on: a settled root's terminal read
                // waits for its publication (FIG-3979).
                (_, _, Subject::Input(receipt)) => tokio::select! {
                    resolution = resolve::resolve_input(&ctx.parts, receipt) => resolution?,
                    () = mailbox::settled_root_held(
                        ctx.parts.work.store_binding(),
                        &ctx.parts.session_id,
                        &receipt.input_id,
                    ) => continue,
                },
                (_, _, Subject::Root(root)) => resolve::resolve_root(&ctx.parts, root).await?,
            };
            match resolution {
                Resolution::Settled { root, outcome } => {
                    adoption.adopt(root.clone(), tap).await;
                    let live = live_report(ctx, subject, &root).await?;
                    let settled_since = *settled_at.get_or_insert_with(tokio::time::Instant::now);
                    let waiting_for_live = live.is_none()
                        && !last_pass
                        && drive_stopped.is_none()
                        && mailbox::may_deposit(
                            ctx.parts.work.store_binding(),
                            &ctx.parts.session_id,
                            &root,
                        )
                        && settled_since.elapsed() < LIVE_REPORT_GRACE;
                    if !waiting_for_live {
                        drain(ctx, &mut adoption, &mut observation, tap).await;
                        ctx.refresh_unless_ran_on(live.as_ref()).await?;
                        let live = live.map(|settled| settled.turn);
                        let observed = observed_before || !adoption.collected.is_empty();
                        return finish_settled(
                            ctx,
                            subject,
                            root,
                            outcome,
                            live,
                            adoption.collected,
                            observation,
                            observed,
                            tap,
                        )
                        .await
                        .map(|outcome| Followed::Answered(Box::new(outcome)));
                    }
                }
                Resolution::Parked(parked) => {
                    adoption.adopt(parked.root.clone(), tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    ctx.refresh().await?;
                    return Ok(Followed::Answered(Box::new(SendOutcome {
                        root: Some(parked.root.clone()),
                        status: TurnStatus::Parked(parked),
                        output: None,
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Refused { root, refusal } => {
                    adoption.adopt(root, tap).await;
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Err(EmbedError::Runtime(refusal));
                }
                Resolution::Stalled(stalled) => {
                    drain(ctx, &mut adoption, &mut observation, tap).await;
                    return Ok(Followed::Answered(Box::new(SendOutcome {
                        root: None,
                        status: TurnStatus::Stalled(stalled),
                        output: None,
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Withdrawn => {
                    // A drive refused after it claimed the input leaves no
                    // application behind either: the refusal is the answer.
                    if let Some(error) = refused.take() {
                        return Err(EmbedError::Runtime(error));
                    }
                    ctx.refresh().await?;
                    return Ok(Followed::Answered(Box::new(SendOutcome {
                        status: TurnStatus::Cancelled,
                        root: None,
                        output: None,
                        gaps: observation.gaps,
                    })));
                }
                Resolution::Undecided { root } => {
                    // The input's drive may have moved on from the one this
                    // follower waits on: follow it, whatever the lost drive's
                    // own attach is still doing.
                    if let Some(ask) = moved_ask(ctx, subject, &request).await {
                        tracing::debug!(
                            session_id = %ctx.parts.session_id,
                            from = request.as_str(),
                            to = ask.as_str(),
                            "send handle follows its input's drive to the relay's next ask"
                        );
                        request = ask;
                        drive = Some(await_drive(ctx, &request, None));
                        drive_stopped = None;
                    }
                    if let Some(root) = root {
                        adoption.adopt(root, tap).await;
                        if let Some(stopped) = drive_stopped
                            && stopped.elapsed() >= UNRESOLVED_CEILING
                            && let Subject::Input(receipt) = subject
                        {
                            return Err(EmbedError::from(SendError::Unresolved {
                                input_id: receipt.input_id.clone(),
                            }));
                        }
                    }
                    if let Some(error) = refused.take() {
                        return Err(EmbedError::Runtime(error));
                    }
                    // Wait on the terminal only while a store read has just
                    // shown the root undecided: a root this read shows ended
                    // may already be retired, which releases no wait
                    // registered after it.
                    if let Some(root) = adoption.root.as_ref() {
                        terminal.hold(ctx, root);
                    }
                }
            }
            if last_pass {
                // The window closed on an undecided subject. A follower that
                // adopted no root yet goes on from where it started, so the
                // activity it buffered is read again.
                let cursor = if adoption.root.is_some() {
                    observation.last_cursor
                } else {
                    start_cursor
                };
                return Ok(Followed::Pending(Position {
                    cursor,
                    observed: observed_before || !adoption.collected.is_empty(),
                    gaps: observation.gaps,
                }));
            }
            poll_at = tokio::time::Instant::now() + poll;
        }
        resolve_now = true;
        // Wait for the first wake.
        let probed = root_probe(ctx, subject, &adoption);
        let probe = async {
            match probed {
                Some(input) => {
                    probe_ticks.tick().await;
                    ctx.parts
                        .store
                        .root_of_input(&ctx.parts.session_id, input)
                        .await
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
                            poll = POLL_FLOOR;
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
            answer = async {
                match drive.as_mut() {
                    Some(drive) => drive.await,
                    None => std::future::pending().await,
                }
            } => {
                drive = None;
                if let Some(ask) = moved_ask(ctx, subject, &request).await {
                    tracing::debug!(
                        session_id = %ctx.parts.session_id,
                        from = request.as_str(),
                        to = ask.as_str(),
                        "send handle follows its input's drive to the relay's next ask"
                    );
                    request = ask;
                    drive = Some(await_drive(ctx, &request, None));
                    drive_stopped = None;
                    continue;
                }
                match answer {
                    Ok(_) | Err(DriveAbort::Parked { .. }) => {
                        drive_stopped.get_or_insert_with(tokio::time::Instant::now);
                    }
                    // The engine retries under its own policy; ask again under
                    // the same id, which attaches rather than drives twice.
                    Err(DriveAbort::Retry(error)) => {
                        tracing::debug!(
                            session_id = %ctx.parts.session_id,
                            request = request.as_str(),
                            error = %error,
                            "send handle's drive attempt failed; waiting on the retry"
                        );
                        drive = Some(await_drive(ctx, &request, Some(RETRY_PAUSE)));
                    }
                    Err(DriveAbort::Refused(error)) => {
                        drive_stopped.get_or_insert_with(tokio::time::Instant::now);
                        refused = Some(error);
                    }
                }
            }
            answer = terminal.next() => {
                terminal.answered(ctx, answer);
            }
            bound = probe => {
                // A root the probe finds is read at once, which holds its
                // terminal wait while the root is undecided.
                resolve_now = matches!(bound, Ok(Some(_)));
                match bound {
                    Ok(Some(root)) => adoption.adopt(root, tap).await,
                    Ok(None) => {}
                    Err(error) => tracing::debug!(
                        session_id = %ctx.parts.session_id,
                        error = %error,
                        "send handle's root probe failed; the store poll goes on"
                    ),
                }
            }
            () = &mut deposited => {
                // A run in this process deposited its report or ended:
                // resolve again when this follower waits on a settled root's
                // report, or when the deposit may be its input's.
                resolve_now = settled_at.is_some()
                    || match subject {
                        Subject::Input(receipt) => mailbox::holds_settled_root(
                            ctx.parts.work.store_binding(),
                            &ctx.parts.session_id,
                            &receipt.input_id,
                        ),
                        Subject::Root(_) => false,
                    };
            }
            () = tokio::time::sleep_until(poll_at) => {
                poll = (poll * 2).min(POLL_CEILING);
            }
            () = closes => {
                last_pass = true;
            }
        }
    }
}

/// The input whose root a follower probes for at the poll floor: an input
/// subject's, until its root is known, on a follower with no resident
/// runtime. Such a follower reads a root that may run in another process,
/// which publishes nothing on this process's replay, so without the probe it
/// learns the root, and holds the root's terminal wait, only on a store
/// poll that backs off to a second (FIG-3981). The probe is one keyed read
/// of the input's root binding: the admission that writes the binding runs
/// in the worker's process and announces it to no other, so there is no
/// event to wait on instead.
fn root_probe<'a>(
    ctx: &SendContext,
    subject: &'a Subject,
    adoption: &Adoption,
) -> Option<&'a InputId> {
    match subject {
        Subject::Input(receipt) if adoption.root.is_none() && ctx.live.is_none() => {
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
/// subject's root stopped, so its activity is published up to here.
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
    {
        for event in events {
            observation.last_cursor = event.cursor.clone();
            let _ = adoption.observe(&event, tap).await;
        }
    }
}

/// The live report this process holds for the subject, taken once.
async fn live_report(
    ctx: &SendContext,
    subject: &Subject,
    root: &TurnId,
) -> Result<Option<mailbox::SettledRoot>> {
    Ok(match subject {
        Subject::Input(receipt) => mailbox::take_settled_root(
            ctx.parts.work.store_binding(),
            &ctx.parts.session_id,
            &receipt.input_id,
        ),
        Subject::Root(_) => mailbox::take_settled_root_of(
            ctx.parts.work.store_binding(),
            &ctx.parts.session_id,
            root,
        ),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the settled answer's parts are each read at a different point of the follow"
)]
async fn finish_settled(
    ctx: &SendContext,
    subject: &Subject,
    root: TurnId,
    outcome: TurnOutcome,
    live: Option<std::sync::Arc<lash_core::facade_support::AssembledTurn>>,
    activities: Vec<TurnActivity>,
    mut observation: Observation,
    observed: bool,
    tap: &mut Tap<'_>,
) -> Result<SendOutcome> {
    if !observed {
        // The root settled, yet none of its activity reached this follower:
        // it ran in another process, or before this follower's cursor.
        let gap = replay_gap(
            ctx,
            &observation.last_cursor,
            LiveReplayGapReason::Unavailable,
        );
        observation.report(gap, tap).await;
    }
    let status = status_of_outcome(&outcome);
    let acceptance = match subject {
        Subject::Input(receipt) => Some(receipt.clone()),
        Subject::Root(_) => None,
    };
    let result = match live {
        Some(turn) => {
            let mut report = TurnReport::from_assembled(turn.as_ref().clone());
            if acceptance.is_some() {
                report.acceptance = acceptance;
            }
            report
        }
        None => durable_report(ctx, outcome, acceptance).await?,
    };
    Ok(SendOutcome {
        status,
        root: Some(root),
        output: Some(TurnOutput { result, activities }),
        gaps: observation.gaps,
    })
}

/// The report of a root that ran elsewhere, rebuilt from the store: honest
/// and thin (D1 §1.5 3b).
pub(super) async fn durable_report(
    ctx: &SendContext,
    outcome: TurnOutcome,
    acceptance: Option<TurnInputAcceptanceReceipt>,
) -> Result<TurnReport> {
    let state = ctx.session_snapshot().await?;
    let assistant_output = match &outcome {
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { text }) => {
            lash_core::facade_support::AssistantOutput {
                safe_text: text.clone(),
                raw_text: text.clone(),
                state: lash_core::facade_support::OutputState::Usable,
            }
        }
        _ => lash_core::facade_support::AssistantOutput {
            safe_text: String::new(),
            raw_text: String::new(),
            state: lash_core::facade_support::OutputState::EmptyOutput,
        },
    };
    Ok(TurnReport {
        state,
        outcome,
        assistant_output,
        usage: Default::default(),
        llm_calls: Vec::new(),
        failure_evidence: Vec::new(),
        tool_calls: Vec::new(),
        omitted: None,
        execution: Default::default(),
        errors: Vec::new(),
        acceptance,
        cancel_input_outcome: Default::default(),
        source: ReportSource::Durable,
    })
}
