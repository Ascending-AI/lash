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

        /// The texts call `?3` of owner `?2` roots, each with its part.
        read_roots = "SELECT part, hash FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3";

        /// The stored children of every text in the JSON array `?1`.
        read_text_children = "SELECT hash, children FROM prompt_texts
             WHERE hash IN (SELECT unhex(value) FROM json_each(?1))";

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

        /// Candidates whose last root or immutable parent was released.
        unreferenced_texts = "SELECT stored.hash FROM prompt_texts AS stored
             WHERE stored.hash IN (SELECT unhex(value) FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = stored.hash
               )";

        /// Reclaim every text in the JSON array `?1` no root references any
        /// more, returning each with its children.
        reclaim_texts = "DELETE FROM prompt_texts AS stored
             WHERE stored.hash IN (SELECT unhex(value) FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_prefixes AS prefix WHERE prefix.hash = stored.hash
               ) AND NOT EXISTS (
                   SELECT 1 FROM prompt_text_edges AS edge WHERE edge.child = stored.hash
               ) RETURNING hash, children";
    }
}

static SQLITE_SQL: LazyLock<SqlitePromptStatements> =
    LazyLock::new(|| SqlitePromptStatements::render(crate::schema_layout::MAIN));

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
CREATE TABLE IF NOT EXISTS prompt_prefixes (
    session_id TEXT PRIMARY KEY,
    hash BLOB NOT NULL REFERENCES prompt_texts(hash)
) WITHOUT ROWID;
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
        // A reclaimed parent's edges go with it, which may leave its
        // children unreferenced on the next level.
        let reclaimed = tx
            .prepare_cached(SQLITE_SQL.reclaim_texts.sql())?
            .query_map([encode(&frontier)?], |row| {
                Ok((
                    crate::codec::hash_column(row, 0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut edges = Vec::new();
        frontier = BTreeSet::new();
        for (parent, children) in reclaimed {
            for child in read_children(children.as_deref())? {
                edges.push([child.clone(), parent.clone()]);
                frontier.insert(child);
            }
        }
        if !edges.is_empty() {
            cached_execute(tx, SQLITE_SQL.delete_index_edges.sql(), [encode(&edges)?])?;
        }
    }
    Ok(())
}

/// One text's `insert_texts` element: `[hash, text]` for a leaf, and
/// `[hash, text, children]` for an index node, its children in address
/// order as its row stores them.
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
    // The texts the call's roots reach under each part, walked level by
    // level through the children each text stores.
    let roots = tx
        .prepare_cached(SQLITE_SQL.read_roots.sql())?
        .query_map(rusqlite::params![session, owner, ordinal], |row| {
            Ok((row.get::<_, String>(0)?, crate::codec::hash_column(row, 1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let closure_of = |part: &str| -> rusqlite::Result<Vec<String>> {
        let mut reached = BTreeSet::new();
        let mut level: BTreeSet<String> = roots
            .iter()
            .filter(|(root_part, _)| root_part == part)
            .map(|(_, hash)| hash.clone())
            .collect();
        while !level.is_empty() {
            let stored = tx
                .prepare_cached(SQLITE_SQL.read_text_children.sql())?
                .query_map([encode(&level)?], |row| row.get::<_, Option<String>>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            reached.extend(std::mem::take(&mut level));
            for children in stored {
                for child in read_children(children.as_deref())? {
                    if !reached.contains(&child) {
                        level.insert(child);
                    }
                }
            }
        }
        Ok(reached.into_iter().collect())
    };
    let request_texts = closure_of(REQUEST_PART)?;
    let snapshot_texts = closure_of(SNAPSHOT_PART)?;
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
