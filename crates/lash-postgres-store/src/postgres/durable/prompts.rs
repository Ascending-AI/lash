//! Prompt snapshots on PostgreSQL: the `prompts` domain's statements, apply
//! and read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. Session deletion releases through [`release`] inside its own
//! transaction (FIG-5272). The DDL is in `schema.sql`.
//!
//! A record and a release that share a text are serialized on that text
//! (FIG-5272). Under READ COMMITTED neither sees the other's uncommitted
//! edge, and the edge's foreign key alone does not order them: a release
//! that reclaims a text a concurrent record found stored fails that record's
//! edge, and a release whose reclaim waits behind a record's edge then fails
//! its own delete. So a record takes each text's advisory lock shared before
//! it looks for the text, and a release (or a settlement) takes it exclusive
//! before it reclaims, both in text order. Records never wait for each
//! other; a reclaim waits for every open record of its texts and, in a
//! statement after the lock, sees their edges, while a record behind a
//! reclaim finds the text gone in a statement after its lock and binds its
//! body again. Under the lock a record binds the bodies of only the texts
//! it does not find (FIG-5946): what it finds stays until it commits. The
//! lock lives in memory, not in the hot shared text rows, which is why it is
//! preferred over locking those rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::{PromptStatements, REQUEST_PART, SNAPSHOT_PART};
use sqlx::PgConnection;

use super::{Committing, get, sqlx_failure};
use crate::connection_sql::connection_sql;

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// The `prompt_texts` and `prompt_snapshot_texts` statements only
    /// PostgreSQL issues: `unnest` is how it binds a call's texts as one
    /// list.
    struct PostgresPromptStatements @ "durable_prompt_postgres" {
        /// Every content address in `?1` already stored.
        held_texts = "SELECT hash FROM prompt_texts WHERE hash = ANY(?1::TEXT[])";

        /// Store each text `?2[i]` at content address `?1[i]`, skipping every
        /// address a concurrent record stored first: the address names the
        /// bytes, so a conflict is the same text.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT hash, text FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(hash, text)
             ON CONFLICT (hash) DO NOTHING";

        /// Root text `?5[i]` under part `?4[i]` of call `?3` of owner `?2`
        /// in session `?1`, for every `i`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, part, hash)
             SELECT ?1, ?2, ?3, part, hash
               FROM unnest(?4::TEXT[], ?5::TEXT[]) AS batch(part, hash)";

        /// Settle every call of owner `?2` in session `?1` but call `?3`
        /// (none when `?3` is null): release its request edges, its root
        /// when the request was all it held, and otherwise its request,
        /// returning each distinct text a released edge named. The two root
        /// writes touch disjoint rows, and a bare root's request edges go in
        /// the same statement, before its foreign key is checked.
        settle = "WITH released AS (
                 DELETE FROM prompt_snapshot_texts
                  WHERE session_id = ?1 AND owner = ?2 AND part = 'request'
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
                 RETURNING hash
             ), bare AS (
                 DELETE FROM prompt_snapshots
                  WHERE session_id = ?1 AND owner = ?2 AND snapshot IS NULL
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
             ), kept AS (
                 UPDATE prompt_snapshots SET request = NULL
                  WHERE session_id = ?1 AND owner = ?2
                    AND snapshot IS NOT NULL AND request IS NOT NULL
                    AND (?3::TEXT IS NULL OR call <> ?3::TEXT)
             )
             SELECT DISTINCT hash FROM released";

        /// Reclaim every text in `?1` no root references any more.
        reclaim_texts = "DELETE FROM prompt_texts AS stored
             WHERE stored.hash = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = stored.hash
               )";
    }
}

static POSTGRES_SQL: LazyLock<PostgresPromptStatements> =
    LazyLock::new(|| PostgresPromptStatements::render(Dialect::postgres()));

/// The advisory-lock seed of prompt-text retention (`connection_sql`).
const TEXT_LOCK_SEED: i64 = 2;

#[cfg(test)]
lash_store_sql::statements! {
    /// The `prompt_texts` statements only PostgreSQL's laws issue.
    pub(crate) struct PromptLawStatements @ "durable_prompt_postgres_law" {
        /// Hold the text at `?1` against every edge to it until this
        /// transaction ends: an edge's foreign key check waits behind it.
        hold_text = "SELECT hash FROM prompt_texts WHERE hash = ?1 FOR UPDATE";
    }
}

/// The statements PostgreSQL's prompt laws issue, rendered once.
#[cfg(test)]
pub(crate) static LAW_SQL: LazyLock<PromptLawStatements> =
    LazyLock::new(|| PromptLawStatements::render(Dialect::postgres()));

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &PromptWrite,
) -> Result<(), DurableError> {
    match write {
        PromptWrite::Record {
            call,
            snapshot,
            request,
            snapshot_texts,
            request_texts,
        } => {
            let recorded = crate::observed_sql::query(SQL.insert_snapshot.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(snapshot.as_deref())
                .bind(request)
                .bind(commit.epoch.0)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if recorded.is_none() {
                return Err(DurableError::Domain(DomainRefusal::PromptCallRecorded {
                    call: call.clone(),
                }));
            }
            // The first text under each address stands for it.
            let mut distinct = BTreeMap::new();
            for text in snapshot_texts.iter().chain(request_texts) {
                distinct
                    .entry(text.hash.as_str())
                    .or_insert(text.text.as_str());
            }
            if distinct.is_empty() {
                return Ok(());
            }
            let hashes: Vec<&str> = distinct.keys().copied().collect();
            crate::observed_sql::query(
                connection_sql().lock_xact_shared_by_text_batch_seeded.sql(),
            )
            .bind(&hashes)
            .bind(TEXT_LOCK_SEED)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
            // A statement after the lock: a text it finds stays until this
            // transaction commits, so only the others' bodies are sent.
            let held: BTreeSet<String> =
                crate::observed_sql::query_scalar(POSTGRES_SQL.held_texts.sql())
                    .bind(&hashes)
                    .fetch_all(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?
                    .into_iter()
                    .collect();
            let (unheld, bodies): (Vec<&str>, Vec<&str>) = distinct
                .iter()
                .filter(|(hash, _)| !held.contains(**hash))
                .map(|(hash, text)| (*hash, *text))
                .unzip();
            if !unheld.is_empty() {
                crate::observed_sql::query(POSTGRES_SQL.insert_texts.sql())
                    .bind(&unheld)
                    .bind(&bodies)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            let (parts, edges): (Vec<&str>, Vec<&str>) =
                edges(snapshot_texts, request_texts).into_iter().unzip();
            crate::observed_sql::query(POSTGRES_SQL.insert_edges.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(&parts)
                .bind(&edges)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        PromptWrite::Settle { session, run, open } => {
            settle(tx, session, run, *open).await.map_err(sqlx_failure)
        }
        PromptWrite::Release { session, run } => release(tx, session, run.as_ref())
            .await
            .map_err(sqlx_failure),
    }
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

/// Release the request of every call of turn `run` in `session` but `open`,
/// and reclaim every text no remaining edge references.
async fn settle(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    open: Option<u32>,
) -> Result<(), sqlx::Error> {
    let released: Vec<String> = crate::observed_sql::query_scalar(POSTGRES_SQL.settle.sql())
        .bind(session.as_str())
        .bind(ModelCallId::turn_owner(run))
        .bind(open.map(|call| call.to_string()))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    reclaim(tx, released).await
}

/// Release every snapshot root of `session`, or of its turn `run` alone, and
/// reclaim every text no remaining root references.
///
/// # Errors
///
/// The first statement PostgreSQL refused.
pub(crate) async fn release(
    tx: &mut PgConnection,
    session: &SessionId,
    run: Option<&TurnId>,
) -> Result<(), sqlx::Error> {
    let released: Vec<String> = match run {
        None => {
            let hashes = crate::observed_sql::query_scalar(SQL.release_session_edges.sql())
                .bind(session.as_str())
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            crate::observed_sql::query(SQL.release_session_snapshots.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
        Some(run) => {
            let hashes = crate::observed_sql::query_scalar(SQL.release_run_edges.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            crate::observed_sql::query(SQL.release_run_snapshots.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
    };
    reclaim(tx, released).await
}

/// Reclaim every text among `released` no remaining edge references.
async fn reclaim(tx: &mut PgConnection, released: Vec<String>) -> Result<(), sqlx::Error> {
    let released: Vec<String> = released
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if released.is_empty() {
        return Ok(());
    }
    // The lock waits out every open record of these texts; the reclaim, a
    // later statement, then sees each edge those records committed.
    crate::observed_sql::query(connection_sql().lock_xact_by_text_batch_seeded.sql())
        .bind(&released)
        .bind(TEXT_LOCK_SEED)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    crate::observed_sql::query(POSTGRES_SQL.reclaim_texts.sql())
        .bind(&released)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    Ok(())
}

pub(super) async fn snapshot(
    tx: &mut PgConnection,
    call: &PromptCallKey,
) -> Result<Option<PromptSnapshotRow>, DurableError> {
    let Some(row) = crate::observed_sql::query(SQL.read_snapshot.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let edges: Vec<(String, String)> = crate::observed_sql::query_as(SQL.read_edges.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let (request_texts, snapshot_texts): (Vec<_>, Vec<_>) = edges
        .into_iter()
        .partition(|(part, _)| part == REQUEST_PART);
    let hashes = |edges: Vec<(String, String)>| edges.into_iter().map(|(_, hash)| hash).collect();
    Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot: get(&row, 0)?,
        request: get(&row, 1)?,
        snapshot_texts: hashes(snapshot_texts),
        request_texts: hashes(request_texts),
        written_epoch: Epoch(get(&row, 2)?),
    }))
}

pub(super) async fn texts(
    tx: &mut PgConnection,
    hashes: &[String],
) -> Result<Vec<PromptText>, DurableError> {
    let mut found = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let text: Option<String> = crate::observed_sql::query_scalar(SQL.read_text.sql())
            .bind(hash)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if let Some(text) = text {
            found.push(PromptText {
                hash: hash.clone(),
                text,
            });
        }
    }
    Ok(found)
}
