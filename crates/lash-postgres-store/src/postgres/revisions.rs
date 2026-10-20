//! Retained revisions and pins on PostgreSQL (FIG-4731).
//!
//! The name of a state is `(session_id, head_revision)`. Every head
//! publication records its revision here, in the publishing transaction, and
//! `lash_session_revisions` membership is the retained-revisions relation
//! every reclaimer roots what it keeps in. A pin names a target and resolves
//! to a revision by query.

use super::*;
use crate::session_sql::session_sql;
use lash_core_store::store::RetainedRevision;
use lash_core_store::store::Retention;
use lash_core_store::store::Target;
use lash_core_store::store_backend_support::TargetResolution;
use sqlx::PgConnection;

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
    let held: Option<String> =
        crate::keys::query_scalar(session_sql().revisions.select_config_ref.sql())
            .bind(session_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    if held.as_deref() != Some(config_ref.as_str()) {
        crate::durable::install_staged(tx, staging.nodes())
            .await
            .map_err(crate::support::config_store_error)?;
    }
    crate::observed_sql::query(session_sql().revisions.insert.sql())
        .bind(session_id.as_str())
        .bind(head_revision)
        .bind(leaf_node_id)
        .bind(checkpoint_ref.map(crate::keys::key))
        .bind(crate::keys::key(config_ref.as_str()))
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Number a terminal once within its publishing transaction, then apply retention
/// at `now`.
pub(crate) async fn record_terminal_conn(
    conn: &mut PgConnection,
    session: &SessionId,
    revision: u64,
    now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    let ordinal: Option<i64> =
        crate::observed_sql::query_scalar(session_sql().revisions.mark_terminal.sql())
            .bind(session.as_str())
            .bind(i64::try_from(revision).map_err(|error| StoreError::Backend(error.to_string()))?)
            .fetch_optional(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    if let Some(ordinal) = ordinal {
        crate::observed_sql::query(session_sql().revisions.advance_terminal.sql())
            .bind(session.as_str())
            .bind(ordinal)
            .execute(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    }
    let retention = crate::observed_sql::query_as::<_, (String, Option<i64>)>(
        session_sql().meta.select_retention.sql(),
    )
    .bind(session.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut *conn))
    .await
    .map_err(store_sqlx_error)?
    .map(|(kind, turns)| Retention::from_stored(&kind, turns))
    .transpose()?;
    if retention.is_some_and(Retention::releases_at_commit) {
        release_unretained_tx(conn, false, Some(session), now).await?;
    }
    Ok(())
}

/// Release every revision nothing retains, of `session_id` or of every
/// session, retire the ancestry only a released revision kept alive, and
/// hold the checkpoint and config roots they published for retirement at
/// `now` (FIG-5949, FIG-6423): a checkpoint root reaches its session's
/// heap, which retirement steps delete in bounded transactions, never this
/// one. `collecting` is a host collection, which ends `until_gc`'s hold.
///
/// A fork holds its source revision's row share-locked until its own head
/// commits, so a release that raced one waits and then retires nothing the
/// fork now runs.
pub(crate) async fn release_unretained_tx(
    tx: &mut PgConnection,
    collecting: bool,
    session_id: Option<&SessionId>,
    now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    let released = crate::observed_sql::query(session_sql().revisions.prune_unretained.sql())
        .bind(i64::from(collecting))
        .bind(session_id.map(SessionId::as_str))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
    release_revision_material_tx(tx, session_id, now).await?;
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
    let roots: std::collections::BTreeSet<String> = released
        .iter()
        .map(|row| {
            Ok([
                crate::keys::get::<Option<String>>(row, 2)?,
                crate::keys::get(row, 3)?,
            ])
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(store_sqlx_error)?
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    crate::durable::retire_roots(tx, &roots, now)
        .await
        .map(drop)
        .map_err(store_sqlx_error)
}

/// Release one page of per-revision material in the prune's transaction.
/// A session also lets go of each queued attachment whose last hold ended,
/// under the session's write lock its caller holds (FIG-6611); a host
/// collection, which names no session, does that a session at a time
/// (`gc_unreachable`).
pub(crate) async fn release_revision_material_tx(
    tx: &mut PgConnection,
    session_id: Option<&SessionId>,
    _now: lash_durable::DurableInstant,
) -> Result<(), StoreError> {
    crate::durable::retention::release_material_batch(tx, session_id)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(session) = session_id {
        crate::attachments::release_session_attachments_tx(
            tx,
            session,
            i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH),
        )
        .await?;
    }
    Ok(())
}

/// The retention policy recorded for `session_id`, or `None` for a session
/// with no metadata row: it recorded no policy, and nothing stands in for
/// one.
pub(crate) async fn retention_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
) -> Result<Option<Retention>, StoreError> {
    crate::observed_sql::query_as::<_, (String, Option<i64>)>(
        session_sql().meta.select_retention.sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?
    .map(|(kind, last_turns)| Retention::from_stored(&kind, last_turns))
    .transpose()
}

/// Refuse a session the catalog does not hold: a deletion tombstone answers
/// [`StoreError::SessionDeleted`], an id it never held
/// [`StoreError::SessionNotFound`].
async fn require_session_tx(tx: &mut Tx<'_>, session_id: &SessionId) -> Result<(), StoreError> {
    let (exists, deleted) = crate::observed_sql::query_as::<_, (bool, bool)>(
        session_sql()
            .meta_postgres
            .exists_materialized_or_deleted
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(crate::observed_sql::executor(&mut **tx))
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
    let head: Option<i64> =
        crate::observed_sql::query_scalar(session_sql().revisions.select_head_revision.sql())
            .bind(session_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
                    let state: Option<String> = crate::observed_sql::query_scalar(
                        crate::turn_ingress::turn_ingress_sql()
                            .pending_inputs
                            .select_state_by_id
                            .sql(),
                    )
                    .bind(session_id.as_str())
                    .bind(input.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
    config_ref: Option<String>,
}

impl RevisionRow {
    /// The configuration a fork of this revision records: the one the head
    /// recorded when the revision was published, so a config command applied
    /// since the frame opened is part of what a fork copies. `fleet` is the
    /// store's recorded `F`: the head document admits the `[N-1, N]` reader
    /// window `F` names.
    async fn config(
        &self,
        conn: &mut PgConnection,
        fleet: lash_core_store::store::FleetFormat,
    ) -> Result<lash_core_store::session_graph::PersistedSessionConfig, StoreError> {
        let payload =
            crate::support::load_config_tx(conn, self.config_ref.as_deref(), fleet, None).await?;
        Ok(payload.config)
    }
}

/// Every pin of `session_id` with the revision it resolves to now, if any.
async fn resolved_pins_tx(
    tx: &mut Tx<'_>,
    session_id: &SessionId,
) -> Result<Vec<(Target, Option<u64>)>, StoreError> {
    let pins = crate::observed_sql::query_as::<_, (String, String)>(
        session_sql().pins.select_by_session.sql(),
    )
    .bind(session_id.as_str())
    .fetch_all(crate::observed_sql::executor(&mut **tx))
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

async fn retained_revision(
    conn: &mut PgConnection,
    session_id: &SessionId,
    row: RevisionRow,
    head_revision: u64,
    pins: &[(Target, Option<u64>)],
    fleet: lash_core_store::store::FleetFormat,
) -> Result<RetainedRevision, StoreError> {
    Ok(RetainedRevision {
        session_id: session_id.clone(),
        head_revision: row.head_revision,
        config: row.config(conn, fleet).await?,
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
    let mut tx = crate::observed_sql::control("BEGIN", pool.begin())
        .await
        .map_err(store_sqlx_error)?;
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)?;
    Ok(tx)
}

impl PostgresStore {
    /// The instant a hold this store's catalog writes is queued at.
    pub(crate) fn retirement_instant(&self) -> lash_durable::DurableInstant {
        lash_durable::DurableInstant(crate::support::clamp_epoch_ms(self.clock.timestamp_ms()))
    }

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
            Ok(sql_revision) => crate::observed_sql::query_as::<
                _,
                (
                    Option<String>,
                    Option<crate::keys::Key>,
                    Option<crate::keys::Key>,
                ),
            >(session_sql().revisions.select.sql())
            .bind(session_id.as_str())
            .bind(sql_revision)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
            &mut tx,
            session_id,
            RevisionRow {
                head_revision: revision,
                leaf_node_id,
                checkpoint_ref: checkpoint_ref.map(|key| key.0),
                config_ref: head_json.map(|key| key.0),
            },
            head_revision,
            &pins,
            fleet,
        )
        .await?;
        crate::observed_sql::control("COMMIT", tx.commit())
            .await
            .map_err(store_sqlx_error)?;
        Ok(retained)
    }

    pub(crate) async fn revisions_in_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RetainedRevision>, StoreError> {
        let fleet = self.fence.fleet();
        let mut tx = begin_snapshot(&self.pool).await?;
        require_session_tx(&mut tx, session_id).await?;
        let rows = crate::observed_sql::query_as::<
            _,
            (
                i64,
                Option<String>,
                Option<crate::keys::Key>,
                Option<crate::keys::Key>,
            ),
        >(session_sql().revisions.select_by_session.sql())
        .bind(session_id.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
        let mut stored = Vec::with_capacity(rows.len());
        for (head_revision, leaf_node_id, checkpoint_ref, head_json) in rows {
            stored.push(RevisionRow {
                head_revision: u64_from_sql("SessionRevision", "head_revision", head_revision)?,
                leaf_node_id,
                checkpoint_ref: checkpoint_ref.map(|key| key.0),
                config_ref: head_json.map(|key| key.0),
            });
        }
        let head_revision = head_revision_tx(&mut tx, session_id).await?;
        let pins = resolved_pins_tx(&mut tx, session_id).await?;
        let mut revisions = Vec::with_capacity(stored.len());
        for row in stored {
            revisions.push(
                retained_revision(&mut tx, session_id, row, head_revision, &pins, fleet).await?,
            );
        }
        crate::observed_sql::control("COMMIT", tx.commit())
            .await
            .map_err(store_sqlx_error)?;
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
        crate::observed_sql::query(session_sql().pins.delete.sql())
            .bind(session_id.as_str())
            .bind(target.kind())
            .bind(target.id())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        // A policy that releases as the session commits releases what this
        // pin alone held now; `until_gc` leaves it to the host's collection,
        // and so does a session that recorded no policy.
        if retention_tx(&mut tx, session_id)
            .await?
            .is_some_and(Retention::releases_at_commit)
        {
            release_unretained_tx(&mut tx, false, Some(session_id), self.retirement_instant())
                .await?;
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
        crate::observed_sql::control("COMMIT", tx.commit())
            .await
            .map_err(store_sqlx_error)?;
        retention.ok_or_else(|| StoreError::SessionNotFound {
            session_id: session_id.clone(),
        })
    }

    pub(crate) async fn set_retention_in_catalog(
        &self,
        session_id: &SessionId,
        retention: Retention,
    ) -> Result<(), StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        require_session_tx(&mut tx, session_id).await?;
        crate::observed_sql::query(session_sql().meta.set_retention.sql())
            .bind(session_id.as_str())
            .bind(retention.kind())
            .bind(retention.last_turns().map(i64::from))
            .execute(crate::observed_sql::executor(&mut **tx))
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
    crate::observed_sql::query(session_sql().pins.insert.sql())
        .bind(session_id.as_str())
        .bind(target.kind())
        .bind(target.id())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
