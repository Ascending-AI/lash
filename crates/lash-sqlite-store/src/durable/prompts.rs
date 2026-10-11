//! Prompt snapshots on SQLite: the `prompts` domain's statements, apply and
//! read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272).
//!
//! SQLite's one writer serializes a record against a release, so a text a
//! record finds stored is never reclaimed under it, and a record binds the
//! bodies of only the texts it does not find (FIG-5946).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART, SNAPSHOT_PART};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing};
use crate::conn::cached_execute;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    /// The `prompt_texts` and `prompt_snapshot_texts` statements only SQLite
    /// issues: `json_each` is how it binds a call's texts as one list.
    struct SqlitePromptStatements @ "durable_prompt_sqlite" {
        /// Every content address in the JSON array `?1` already stored.
        held_texts = "SELECT hash FROM prompt_texts
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// Store each `[hash, text]` pair of the JSON array `?1`, returning
        /// each address stored. The caller binds only addresses it found
        /// unstored under the one writer, so every pair is a new row.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]')
               FROM json_each(?1)
             RETURNING hash";

        /// Root each text of the `[part, hash]` pairs of the JSON array `?4`
        /// under that part of call `?3` of owner `?2` in session `?1`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, json_extract(value, '$[0]'), json_extract(value, '$[1]')
               FROM json_each(?4)";

        /// Release the request edges of every call of owner `?2` in session
        /// `?1` but call `?3` (none when `?3` is null), returning each text
        /// they named.
        settle_edges = "DELETE FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2 AND part = 'request'
               AND (?3 IS NULL OR call <> ?3)
             RETURNING hash";

        /// Release the root of every such call that holds no snapshot: its
        /// request was all it held.
        settle_bare_roots = "DELETE FROM prompt_snapshots
             WHERE session_id = ?1 AND owner = ?2 AND snapshot IS NULL
               AND (?3 IS NULL OR call <> ?3)";

        /// Release the request of every other such call, keeping its
        /// snapshot.
        settle_requests = "UPDATE prompt_snapshots SET request = NULL
             WHERE session_id = ?1 AND owner = ?2 AND request IS NOT NULL
               AND (?3 IS NULL OR call <> ?3)";

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

/// `prompt_snapshots`: P2's (FIG-5256) roots, one per admitted model call,
/// keyed by its owner (`turn:<run>` or `owned:<scope>`) and its call there
/// (FIG-5259), holding its snapshot and, until the call settles, its request
/// (FIG-5946); `prompt_texts`: section text and body chunks stored once by
/// content address; `prompt_snapshot_texts`: each root part's edge to every
/// text it references.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    snapshot TEXT,
    request TEXT,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, owner, call),
    CONSTRAINT ck_prompt_snapshots_held CHECK (snapshot IS NOT NULL OR request IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS prompt_texts (
    hash TEXT PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    part TEXT NOT NULL CONSTRAINT ck_prompt_snapshot_texts_part
        CHECK (part IN ('snapshot', 'request')),
    hash TEXT NOT NULL,
    PRIMARY KEY (session_id, owner, call, part, hash),
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
            request,
            snapshot_texts,
            request_texts,
        } => {
            let (session, owner, ordinal) = call_params(call);
            let recorded = tx
                .prepare_cached(SQL.insert_snapshot.sql())?
                .query_row(
                    rusqlite::params![session, owner, ordinal, snapshot, request, commit.epoch.0],
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
            for text in snapshot_texts.iter().chain(request_texts) {
                distinct
                    .entry(text.hash.as_str())
                    .or_insert(text.text.as_str());
            }
            if distinct.is_empty() {
                return Ok(Ok(()));
            }
            // Only the bodies the store does not hold are bound.
            let held = hash_set(
                tx,
                SQLITE_SQL.held_texts.sql(),
                [encode(&distinct.keys().collect::<Vec<_>>())?],
            )?;
            let unheld: Vec<_> = distinct
                .iter()
                .filter(|(hash, _)| !held.contains(**hash))
                .collect();
            if !unheld.is_empty() {
                let stored = hash_set(tx, SQLITE_SQL.insert_texts.sql(), [encode(&unheld)?])?;
                debug_assert_eq!(stored.len(), unheld.len());
            }
            cached_execute(
                tx,
                SQLITE_SQL.insert_edges.sql(),
                rusqlite::params![
                    session,
                    owner,
                    ordinal,
                    encode(&edges(snapshot_texts, request_texts))?
                ],
            )?;
            Ok(Ok(()))
        }
        PromptWrite::Settle { session, run, open } => {
            let owner = ModelCallId::turn_owner(run);
            let open = open.map(|call| call.to_string());
            let params = rusqlite::params![session.as_str(), owner, open];
            let released = hash_set(tx, SQLITE_SQL.settle_edges.sql(), params)?;
            cached_execute(tx, SQLITE_SQL.settle_bare_roots.sql(), params)?;
            cached_execute(tx, SQLITE_SQL.settle_requests.sql(), params)?;
            reclaim(tx, &released)?;
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
            let hashes = hash_set(
                tx,
                SQL.release_session_edges.sql(),
                rusqlite::params![session.as_str()],
            )?;
            cached_execute(tx, SQL.release_session_snapshots.sql(), [session.as_str()])?;
            hashes
        }
        Some(run) => {
            let hashes = hash_set(
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
    reclaim(tx, &released)
}

/// Each distinct `(part, hash)` edge of a root over `snapshot_texts` and
/// `request_texts`.
fn edges<'a>(
    snapshot_texts: &'a [PromptText],
    request_texts: &'a [PromptText],
) -> BTreeSet<(&'static str, &'a str)> {
    let part =
        |part, texts: &'a [PromptText]| texts.iter().map(move |text| (part, text.hash.as_str()));
    part(SNAPSHOT_PART, snapshot_texts)
        .chain(part(REQUEST_PART, request_texts))
        .collect()
}

/// Reclaim every text among `released` no remaining edge references.
fn reclaim(tx: &Connection, released: &BTreeSet<String>) -> rusqlite::Result<()> {
    if !released.is_empty() {
        cached_execute(tx, SQLITE_SQL.reclaim_texts.sql(), [encode(released)?])?;
    }
    Ok(())
}

/// A statement's JSON list argument.
fn encode(value: &impl serde::Serialize) -> rusqlite::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
}

/// Run a statement returning content addresses, collecting each distinct
/// one.
fn hash_set(
    tx: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = tx.prepare_cached(sql)?;
    let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
    rows.collect()
}

pub(super) fn snapshot(tx: &Connection, call: &PromptCallKey) -> Answer<Option<PromptSnapshotRow>> {
    let (session, owner, ordinal) = call_params(call);
    let Some((snapshot, request, written_epoch)) = tx
        .prepare_cached(SQL.read_snapshot.sql())?
        .query_row(rusqlite::params![session, owner, ordinal], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .optional()?
    else {
        return Ok(Ok(None));
    };
    let mut snapshot_texts = Vec::new();
    let mut request_texts = Vec::new();
    let mut statement = tx.prepare_cached(SQL.read_edges.sql())?;
    let edges = statement.query_map(rusqlite::params![session, owner, ordinal], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for edge in edges {
        let (part, hash) = edge?;
        if part == REQUEST_PART {
            request_texts.push(hash);
        } else {
            snapshot_texts.push(hash);
        }
    }
    Ok(Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot,
        request,
        snapshot_texts,
        request_texts,
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
