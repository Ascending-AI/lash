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
    RequestTexts,
};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART, SNAPSHOT_PART};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
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
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

        /// Store each `[hash, text]` leaf and `[hash, text, children]`
        /// index node of the JSON array `?1`, returning each address
        /// stored. The caller binds only addresses it found unstored under
        /// the one writer, so every element is a new row.
        insert_texts = "INSERT INTO prompt_texts (hash, text, children)
             SELECT unhex(json_extract(value, '$[0]')), json_extract(value, '$[1]'),
                    json_extract(value, '$[2]')
               FROM json_each(?1)
             RETURNING hash";

        /// Root each text of the `[part, hash]` pairs of the JSON array `?4`
        /// under that part of call `?3` of owner `?2` in session `?1`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, json_extract(value, '$[0]'), unhex(json_extract(value, '$[1]'))
               FROM json_each(?4)";

        /// Install the edges of newly stored immutable index nodes only.
        insert_index_edges = "INSERT INTO prompt_text_edges (parent, child)
             SELECT unhex(json_extract(value, '$[0]')), unhex(json_extract(value, '$[1]'))
               FROM json_each(?1)";

        /// Delete the edges of JSON `?1`, `[child, parent]` pairs: the
        /// edges of reclaimed parents.
        delete_index_edges = "DELETE FROM prompt_text_edges
             WHERE (child, parent) IN (SELECT unhex(value ->> 0), unhex(value ->> 1) FROM json_each(?1))";

        /// Stored texts in caller order, including repeated addresses and
        /// omitting missing ones, just like individual lookups.
        read_texts = "SELECT stored.hash, stored.text, stored.children
             FROM json_each(?1) AS wanted JOIN prompt_texts AS stored ON stored.hash = unhex(wanted.value)
             ORDER BY wanted.key";

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
             WHERE session_id = ?1 AND owner = ?2 AND snapshot IS NULL AND request IS NOT NULL
               AND (?3 IS NULL OR call <> ?3)";

        /// Release the request of every other such call, keeping its
        /// snapshot.
        settle_requests = "UPDATE prompt_snapshots SET request = NULL
             WHERE session_id = ?1 AND owner = ?2 AND request IS NOT NULL
               AND (?3 IS NULL OR call <> ?3)";

        /// The reusable prefix currently owned by this session.
        read_prefix = "SELECT hash FROM prompt_prefixes WHERE session_id = ?1";

        /// Persist cleanup before releasing the transaction's root ownership.
        retirement_put = "INSERT INTO prompt_retirement_frontier(hash)
             SELECT unhex(value) FROM json_each(?1) WHERE true ON CONFLICT DO NOTHING";

        /// One bounded candidate set. Shared/live boundaries stop discovery;
        /// outside incoming edges seed protection, propagated through the DAG.
        /// A seed with any incoming edge is a boundary: its parent may be
        /// live outside this batch. If that parent is also deleted, its edge
        /// queues this child again, so deferring discovery loses no cleanup.
        /// Casting the integer limit keeps rebinding from expiring the cached
        /// recursive program for compile-time limit specialization.
        retirement_candidates = "WITH RECURSIVE candidates AS (
             SELECT hash FROM (SELECT hash FROM prompt_retirement_frontier ORDER BY hash LIMIT CAST(?1 AS INTEGER))
             UNION
             SELECT edge.child FROM candidates
               JOIN prompt_text_edges AS edge ON edge.parent = candidates.hash
              WHERE NOT EXISTS (SELECT 1 FROM prompt_snapshot_texts WHERE hash = candidates.hash)
                AND NOT EXISTS (SELECT 1 FROM prompt_prefixes WHERE hash = candidates.hash)
                AND (NOT EXISTS (SELECT 1 FROM prompt_retirement_frontier WHERE hash = candidates.hash)
                     OR NOT EXISTS (SELECT 1 FROM prompt_text_edges WHERE child = candidates.hash))
                AND (SELECT count(*) FROM (SELECT 1 FROM prompt_text_edges WHERE child = candidates.hash LIMIT 2)) <= 1
             LIMIT CAST(?1 AS INTEGER)
         ), protected AS (
             SELECT hash FROM candidates
              WHERE EXISTS (SELECT 1 FROM prompt_snapshot_texts WHERE hash = candidates.hash)
                 OR EXISTS (SELECT 1 FROM prompt_prefixes WHERE hash = candidates.hash)
                 OR EXISTS (SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = candidates.hash
                     AND edge.parent NOT IN (SELECT hash FROM candidates))
             UNION
             SELECT edge.child FROM protected JOIN prompt_text_edges AS edge ON edge.parent = protected.hash
              WHERE edge.child IN (SELECT hash FROM candidates)
         ) SELECT candidates.hash, stored.children,
                  candidates.hash NOT IN (SELECT hash FROM protected), length(CAST(stored.text AS BLOB))
             FROM candidates LEFT JOIN prompt_texts AS stored ON stored.hash = candidates.hash";

        /// Remove processed queue positions, including candidates still held.
        retirement_take = "DELETE FROM prompt_retirement_frontier WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

        /// Delete outgoing edges of the transaction's proven unprotected set.
        delete_retired_edges = "DELETE FROM prompt_text_edges WHERE parent IN (SELECT unhex(value) FROM json_each(?1))";

        /// The same synchronization owner proved this exact set unprotected.
        delete_retired_texts = "DELETE FROM prompt_texts WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";


    }
}

static SQLITE_SQL: LazyLock<SqlitePromptStatements> =
    LazyLock::new(|| SqlitePromptStatements::render(crate::schema_layout::MAIN));

#[cfg(test)]
pub(super) fn retirement_sql_for_testing() -> &'static str {
    SQLITE_SQL.retirement_candidates.sql()
}

/// `prompt_snapshots`: P2's (FIG-5256) roots, one per admitted model call,
/// keyed by its owner (`turn:<run>` or `owned:<scope>`) and its call there
/// (FIG-5259), holding its snapshot and, until the call settles, its request
/// (FIG-5946); `prompt_texts`: section text and body chunks stored once by
/// content address, an index node with its children; `prompt_snapshot_texts`:
/// each root part's edge to every text it references; `prompt_text_edges`:
/// each index node's children keyed by the child, so a text a parent still
/// names is never reclaimed (FIG-6939).
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    snapshot TEXT,
    request TEXT,
    written_epoch INTEGER NOT NULL,
    -- Both parts NULL is a compact expired-call identity.
    PRIMARY KEY (session_id, owner, call)
);

-- Reclaimed call identities stay out of the release scan.
CREATE INDEX IF NOT EXISTS idx_prompt_snapshots_settled
    ON prompt_snapshots (session_id, owner, call)
    WHERE request IS NULL AND snapshot IS NOT NULL;

CREATE TABLE IF NOT EXISTS prompt_texts (
    hash BLOB PRIMARY KEY,
    text TEXT NOT NULL,
    -- An index node's children, a JSON array in address order; NULL for a
    -- leaf. The forward direction of its edges.
    children TEXT
);

CREATE TABLE IF NOT EXISTS prompt_snapshot_texts (
    session_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    call TEXT NOT NULL,
    part TEXT NOT NULL CONSTRAINT ck_prompt_snapshot_texts_part
        CHECK (part IN ('snapshot', 'request')),
    hash BLOB NOT NULL,
    PRIMARY KEY (session_id, owner, call, part, hash),
    FOREIGN KEY (session_id, owner, call)
        REFERENCES prompt_snapshots (session_id, owner, call),
    FOREIGN KEY (hash) REFERENCES prompt_texts (hash)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_prompt_snapshot_texts_hash ON prompt_snapshot_texts (hash);

CREATE TABLE IF NOT EXISTS prompt_text_edges (
    parent BLOB NOT NULL,
    child BLOB NOT NULL REFERENCES prompt_texts(hash),
    PRIMARY KEY (child, parent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_prompt_text_edges_parent ON prompt_text_edges(parent);
CREATE TABLE IF NOT EXISTS prompt_retirement_frontier (hash BLOB PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS prompt_prefixes (
    session_id TEXT PRIMARY KEY,
    hash BLOB NOT NULL REFERENCES prompt_texts(hash)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_prompt_prefixes_hash ON prompt_prefixes(hash);
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
            let snapshot_by_hash: BTreeMap<_, _> = snapshot_texts
                .iter()
                .map(|text| (text.hash.as_str(), text))
                .collect();
            let text = |hash: &str| {
                request_texts
                    .get(hash)
                    .or_else(|| snapshot_by_hash.get(hash).copied())
            };
            let mut frontier: BTreeSet<&str> = snapshot_texts
                .iter()
                .map(|text| text.hash.as_str())
                .chain(request_texts.roots().iter().map(String::as_str))
                .chain(prefix.as_deref())
                .collect();
            let mut unheld = BTreeMap::new();
            if let Some(delta) = request_texts.supplied_delta() {
                // One set read validates the complete direct frontier. A
                // remembered digest never resurrects a missing old body.
                let supplied: BTreeMap<_, _> = delta
                    .map(|text| (text.hash.as_str(), text))
                    .chain(snapshot_by_hash.iter().map(|(hash, text)| (*hash, *text)))
                    .collect();
                frontier.extend(
                    supplied
                        .values()
                        .flat_map(|text| text.children.iter().map(String::as_str)),
                );
                frontier.extend(supplied.keys().copied());
                let held = hash_set(tx, SQLITE_SQL.held_texts.sql(), [encode(&frontier)?])?;
                for hash in frontier {
                    if held.contains(hash) {
                        continue;
                    }
                    let Some(text) = supplied.get(hash) else {
                        return Ok(Err(DurableError::Domain(
                            DomainRefusal::PromptFragmentMissing {
                                hash: hash.to_owned(),
                            },
                        )));
                    };
                    unheld.insert(hash, *text);
                }
            } else {
                // Probe only new index paths: an existing immutable node owns
                // its complete closure, so no child metadata is resubmitted.
                while !frontier.is_empty() {
                    let held = hash_set(tx, SQLITE_SQL.held_texts.sql(), [encode(&frontier)?])?;
                    let mut next = BTreeSet::new();
                    for hash in frontier {
                        if held.contains(hash) || unheld.contains_key(hash) {
                            continue;
                        }
                        let Some(text) = text(hash) else {
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
            }
            if !unheld.is_empty() {
                let bodies: Vec<_> = unheld
                    .iter()
                    .map(|(hash, text)| stored_text(hash, text))
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
                            rusqlite::params![session, crate::codec::SqlHash(prefix)],
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

/// Release one bounded page of settled audit roots. Call identities survive,
/// so a repeated admission remains refused after physical reclamation.
pub(crate) fn release_unretained(
    tx: &Connection,
    session: Option<&SessionId>,
) -> rusqlite::Result<usize> {
    let calls = {
        let mut statement = tx.prepare_cached(SQL.expired_calls.sql())?;
        let rows = statement.query_map(
            rusqlite::params![
                session.map(SessionId::as_str),
                i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut released = BTreeSet::new();
    for (session, owner, call) in &calls {
        released.extend(hash_set(
            tx,
            SQL.release_audit_edges.sql(),
            rusqlite::params![session, owner, call],
        )?);
        cached_execute(
            tx,
            SQL.expire_audit.sql(),
            rusqlite::params![session, owner, call],
        )?;
    }
    reclaim(tx, &released)?;
    Ok(calls.len())
}

/// Each distinct `(part, hash)` edge of a root over `snapshot_texts` and
/// `request_texts`.
fn edges<'a>(
    snapshot_texts: &'a [PromptText],
    request_texts: &'a RequestTexts,
) -> BTreeSet<(&'static str, &'a str)> {
    snapshot_texts
        .iter()
        .map(|text| (SNAPSHOT_PART, text.hash.as_str()))
        .chain(
            request_texts
                .roots()
                .iter()
                .map(|hash| (REQUEST_PART, hash.as_str())),
        )
        .collect()
}

/// A root handoff records cleanup atomically; one bounded set handles the
/// changed append path. Large resets leave their remaining work durably queued.
fn reclaim(tx: &Connection, released: &BTreeSet<String>) -> rusqlite::Result<()> {
    if released.is_empty() {
        return Ok(());
    }
    cached_execute(tx, SQLITE_SQL.retirement_put.sql(), [encode(released)?])?;
    drain_retired(tx, 4096).map(drop)
}

pub(crate) fn retirement_backlog(tx: &Connection) -> rusqlite::Result<u64> {
    tx.prepare_cached(SQL.retirement_count.sql())?
        .query_row([], |row| row.get(0))
}

/// Answers examined nodes, reclaimed nodes and reclaimed payload bytes.
/// Prompt bytes are separate from graph blob accounting.
pub(crate) fn drain_retired(
    tx: &Connection,
    limit: usize,
) -> rusqlite::Result<(usize, usize, u64)> {
    let candidates = tx
        .prepare_cached(SQLITE_SQL.retirement_candidates.sql())?
        .query_map([i64::try_from(limit.max(1)).unwrap_or(i64::MAX)], |row| {
            Ok((
                crate::codec::hash_column(row, 0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<u64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if candidates.is_empty() {
        return Ok((0, 0, 0));
    }
    let examined: BTreeSet<_> = candidates
        .iter()
        .map(|(hash, _, _, _)| hash.clone())
        .collect();
    let gone: BTreeSet<_> = candidates
        .iter()
        .filter(|(_, _, unprotected, bytes)| *unprotected && bytes.is_some())
        .map(|(hash, _, _, _)| hash.clone())
        .collect();
    let mut next = BTreeSet::new();
    let mut bytes = 0;
    for (hash, children, _, size) in &candidates {
        if gone.contains(hash) {
            next.extend(
                read_children(children.as_deref())?
                    .into_iter()
                    .filter(|child| !examined.contains(child)),
            );
            bytes += size.unwrap_or(0);
        }
    }
    cached_execute(tx, SQLITE_SQL.retirement_take.sql(), [encode(&examined)?])?;
    if !gone.is_empty() {
        let batch = encode(&gone)?;
        cached_execute(tx, SQLITE_SQL.delete_retired_edges.sql(), [&batch])?;
        cached_execute(tx, SQLITE_SQL.delete_retired_texts.sql(), [&batch])?;
    }
    if !next.is_empty() {
        cached_execute(tx, SQLITE_SQL.retirement_put.sql(), [encode(&next)?])?;
    }
    Ok((examined.len(), gone.len(), bytes))
}

/// Canonical rows for a set install; immutable child edges keep set semantics.
fn stored_text(hash: &str, text: &PromptText) -> serde_json::Value {
    let children: BTreeSet<&str> = text.children.iter().map(String::as_str).collect();
    if children.is_empty() {
        serde_json::json!([hash, &*text.text])
    } else {
        serde_json::json!([hash, &*text.text, children])
    }
}

/// The children a text's row stores.
fn read_children(stored: Option<&str>) -> rusqlite::Result<Vec<String>> {
    stored.map_or(Ok(Vec::new()), |children| {
        serde_json::from_str(children).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
        })
    })
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
    let rows = statement.query_map(params, |row| crate::codec::hash_column(row, 0))?;
    rows.collect()
}

pub(super) fn snapshot(tx: &Connection, call: &PromptCallKey) -> Answer<Option<PromptSnapshotRow>> {
    let (session, owner, ordinal) = call_params(call);
    let Some((snapshot, request, written_epoch, expired)) = tx
        .prepare_cached(SQL.read_snapshot.sql())?
        .query_row(rusqlite::params![session, owner, ordinal], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .optional()?
    else {
        return Ok(Ok(None));
    };
    if expired {
        return Ok(Err(DurableError::Domain(
            DomainRefusal::PromptAuditExpired { call: call.clone() },
        )));
    }
    // Recover the complete closure in one statement, irrespective of the
    // chronological prefix's depth. Both dialects use the same edge owner.
    let roots = tx
        .prepare_cached(SQL.read_edges.sql())?
        .query_map(rusqlite::params![session, owner, ordinal], |row| {
            Ok((row.get::<_, String>(0)?, crate::codec::hash_column(row, 1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (request_texts, snapshot_texts): (Vec<_>, Vec<_>) = roots
        .into_iter()
        .partition(|(part, _)| part == REQUEST_PART);
    let hashes = |edges: Vec<(String, String)>| edges.into_iter().map(|(_, hash)| hash).collect();
    Ok(Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot,
        request,
        snapshot_texts: hashes(snapshot_texts),
        request_texts: hashes(request_texts),
        written_epoch: Epoch(written_epoch),
    })))
}

pub(super) fn texts(tx: &Connection, hashes: &[String]) -> Answer<Vec<PromptText>> {
    if hashes.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let list = encode(&hashes)?;
    let mut statement = tx.prepare_cached(SQLITE_SQL.read_texts.sql())?;
    let texts = statement
        .query_map([&list], |row| {
            Ok((
                crate::codec::hash_column(row, 0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|row| {
            let (hash, text, children) = row?;
            Ok(PromptText {
                hash,
                text: text.into(),
                children: read_children(children.as_deref())?,
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Ok(texts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::store::SessionCatalogStore as _;
    use lash_core_store::store::Target;
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore};

    /// PROMPT-RETIRE-WORK: publishing and settling one more turn must not
    /// rewrite or retire paths proportional to the history already retained.
    #[tokio::test]
    async fn prompt_retirement_work_is_independent_of_session_history() {
        let mut bounded = true;
        for (repeated, compacted, rebuilt) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            bounded &= prompt_history_work(repeated, compacted, rebuilt).await;
        }
        assert!(
            bounded,
            "prompt publication and retirement grow with session history"
        );
    }

    async fn prompt_history_work(repeated: bool, compacted: bool, rebuilt: bool) -> bool {
        use lash_core_ids::operational_metrics::sql::collect;
        use lash_core_store::prompt_sections::OwnerPromptState;
        use lash_protocol_types::llm::types::{
            ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
        };
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
        let session = SessionId::from("sql-work-owner");
        let actor = ActorKey::session(session.as_str()).unwrap();
        let route = ProviderRouteIdentity {
            provider: "fixture".into(),
            endpoint: "https://fixture.test".into(),
            model: "model".into(),
        };

        let mut owner = OwnerPromptState::default();
        let mut items = RequestItems::default();
        let mut early: [Vec<[u64; 3]>; 7] = std::array::from_fn(|_| Vec::new());
        let mut late = early.clone();
        for turn in 1_usize..=1000 {
            if compacted && turn > 1 && (turn - 1).is_multiple_of(100) {
                let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
                tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
                    session: session.clone(),
                }));
                store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
                owner.clear();
                items = RequestItems::default()
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": "user", "content": "compaction summary"
                    }))
                    .unwrap()])
                    .unwrap();
            }
            for role in if repeated {
                &["user"][..]
            } else {
                &["user", "assistant"][..]
            } {
                items = items
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": role, "content": if repeated { "history turn".to_owned() } else { format!("history turn {turn}") }
                    }))
                    .unwrap()])
                    .unwrap();
            }
            // Lowerer/cache replacement can supply an equal complete prefix
            // under fresh segment identities. Held ancestors still own it.
            if rebuilt {
                let segments: Vec<_> = items
                    .iter()
                    .map(|item| {
                        RequestSegment::item_json(
                            &serde_json::from_str::<serde_json::Value>(&item.text_lossy()).unwrap(),
                        )
                        .unwrap()
                    })
                    .collect();
                items = RequestItems::default().append(&segments).unwrap();
            }
            let template = RecordedRequestTemplate::from_item_run(
                route.clone(),
                true,
                None,
                "{\"messages\":[".into(),
                items.clone(),
                "]}".into(),
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            let run = TurnId::parse(format!("history-{turn}")).unwrap();
            let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: PromptCallKey {
                    session: session.clone(),
                    call: ModelCallId::Turn {
                        run: run.clone(),
                        ordinal: 1,
                    },
                },
                snapshot: None,
                request: serde_json::to_string(&chunked).unwrap(),
                snapshot_texts: Vec::new(),
                request_texts: texts,
                prefix: chunked.prefix.map(|root| root.blob.0),
            }));
            tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                session: session.clone(),
                run,
                open: None,
            }));
            let (result, receipt) = collect(
                "prompt-history",
                "sqlite",
                store.commit(tx, CommitLabel::new("model.done")),
            )
            .await;
            result.unwrap();
            if repeated {
                items = items
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": "assistant", "content": "history reply"
                    }))
                    .unwrap()])
                    .unwrap();
            }

            let mut work = [[0; 3]; 7];
            for (sql, shape) in &receipt.shapes {
                let family = if sql.starts_with("delete from main.prompt_texts ") {
                    Some(0)
                } else if sql.starts_with("delete from main.prompt_text_edges ") {
                    Some(1)
                } else if sql.starts_with("insert into main.prompt_text_edges ") {
                    Some(2)
                } else if sql.starts_with("with recursive candidates as") {
                    Some(3)
                } else if sql.starts_with("select hash from main.prompt_texts ") {
                    Some(4)
                } else if sql.starts_with("insert into main.prompt_texts ") {
                    Some(5)
                } else if sql.starts_with("delete from main.prompt_retirement_frontier ") {
                    Some(6)
                } else {
                    None
                };
                if let Some(family) = family {
                    work[family][0] += shape.statements;
                    work[family][1] += shape.sqlite_work.unwrap().vm_steps;
                    work[family][2] += shape.rows_returned;
                }
            }
            let cohort = if (10..=30).contains(&turn) {
                Some(&mut early)
            } else if (if compacted { 910..=930 } else { 980..=1000 }).contains(&turn) {
                Some(&mut late)
            } else {
                None
            };
            if let Some(cohort) = cohort {
                for (samples, steps) in cohort.iter_mut().zip(work) {
                    samples.push(steps);
                }
            }
        }
        let mut bounded = true;
        for ((family, early), late) in [
            "prompt delete",
            "edge delete",
            "edge insert",
            "retirement discovery",
            "held frontier",
            "prompt insert",
            "retirement take",
        ]
        .into_iter()
        .zip(early)
        .zip(late)
        {
            assert_eq!(early.len(), 21);
            assert_eq!(late.len(), 21);
            for (metric, name) in ["statements", "VM steps", "rows"].into_iter().enumerate() {
                let median = |samples: &[[u64; 3]]| {
                    let mut values: Vec<_> = samples.iter().map(|sample| sample[metric]).collect();
                    values.sort_unstable();
                    values[10]
                };
                let (early, late) = (median(&early), median(&late));
                eprintln!(
                    "repeated={repeated} compacted={compacted} rebuilt={rebuilt} {family} {name}: early={early} late={late}"
                );
                if metric == 1 && !matches!(family, "prompt delete" | "edge delete") {
                    assert!(early > 0, "{family} was not measured");
                }
                bounded &= late * 100 <= early * 105;
            }
        }
        bounded
    }

    /// PREFIX-READER: a carried forest's obsolete links live exactly as long as
    /// an admitted reader, even across carry, prefix reset and last-reader release.
    #[tokio::test]
    async fn held_prompt_links_survive_carries_until_the_last_reader_settles() {
        use lash_core_store::prompt_sections::OwnerPromptState;
        use lash_durable::domain::{ModelCallId, PromptCallKey, PromptWrite};

        use lash_protocol_types::llm::types::{
            ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
        };
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
        let actor = ActorKey::session("sql-work-owner").unwrap();
        let session = SessionId::from("sql-work-owner");
        let route = ProviderRouteIdentity {
            provider: "fixture".into(),
            endpoint: "https://fixture.test".into(),
            model: "model".into(),
        };

        let mut owner = OwnerPromptState::default();
        let mut items = RequestItems::default();
        let mut held = None;
        let mut all = std::collections::BTreeSet::new();
        for turn in 1..=64 {
            items = items
                .append(&[RequestSegment::item_json(
                    &serde_json::json!({"content":format!("turn {turn}")}),
                )
                .unwrap()])
                .unwrap();
            let template = RecordedRequestTemplate::from_item_run(
                route.clone(),
                true,
                None,
                "{\"messages\":[".into(),
                items.clone(),
                "]}".into(),
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            all.extend(texts.iter().map(|text| text.hash.clone()));
            let run = TurnId::parse(format!("history-{turn}")).unwrap();
            if turn == 14 {
                held = Some((chunked.clone(), texts.clone(), template, run.clone()));
            }
            let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: PromptCallKey {
                    session: session.clone(),
                    call: ModelCallId::Turn {
                        run: run.clone(),
                        ordinal: 1,
                    },
                },
                snapshot: None,
                request: serde_json::to_string(&chunked).unwrap(),
                snapshot_texts: Vec::new(),
                request_texts: texts,
                prefix: chunked.prefix.map(|root| root.blob.0),
            }));
            if turn != 14 {
                tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                    session: session.clone(),
                    run,
                    open: None,
                }));
            }
            store.commit(tx, CommitLabel::MODEL_START).await.unwrap();
        }
        let (chunked, texts, exact, run) = held.unwrap();
        let held_call = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: run.clone(),
                ordinal: 1,
            },
        };
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
            session: session.clone(),
        }));
        store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
        let row = store.prompt_snapshot(&held_call).await.unwrap().unwrap();
        assert_eq!(
            row.request_texts,
            texts
                .iter()
                .map(|text| text.hash.clone())
                .collect::<Vec<_>>(),
            "recovery reads the held prefix's exact closure"
        );
        let hashes: Vec<_> = all.into_iter().collect();
        let found = store.prompt_texts(&hashes).await.unwrap();
        assert_eq!(
            found,
            texts
                .iter()
                .cloned()
                .map(|mut text| {
                    text.children.sort();
                    text.children.dedup();
                    text
                })
                .collect::<Vec<_>>(),
            "only the old reader's exact closure survives"
        );
        assert_eq!(
            chunked
                .assemble(|hash| found
                    .iter()
                    .find(|text| text.hash == hash.as_str())
                    .map(|text| &*text.text))
                .unwrap(),
            exact
        );
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run,
            open: None,
        }));
        store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
        assert!(
            store.prompt_texts(&hashes).await.unwrap().is_empty(),
            "last reader releases every old link"
        );
        let (stale, request_texts) = owner.chunk(
            &RecordedRequestTemplate::from_item_run(
                route,
                true,
                None,
                "{\"messages\":[".into(),
                items,
                "]}".into(),
            )
            .unwrap(),
        );
        let missing = stale.prefix.as_ref().unwrap().blob.0.clone();
        assert!(
            request_texts.get(&missing).is_some(),
            "the old owner still has bytes"
        );
        let call = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: TurnId::from("retired-prefix"),
                ordinal: 1,
            },
        };
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call.clone(),
            snapshot: None,
            request: serde_json::to_string(&stale).unwrap(),
            snapshot_texts: Vec::new(),
            request_texts,
            prefix: Some(missing.clone()),
        }));
        assert_eq!(
            store
                .commit(tx, CommitLabel::MODEL_START)
                .await
                .unwrap_err(),
            DurableError::Domain(DomainRefusal::PromptFragmentMissing { hash: missing })
        );
        assert!(
            store.prompt_snapshot(&call).await.unwrap().is_none(),
            "refusal rolls admission back"
        );
    }

    /// PREFIX-RETIRE-BOUNDARY: a released root still reached by a live
    /// prefix must not discover the retained chronological ancestry.
    #[tokio::test]
    async fn a_live_parent_stops_retirement_at_the_released_prefix() {
        let mut work = Vec::new();
        for depth in [32, 1024] {
            let set = crate::SqliteStoreSet::memory().await.unwrap();
            let store = set.session_store_factory();
            let steps =
                store
                    .conn
                    .call(move |connection| {
                        let hashes: Vec<_> = (0..=depth)
                            .map(|i| {
                                lash_core_store::store::BlobRef::for_content(
                                    format!("retained-prefix-{i}").as_bytes(),
                                )
                                .0
                            })
                            .collect();
                        for (i, hash) in hashes.iter().enumerate() {
                            let children = if i == 0 {
                                Vec::new()
                            } else {
                                vec![hashes[i - 1].clone()]
                            };
                            connection.execute(
                        "INSERT INTO prompt_texts(hash, text, children) VALUES (?1, 'node', ?2)",
                        rusqlite::params![crate::codec::SqlHash(hash), encode(&children)?],
                    )?;
                            for child in children {
                                connection.execute(
                                    "INSERT INTO prompt_text_edges(parent, child) VALUES (?1, ?2)",
                                    rusqlite::params![
                                        crate::codec::SqlHash(hash),
                                        crate::codec::SqlHash(&child)
                                    ],
                                )?;
                            }
                        }
                        connection.execute(
                            "INSERT INTO prompt_prefixes(session_id, hash) VALUES ('live', ?1)",
                            [crate::codec::SqlHash(&hashes[depth])],
                        )?;
                        connection.execute(
                            "INSERT INTO prompt_retirement_frontier(hash) VALUES (?1)",
                            [crate::codec::SqlHash(&hashes[depth - 1])],
                        )?;
                        let retired = super::drain_retired(connection, 4096)?;
                        let steps = connection
                            .prepare_cached(super::SQLITE_SQL.retirement_candidates.sql())?
                            .get_status(rusqlite::StatementStatus::VmStep);
                        assert_eq!(
                            retired,
                            (1, 0, 0),
                            "a live parent protects the complete prefix without walking it"
                        );
                        assert_eq!(
                            connection.query_row("SELECT count(*) FROM prompt_texts", [], |r| r
                                .get::<_, usize>(0))?,
                            depth + 1
                        );
                        connection.execute("DELETE FROM prompt_prefixes", [])?;
                        super::reclaim(connection, &BTreeSet::from([hashes[depth].clone()]))?;
                        assert_eq!(
                            connection.query_row("SELECT count(*) FROM prompt_texts", [], |r| r
                                .get::<_, usize>(0))?,
                            0,
                            "last root release still reclaims the complete chain"
                        );
                        Ok(steps)
                    })
                    .await
                    .unwrap();
            work.push(steps);
        }
        eprintln!("PREFIX-RETIRE-BOUNDARY VM steps: {work:?}");
        assert!(
            work[1] <= work[0] * 2,
            "retained ancestry must not multiply discovery work: {work:?}"
        );
    }

    /// LO-ROOT-WORK: an unrelated session's live prefix cannot add work to
    /// deciding whether the one retired prompt leaf is protected.
    #[tokio::test]
    async fn lo_root_prompt_retirement_ignores_unrelated_sessions() {
        let mut work = Vec::new();
        for size in [32_usize, 4096] {
            let set = crate::SqliteStoreSet::memory().await.unwrap();
            let store = set.session_store_factory();
            let steps = store
                .conn
                .call(move |connection| {
                    let shared =
                        lash_core_store::store::BlobRef::for_content(b"shared live prompt");
                    let retired = lash_core_store::store::BlobRef::for_content(b"retired prompt");
                    for hash in [&shared, &retired] {
                        connection.execute(
                        "INSERT INTO prompt_texts(hash, text, children) VALUES (?1, 'leaf', '[]')",
                        [crate::codec::SqlHash(hash.as_str())],
                    )?;
                    }
                    for session in 0..size {
                        connection.execute(
                            "INSERT INTO prompt_prefixes(session_id, hash) VALUES (?1, ?2)",
                            rusqlite::params![
                                format!("unrelated-{session}"),
                                crate::codec::SqlHash(shared.as_str())
                            ],
                        )?;
                    }
                    connection.execute(
                        "INSERT INTO prompt_retirement_frontier(hash) VALUES (?1)",
                        [crate::codec::SqlHash(retired.as_str())],
                    )?;
                    assert_eq!(super::drain_retired(connection, 256)?, (1, 1, 4));
                    assert_eq!(
                        connection.query_row(
                            "SELECT count(*) FROM prompt_prefixes",
                            [],
                            |row| row.get::<_, usize>(0)
                        )?,
                        size
                    );
                    let statement =
                        connection.prepare_cached(super::SQLITE_SQL.retirement_candidates.sql())?;
                    Ok(statement.get_status(rusqlite::StatementStatus::VmStep))
                })
                .await
                .unwrap();
            eprintln!("LO-ROOT-WORK unrelated_sessions={size} vm_steps={steps}");
            work.push(steps);
        }
        assert!(
            work[1] <= work[0] * 2,
            "unrelated session prefixes must not multiply retirement work: {work:?}"
        );
    }

    /// PROMPT-AUDIT-RETENTION: revision expiry releases settled audit plans
    /// and last-reference texts, retaining pending requests and the prefix.
    #[tokio::test]
    async fn settled_prompt_audits_follow_revision_retention() {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let store = set.durable_store();
        let session = SessionId::from("sql-work-owner");
        let actor = ActorKey::session(session.as_str()).unwrap();
        let call = |ordinal| PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: TurnId::from("settled"),
                ordinal,
            },
        };
        let text = |hash: &str| PromptText {
            hash: lash_core_store::store::BlobRef::for_content(hash.as_bytes()).0,
            text: hash.to_owned().into(),
            children: Vec::new(),
        };
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        for ordinal in 1..=131 {
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: call(ordinal),
                snapshot: Some(format!("audit-{ordinal}")),
                request: format!("exact-request-{ordinal}"),
                snapshot_texts: vec![text("shared"), text(&format!("unique-{ordinal}"))],
                request_texts: vec![text("request"), text("prefix")].into(),
                prefix: Some(text("prefix").hash),
            }));
        }
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run: TurnId::from("settled"),
            open: Some(131),
        }));
        store
            .commit(tx, CommitLabel::new("model.start"))
            .await
            .unwrap();
        // Seed two published revisions and the settled run's exact revision.
        // The last call deliberately remains unconsumed even past revision
        // release: recovery owns its request and snapshot until settlement.
        store.conn.write(|tx| {
            tx.execute("INSERT INTO blobs (hash, content, encoding) VALUES (zeroblob(32), X'00', 0)", [])?;
            tx.execute("INSERT INTO session_meta (session_id, relation_kind, retention_kind)
                VALUES ('sql-work-owner', 'root', 'head_only')", [])?;
            for revision in [1, 2] {
                tx.execute("INSERT INTO session_revisions (session_id, head_revision, config_ref)
                    VALUES ('sql-work-owner', ?1, zeroblob(32))", [revision])?;
            }
            tx.execute("INSERT INTO session_head (session_id, head_revision) VALUES ('sql-work-owner', 2)", [])?;
            tx.execute("INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json,
                terminal_at_ms, terminal_head_revision, terminal_window_revision)
                VALUES ('sql-work-owner', 'settled', 'answered', '{\"cause\":\"commands_applied\"}', 0, 1, 1)", [])?;
            tx.execute("INSERT INTO pins (session_id, target_kind, target_id)
                VALUES ('sql-work-owner', 'revision', '1')", [])?;
            crate::revisions::record_terminal_conn(tx, &SessionId::from("sql-work-owner"), 2, lash_durable::DurableInstant(0))
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
        }).await.unwrap();
        assert_eq!(
            store
                .prompt_snapshot(&call(1))
                .await
                .unwrap()
                .unwrap()
                .snapshot
                .as_deref(),
            Some("audit-1"),
            "the revision pin keeps its settled audits exact"
        );
        let catalog = set.session_store_factory();
        catalog.unpin(&session, &Target::Revision(1)).await.unwrap();
        // Logical expiry never waits for a physical page: even the two roots
        // beyond the first batch already answer the same typed refusal.
        for ordinal in 1..=130 {
            assert!(matches!(store.prompt_snapshot(&call(ordinal)).await,
                Err(DurableError::Domain(DomainRefusal::PromptAuditExpired { call: expired }))
                    if expired == call(ordinal)));
        }
        let remaining = store.conn.read(|tx| tx.query_row(
            "SELECT COUNT(*) FROM prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL", [],
            |row| row.get::<_, i64>(0))).await.unwrap();
        assert_eq!(
            remaining, 2,
            "one invocation releases exactly one internal batch"
        );
        catalog.gc_unreachable().await.unwrap();
        let remaining = store.conn.read(|tx| tx.query_row(
            "SELECT COUNT(*) FROM prompt_snapshots WHERE snapshot IS NOT NULL AND request IS NULL", [],
            |row| row.get::<_, i64>(0))).await.unwrap();
        assert_eq!(
            remaining, 0,
            "GC drains a quiet session without another commit"
        );
        let pending = store.prompt_snapshot(&call(131)).await.unwrap().unwrap();
        assert_eq!(pending.request.as_deref(), Some("exact-request-131"));
        assert_eq!(pending.snapshot.as_deref(), Some("audit-131"));
        let hashes = [
            "shared",
            "unique-1",
            "unique-2",
            "unique-131",
            "request",
            "prefix",
        ]
        .map(|name| text(name).hash);
        let found = store.prompt_texts(&hashes).await.unwrap();
        assert_eq!(
            found.iter().map(|text| &*text.text).collect::<Vec<_>>(),
            ["shared", "unique-131", "request", "prefix"]
        );
        // Settling the last request does not erase previously expired identities.
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Settle {
            session: session.clone(),
            run: TurnId::from("settled"),
            open: None,
        }));
        store
            .commit(tx, CommitLabel::new("model.done"))
            .await
            .unwrap();
        catalog.gc_unreachable().await.unwrap();
        assert_eq!(
            store.prompt_texts(&hashes).await.unwrap(),
            [text("prefix")],
            "the current lowering prefix keeps its own independent reference"
        );
        let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call(1),
            snapshot: None,
            request: "changed".to_owned(),
            snapshot_texts: Vec::new(),
            request_texts: Default::default(),
            prefix: None,
        }));
        assert!(matches!(
            store.commit(tx, CommitLabel::new("model.start")).await,
            Err(DurableError::Domain(
                DomainRefusal::PromptCallRecorded { .. }
            ))
        ));
    }
}
