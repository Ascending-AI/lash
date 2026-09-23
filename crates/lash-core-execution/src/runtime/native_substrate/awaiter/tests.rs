//! Unit tests for the process awaiter backoff and the change hub; the tests
//! that need a registry run over a SQLite memory deployment in
//! `tests/store_backed` (ADR 0102).

use std::time::Duration;

use super::*;
use crate::ProcessId;

#[test]
fn backoff_schedule_uses_work_cadence_poll_bounds() {
    let defaults = WorkCadencePolicy::default();
    assert_eq!(defaults.poll_initial, Duration::from_millis(25));
    assert_eq!(defaults.poll_max, Duration::from_secs(1));

    let mut backoff = defaults.poll_initial;
    let mut schedule = vec![backoff];
    while backoff < defaults.poll_max {
        backoff = next_backoff(backoff, defaults.poll_max);
        schedule.push(backoff);
    }
    assert_eq!(
        schedule,
        [25, 50, 100, 200, 400, 800, 1000]
            .into_iter()
            .map(Duration::from_millis)
            .collect::<Vec<_>>(),
        "the backoff doubles from the 25ms floor and saturates at the 1s cap"
    );
    assert_eq!(
        next_backoff(defaults.poll_max, defaults.poll_max),
        defaults.poll_max,
        "the cap is absorbing"
    );

    let custom_max = Duration::from_millis(90);
    assert_eq!(
        next_backoff(Duration::from_millis(60), custom_max),
        custom_max,
        "a configured poll cap, rather than a hidden constant, bounds the awaiter"
    );
}

#[tokio::test]
async fn hub_subscribe_then_notify_wakes_and_gc_drops_empty_entry() {
    let hub = ProcessChangeHub::new();
    let mut rx = hub.subscribe(&ProcessId::from("proc"));
    hub.notify(&ProcessId::from("proc"));
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("notify should wake")
        .expect("sender remains open");

    drop(rx);
    hub.notify(&ProcessId::from("proc"));
    assert_eq!(hub.tracked_processes(), 0);
}
