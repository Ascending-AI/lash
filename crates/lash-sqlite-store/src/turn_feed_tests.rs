//! The turn-change feed's cursor law (FIG-5276) over SQLite: the serialized
//! writer commits in sequence order, and a reader polling while many writers
//! commit sees each change exactly once.

use crate::SqliteStoreSet;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_polling_reader_never_skips_or_repeats_a_change_in_memory() {
    let set = SqliteStoreSet::memory()
        .await
        .expect("open the memory store set");
    lash_core_execution::testing::turn_feed_law::a_polling_reader_never_skips_or_repeats_a_turn_change(
        set.session_store_factory(),
        16,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_polling_reader_never_skips_or_repeats_a_change_on_file() {
    let root = tempfile::tempdir().expect("store directory");
    let set = SqliteStoreSet::open(root.path().join("lash.db"))
        .await
        .expect("open the file store set");
    lash_core_execution::testing::turn_feed_law::a_polling_reader_never_skips_or_repeats_a_turn_change(
        set.session_store_factory(),
        16,
        None,
    )
    .await;
}
