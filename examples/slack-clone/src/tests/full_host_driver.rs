//! The full-host E2E driver reads the bot's storage where the bot writes it.
//!
//! `scripts/slack-clone-full-host-e2e.py` asserts over the stores themselves —
//! the platform database, the bot's event ledger and the SQLite backend's
//! session catalog — rather than over an API, so it carries its own picture of
//! the bot's data layout. It runs only on the manual full-profile dispatch, so
//! a layout that moves under it would otherwise surface there, after the move
//! has merged. These checks run with the crate's unit tests on every PR that
//! changes the bot or its backend.

use std::collections::BTreeSet;

use crate::bot::runtime::{PRIOR_STORE_LAYOUT, SESSIONS_ROOT, open_backend};

/// The driver as checked in, read as data.
const DRIVER: &str = include_str!("../../../../scripts/slack-clone-full-host-e2e.py");

/// The session catalog the SQLite backend keeps under its sessions root.
const SESSION_CATALOG: &str = "durable-core.db";

fn tables(connection: &rusqlite::Connection) -> BTreeSet<String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("prepare table listing");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("list tables")
        .map(|name| name.expect("table name"))
        .collect()
}

fn schema_tables(schema: &str) -> BTreeSet<String> {
    let connection = rusqlite::Connection::open_in_memory().expect("open in-memory database");
    connection.execute_batch(schema).expect("apply schema");
    tables(&connection)
}

/// Every table named after `FROM` or `JOIN` in the driver's SQL.
fn driver_tables() -> BTreeSet<String> {
    let words: Vec<&str> = DRIVER
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == "FROM" || pair[0] == "JOIN")
        .map(|pair| pair[1].to_string())
        .collect()
}

#[test]
fn the_driver_reads_no_store_the_bot_refuses_as_an_earlier_layout() {
    for entry in PRIOR_STORE_LAYOUT {
        assert!(
            !DRIVER.contains(&format!("\"{entry}\"")),
            "the full-host driver reads `{entry}`, which the bot refuses as a store from an \
             earlier data layout; read that data where the bot's backend now keeps it"
        );
    }
}

#[tokio::test]
async fn every_table_the_driver_reads_is_one_the_bot_creates() {
    let data_dir = tempfile::tempdir().expect("bot data dir");
    let _backend = open_backend(data_dir.path())
        .await
        .expect("open the bot's backend");
    let catalog = data_dir.path().join(SESSIONS_ROOT).join(SESSION_CATALOG);
    assert!(
        catalog.is_file(),
        "the backend keeps no session catalog at {}",
        catalog.display()
    );
    for component in [SESSIONS_ROOT, SESSION_CATALOG] {
        assert!(
            DRIVER.contains(&format!("\"{component}\"")),
            "the full-host driver does not name the session catalog path component `{component}`"
        );
    }

    let mut created = tables(&rusqlite::Connection::open(&catalog).expect("open session catalog"));
    // The attach checkpoint reads the badge's bytes out of this table by the
    // reference's id; it has to be the backend's, not one the test supplies.
    assert!(
        created.contains("attachment_blobs"),
        "the backend's session catalog keeps no attachment_blobs table"
    );
    created.extend(schema_tables(crate::platform::db::SCHEMA));
    created.extend(schema_tables(crate::bot::ledger::SCHEMA));

    let read = driver_tables();
    assert!(
        read.contains("attachment_blobs"),
        "the driver no longer reads attachment bytes"
    );
    let missing: Vec<_> = read.difference(&created).collect();
    assert!(
        missing.is_empty(),
        "the full-host driver reads tables no store of the bot creates: {missing:?}"
    );
}
