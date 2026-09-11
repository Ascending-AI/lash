use lash_sansio::SessionId;
/// FIG-653: graph retention is a prune precondition for committed attachment roots.
/// Owner-level retention deliberately includes suffix attachments: the manifest
/// has no node edge. Forks and pins keep these rows until their final prefix dies.
pub(crate) const RECLAIM_DELETED_ATTACHMENT_ROOTS: &str =
    "DELETE FROM lash_attachment_manifest AS manifest
 WHERE EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
               WHERE deleted.session_id = manifest.session_id)
   AND (manifest.committed_at_ms IS NULL OR NOT EXISTS (
       SELECT 1 FROM lash_graph_nodes AS node
       WHERE node.session_id = manifest.session_id AND node.tombstoned = FALSE
   ))";

use crate::*;

fn process_owner_death_sql(process_registry_shared: bool) -> String {
    if process_registry_shared {
        format!(
            "OR (
                manifest.owner_kind = '{}'
                AND NOT EXISTS (
                    SELECT 1 FROM lash_processes AS process
                    WHERE process.process_id = manifest.owner_id
                )
            )",
            AttachmentOwnerKind::Process.as_str()
        )
    } else {
        String::new()
    }
}

pub(crate) fn live_attachment_ref_sql(process_registry_shared: bool) -> String {
    let process_dead = process_owner_death_sql(process_registry_shared);
    format!(
        "SELECT 1 FROM lash_attachment_manifest AS manifest
         WHERE manifest.attachment_id = $1
           AND NOT (
                manifest.committed_at_ms IS NULL
                AND manifest.intent_at_ms <= $2
                AND (
                    manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                    OR (
                        manifest.owner_kind = '{}'
                        AND EXISTS (
                            SELECT 1 FROM lash_runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
                    {process_dead}
                )
           )
         LIMIT 1",
        AttachmentOwnerKind::Turn.as_str()
    )
}

pub(crate) fn forget_aged_uncommitted_attachment_intents_sql(
    process_registry_shared: bool,
) -> String {
    let process_dead = process_owner_death_sql(process_registry_shared);
    format!(
        "DELETE FROM lash_attachment_manifest AS manifest
         WHERE manifest.committed_at_ms IS NULL
           AND manifest.intent_at_ms <= $1
           AND (
                manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                OR (
                    manifest.owner_kind = '{}'
                    AND EXISTS (
                        SELECT 1 FROM lash_runtime_turn_commits AS turn_commit
                        WHERE turn_commit.session_id = manifest.session_id
                          AND turn_commit.turn_id <> manifest.owner_id
                          AND turn_commit.committed_at_ms > manifest.intent_at_ms
                    )
                )
                {process_dead}
           )",
        AttachmentOwnerKind::Turn.as_str()
    )
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
    sqlx::query(
        "DELETE FROM lash_attachment_condemnations
         WHERE attachment_id = $1
           AND (phase = 'deleting'
                OR (phase = 'condemned' AND write_token IS NULL))",
    )
    .bind(attachment_id)
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)
}

/// Clear an abandoned restoring writer under explicit host quiescence.
/// Preserve `Reclaimed`; retire `Condemned` only when its associated intent
/// became committed, otherwise preserve it after removing that intent.
pub(crate) async fn recover_abandoned_attachment_write(
    pool: &PgPool,
    attachment_id: &str,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    lock_attachment_fence_tx(&mut tx, attachment_id).await?;
    let claim = sqlx::query_as::<_, (String, String)>(
        "SELECT write_token, write_session_id
         FROM lash_attachment_condemnations
         WHERE attachment_id = $1
           AND phase IN ('condemned', 'reclaimed')
           AND write_token IS NOT NULL",
    )
    .bind(attachment_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    if let Some((token, session_id)) = claim {
        sqlx::query(
            "DELETE FROM lash_attachment_manifest
             WHERE attachment_id = $1 AND session_id = $2
               AND committed_at_ms IS NULL",
        )
        .bind(attachment_id)
        .bind(&session_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let condemned_superseded = sqlx::query(
            "DELETE FROM lash_attachment_condemnations
             WHERE attachment_id = $1 AND write_token = $2
               AND phase = 'condemned'
               AND EXISTS (
                   SELECT 1 FROM lash_attachment_manifest
                    WHERE attachment_id = $1 AND session_id = $3
                      AND committed_at_ms IS NOT NULL
               )",
        )
        .bind(attachment_id)
        .bind(&token)
        .bind(&session_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if condemned_superseded == 0 {
            sqlx::query(
                "UPDATE lash_attachment_condemnations
                 SET write_token = NULL, write_session_id = NULL
                 WHERE attachment_id = $1 AND write_token = $2",
            )
            .bind(attachment_id)
            .bind(token)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
    }
    tx.commit().await.map_err(store_sqlx_error)
}

impl AttachmentManifest for PostgresSessionStore {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &intent.session_id)
                .await?;
            lock_attachment_fence_tx(&mut tx, intent.attachment_id.as_str()).await?;
            let condemnation = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT phase, write_token FROM lash_attachment_condemnations
                 WHERE attachment_id = $1",
            )
            .bind(intent.attachment_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            match condemnation
                .as_ref()
                .map(|(phase, token)| (phase.as_str(), token.is_some()))
            {
                Some(("deleting", _)) => {
                    return Err(StoreError::Backend(format!(
                        "cannot record attachment `{}` while physical deletion is in flight",
                        intent.attachment_id
                    )));
                }
                Some(("reclaimed", false)) => {
                    return Err(StoreError::AttachmentBytesReclaimed {
                        digest: intent.attachment_id,
                    });
                }
                Some(("condemned", false)) => {
                    return Err(StoreError::Backend(format!(
                        "cannot record attachment `{}` through the unfenced manifest path while it is condemned; use begin_attachment_write",
                        intent.attachment_id
                    )));
                }
                Some(("condemned" | "reclaimed", true)) => {
                    return Err(StoreError::Backend(format!(
                        "cannot record attachment `{}` while its bytes are being restored",
                        intent.attachment_id
                    )));
                }
                None => {}
                Some((phase, _)) => {
                    return Err(StoreError::Backend(format!(
                        "attachment `{}` has unknown condemnation phase `{phase}`",
                        intent.attachment_id
                    )));
                }
            }
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
            .bind(intent.session_id.as_str())
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
            let condemnation = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT phase, write_token FROM lash_attachment_condemnations
                 WHERE attachment_id = $1",
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
            let permit = match condemnation
                .as_ref()
                .map(|(phase, token)| (phase.as_str(), token.is_some()))
            {
                // The physical delete is already in flight: record nothing, so
                // these bytes cannot land inside it.
                Some(("deleting", _)) | Some(("condemned" | "reclaimed", true)) => {
                    tx.commit().await.map_err(store_sqlx_error)?;
                    return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                }
                // Keep the prior phase present and own it with an opaque token
                // until the backend put settles.
                Some(("condemned" | "reclaimed", false)) => {
                    let token = lash_core::AttachmentWriteToken::new();
                    let claimed = sqlx::query(
                        "UPDATE lash_attachment_condemnations
                         SET write_token = $2, write_session_id = $3
                         WHERE attachment_id = $1
                           AND phase IN ('condemned', 'reclaimed')
                           AND write_token IS NULL",
                    )
                    .bind(intent.attachment_id.as_str())
                    .bind(token.as_hex())
                    .bind(intent.session_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .rows_affected();
                    if claimed == 0 {
                        tx.commit().await.map_err(store_sqlx_error)?;
                        return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                    }
                    lash_core::AttachmentWritePermit::restoring(token)
                }
                None => lash_core::AttachmentWritePermit::ordinary(),
                Some((phase, _)) => {
                    return Err(StoreError::Backend(format!(
                        "attachment `{}` has unknown condemnation phase `{phase}`",
                        intent.attachment_id
                    )));
                }
            };
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
            .bind(intent.session_id.as_str())
            .bind(intent.canonical_uri)
            .bind(intent.intent_at_epoch_ms as i64)
            .bind(intent.owner_kind.map(AttachmentOwnerKind::as_str))
            .bind(intent.owner_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            Ok(lash_core::AttachmentWriteFence::Granted(permit))
        })
    }

    fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let Some(token) = permit.rollback_token() else {
            return Ok(());
        };
        let pool = self.pool.clone();
        let attachment_id = intent.attachment_id.to_string();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            lock_attachment_fence_tx(&mut tx, &attachment_id).await?;
            sqlx::query(
                "DELETE FROM lash_attachment_condemnations
                 WHERE attachment_id = $1 AND write_token = $2",
            )
            .bind(&attachment_id)
            .bind(token.as_hex())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)
        })
    }

    fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let Some(token) = permit.rollback_token() else {
            return Ok(());
        };
        let pool = self.pool.clone();
        let attachment_id = intent.attachment_id.to_string();
        let session_id = intent.session_id.clone();
        block_on_detached(async move {
            let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
            lock_attachment_fence_tx(&mut tx, &attachment_id).await?;
            let token = token.as_hex();
            let owns_phase = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM lash_attachment_condemnations
                    WHERE attachment_id = $1 AND write_token = $2
                 )",
            )
            .bind(&attachment_id)
            .bind(&token)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if owns_phase {
                sqlx::query(
                    "DELETE FROM lash_attachment_manifest
                     WHERE attachment_id = $1 AND session_id = $2
                       AND committed_at_ms IS NULL",
                )
                .bind(&attachment_id)
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                let condemned_superseded = sqlx::query(
                    "DELETE FROM lash_attachment_condemnations
                     WHERE attachment_id = $1 AND write_token = $2
                       AND phase = 'condemned'
                       AND EXISTS (
                           SELECT 1 FROM lash_attachment_manifest
                            WHERE attachment_id = $1 AND session_id = $3
                              AND committed_at_ms IS NOT NULL
                       )",
                )
                .bind(&attachment_id)
                .bind(&token)
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
                if condemned_superseded == 0 {
                    sqlx::query(
                        "UPDATE lash_attachment_condemnations
                         SET write_token = NULL, write_session_id = NULL
                         WHERE attachment_id = $1 AND write_token = $2",
                    )
                    .bind(&attachment_id)
                    .bind(&token)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
                }
            }
            tx.commit().await.map_err(store_sqlx_error)
        })
    }

    fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let now = self.clock.timestamp_ms();
        let session_id = SessionId::from(session_id.to_string());
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
        let older_than = clamp_epoch_ms(older_than_epoch_ms);
        block_on_detached(async move {
            let rows = sqlx::query(
                "SELECT attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms,
                        owner_kind, owner_id
                 FROM lash_attachment_manifest
                 WHERE committed_at_ms IS NULL AND intent_at_ms <= $1
                 ORDER BY attachment_id ASC",
            )
            .bind(older_than)
            .fetch_all(&pool)
            .await
            .map_err(store_sqlx_error)?;
            rows.into_iter()
                .map(|row| {
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
                        committed_at_epoch_ms: row
                            .get::<Option<i64>, _>(4)
                            .map(|value| {
                                u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                            })
                            .transpose()?,
                        owner_kind: row
                            .get::<Option<String>, _>(5)
                            .as_deref()
                            .map(|value| {
                                AttachmentOwnerKind::from_wire_str(value).ok_or_else(|| {
                                    StoreError::StoredDataCorrupt {
                                        record_kind: "AttachmentManifest owner kind",
                                        message: format!("unknown attachment owner kind `{value}`"),
                                    }
                                })
                            })
                            .transpose()?,
                        owner_id: row.get(6),
                    })
                })
                .collect()
        })
    }

    fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let pool = self.pool.clone();
        let session_id = SessionId::from(session_id.to_string());
        let attachment_id = attachment_id.to_string();
        block_on_detached(async move {
            sqlx::query(
                "DELETE FROM lash_attachment_manifest
                 WHERE session_id = $1 AND attachment_id = $2 AND (
                             committed_at_ms IS NULL OR NOT EXISTS (
                                 SELECT 1 FROM lash_graph_nodes AS node
                                 WHERE node.session_id = lash_attachment_manifest.session_id
                                   AND node.tombstoned = FALSE
                             ))",
            )
            .bind(session_id.as_str())
            .bind(attachment_id)
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(store_sqlx_error)
        })
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        let pool = self.pool.clone();
        block_on_detached(async move {
            let rows = sqlx::query("SELECT DISTINCT attachment_id FROM lash_attachment_manifest")
                .fetch_all(&pool)
                .await
                .map_err(store_sqlx_error)?;
            rows.into_iter()
                .map(|row| {
                    attachment_id_from_sql("AttachmentManifest", "attachment_id", row.get(0))
                })
                .collect()
        })
    }
}
