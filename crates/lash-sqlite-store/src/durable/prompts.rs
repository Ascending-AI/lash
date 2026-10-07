//! Prompt snapshots on SQLite: the `prompts` domain's statements, apply and
//! read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272).
//!
//! SQLite's one writer serializes a record against a release, so a text a
//! record names is never reclaimed under it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::durable::prompts::PromptStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing};
use crate::conn::cached_execute;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    /// The `prompt_texts` and `prompt_snapshot_texts` statements only SQLite
    /// issues: `json_each` is how it binds a call's texts as one list.
    struct SqlitePromptStatements @ "durable_prompt_sqlite" {
        /// Store each `[hash, text]` pair of the JSON array `?1`, skipping
        /// every hash already stored: the address names the bytes, so a
        /// conflict is the same text.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]')
               FROM json_each(?1)
              WHERE true
             ON CONFLICT (hash) DO NOTHING";

        /// Root every text in the JSON array `?4` under call `?3` of owner
        /// `?2` in session `?1`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, hash)
             SELECT ?1, ?2, ?3, value FROM json_each(?4)";

        /// Reclaim every text in the JSON array `?1` no root references any
        /// more.
        reclaim_texts = "DELETE FROM prompt_texts AS stored
             WHERE stored.hash IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               )";
    }
}

static SQLITE_SQL: LazyLock<SqlitePromptStatements> =
    LazyLock::new(|| SqlitePromptStatements::render(crate::schema_layout::MAIN));

/// `prompt_snapshots`: P2's (FIG-5256) audit roots, one per admitted model
/// call, keyed by its owner (`turn:<run>` or `owned:<scope>`) and its call
/// there (FIG-5259); `prompt_texts`: section text stored once by content address;
/// `prompt_snapshot_texts`: each root's edge to every text it references.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, owner, call)
);

CREATE TABLE IF NOT EXISTS prompt_texts (
    hash TEXT PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (session_id, owner, call, hash),
    FOREIGN KEY (session_id, owner, call)
        REFERENCES prompt_snapshots (session_id, owner, call),
    FOREIGN KEY (hash) REFERENCES prompt_texts (hash)
);

CREATE INDEX IF NOT EXISTS idx_prompt_snapshot_texts_hash ON prompt_snapshot_texts (hash);
";

fn call_params(call: &PromptCallKey) -> (String, String, String) {
    (
        call.session.as_str().to_owned(),
        call.call.owner_column(),
        call.call.call_column(),
    )
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &PromptWrite) -> Answer<()> {
    match write {
        PromptWrite::Record {
            call,
            snapshot,
            texts,
        } => {
            let (session, run, ordinal) = call_params(call);
            let recorded = tx
                .prepare_cached(SQL.insert_snapshot.sql())?
                .query_row(
                    rusqlite::params![session, run, ordinal, snapshot, commit.epoch.0],
                    |_| Ok(()),
                )
                .optional()?;
            if recorded.is_none() {
                return Ok(Err(DurableError::Domain(
                    DomainRefusal::PromptCallRecorded { call: call.clone() },
                )));
            }
            // The first text under each address stands for it.
            let mut distinct = BTreeMap::new();
            for text in texts {
                distinct
                    .entry(text.hash.as_str())
                    .or_insert(text.text.as_str());
            }
            if distinct.is_empty() {
                return Ok(Ok(()));
            }
            let pairs = encode(&distinct.iter().collect::<Vec<_>>())?;
            let hashes = encode(&distinct.keys().collect::<Vec<_>>())?;
            cached_execute(tx, SQLITE_SQL.insert_texts.sql(), [pairs])?;
            cached_execute(
                tx,
                SQLITE_SQL.insert_edges.sql(),
                rusqlite::params![session, run, ordinal, hashes],
            )?;
            Ok(Ok(()))
        }
        PromptWrite::Release { session, run } => {
            release(tx, session, run.as_ref())?;
            Ok(Ok(()))
        }
    }
}

/// Release every snapshot root of `session`, or of its turn `run` alone, and
/// reclaim every text no remaining root references.
///
/// # Errors
///
/// The first statement SQLite refused.
pub(crate) fn release(
    tx: &Connection,
    session: &SessionId,
    run: Option<&TurnId>,
) -> rusqlite::Result<()> {
    let released = match run {
        None => {
            let hashes = released_hashes(
                tx,
                SQL.release_session_edges.sql(),
                rusqlite::params![session.as_str()],
            )?;
            cached_execute(tx, SQL.release_session_snapshots.sql(), [session.as_str()])?;
            hashes
        }
        Some(run) => {
            let hashes = released_hashes(
                tx,
                SQL.release_run_edges.sql(),
                rusqlite::params![session.as_str(), ModelCallId::turn_owner(run)],
            )?;
            cached_execute(
                tx,
                SQL.release_run_snapshots.sql(),
                rusqlite::params![session.as_str(), ModelCallId::turn_owner(run)],
            )?;
            hashes
        }
    };
    if !released.is_empty() {
        cached_execute(tx, SQLITE_SQL.reclaim_texts.sql(), [encode(&released)?])?;
    }
    Ok(())
}

/// A statement's JSON list argument.
fn encode(value: &impl serde::Serialize) -> rusqlite::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
}

/// Run a release of edges, collecting each distinct text they named.
fn released_hashes(
    tx: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = tx.prepare_cached(sql)?;
    let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
    rows.collect()
}

pub(super) fn snapshot(tx: &Connection, call: &PromptCallKey) -> Answer<Option<PromptSnapshotRow>> {
    let (session, run, ordinal) = call_params(call);
    let Some((snapshot, written_epoch)) = tx
        .prepare_cached(SQL.read_snapshot.sql())?
        .query_row(rusqlite::params![session, run, ordinal], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?
    else {
        return Ok(Ok(None));
    };
    let texts = tx
        .prepare_cached(SQL.read_edges.sql())?
        .query_map(rusqlite::params![session, run, ordinal], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot,
        texts,
        written_epoch: Epoch(written_epoch),
    })))
}

pub(super) fn texts(tx: &Connection, hashes: &[String]) -> Answer<Vec<PromptText>> {
    let mut statement = tx.prepare_cached(SQL.read_text.sql())?;
    let mut found = Vec::with_capacity(hashes.len());
    for hash in hashes {
        if let Some(text) = statement
            .query_row([hash], |row| row.get::<_, String>(0))
            .optional()?
        {
            found.push(PromptText {
                hash: hash.clone(),
                text,
            });
        }
    }
    Ok(Ok(found))
}
