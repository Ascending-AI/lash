use super::*;

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for Store {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let owner = owner.clone();
        let executor_id = executor_id.to_string();
        let lease_token = claim_nonce.as_str().to_string();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SessionExecutionLeaseClaimOutcome, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let current = load_session_execution_lease_row_conn(tx, &session_id)?;
                    if current.as_ref().is_some_and(|lease| {
                        lease.lease_token.is_some() && lease.expires_at_ms > now
                    }) {
                        let current = current.expect("checked current lease is present");
                        if current
                            .owner
                            .as_ref()
                            .is_some_and(|current_owner| current_owner.same_incarnation(&owner))
                            && current.executor_id.as_deref() == Some(executor_id.as_str())
                        {
                            let expires_at = now.saturating_add(lease_ttl_ms);
                            let sql_expires_at = sql_counter_value(
                                "session_execution_lease_expires_at_ms",
                                expires_at,
                            )?;
                            let sql_lease_term =
                                sql_counter_value("session_execution_lease_term_ms", lease_ttl_ms)?;
                            let claimed_at = current.claimed_at_ms;
                            tx.execute(
                                "UPDATE session_execution_leases
                                 SET lease_token = ?2,
                                     lease_claimed_at_ms = ?3,
                                     lease_expires_at_ms = ?4,
                                     lease_term_ms = ?5
                                 WHERE session_id = ?1",
                                params![
                                    session_id.as_str(),
                                    lease_token,
                                    claimed_at as i64,
                                    sql_expires_at,
                                    sql_lease_term
                                ],
                            )
                            .map_err(sqlite_error)?;
                            // Reentry advances no generation: nobody is displaced.
                            return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                                SessionExecutionLeaseAcquisition::fresh(SessionExecutionLease {
                                    session_id,
                                    owner,
                                    executor_id,
                                    lease_token,
                                    fencing_token: current.fencing_token,
                                    claimed_at_epoch_ms: claimed_at,
                                    lease_term_ms: lease_ttl_ms,
                                    expires_at_epoch_ms: expires_at,
                                }),
                            ));
                        }
                        return Ok(SessionExecutionLeaseClaimOutcome::Busy {
                            holder: row_to_session_execution_lease(&session_id, current)?,
                        });
                    }
                    // The lapsed holder, read inside the claim transaction. The
                    // winner is the only party guaranteed alive to report the
                    // takeover, so the row must hand it over here.
                    let displaced = current.as_ref().and_then(|lease| {
                        lease
                            .owner
                            .clone()
                            .zip(lease.executor_id.clone())
                            .filter(|(previous, previous_executor_id)| {
                                !previous.same_incarnation(&owner)
                                    || previous_executor_id != &executor_id
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
                    let acquired = acquire_session_execution_lease_conn(
                        tx,
                        lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity {
                            session_id: &session_id,
                            owner: &owner,
                            executor_id: &executor_id,
                            lease_token: &lease_token,
                        },
                        current.as_ref().map_or(0, |lease| lease.fencing_token),
                        now,
                        lease_ttl_ms,
                    )?;
                    // FIG-1573: this claim deliberately does NOT repair
                    // orphaned active-turn inputs. A takeover proves the
                    // previous *runner* is gone, not that its turn is: cold
                    // recovery resumes the interrupted turn under the same turn
                    // id at the new generation, and it must still receive the
                    // inputs pinned to it (proved by the cold-process crash
                    // matrix, which fails with a replay hash conflict if they
                    // are swept here). The repair belongs to the runtime, which
                    // knows whether the turn is coming back.
                    Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                        match displaced {
                            Some((
                                previous,
                                previous_executor_id,
                                generation,
                                expired_at_epoch_ms,
                            )) => SessionExecutionLeaseAcquisition::displacing_observed(
                                acquired,
                                previous,
                                previous_executor_id,
                                generation,
                                expired_at_epoch_ms,
                            ),
                            None => SessionExecutionLeaseAcquisition::fresh(acquired),
                        },
                    ))
                })(
                );
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let fence = fence.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SessionExecutionLease, StoreError> = (|| {
                    let current = load_session_execution_lease_row_conn(tx, &fence.session_id)?;
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
                            "sqlite_write_transaction",
                            &fence,
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
                    let sql_expires_at = sql_counter_value(
                        "session_execution_lease_expires_at_ms",
                        expires_at,
                    )?;
                    let sql_lease_term = sql_counter_value(
                        "session_execution_lease_term_ms",
                        lease_ttl_ms,
                    )?;
                    let renewed = tx.execute(
                        "UPDATE session_execution_leases
                         SET lease_expires_at_ms = ?6,
                             lease_term_ms = ?7
                         WHERE session_id = ?1
                           AND lease_owner_id = ?2
                           AND lease_owner_incarnation_id = ?3
                           AND lease_executor_id = ?4
                           AND lease_token = ?5",
                        params![
                            fence.session_id.as_str(),
                            fence.owner.owner_id.as_str(),
                            fence.owner.incarnation_id.as_str(),
                            fence.executor_id.as_str(),
                            fence.lease_token,
                            sql_expires_at,
                            sql_lease_term
                        ],
                    )
                    .map_err(sqlite_error)?;
                    if renewed != 1 {
                        lash_core::store_backend_support::trace_session_execution_lease_refusal(
                            lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                            "conditional_update_did_not_match",
                            "sqlite_write_transaction",
                            &fence,
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
                    Ok(SessionExecutionLease {
                        session_id: fence.session_id,
                        owner: fence.owner,
                        executor_id: fence.executor_id,
                        lease_token: fence.lease_token,
                        fencing_token: current.fencing_token,
                        claimed_at_epoch_ms: current.claimed_at_ms,
                        lease_term_ms: lease_ttl_ms,
                        expires_at_epoch_ms: expires_at,
                    })
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let completion = completion.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    let current =
                        load_session_execution_lease_row_conn(tx, &completion.session_id)?;
                    if !release_session_execution_lease_conn(tx, &completion)? {
                        lash_core::store_backend_support::trace_session_execution_lease_refusal(
                            lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                            "token_scoped_release_did_not_match",
                            "sqlite_write_transaction",
                            &completion,
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
                    Ok(())
                })();
                match outcome {
                    Ok(()) => Ok(TxOutcome::Commit(Ok(()))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::SessionExecutionLeaseObservation, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let observed_at_epoch_ms = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Option<SessionExecutionLease>, StoreError> = (|| {
                    let Some(row) = load_session_execution_lease_row_conn(conn, &session_id)?
                    else {
                        return Ok(None);
                    };
                    // A released row keeps its generation but clears owner and
                    // token. Expiry is reported as a raw fact, not filtered.
                    if row.owner.is_none() || row.lease_token.is_none() {
                        return Ok(None);
                    }
                    Ok(Some(row_to_session_execution_lease(&session_id, row)?))
                })(
                );
                Ok(
                    outcome.map(|lease| lash_core::SessionExecutionLeaseObservation {
                        observed_at_epoch_ms,
                        lease,
                    }),
                )
            })
            .await
            .map_err(sqlite_error)?
    }
}
