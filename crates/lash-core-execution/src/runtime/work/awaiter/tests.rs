//! Unit tests for the process change hub; the tests
//! that need a registry run over a SQLite memory store set in
//! `tests/store_backed` (ADR 0102).

use super::*;

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
