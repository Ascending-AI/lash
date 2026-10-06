//! The durability engine's fencing laws (`lash_durable::laws`) over SQLite,
//! in memory and in a file.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::testing::TestClock;
use lash_durable::laws;

use crate::SqliteStoreSet;

/// A fresh store set on a clock the law moves, and the directory a file set
/// lives in.
async fn world(file: bool) -> (Option<tempfile::TempDir>, SqliteStoreSet, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000_000));
    if file {
        let root = tempfile::tempdir().expect("store root");
        let set = SqliteStoreSet::open_with_clock(root.path(), clock.clone())
            .await
            .expect("open the file store set");
        (Some(root), set, clock)
    } else {
        let set = SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("open the memory store set");
        (None, set, clock)
    }
}

fn advancing(clock: &Arc<TestClock>) -> impl Fn(Duration) + Sync + '_ {
    move |by| clock.advance(u64::try_from(by.as_millis()).expect("a law advances by millis"))
}

macro_rules! law {
    ($name:ident, |$store:ident, $advance:ident| $law:expr) => {
        mod $name {
            use super::*;

            async fn run(file: bool) {
                let (_root, set, clock) = world(file).await;
                let $store = &set.durable_store();
                let $advance = &advancing(&clock);
                $law.await.unwrap_or_else(|broken| panic!("{broken}"));
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn memory() {
                run(false).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn file() {
                run(true).await;
            }
        }
    };
}

law!(one_writer_under_a_claim_race, |store, _advance| {
    laws::one_writer_under_a_claim_race(store)
});
law!(a_zombie_owner_past_reap_cannot_commit, |store, advance| {
    laws::a_zombie_owner_past_reap_cannot_commit(store, advance)
});
law!(
    a_lapsed_heartbeat_is_reaped_with_an_epoch_bump,
    |store, advance| { laws::a_lapsed_heartbeat_is_reaped_with_an_epoch_bump(store, advance) }
);
law!(mail_from_non_owners_wakes_the_actor, |store, advance| {
    laws::mail_from_non_owners_wakes_the_actor(store, advance)
});
law!(no_write_through_a_stale_epoch, |store, advance| {
    laws::no_write_through_a_stale_epoch(store, advance)
});
