use std::convert::Infallible;
use std::sync::atomic::AtomicUsize;

use lash_core::{SessionCursor, SessionObservationEventPayload, SessionRevision};

use super::*;

fn session() -> SessionId {
    SessionId::from("fanout")
}

fn row<'a>(incarnation: &'a str, position: u64, revision: u64, payload: &[u8]) -> Row<'a> {
    Row {
        incarnation,
        position,
        revision,
        turn_id: None,
        payload: payload.to_vec(),
    }
}

/// Decodes a row into a resident change, counting the decodes.
fn decoder(
    decodes: &AtomicUsize,
) -> impl Fn(&Row<'_>) -> Result<Arc<SessionObservationEvent>, Infallible> + '_ {
    move |row| {
        decodes.fetch_add(1, Ordering::SeqCst);
        let cursor = SessionCursor::new(
            row.incarnation,
            session(),
            SessionRevision::new(row.revision),
            row.position,
        );
        Ok(Arc::new(
            SessionObservationEvent::new(
                None,
                cursor,
                SessionObservationEventPayload::ResidentChanged,
            )
            .expect("a store cursor is valid"),
        ))
    }
}

fn decoded(reader: &Reader, row: Row<'_>, decodes: &AtomicUsize) -> Arc<SessionObservationEvent> {
    let Ok(event) = reader.event(row, decoder(decodes));
    event
}

/// A followed session decodes a row once for every reader, and decodes it
/// again whenever the row it reads differs in its payload bytes, revision
/// or incarnation: a held event never stands in for a changed row. A
/// session nobody here follows holds nothing.
#[test]
fn a_held_event_stands_only_for_the_identical_row() {
    let fanout = Fanout::default();
    let decodes = AtomicUsize::new(0);
    let unfollowed = fanout.reader(&session());
    decoded(&unfollowed, row("a", 1, 1, b"one"), &decodes);
    decoded(&unfollowed, row("a", 1, 1, b"one"), &decodes);
    assert_eq!(decodes.load(Ordering::SeqCst), 2);

    let _follow = fanout.follow(&session(), 16, 1024);
    let first = decoded(&fanout.reader(&session()), row("a", 1, 1, b"one"), &decodes);
    let again = decoded(&fanout.reader(&session()), row("a", 1, 1, b"one"), &decodes);
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(decodes.load(Ordering::SeqCst), 3);

    let reader = fanout.reader(&session());
    for changed in [
        row("a", 1, 1, b"One"),
        row("a", 1, 2, b"One"),
        row("b", 1, 2, b"One"),
    ] {
        let event = decoded(&reader, changed, &decodes);
        assert!(!Arc::ptr_eq(&first, &event));
    }
    assert_eq!(decodes.load(Ordering::SeqCst), 6);
}

/// A doorbell that ends a session's continuity drops what it holds, and a
/// read that started before it decodes for itself without repopulating
/// the share.
#[test]
fn a_read_from_before_an_invalidation_never_repopulates_the_share() {
    let fanout = Fanout::default();
    let decodes = AtomicUsize::new(0);
    let _follow = fanout.follow(&session(), 16, 1024);
    decoded(&fanout.reader(&session()), row("a", 1, 1, b"one"), &decodes);
    let before = fanout.reader(&session());
    fanout.rang(&session(), true);
    assert_eq!(fanout.held.load(Ordering::SeqCst), 0);
    decoded(&before, row("a", 2, 1, b"two"), &decodes);
    let after = fanout.reader(&session());
    decoded(&after, row("a", 1, 1, b"one"), &decodes);
    decoded(&after, row("a", 2, 1, b"two"), &decodes);
    assert_eq!(decodes.load(Ordering::SeqCst), 4);
    decoded(&after, row("a", 2, 1, b"two"), &decodes);
    assert_eq!(decodes.load(Ordering::SeqCst), 4);
}

/// A share holds at most its session's retained count and twice its
/// retained bytes, and every share together at most the node's budget;
/// the last subscriber leaving releases everything its session held.
#[test]
fn held_events_stay_within_their_session_and_node_bounds() {
    let charge = 2 * 8 + HELD_CHARGE;
    let fanout = Fanout::with_budget(3 * charge);
    let decodes = AtomicUsize::new(0);
    let left = fanout.follow(&session(), 2, 1024);
    for position in 1..=3 {
        decoded(
            &fanout.reader(&session()),
            row("a", position, 1, b"8 bytes."),
            &decodes,
        );
    }
    assert_eq!(fanout.held.load(Ordering::SeqCst), 2 * charge);
    decoded(
        &fanout.reader(&session()),
        row("a", 1, 1, b"8 bytes."),
        &decodes,
    );
    assert_eq!(
        decodes.load(Ordering::SeqCst),
        4,
        "the oldest event was evicted"
    );

    let other = SessionId::from("other");
    let right = fanout.follow(&other, 16, 1024);
    for position in 1..=3 {
        decoded(
            &fanout.reader(&other),
            row("a", position, 1, b"8 bytes."),
            &decodes,
        );
        assert!(fanout.held.load(Ordering::SeqCst) <= 3 * charge);
    }
    drop((left, right));
    assert_eq!(fanout.held.load(Ordering::SeqCst), 0);
}

fn events() -> LiveRead {
    Arc::new(Ok(Read::Events(Vec::new())))
}

/// Tails at one position share one read, in flight or finished, until the
/// next doorbell; a tail woken by a doorbell that rang after that read
/// started reads for itself: the shared snapshot may predate what the
/// doorbell announced.
#[tokio::test]
async fn a_tail_woken_after_a_read_started_never_shares_it() {
    let fanout = Fanout::default();
    let follow = fanout.follow(&session(), 16, 1024);
    let share = Arc::clone(follow.share());
    let starts = Arc::new(AtomicUsize::new(0));
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let held = held.shared();
    let start = || {
        let starts = Arc::clone(&starts);
        let held = held.clone();
        move || {
            starts.fetch_add(1, Ordering::SeqCst);
            async move {
                let _ = held.await;
                events()
            }
            .boxed()
        }
    };
    let leader = share.live_read("a", 4, start());
    let joined = share.live_read("a", 4, start());
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    fanout.rang(&session(), false);
    let woken = share.live_read("a", 4, start());
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    let _ = release.send(());
    let (leader, joined, woken) = tokio::join!(leader, joined, woken);
    assert!(Arc::ptr_eq(&leader, &joined));
    assert!(!Arc::ptr_eq(&leader, &woken));
    let late = share.live_read("a", 4, start()).await;
    assert!(
        Arc::ptr_eq(&late, &woken),
        "a finished read is shared until the next doorbell"
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}
