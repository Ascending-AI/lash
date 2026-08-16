use crate::*;

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

impl AttachmentManifest for PostgresSessionStore {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &intent.session_id)
                .await?;
            // Re-recording refreshes the timestamp and durable owner together.
            // The GC statement later composes this age with owner-death proof.
            sqlx::query(
                "INSERT INTO lash_attachment_manifest (
                    attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms,
                    owner_kind, owner_id
                 )
                 VALUES ($1, $2, $3, $4, NULL, $5, $6)
                 ON CONFLICT (session_id, attachment_id) DO UPDATE SET
                    canonical_uri = EXCLUDED.canonical_uri,
                    intent_at_ms = EXCLUDED.intent_at_ms,
                    owner_kind = EXCLUDED.owner_kind,
                    owner_id = EXCLUDED.owner_id",
            )
            .bind(intent.attachment_id.as_str())
            .bind(intent.session_id)
            .bind(intent.canonical_uri)
            .bind(intent.intent_at_epoch_ms as i64)
            .bind(intent.owner_kind.map(AttachmentOwnerKind::as_str))
            .bind(intent.owner_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)
        })
    }

    /// The writer half of the GC fence: the condemnation read, the revoke, and
    /// the intent upsert are one transaction, so a sweeper's condemn CAS either
    /// runs before all of it or fails against the intent it wrote.
    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &intent.session_id)
                .await?;
            lock_attachment_fence_tx(&mut tx, intent.attachment_id.as_str()).await?;
            let phase = sqlx::query_scalar::<_, String>(
                "SELECT phase FROM lash_attachment_condemnations
                 WHERE attachment_id = $1",
            )
            .bind(intent.attachment_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            match phase.as_deref() {
                // The physical delete is already in flight: record nothing, so
                // these bytes cannot land inside it.
                Some("deleting") => {
                    tx.commit().await.map_err(store_sqlx_error)?;
                    return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                }
                // Take the digest back before the sweeper can arm its delete.
                Some(_) => {
                    sqlx::query(
                        "DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1",
                    )
                    .bind(intent.attachment_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
                }
                None => {}
            }
            sqlx::query(
                "INSERT INTO lash_attachment_manifest (
                    attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms,
                    owner_kind, owner_id
                 )
                 VALUES ($1, $2, $3, $4, NULL, $5, $6)
                 ON CONFLICT (session_id, attachment_id) DO UPDATE SET
                    canonical_uri = EXCLUDED.canonical_uri,
                    intent_at_ms = EXCLUDED.intent_at_ms,
                    owner_kind = EXCLUDED.owner_kind,
                    owner_id = EXCLUDED.owner_id",
            )
            .bind(intent.attachment_id.as_str())
            .bind(intent.session_id)
            .bind(intent.canonical_uri)
            .bind(intent.intent_at_epoch_ms as i64)
            .bind(intent.owner_kind.map(AttachmentOwnerKind::as_str))
            .bind(intent.owner_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            Ok(lash_core::AttachmentWriteFence::Granted)
        })
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let now = self.clock.timestamp_ms();
        let session_id = session_id.to_string();
        let attachment_ids = attachment_ids.to_vec();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &session_id).await?;
            commit_attachment_refs_tx(&mut tx, &session_id, &attachment_ids, now).await?;
            tx.commit().await.map_err(store_sqlx_error)
        })
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let rows = sqlx::query(
                "SELECT attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms,
                        owner_kind, owner_id
                 FROM lash_attachment_manifest
                 WHERE committed_at_ms IS NULL AND intent_at_ms <= $1
                 ORDER BY attachment_id ASC",
            )
            .bind(older_than_epoch_ms as i64)
            .fetch_all(&pool)
            .await
            .map_err(store_sqlx_error)?;
            rows.into_iter()
                .map(|row| {
                    Ok(AttachmentManifestEntry {
                        attachment_id: AttachmentId::new(row.get::<String, _>(0)),
                        session_id: row.get(1),
                        canonical_uri: row.get(2),
                        intent_at_epoch_ms: u64_from_sql(
                            "AttachmentManifest",
                            "intent_at_ms",
                            row.get(3),
                        )?,
                        committed_at_epoch_ms: row
                            .get::<Option<i64>, _>(4)
                            .map(|value| {
                                u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                            })
                            .transpose()?,
                        owner_kind: match row.get::<Option<String>, _>(5).as_deref() {
                            Some("turn") => Some(AttachmentOwnerKind::Turn),
                            Some("process") => Some(AttachmentOwnerKind::Process),
                            _ => None,
                        },
                        owner_id: row.get(6),
                    })
                })
                .collect()
        })
    }

    fn forget(&self, session_id: &str, attachment_id: &AttachmentId) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        let attachment_id = attachment_id.to_string();
        block_on_detached(async move {
            sqlx::query(
                "DELETE FROM lash_attachment_manifest
                 WHERE session_id = $1 AND attachment_id = $2",
            )
            .bind(session_id)
            .bind(attachment_id)
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(store_sqlx_error)
        })
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, StoreError> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        let attachment_id = attachment_id.to_string();
        block_on_detached(async move {
            let row = sqlx::query(
                "SELECT 1 FROM lash_attachment_manifest
                 WHERE session_id = $1 AND attachment_id = $2
                 LIMIT 1",
            )
            .bind(session_id)
            .bind(attachment_id)
            .fetch_optional(&pool)
            .await
            .map_err(store_sqlx_error)?;
            Ok(row.is_some())
        })
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let rows = sqlx::query("SELECT DISTINCT attachment_id FROM lash_attachment_manifest")
                .fetch_all(&pool)
                .await
                .map_err(store_sqlx_error)?;
            Ok(rows
                .into_iter()
                .map(|row| AttachmentId::new(row.get::<String, _>(0)))
                .collect())
        })
    }
}
