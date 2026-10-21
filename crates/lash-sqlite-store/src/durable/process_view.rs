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
        /// positions, one row per pending mail, each with process `?2`'s
        /// actor row, its registry record (or the tombstone of a pruned
        /// one) and execution root `?3`'s row.
        open = "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms,
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
    }
}

static SQL: LazyLock<ProcessViewStatements> =
    LazyLock::new(|| ProcessViewStatements::render(crate::schema_layout::MAIN));

/// The view's columns in an open row: the process row from 9, the registry
/// record and tombstone from 16, the execution root from 19.
fn view_of(
    row: &rusqlite::Row<'_>,
    process: &ProcessId,
    exec: &ExecKey,
) -> rusqlite::Result<Result<ProcessView, DurableError>> {
    let registry = match (
        row.get::<_, Option<String>>(16)?,
        row.get::<_, Option<String>>(17)?,
    ) {
        (Some(record), _) => ProcessRegistryRow::Retained(record),
        (None, Some(terminal_label)) => ProcessRegistryRow::Pruned {
            terminal_label,
            pruned_at_ms: row.get(18)?,
        },
        (None, None) => ProcessRegistryRow::Absent,
    };
    let root = match row.get::<_, Option<i64>>(19)? {
        None => None,
        Some(rev) => {
            let root = crate::codec::hash_column(row, 20)?;
            match snapshots::row_of(
                exec.clone(),
                rev,
                &root,
                row.get(21)?,
                &row.get::<_, Vec<u8>>(22)?,
            )? {
                Ok(root) => Some(root),
                Err(error) => return Ok(Err(error)),
            }
        }
    };
    Ok(Ok(ProcessView {
        row: processes::actor_row(row, 9, process)?,
        registry,
        root,
    }))
}

impl SqliteDurableStore {
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
        let actor = actor.clone();
        let at = self.instant()?;
        self.query(move |connection| {
            let mut statement = connection.prepare_cached(SQL.open.sql())?;
            let mut rows = statement.query(rusqlite::params![
                actor.as_str(),
                process.as_str(),
                exec.stored()
            ])?;
            let mut opened: Option<(OpenedActor, ProcessView)> = None;
            while let Some(row) = rows.next()? {
                let (opened, _) = match &mut opened {
                    Some(opened) => opened,
                    None => {
                        let current = Epoch(row.get(0)?);
                        let state: String = row.get(1)?;
                        if current != epoch || state != ActorState::Owned.as_str() {
                            return Ok(Err(DurableError::OwnershipLost(Fenced {
                                actor: actor.clone(),
                                held: epoch,
                                current: Some(current),
                            })));
                        }
                        let view = match view_of(row, &process, &exec)? {
                            Ok(view) => view,
                            Err(error) => return Ok(Err(error)),
                        };
                        opened.insert((
                            OpenedActor {
                                actor: actor.clone(),
                                epoch,
                                revision: StateRevision(row.get(2)?),
                                acked: MailSeq(row.get(3)?),
                                seen: MailSeq(row.get(4)?),
                                mail: Vec::new(),
                                at,
                                turn_cancel: None,
                            },
                            view,
                        ))
                    }
                };
                if let Some(seq) = row.get::<_, Option<i64>>(5)?
                    && seq <= opened.seen.0
                {
                    opened.mail.push(Mail {
                        seq: MailSeq(seq),
                        kind: MailKind::new(row.get::<_, String>(6)?),
                        body: row.get(7)?,
                        appended_at: DurableInstant(row.get(8)?),
                    });
                }
            }
            Ok(match opened {
                Some((opened, view)) => Ok((ActorTx::opened(opened), view)),
                None => Err(DurableError::OwnershipLost(Fenced {
                    actor,
                    held: epoch,
                    current: None,
                })),
            })
        })
        .await
    }
}
