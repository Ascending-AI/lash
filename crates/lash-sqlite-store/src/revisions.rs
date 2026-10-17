//! Retained revisions and pins on the SQLite durable core (FIG-4731).
//!
//! The name of a state is `(session_id, head_revision)`. Every head
//! publication records its revision here, in the publishing transaction, and
//! `session_revisions` membership is the retained-revisions relation every
//! reclaimer roots what it keeps in. A pin names a target and resolves to a
//! revision by query. The `*_conn` helpers are synchronous and run inside the
//! caller's transaction on the connection thread.

use super::*;
use crate::session_sql::session_sql;
use lash_core_execution::store_backend_support::TargetResolution;
use lash_core_execution::{RetainedRevision, Retention, Target};

/// Record revision `head_revision` of `session_id` in the transaction that
/// publishes it. `head_json` is the head document the revision was published
/// with, including config commands applied since its frame opened.
pub(crate) fn record_revision_conn(
    tx: &Connection,
    session_id: &SessionId,
    head_revision: i64,
    leaf_node_id: Option<&str>,
    checkpoint_ref: Option<&str>,
    head_json: &str,
) -> Result<(), StoreError> {
    let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
        lash_core_store::checkpoint_graph::EncodeBudget::new(
            lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
        ),
    );
    let value: serde_json::Value =
        serde_json::from_str(head_json).map_err(|error| StoreError::Backend(error.to_string()))?;
    let root = lash_core_store::checkpoint_graph::config::stage(&mut staging, &value).map_err(
        |message| StoreError::StoredDataCorrupt {
            record_kind: "SessionHeadMeta",
            message,
        },
    )?;
    let config_ref = root.blob_ref();
    let held: Option<String> = tx
        .query_row(
            session_sql().revisions.select_config_ref.sql(),
            [session_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    if held.as_deref() != Some(config_ref.as_str()) {
        crate::durable::install_staged(tx, staging.nodes())
            .map_err(sqlite_error)?
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: error.to_string(),
            })?;
    }
    crate::conn::cached_execute(
        tx,
        session_sql().revisions.insert.sql(),
        params![
            session_id.as_str(),
            head_revision,
            leaf_node_id,
            checkpoint_ref,
            config_ref.as_str(),
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Number a newly committed terminal once, then release revisions outside its window,
/// at `now`. A staged revision becomes terminal in the same publishing transaction; a
/// replay cannot advance the ordinal again.
pub(crate) fn record_terminal_conn(
    tx: &Connection,
    session: &SessionId,
    revision: u64,
    now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    let ordinal: Option<i64> = tx
        .query_row(
            session_sql().revisions.mark_terminal.sql(),
            params![
                session.as_str(),
                i64::try_from(revision).map_err(|error| StoreError::Backend(error.to_string()))?
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    if let Some(ordinal) = ordinal {
        crate::conn::cached_execute(
            tx,
            session_sql().revisions.advance_terminal.sql(),
            params![session.as_str(), ordinal],
        )
        .map_err(sqlite_error)?;
    }
    if retention_conn(tx, session)?.is_some_and(Retention::releases_at_commit) {
        release_unretained_conn(tx, false, Some(session), now)?;
    }
    Ok(())
}

/// Release every revision nothing retains, of `session_id` or of every
/// session, retire the ancestry only a released revision kept alive, and
/// hold the checkpoint and config roots they published for retirement at
/// `now` (FIG-5949, FIG-6423): a checkpoint root reaches its session's
/// heap, which retirement steps delete in bounded transactions, never this
/// one. `collecting` is a host collection, which ends `until_gc`'s hold.
pub(crate) fn release_unretained_conn(
    tx: &Connection,
    collecting: bool,
    session_id: Option<&SessionId>,
    now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    let released = {
        let mut stmt = tx
            .prepare_cached(session_sql().revisions.prune_unretained.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![i64::from(collecting), session_id.map(SessionId::as_str)],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    release_revision_material_conn(tx, session_id, now)?;
    let mut roots = std::collections::BTreeSet::new();
    for (leaf_node_id, checkpoint_ref, config_ref) in released {
        if let Some(leaf_node_id) = leaf_node_id {
            persistence::retire_unreachable_ancestry_conn(tx, &leaf_node_id)?;
        }
        roots.extend(checkpoint_ref);
        roots.extend(config_ref);
    }
    crate::durable::retire_roots(tx, &roots, now)
        .map(drop)
        .map_err(sqlite_error)
}

/// Release one page of per-revision material in the revision prune's
/// transaction. A session also lets go of each queued attachment whose last
/// hold ended (FIG-6611).
pub(crate) fn release_revision_material_conn(
    tx: &Connection,
    session_id: Option<&SessionId>,
    _now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    crate::durable::release_material_batch(tx, session_id)
        .map(drop)
        .map_err(sqlite_error)?;
    crate::attachments::release_session_attachments_conn(
        tx,
        session_id,
        i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH),
    )
    .map(drop)
}

/// The retention policy recorded for `session_id`, or `None` for a session
/// with no metadata row: it recorded no policy, and nothing stands in for
/// one.
pub(crate) fn retention_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<Retention>, StoreError> {
    conn.query_row(
        session_sql().meta.select_retention.sql(),
        params![session_id.as_str()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
    )
    .optional()
    .map_err(sqlite_error)?
    .map(|(kind, last_turns)| Retention::from_stored(&kind, last_turns))
    .transpose()
}

/// Refuse a session the catalog does not hold: a deletion tombstone answers
/// [`StoreError::SessionDeleted`], an id it never held
/// [`StoreError::SessionNotFound`].
fn require_session_conn(conn: &Connection, session_id: &SessionId) -> Result<(), StoreError> {
    persistence::ensure_session_not_deleted_conn(conn, session_id)?;
    let exists = conn
        .query_row(
            session_sql().meta_sqlite.exists_materialized.sql(),
            params![session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(StoreError::SessionNotFound {
            session_id: session_id.clone(),
        })
    }
}

/// The published pointer of `session_id`: its head.
fn head_revision_conn(conn: &Connection, session_id: &SessionId) -> Result<u64, StoreError> {
    let head = conn
        .query_row(
            session_sql().revisions.select_head_revision.sql(),
            params![session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .unwrap_or(0);
    u64_from_sql("SessionRevision", "head_revision", head).map_err(sqlite_error)
}

/// What the rows `target` resolves through say about it.
fn resolve_conn(
    conn: &Connection,
    session_id: &SessionId,
    target: &Target,
) -> Result<TargetResolution, StoreError> {
    let of_run = |run: &lash_core_execution::TurnId| {
        crate::session_runs::run_terminal_conn(conn, session_id, run)
            .map(|terminal| TargetResolution::of_run(terminal.as_ref()))
    };
    match target {
        Target::Revision(revision) => Ok(TargetResolution::of_revision(
            *revision,
            head_revision_conn(conn, session_id)?,
        )),
        Target::Turn(run) => of_run(run),
        Target::Input(input) => {
            if let Some(run) = crate::session_runs::run_binding_conn(conn, session_id, input)? {
                return of_run(&run);
            }
            let state: Option<String> = conn
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs
                        .select_state_by_id
                        .sql(),
                    params![session_id.as_str(), input.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            TargetResolution::of_unbound_input(state.as_deref())
        }
    }
}

/// One stored revision row.
struct RevisionRow {
    head_revision: u64,
    leaf_node_id: Option<String>,
    checkpoint_ref: Option<String>,
    config_ref: Option<String>,
}

impl RevisionRow {
    /// The configuration a fork of this revision records: the one the head
    /// recorded when the revision was published, so a config command applied
    /// since the frame opened is part of what a fork copies. `fleet` is the
    /// store's recorded `F`: the head document admits the `[N-1, N]` reader
    /// window `F` names.
    fn config(
        &self,
        conn: &Connection,
        fleet: lash_core_execution::FleetFormat,
    ) -> Result<lash_core_execution::PersistedSessionConfig, StoreError> {
        let payload = crate::codec::load_config_conn(conn, self.config_ref.as_deref(), fleet)?;
        Ok(payload.config)
    }
}

/// Revision `head_revision` of `session_id`, if it is retained.
fn revision_row_conn(
    conn: &Connection,
    session_id: &SessionId,
    head_revision: u64,
) -> Result<Option<RevisionRow>, StoreError> {
    let Ok(sql_revision) = i64::try_from(head_revision) else {
        return Ok(None);
    };
    conn.query_row(
        session_sql().revisions.select.sql(),
        params![session_id.as_str(), sql_revision],
        |row| {
            Ok(RevisionRow {
                head_revision,
                leaf_node_id: row.get(0)?,
                checkpoint_ref: row.get(1)?,
                config_ref: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(sqlite_error)
}

/// Every pin of `session_id` with the revision it resolves to now, if any.
fn resolved_pins_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Vec<(Target, Option<u64>)>, StoreError> {
    let pins = {
        let mut stmt = conn
            .prepare_cached(session_sql().pins.select_by_session.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    pins.into_iter()
        .map(|(kind, id)| {
            let target = Target::from_stored(&kind, &id)?;
            let revision = match resolve_conn(conn, session_id, &target)? {
                TargetResolution::Revision(revision) => Some(revision),
                TargetResolution::Pending | TargetResolution::Unavailable => None,
            };
            Ok((target, revision))
        })
        .collect()
}

fn retained_revision(
    conn: &Connection,
    session_id: &SessionId,
    row: RevisionRow,
    head_revision: u64,
    pins: &[(Target, Option<u64>)],
    fleet: lash_core_execution::FleetFormat,
) -> Result<RetainedRevision, StoreError> {
    Ok(RetainedRevision {
        session_id: session_id.clone(),
        head_revision: row.head_revision,
        config: row.config(conn, fleet)?,
        head: row.head_revision == head_revision,
        pinned_by: pins
            .iter()
            .filter(|(_, revision)| *revision == Some(row.head_revision))
            .map(|(target, _)| target.clone())
            .collect(),
        leaf_node_id: row.leaf_node_id.map(TryInto::try_into).transpose()?,
        checkpoint_ref: row.checkpoint_ref.map(BlobRef),
    })
}

/// The retained revision `target` of `session_id` names, or its typed
/// refusal.
pub(crate) fn resolve_target_conn(
    conn: &Connection,
    session_id: &SessionId,
    target: &Target,
    fleet: lash_core_execution::FleetFormat,
) -> Result<RetainedRevision, StoreError> {
    require_session_conn(conn, session_id)?;
    let revision = resolve_conn(conn, session_id, target)?.revision(session_id, target)?;
    let row = revision_row_conn(conn, session_id, revision)?.ok_or_else(|| {
        StoreError::ForkTargetPruned {
            session_id: session_id.clone(),
            target: target.clone(),
        }
    })?;
    let head_revision = head_revision_conn(conn, session_id)?;
    let pins = resolved_pins_conn(conn, session_id)?;
    retained_revision(conn, session_id, row, head_revision, &pins, fleet)
}

/// Every retained revision of `session_id`, oldest first.
pub(crate) fn revisions_conn(
    conn: &Connection,
    session_id: &SessionId,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Vec<RetainedRevision>, StoreError> {
    require_session_conn(conn, session_id)?;
    let rows = {
        let mut stmt = conn
            .prepare_cached(session_sql().revisions.select_by_session.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id.as_str()], |row| {
                Ok(RevisionRow {
                    head_revision: u64_from_sql(
                        "SessionRevision",
                        "head_revision",
                        row.get::<_, i64>(0)?,
                    )?,
                    leaf_node_id: row.get(1)?,
                    checkpoint_ref: row.get(2)?,
                    config_ref: row.get(3)?,
                })
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let head_revision = head_revision_conn(conn, session_id)?;
    let pins = resolved_pins_conn(conn, session_id)?;
    rows.into_iter()
        .map(|row| retained_revision(conn, session_id, row, head_revision, &pins, fleet))
        .collect()
}

/// Pin `target` of `session_id` in the caller's transaction. Idempotent.
pub(crate) fn pin_conn(
    tx: &Connection,
    session_id: &SessionId,
    target: &Target,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        session_sql().pins.insert.sql(),
        params![session_id.as_str(), target.kind(), target.id()],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

impl SqliteStore {
    /// The instant a hold this store's catalog writes is queued at: the
    /// clock the store set's durable core reads.
    pub(crate) fn retirement_instant(&self) -> lash_durable::DurableInstant {
        lash_durable::DurableInstant(crate::clamp_epoch_ms(self.clock.timestamp_ms()))
    }

    /// Run `f` in one write transaction, rolling back on a store refusal.
    async fn revision_write<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.conn
            .write_flow(move |tx| {
                Ok(match f(tx) {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    /// Run `f` in one read transaction with the store's recorded fleet format.
    async fn revision_read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, lash_core_execution::FleetFormat) -> Result<T, StoreError>
            + Send
            + 'static,
    {
        self.read_connection()
            .read(move |tx| {
                let fleet =
                    crate::compat::read_recorded(tx, lash_core_execution::FleetFormat::writable())?;
                Ok(f(tx, fleet))
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(crate) async fn resolve_target_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<RetainedRevision, StoreError> {
        let (session_id, target) = (session_id.clone(), target.clone());
        self.revision_read(move |conn, fleet| {
            resolve_target_conn(conn, &session_id, &target, fleet)
        })
        .await
    }

    pub(crate) async fn revisions_in_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RetainedRevision>, StoreError> {
        let session_id = session_id.clone();
        self.revision_read(move |conn, fleet| revisions_conn(conn, &session_id, fleet))
            .await
    }

    pub(crate) async fn pin_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<(), StoreError> {
        let (session_id, target) = (session_id.clone(), target.clone());
        self.revision_write(move |tx| {
            require_session_conn(tx, &session_id)?;
            pin_conn(tx, &session_id, &target)
        })
        .await
    }

    pub(crate) async fn unpin_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<(), StoreError> {
        let (session_id, target) = (session_id.clone(), target.clone());
        let now = self.retirement_instant();
        self.revision_write(move |tx| {
            crate::conn::cached_execute(
                tx,
                session_sql().pins.delete.sql(),
                params![session_id.as_str(), target.kind(), target.id()],
            )
            .map_err(sqlite_error)?;
            // A policy that releases as the session commits releases what
            // this pin alone held now; `until_gc` leaves it to the host's
            // collection, and so does a session that recorded no policy.
            if retention_conn(tx, &session_id)?.is_some_and(Retention::releases_at_commit) {
                release_unretained_conn(tx, false, Some(&session_id), now)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn retention_in_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Retention, StoreError> {
        let session_id = session_id.clone();
        self.revision_read(move |conn, _| {
            require_session_conn(conn, &session_id)?;
            retention_conn(conn, &session_id)?.ok_or_else(|| StoreError::SessionNotFound {
                session_id: session_id.clone(),
            })
        })
        .await
    }

    pub(crate) async fn set_retention_in_catalog(
        &self,
        session_id: &SessionId,
        retention: Retention,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        self.revision_write(move |tx| {
            require_session_conn(tx, &session_id)?;
            crate::conn::cached_execute(
                tx,
                session_sql().meta.set_retention.sql(),
                params![
                    session_id.as_str(),
                    retention.kind(),
                    retention.last_turns().map(i64::from)
                ],
            )
            .map_err(sqlite_error)?;
            Ok(())
        })
        .await
    }
}
