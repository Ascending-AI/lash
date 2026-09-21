//! The [`lash_core::ProcessLeases`] concern for the Postgres registry.

use super::*;
use lash_sansio::ProcessId;

#[async_trait::async_trait]
impl lash_core::ProcessLeases for PostgresProcessRegistry {
    async fn claim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        let fencing_token = match registry_transitions::decide_process_lease_claim(
            current.as_ref(),
            owner,
            now,
            lease_ttl_ms,
        ) {
            registry_transitions::ProcessLeaseClaimDecision::ExtendHeldLease { lease } => {
                // Same incarnation re-enters its own live lease: extend the
                // expiry, keep token and fencing token.
                sqlx::query(process_sql().lease.extend_unfenced.sql())
                    .bind(process_id.as_str())
                    .bind(lease.expires_at_epoch_ms as i64)
                    .execute(&mut *tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease));
            }
            registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder });
            }
            registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
                registry_transitions::next_process_lease_fencing_token(
                    retained_process_lease_fencing_token(&mut tx, process_id).await?,
                )?
            }
        };
        let lease =
            acquire_process_lease_tx(&mut tx, process_id, owner, fencing_token, now, lease_ttl_ms)
                .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &LeaseOwnerIdentity,
        _observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        let fencing_token =
            match registry_transitions::decide_process_lease_reclaim(current.as_ref(), now)? {
                registry_transitions::ProcessLeaseReclaimDecision::AcquireOnRetainedFence => {
                    // Free (or released) lease: acquire on the retained fencing
                    // token like a plain claim would.
                    registry_transitions::next_process_lease_fencing_token(
                        retained_process_lease_fencing_token(&mut tx, process_id).await?,
                    )?
                }
                ProcessLeaseReclaimDecision::AcquireOnObservedFence { fencing_token } => {
                    fencing_token
                }
                registry_transitions::ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                    tx.commit().await.map_err(plugin_sqlx_error)?;
                    return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder });
                }
            };
        let lease =
            acquire_process_lease_tx(&mut tx, process_id, owner, fencing_token, now, lease_ttl_ms)
                .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, &lease.process_id).await?;
        registry_transitions::authorize_process_lease_write(
            &lease.process_id,
            lease,
            current.as_ref(),
            now,
        )?;
        let renewed = ProcessLease {
            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
            ..lease.clone()
        };
        sqlx::query(process_sql().lease.renew_fenced.sql())
            .bind(renewed.process_id.as_str())
            .bind(renewed.expires_at_epoch_ms as i64)
            .bind(&renewed.lease_token)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(renewed)
    }

    async fn get_process_lease(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessLease>, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let lease = load_process_lease_tx(&mut tx, process_id).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lease)
    }

    async fn get_process_leases(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<Option<ProcessLease>>, PluginError> {
        if process_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(process_sql().lease_postgres.list_by_process_ids.sql())
            .bind(
                process_ids
                    .iter()
                    .map(ProcessId::as_str)
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut leases_by_id = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let process_id: ProcessId = ProcessId::from(row.get::<String, _>(0));
            let lease = facade_support::registry_transitions::ProcessLeaseRow {
                owner_id: row.get(1),
                incarnation_id: row.get(6),
                lease_token: row.get(2),
                fencing_token: row.get(3),
                claimed_at_ms: row.get(4),
                expires_at_ms: row.get(5),
            }
            .project(&process_id);
            leases_by_id.insert(process_id, lease);
        }
        Ok(process_ids
            .iter()
            .map(|process_id| leases_by_id.get(process_id).cloned().flatten())
            .collect())
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError> {
        // The same release decision `complete_process_with_lease` makes
        // (FIG-3388): lock the row, run the shared verdict, and let the one
        // release statement's predicate backstop it. A stale or superseded
        // presentation is a no-op, not an error — release is idempotent.
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_row_tx(&mut tx, &completion.process_id).await?;
        let verdict = lash_core::store_backend_support::process_lease_verdict(
            current
                .as_ref()
                .map(registry_transitions::ProcessLeaseRow::facts),
            lash_core::store_backend_support::ProcessLeaseAuthority {
                lease_token: &completion.lease_token,
                fencing_token: completion.fencing_token,
            },
            now,
        );
        if matches!(
            verdict,
            lash_core::store_backend_support::ProcessLeaseVerdict::Current
                | lash_core::store_backend_support::ProcessLeaseVerdict::Expired
        ) {
            // An expired lease still clears: the holder fields belong to the
            // lapsed claim and the retained fencing token is what a re-claim
            // builds on.
            let released = sqlx::query(process_sql().lease.release.sql())
                .bind(completion.process_id.as_str())
                .bind(&completion.lease_token)
                .bind(completion.fencing_token as i64)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected();
            lash_core::store_backend_support::require_fenced_write_applied(
                lash_core::store_backend_support::FencedWrite::ProcessLeaseRelease,
                crate::POSTGRES_BACKEND,
                completion.process_id.as_str(),
                released,
                || PluginError::ProcessLeaseSuperseded {
                    process_id: completion.process_id.clone(),
                },
            )?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }
}
