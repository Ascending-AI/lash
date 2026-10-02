//! A gate a law holds a trait seam at (ADR 0044 §Simulation).
//!
//! The seam calls [`Gate::pass`] where the law wants to stop it. The law
//! waits for arrivals with [`Gate::reached`] and lets them through with
//! [`Gate::open_one`] or [`Gate::open_all`]. Nothing here blocks an OS
//! thread, and every wait a law makes is bounded by [`GATE_DEADLINE`].
//!
//! A failed law leaves nothing held: a script opens its gates when it drops,
//! and an arrival at any other gate is a future of the law's own runtime,
//! which drops it with the law.
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The longest a law waits for a seam to reach a gate.
pub const GATE_DEADLINE: Duration = Duration::from_secs(10);

/// What a gate prints beside its label when a wait on it expires.
type Context = Box<dyn Fn() -> String + Send + Sync>;

/// Holds every arrival at one point of a seam until the law lets it through,
/// counting the arrivals.
pub struct Gate {
    label: String,
    arrived: AtomicUsize,
    arrivals: tokio::sync::Notify,
    permits: tokio::sync::Semaphore,
    context: Option<Context>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("label", &self.label)
            .field("arrived", &self.arrived())
            .finish_non_exhaustive()
    }
}

impl Gate {
    /// A closed gate. `label` names it in a failure.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            arrived: AtomicUsize::new(0),
            arrivals: tokio::sync::Notify::new(),
            permits: tokio::sync::Semaphore::new(0),
            context: None,
        }
    }

    /// A closed gate whose expired wait also prints `context`.
    pub(super) fn with_context(label: String, context: Context) -> Self {
        let mut gate = Self::new(label);
        gate.context = Some(context);
        gate
    }

    /// Seam side: count this arrival, then wait until the law lets it
    /// through. Returns at once when the gate was opened for good.
    pub async fn pass(&self) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        self.arrivals.notify_waiters();
        if let Ok(permit) = self.permits.acquire().await {
            permit.forget();
        }
    }

    /// How many arrivals reached the gate so far.
    pub fn arrived(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    /// Wait until `count` arrivals reached the gate. Panics with the gate's
    /// label when they have not within [`GATE_DEADLINE`].
    pub async fn reached(&self, count: usize) {
        if tokio::time::timeout(GATE_DEADLINE, self.arrivals(count))
            .await
            .is_err()
        {
            panic!("{}", self.expired(count));
        }
    }

    /// Drive `work` until `count` arrivals reached the gate. Panics when
    /// `work` finishes first, or when [`GATE_DEADLINE`] passes.
    pub async fn reached_by<T>(&self, work: &mut (impl Future<Output = T> + Unpin), count: usize) {
        tokio::select! {
            biased;
            _ = work => panic!(
                "the work held at gate `{}` finished before the law let it through",
                self.label
            ),
            () = self.reached(count) => {}
        }
    }

    /// Let the arrival that has waited longest through.
    pub fn open_one(&self) {
        self.permits.add_permits(1);
    }

    /// Open the gate for good: every waiting arrival and every later one
    /// passes.
    pub fn open_all(&self) {
        self.permits.close();
    }

    /// Wait until `count` arrivals reached the gate, however long that
    /// takes: the caller bounds the wait and words its own failure.
    /// [`Gate::reached`] is the bounded wait.
    pub async fn arrivals(&self, count: usize) {
        loop {
            // Armed before the count is read, so an arrival between the read
            // and the wait still wakes this.
            let arrival = self.arrivals.notified();
            if self.arrived() >= count {
                return;
            }
            arrival.await;
        }
    }

    fn expired(&self, count: usize) -> String {
        let mut message = format!(
            "gate `{}`: {} of {count} arrivals within {GATE_DEADLINE:?}",
            self.label,
            self.arrived()
        );
        if let Some(context) = &self.context {
            message.push_str("; ");
            message.push_str(&context());
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_arrival_waits_until_the_law_opens_the_gate() {
        let gate = Gate::new("held");
        let mut seam = std::pin::pin!(gate.pass());
        gate.reached_by(&mut seam, 1).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut seam)
                .await
                .is_err(),
            "the arrival passed a closed gate"
        );
        gate.open_one();
        seam.await;
    }

    #[tokio::test(start_paused = true)]
    async fn open_one_lets_one_arrival_through_and_open_all_every_one() {
        let gate = Gate::new("three");
        let passed = AtomicUsize::new(0);
        let arrival = || async {
            gate.pass().await;
            passed.fetch_add(1, Ordering::SeqCst);
        };
        let mut arrivals = std::pin::pin!(async { tokio::join!(arrival(), arrival(), arrival()) });
        gate.reached_by(&mut arrivals, 3).await;
        gate.open_one();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut arrivals)
                .await
                .is_err(),
            "one permit let every arrival through"
        );
        assert_eq!(passed.load(Ordering::SeqCst), 1);
        gate.open_all();
        arrivals.await;
        gate.pass().await;
        assert_eq!(
            (passed.load(Ordering::SeqCst), gate.arrived()),
            (3, 4),
            "an open gate holds no later arrival"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "finished before the law let it through")]
    async fn work_that_finishes_before_its_gate_fails_the_law() {
        let gate = Gate::new("never reached");
        let mut work = std::pin::pin!(async {});
        gate.reached_by(&mut work, 1).await;
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "gate `missed`: 0 of 1 arrivals within 10s; the trace")]
    async fn a_gate_nothing_reaches_fails_at_the_deadline_with_its_label_and_context() {
        let gate = Gate::with_context("missed".into(), Box::new(|| "the trace".into()));
        gate.reached(1).await;
    }
}
