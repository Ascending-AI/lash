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
    request_roots,
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

        /// Install the edges of newly stored immutable index nodes only.
        insert_index_edges = "INSERT INTO prompt_text_edges (parent, child)
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]')
               FROM json_each(?1)";

        /// Outgoing edges of one reclamation level, including every parent
        /// in the JSON array `?1` in the same read.
        read_index_edges = "SELECT parent, child FROM prompt_text_edges
             WHERE parent IN (SELECT value FROM json_each(?1)) ORDER BY parent, child";

        /// Stored texts in caller order, including repeated addresses and
        /// omitting missing ones, just like individual lookups.
        read_texts = "SELECT wanted.key, stored.hash, stored.text, edge.child
             FROM json_each(?1) AS wanted JOIN prompt_texts AS stored ON stored.hash = wanted.value
             LEFT JOIN prompt_text_edges AS edge ON edge.parent = stored.hash
             ORDER BY wanted.key, edge.child";

        /// Whether calls eligible for settlement hold a snapshot.
        settling_calls = "SELECT snapshot IS NOT NULL FROM prompt_snapshots
             WHERE session_id = ?1 AND owner = ?2 AND request IS NOT NULL
               AND (?3 IS NULL OR call <> ?3)";

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

        /// The reusable prefix currently owned by this session.
        read_prefix = "SELECT hash FROM prompt_prefixes WHERE session_id = ?1";

        /// Candidates whose last root or immutable parent was released.
        unreferenced_texts = "SELECT stored.hash FROM prompt_texts AS stored
             WHERE stored.hash IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = stored.hash
               )";

        /// Reclaim every text in the JSON array `?1` no root references any
        /// more.
        reclaim_texts = "DELETE FROM prompt_texts AS stored
             WHERE stored.hash IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = stored.hash
               ) RETURNING hash";
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

CREATE TABLE IF NOT EXISTS prompt_text_edges (
    parent TEXT NOT NULL REFERENCES prompt_texts(hash) ON DELETE CASCADE,
    child TEXT NOT NULL REFERENCES prompt_texts(hash),
    PRIMARY KEY (parent, child)
);
CREATE INDEX IF NOT EXISTS idx_prompt_text_edges_child ON prompt_text_edges(child);
CREATE TABLE IF NOT EXISTS prompt_prefixes (
    session_id TEXT PRIMARY KEY,
    hash TEXT NOT NULL REFERENCES prompt_texts(hash)
);
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
            prefix,
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
            let distinct: BTreeMap<_, _> = snapshot_texts
                .iter()
                .chain(request_texts)
                .map(|text| (text.hash.as_str(), text))
                .collect();
            let mut frontier: BTreeSet<&str> = snapshot_texts
                .iter()
                .map(|text| text.hash.as_str())
                .chain(request_roots(request_texts))
                .chain(prefix.as_deref())
                .collect();
            let mut unheld = BTreeMap::new();
            // Probe only new index paths: an existing immutable node owns
            // its complete closure, so no child metadata is resubmitted.
            while !frontier.is_empty() {
                let held = hash_set(tx, SQLITE_SQL.held_texts.sql(), [encode(&frontier)?])?;
                let mut next = BTreeSet::new();
                for hash in frontier {
                    if held.contains(hash) || unheld.contains_key(hash) {
                        continue;
                    }
                    let Some(text) = distinct.get(hash) else {
                        return Ok(Err(DurableError::Domain(
                            DomainRefusal::PromptFragmentMissing {
                                hash: hash.to_owned(),
                            },
                        )));
                    };
                    next.extend(text.children.iter().map(String::as_str));
                    unheld.insert(hash, text);
                }
                frontier = next;
            }
            if !unheld.is_empty() {
                let bodies: Vec<_> = unheld
                    .iter()
                    .map(|(hash, text)| (*hash, text.text.as_str()))
                    .collect();
                hash_set(tx, SQLITE_SQL.insert_texts.sql(), [encode(&bodies)?])?;
                let paths: BTreeSet<_> = unheld
                    .iter()
                    .flat_map(|(hash, text)| {
                        text.children
                            .iter()
                            .map(move |child| (*hash, child.as_str()))
                    })
                    .collect();
                if !paths.is_empty() {
                    cached_execute(tx, SQLITE_SQL.insert_index_edges.sql(), [encode(&paths)?])?;
                }
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
            if matches!(call.call, ModelCallId::Turn { .. }) {
                let old = if let Some(prefix) = prefix {
                    let old = hash_set(tx, SQLITE_SQL.read_prefix.sql(), [session.as_str()])?;
                    if !old.contains(prefix) {
                        cached_execute(
                            tx,
                            SQL.set_prefix.sql(),
                            rusqlite::params![session, prefix],
                        )?;
                    }
                    old
                } else {
                    hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?
                };
                reclaim(tx, &old)?;
            }
            Ok(Ok(()))
        }
        PromptWrite::ResetPrefix { session } => {
            let old = hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?;
            reclaim(tx, &old)?;
            Ok(Ok(()))
        }
        PromptWrite::Settle { session, run, open } => {
            let owner = ModelCallId::turn_owner(run);
            let open = open.map(|call| call.to_string());
            let params = rusqlite::params![session.as_str(), owner, open];
            let mut statement = tx.prepare_cached(SQLITE_SQL.settling_calls.sql())?;
            let snapshots = statement
                .query_map(params, |row| row.get::<_, bool>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if snapshots.is_empty() {
                return Ok(Ok(()));
            }
            let released = hash_set(tx, SQLITE_SQL.settle_edges.sql(), params)?;
            if snapshots.contains(&false) {
                cached_execute(tx, SQLITE_SQL.settle_bare_roots.sql(), params)?;
            }
            if snapshots.contains(&true) {
                cached_execute(tx, SQLITE_SQL.settle_requests.sql(), params)?;
            }
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
    let mut released = match run {
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
    if run.is_none() {
        released.extend(hash_set(tx, SQL.release_prefix.sql(), [session.as_str()])?);
    }
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
        .chain(
            request_roots(request_texts)
                .into_iter()
                .map(|hash| (REQUEST_PART, hash)),
        )
        .collect()
}

/// Reclaim every text among `released` no remaining edge references.
fn reclaim(tx: &Connection, released: &BTreeSet<String>) -> rusqlite::Result<()> {
    let mut frontier = released.clone();
    while !frontier.is_empty() {
        // Admission can leave the old root held by an open call, and
        // settlement can release the current session prefix. Neither
        // needs a write or a traversal of its retained closure.
        frontier = hash_set(
            tx,
            SQLITE_SQL.unreferenced_texts.sql(),
            [encode(&frontier)?],
        )?;
        if frontier.is_empty() {
            break;
        }
        let list = encode(&frontier)?;
        let mut children = index_edges(tx, &list)?;
        let removed = hash_set(tx, SQLITE_SQL.reclaim_texts.sql(), [&list])?;
        frontier = removed
            .into_iter()
            .filter_map(|hash| children.remove(&hash))
            .flatten()
            .collect();
    }
    Ok(())
}

/// Read one batch's outgoing index edges in the existing child order.
fn index_edges(
    tx: &Connection,
    list: &str,
) -> rusqlite::Result<BTreeMap<String, BTreeSet<String>>> {
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut statement = tx.prepare_cached(SQLITE_SQL.read_index_edges.sql())?;
    let edges = statement.query_map([list], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for edge in edges {
        let (parent, child) = edge?;
        children.entry(parent).or_default().insert(child);
    }
    Ok(children)
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
    if hashes.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let list = encode(&hashes)?;
    let mut statement = tx.prepare_cached(SQLITE_SQL.read_texts.sql())?;
    let found = statement.query_map([&list], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut texts: Vec<PromptText> = Vec::new();
    let mut previous = None;
    for row in found {
        let (index, hash, text, child) = row?;
        if previous != Some(index) {
            texts.push(PromptText {
                hash,
                text,
                children: Vec::new(),
            });
            previous = Some(index);
        }
        if let (Some(child), Some(text)) = (child, texts.last_mut()) {
            text.children.push(child);
        }
    }
    Ok(Ok(texts))
}
