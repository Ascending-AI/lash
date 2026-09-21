//! The attachment write-ahead manifest and its garbage-collection fence.
//!
//! The PostgreSQL owner of the attachment family: `lash_attachment_manifest`
//! and `lash_attachment_condemnations`. Every atom runs in a transaction that
//! first takes the per-digest advisory key below, which is what buys under
//! `READ COMMITTED` the mutual exclusion SQLite gets from `BEGIN IMMEDIATE`.

use std::sync::LazyLock;

use lash_sansio::SessionId;
use lash_store_sql::attachment::condemnation::CondemnationStatements;
use lash_store_sql::attachment::manifest::{ManifestProcessOwnerStatements, ManifestStatements};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

use crate::*;

lash_store_sql::statements! {
    /// `lash_attachment_manifest` statements only PostgreSQL issues.
    pub(crate) struct ManifestPostgresStatements @ "attachment_manifest" {
        /// Reclaim every attachment root a deleted session left behind.
        ///
        /// FIG-653: graph retention is a prune precondition for committed
        /// roots, and owner-level retention deliberately includes suffix
        /// attachments, because the manifest has no node edge — forks and
        /// pins keep these rows until their final prefix dies.
        ///
        /// Forks on the tombstone literal: `graph_nodes.tombstoned` is
        /// BOOLEAN on PostgreSQL and INTEGER 0/1 on SQLite.
        delete_deleted_session_roots = "DELETE FROM attachment_manifest AS manifest
             WHERE EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = manifest.session_id)
               AND (manifest.committed_at_ms IS NULL OR NOT EXISTS (
                   SELECT 1 FROM graph_nodes AS node
                   WHERE node.session_id = manifest.session_id AND node.tombstoned = FALSE
               ))";

        /// Forget `?2` in session `?1` unless a live node still roots it.
        /// Same tombstone-literal fork as
        /// [`ManifestPostgresStatements::delete_deleted_session_roots`].
        forget_for_session = "DELETE FROM attachment_manifest
             WHERE session_id = ?1 AND attachment_id = ?2 AND (
                 committed_at_ms IS NULL OR NOT EXISTS (
                     SELECT 1 FROM graph_nodes AS node
                     WHERE node.session_id = attachment_manifest.session_id
                       AND node.tombstoned = FALSE
                 ))";

        /// Every uncommitted intent older than `?1`.
        ///
        /// The ordering is the fork: PostgreSQL reports digest order, SQLite
        /// reports oldest intent first. Both are total and neither caller
        /// depends on the other's, so the two orders are left exactly as they
        /// stand rather than unified inside a refactor.
        select_uncommitted = "SELECT attachment_id, session_id, canonical_uri, intent_at_ms,
                 committed_at_ms, owner_kind, owner_id, owner_incarnation, written_at_ms
             FROM attachment_manifest
             WHERE committed_at_ms IS NULL AND intent_at_ms <= ?1
             ORDER BY attachment_id ASC";
    }
}

lash_store_sql::statements! {
    /// `lash_attachment_condemnations` statements only PostgreSQL issues.
    pub(crate) struct CondemnationPostgresStatements @ "attachment_condemnation" {
        /// Whether a physical delete is already in flight for `?1`.
        ///
        /// Wrapped in `SELECT EXISTS(…)` because this driver reads a scalar
        /// that is always present; SQLite reads the row's presence instead.
        select_deleting = "SELECT EXISTS(
                SELECT 1 FROM attachment_condemnations
                WHERE attachment_id = ?1 AND phase = 'deleting'
             )";

        /// Condemn `?1`, reporting whether this sweeper is the one that did.
        ///
        /// `ON CONFLICT DO NOTHING` is the fork *and* the contention check:
        /// `READ COMMITTED` cannot hold "read the absence, then insert"
        /// atomic, so a peer sweeper is detected by the conflict rather than
        /// by a prior read. SQLite reads the absence under its write lock.
        insert_condemned = "INSERT INTO attachment_condemnations (attachment_id, phase)
             VALUES (?1, 'condemned')
             ON CONFLICT (attachment_id) DO NOTHING";
    }
}

/// The attachment owner classes, spelled once in `lash-core` and named as
/// tokens by the GC predicates that compare against them.
const ATTACHMENT_OWNER: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new(
        "turn_attachment_owner",
        lash_core::store_backend_support::turn_attachment_owner_predicate_sql,
    ),
    VocabularyTerm::new(
        "process_attachment_owner",
        lash_core::store_backend_support::process_attachment_owner_predicate_sql,
    ),
]);

/// Every attachment-family statement, rendered once.
pub(crate) struct AttachmentSql {
    /// `attachment_manifest` statements both backends issue verbatim.
    pub(crate) manifest: ManifestStatements,
    /// `attachment_manifest` statements only PostgreSQL issues.
    pub(crate) manifest_postgres: ManifestPostgresStatements,
    /// The GC probes that prove a process owner dead. PostgreSQL keeps the
    /// process registry in the same database, so there is one dialect here
    /// and no layout to choose; whether the tier *shares* a registry is still
    /// a call-site decision, because a deployment that does not cannot prove
    /// owner death from rows it has no claim on.
    pub(crate) manifest_process_owner: ManifestProcessOwnerStatements,
    /// `attachment_condemnations` statements both backends issue verbatim.
    pub(crate) condemnation: CondemnationStatements,
    /// `attachment_condemnations` statements only PostgreSQL issues.
    pub(crate) condemnation_postgres: CondemnationPostgresStatements,
}

static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres().with_vocabulary(ATTACHMENT_OWNER);
    AttachmentSql {
        manifest: ManifestStatements::render(dialect),
        manifest_postgres: ManifestPostgresStatements::render(dialect),
        manifest_process_owner: ManifestProcessOwnerStatements::render(dialect),
        condemnation: CondemnationStatements::render(dialect),
        condemnation_postgres: CondemnationPostgresStatements::render(dialect),
    }
});

/// The attachment-family statements, rendered once at first use.
pub(crate) fn attachment_sql() -> &'static AttachmentSql {
    &ATTACHMENT_SQL
}

/// The live-root probe this tier may issue, parameterised
/// `$1 = attachment_id`, `$2 = intent_grace_cutoff_ms`.
///
/// The targeted probe and the condemn CAS read the same one so the fence and
/// the probe cannot drift apart.
pub(crate) fn live_attachment_ref_sql(process_registry_shared: bool) -> &'static str {
    if process_registry_shared {
        attachment_sql()
            .manifest_process_owner
            .select_live_root_proving_process_death
            .sql()
    } else {
        attachment_sql().manifest.select_live_root.sql()
    }
}

/// The aged-intent forget this tier may issue, the negation of
/// [`live_attachment_ref_sql`] over every digest at once.
pub(crate) fn forget_aged_uncommitted_attachment_intents_sql(
    process_registry_shared: bool,
) -> &'static str {
    if process_registry_shared {
        attachment_sql()
            .manifest_process_owner
            .delete_aged_uncommitted_proving_process_death
            .sql()
    } else {
        attachment_sql().manifest.delete_aged_uncommitted.sql()
    }
}

/// Advisory-lock namespace for the attachment GC fence. Both halves of the
/// fence — a writer recording an intent and a sweeper condemning a digest —
/// take this lock keyed on the digest for the duration of their transaction.
///
/// A row lock cannot serialize them: at `READ COMMITTED` the writer's manifest
/// insert and the sweeper's root query can each miss the other's uncommitted row
/// (write skew), and there is no existing row for either side to lock when the
/// digest is `Free`. One advisory key per digest makes the two conditional
/// mutations mutually exclusive without introducing any wait a writer can see
/// beyond the other side's transaction.
pub(crate) const ATTACHMENT_FENCE_LOCK_NAMESPACE: i32 = 715_422;

/// Test-only fault injection: how long the writer half holds its transaction
/// open between reading a digest's condemnation phase and revoking it.
///
/// That gap is microseconds in production — two round trips — which is exactly
/// why a transition running bare on the pool instead of under the per-digest
/// advisory key can slip through it unnoticed. Widening it makes the race
/// deterministic for
/// [`arming_a_delete_and_a_concurrent_writer_never_both_win`](crate::tests).
///
/// The fence's correctness is *width-indifferent*, which is the point: a
/// concurrent `arm` waits on the per-digest key however long the window is, so
/// widening it cannot make a correct implementation fail — it can only expose an
/// incorrect one sooner. This knob therefore changes when the law observes a
/// defect, never whether one exists. It compiles only under `cfg(test)`; the
/// shipped writer reads no such value.
#[cfg(test)]
pub(crate) static FENCE_WRITER_WINDOW_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Take the per-digest fence lock for the rest of `tx`.
pub(crate) async fn lock_attachment_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attachment_id: &str,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ATTACHMENT_FENCE_LOCK_NAMESPACE)
        .bind(attachment_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Release only sweep-owned state under the digest's fence lock. A stale sweep
/// cannot clear a restoring writer's token.
pub(crate) async fn release_attachment_condemnation(
    pool: &PgPool,
    attachment_id: &str,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    lock_attachment_fence_tx(&mut tx, attachment_id).await?;
    sqlx::query(attachment_sql().condemnation.delete_sweep_owned.sql())
        .bind(attachment_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)
}

/// Enumerate the durable condemnation authority without exposing write
/// tokens. Persisted phase/provenance combinations are decoded strictly so a
/// corrupt row cannot be mistaken for sweep-owned maintenance work.
pub(crate) async fn list_attachment_condemnations(
    pool: &PgPool,
) -> Result<Vec<lash_core::AttachmentCondemnationRecord>, StoreError> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        attachment_sql().condemnation.select_all.sql(),
    )
    .fetch_all(pool)
    .await
    .map_err(store_sqlx_error)?;
    let mut condemnations = rows
        .into_iter()
        .map(|(digest, phase, write_token, write_session_id)| {
            let digest =
                attachment_id_from_sql("attachment condemnation", "attachment_id", digest)?;
            lash_core::store::decode_attachment_condemnation_record(
                digest,
                &phase,
                write_token.is_some(),
                write_session_id.map(SessionId::from),
            )
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    condemnations.sort_by(|left, right| left.digest.cmp(&right.digest));
    Ok(condemnations)
}

/// Clear an abandoned restoring writer under explicit host quiescence.
/// Retire `Condemned` only when its associated intent became committed,
/// otherwise preserve it after removing that unstamped intent.
pub(crate) async fn recover_abandoned_attachment_write(
    pool: &PgPool,
    attachment_id: &str,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    lock_attachment_fence_tx(&mut tx, attachment_id).await?;
    let claim =
        sqlx::query_as::<_, (String, String)>(attachment_sql().condemnation.select_claim.sql())
            .bind(attachment_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
    if let Some((token, session_id)) = claim {
        sqlx::query(attachment_sql().manifest.delete_unproven_for_session.sql())
            .bind(attachment_id)
            .bind(&session_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let condemned_superseded =
            sqlx::query(attachment_sql().condemnation.delete_superseded_claim.sql())
                .bind(attachment_id)
                .bind(&token)
                .bind(&session_id)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
        if condemned_superseded == 0 {
            sqlx::query(attachment_sql().condemnation.clear_write_claim.sql())
                .bind(attachment_id)
                .bind(token)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    tx.commit().await.map_err(store_sqlx_error)
}

#[async_trait::async_trait]
impl AttachmentManifest for PostgresSessionStore {
    /// The writer half of the GC fence: the condemnation read, the claim, and
    /// the intent upsert are one transaction, so a sweeper's condemn CAS either
    /// runs before all of it or fails against the intent it wrote.
    async fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        let pool = self.pool.clone();
        {
            let write_id = lash_core::AttachmentWriteToken::new();
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &intent.session_id)
                .await?;
            lock_attachment_fence_tx(&mut tx, intent.attachment_id.as_str()).await?;
            let condemnation = sqlx::query_as::<_, (String, Option<String>)>(
                attachment_sql().condemnation.select_phase_and_claim.sql(),
            )
            .bind(intent.attachment_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            #[cfg(test)]
            if condemnation.is_some() {
                let window_ms =
                    FENCE_WRITER_WINDOW_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
                if window_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
                }
            }
            match condemnation
                .as_ref()
                .map(|(phase, token)| (phase.as_str(), token.is_some()))
            {
                // The physical delete is already in flight: record nothing, so
                // these bytes cannot land inside it.
                Some(("deleting", _)) | Some(("condemned", true)) => {
                    tx.commit().await.map_err(store_sqlx_error)?;
                    return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                }
                // Keep the condemnation present and own it with this attempt's
                // identity until the backend put settles.
                Some(("condemned", false)) => {
                    let claimed = sqlx::query(attachment_sql().condemnation.claim_write.sql())
                        .bind(intent.attachment_id.as_str())
                        .bind(write_id.as_hex())
                        .bind(intent.session_id.as_str())
                        .execute(&mut *tx)
                        .await
                        .map_err(store_sqlx_error)?
                        .rows_affected();
                    if claimed == 0 {
                        tx.commit().await.map_err(store_sqlx_error)?;
                        return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                    }
                }
                None => {}
                Some((phase, _)) => {
                    return Err(StoreError::Backend(format!(
                        "attachment `{}` has unknown condemnation phase `{phase}`",
                        intent.attachment_id
                    )));
                }
            }
            // A fresh attempt has proven nothing, so it takes the row with no
            // upload stamp. Evidence and commitment already on the row were
            // earned by earlier attempts and are kept.
            sqlx::query(attachment_sql().manifest.insert_intent.sql())
                .bind(intent.attachment_id.as_str())
                .bind(intent.session_id.as_str())
                .bind(intent.canonical_uri)
                .bind(intent.intent_at_epoch_ms as i64)
                .bind(intent.owner.as_ref().map(|owner| owner.kind().as_str()))
                .bind(intent.owner.as_ref().map(|owner| owner.id().to_string()))
                .bind(
                    intent
                        .owner
                        .as_ref()
                        .and_then(lash_core::AttachmentOwner::incarnation)
                        .map(|incarnation| i64::try_from(incarnation.registration_sequence()))
                        .transpose()
                        .map_err(|_| {
                            StoreError::Backend(
                                "attachment owner incarnation exceeds i64".to_string(),
                            )
                        })?,
                )
                .bind(write_id.as_hex())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            Ok(lash_core::AttachmentWriteFence::Granted(
                lash_core::AttachmentWritePermit::new(write_id),
            ))
        }
    }

    async fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let digest = intent.attachment_id.clone();
        let attachment_id = intent.attachment_id.to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        let written_at_ms = clamp_epoch_ms(self.clock.timestamp_ms());
        {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            lock_attachment_fence_tx(&mut tx, &attachment_id).await?;
            // Id-matched: only the row this attempt still owns is stamped, and
            // the first proven upload is kept.
            let stamped = sqlx::query(attachment_sql().manifest.stamp_written.sql())
                .bind(&attachment_id)
                .bind(session_id.as_str())
                .bind(&write_id)
                .bind(written_at_ms)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
            if stamped == 0 {
                return Err(StoreError::StaleWritePermit { digest });
            }
            // The bytes exist now, so this attempt's claim on the condemnation
            // is released with the condemnation itself.
            sqlx::query(attachment_sql().condemnation.delete_by_write_token.sql())
                .bind(&attachment_id)
                .bind(&write_id)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)
        }
    }

    async fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let attachment_id = intent.attachment_id.to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            lock_attachment_fence_tx(&mut tx, &attachment_id).await?;
            // Only this attempt's own unstamped, uncommitted row. A superseded
            // permit matches nothing and deletes nothing.
            sqlx::query(attachment_sql().manifest.delete_unproven_for_write.sql())
                .bind(&attachment_id)
                .bind(session_id.as_str())
                .bind(&write_id)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            let condemned_superseded =
                sqlx::query(attachment_sql().condemnation.delete_superseded_claim.sql())
                    .bind(&attachment_id)
                    .bind(&write_id)
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .rows_affected();
            if condemned_superseded == 0 {
                sqlx::query(attachment_sql().condemnation.clear_write_claim.sql())
                    .bind(&attachment_id)
                    .bind(&write_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            }
            tx.commit().await.map_err(store_sqlx_error)
        }
    }

    async fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let now = self.clock.timestamp_ms();
        let session_id = SessionId::from(session_id.to_string());
        let attachment_ids = attachment_ids.to_vec();
        {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &session_id).await?;
            commit_attachment_refs_tx(&mut tx, &session_id, &attachment_ids, now).await?;
            tx.commit().await.map_err(store_sqlx_error)
        }
    }

    async fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        let pool = self.pool.clone();
        let older_than = clamp_epoch_ms(older_than_epoch_ms);
        {
            let rows = sqlx::query(attachment_sql().manifest_postgres.select_uncommitted.sql())
                .bind(older_than)
                .fetch_all(&pool)
                .await
                .map_err(store_sqlx_error)?;
            rows.into_iter()
                .map(|row| {
                    let owner_kind = row.get::<Option<String>, _>(5);
                    let owner_id = row.get::<Option<String>, _>(6);
                    let owner_incarnation = row
                        .get::<Option<i64>, _>(7)
                        .map(|value| u64_from_sql("AttachmentManifest", "owner_incarnation", value))
                        .transpose()?;
                    let owner = lash_core::store::decode_attachment_owner(
                        owner_kind.as_deref(),
                        owner_id,
                        owner_incarnation,
                    )?;
                    Ok(AttachmentManifestEntry {
                        attachment_id: attachment_id_from_sql(
                            "AttachmentManifest",
                            "attachment_id",
                            row.get(0),
                        )?,
                        session_id: SessionId::from(row.get::<String, _>(1)),
                        canonical_uri: row.get(2),
                        intent_at_epoch_ms: u64_from_sql(
                            "AttachmentManifest",
                            "intent_at_ms",
                            row.get(3),
                        )?,
                        written_at_epoch_ms: row
                            .get::<Option<i64>, _>(8)
                            .map(|value| u64_from_sql("AttachmentManifest", "written_at_ms", value))
                            .transpose()?,
                        committed_at_epoch_ms: row
                            .get::<Option<i64>, _>(4)
                            .map(|value| {
                                u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                            })
                            .transpose()?,
                        owner,
                    })
                })
                .collect()
        }
    }

    async fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let session_id = SessionId::from(session_id.to_string());
        let attachment_id = attachment_id.to_string();
        {
            sqlx::query(attachment_sql().manifest_postgres.forget_for_session.sql())
                .bind(session_id.as_str())
                .bind(attachment_id)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(store_sqlx_error)
        }
    }

    async fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        let pool = self.pool.clone();
        {
            let rows = sqlx::query(attachment_sql().manifest.select_rooted_ids.sql())
                .fetch_all(&pool)
                .await
                .map_err(store_sqlx_error)?;
            rows.into_iter()
                .map(|row| {
                    attachment_id_from_sql("AttachmentManifest", "attachment_id", row.get(0))
                })
                .collect()
        }
    }
}
