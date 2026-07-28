use crate::*;

impl AttachmentManifest for PostgresSessionStore {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
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
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(store_sqlx_error)
        })
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        let attachment_ids = attachment_ids.to_vec();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            commit_attachment_refs_tx(&mut tx, &session_id, &attachment_ids).await?;
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
            Ok(rows
                .into_iter()
                .map(|row| AttachmentManifestEntry {
                    attachment_id: AttachmentId::new(row.get::<String, _>(0)),
                    session_id: row.get(1),
                    canonical_uri: row.get(2),
                    intent_at_epoch_ms: row.get::<i64, _>(3) as u64,
                    committed_at_epoch_ms: row.get::<Option<i64>, _>(4).map(|value| value as u64),
                    owner_kind: match row.get::<Option<String>, _>(5).as_deref() {
                        Some("turn") => Some(AttachmentOwnerKind::Turn),
                        Some("process") => Some(AttachmentOwnerKind::Process),
                        _ => None,
                    },
                    owner_id: row.get(6),
                })
                .collect())
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
