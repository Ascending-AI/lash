//! Poll helpers for laws that wait on state no event reports: an atomic a
//! fixture flips, or a live server's admin view of an invocation.

use std::time::{Duration, Instant};

/// Wait until `ready` holds, polling every 20 ms; panics naming `what` when
/// it still does not after 30 s.
pub async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Poll `probe` every 200 ms until it yields `Some` and return it; panics
/// naming `what` once `budget` is spent.
pub async fn poll_until<T, P, F>(budget: Duration, what: &str, mut probe: P) -> T
where
    P: FnMut() -> F,
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {budget:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
