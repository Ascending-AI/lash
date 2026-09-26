//! Following one input or root to its answer (FIG-3600 S5b, D1 §1.3–§1.5).
//!
//! A follower subscribes to the session's live replay from its cursor, adopts
//! the activity of the root that applies its subject, and resolves the
//! subject from the store on every wake: the engine's drive barrier, a
//! commit or queue change on the observation, and a bounded poll. It never
//! answers from events: a follower whose replay window is gone still answers
//! from the store.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use lash_core::drive::root_of_physical_turn;
use lash_core::engine::{DriveAbort, DriveOutcome, DriveRequestId};
use lash_core::facade_support::LiveReplayGap;
use lash_core::facade_support::TurnOutcome;
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::{
    LiveReplayGapReason, LiveReplayOutcome, LiveReplaySubscribeOutcome, LiveReplaySubscription,
    SessionCursor, SessionObservationEvent, SessionObservationEventPayload, SessionRevision,
    SessionWorkEngine, TurnActivity, TurnEvent, TurnId,
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
/// How long a follower waits for a live report from this process once the
/// store shows the root settled but the engine's drive has not stopped.
const LIVE_REPORT_GRACE: Duration = Duration::from_secs(5);
/// How long an applied input's terminal may stay unreadable after its drive
/// stopped before the follower answers [`SendError::Unresolved`].
const UNRESOLVED_CEILING: Duration = Duration::from_secs(30);
/// The pause before re-asking an engine whose drive attempt failed.
const RETRY_PAUSE: Duration = Duration::from_millis(50);
/// Unadopted activities a follower buffers before dropping the oldest.
const BUFFER_CAPACITY: usize = 4096;

/// What a follower follows.
#[derive(Clone, Debug)]
pub(super) enum Subject {
    /// An accepted input: its root is the one whose turn applies it.
    Input(TurnInputAcceptanceReceipt),
    /// A logical root, by id.
    Root(TurnId),
}

impl Subject {
    /// The drive request a follower waits on: an accepted row's request is
    /// its input id (the id its acceptance scheduled), so waiting attaches to
    /// that drive, or starts it when its ask was lost.
    fn drive_request(&self) -> DriveRequestId {
        match self {
            Self::Input(receipt) => DriveRequestId::new(receipt.input_id.to_string()),
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

    async fn gap(&mut self, gap: LiveReplayGap) {
        if let Self::Channel(tx) = self {
            let _ = tx
                .send(Err(EmbedError::from(SendError::ObservationGap(gap))))
                .await;
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

/// Follow `subject` from `cursor` until it answers.
pub(super) async fn follow(
    ctx: &SendContext,
    subject: &Subject,
    cursor: &SessionCursor,
    mut tap: Tap<'_>,
) -> Result<SendOutcome> {
    let mut adoption = Adoption::new(subject);
    let mut last_cursor = cursor.clone();
    let mut replay = match ctx.parts.live_replay_store.subscribe_after_cursor(cursor) {
        Ok(LiveReplaySubscribeOutcome::Subscribed(subscription)) => Replay::Live(subscription),
        Ok(LiveReplaySubscribeOutcome::Gap(reason)) => {
            tap.gap(replay_gap(ctx, cursor, reason)).await;
            Replay::Ended
        }
        Err(error) => {
            tracing::warn!(
                session_id = %ctx.parts.session_id,
                error = %error,
                "send handle could not subscribe to live replay; it answers from the store"
            );
            Replay::Ended
        }
    };
    let request = subject.drive_request();
    let mut drive = Some(await_drive(ctx, &request, None));
    let mut drive_stopped: Option<tokio::time::Instant> = None;
    let mut refused: Option<lash_core::RuntimeError> = None;
    let mut settled_at: Option<tokio::time::Instant> = None;
    let mut poll = POLL_FLOOR;
    let mut resolve_now = true;
    loop {
        if resolve_now {
            // A root this process ran to its commit deposited its final turn
            // for every input it drove: the report as it ran, and the
            // evidence that the input settled.
            if let Subject::Input(receipt) = subject
                && let Some((root, turn)) =
                    mailbox::take_settled_root(&ctx.parts.session_id, &receipt.input_id)
            {
                adoption.adopt(root, &mut tap).await;
                drain(ctx, &mut adoption, &mut replay, &last_cursor, &mut tap).await;
                ctx.refresh().await?;
                let outcome = turn.outcome.clone();
                return finish_settled(ctx, subject, outcome, Some(turn), adoption.collected).await;
            }
            let resolution = match subject {
                Subject::Input(receipt) => resolve::resolve_input(&ctx.parts, receipt).await?,
                Subject::Root(root) => resolve::resolve_root(&ctx.parts, root).await?,
            };
            match resolution {
                Resolution::Settled { root, outcome } => {
                    adoption.adopt(root.clone(), &mut tap).await;
                    let live = live_report(ctx, subject, &root).await?;
                    let settled_since = *settled_at.get_or_insert_with(tokio::time::Instant::now);
                    let waiting_for_live = live.is_none()
                        && drive_stopped.is_none()
                        && settled_since.elapsed() < LIVE_REPORT_GRACE;
                    if !waiting_for_live {
                        drain(ctx, &mut adoption, &mut replay, &last_cursor, &mut tap).await;
                        ctx.refresh().await?;
                        return finish_settled(ctx, subject, outcome, live, adoption.collected)
                            .await;
                    }
                }
                Resolution::Parked(parked) => {
                    adoption.adopt(parked.root.clone(), &mut tap).await;
                    drain(ctx, &mut adoption, &mut replay, &last_cursor, &mut tap).await;
                    ctx.refresh().await?;
                    return Ok(SendOutcome {
                        status: TurnStatus::Parked(parked),
                        output: None,
                    });
                }
                Resolution::Withdrawn => {
                    // A drive refused after it claimed the input leaves no
                    // application behind either: the refusal is the answer.
                    if let Some(error) = refused.take() {
                        return Err(EmbedError::Runtime(error));
                    }
                    ctx.refresh().await?;
                    return Ok(SendOutcome {
                        status: TurnStatus::Cancelled,
                        output: None,
                    });
                }
                Resolution::Undecided { root } => {
                    if let Some(root) = root {
                        adoption.adopt(root, &mut tap).await;
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
                }
            }
        }
        resolve_now = true;
        // Wait for the first wake.
        let sleep = tokio::time::sleep(poll);
        tokio::pin!(sleep);
        tokio::select! {
            event = next_event(&mut replay) => {
                match event {
                    Some(Ok(event)) => {
                        last_cursor = event.cursor.clone();
                        resolve_now = adoption.observe(&event, &mut tap).await;
                        if resolve_now {
                            poll = POLL_FLOOR;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!(
                            session_id = %ctx.parts.session_id,
                            error = %error,
                            "send handle's live replay ended"
                        );
                        tap.gap(replay_gap(ctx, &last_cursor, LiveReplayGapReason::Unavailable)).await;
                        replay = Replay::Ended;
                    }
                    None => replay = Replay::Ended,
                }
            }
            answer = async {
                match drive.as_mut() {
                    Some(drive) => drive.await,
                    None => std::future::pending().await,
                }
            } => {
                drive = None;
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
            () = &mut sleep => {
                poll = (poll * 2).min(POLL_CEILING);
            }
        }
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

/// Deliver what the live replay already holds past `last_cursor`: the
/// subject's root settled, so its activity is published up to here.
async fn drain(
    ctx: &SendContext,
    adoption: &mut Adoption,
    replay: &mut Replay,
    last_cursor: &SessionCursor,
    tap: &mut Tap<'_>,
) {
    if matches!(replay, Replay::Ended) {
        return;
    }
    *replay = Replay::Ended;
    if let Ok(LiveReplayOutcome::Replayed(events)) =
        ctx.parts.live_replay_store.replay_after_cursor(last_cursor)
    {
        for event in events {
            let _ = adoption.observe(&event, tap).await;
        }
    }
}

/// The live report this process holds for the subject, taken once.
async fn live_report(
    ctx: &SendContext,
    subject: &Subject,
    root: &TurnId,
) -> Result<Option<std::sync::Arc<lash_core::facade_support::AssembledTurn>>> {
    let inputs = match subject {
        Subject::Input(receipt) => vec![receipt.input_id.clone()],
        Subject::Root(_) => resolve::inputs_of_root(&ctx.parts, root).await?,
    };
    Ok(inputs.into_iter().find_map(|input| {
        mailbox::take_settled_root(&ctx.parts.session_id, &input).map(|(_, turn)| turn)
    }))
}

async fn finish_settled(
    ctx: &SendContext,
    subject: &Subject,
    outcome: TurnOutcome,
    live: Option<std::sync::Arc<lash_core::facade_support::AssembledTurn>>,
    activities: Vec<TurnActivity>,
) -> Result<SendOutcome> {
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
        output: Some(TurnOutput { result, activities }),
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
