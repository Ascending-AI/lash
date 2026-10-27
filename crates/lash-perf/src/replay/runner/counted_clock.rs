//! Timers remain pending while the counted fixture clock is frozen.
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct CountedClock(pub(super) Instant);

impl lash_core::Clock for CountedClock {
    fn now(&self) -> Instant {
        self.0
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::UNIX_EPOCH + chrono::TimeDelta::seconds(1_700_000_000)
    }

    fn sleep<'clock, 'future>(
        &'clock self,
        duration: Duration,
    ) -> futures_util::future::BoxFuture<'future, ()>
    where
        'clock: 'future,
        Self: 'future,
    {
        timer(duration.is_zero())
    }

    fn sleep_until<'clock, 'future>(
        &'clock self,
        deadline: Instant,
    ) -> futures_util::future::BoxFuture<'future, ()>
    where
        'clock: 'future,
        Self: 'future,
    {
        timer(deadline <= self.0)
    }
}

// Both futures have zero-sized state: polling an incidental timer cannot
// move a heap allocation into a turn according to which branch is ready.
fn timer<'a>(ready: bool) -> futures_util::future::BoxFuture<'a, ()> {
    if ready {
        Box::pin(std::future::poll_fn(|_| std::task::Poll::Ready(())))
    } else {
        Box::pin(std::future::pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::Clock as _;

    /// FIG-7473: wall-clock wakeups cannot fire timers while the counted
    /// fixture's injected clock remains before their deadlines.
    #[tokio::test(start_paused = true)]
    async fn fixed_clock_timers_wait_for_their_injected_deadline() {
        let clock = CountedClock(Instant::now());
        let delay = Duration::from_secs(1);
        let relative = clock.sleep(delay);
        let absolute = clock.sleep_until(clock.now() + delay);
        assert_eq!(std::mem::size_of_val(relative.as_ref().get_ref()), 0);
        assert_eq!(std::mem::size_of_val(absolute.as_ref().get_ref()), 0);
        tokio::pin!(relative, absolute);
        assert!(futures_util::poll!(relative.as_mut()).is_pending());
        assert!(futures_util::poll!(absolute.as_mut()).is_pending());
        tokio::time::advance(delay * 2).await;
        assert!(futures_util::poll!(relative.as_mut()).is_pending());
        assert!(futures_util::poll!(absolute.as_mut()).is_pending());
        clock.sleep(Duration::ZERO).await;
        clock.sleep_until(clock.now()).await;
    }
}
