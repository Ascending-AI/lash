//! Attachment edges and external-byte write state, serialized by referrer then digest.
use crate::*;
use lash_core_execution::{ArtifactReferrer, AttachmentWrite, ReferrerClaim, SessionReferrerState};
use lash_store_sql::Dialect;
use lash_store_sql::attachment::{
    condemnation::CondemnationStatements, edges::AttachmentEdgeStatements,
    pending_writes::PendingWriteStatements, sweep_clock::SweepClockStatements,
    uploads::UploadStatements,
};
use std::sync::LazyLock;

lash_store_sql::statements! {
    pub(crate) struct AttachmentPostgresStatements @ "attachment_referrer_edge" {
        select_deleting = "SELECT EXISTS (SELECT 1 FROM attachment_condemnations WHERE attachment_id = ?1 AND phase = 'deleting')";
        insert_condemned = "INSERT INTO attachment_condemnations (attachment_id, phase, sweep_generation) VALUES (?1, 'condemned', ?2) ON CONFLICT (attachment_id) DO NOTHING";
        session_state = "SELECT EXISTS (SELECT 1 FROM session_meta WHERE session_id = ?1), EXISTS (SELECT 1 FROM deleted_sessions WHERE session_id = ?1), EXISTS (SELECT 1 FROM graph_nodes WHERE session_id = ?1 AND tombstoned = FALSE)";
    }
}
pub(crate) struct AttachmentSql {
    pub(crate) edges: AttachmentEdgeStatements,
    pub(crate) pending: PendingWriteStatements,
    pub(crate) uploads: UploadStatements,
    pub(crate) condemnation: CondemnationStatements,
    pub(crate) postgres: AttachmentPostgresStatements,
    pub(crate) sweep_clock: SweepClockStatements,
}
static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    AttachmentSql {
        edges: AttachmentEdgeStatements::render(dialect),
        pending: PendingWriteStatements::render(dialect),
        uploads: UploadStatements::render(dialect),
        condemnation: CondemnationStatements::render(dialect),
        postgres: AttachmentPostgresStatements::render(dialect),
        sweep_clock: SweepClockStatements::render(dialect),
    }
});
pub(crate) fn attachment_sql() -> &'static AttachmentSql {
    &ATTACHMENT_SQL
}
fn check_kind(referrer: &ArtifactReferrer) -> Result<(), StoreError> {
    if !referrer.kind().holds_attachments() {
        return Err(StoreError::ReferrerKindRefused {
            kind: referrer.kind(),
            store: "attachment",
        });
    }
    ArtifactReferrer::decode(referrer.kind().as_str(), &referrer.canonical_id())
        .map_err(|error| error.into_store_error("attachment referrer"))?;
    Ok(())
}
pub(crate) async fn lock_attachment_referrer_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    referrer: &ArtifactReferrer,
) -> Result<(), StoreError> {
    check_kind(referrer)?;
    crate::artifact_store::lock_referrer_tx(tx, referrer)
        .await
        .map_err(store_sqlx_error)
}
async fn check_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    referrer: &ArtifactReferrer,
) -> Result<(), StoreError> {
    let fenced: bool = sqlx::query_scalar(
        crate::artifact_store::artifact_sql()
            .fences
            .select_is_fenced
            .sql(),
    )
    .bind(referrer.kind().as_str())
    .bind(referrer.canonical_id())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if fenced {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: referrer.clone(),
        });
    }
    Ok(())
}
pub(crate) async fn acquire_attachment_refs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim: &ReferrerClaim,
    ids: &[AttachmentId],
    now: u64,
) -> Result<(), StoreError> {
    // The caller took every referrer lock before any artifact lock.
    check_kind(claim.referrer())?;
    check_fence_tx(tx, claim.referrer()).await?;
    let ids = ids.iter().collect::<std::collections::BTreeSet<_>>();
    for id in &ids {
        lock_attachment_fence_tx(tx, id.as_str()).await?;
    }
    for id in &ids {
        let deleting: bool = sqlx::query_scalar(attachment_sql().postgres.select_deleting.sql())
            .bind(id.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let evidenced = sqlx::query(attachment_sql().uploads.select_evidence.sql())
            .bind(id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .is_some();
        if deleting || !evidenced {
            return Err(StoreError::UnknownAttachment {
                digest: (*id).clone(),
            });
        }
    }
    for id in &ids {
        sqlx::query(
            attachment_sql()
                .condemnation
                .delete_unclaimed_condemned
                .sql(),
        )
        .bind(id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        insert_edge_tx(tx, claim.referrer(), id).await?;
    }
    if let Some(cleanup) = claim.guard_cleanup() {
        crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now).await?;
    }
    Ok(())
}
async fn insert_edge_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    referrer: &ArtifactReferrer,
    id: &AttachmentId,
) -> Result<(), StoreError> {
    sqlx::query(attachment_sql().edges.insert.sql())
        .bind(id.as_str())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
async fn has_permit_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    write: &AttachmentWrite,
    token: &str,
) -> Result<bool, StoreError> {
    Ok(sqlx::query(attachment_sql().pending.select_permit.sql())
        .bind(token)
        .bind(write.attachment_id.as_str())
        .bind(write.claim.referrer().kind().as_str())
        .bind(write.claim.referrer().canonical_id())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .is_some())
}
async fn abort_write_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &AttachmentId,
    referrer: &ArtifactReferrer,
    token: &str,
) -> Result<(), StoreError> {
    sqlx::query(attachment_sql().condemnation.delete_superseded_claim.sql())
        .bind(id.as_str())
        .bind(token)
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(attachment_sql().pending.delete_permit.sql())
        .bind(token)
        .bind(id.as_str())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(attachment_sql().edges.delete_unproven_ref.sql())
        .bind(id.as_str())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
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
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_class_and_text
            .sql(),
    )
    .bind(ATTACHMENT_FENCE_LOCK_NAMESPACE)
    .bind(attachment_id)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Advisory-lock class for attachment sweep pass liveness, keyed on the
/// catalog and the pass's generation. A pass holds its key as a
/// session-scoped lock on a dedicated connection for its whole life; the
/// server releases it when that connection closes, whether the pass returned,
/// was cancelled, or its process died. Nothing else takes the key exclusively,
/// so a probe that acquires it has proven the pass dead.
pub(crate) const ATTACHMENT_SWEEP_LIVENESS_LOCK_NAMESPACE: i32 = 715_424;

/// How long an adoption probe waits for a pass's liveness key before it
/// treats the pass as live. A closing connection releases its lock a moment
/// after the client drops it, so the probe waits that moment out rather than
/// deferring a just-crashed pass's rows. The wait decides nothing on expiry:
/// a probe that times out leaves the rows to their pass.
const ATTACHMENT_SWEEP_LIVENESS_PROBE_TIMEOUT: &str = "500ms";

/// How many generations a pass mints before giving up on a liveness key no
/// other live pass shares. Keys are hashed, so a collision is possible and
/// astronomically rare; minting again sidesteps it.
const ATTACHMENT_SWEEP_MINT_ATTEMPTS: u32 = 3;

fn sweep_liveness_key(catalog_id: &str, generation: i64) -> String {
    format!("{catalog_id}:{generation}")
}

/// A live sweep pass's dedicated connection. Dropping it closes the
/// connection, and the server releases the pass's liveness key with it.
struct PostgresSweepLiveness {
    _connection: std::sync::Mutex<sqlx::PgConnection>,
}

/// Mint a sweep generation and take its liveness key on a dedicated
/// connection held for the pass's life.
pub(crate) async fn begin_attachment_sweep(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    catalog_id: &str,
) -> Result<lash_core_execution::AttachmentSweepGeneration, StoreError> {
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    for _ in 0..ATTACHMENT_SWEEP_MINT_ATTEMPTS {
        let mut tx = crate::begin_guarded(&mut connection, fence).await?;
        let generation: i64 =
            sqlx::query_scalar(attachment_sql().sweep_clock.mint_generation.sql())
                .bind(true)
                .fetch_one(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let held: bool = sqlx::query_scalar(
            crate::connection_sql::connection_sql()
                .try_lock_session_by_class_and_text
                .sql(),
        )
        .bind(ATTACHMENT_SWEEP_LIVENESS_LOCK_NAMESPACE)
        .bind(sweep_liveness_key(catalog_id, generation))
        .fetch_one(&mut connection)
        .await
        .map_err(store_sqlx_error)?;
        if held {
            let generation = u64::try_from(generation).map_err(|_| {
                StoreError::Backend(format!(
                    "attachment sweep clock minted negative generation {generation}"
                ))
            })?;
            return Ok(lash_core_execution::AttachmentSweepGeneration::new(
                generation,
                Box::new(PostgresSweepLiveness {
                    _connection: std::sync::Mutex::new(connection),
                }),
            ));
        }
    }
    Err(StoreError::Backend(format!(
        "no attachment sweep generation with a free liveness key after {ATTACHMENT_SWEEP_MINT_ATTEMPTS} mints"
    )))
}

/// Whether generation `generation`'s pass has provably ended: its liveness
/// key can be taken. A dead pass never becomes live again, because no pass
/// takes another generation's key, so the answer stays true once given.
async fn sweep_pass_is_dead(
    pool: &PgPool,
    catalog_id: &str,
    generation: i64,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .set_local_lock_timeout
            .sql(),
    )
    .bind(ATTACHMENT_SWEEP_LIVENESS_PROBE_TIMEOUT)
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    let probe = sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_class_and_text
            .sql(),
    )
    .bind(ATTACHMENT_SWEEP_LIVENESS_LOCK_NAMESPACE)
    .bind(sweep_liveness_key(catalog_id, generation))
    .execute(&mut *tx)
    .await;
    match probe {
        Ok(_) => {
            tx.commit().await.map_err(store_sqlx_error)?;
            Ok(true)
        }
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("55P03") => {
            tx.rollback().await.map_err(store_sqlx_error)?;
            Ok(false)
        }
        Err(error) => Err(store_sqlx_error(error)),
    }
}

/// Adopt every sweep-owned row an older generation left whose pass is dead,
/// one CAS per row under the digest's fence lock.
pub(crate) async fn adopt_attachment_condemnations(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    catalog_id: &str,
    generation: &lash_core_execution::AttachmentSweepGeneration,
    now: u64,
) -> Result<lash_core_execution::AttachmentCondemnationAdoption, StoreError> {
    let mine = sweep_generation_sql(generation)?;
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let rows = sqlx::query_as::<_, (String, i64, String, i32, Option<String>, i64)>(
        attachment_sql().condemnation.select_adoptable.sql(),
    )
    .bind(mine)
    .fetch_all(pool)
    .await
    .map_err(store_sqlx_error)?;
    let mut dead = std::collections::BTreeMap::new();
    let mut adoption = lash_core_execution::AttachmentCondemnationAdoption::default();
    for (digest, owner, phase, delete_attempts, stall_reason, next_delete_at_ms) in rows {
        let id = attachment_id_from_sql("attachment condemnation", "attachment_id", digest)?;
        let stalled = stall_reason
            .map(|label| {
                lash_core_execution::AttachmentDeleteStallReason::from_label(&label).ok_or_else(
                    || StoreError::StoredDataCorrupt {
                        record_kind: "attachment condemnation",
                        message: format!("attachment `{id}` has unknown stall reason `{label}`"),
                    },
                )
            })
            .transpose()?;
        if phase != "deleting" && next_delete_at_ms > now {
            if stalled.is_some() {
                adoption.stalled.push(id);
            } else {
                adoption.backing_off.push(id);
            }
            continue;
        }
        let owner_dead = match dead.get(&owner) {
            Some(owner_dead) => *owner_dead,
            None => {
                let owner_dead = sweep_pass_is_dead(pool, catalog_id, owner).await?;
                dead.insert(owner, owner_dead);
                owner_dead
            }
        };
        if !owner_dead {
            if stalled.is_some() {
                adoption.stalled.push(id.clone());
            }
            adoption.held_by_live_pass.push(id);
            continue;
        }
        let mut tx = crate::begin_guarded(pool, fence).await?;
        lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let adopted = sqlx::query(attachment_sql().condemnation.adopt.sql())
            .bind(id.as_str())
            .bind(mine)
            .bind(owner)
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        if adopted == 1 {
            let phase = match phase.as_str() {
                "condemned" => lash_core_execution::AttachmentCondemnationPhase::Condemned,
                "deleting" => lash_core_execution::AttachmentCondemnationPhase::Deleting,
                unknown => {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "attachment condemnation",
                        message: format!("attachment `{id}` has unknown phase `{unknown}`"),
                    });
                }
            };
            let delete_attempts =
                u32::try_from(delete_attempts).map_err(|_| StoreError::StoredDataCorrupt {
                    record_kind: "attachment condemnation",
                    message: format!(
                        "attachment `{id}` has delete attempt count {delete_attempts}"
                    ),
                })?;
            adoption
                .adopted
                .push(lash_core_execution::AdoptedAttachmentCondemnation {
                    digest: id,
                    phase,
                    delete_attempts,
                    stalled,
                });
        }
    }
    Ok(adoption)
}

/// Settle one condemnation `generation` owns, under the digest's fence lock.
/// A restoring writer's token is never cleared here.
pub(crate) async fn settle_attachment_condemnation(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    attachment_id: &str,
    generation: &lash_core_execution::AttachmentSweepGeneration,
    settlement: lash_core_execution::AttachmentCondemnationSettlement,
    now: u64,
) -> Result<lash_core_execution::AttachmentSettlementOutcome, StoreError> {
    let generation = sweep_generation_sql(generation)?;
    let mut tx = crate::begin_guarded(pool, fence).await?;
    lock_attachment_fence_tx(&mut tx, attachment_id).await?;
    let statements = &attachment_sql().condemnation;
    let settled = match settlement {
        lash_core_execution::AttachmentCondemnationSettlement::Deleted => {
            sqlx::query(statements.delete_armed.sql())
                .bind(attachment_id)
                .bind(generation)
                .execute(&mut **tx)
                .await
        }
        lash_core_execution::AttachmentCondemnationSettlement::Spared => {
            sqlx::query(statements.delete_spared.sql())
                .bind(attachment_id)
                .bind(generation)
                .execute(&mut **tx)
                .await
        }
        lash_core_execution::AttachmentCondemnationSettlement::Failed { stall, error } => {
            sqlx::query(statements.record_failed_delete.sql())
                .bind(attachment_id)
                .bind(generation)
                .bind(error)
                .bind(stall.map(|reason| reason.as_str()))
                .bind(
                    i64::try_from(now)
                        .unwrap_or(i64::MAX)
                        .min(i64::MAX - 900_000),
                )
                .execute(&mut **tx)
                .await
        }
    }
    .map_err(store_sqlx_error)?
    .rows_affected();
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(if settled == 1 {
        lash_core_execution::AttachmentSettlementOutcome::Applied
    } else {
        lash_core_execution::AttachmentSettlementOutcome::NotOwned
    })
}

pub(crate) fn sweep_generation_sql(
    generation: &lash_core_execution::AttachmentSweepGeneration,
) -> Result<i64, StoreError> {
    i64::try_from(generation.generation()).map_err(|_| {
        StoreError::Backend(format!(
            "attachment sweep generation {} exceeds the stored range",
            generation.generation()
        ))
    })
}

pub(crate) async fn list_attachment_condemnations(
    pool: &PgPool,
) -> Result<Vec<lash_core_execution::AttachmentCondemnationRecord>, StoreError> {
    let rows = sqlx::query(attachment_sql().condemnation.select_all.sql())
        .fetch_all(pool)
        .await
        .map_err(store_sqlx_error)?;
    let mut rows = rows
        .into_iter()
        .map(|row| {
            let write_referrer = match (
                row.get::<Option<String>, _>(3),
                row.get::<Option<String>, _>(4),
            ) {
                (None, None) => None,
                (Some(kind), Some(id)) => Some((kind, id)),
                _ => {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "attachment condemnation",
                        message: "incomplete writer referrer".into(),
                    });
                }
            };
            lash_core_execution::store::decode_attachment_condemnation_record(
                lash_core_execution::store::StoredAttachmentCondemnation {
                    digest: attachment_id_from_sql(
                        "attachment condemnation",
                        "attachment_id",
                        row.get(0),
                    )?,
                    phase: row.get(1),
                    write_token_present: row.get::<Option<String>, _>(2).is_some(),
                    write_referrer,
                    delete_attempts: i64::from(row.get::<i32, _>(5)),
                    last_delete_error: row.get(6),
                    stall_reason: row.get(7),
                },
            )
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    rows.sort_by(|a, b| a.digest.cmp(&b.digest));
    Ok(rows)
}
pub(crate) async fn recover_abandoned_attachment_write(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    id: &str,
) -> Result<(), StoreError> {
    // Discover the referrer without a lock, then re-read the claim after taking
    // its referrer and digest locks in global order. End may have released it.
    let claim = sqlx::query_as::<_, (String, String, String)>(
        attachment_sql().condemnation.select_claim.sql(),
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(store_sqlx_error)?;
    let Some((token, kind, referrer_id)) = claim else {
        return Ok(());
    };
    let referrer = ArtifactReferrer::decode(&kind, &referrer_id)
        .map_err(|error| error.into_store_error("attachment pending write"))?;
    let id = AttachmentId::parse(id).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind: "attachment condemnation",
        message: error.to_string(),
    })?;
    let mut tx = crate::begin_guarded(pool, fence).await?;
    lock_attachment_referrer_tx(&mut tx, &referrer).await?;
    lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
    let current = sqlx::query_as::<_, (String, String, String)>(
        attachment_sql().condemnation.select_claim.sql(),
    )
    .bind(id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if current
        .as_ref()
        .is_some_and(|claim| claim == &(token.clone(), kind, referrer_id))
    {
        abort_write_tx(&mut tx, &id, &referrer, &token).await?;
    }
    tx.commit().await.map_err(store_sqlx_error)
}
#[async_trait::async_trait]
impl AttachmentReferrers for PostgresStore {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<lash_core_execution::AttachmentWriteFence, StoreError> {
        let referrer = write.claim.referrer();
        let token = lash_core_execution::AttachmentWriteToken::new();
        let now = self.clock.timestamp_ms();
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        lock_attachment_referrer_tx(&mut tx, referrer).await?;
        check_fence_tx(&mut tx, referrer).await?;
        lock_attachment_fence_tx(&mut tx, write.attachment_id.as_str()).await?;
        let condemnation = sqlx::query_as::<_, (String, Option<String>)>(
            attachment_sql().condemnation.select_phase_and_claim.sql(),
        )
        .bind(write.attachment_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        #[cfg(test)]
        if condemnation.is_some() {
            let delay = FENCE_WRITER_WINDOW_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
        match condemnation
            .as_ref()
            .map(|(phase, claim)| (phase.as_str(), claim.is_some()))
        {
            Some(("deleting", _)) | Some(("condemned", true)) => {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(lash_core_execution::AttachmentWriteFence::ReclamationInFlight);
            }
            None | Some(("condemned", false)) => {}
            Some((phase, _)) => {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "attachment condemnation",
                    message: format!("unknown phase {phase}"),
                });
            }
        }
        sqlx::query(attachment_sql().pending.insert.sql())
            .bind(token.as_hex())
            .bind(write.attachment_id.as_str())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .bind(clamp_epoch_ms(now))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if condemnation.is_some() {
            sqlx::query(attachment_sql().condemnation.claim_write.sql())
                .bind(write.attachment_id.as_str())
                .bind(token.as_hex())
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        insert_edge_tx(&mut tx, referrer, &write.attachment_id).await?;
        if let Some(cleanup) = write.claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, now).await?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::AttachmentWriteFence::Granted(
            lash_core_execution::AttachmentWritePermit::new(token),
        ))
    }
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: lash_core_execution::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let referrer = write.claim.referrer();
        let token = permit.write_id().as_hex();
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        lock_attachment_referrer_tx(&mut tx, referrer).await?;
        lock_attachment_fence_tx(&mut tx, write.attachment_id.as_str()).await?;
        if !has_permit_tx(&mut tx, write, &token).await? {
            return Err(StoreError::StaleWritePermit {
                digest: write.attachment_id.clone(),
            });
        }
        sqlx::query(attachment_sql().uploads.insert.sql())
            .bind(write.attachment_id.as_str())
            .bind(clamp_epoch_ms(self.clock.timestamp_ms()))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(attachment_sql().condemnation.delete_by_write_token.sql())
            .bind(write.attachment_id.as_str())
            .bind(&token)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(attachment_sql().pending.delete_permit.sql())
            .bind(token)
            .bind(write.attachment_id.as_str())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: lash_core_execution::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        let token = permit.write_id().as_hex();
        lock_attachment_referrer_tx(&mut tx, write.claim.referrer()).await?;
        lock_attachment_fence_tx(&mut tx, write.attachment_id.as_str()).await?;
        if has_permit_tx(&mut tx, write, &token).await? {
            abort_write_tx(
                &mut tx,
                &write.attachment_id,
                write.claim.referrer(),
                &token,
            )
            .await?;
        }
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        lock_attachment_referrer_tx(&mut tx, claim.referrer()).await?;
        acquire_attachment_refs_tx(&mut tx, claim, ids, self.clock.timestamp_ms()).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        if let ArtifactReferrer::Session(session) = referrer {
            crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, session).await?;
        }
        lock_attachment_referrer_tx(&mut tx, referrer).await?;
        lock_attachment_fence_tx(&mut tx, id.as_str()).await?;
        let fenced: bool = sqlx::query_scalar(
            crate::artifact_store::artifact_sql()
                .fences
                .select_is_fenced
                .sql(),
        )
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if !fenced {
            let retained = if let ArtifactReferrer::Session(session) = referrer {
                let row: (bool, bool, bool) =
                    sqlx::query_as(attachment_sql().postgres.session_state.sql())
                        .bind(session.as_str())
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?;
                row.2
            } else {
                false
            };
            if !retained {
                sqlx::query(attachment_sql().edges.delete_ref.sql())
                    .bind(id.as_str())
                    .bind(referrer.kind().as_str())
                    .bind(referrer.canonical_id())
                    .execute(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?;
            }
        }
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn end_attachment_referrer(&self, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        lock_attachment_referrer_tx(&mut tx, referrer).await?;
        let ids: Vec<String> =
            sqlx::query_scalar(attachment_sql().pending.select_referrer_digests.sql())
                .bind(referrer.kind().as_str())
                .bind(referrer.canonical_id())
                .fetch_all(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        for id in ids {
            lock_attachment_fence_tx(&mut tx, &id).await?;
        }
        sqlx::query(
            crate::artifact_store::artifact_sql()
                .fences
                .insert_fence
                .sql(),
        )
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .bind(clamp_epoch_ms(self.clock.timestamp_ms()))
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        sqlx::query(attachment_sql().pending.delete_referrer.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(attachment_sql().edges.delete_referrer.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn session_referrer_state(
        &self,
        session: &SessionId,
    ) -> Result<SessionReferrerState, StoreError> {
        let (metadata, deleted, retained): (bool, bool, bool) =
            sqlx::query_as(attachment_sql().postgres.session_state.sql())
                .bind(session.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(store_sqlx_error)?;
        Ok(if !metadata && !deleted {
            SessionReferrerState::Absent
        } else if metadata && !deleted {
            SessionReferrerState::Live
        } else if retained {
            SessionReferrerState::DeletedRetained
        } else {
            SessionReferrerState::DeletedRetired
        })
    }
    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as(attachment_sql().edges.select_referrers.sql())
                .bind(id.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(store_sqlx_error)?;
        rows.into_iter()
            .map(|(kind, id)| {
                ArtifactReferrer::decode(&kind, &id)
                    .map_err(|error| error.into_store_error("attachment referrer edge"))
            })
            .collect()
    }
}
