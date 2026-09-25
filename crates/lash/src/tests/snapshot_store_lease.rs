use super::*;

/// SnapshotStore's execution lane: one lease per session, fenced exactly as
/// a durable store fences it, so the facade tests exercise the real claim,
/// renewal and release contract.
#[async_trait]
impl lash_core::SessionExecutionLeaseStore for SnapshotStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        let lease_token = claim_nonce.as_str();
        let mut leases = self.session_execution_leases.lock_recover();
        if let Some(existing) = leases.get(session_id)
            && existing.expires_at_epoch_ms > now_epoch_ms()
        {
            if existing.owner.same_incarnation(owner) && existing.executor_id == executor_id {
                let mut lease = existing.clone();
                if lease.lease_token != lease_token {
                    lease.lease_token = lease_token.to_string();
                }
                lease.expires_at_epoch_ms = now_epoch_ms().saturating_add(lease_ttl_ms);
                leases.insert(SessionId::from(session_id.to_string()), lease.clone());
                return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
                    lash_core::SessionExecutionLeaseAcquisition::fresh(lease),
                ));
            }
            return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Busy {
                holder: existing.clone(),
            });
        }
        // The lapsed holder this claim takes the lane from, read before the
        // overwrite. A double that reports no displacement would silently
        // disable the takeover event for every facade test that runs on it.
        let displaced = leases.get(session_id).and_then(|previous| {
            (!previous.owner.same_incarnation(owner) || previous.executor_id != executor_id).then(
                || {
                    (
                        previous.owner.clone(),
                        previous.executor_id.clone(),
                        previous.fencing_token,
                        previous.expires_at_epoch_ms,
                    )
                },
            )
        });
        // Mint from the retained counter, not from the live row: the row is gone
        // after a release, and restarting the fence there would reissue a
        // generation a stale claim still pins.
        let mut generations = self.session_execution_lease_generations.lock_recover();
        let next_fencing_token = generations
            .get(session_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(SessionId::from(session_id.to_string()), next_fencing_token);
        drop(generations);
        let mut lease = test_session_execution_lease(
            session_id,
            owner,
            executor_id,
            lease_ttl_ms,
            next_fencing_token,
        );
        lease.lease_token = lease_token.to_string();
        leases.insert(SessionId::from(session_id.to_string()), lease.clone());
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    lash_core::SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => lash_core::SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> std::result::Result<lash_core::SessionExecutionLease, lash_core::store::StoreError> {
        let mut leases = self.session_execution_leases.lock_recover();
        let Some(existing) = leases.get_mut(&fence.session_id) else {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !session_fence_matches(existing, fence) {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "facade_test_double_lock",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    Some(&existing.owner),
                    Some(existing.executor_id.as_str()),
                    Some(existing.lease_token.as_str()),
                ),
            );
            return Err(
                lash_core::store::StoreError::SessionExecutionLeaseRenewalRefused {
                    session_id: fence.session_id.clone(),
                },
            );
        }
        if existing.expires_at_epoch_ms <= now_epoch_ms() {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        existing.expires_at_epoch_ms = now_epoch_ms().saturating_add(lease_ttl_ms);
        Ok(existing.clone())
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        let mut leases = self.session_execution_leases.lock_recover();
        if leases
            .get(&completion.session_id)
            .is_some_and(|lease| session_completion_matches(lease, completion))
        {
            // The live row goes; the generation counter deliberately stays, so
            // the next claim mints `previous + 1` (ADR 0029).
            leases.remove(&completion.session_id);
            Ok(())
        } else {
            let current = leases.get(&completion.session_id);
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                "token_scoped_release_did_not_match",
                "facade_test_double_lock",
                completion,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.map(|lease| &lease.owner),
                    current.map(|lease| lease.executor_id.as_str()),
                    current.map(|lease| lease.lease_token.as_str()),
                ),
            );
            Err(
                lash_core::store::StoreError::SessionExecutionLeaseReleaseRefused {
                    session_id: completion.session_id.clone(),
                },
            )
        }
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<SessionExecutionLeaseObservation, StoreError> {
        let lease = self
            .session_execution_leases
            .lock_recover()
            .get(session_id)
            .cloned();
        Ok(SessionExecutionLeaseObservation {
            observed_at_epoch_ms: now_epoch_ms(),
            lease,
        })
    }
}
