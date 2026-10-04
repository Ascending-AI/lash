//! The SDK registers an owned callback; its logical owner polls the borrowed body.
//! Only the callback-start signal and the captured value cross this rendezvous.
//! A replayed X never starts its body, and the SDK alone accepts the result.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use lash_sansio::sync::MutexExt;

/// Register every callback with the SDK, including journaled work nested in V.
/// Borrowing Run::poll can discard a not-yet-executable nested closure; start
/// retains it until engine progress selects it.
pub(super) fn register<'run, 'ctx, C, T, F>(
    context: &C,
    name: String,
    retry_policy: Option<restate_sdk::context::RunRetryPolicy>,
    body: F,
) -> impl Future<Output = Result<restate_sdk::serde::Json<T>, restate_sdk::errors::TerminalError>>
+ Send
+ 'run
where
    C: restate_sdk::context::ContextSideEffects<'ctx>,
    T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'run,
{
    let (callback, owner) = bridge();
    let relay = Arc::new(super::wake::ClosureWakeRelay::default());
    let closure_relay = relay.clone();
    let run = restate_sdk::context::ContextSideEffects::run(context, move || async move {
        super::wake::relay_closure_wakes(callback, closure_relay)
            .await
            .map(restate_sdk::serde::Json)
            .map_err(|fault| restate_sdk::errors::HandlerError::from(std::io::Error::other(fault)))
    });
    let run = restate_sdk::context::RunFuture::name(run, name);
    let run = match retry_policy {
        Some(policy) => restate_sdk::context::RunFuture::retry_policy(run, policy),
        None => run,
    };
    owner.drive(
        body,
        super::wake::guard_restate_run_future(run.start(), relay),
    )
}

struct State<T> {
    started: bool,
    owner_alive: bool,
    value: Option<Result<T, String>>,
    owner_wake: Option<Waker>,
    callback_wake: Option<Waker>,
}

pub(super) struct Callback<T>(Arc<Mutex<State<T>>>);
pub(super) struct Owner<T>(Arc<Mutex<State<T>>>);

pub(super) fn bridge<T>() -> (Callback<T>, Owner<T>) {
    let state = Arc::new(Mutex::new(State {
        started: false,
        owner_alive: true,
        value: None,
        owner_wake: None,
        callback_wake: None,
    }));
    (Callback(state.clone()), Owner(state))
}

impl<T> Future for Callback<T> {
    type Output = Result<T, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (answer, wake) = {
            let mut state = self.0.lock_recover();
            state.started = true;
            let wake = state.owner_wake.take();
            let answer = if let Some(value) = state.value.take() {
                Poll::Ready(value)
            } else if !state.owner_alive {
                Poll::Ready(Err("the logical Run owner ended before X".into()))
            } else {
                state.callback_wake = Some(cx.waker().clone());
                Poll::Pending
            };
            (answer, wake)
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        answer
    }
}

impl<T> Owner<T> {
    fn started(&self, cx: &Context<'_>) -> bool {
        let mut state = self.0.lock_recover();
        if !state.started {
            state.owner_wake = Some(cx.waker().clone());
        }
        state.started
    }

    fn complete(&self, value: Result<T, String>) {
        let wake = {
            let mut state = self.0.lock_recover();
            state.value = Some(value);
            state.callback_wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    pub(super) async fn drive<F, R>(self, body: F, result: R) -> R::Output
    where
        F: Future<Output = Result<T, String>>,
        R: Future,
    {
        let mut body = Some(Box::pin(body));
        let mut result = Box::pin(result);
        std::future::poll_fn(|cx| {
            self.started(cx);
            // Progressing the SDK runs its fresh callback or supplies cached X.
            if let Poll::Ready(value) = result.as_mut().poll(cx) {
                return Poll::Ready(value);
            }
            if self.started(cx)
                && let Some(future) = body.as_mut()
                && let Poll::Ready(value) = future.as_mut().poll(cx)
            {
                body = None;
                self.complete(value);
            }
            Poll::Pending
        })
        .await
    }
}

impl<T> Drop for Owner<T> {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.0.lock_recover();
            state.owner_alive = false;
            state.callback_wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}
