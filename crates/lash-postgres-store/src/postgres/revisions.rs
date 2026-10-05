//! Retained revisions and pins on PostgreSQL (FIG-4731).
//!
//! The name of a state is `(session_id, head_revision)`. Every head
//! publication records its revision here, in the publishing transaction, and
//! `lash_session_revisions` membership is the retained-revisions relation
//! every reclaimer roots what it keeps in. A pin names a target and resolves
//! to a revision by query.

use super::*;
use crate::session_sql::session_sql;
use lash_core_execution::store_backend_support::TargetResolution;
use lash_core_execution::{RetainedRevision, Retention, Target};

type Tx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// Record revision `head_revision` of `session_id` in the transaction that
/// publishes it. `head_json` is the head document the revision was published
/// with, including config commands applied since its frame opened.
pub(crate) async fn record_revision_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
    head_revision: i64,
    leaf_node_id: Option<&str>,
    checkpoint_ref: Option<&str>,
    head_json: &str,
) -> Result<(), StoreError> {
    sqlx::query(session_sql().revisions.insert.sql())
        .bind(session_id.as_str())
        .bind(head_revision)
        .bind(leaf_node_id)
        .bind(checkpoint_ref)
        .bind(head_json)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Release every revision nothing retains, of `session_id` or of every
/// session, and retire the ancestry only a released revision kept alive.
/// `collecting` is a host collection, which ends `until_gc`'s hold. Answers
/// how many revisions were released.
///
/// A fork holds its source revision's row share-locked until its own head
/// commits, so a release that raced one waits and then retires nothing the
/// fork now runs.
pub(crate) async fn release_unretained_tx(
    tx: &mut Tx<'_>,
    collecting: bool,
    session_id: Option<&SessionId>,
) -> Result<usize, StoreError> {
    let released = sqlx::query(session_sql().revisions.prune_unretained.sql())
        .bind(i64::from(collecting))
        .bind(session_id.map(SessionId::as_str))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    // Retirement takes node locks in id order across the whole release, the
    // order every other multi-node writer uses.
    let mut leaves = released
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>(1))
        .collect::<Vec<_>>();
    leaves.sort();
    leaves.dedup();
    for leaf_node_id in &leaves {
        crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, leaf_node_id).await?;
    }
    Ok(released.len())
}

/// The retention policy of `session_id`; a session with no metadata row
/// keeps the default.
pub(crate) async fn retention_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
) -> Result<Retention, StoreError> {
    sqlx::query_as::<_, (String, Option<i64>)>(session_sql().meta.select_retention.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .map_or(Ok(Retention::default()), |(kind, last_turns)| {
            Retention::from_stored(&kind, last_turns)
        })
}

/// Refuse a session the catalog does not hold: a deletion tombstone answers
/// [`StoreError::SessionDeleted`], an id it never held
/// [`StoreError::SessionNotFound`].
async fn require_session_tx(tx: &mut Tx<'_>, session_id: &SessionId) -> Result<(), StoreError> {
    let (exists, deleted) = sqlx::query_as::<_, (bool, bool)>(
        session_sql()
            .meta_postgres
            .exists_materialized_or_deleted
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: session_id.clone(),
        })
    } else if exists {
        Ok(())
    } else {
        Err(StoreError::SessionNotFound {
            session_id: session_id.clone(),
        })
    }
}

/// The published pointer of `session_id`: its head.
async fn head_revision_tx(tx: &mut Tx<'_>, session_id: &SessionId) -> Result<u64, StoreError> {
    let head: Option<i64> = sqlx::query_scalar(session_sql().revisions.select_head_revision.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    u64_from_sql("SessionRevision", "head_revision", head.unwrap_or(0))
}

/// What the rows `target` resolves through say about it.
async fn resolve_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
    target: &Target,
) -> Result<TargetResolution, StoreError> {
    let run = match target {
        Target::Revision(revision) => {
            return Ok(TargetResolution::of_revision(
                *revision,
                head_revision_tx(tx, session_id).await?,
            ));
        }
        Target::Turn(run) => run.clone(),
        Target::Input(input) => {
            match crate::session_runs::run_binding_conn(tx, session_id, input).await? {
                Some(run) => run,
                None => {
                    let state: Option<String> = sqlx::query_scalar(
                        crate::turn_ingress::turn_ingress_sql()
                            .pending_inputs
                            .select_state_by_id
                            .sql(),
                    )
                    .bind(session_id.as_str())
                    .bind(input.as_str())
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?;
                    return TargetResolution::of_unbound_input(state.as_deref());
                }
            }
        }
    };
    let terminal = crate::session_runs::run_terminal_conn(tx, session_id, &run).await?;
    Ok(TargetResolution::of_run(terminal.as_ref()))
}

/// One stored revision row.
struct RevisionRow {
    head_revision: u64,
    leaf_node_id: Option<String>,
    checkpoint_ref: Option<String>,
    head_json: String,
}

impl RevisionRow {
    /// The configuration a fork of this revision records: the one the head
    /// recorded when the revision was published, so a config command applied
    /// since the frame opened is part of what a fork copies. `fleet` is the
    /// store's recorded `F`: the head document admits the `[N-1, N]` reader
    /// window `F` names.
    fn config(
        &self,
        fleet: lash_core_execution::FleetFormat,
    ) -> Result<lash_core_execution::PersistedSessionConfig, StoreError> {
        let payload: lash_core_execution::store::SessionHeadPayload =
            lash_core_execution::store::decode_versioned_json_record_for_fleet(
                &self.head_json,
                "SessionHeadMeta",
                lash_core_execution::surface_format!(
                    lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION
                ),
                fleet,
            )?;
        Ok(payload.config)
    }
}

/// Every pin of `session_id` with the revision it resolves to now, if any.
async fn resolved_pins_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
) -> Result<Vec<(Target, Option<u64>)>, StoreError> {
    let pins = sqlx::query_as::<_, (String, String)>(session_sql().pins.select_by_session.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut resolved = Vec::with_capacity(pins.len());
    for (kind, id) in pins {
        let target = Target::from_stored(&kind, &id)?;
        let revision = match resolve_tx(tx, session_id, &target).await? {
            TargetResolution::Revision(revision) => Some(revision),
            TargetResolution::Pending | TargetResolution::Unavailable => None,
        };
        resolved.push((target, revision));
    }
    Ok(resolved)
}

fn retained_revision(
    session_id: &SessionId,
    row: RevisionRow,
    head_revision: u64,
    pins: &[(Target, Option<u64>)],
    fleet: lash_core_execution::FleetFormat,
) -> Result<RetainedRevision, StoreError> {
    Ok(RetainedRevision {
        session_id: session_id.clone(),
        head_revision: row.head_revision,
        config: row.config(fleet)?,
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

/// A read transaction that sees one snapshot across its statements.
async fn begin_snapshot(pool: &PgPool) -> Result<Tx<'_>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read
            .sql(),
    )
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(tx)
}

impl PostgresStore {
    pub(crate) async fn resolve_target_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<RetainedRevision, StoreError> {
        let fleet = self.fence.fleet();
        let mut tx = begin_snapshot(&self.pool).await?;
        require_session_tx(&mut tx, session_id).await?;
        let revision = resolve_tx(&mut tx, session_id, target)
            .await?
            .revision(session_id, target)?;
        let row = match i64::try_from(revision) {
            Ok(sql_revision) => sqlx::query_as::<_, (Option<String>, Option<String>, String)>(
                session_sql().revisions.select.sql(),
            )
            .bind(session_id.as_str())
            .bind(sql_revision)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
            Err(_) => None,
        };
        let (leaf_node_id, checkpoint_ref, head_json) =
            row.ok_or_else(|| StoreError::ForkTargetPruned {
                session_id: session_id.clone(),
                target: target.clone(),
            })?;
        let head_revision = head_revision_tx(&mut tx, session_id).await?;
        let pins = resolved_pins_tx(&mut tx, session_id).await?;
        let retained = retained_revision(
            session_id,
            RevisionRow {
                head_revision: revision,
                leaf_node_id,
                checkpoint_ref,
                head_json,
            },
            head_revision,
            &pins,
            fleet,
        )?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(retained)
    }

    pub(crate) async fn revisions_in_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RetainedRevision>, StoreError> {
        let fleet = self.fence.fleet();
        let mut tx = begin_snapshot(&self.pool).await?;
        require_session_tx(&mut tx, session_id).await?;
        let rows = sqlx::query_as::<_, (i64, Option<String>, Option<String>, String)>(
            session_sql().revisions.select_by_session.sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut stored = Vec::with_capacity(rows.len());
        for (head_revision, leaf_node_id, checkpoint_ref, head_json) in rows {
            stored.push(RevisionRow {
                head_revision: u64_from_sql("SessionRevision", "head_revision", head_revision)?,
                leaf_node_id,
                checkpoint_ref,
                head_json,
            });
        }
        let head_revision = head_revision_tx(&mut tx, session_id).await?;
        let pins = resolved_pins_tx(&mut tx, session_id).await?;
        let mut revisions = Vec::with_capacity(stored.len());
        for row in stored {
            revisions.push(retained_revision(
                session_id,
                row,
                head_revision,
                &pins,
                fleet,
            )?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(revisions)
    }

    pub(crate) async fn pin_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<(), StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        // The session's history fence orders this write against its
        // deletion, which removes its pins; it grants no execution authority.
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        require_session_tx(&mut tx, session_id).await?;
        pin_tx(&mut tx, session_id, target).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    pub(crate) async fn unpin_in_catalog(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<(), StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        sqlx::query(session_sql().pins.delete.sql())
            .bind(session_id.as_str())
            .bind(target.kind())
            .bind(target.id())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        // A policy that releases as the session commits releases what this
        // pin alone held now; `until_gc` leaves it to the host's collection.
        if retention_tx(&mut tx, session_id)
            .await?
            .releases_at_commit()
        {
            release_unretained_tx(&mut tx, false, Some(session_id)).await?;
        }
        tx.commit().await.map_err(store_sqlx_error)
    }

    pub(crate) async fn retention_in_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Retention, StoreError> {
        let mut tx = begin_snapshot(&self.pool).await?;
        require_session_tx(&mut tx, session_id).await?;
        let retention = retention_tx(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(retention)
    }

    pub(crate) async fn set_retention_in_catalog(
        &self,
        session_id: &SessionId,
        retention: Retention,
    ) -> Result<(), StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        require_session_tx(&mut tx, session_id).await?;
        sqlx::query(session_sql().meta.set_retention.sql())
            .bind(session_id.as_str())
            .bind(retention.kind())
            .bind(retention.last_turns().map(i64::from))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}

/// Pin `target` of `session_id` in the caller's transaction. Idempotent.
pub(crate) async fn pin_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
    target: &Target,
) -> Result<(), StoreError> {
    sqlx::query(session_sql().pins.insert.sql())
        .bind(session_id.as_str())
        .bind(target.kind())
        .bind(target.id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
