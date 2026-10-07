//! Prompt snapshots on PostgreSQL: the `prompts` domain's statements, apply
//! and read (FIG-5256, ADR 0133 §5).
//!
//! Owned by P2 (FIG-5256). The dispatch in `durable/mod.rs` calls these
//! inside the fenced owner commit, after the fence; a refusal rolls the whole
//! commit back. The DDL is in `schema.sql`.
//!
//! A release and a record that share a text cannot leave an edge to a
//! reclaimed text: the edge's foreign key locks the text row it names, so a
//! concurrent reclaim of that text fails instead of deleting it.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::Dialect;
use lash_store_sql::durable::prompts::PromptStatements;
use sqlx::PgConnection;

use super::{Committing, get, sqlx_failure};

static SQL: LazyLock<PromptStatements> =
    LazyLock::new(|| PromptStatements::render(Dialect::postgres()));

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
                .bind(call.run.as_str())
                .bind(i64::from(call.call))
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
            let mut rooted = BTreeSet::new();
            for text in texts {
                if !rooted.insert(text.hash.as_str()) {
                    continue;
                }
                sqlx::query(SQL.insert_text.sql())
                    .bind(&text.hash)
                    .bind(&text.text)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
                sqlx::query(SQL.insert_edge.sql())
                    .bind(call.session.as_str())
                    .bind(call.run.as_str())
                    .bind(i64::from(call.call))
                    .bind(&text.hash)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            Ok(())
        }
        PromptWrite::Release { session, run } => {
            let released: Vec<String> = match run {
                None => {
                    let hashes = sqlx::query_scalar(SQL.release_session_edges.sql())
                        .bind(session.as_str())
                        .fetch_all(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                    sqlx::query(SQL.release_session_snapshots.sql())
                        .bind(session.as_str())
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                    hashes
                }
                Some(run) => {
                    let hashes = sqlx::query_scalar(SQL.release_run_edges.sql())
                        .bind(session.as_str())
                        .bind(run.as_str())
                        .fetch_all(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                    sqlx::query(SQL.release_run_snapshots.sql())
                        .bind(session.as_str())
                        .bind(run.as_str())
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                    hashes
                }
            };
            for hash in released.into_iter().collect::<BTreeSet<_>>() {
                sqlx::query(SQL.reclaim_text.sql())
                    .bind(hash)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            Ok(())
        }
    }
}

pub(super) async fn snapshot(
    tx: &mut PgConnection,
    call: &PromptCallKey,
) -> Result<Option<PromptSnapshotRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.read_snapshot.sql())
        .bind(call.session.as_str())
        .bind(call.run.as_str())
        .bind(i64::from(call.call))
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let texts: Vec<String> = sqlx::query_scalar(SQL.read_edges.sql())
        .bind(call.session.as_str())
        .bind(call.run.as_str())
        .bind(i64::from(call.call))
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
