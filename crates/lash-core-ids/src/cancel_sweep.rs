//! The cancellation sweep: proof that an operation survives being dropped at
//! each of its awaits.
//!
//! A future can be dropped at any point it returned `Pending`: a `select!`
//! loses, a timeout fires, a caller goes away. Whatever the operation claimed
//! before that point (a queue position, a cache slot, an installed frame, an
//! in-flight count) must be released or handed on by an owner that already
//! exists. [`sweep_drops`] checks exactly that. It starts a fresh scenario,
//! polls its future `n` times and drops it, for `n = 0, 1, 2, …`, until a run
//! completes, and after every cut (and after the completing run, its output
//! dropped) it runs the caller's invariant check: the next acquirer proceeds,
//! no reserved, held or sealed state remains, the prior frame is installed.
//!
//! Polls use a manual waker: a run polls its future again only once the
//! future's own waker fired, so every cut is a point the operation really
//! suspended at. Waiting for that wake yields to the test's runtime, which
//! lets tasks the scenario spawned run and, under a paused tokio clock,
//! advances time to the next timer. The scenario must make progress by
//! itself (a releaser joined into its future, a server it talks to); a run
//! that is never woken again hangs, and one that keeps waking without
//! completing fails at [`MAX_POLLS`].

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};

/// The most polls one run may take before the sweep refuses it.
pub const MAX_POLLS: usize = 10_000;

/// Where one run of a sweep ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut {
    /// How many times the future was polled.
    pub polls: usize,
    /// Whether the last poll completed the future (its output was dropped
    /// before the check); otherwise the future was dropped while pending.
    pub completed: bool,
}

impl std::fmt::Display for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ended = if self.completed {
            "completed"
        } else {
            "dropped"
        };
        let plural = if self.polls == 1 { "" } else { "s" };
        write!(f, "{ended} after {} poll{plural}", self.polls)
    }
}

/// Drops a fresh scenario's future after 0, 1, 2, … polls until one run
/// completes, checking `check` after every run. `start` builds the scenario:
/// state the check reads and the future under test, which must own what it
/// uses. Returns the number of polls the completing run took.
pub async fn sweep_drops<S, F>(
    mut start: impl AsyncFnMut() -> (S, F),
    mut check: impl AsyncFnMut(S, Cut),
) -> usize
where
    F: Future,
{
    for budget in 0..=MAX_POLLS {
        let (state, future) = start().await;
        let mut future = Box::pin(future);
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut polls = 0;
        let mut output = None;
        while polls < budget {
            if polls > 0 {
                wakes.next().await;
            }
            polls += 1;
            if let Poll::Ready(ready) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
                output = Some(ready);
                break;
            }
        }
        drop(future);
        let completed = output.is_some();
        drop(output);
        check(state, Cut { polls, completed }).await;
        if completed {
            return polls;
        }
    }
    panic!("the swept future did not complete within {MAX_POLLS} polls");
}

/// The manual waker of one run: records that the future asked to be polled
/// again and wakes the sweep's own task to do it.
#[derive(Default)]
struct Wakes {
    woken: AtomicBool,
    sweep: Mutex<Option<Waker>>,
}

impl Wakes {
    /// Ready once the future's waker fired since the last poll.
    fn next(&self) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(|cx| {
            if self.woken.swap(false, Ordering::SeqCst) {
                return Poll::Ready(());
            }
            *self.sweep.lock().unwrap_or_else(PoisonError::into_inner) = Some(cx.waker().clone());
            // A wake between the check and the registration is not lost.
            if self.woken.swap(false, Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }
}

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        let sweep = self
            .sweep
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(sweep) = sweep {
            sweep.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The sweep cuts a future at every suspension it really reached, in
    /// order, and checks after each cut and after completion.
    #[tokio::test]
    async fn a_sweep_cuts_at_every_suspension_then_completes() {
        let cuts = Mutex::new(Vec::new());
        let entered = Arc::new(AtomicUsize::new(0));
        let polls = sweep_drops(
            async || {
                entered.store(0, Ordering::SeqCst);
                let entered = Arc::clone(&entered);
                ((), async move {
                    for _ in 0..3 {
                        entered.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                    }
                })
            },
            async |(), cut| {
                let reached = entered.load(Ordering::SeqCst);
                cuts.lock().unwrap().push((cut, reached));
            },
        )
        .await;
        assert_eq!(polls, 4);
        let cut = |polls, completed| Cut { polls, completed };
        assert_eq!(
            cuts.into_inner().unwrap(),
            [
                (cut(0, false), 0),
                (cut(1, false), 1),
                (cut(2, false), 2),
                (cut(3, false), 3),
                (cut(4, true), 3),
            ]
        );
    }
}
