use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;

/// Drive `future` to completion while pumping `event_rx` into `on_event`.
///
/// Contract (FIG-790): `future` is polled at most once per task poll.
/// Durable substrates signal a terminal attempt state — including a genuine
/// suspension — by waking synchronously and returning `Pending`, and rely on
/// their own handler wrapper consuming that state on the next poll.
/// Servicing another ready branch and re-polling `future` inside the same
/// task poll re-enters an already-completed combinator and aborts the process.
#[doc(hidden)]
pub async fn drive_with_event_pump<F, T, S, H>(
    mut future: Pin<&mut F>,
    event_rx: &mut mpsc::Receiver<T>,
    state: &mut S,
    mut on_event: H,
) -> F::Output
where
    F: Future + ?Sized,
    H: for<'event> FnMut(&'event mut S, T) -> Pin<Box<dyn Future<Output = ()> + Send + 'event>>,
{
    loop {
        tokio::select! {
            biased;

            output = future.as_mut() => return output,
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    let ready_at_start = event_rx.len();
                    on_event(state, event).await;
                    for _ in 0..ready_at_start {
                        let Ok(event) = event_rx.try_recv() else {
                            break;
                        };
                        on_event(state, event).await;
                    }
                }
                tokio::task::yield_now().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct PanicsWhenPolledAfterErrorWake {
        polled: bool,
    }

    impl Future for PanicsWhenPolledAfterErrorWake {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            assert!(
                !self.polled,
                "synchronously-woken pending future was re-polled in one task poll"
            );
            self.polled = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    #[test]
    fn synchronously_woken_pending_future_is_polled_once_per_task_poll() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        event_tx.try_send(()).expect("pre-queue pump event");
        let mut driven = Box::pin(PanicsWhenPolledAfterErrorWake { polled: false });
        let mut handler_state = ();
        let mut pump = Box::pin(super::drive_with_event_pump(
            driven.as_mut(),
            &mut event_rx,
            &mut handler_state,
            |(), _| Box::pin(async {}),
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);

        assert_eq!(pump.as_mut().poll(&mut context), Poll::Pending);
    }

    struct CompletesOnSecondPoll {
        polls: usize,
    }

    impl Future for CompletesOnSecondPoll {
        type Output = &'static str;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls += 1;
            if self.polls == 1 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready("completed")
            }
        }
    }

    #[tokio::test]
    async fn always_ready_event_channel_cannot_starve_the_driven_future() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        event_tx.try_send(()).expect("pre-queue pump event");
        let mut driven = Box::pin(CompletesOnSecondPoll { polls: 0 });
        let mut handled = 0_usize;

        let output = super::drive_with_event_pump(
            driven.as_mut(),
            &mut event_rx,
            &mut handled,
            move |handled, ()| {
                let event_tx = event_tx.clone();
                Box::pin(async move {
                    *handled += 1;
                    assert!(
                        *handled <= 16,
                        "an always-ready producer starved the driven future"
                    );
                    event_tx
                        .send(())
                        .await
                        .expect("keep the event channel continuously ready");
                })
            },
        )
        .await;

        assert_eq!(output, "completed");
        assert_eq!(handled, 1);
    }

    #[tokio::test]
    async fn drains_the_burst_that_was_ready_when_the_pass_began() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        for event in 1..=3 {
            event_tx.try_send(event).expect("pre-queue burst event");
        }
        drop(event_tx);
        let mut driven = Box::pin(CompletesOnSecondPoll { polls: 0 });
        let mut handled = Vec::new();

        let output = super::drive_with_event_pump(
            driven.as_mut(),
            &mut event_rx,
            &mut handled,
            |handled, event| {
                Box::pin(async move {
                    handled.push(event);
                })
            },
        )
        .await;

        assert_eq!(output, "completed");
        assert_eq!(handled, vec![1, 2, 3]);
    }
}
