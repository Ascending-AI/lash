//! The in-memory backend's session-execution-lease lane.
//!
//! Split out of the parent module to stay inside the production file-size budget;
//! the semantics live with the trait contract in `crate::store`.

use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

#[async_trait::async_trait]
impl crate::store::SessionExecutionLeaseStore for InMemorySessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        claim_nonce: &crate::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLeaseClaimOutcome, crate::store::StoreError> {
        let lease_token = claim_nonce.as_str();
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        let mut leases = self.session_execution_leases.lock_recover();
        let current = leases.entry(session_id.to_string()).or_default();
        if current.is_live(now) {
            if current
                .owner
                .as_ref()
                .is_some_and(|current_owner| current_owner.same_incarnation(owner))
            {
                if current.lease_token.as_deref() != Some(lease_token) {
                    current.lease_token = Some(lease_token.to_string());
                }
                current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                // Reentry advances no generation, so it displaces nobody.
                return Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
                    crate::SessionExecutionLeaseAcquisition::fresh(
                        Self::in_memory_session_execution_lease(session_id, current),
                    ),
                ));
            }
            return Ok(crate::SessionExecutionLeaseClaimOutcome::Busy {
                holder: Self::in_memory_session_execution_lease(session_id, current),
            });
        }
        // Read the lapsed holder before overwriting it: this claim is the only
        // atomic moment a takeover is observable, and the displaced runner is
        // usually why the lease lapsed and so cannot be relied on to report it.
        let displaced = current
            .owner
            .clone()
            .filter(|previous| !previous.same_incarnation(owner))
            .map(|previous| (previous, current.fencing_token, current.expires_at_epoch_ms));
        let lease = Self::acquire_session_execution_lease_in_memory(
            session_id,
            owner,
            lease_token,
            current,
            now,
            lease_ttl_ms,
        )?;
        Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, generation, expired_at_epoch_ms)) => {
                    crate::SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => crate::SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &crate::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLease, crate::store::StoreError> {
        #[cfg(test)]
        {
            self.session_execution_lease_renewal_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let injected = self
                .fail_next_session_execution_lease_renewal
                .lock_recover()
                .take();
            if let Some(error) = injected {
                return Err(error);
            }
        }
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let mut leases = self.session_execution_leases.lock_recover();
        let Some(current) = leases.get_mut(&fence.session_id) else {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !current
            .owner
            .as_ref()
            .is_some_and(|owner| owner.same_incarnation(&fence.owner))
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
        {
            crate::store_backend_support::trace_session_execution_lease_refusal(
                crate::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "in_memory_write_transaction",
                fence,
                crate::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(
                crate::store::StoreError::SessionExecutionLeaseRenewalRefused {
                    session_id: fence.session_id.clone(),
                },
            );
        }
        if current.expires_at_epoch_ms <= now {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        let renewed = crate::SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: current.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_epoch_ms,
            expires_at_epoch_ms: current.expires_at_epoch_ms,
        };
        #[cfg(test)]
        if let Some(injected) = self
            .next_session_execution_lease_renewal_response
            .lock_recover()
            .take()
        {
            return Ok(injected);
        }
        Ok(renewed)
    }

    async fn release_session_execution_lease(
        &self,
        completion: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<(), crate::store::StoreError> {
        #[cfg(test)]
        {
            let gate = self
                .session_execution_lease_release_gate
                .lock_recover()
                .clone();
            if let Some(gate) = gate {
                gate.enter().await;
            }
        }
        {
            let _transaction = self.write_transaction.lock_recover();
            #[cfg(test)]
            self.session_execution_lease_release_attempt_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !self.release_session_execution_lease_in_memory(completion, true) {
                return Err(
                    crate::store::StoreError::SessionExecutionLeaseReleaseRefused {
                        session_id: completion.session_id.clone(),
                    },
                );
            }
        }
        Ok(())
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::SessionExecutionLease>, crate::store::StoreError> {
        #[cfg(test)]
        self.refuse_injected_counter_defect("session_lease_fencing_token")?;
        let leases = self.session_execution_leases.lock_recover();
        Ok(leases.get(session_id).and_then(|current| {
            // An unleased or released row keeps its generation but drops owner
            // and token; only a held row is reported. Expiry is not filtered:
            // a lapsed holder is the fact a triage read needs.
            (current.owner.is_some() && current.lease_token.is_some())
                .then(|| Self::in_memory_session_execution_lease(session_id, current))
        }))
    }
}
