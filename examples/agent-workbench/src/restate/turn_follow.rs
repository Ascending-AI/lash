//! How the workbench starts a user turn and follows it to its settlement.
//!
//! A turn is never run here. The session's `send()` takes the input durably
//! under the turn id, and the session's engine executes the run: lash's
//! `LashSession` service, in a Restate handler. The workbench follows the
//! run through its handle and turns its settled report into the product rows
//! the page shows, then releases the session's active-turn claim.
//!
//! The engine also starts runs nobody sent from here: a next-turn input, a
//! process wake, a cron occurrence. A watch on each session this process
//! serves follows those too, so every run's answer reaches the page.

use super::*;
use futures_util::StreamExt as _;
use lash::rlm::RlmSendBuilderExt as _;
use std::collections::HashSet;

/// How a followed turn ended on the page: settled, or failed with the reason
/// its terminalization recorded.
pub(crate) type TurnSettlement = Result<(), String>;

/// A turn the browser asked for.
#[derive(Clone, Debug)]
pub(crate) struct UserTurnRequest {
    pub(crate) turn_id: TurnId,
    pub(crate) session_id: SessionId,
    pub(crate) text: String,
    pub(crate) model: LlmProfileSelection,
    pub(crate) attachment_id: Option<String>,
}

/// How long a follower waits before it reads a run again after a read that
/// failed retryably.
const REFOLLOW_BACKOFF: Duration = Duration::from_millis(250);
/// The longest a resumed follower waits between opens of a session a dead
/// owner's lease still holds.
const REFOLLOW_BACKOFF_CEILING: Duration = Duration::from_secs(2);

/// The runs this process follows to settlement, and the sessions it watches
/// for runs the engine starts on its own. A run is followed once: whoever
/// claims it first follows it, and its follower lets it go.
#[derive(Clone, Default)]
pub(crate) struct RunFollows {
    inner: Arc<Mutex<RunFollowsInner>>,
}

#[derive(Default)]
struct RunFollowsInner {
    runs: HashSet<(SessionId, TurnId)>,
    watched: HashSet<SessionId>,
}

impl RunFollows {
    /// Claim `run` for a follower; false when one already follows it.
    fn claim(&self, session_id: &SessionId, run: &TurnId) -> bool {
        self.inner
            .lock_recover()
            .runs
            .insert((session_id.clone(), run.clone()))
    }

    fn release(&self, session_id: &SessionId, run: &TurnId) {
        self.inner
            .lock_recover()
            .runs
            .remove(&(session_id.clone(), run.clone()));
    }

    /// Start watching `session_id`; false when a watch already runs.
    fn watch(&self, session_id: &SessionId) -> bool {
        self.inner.lock_recover().watched.insert(session_id.clone())
    }

    /// Whether a follower here still holds a run of `session_id`.
    pub(crate) fn follows_any(&self, session_id: &SessionId) -> bool {
        self.inner
            .lock_recover()
            .runs
            .iter()
            .any(|(followed, _)| followed == session_id)
    }

    fn unwatch(&self, session_id: &SessionId) {
        self.inner.lock_recover().watched.remove(session_id);
    }
}

/// Lets a claimed run go when its follower ends, however it ends.
struct RunClaim {
    follows: RunFollows,
    session_id: SessionId,
    run: TurnId,
}

impl Drop for RunClaim {
    fn drop(&mut self) {
        self.follows.release(&self.session_id, &self.run);
    }
}

/// Accept `request`'s input under its turn id, then follow the run in the
/// background. The returned task ends once the run is settled on the page.
pub(crate) async fn start_user_turn(
    state: &AppState,
    request: UserTurnRequest,
) -> Result<tokio::task::JoinHandle<TurnSettlement>, AppError> {
    let input = workbench_turn_input(state, &request).await?;
    let turn_profile = request.model.clone();
    let session = state
        .create_or_open_session(&request.session_id, "api.turn")
        .await
        .map_err(AppError::session_open)?;
    apply_llm_profile_selection_to_session(state, &session, turn_profile, "user_turn").await?;
    watch_session_runs(state, &request.session_id).await;
    // Claimed before the send, so the session's watch leaves this run to
    // the follower below.
    let follows = &state.active_turns.follows;
    follows.claim(&request.session_id, &request.turn_id);
    let claim = RunClaim {
        follows: follows.clone(),
        session_id: request.session_id.clone(),
        run: request.turn_id.clone(),
    };
    let send = session.send(input).id(request.turn_id.clone());
    let send = match crate::session_protocol::selected().map_err(AppError::internal)? {
        crate::session_protocol::SessionProtocol::Standard => send,
        crate::session_protocol::SessionProtocol::Rlm => {
            send.require_finish().map_err(AppError::internal)?
        }
    };
    let handle = send.await.map_err(AppError::runtime)?;
    state.trace_for_session(
        &request.session_id,
        "turn.accepted",
        json!({
            "turn_id": request.turn_id,
            "input_id": handle.input_id(),
        }),
    );
    Ok(spawn_turn_follower(
        state.clone(),
        session,
        claim,
        FollowOrigin::UserTurn,
        FollowFrom::Send(Box::new(handle)),
    ))
}

/// Follow every claimed user turn this process found in its active-turn
/// ledger at startup: a restarted host re-attaches to the runs its previous
/// incarnation was following, and settles each once its engine does.
pub(crate) async fn resume_turn_followers(state: &AppState) {
    for active_turn in state.active_turns.snapshot() {
        let lash::TurnAddress {
            session_id,
            turn_id,
        } = active_turn.address;
        // A queued-kind claim names no run the engine executions: the queued-turn
        // workflow that minted it is gone, so the claim is released.
        if active_turn.kind == crate::WorkbenchTurnKind::Queued {
            state.active_turns.remove(&session_id, &turn_id);
            continue;
        }
        drop(tokio::spawn(resume_turn_follower(
            state.clone(),
            session_id,
            turn_id,
        )));
    }
}

/// Open the session a claimed turn belongs to and follow its run. An open can
/// race another writer still holding the lane, so a contended open is retried;
/// any other refusal leaves the claim for an operator.
async fn resume_turn_follower(state: AppState, session_id: SessionId, turn_id: TurnId) {
    let follows = &state.active_turns.follows;
    if !follows.claim(&session_id, &turn_id) {
        return;
    }
    let claim = RunClaim {
        follows: follows.clone(),
        session_id: session_id.clone(),
        run: turn_id.clone(),
    };
    watch_session_runs(&state, &session_id).await;
    let mut backoff = REFOLLOW_BACKOFF;
    loop {
        match state.open_session(&session_id, "turn.resume_follow").await {
            Ok(session) => {
                state.trace_for_session(
                    &session_id,
                    "turn.follow_resumed",
                    json!({ "turn_id": turn_id }),
                );
                drop(spawn_turn_follower(
                    state.clone(),
                    session,
                    claim,
                    FollowOrigin::UserTurn,
                    FollowFrom::Run,
                ));
                return;
            }
            Err(error) => {
                let error = AppError::session_open(error);
                let retrying = error.verdict == AppErrorVerdict::Retryable;
                state.trace_for_session(
                    &session_id,
                    "turn.follow_resume_failed",
                    json!({
                        "turn_id": turn_id,
                        "error": error.message,
                        "retrying": retrying,
                    }),
                );
                if !retrying {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(REFOLLOW_BACKOFF_CEILING);
            }
        }
    }
}

/// Who started a followed run: the page's send, or the engine on its own
/// (a next-turn input, a wake, a cron occurrence). It names the follower's
/// traces and the reason its cron resynchronisation reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowOrigin {
    UserTurn,
    QueuedTurn,
}

impl FollowOrigin {
    fn reason(self) -> &'static str {
        match self {
            Self::UserTurn => "user_turn",
            Self::QueuedTurn => "queued_turn",
        }
    }
}

/// How long a follower keeps retrying a settlement that failed retryably
/// (the session briefly busy) before it gives up and leaves the claim.
const SETTLE_RETRY_WINDOW: Duration = Duration::from_secs(120);

/// Where a follower reads its run's settlement from.
enum FollowFrom {
    /// The accepting handle: its live activity streams to the page.
    Send(Box<lash::SendHandle>),
    /// The run alone, read again after a restart or a retryable failure.
    Run,
}

fn spawn_turn_follower(
    state: AppState,
    session: lash::LashSession,
    claim: RunClaim,
    origin: FollowOrigin,
    from: FollowFrom,
) -> tokio::task::JoinHandle<TurnSettlement> {
    tokio::spawn(async move {
        let _claim = &claim;
        let turn_id = claim.run.clone();
        let session_id = session.session_id();
        let mut from = from;
        loop {
            let followed = AssertUnwindSafe(Box::pin(follow_once(
                &state, &session, &turn_id, origin, from,
            )))
            .catch_unwind()
            .await;
            // A read that failed retryably leaves the run unsettled on the
            // page: read it again rather than settle a turn still running.
            if matches!(
                &followed,
                Ok(Err(error)) if error.verdict == AppErrorVerdict::Retryable
            ) {
                tokio::time::sleep(REFOLLOW_BACKOFF).await;
                from = FollowFrom::Run;
                continue;
            }
            // The outcome is traced and published by the terminalization; a
            // follower has no caller left to hand an error to. A recorded
            // run whose settlement meets a briefly busy session settles
            // again rather than leave its claim held.
            let settled = match followed {
                Ok(Ok(())) => settle_with_retry(&state, &session_id, &turn_id)
                    .await
                    .map_err(|error| format!("{error:?}")),
                followed => terminalize_turn_execution(
                    &state,
                    &session_id,
                    &turn_id,
                    &format!("{}.failed", origin.reason()),
                    followed,
                )
                .await
                .map_err(|error| format!("{error:?}")),
            };
            settled?;
            // A settled turn may have changed the session's trigger
            // registrations: resynchronise its cron jobs with them.
            if let Err(error) =
                sync_cron_jobs_after_turn(&state, &session_id, origin.reason()).await
            {
                state.trace_for_session(
                    &session_id,
                    "cron.restate.sync_failed",
                    json!({
                        "turn_id": turn_id,
                        "error": error.message,
                    }),
                );
            }
            return Ok(());
        }
    })
}

/// Settle a recorded run, retrying while the session is briefly busy.
async fn settle_with_retry(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<(), AppError> {
    let deadline = tokio::time::Instant::now() + SETTLE_RETRY_WINDOW;
    let mut backoff = REFOLLOW_BACKOFF;
    loop {
        match settle_workbench_turn(state, session_id, turn_id).await {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.verdict,
                    AppErrorVerdict::Retryable | AppErrorVerdict::Ambiguous
                ) && tokio::time::Instant::now() < deadline =>
            {
                state.trace_for_session(
                    session_id,
                    "turn.settle_retrying",
                    json!({ "turn_id": turn_id, "error": error.message }),
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(REFOLLOW_BACKOFF_CEILING);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Read the run's settled report and publish it.
async fn follow_once(
    state: &AppState,
    session: &lash::LashSession,
    turn_id: &TurnId,
    origin: FollowOrigin,
    from: FollowFrom,
) -> Result<(), AppError> {
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let outcome = match from {
        FollowFrom::Send(handle) => {
            let ui_events = ChannelTurnEvents {
                turn_state: Arc::clone(&turn_state),
            };
            (*handle).outcome_into(&ui_events).await
        }
        FollowFrom::Run => session.run(turn_id.clone().into()).outcome().await,
    }
    .map_err(AppError::runtime)?;
    // Answered, Failed and Cancelled runs ran and settled: each has a report
    // the page shows. A parked run, or an input withdrawn before it ran, has
    // none.
    let output = match outcome {
        lash::SendOutcome::Settled { output, .. } => output.result,
        outcome => return Err(unsettled_turn(&outcome.status())),
    };
    record_turn_output_for_profile(
        state,
        session,
        TurnOutputIdentity {
            turn_id,
            durable_turn_id: turn_id,
        },
        output,
        turn_state,
        &format!("{}.completed", origin.reason()),
    )
    .await
}

/// Watch `session_id` for runs the engine starts that no follower here
/// claimed, and follow each to settlement. One watch per session per process;
/// it ends when the session can no longer be opened.
///
/// The watch subscribes before this returns, so a run the engine starts once
/// the caller goes on — the send, wake or trigger it is about to admit — is one
/// the watch sees. An open that fails here is retried by the watch itself.
pub(crate) async fn watch_session_runs(state: &AppState, session_id: &SessionId) {
    if !state.active_turns.follows.watch(session_id) {
        return;
    }
    let subscribed = Box::pin(subscribe_session_runs(state, session_id))
        .await
        .ok();
    drop(tokio::spawn(session_run_watch_task(
        state.clone(),
        session_id.clone(),
        subscribed,
    )));
}

/// A session opened for observation and its update stream from the current
/// cursor: what a watch reads run starts from.
type SessionRunSubscription = (
    lash::LashSession,
    lash::recoverable_chat::RecoverableChatSubscription,
);

async fn subscribe_session_runs(
    state: &AppState,
    session_id: &SessionId,
) -> Result<SessionRunSubscription, lash::EmbedError> {
    let session = state.open_session_for_observation(session_id).await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    let updates = session.observe().subscribe_recoverable_chat(cursor);
    Ok((session, updates))
}

async fn session_run_watch_task(
    state: AppState,
    session_id: SessionId,
    mut subscribed: Option<SessionRunSubscription>,
) {
    let follows = state.active_turns.follows.clone();
    loop {
        watch_until_idle(&state, &session_id, subscribed.take()).await;
        follows.unwatch(&session_id);
        // Work that arrived as the watch was ending takes the watch up again,
        // unless a new watch already has.
        match state.open_session_for_observation(&session_id).await {
            Ok(session) if !session_is_idle(&follows, &session).await => {
                if !follows.watch(&session_id) {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// How often an idle watch checks whether the session still has work.
const WATCH_IDLE_CHECK: Duration = Duration::from_secs(1);
/// Consecutive idle checks after which a watch ends: long enough for a
/// process a trigger started to deliver its wake.
const WATCH_IDLE_CHECKS: u32 = 5;

/// Follow each run the engine starts on `session_id` until the session has
/// no work left: no followed run, no pending input, no queued work. Reads
/// from `subscribed` when the caller already subscribed, and subscribes
/// otherwise.
async fn watch_until_idle(
    state: &AppState,
    session_id: &SessionId,
    subscribed: Option<SessionRunSubscription>,
) {
    use lash::recoverable_chat::RecoverableChatUpdate;

    let follows = &state.active_turns.follows;
    let mut backoff = REFOLLOW_BACKOFF;
    let (session, mut updates) = match subscribed {
        Some(subscribed) => subscribed,
        None => loop {
            match subscribe_session_runs(state, session_id).await {
                Ok(subscribed) => break subscribed,
                Err(error) => {
                    if AppError::session_open(error).verdict != AppErrorVerdict::Retryable {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(REFOLLOW_BACKOFF_CEILING);
                }
            }
        },
    };
    let mut idle_checks = 0;
    loop {
        let update = tokio::select! {
            update = updates.next() => update,
            () = tokio::time::sleep(WATCH_IDLE_CHECK) => {
                if session_is_idle(follows, &session).await {
                    idle_checks += 1;
                    if idle_checks >= WATCH_IDLE_CHECKS {
                        return;
                    }
                } else {
                    idle_checks = 0;
                }
                continue;
            }
        };
        let Some(update) = update else {
            return;
        };
        let Ok(RecoverableChatUpdate::Event { event, .. }) = update else {
            continue;
        };
        let lash::observe::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            continue;
        };
        let lash::TurnEvent::TurnStarted { turn_id } = &activity.event else {
            continue;
        };
        idle_checks = 0;
        let run = run_of_physical_turn(turn_id);
        if !follows.claim(session_id, &run) {
            continue;
        }
        let claim = RunClaim {
            follows: follows.clone(),
            session_id: session_id.clone(),
            run,
        };
        state.trace_for_session(
            session_id,
            "turn.follow_engine_run",
            json!({ "turn_id": claim.run }),
        );
        drop(spawn_turn_follower(
            state.clone(),
            session.clone(),
            claim,
            FollowOrigin::QueuedTurn,
            FollowFrom::Run,
        ));
    }
}

/// Whether `session` has no work a watch could still see start: no run a
/// follower here holds, no pending input, no queued work and no unfinished
/// run.
async fn session_is_idle(follows: &RunFollows, session: &lash::LashSession) -> bool {
    if follows.follows_any(&session.session_id()) {
        return false;
    }
    let durable = session.durable();
    matches!(durable.pending_turn_inputs().await, Ok(inputs) if inputs.is_empty())
        && matches!(durable.queued_work().await, Ok(batches) if batches.is_empty())
        && matches!(durable.unfinished_run().await, Ok(None))
}

/// The run a physical turn belongs to: a run's later turns are
/// `{run}:agent-frame:{n}`.
fn run_of_physical_turn(turn_id: &TurnId) -> TurnId {
    match turn_id.as_str().rsplit_once(":agent-frame:") {
        Some((run, ordinal)) if ordinal.parse::<u64>().is_ok_and(|ordinal| ordinal > 0) => {
            TurnId::parse(run).unwrap_or_else(|_| turn_id.clone())
        }
        _ => turn_id.clone(),
    }
}

/// A followed run that did not settle: it parked, holding its work until an
/// operator resolves the park; its input's delivery stalled, holding the
/// input until an operator re-arms it; or its input was withdrawn before any
/// turn ran it.
fn unsettled_turn(status: &lash::TurnStatus) -> AppError {
    match status {
        lash::TurnStatus::Parked(_) => AppError {
            status: axum::http::StatusCode::CONFLICT,
            message: format!("turn_parked: {}", crate::PARKED_TURN_MESSAGE),
            verdict: AppErrorVerdict::Parked,
            retirement: None,
        },
        // Not terminal (ADR 0109 §3): the input stays durable, and a re-armed
        // delivery executes it. Like a park, the turn is neither settled nor
        // failed, and its invocation keeps its journal.
        lash::TurnStatus::Stalled(stalled) => AppError {
            status: axum::http::StatusCode::CONFLICT,
            message: format!(
                "turn_stalled: input `{}` was never delivered to the engine ({:?} after {} \
                 attempt(s)); an operator re-arm of its delivery shifts it",
                stalled.input_id.as_str(),
                stalled.reason,
                stalled.attempts
            ),
            verdict: AppErrorVerdict::Parked,
            retirement: None,
        },
        status => AppError {
            status: axum::http::StatusCode::CONFLICT,
            message: format!("the turn ended {status:?} before any turn ran its input"),
            verdict: AppErrorVerdict::Terminal,
            retirement: None,
        },
    }
}
