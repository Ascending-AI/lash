//! A process owner's fenced open with the decision view of its pass, one
//! statement and so one snapshot
//! ([`DurableStore::open_process`](lash_durable::DurableStore::open_process)).
use super::*;
use lash_durable::domain::{ProcessRegistryRow, ProcessView};
use lash_vocabulary::identity::ProcessId;

lash_store_sql::statements! {
    /// The process owner's open, as SQLite reads it: the store's clock is
    /// the injected one, read beside the statement.
    struct ProcessViewStatements @ "durable_sqlite_process_view" {
        /// Process actor `?1`'s epoch, state, revision and mailbox
        /// positions and one bounded page, with process `?2`'s
        /// actor row, its registry record (or the tombstone of a pruned
        /// one) and execution root `?3`'s row.
        open = lash_store_sql::mailbox_query!("?4", "?5", "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    p.state_rev, p.driver_json, p.cancel_requested_at_ms, p.status,
                    p.cascade_cursor, p.written_epoch, p.published_event_sequence,
                    (substr(p.start_json, 1, length(p.start_json) - 1) || ',' || substr(p.execution_json, 2, length(p.execution_json) - 2) || ',' || substr(p.lifecycle_json, 2)),
                    t.terminal_label, t.pruned_at_ms,
                    s.rev, s.root_ref, s.written_epoch, s.commit_id,
                    (SELECT json_group_array(json_array(seq, kind, body, appended_at_ms, body_bytes)) FROM mail_page)
             FROM actors a
             LEFT JOIN processes p ON p.process_id = ?2
             LEFT JOIN process_tombstones t ON t.process_id = ?2
             LEFT JOIN exec_snapshots s ON s.exec_key = ?3
             WHERE a.actor_key = ?1");
        control = "SELECT json_extract(lifecycle_json, '$.cancel_request'),
                    json_extract(lifecycle_json, '$.last_event_sequence'),
                    json_extract(lifecycle_json, '$.updated_at_ms'),
                    json_extract(lifecycle_json, '$.external_ref')
             FROM processes WHERE process_id = ?1";
        /// Authority, bounded pending mail and current cancellation only.
        frontier = lash_store_sql::mailbox_query!("?3", "?4", "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    json_extract(p.lifecycle_json, '$.cancel_request'),
                    json_extract(p.lifecycle_json, '$.last_event_sequence'),
                    json_extract(p.lifecycle_json, '$.updated_at_ms'),
                    json_extract(p.lifecycle_json, '$.external_ref'),
                    (SELECT json_group_array(json_array(seq, kind, body, appended_at_ms, body_bytes)) FROM mail_page)
             FROM actors a
             LEFT JOIN processes p ON p.process_id = ?2
             WHERE a.actor_key = ?1");
    }
}

static SQL: LazyLock<ProcessViewStatements> =
    LazyLock::new(|| ProcessViewStatements::render(crate::schema_layout::MAIN));

/// The process row starts at 5, the registry at 12 and the execution root at 15.
fn view_of(
    row: &rusqlite::Row<'_>,
    process: &ProcessId,
    exec: &ExecKey,
) -> rusqlite::Result<Result<ProcessView, DurableError>> {
    let registry = match (
        row.get::<_, Option<String>>(12)?,
        row.get::<_, Option<String>>(13)?,
    ) {
        (Some(record), _) => ProcessRegistryRow::Retained(record),
        (None, Some(terminal_label)) => ProcessRegistryRow::Pruned {
            terminal_label,
            pruned_at_ms: row.get(14)?,
        },
        (None, None) => ProcessRegistryRow::Absent,
    };
    let root = match row.get::<_, Option<i64>>(15)? {
        None => None,
        Some(rev) => {
            let root = crate::codec::hash_column(row, 16)?;
            match snapshots::row_of(
                exec.clone(),
                rev,
                &root,
                row.get(17)?,
                &row.get::<_, Vec<u8>>(18)?,
            )? {
                Ok(root) => Some(root),
                Err(error) => return Ok(Err(error)),
            }
        }
    };
    Ok(Ok(ProcessView {
        row: processes::actor_row(row, 5, process)?,
        registry,
        root,
    }))
}

impl SqliteDurableStore {
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
        let actor = actor.clone();
        let at = self.instant()?;
        self.query(move |connection| {
            let mut statement = connection.prepare_cached(SQL.open.sql())?;
            let mut rows = statement.query(rusqlite::params![
                actor.as_str(),
                process.as_str(),
                exec.stored(),
                i64::try_from(max_mail_bytes).unwrap_or(i64::MAX),
                OpenedActor::MAIL_PAGE_ROWS
            ])?;
            let Some(row) = rows.next()? else {
                return Ok(Err(DurableError::OwnershipLost(Fenced {
                    actor,
                    held: epoch,
                    current: None,
                })));
            };
            let state: String = row.get(1)?;
            let revision = match lash_durable::fenced_read::validate_authority(
                &actor,
                epoch,
                None,
                Some((Epoch(row.get(0)?), &state, StateRevision(row.get(2)?))),
            ) {
                Ok(revision) => revision,
                Err(error) => return Ok(Err(error)),
            };
            let view = match view_of(row, &process, &exec)? {
                Ok(view) => view,
                Err(error) => return Ok(Err(error)),
            };
            let opened = OpenedActor {
                actor,
                epoch,
                revision,
                acked: MailSeq(row.get(3)?),
                seen: MailSeq(row.get(4)?),
                mail: Vec::new(),
                close_requested: false,
                at,
                turn_cancel: None,
            };
            Ok(opened
                .with_mail_page(row.get_ref(19)?.as_str()?, max_mail_bytes)
                .map(|opened| (ActorTx::opened(opened), view)))
        })
        .await
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
        let actor = actor.clone();
        let at = self.instant()?;
        self.query(move |connection| {
            let mut statement = connection.prepare_cached(SQL.frontier.sql())?;
            let mut rows = statement.query(rusqlite::params![
                actor.as_str(),
                actor.id(),
                i64::try_from(max_mail_bytes).unwrap_or(i64::MAX),
                OpenedActor::MAIL_PAGE_ROWS
            ])?;
            let Some(row) = rows.next()? else {
                return Ok(Err(DurableError::OwnershipLost(Fenced {
                    actor,
                    held: epoch,
                    current: None,
                })));
            };
            let state: String = row.get(1)?;
            let revision = match lash_durable::fenced_read::validate_authority(
                &actor,
                epoch,
                Some(expected),
                Some((Epoch(row.get(0)?), &state, StateRevision(row.get(2)?))),
            ) {
                Ok(revision) => revision,
                Err(error) => return Ok(Err(error)),
            };
            let cancel = match row.get::<_, Option<String>>(5)? {
                Some(json) => match serde_json::from_str(&json) {
                    Ok(cancel) => Some(cancel),
                    Err(_) => return Ok(Err(corrupt("process cancel", &json))),
                },
                None => None,
            };
            let frontier = lash_durable::domain::ProcessFrontier {
                cancel,
                last_event_sequence: row.get::<_, Option<u64>>(6)?.unwrap_or(0),
                updated_at_ms: row.get::<_, Option<u64>>(7)?.unwrap_or(0),
                external_ref: row.get(8)?,
            };
            let opened = OpenedActor {
                actor,
                epoch,
                revision,
                acked: MailSeq(row.get(3)?),
                seen: MailSeq(row.get(4)?),
                mail: Vec::new(),
                close_requested: false,
                at,
                turn_cancel: None,
            };
            Ok(opened
                .with_mail_page(row.get_ref(9)?.as_str()?, max_mail_bytes)
                .map(|opened| (ActorTx::opened(opened), frontier)))
        })
        .await
    }
}

pub(super) fn refresh_preimage(
    tx: &rusqlite::Connection,
    write: &ActorTx,
) -> rusqlite::Result<Result<Option<ProcessView>, DurableError>> {
    refresh_view(tx, write.actor(), write.process_preimage().cloned())
}

pub(super) fn refresh_view(
    tx: &rusqlite::Connection,
    actor: &ActorKey,
    view: Option<ProcessView>,
) -> rusqlite::Result<Result<Option<ProcessView>, DurableError>> {
    let Some(mut view) = view else {
        return Ok(Ok(None));
    };
    let frontier = tx
        .prepare_cached(SQL.control.sql())?
        .query_row([actor.id()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .optional()?;
    if let Some((cancel, last_event_sequence, updated_at_ms, external_ref)) = frontier {
        let cancel = match cancel.map(|json| serde_json::from_str(&json)).transpose() {
            Ok(cancel) => cancel,
            Err(error) => return Ok(Err(corrupt("process frontier", &error.to_string()))),
        };
        let frontier = lash_durable::domain::ProcessFrontier {
            cancel,
            last_event_sequence,
            updated_at_ms,
            external_ref,
        };
        if let Err(error) =
            lash_execution_contracts::process::registry_transitions::refresh_frontier(
                &mut view, frontier,
            )
        {
            return Ok(Err(corrupt("process frontier", &error.to_string())));
        }
    }
    Ok(Ok(Some(view)))
}
