//! A process owner's fenced open with the decision view of its pass, one
//! statement and so one snapshot
//! ([`DurableStore::open_process`](lash_durable::DurableStore::open_process)).
use std::sync::LazyLock;

use lash_durable::domain::{ExecKey, ProcessRegistryRow, ProcessView};
use lash_durable::{
    ActorKey, ActorState, ActorTx, DurableError, DurableInstant, Epoch, Fenced, Mail, MailKind,
    MailSeq, OpenedActor, StateRevision,
};
use lash_store_sql::Dialect;
use lash_vocabulary::identity::ProcessId;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::{PostgresDurableStore, corrupt, get, processes, snapshots, sqlx_failure};

lash_store_sql::statements! {
    /// The process owner's open, as PostgreSQL reads it: the store's clock
    /// is the server's, read in the statement.
    struct ProcessViewStatements @ "durable_postgres_process_view" {
        /// Process actor `?1`'s epoch, state, revision and mailbox
        /// positions, one row per pending mail, each with the server's
        /// clock, process `?2`'s actor row, its registry record (or the
        /// tombstone of a pruned one) and execution root `?3`'s row.
        open = "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    p.state_rev, p.driver_json, p.cancel_requested_at_ms, p.status,
                    p.cascade_cursor, p.written_epoch, p.published_event_sequence,
                    (substr(p.start_json, 1, length(p.start_json) - 1) || ',' || substr(p.execution_json, 2, length(p.execution_json) - 2) || ',' || substr(p.lifecycle_json, 2)),
                    t.terminal_label, t.pruned_at_ms,
                    s.rev, s.root_ref, s.written_epoch, s.commit_id
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             LEFT JOIN processes p ON p.process_id = ?2
             LEFT JOIN process_tombstones t ON t.process_id = ?2
             LEFT JOIN exec_snapshots s ON s.exec_key = ?3
             WHERE a.actor_key = ?1
             ORDER BY m.seq";
        control = "SELECT (lifecycle_json::jsonb -> 'cancel_request')::text,
                    (lifecycle_json::jsonb ->> 'last_event_sequence')::bigint,
                    (lifecycle_json::jsonb ->> 'updated_at_ms')::bigint,
                    (lifecycle_json::jsonb -> 'external_ref')::text
             FROM processes WHERE process_id = ?1 FOR UPDATE";
        /// Authority, bounded pending mail and current cancellation only.
        frontier = "WITH pending AS (
                SELECT seq, kind, body, appended_at_ms,
                       SUM(octet_length(body)) OVER (ORDER BY seq) AS bytes,
                       octet_length(body) AS body_bytes
                FROM actor_mail WHERE actor_key = ?1
                  AND seq > (SELECT acked_seq FROM actors WHERE actor_key = ?1)
             )
             SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, CASE WHEN m.bytes <= ?3 THEN m.body END,
                    m.appended_at_ms, m.bytes, m.body_bytes, (p.lifecycle_json::jsonb -> 'cancel_request')::text,
                    (p.lifecycle_json::jsonb ->> 'last_event_sequence')::bigint,
                    (p.lifecycle_json::jsonb ->> 'updated_at_ms')::bigint,
                    (p.lifecycle_json::jsonb -> 'external_ref')::text, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
             FROM actors a
             LEFT JOIN processes p ON p.process_id = ?2
             LEFT JOIN pending m ON m.bytes - m.body_bytes <= ?3
             WHERE a.actor_key = ?1 ORDER BY m.seq";
    }
}

static SQL: LazyLock<ProcessViewStatements> =
    LazyLock::new(|| ProcessViewStatements::render(Dialect::postgres()));

/// The view's columns in an open row: the process row from 10, the
/// registry record and tombstone from 17, the execution root from 20.
fn view_of(row: &PgRow, process: &ProcessId, exec: &ExecKey) -> Result<ProcessView, DurableError> {
    let registry = match (
        get::<Option<String>>(row, 17)?,
        get::<Option<String>>(row, 18)?,
    ) {
        (Some(record), _) => ProcessRegistryRow::Retained(record),
        (None, Some(terminal_label)) => ProcessRegistryRow::Pruned {
            terminal_label,
            pruned_at_ms: get(row, 19)?,
        },
        (None, None) => ProcessRegistryRow::Absent,
    };
    let root = match get::<Option<i64>>(row, 20)? {
        None => None,
        Some(rev) => {
            let root: String = crate::keys::get(row, 21).map_err(sqlx_failure)?;
            let commit_id: Vec<u8> = row.try_get(23).map_err(sqlx_failure)?;
            Some(snapshots::row_of(
                exec.clone(),
                rev,
                &root,
                get(row, 22)?,
                &commit_id,
            )?)
        }
    };
    Ok(ProcessView {
        row: processes::actor_row(row, 10, process)?,
        registry,
        root,
    })
}

impl PostgresDurableStore {
    pub(super) async fn open_process_view(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, ProcessView), DurableError> {
        if actor.kind() != lash_durable::ActorKind::Process {
            return Err(corrupt("process actor", actor.as_str()));
        }
        let process =
            ProcessId::parse(actor.id()).map_err(|_| corrupt("process id", actor.id()))?;
        let exec = ExecKey::Process(process.clone());
        let stored = exec.stored();
        let rows = self
            .read(|connection| {
                let stored = stored.clone();
                Box::pin(async move {
                    crate::observed_sql::query(SQL.open.sql())
                        .bind(actor.as_str())
                        .bind(actor.id())
                        .bind(stored)
                        .fetch_all(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(first) = rows.first() else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let current = Epoch(get(first, 0)?);
        if current != epoch || get::<String>(first, 1)? != ActorState::Owned.as_str() {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: Some(current),
            }));
        }
        let view = view_of(first, &process, &exec)?;
        let mut opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision: StateRevision(get(first, 2)?),
            acked: MailSeq(get(first, 3)?),
            seen: MailSeq(get(first, 4)?),
            mail: Vec::new(),
            at: self.instant_or(get(first, 9)?)?,
            turn_cancel: None,
        };
        for row in &rows {
            if let Some(seq) = get::<Option<i64>>(row, 5)? {
                opened.mail.push(Mail {
                    seq: MailSeq(seq),
                    kind: MailKind::new(get::<String>(row, 6)?),
                    body: get(row, 7)?,
                    appended_at: DurableInstant(get(row, 8)?),
                });
            }
        }
        Ok((ActorTx::opened(opened), view))
    }
}

impl PostgresDurableStore {
    pub(super) async fn process_frontier(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        max_mail_bytes: u64,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessFrontier), DurableError> {
        if actor.kind() != lash_durable::ActorKind::Process {
            return Err(corrupt("process actor", actor.as_str()));
        }
        let limit = i64::try_from(max_mail_bytes).unwrap_or(i64::MAX);
        let rows = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query(SQL.frontier.sql())
                        .bind(actor.as_str())
                        .bind(actor.id())
                        .bind(limit)
                        .fetch_all(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(first) = rows.first() else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let current = Epoch(get(first, 0)?);
        if current != epoch || get::<String>(first, 1)? != ActorState::Owned.as_str() {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: Some(current),
            }));
        }
        let revision = StateRevision(get(first, 2)?);
        if revision != expected {
            return Err(DurableError::RevisionConflict {
                actor: actor.clone(),
                epoch,
                expected,
                actual: revision,
            });
        }
        let cancel = get::<Option<String>>(first, 11)?
            .filter(|json| json != "null")
            .map(|json| serde_json::from_str(&json).map_err(|_| corrupt("process cancel", &json)))
            .transpose()?;
        let mut opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision,
            acked: MailSeq(get(first, 3)?),
            seen: MailSeq(get(first, 4)?),
            mail: Vec::new(),
            at: self.instant_or(get(first, 15)?)?,
            turn_cancel: None,
        };
        for row in &rows {
            if let Some(seq) = get::<Option<i64>>(row, 5)? {
                // octet_length is int4; SUM returns int8.
                let bytes: i64 = get(row, 9)?;
                if bytes > limit {
                    if max_mail_bytes > 0 && opened.mail.is_empty() {
                        return Err(DurableError::MailPageTooSmall {
                            actor: actor.clone(),
                            bytes: super::integer::<u64>(get::<i32>(row, 10)?)?,
                            limit: max_mail_bytes,
                        });
                    }
                    opened.seen = opened.mail.last().map_or(opened.acked, |mail| mail.seq);
                    break;
                }
                opened.mail.push(Mail {
                    seq: MailSeq(seq),
                    kind: MailKind::new(get::<String>(row, 6)?),
                    body: get(row, 7)?,
                    appended_at: DurableInstant(get(row, 8)?),
                });
            }
        }
        Ok((
            ActorTx::opened(opened),
            lash_durable::domain::ProcessFrontier {
                last_event_sequence: super::integer::<u64>(
                    get::<Option<i64>>(first, 12)?.unwrap_or(0),
                )?,
                updated_at_ms: super::integer::<u64>(get::<Option<i64>>(first, 13)?.unwrap_or(0))?,
                external_ref: get::<Option<String>>(first, 14)?.filter(|json| json != "null"),
                cancel,
            },
        ))
    }
}

pub(super) async fn refresh_preimage(
    tx: &mut super::Tx,
    write: &ActorTx,
) -> Result<Option<ProcessView>, DurableError> {
    refresh_view(tx, write.actor(), write.process_preimage().cloned()).await
}

pub(super) async fn refresh_view(
    tx: &mut sqlx::PgConnection,
    actor: &ActorKey,
    view: Option<ProcessView>,
) -> Result<Option<ProcessView>, DurableError> {
    let Some(mut view) = view else {
        return Ok(None);
    };
    let row = crate::observed_sql::query(SQL.control.sql())
        .bind(actor.id())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if let Some(row) = row {
        let cancel = get::<Option<String>>(&row, 0)?
            .filter(|json| json != "null")
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|error| corrupt("process frontier", &error.to_string()))
            })
            .transpose()?;
        let frontier = lash_durable::domain::ProcessFrontier {
            cancel,
            last_event_sequence: super::integer::<u64>(get::<i64>(&row, 1)?)?,
            updated_at_ms: super::integer::<u64>(get::<i64>(&row, 2)?)?,
            external_ref: get::<Option<String>>(&row, 3)?.filter(|json| json != "null"),
        };
        lash_execution_contracts::process::registry_transitions::refresh_frontier(
            &mut view, frontier,
        )
        .map_err(|error| corrupt("process frontier", &error.to_string()))?;
    }
    Ok(Some(view))
}
