//! The full-host E2E driver reads the bot's storage where the bot writes it.
//!
//! `scripts/slack-clone-full-host-e2e.py` asserts over the stores themselves —
//! the platform database, the bot's event ledger and the SQLite store set's
//! session catalog — rather than over an API, so it carries its own picture of
//! the bot's data layout. It runs only on the manual full-profile dispatch, so
//! a layout that moves under it would otherwise surface there, after the move
//! has merged. These checks run with the crate's unit tests on every PR that
//! changes the bot or its backend.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::bot::runtime::{PRIOR_STORE_LAYOUT, SESSIONS_ROOT, open_stores};

/// The driver as checked in, read as data.
const DRIVER: &str = include_str!("../../../../scripts/slack-clone-full-host-e2e.py");

/// The session catalog the SQLite store set keeps under its sessions root.
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

fn driver_snapshot(catalog: &Path) -> serde_json::Value {
    // Compile the actual Journey class without its optional browser imports.
    let script = r#"
from __future__ import annotations
import ast
import json
from pathlib import Path
import sqlite3
import sys

module = ast.parse(sys.stdin.read())
journey = next(node for node in module.body if isinstance(node, ast.ClassDef) and node.name == "Journey")
module.body = [journey]
exec(compile(module, "full-host-driver", "exec"))
snapshot = Journey.__new__(Journey)
snapshot.session_db = Path(sys.argv[1])
print(json.dumps(snapshot.session_snapshot()))
"#;
    let mut child = Command::new("python3")
        .args(["-c", script])
        .arg(catalog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the session snapshot driver");
    child
        .stdin
        .take()
        .expect("driver stdin")
        .write_all(DRIVER.as_bytes())
        .expect("send the checked-in driver");
    let output = child.wait_with_output().expect("wait for snapshot");
    assert!(
        output.status.success(),
        "the full-host session snapshot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("session snapshot JSON")
}

#[test]
fn the_driver_expects_the_mcp_names_published_by_the_bot_servers() {
    for (server, tools) in [
        (
            "slack_clone",
            &[
                "sample_summary",
                "elicit_confirmation",
                "elicit_via_url",
                "list_host_roots",
            ][..],
        ),
        ("workspace_http", &["workspace_badge"][..]),
    ] {
        for name in lash_plugin_mcp::mcp_tool_names(server, tools).values() {
            assert!(
                DRIVER.contains(&format!("\"{name}\"")),
                "the full-host driver does not expect the published MCP tool name `{name}`"
            );
        }
    }
}

#[tokio::test]
async fn the_driver_reads_usage_with_separate_model_identities_from_the_bot_store() {
    let data_dir = tempfile::tempdir().expect("bot data dir");
    let _stores = open_stores(data_dir.path())
        .await
        .expect("open the bot's store set");
    let catalog = data_dir.path().join(SESSIONS_ROOT).join(SESSION_CATALOG);
    assert_eq!(driver_snapshot(&catalog)["usage"], serde_json::json!([]));

    let connection = rusqlite::Connection::open(&catalog).expect("open session catalog");
    for (kind, key, served) in [
        ("session", "key-a", Some("served-wire")),
        ("session", "key-b", None),
        ("process", "process-key", Some("process-wire")),
    ] {
        connection
            .execute(
                "INSERT INTO usage_facts (owner_kind, owner_id, effect_key, call_ordinal, \
                 provider_attempt, fact_kind, disposition, run_id, llm_call_id, source, \
                 profile_key, requested_model, served_model, input_tokens, output_tokens, \
                 cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, \
                 payload_hash, recorded_at_ms) VALUES (?1, 'owner', ?2, 0, 0, 'attempt', \
                 'reported', 'run', 'call', 'turn', ?2, 'requested-wire', ?3, 7, 3, 0, 0, 0, \
                 'payload', 1)",
                rusqlite::params![kind, key, served],
            )
            .expect("record usage fixture");
    }

    assert_eq!(
        driver_snapshot(&catalog)["usage"],
        serde_json::json!([
            {"owner_id": "owner", "profile_key": "key-a", "requested_model": "requested-wire",
             "served_model": "served-wire", "input_tokens": 7, "output_tokens": 3},
            {"owner_id": "owner", "profile_key": "key-b", "requested_model": "requested-wire",
             "served_model": null, "input_tokens": 7, "output_tokens": 3},
        ])
    );
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
    let _stores = open_stores(data_dir.path())
        .await
        .expect("open the bot's store set");
    let catalog = data_dir.path().join(SESSIONS_ROOT).join(SESSION_CATALOG);
    assert!(
        catalog.is_file(),
        "the store set keeps no session catalog at {}",
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
    // reference's id; it has to be the store set's, not one the test supplies.
    assert!(
        created.contains("attachment_blobs"),
        "the store set's session catalog keeps no attachment_blobs table"
    );
    created.extend(schema_tables(crate::platform::db::SCHEMA));
    created.extend(schema_tables(&crate::bot::ledger::SCHEMA));

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
