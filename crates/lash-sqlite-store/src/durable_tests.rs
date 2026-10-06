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
        let set = SqliteStoreSet::open_with_clock(root.path().join("lash.db"), clock.clone())
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

/// A process-registry row and a trigger row keyed by `?1`.
const REGISTRY_ROW: &str = "INSERT INTO wake_allocation_floors \
     (target_session_id, process_id, allocation_floor) VALUES (?1, 'process', 1)";
const TRIGGER_ROW: &str = "INSERT INTO trigger_mutation_receipts \
     (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
     VALUES (?1, 'host', 'host', 'fingerprint', '{}', 1)";

/// How many process-registry and trigger rows `key` has, read by a
/// connection of its own, as another process sees the database.
fn registry_and_trigger_rows(path: &std::path::Path, key: &str) -> [i64; 2] {
    let connection = rusqlite::Connection::open(path).expect("open a raw connection");
    [
        "SELECT count(*) FROM wake_allocation_floors WHERE target_session_id = ?1",
        "SELECT count(*) FROM trigger_mutation_receipts WHERE operation_id = ?1",
    ]
    .map(|sql| {
        connection
            .query_row(sql, [key], |row| row.get(0))
            .expect("count a family's rows")
    })
}

/// A SQLite deployment is one database file (ADR 0132 §12): the store set's
/// writer reaches the process registry, the trigger store and the durability
/// core in its `main` database alone, so one transaction spans a
/// process-registry row, a trigger row and an actor row, and a cut before its
/// commit leaves none of them.
#[tokio::test]
async fn a_transaction_over_registry_trigger_and_actor_rows_is_atomic_under_a_cut() {
    use lash_durable::{ActorKey, DurableInstant, DurableStore as _, FormatSet, MailTx};

    let (root, set, _clock) = world(true).await;
    let root = root.expect("a file store set");
    let path = root.path().join("lash.db");
    let store = set.durable_store();
    let databases = store
        .conn
        .read(|tx| {
            tx.prepare("SELECT name FROM pragma_database_list WHERE name <> 'temp'")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .expect("list the writer's databases");
    assert_eq!(databases, ["main"], "the writer holds one database file");

    for (key, cut) in [("cut", true), ("committed", false)] {
        let actor = ActorKey::session(key).expect("an actor key");
        let mut mail = MailTx::new();
        mail.create_actor(actor.clone(), FormatSet::new("[]"));
        let written = store
            .conn
            .write_flow(move |tx| {
                tx.execute(REGISTRY_ROW, [key])?;
                tx.execute(TRIGGER_ROW, [key])?;
                let created = super::apply_mail(tx, mail, DurableInstant(1))?;
                if cut {
                    // The cut: the transaction ends here, before its commit.
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ABORT),
                        Some("cut before commit".to_owned()),
                    ));
                }
                Ok(created)
            })
            .await;
        let rows = registry_and_trigger_rows(&path, key);
        let actor_row = store.actor(&actor).await.expect("read the actor");
        if cut {
            assert!(written.is_err(), "the cut transaction does not commit");
            assert_eq!(rows, [0, 0], "a cut leaves no registry or trigger row");
            assert!(actor_row.is_none(), "a cut leaves no actor row");
        } else {
            written
                .expect("the transaction commits")
                .expect("the actor is created");
            assert_eq!(rows, [1, 1], "a commit lands the registry and trigger rows");
            assert!(actor_row.is_some(), "a commit lands the actor row");
        }
    }
}
