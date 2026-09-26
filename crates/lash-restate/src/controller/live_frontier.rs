//! Restate's answer to the served-only contract (FIG-3719, FIG-3779).
//!
//! A served-only effect belongs to a replayed command whose tool binding
//! drifted: it may be served its recorded result, never run live. Restate
//! replays its journal by position, so whether a run's result is recorded is
//! known only as the replay reaches the run. A recorded result is served
//! without running the run's closure; a closure that runs is the live
//! frontier, so it raises [`LiveFrontier`] and never completes. The run then
//! proposes no result, the journal records nothing for it, and the effect
//! refuses with its drift.
//!
//! A process start and a timer act through journaled commands of their own —
//! a registry write and a workflow send, a sleep — before any closure of
//! theirs could tell. So each journals a frontier marker first: a `ctx.run`
//! at `lash:{replay_key}:frontier`, unconditionally, whether or not the
//! effect is served only, since whether it is depends on the live registry
//! and the journal must not. A served-only start or sleep whose marker's
//! closure runs is at the live frontier and refuses having acted on nothing;
//! one whose marker is served was issued before, and replays. A served start
//! goes on only when its registration was recorded or a retained process
//! already holds its start key (FIG-3779 option 3, ADR 0107): a marker
//! recorded by an attempt that died before registering is not a recorded
//! start, so it refuses and records nothing.
//!
//! Every other process command still refuses up front when served only.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use lash_core::{
    RuntimeEffectControllerError, RuntimeEffectInvocation, RuntimeErrorCode, ServedOnly, StartKey,
};
use lash_sansio::sync::MutexExt as _;
use restate_sdk::serde::Json;

use super::context::RestateControllerContext;
use super::effect_journal::{FrontierMark, frontier_entry, recorded_frontier_mark};

/// Refuses a served-only effect (FIG-3719) that has no frontier marker —
/// every process command but a start — before it acts. Such a command's
/// recorded steps (FIG-3827) do not check the live frontier, so it cannot
/// tell a recorded outcome from a live one, and the command parks. A start and a timer answer at their frontier marker
/// instead ([`pass_process_start_frontier`], [`pass_sleep_frontier`]).
pub(super) fn refuse_outside_a_run(
    execution: &super::RestateEffectExecution,
    local_executor: &lash_core::RuntimeEffectLocalExecutor<'_>,
) -> Result<(), RuntimeEffectControllerError> {
    let marked = match execution {
        super::RestateEffectExecution::DirectProcess { command, .. } => {
            matches!(command.as_ref(), lash_core::ProcessCommand::Start { .. })
        }
        super::RestateEffectExecution::DurableProcessCommand { .. } => false,
        _ => true,
    };
    match local_executor.served_only() {
        Some(served_only) if !marked => Err(served_only.refuse()),
        _ => Ok(()),
    }
}

/// Journals a process start's frontier marker, recording the start's
/// idempotency key, and answers whether the start may go on to its
/// registration step.
///
/// Every start passes it: one that is not served only goes on whether its
/// marker ran or was served. A served-only start whose marker's closure runs
/// is at the live frontier and refuses. One whose marker is served goes on to
/// its journaled registration step, which answers the rest: a recorded
/// registration is that start and replays; a registration step that runs live
/// goes on only when a retained process already holds the start's key (the
/// attempt that issued it registered and died before the step journaled);
/// with none, nothing was started and the start refuses there, having acted
/// on nothing (FIG-3779 option 3, ADR 0107).
pub(super) async fn pass_process_start_frontier<'ctx, C>(
    context: &C,
    invocation: &RuntimeEffectInvocation,
    start_key: &StartKey,
    served_only: Option<&ServedOnly>,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let mark = FrontierMark::ProcessStart {
        start_key: start_key.clone(),
    };
    pass_frontier(context, invocation, mark, served_only).await
}

/// Journals a timer's frontier marker: a served-only sleep whose marker's
/// closure runs refuses before it journals its sleep, and one whose marker is
/// served replays its sleep.
///
/// A sleep acts on nothing outside the journal, so a marker recorded by an
/// attempt that died before journaling its sleep lets that sleep be journaled
/// on the redrive; the next dispatching effect of the drifted command still
/// answers at its own frontier.
pub(super) async fn pass_sleep_frontier<'ctx, C>(
    context: &C,
    invocation: &RuntimeEffectInvocation,
    served_only: Option<&ServedOnly>,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    pass_frontier(context, invocation, FrontierMark::Sleep, served_only).await
}

/// Journals `mark` at `lash:{replay_key}:frontier`. A served-only effect's
/// marker whose closure runs is the live frontier: the run proposes nothing
/// and the effect refuses. Otherwise the recorded mark must be `mark`.
async fn pass_frontier<'ctx, C>(
    context: &C,
    invocation: &RuntimeEffectInvocation,
    mark: FrontierMark,
    served_only: Option<&ServedOnly>,
) -> Result<(), RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let name = format!("{}:frontier", super::restate_effect_name(invocation));
    let entry = frontier_entry(&mark)?;
    let live = served_only.cloned().map(LiveFrontier::new);
    let closure_live = live.clone();
    let run = context.run_json_send::<serde_json::Value, _>(name.clone(), None, async move {
        if let Some(live) = &closure_live {
            return live.reached().await;
        }
        entry
    });
    let journaled = match live {
        None => run.await,
        Some(live) => live.serve(run).await?,
    };
    let Json(recorded) = journaled.map_err(|terminal| {
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineEffectController,
            format!("Restate frontier marker `{name}` failed: {terminal}"),
        )
    })?;
    recorded_frontier_mark(&name, recorded, &mark)
}

/// The signal a served-only run's closure raises when Restate runs it live,
/// built only for a served-only effect.
#[derive(Clone)]
pub(crate) struct LiveFrontier {
    served_only: ServedOnly,
    state: Arc<Mutex<(bool, Option<Waker>)>>,
}

impl LiveFrontier {
    pub(super) fn new(served_only: ServedOnly) -> Self {
        Self {
            served_only,
            state: Arc::default(),
        }
    }

    /// Called in place of a served-only run's body when Restate runs its
    /// closure: the live frontier is reached. Signals the racing caller and
    /// never completes, so the run proposes no result.
    pub(super) async fn reached<T>(&self) -> T {
        {
            let mut state = self.state.lock_recover();
            state.0 = true;
            if let Some(waker) = state.1.take() {
                waker.wake();
            }
        }
        std::future::pending().await
    }

    /// Awaits `run`, or refuses the served-only effect once its closure has
    /// reached the live frontier. Exactly one side can finish: the run with
    /// its served result, or the signal from a closure that never completes.
    pub(super) async fn serve<T>(
        &self,
        run: impl Future<Output = T>,
    ) -> Result<T, RuntimeEffectControllerError> {
        let mut run = std::pin::pin!(run);
        let served = std::future::poll_fn(|cx| {
            if let Poll::Ready(recorded) = run.as_mut().poll(cx) {
                return Poll::Ready(Some(recorded));
            }
            self.poll_reached(cx).map(|()| None)
        })
        .await;
        // The refusal leaves an orphaned run command — stored, never
        // completed — as the last entry of this attempt's journal. Nothing
        // may be journaled after it in the same attempt: the next replay
        // would await the orphan while commands remain and fail with
        // JOURNAL_MISMATCH, wedging the invocation even after the tool is
        // restored. The refusal trips the command's guard, the run stops on
        // it (no seal is journaled after a nested error), and the park is
        // written through the session store, not the journal.
        served.ok_or_else(|| self.served_only.refuse())
    }

    fn poll_reached(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock_recover();
        if state.0 {
            return Poll::Ready(());
        }
        state.1 = Some(cx.waker().clone());
        Poll::Pending
    }
}
