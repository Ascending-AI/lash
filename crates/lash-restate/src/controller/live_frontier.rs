//! Restate's answer to the served-only contract (FIG-3719).
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
//! Only effects that run inside a `ctx.run` closure answer here. A process
//! command or a timer acts through its own journaled commands before any
//! closure could tell, so the controller refuses a served-only one up front.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use lash_core::{RuntimeEffectControllerError, ServedOnly};
use lash_sansio::sync::MutexExt as _;

/// Refuses a served-only effect (FIG-3719) that acts outside a `ctx.run`
/// closure — a process command, which reaches Restate through its own
/// journaled commands, or a timer — before it acts: such an effect cannot tell
/// a recorded outcome from a live one, so the command parks. A drifted
/// orchestrating binding never starts its process, and a served-only command
/// journals no sleep.
pub(super) fn refuse_outside_a_run(
    execution: &super::RestateEffectExecution,
    local_executor: &lash_core::RuntimeEffectLocalExecutor<'_>,
) -> Result<(), RuntimeEffectControllerError> {
    match local_executor.served_only() {
        Some(served_only)
            if matches!(
                execution,
                super::RestateEffectExecution::DirectProcess { .. }
                    | super::RestateEffectExecution::DurableProcessCommand { .. }
                    | super::RestateEffectExecution::Timer { .. }
            ) =>
        {
            Err(served_only.refuse())
        }
        _ => Ok(()),
    }
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
