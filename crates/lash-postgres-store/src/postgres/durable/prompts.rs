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
//! it stores the text, and a release takes it exclusive before it reclaims,
//! both in text order. Records never wait for each other; a reclaim waits
//! for every open record of its texts and, in a statement after the lock,
//! sees their edges, while a record behind a reclaim stores the text again.
//! The lock lives in memory, not in the hot shared text rows, which is why
//! it is preferred over locking those rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::PromptStatements;
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
        /// Store each text `?2[i]` at content address `?1[i]`, skipping every
        /// address already stored: the address names the bytes, so a
        /// conflict is the same text.
        insert_texts = "INSERT INTO prompt_texts (hash, text)
             SELECT hash, text FROM unnest(?1::TEXT[], ?2::TEXT[]) AS batch(hash, text)
             ON CONFLICT (hash) DO NOTHING";

        /// Root every text in `?4` under call `?3` of owner `?2` in session
        /// `?1`.
        insert_edges = "INSERT INTO prompt_snapshot_texts (session_id, owner, call, hash)
             SELECT ?1, ?2, ?3, hash FROM unnest(?4::TEXT[]) AS batch(hash)";

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
            texts,
        } => {
            let recorded = sqlx::query(SQL.insert_snapshot.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(snapshot)
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
            for text in texts {
                distinct
                    .entry(text.hash.as_str())
                    .or_insert(text.text.as_str());
            }
            if distinct.is_empty() {
                return Ok(());
            }
            let hashes: Vec<&str> = distinct.keys().copied().collect();
            let bodies: Vec<&str> = distinct.values().copied().collect();
            sqlx::query(connection_sql().lock_xact_shared_by_text_batch_seeded.sql())
                .bind(&hashes)
                .bind(TEXT_LOCK_SEED)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            sqlx::query(POSTGRES_SQL.insert_texts.sql())
                .bind(&hashes)
                .bind(&bodies)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            sqlx::query(POSTGRES_SQL.insert_edges.sql())
                .bind(call.session.as_str())
                .bind(call.call.owner_column())
                .bind(call.call.call_column())
                .bind(&hashes)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        PromptWrite::Release { session, run } => release(tx, session, run.as_ref())
            .await
            .map_err(sqlx_failure),
    }
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
            let hashes = sqlx::query_scalar(SQL.release_session_edges.sql())
                .bind(session.as_str())
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            sqlx::query(SQL.release_session_snapshots.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
        Some(run) => {
            let hashes = sqlx::query_scalar(SQL.release_run_edges.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await?;
            sqlx::query(SQL.release_run_snapshots.sql())
                .bind(session.as_str())
                .bind(ModelCallId::turn_owner(run))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
            hashes
        }
    };
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
    sqlx::query(connection_sql().lock_xact_by_text_batch_seeded.sql())
        .bind(&released)
        .bind(TEXT_LOCK_SEED)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    sqlx::query(POSTGRES_SQL.reclaim_texts.sql())
        .bind(&released)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    Ok(())
}

pub(super) async fn snapshot(
    tx: &mut PgConnection,
    call: &PromptCallKey,
) -> Result<Option<PromptSnapshotRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.read_snapshot.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let texts: Vec<String> = sqlx::query_scalar(SQL.read_edges.sql())
        .bind(call.session.as_str())
        .bind(call.call.owner_column())
        .bind(call.call.call_column())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(Some(PromptSnapshotRow {
        call: call.clone(),
        snapshot: get(&row, 0)?,
        texts,
        written_epoch: Epoch(get(&row, 1)?),
    }))
}

pub(super) async fn texts(
    tx: &mut PgConnection,
    hashes: &[String],
) -> Result<Vec<PromptText>, DurableError> {
    let mut found = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let text: Option<String> = sqlx::query_scalar(SQL.read_text.sql())
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
