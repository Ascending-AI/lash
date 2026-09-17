//! Laws of the batch intent-drain gate and its guard.
//!
//! Discharge used to be twelve hand-written calls, so any exit path that
//! forgot one pinned the gate's next index and parked every later child of the
//! batch forever. These tests pin the replacement property: the guard's
//! lifetime is the discharge, on every path out of a child body.

use super::*;
use crate::tool_dispatch::{BatchIntentDrainGate, IntentDrainGuard};

fn gate() -> Arc<BatchIntentDrainGate> {
    Arc::new(BatchIntentDrainGate::default())
}

/// The early-exit path that used to need a hand-written `complete_final_drain`:
/// a body that takes its drain turn and then returns without completing.
#[tokio::test]
async fn dropping_a_guard_mid_final_drain_releases_the_next_slot() {
    let gate = gate();
    let (first, _first_commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);
    let (second, _second_commit) = IntentDrainGuard::new(Arc::clone(&gate), 1);

    first.begin_final_drain().await;
    // Stands in for an error return between begin-drain and the former
    // complete-drain: nothing discharges the slot by hand.
    drop(first);

    timeout(Duration::from_secs(5), second.begin_final_drain())
        .await
        .expect("the dropped guard must release the next slot");
}

/// The pending and runtime-failure returns never drain intents at all; they
/// used to call `finish` by hand instead.
#[tokio::test]
async fn a_guard_dropped_without_draining_releases_the_next_slot() {
    let gate = gate();
    let (first, _first_commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);
    let (second, _second_commit) = IntentDrainGuard::new(Arc::clone(&gate), 1);

    drop(first);

    timeout(Duration::from_secs(5), second.begin_final_drain())
        .await
        .expect("a guard that never drained must still release the next slot");
}

/// A guard dropped while its child future is suspended — the cancel-grace path
/// drops the whole child future — releases the slot from the drop alone.
#[tokio::test]
async fn abandoning_a_child_future_mid_drain_releases_the_next_slot() {
    let gate = gate();
    let (first, _first_commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);
    let (second, _second_commit) = IntentDrainGuard::new(Arc::clone(&gate), 1);

    let mut child = Box::pin(async move {
        first.begin_final_drain().await;
        std::future::pending::<()>().await;
    });
    assert!(
        timeout(Duration::from_millis(50), &mut child)
            .await
            .is_err(),
        "the child must still be parked when it is abandoned"
    );
    drop(child);

    timeout(Duration::from_secs(5), second.begin_final_drain())
        .await
        .expect("dropping the child future must release the next slot");
}

/// Discharge is order-free, but turns are not: a later child that finishes
/// first cannot pull its own turn forward past an unfinished earlier child.
#[tokio::test]
async fn an_early_discharge_does_not_pull_a_later_turn_forward() {
    let gate = gate();
    let (first, _first_commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);
    let (second, _second_commit) = IntentDrainGuard::new(Arc::clone(&gate), 1);
    let (third, _third_commit) = IntentDrainGuard::new(Arc::clone(&gate), 2);

    drop(second);
    assert!(
        timeout(Duration::from_millis(50), third.begin_final_drain())
            .await
            .is_err(),
        "slot 2 must not take its turn while slot 0 is outstanding"
    );

    drop(first);
    timeout(Duration::from_secs(5), third.begin_final_drain())
        .await
        .expect("slot 2 takes its turn once slots 0 and 1 are both discharged");
}

/// Every discharge order leaves the gate open for the rest of the batch, which
/// is the property the removed debug assertion only checked in debug builds.
#[tokio::test]
async fn every_discharge_order_leaves_the_gate_open() {
    for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [1, 0, 2]] {
        let gate = gate();
        let mut guards: Vec<Option<IntentDrainGuard>> = (0..3)
            .map(|index| Some(IntentDrainGuard::new(Arc::clone(&gate), index).0))
            .collect();
        let (last, _last_commit) = IntentDrainGuard::new(Arc::clone(&gate), 3);

        for index in order {
            guards[index] = None;
        }

        timeout(Duration::from_secs(5), last.begin_final_drain())
            .await
            .unwrap_or_else(|_| panic!("discharge order {order:?} must leave the gate open"));
    }
}

/// The batch's commit view outlives the child's guard: a guard dropped without
/// committing must not be reported to the batch as a committed final result,
/// or a cancelled child would be settled instead of abandoned.
#[tokio::test]
async fn a_dropped_guard_is_not_a_committed_final_result() {
    let gate = gate();
    let (first, mut commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);

    assert!(!commit.is_committed());
    drop(first);
    assert!(!commit.is_committed());
    assert!(
        timeout(Duration::from_millis(50), commit.committed())
            .await
            .is_err(),
        "a guard dropped without committing must leave the commit signal pending"
    );
}

#[tokio::test]
async fn beginning_the_final_drain_commits_the_result() {
    let gate = gate();
    let (first, mut commit) = IntentDrainGuard::new(Arc::clone(&gate), 0);

    first.begin_final_drain().await;

    assert!(commit.is_committed());
    timeout(Duration::from_secs(5), commit.committed())
        .await
        .expect("a committed result resolves the batch's commit signal");
}
