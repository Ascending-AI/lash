//! A process owner's fenced open with the decision view of its pass, one
//! statement and so one snapshot
//! ([`DurableStore::open_process`](lash_durable::DurableStore::open_process)).
use std::sync::LazyLock;

use lash_durable::domain::{ExecKey, ProcessRegistryRow, ProcessView};
use lash_durable::{
    ActorKey, ActorTx, DurableError, Epoch, Fenced, MailSeq, OpenedActor, StateRevision,
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
        /// positions and one bounded page, with the server's clock,
        /// process `?2`'s actor row, its registry record (or the
        /// tombstone of a pruned one) and execution root `?3`'s row.
        open = lash_store_sql::mailbox_query!("?4", "?5", "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    p.state_rev, p.driver_json, p.cancel_requested_at_ms, p.status,
                    p.cascade_cursor, p.written_epoch, p.published_event_sequence,
                    (substr(p.start_json, 1, length(p.start_json) - 1) || ',' || substr(p.execution_json, 2, length(p.execution_json) - 2) || ',' || substr(p.lifecycle_json, 2)),
                    t.terminal_label, t.pruned_at_ms,
                    s.rev, (SELECT hash FROM blobs WHERE id = s.root_ref) AS root_ref, s.written_epoch, s.commit_id,
                    COALESCE((SELECT json_agg(json_build_array(seq, kind, body, appended_at_ms, body_bytes) ORDER BY seq) FROM mail_page), '[]'::json)::text
             FROM actors a
             LEFT JOIN processes p ON p.process_id = ?2
             LEFT JOIN process_tombstones t ON t.process_id = ?2
             LEFT JOIN exec_snapshots s ON s.exec_key = ?3
             WHERE a.actor_key = ?1");
        /// Process `?1`'s frontier under a row lock, each column named:
        /// what a one-call owner commit answers for its process view
        /// (FIG-7585).
        control_answered = "SELECT (lifecycle_json::jsonb -> 'cancel_request')::text AS cancel,
                    (lifecycle_json::jsonb ->> 'last_event_sequence')::bigint AS last_event_sequence,
                    (lifecycle_json::jsonb ->> 'updated_at_ms')::bigint AS updated_at_ms,
                    (lifecycle_json::jsonb -> 'external_ref')::text AS external_ref
             FROM processes WHERE process_id = ?1 FOR UPDATE";
        control = "SELECT (lifecycle_json::jsonb -> 'cancel_request')::text,
                    (lifecycle_json::jsonb ->> 'last_event_sequence')::bigint,
                    (lifecycle_json::jsonb ->> 'updated_at_ms')::bigint,
                    (lifecycle_json::jsonb -> 'external_ref')::text
             FROM processes WHERE process_id = ?1 FOR UPDATE";
        /// Authority, bounded pending mail and current cancellation only.
        frontier = lash_store_sql::mailbox_query!("?3", "?4", "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    (p.lifecycle_json::jsonb -> 'cancel_request')::text,
                    (p.lifecycle_json::jsonb ->> 'last_event_sequence')::bigint,
                    (p.lifecycle_json::jsonb ->> 'updated_at_ms')::bigint,
                    (p.lifecycle_json::jsonb -> 'external_ref')::text,
                    COALESCE((SELECT json_agg(json_build_array(seq, kind, body, appended_at_ms, body_bytes) ORDER BY seq) FROM mail_page), '[]'::json)::text, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
             FROM actors a
             LEFT JOIN processes p ON p.process_id = ?2
             WHERE a.actor_key = ?1");
    }
}

static SQL: LazyLock<ProcessViewStatements> =
    LazyLock::new(|| ProcessViewStatements::render(Dialect::postgres()));

/// The process row starts at 6, the registry at 13 and the execution root at 16.
fn view_of(row: &PgRow, process: &ProcessId, exec: &ExecKey) -> Result<ProcessView, DurableError> {
    let registry = match (
        get::<Option<String>>(row, 13)?,
        get::<Option<String>>(row, 14)?,
    ) {
        (Some(record), _) => ProcessRegistryRow::Retained(record),
        (None, Some(terminal_label)) => ProcessRegistryRow::Pruned {
            terminal_label,
            pruned_at_ms: get(row, 15)?,
        },
        (None, None) => ProcessRegistryRow::Absent,
    };
    let root = match get::<Option<i64>>(row, 16)? {
        None => None,
        Some(rev) => {
            let root: String = crate::keys::get(row, 17).map_err(sqlx_failure)?;
            let commit_id: Vec<u8> = row.try_get(19).map_err(sqlx_failure)?;
            Some(snapshots::row_of(
                exec.clone(),
                rev,
                &root,
                get(row, 18)?,
                &commit_id,
            )?)
        }
    };
    Ok(ProcessView {
        row: processes::actor_row(row, 6, process)?,
        registry,
        root,
    })
}

impl PostgresDurableStore {
    pub(super) async fn open_process_view(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        max_mail_bytes: u64,
    ) -> Result<(ActorTx, ProcessView), DurableError> {
        if actor.kind() != lash_durable::ActorKind::Process {
            return Err(corrupt("process actor", actor.as_str()));
        }
        let process =
            ProcessId::parse(actor.id()).map_err(|_| corrupt("process id", actor.id()))?;
        let exec = ExecKey::Process(process.clone());
        let stored = exec.stored();
        let row = self
            .read(|connection| {
                let stored = stored.clone();
                Box::pin(async move {
                    crate::observed_sql::query(SQL.open.sql())
                        .bind(actor.as_str())
                        .bind(actor.id())
                        .bind(stored)
                        .bind(i64::try_from(max_mail_bytes).unwrap_or(i64::MAX))
                        .bind(OpenedActor::MAIL_PAGE_ROWS)
                        .fetch_optional(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(row) = row else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let state: String = get(&row, 1)?;
        let revision = lash_durable::fenced_read::validate_authority(
            actor,
            epoch,
            None,
            Some((Epoch(get(&row, 0)?), &state, StateRevision(get(&row, 2)?))),
        )?;
        let view = view_of(&row, &process, &exec)?;
        let opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision,
            acked: MailSeq(get(&row, 3)?),
            seen: MailSeq(get(&row, 4)?),
            mail: Vec::new(),
            close_requested: false,
            at: self.instant_or(get(&row, 5)?)?,
            turn_cancel: None,
        }
        .with_mail_page(&get::<String>(&row, 20)?, max_mail_bytes)?;
        Ok((ActorTx::opened(opened), view))
    }

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
        let row = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query(SQL.frontier.sql())
                        .bind(actor.as_str())
                        .bind(actor.id())
                        .bind(i64::try_from(max_mail_bytes).unwrap_or(i64::MAX))
                        .bind(OpenedActor::MAIL_PAGE_ROWS)
                        .fetch_optional(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(row) = row else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let state: String = get(&row, 1)?;
        let revision = lash_durable::fenced_read::validate_authority(
            actor,
            epoch,
            Some(expected),
            Some((Epoch(get(&row, 0)?), &state, StateRevision(get(&row, 2)?))),
        )?;
        let cancel = get::<Option<String>>(&row, 5)?
            .filter(|json| json != "null")
            .map(|json| serde_json::from_str(&json).map_err(|_| corrupt("process cancel", &json)))
            .transpose()?;
        let frontier = lash_durable::domain::ProcessFrontier {
            cancel,
            last_event_sequence: super::integer::<u64>(get::<Option<i64>>(&row, 6)?.unwrap_or(0))?,
            updated_at_ms: super::integer::<u64>(get::<Option<i64>>(&row, 7)?.unwrap_or(0))?,
            external_ref: get::<Option<String>>(&row, 8)?.filter(|json| json != "null"),
        };
        let opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision,
            acked: MailSeq(get(&row, 3)?),
            seen: MailSeq(get(&row, 4)?),
            mail: Vec::new(),
            close_requested: false,
            at: self.instant_or(get(&row, 10)?)?,
            turn_cancel: None,
        }
        .with_mail_page(&get::<String>(&row, 9)?, max_mail_bytes)?;
        Ok((ActorTx::opened(opened), frontier))
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
        refreshed(
            &mut view,
            get::<Option<String>>(&row, 0)?,
            get::<i64>(&row, 1)?,
            get::<i64>(&row, 2)?,
            get::<Option<String>>(&row, 3)?,
        )?;
    }
    Ok(Some(view))
}

/// `view` with the frontier its process row holds: its cancel request and
/// external reference as JSON text, its last event and its update time.
fn refreshed(
    view: &mut ProcessView,
    cancel: Option<String>,
    last_event_sequence: i64,
    updated_at_ms: i64,
    external_ref: Option<String>,
) -> Result<(), DurableError> {
    let cancel = cancel
        .filter(|json| json != "null")
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|error| corrupt("process frontier", &error.to_string()))
        })
        .transpose()?;
    let frontier = lash_durable::domain::ProcessFrontier {
        cancel,
        last_event_sequence: super::integer::<u64>(last_event_sequence)?,
        updated_at_ms: super::integer::<u64>(updated_at_ms)?,
        external_ref: external_ref.filter(|json| json != "null"),
    };
    lash_execution_contracts::process::registry_transitions::refresh_frontier(view, frontier)
        .map_err(|error| corrupt("process frontier", &error.to_string()))
}

/// A one-call owner commit's read of the frontier of the process whose
/// view `tx` carries, before any of its writes: its row joins the call's
/// answer as `process_frontier` (FIG-7585).
pub(super) static FRONTIER: super::steps::Entry = super::steps::Entry::new(
    || &SQL.control_answered,
    &[super::steps::Ty::Text],
    super::steps::Shape::Select,
)
.collecting(super::steps::Collect::Answer("process_frontier"));

/// The frontier step of `tx`, when it carries a process view.
pub(super) fn frontier_step(tx: &ActorTx) -> Option<super::steps::Step> {
    tx.process_preimage()?;
    Some(super::steps::Step::new(
        &FRONTIER,
        vec![super::steps::Value::text(tx.actor().id())],
        None,
    ))
}

/// The process views a one-call commit of `tx` leaves: the view it
/// carried, refreshed with the frontier `answered` (null when its process
/// row is gone), then each of its writes applied, as the explicit form
/// derives them.
pub(super) fn postimages(
    tx: &ActorTx,
    answered: &serde_json::Value,
) -> Result<Vec<ProcessView>, DurableError> {
    let Some(mut view) = tx.process_preimage().cloned() else {
        return Ok(Vec::new());
    };
    let corrupt_answer = || corrupt("owner commit process frontier", &answered.to_string());
    if !answered.is_null() {
        let text = |name: &str| answered[name].as_str().map(str::to_owned);
        let number = |name: &str| answered[name].as_i64().ok_or_else(corrupt_answer);
        refreshed(
            &mut view,
            text("cancel"),
            number("last_event_sequence")?,
            number("updated_at_ms")?,
            text("external_ref"),
        )?;
    }
    let views = lash_durable::domain::processes::ProcessPostimages::from_view(Some(view));
    for write in tx.domain() {
        views.applied(write, tx.epoch());
    }
    Ok(views.into_views())
}
