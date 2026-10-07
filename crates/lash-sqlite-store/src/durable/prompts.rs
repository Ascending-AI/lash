//! Prompt snapshots on SQLite: the `prompts` domain's statements, apply and
//! read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::durable::prompts::PromptStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing};
use crate::conn::cached_execute;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(crate::schema_layout::MAIN));

/// `prompt_snapshots`: P2's (FIG-5256) audit roots, one per admitted model
/// call; `prompt_texts`: section text stored once by content address;
/// `prompt_snapshot_texts`: each root's edge to every text it references.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    call_ordinal INTEGER NOT NULL CONSTRAINT ck_prompt_snapshots_call
        CHECK (call_ordinal BETWEEN 0 AND 4294967295),
    snapshot TEXT NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, run, call_ordinal)
);

CREATE TABLE IF NOT EXISTS prompt_texts (
    hash TEXT PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    call_ordinal INTEGER NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (session_id, run, call_ordinal, hash),
    FOREIGN KEY (session_id, run, call_ordinal)
        REFERENCES prompt_snapshots (session_id, run, call_ordinal),
    FOREIGN KEY (hash) REFERENCES prompt_texts (hash)
);

CREATE INDEX IF NOT EXISTS idx_prompt_snapshot_texts_hash ON prompt_snapshot_texts (hash);
";

fn call_params(call: &PromptCallKey) -> (String, String, i64) {
    (
        call.session.as_str().to_owned(),
        call.run.as_str().to_owned(),
        i64::from(call.call),
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
            let mut rooted = BTreeSet::new();
            for text in texts {
                if !rooted.insert(text.hash.as_str()) {
                    continue;
                }
                cached_execute(
                    tx,
                    SQL.insert_text.sql(),
                    rusqlite::params![text.hash, text.text],
                )?;
                cached_execute(
                    tx,
                    SQL.insert_edge.sql(),
                    rusqlite::params![session, run, ordinal, text.hash],
                )?;
            }
            Ok(Ok(()))
        }
        PromptWrite::Release { session, run } => {
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
                        rusqlite::params![session.as_str(), run.as_str()],
                    )?;
                    cached_execute(
                        tx,
                        SQL.release_run_snapshots.sql(),
                        rusqlite::params![session.as_str(), run.as_str()],
                    )?;
                    hashes
                }
            };
            for hash in released {
                cached_execute(tx, SQL.reclaim_text.sql(), [hash])?;
            }
            Ok(Ok(()))
        }
    }
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
