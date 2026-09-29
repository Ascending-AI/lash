//! Unit tests for the process awaiter backoff and the change hub; the tests
//! that need a registry run over a SQLite memory deployment in
//! `tests/store_backed` (ADR 0102).

use std::time::Duration;

use super::*;

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
    let mut rx = hub.subscribe(&crate::process_id_for_test("proc"));
    hub.notify(&crate::process_id_for_test("proc"));
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("notify should wake")
        .expect("sender remains open");

    drop(rx);
    assert_eq!(hub.tracked_processes(), 0);
}

#[test]
fn hub_drop_removes_entry_without_notification() {
    let hub = ProcessChangeHub::new();
    let subscription = hub.subscribe(&crate::process_id_for_test("unmodified"));
    assert_eq!(hub.tracked_processes(), 1);
    drop(subscription);
    assert_eq!(hub.tracked_processes(), 0);
}

#[test]
fn hub_multiple_subscriptions_and_clones_release_only_last() {
    let hub = ProcessChangeHub::new();
    let process_id = crate::process_id_for_test("shared");
    let subscription = hub.subscribe(&process_id);
    let mut cloned = subscription.clone();
    let mut another = hub.subscribe(&process_id);
    drop(subscription);
    assert_eq!(hub.tracked_processes(), 1);
    hub.notify(&process_id);
    assert!(cloned.has_changed().expect("clone stays attached"));
    assert!(
        another
            .has_changed()
            .expect("second subscriber stays attached")
    );
    cloned.mark_unchanged();
    another.mark_unchanged();
    drop(another);
    assert_eq!(hub.tracked_processes(), 1);
    hub.notify(&process_id);
    assert!(
        cloned
            .has_changed()
            .expect("last subscriber stays attached")
    );
    drop(cloned);
    assert_eq!(hub.tracked_processes(), 0);
}

#[test]
fn hub_distinct_processes_track_only_live_subscriptions() {
    let hub = ProcessChangeHub::new();
    let live = (0..4)
        .map(|index| hub.subscribe(&crate::process_id_for_test(&format!("live-{index}"))))
        .collect::<Vec<_>>();
    for index in 0..128 {
        let process_id = crate::process_id_for_test(&format!("finished-{index}"));
        let subscription = hub.subscribe(&process_id);
        assert_eq!(hub.tracked_processes(), live.len() + 1);
        if index % 2 == 0 {
            hub.notify(&process_id);
        }
        drop(subscription);
        assert_eq!(hub.tracked_processes(), live.len());
    }
    drop(live);
    assert_eq!(hub.tracked_processes(), 0);
}

#[test]
fn hub_subscribe_drop_notify_interleavings_preserve_final_wake() {
    let hub = ProcessChangeHub::new();
    let process_id = crate::process_id_for_test("reused");
    for drop_before_subscribe in [true, false] {
        for notify_before_drop in [true, false] {
            let old = hub.subscribe(&process_id);
            if notify_before_drop {
                hub.notify(&process_id);
            }
            let mut fresh = if drop_before_subscribe {
                drop(old);
                assert_eq!(hub.tracked_processes(), 0);
                hub.subscribe(&process_id)
            } else {
                let fresh = hub.subscribe(&process_id);
                drop(old);
                fresh
            };
            fresh.mark_unchanged();
            hub.notify(&process_id);
            assert!(fresh.has_changed().expect("fresh channel remains attached"));
            assert_eq!(hub.tracked_processes(), 1);
            drop(fresh);
            assert_eq!(hub.tracked_processes(), 0);
        }
    }
}

#[test]
fn hub_concurrent_unsubscribe_and_subscribe_preserve_final_wake() {
    let hub = ProcessChangeHub::new();
    let process_id = crate::process_id_for_test("racing");
    for _ in 0..64 {
        let old = hub.subscribe(&process_id);
        let start = std::sync::Barrier::new(3);
        let (ready, subscribed) = std::sync::mpsc::channel();
        let fresh = std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                drop(old);
            });
            let fresh = scope.spawn(|| {
                start.wait();
                let subscription = hub.subscribe(&process_id);
                ready.send(()).expect("notify thread is waiting");
                subscription
            });
            start.wait();
            subscribed.recv().expect("fresh subscription is registered");
            hub.notify(&process_id);
            fresh.join().expect("subscriber thread")
        });
        assert!(fresh.has_changed().expect("final wake stays observable"));
        assert_eq!(hub.tracked_processes(), 1);
        drop(fresh);
        assert_eq!(hub.tracked_processes(), 0);
    }
}

#[tokio::test]
async fn hub_subscription_can_outlive_hub() {
    let hub = ProcessChangeHub::new();
    let subscription = hub.subscribe(&crate::process_id_for_test("orphan"));
    drop(hub);
    let mut cloned = subscription.clone();
    assert!(cloned.changed().await.is_err(), "the hub owned the sender");
    drop(subscription);
    drop(cloned);
}
