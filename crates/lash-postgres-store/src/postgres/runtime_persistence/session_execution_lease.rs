use super::*;

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for PostgresSessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let lease_token = claim_nonce.as_str();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        lock_session_execution_lease_tx(&mut tx, session_id).await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let current = load_session_execution_lease_tx(&mut tx, session_id).await?;
        if current
            .as_ref()
            .is_some_and(|lease| lease.lease_token.is_some() && lease.expires_at_ms > now)
        {
            let current = current.expect("checked current lease is present");
            if current
                .owner
                .as_ref()
                .is_some_and(|current_owner| current_owner.same_incarnation(owner))
                && current.executor_id.as_deref() == Some(executor_id)
            {
                let expires_at = now.saturating_add(lease_ttl_ms);
                let sql_expires_at =
                    sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
                let sql_lease_term =
                    sql_counter_value("session_execution_lease_term_ms", lease_ttl_ms)?;
                let claimed_at = current.claimed_at_ms;
                sqlx::query(
                    "UPDATE lash_session_execution_leases
                     SET lease_token = $2,
                         lease_claimed_at_ms = $3,
                         lease_expires_at_ms = $4,
                         lease_term_ms = $5
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .bind(lease_token)
                .bind(claimed_at as i64)
                .bind(sql_expires_at)
                .bind(sql_lease_term)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                tx.commit().await.map_err(store_sqlx_error)?;
                // Reentry advances no generation: nobody is displaced.
                return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                    SessionExecutionLeaseAcquisition::fresh(SessionExecutionLease {
                        session_id: session_id.to_string(),
                        owner: owner.clone(),
                        executor_id: executor_id.to_string(),
                        lease_token: lease_token.to_string(),
                        fencing_token: current.fencing_token,
                        claimed_at_epoch_ms: claimed_at,
                        lease_term_ms: lease_ttl_ms,
                        expires_at_epoch_ms: expires_at,
                    }),
                ));
            }
            let holder = row_to_session_execution_lease(session_id, current)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(SessionExecutionLeaseClaimOutcome::Busy { holder });
        }
        let previous_fencing_token = current.as_ref().map_or(0, |lease| lease.fencing_token);
        // The lapsed holder, read under the same row lock as the claim. The
        // winner is the only party guaranteed alive to report the takeover.
        let displaced = current.as_ref().and_then(|lease| {
            lease
                .owner
                .clone()
                .zip(lease.executor_id.clone())
                .filter(|(previous, previous_executor_id)| {
                    !previous.same_incarnation(owner) || previous_executor_id != executor_id
                })
                .map(|(previous, previous_executor_id)| {
                    (
                        previous,
                        previous_executor_id,
                        lease.fencing_token,
                        lease.expires_at_ms,
                    )
                })
        });
        let lease = acquire_session_execution_lease_tx(
            &mut tx,
            lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity {
                session_id,
                owner,
                executor_id,
                lease_token,
            },
            previous_fencing_token,
            now,
            lease_ttl_ms,
        )
        .await?;
        // FIG-1573: no orphan repair here. A takeover proves the previous runner
        // is gone, not that its turn is - cold recovery resumes the interrupted
        // turn under the same turn id at the new generation and must still
        // receive the inputs pinned to it. The runtime owns the repair.
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        // Keep claim and renewal on one explicit per-session lock order. The
        // row read below was already `FOR UPDATE`, so this is a hardening pin
        // and an auditable lock-ordering rule, not a repair for a reachable
        // stale-read race.
        lock_session_execution_lease_tx(&mut tx, &fence.session_id).await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let current = load_session_execution_lease_tx(&mut tx, &fence.session_id).await?;
        let Some(current) = current else {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !current
            .owner
            .as_ref()
            .is_some_and(|owner| owner.same_incarnation(&fence.owner))
            || current.executor_id.as_deref() != Some(fence.executor_id.as_str())
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
        {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "postgres_locked_transaction",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.executor_id.as_deref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseRenewalRefused {
                session_id: fence.session_id.clone(),
            });
        }
        if current.expires_at_ms <= now {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        let expires_at = now.saturating_add(lease_ttl_ms);
        let sql_expires_at =
            sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
        let sql_lease_term = sql_counter_value("session_execution_lease_term_ms", lease_ttl_ms)?;
        let renewed = sqlx::query(
            "UPDATE lash_session_execution_leases
             SET lease_expires_at_ms = $6,
                 lease_term_ms = $7
             WHERE session_id = $1
               AND lease_owner_id = $2
               AND lease_owner_incarnation_id = $3
               AND lease_executor_id = $4
               AND lease_token = $5",
        )
        .bind(&fence.session_id)
        .bind(&fence.owner.owner_id)
        .bind(&fence.owner.incarnation_id)
        .bind(&fence.executor_id)
        .bind(&fence.lease_token)
        .bind(sql_expires_at)
        .bind(sql_lease_term)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if renewed.rows_affected() != 1 {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "conditional_update_did_not_match",
                "postgres_locked_transaction",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.executor_id.as_deref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseRenewalRefused {
                session_id: fence.session_id.clone(),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            executor_id: fence.executor_id.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: current.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_ms,
            lease_term_ms: lease_ttl_ms,
            expires_at_epoch_ms: expires_at,
        })
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        if !release_session_execution_lease_tx(&mut tx, completion).await? {
            let current = load_session_execution_lease_tx(&mut tx, &completion.session_id).await?;
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                "token_scoped_release_did_not_match",
                "postgres_locked_transaction",
                completion,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.as_ref().and_then(|lease| lease.owner.as_ref()),
                    current
                        .as_ref()
                        .and_then(|lease| lease.executor_id.as_deref()),
                    current
                        .as_ref()
                        .and_then(|lease| lease.lease_token.as_deref()),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseReleaseRefused {
                session_id: completion.session_id.clone(),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<lash_core::SessionExecutionLeaseObservation, StoreError> {
        // Non-locking on purpose: observation must never be able to delay the
        // lane it observes. See `read_session_execution_lease_unlocked`.
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let observed_at_epoch_ms = postgres_transaction_epoch_ms(&mut tx).await?;
        let current = read_session_execution_lease_unlocked(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        // A released row keeps its generation but clears owner and token; only a
        // held row is reported. Expiry stays a raw fact for the caller.
        let lease = current
            .filter(|lease| lease.owner.is_some() && lease.lease_token.is_some())
            .map(|row| row_to_session_execution_lease(session_id, row))
            .transpose()?;
        Ok(lash_core::SessionExecutionLeaseObservation {
            observed_at_epoch_ms,
            lease,
        })
    }
}
