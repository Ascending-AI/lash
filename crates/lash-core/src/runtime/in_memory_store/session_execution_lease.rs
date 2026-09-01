//! The in-memory backend's session-execution-lease lane.
//!
//! Split out of the parent module to stay inside the production file-size budget;
//! the semantics live with the trait contract in `crate::store`.

use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

#[derive(Clone)]
pub(super) enum Lease {
    Free,
    Held {
        owner: crate::LeaseOwnerIdentity,
        executor_id: String,
        lease_token: String,
        claimed_at_epoch_ms: u64,
        lease_term_ms: u64,
        expires_at_epoch_ms: u64,
    },
}

#[derive(Clone)]
pub(super) struct InMemorySessionExecutionLease {
    pub(super) lease: Lease,
    pub(super) fencing_token: u64,
}

#[derive(Clone, Copy)]
pub(super) struct HeldLeaseFields<'a> {
    pub(super) owner: &'a crate::LeaseOwnerIdentity,
    pub(super) executor_id: &'a str,
    pub(super) lease_token: &'a str,
    pub(super) claimed_at_epoch_ms: u64,
    pub(super) lease_term_ms: u64,
    pub(super) expires_at_epoch_ms: u64,
}

impl Default for InMemorySessionExecutionLease {
    fn default() -> Self {
        Self {
            lease: Lease::Free,
            fencing_token: 0,
        }
    }
}

impl InMemorySessionExecutionLease {
    pub(super) fn is_held(&self) -> bool {
        matches!(&self.lease, Lease::Held { .. })
    }

    pub(super) fn is_live(&self, now: u64) -> bool {
        matches!(
            &self.lease,
            Lease::Held {
                expires_at_epoch_ms,
                ..
            } if *expires_at_epoch_ms > now
        )
    }

    pub(super) fn is_held_by(&self, owner: &crate::LeaseOwnerIdentity, executor_id: &str) -> bool {
        matches!(
            &self.lease,
            Lease::Held {
                owner: current_owner,
                executor_id: current_executor_id,
                ..
            } if current_owner.same_incarnation(owner) && current_executor_id == executor_id
        )
    }

    pub(super) fn lease_token_matches(&self, lease_token: &str) -> bool {
        matches!(
            &self.lease,
            Lease::Held {
                lease_token: current_lease_token,
                ..
            } if current_lease_token == lease_token
        )
    }

    pub(super) fn held_fields(&self) -> Option<HeldLeaseFields<'_>> {
        match &self.lease {
            Lease::Free => None,
            Lease::Held {
                owner,
                executor_id,
                lease_token,
                claimed_at_epoch_ms,
                lease_term_ms,
                expires_at_epoch_ms,
            } => Some(HeldLeaseFields {
                owner,
                executor_id,
                lease_token,
                claimed_at_epoch_ms: *claimed_at_epoch_ms,
                lease_term_ms: *lease_term_ms,
                expires_at_epoch_ms: *expires_at_epoch_ms,
            }),
        }
    }

    pub(super) fn fence_facts(
        &self,
    ) -> crate::store::session_execution_lease::SessionExecutionLeaseFenceFacts<'_> {
        let held = self.held_fields();
        crate::store::session_execution_lease::SessionExecutionLeaseFenceFacts {
            owner: held.map(|fields| fields.owner),
            executor_id: held.map(|fields| fields.executor_id),
            lease_token: held.map(|fields| fields.lease_token),
            fencing_token: self.fencing_token,
            expires_at_epoch_ms: held.map_or(0, |fields| fields.expires_at_epoch_ms),
        }
    }
}

#[async_trait::async_trait]
impl crate::store::SessionExecutionLeaseStore for InMemorySessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        executor_id: &str,
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
            if current.is_held_by(owner, executor_id) {
                if let super::Lease::Held {
                    lease_token: current_lease_token,
                    lease_term_ms,
                    expires_at_epoch_ms,
                    ..
                } = &mut current.lease
                {
                    if current_lease_token != lease_token {
                        *current_lease_token = lease_token.to_string();
                    }
                    *lease_term_ms = lease_ttl_ms;
                    *expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                }
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
        let displaced = match &current.lease {
            super::Lease::Free => None,
            super::Lease::Held {
                owner: previous,
                executor_id: previous_executor_id,
                expires_at_epoch_ms,
                ..
            } if previous.same_incarnation(owner) && previous_executor_id == executor_id => None,
            super::Lease::Held {
                owner: previous,
                executor_id: previous_executor_id,
                expires_at_epoch_ms,
                ..
            } => Some((
                previous.clone(),
                previous_executor_id.clone(),
                current.fencing_token,
                *expires_at_epoch_ms,
            )),
        };
        let lease = Self::acquire_session_execution_lease_in_memory(
            session_id,
            owner,
            executor_id,
            lease_token,
            current,
            now,
            lease_ttl_ms,
        )?;
        // FIG-1573: no orphan repair here. A takeover proves the previous runner
        // is gone, not that its turn is - cold recovery resumes the interrupted
        // turn under the same turn id at the new generation and must still
        // receive the inputs pinned to it. The runtime owns the repair.
        Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    crate::SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
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
        if !current.is_held_by(&fence.owner, &fence.executor_id)
            || !current.lease_token_matches(&fence.lease_token)
        {
            let held = current.held_fields();
            crate::store_backend_support::trace_session_execution_lease_refusal(
                crate::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "in_memory_write_transaction",
                fence,
                crate::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    held.map(|fields| fields.owner),
                    held.map(|fields| fields.executor_id),
                    held.map(|fields| fields.lease_token),
                ),
            );
            return Err(
                crate::store::StoreError::SessionExecutionLeaseRenewalRefused {
                    session_id: fence.session_id.clone(),
                },
            );
        }
        if current
            .held_fields()
            .is_none_or(|fields| fields.expires_at_epoch_ms <= now)
        {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        #[cfg(test)]
        if self
            .force_next_session_execution_lease_renewal_zero_match
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let held = current.held_fields();
            crate::store_backend_support::trace_session_execution_lease_refusal(
                crate::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "conditional_update_did_not_match",
                "in_memory_write_transaction",
                fence,
                crate::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    held.map(|fields| fields.owner),
                    held.map(|fields| fields.executor_id),
                    held.map(|fields| fields.lease_token),
                ),
            );
            return Err(
                crate::store::StoreError::SessionExecutionLeaseRenewalRefused {
                    session_id: fence.session_id.clone(),
                },
            );
        }
        let renewed = match &mut current.lease {
            super::Lease::Free => unreachable!("renewal passed the held-lease predicate"),
            super::Lease::Held {
                lease_term_ms,
                expires_at_epoch_ms,
                claimed_at_epoch_ms,
                ..
            } => {
                *lease_term_ms = lease_ttl_ms;
                *expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                crate::SessionExecutionLease {
                    session_id: fence.session_id.clone(),
                    owner: fence.owner.clone(),
                    executor_id: fence.executor_id.clone(),
                    lease_token: fence.lease_token.clone(),
                    fencing_token: current.fencing_token,
                    claimed_at_epoch_ms: *claimed_at_epoch_ms,
                    lease_term_ms: *lease_term_ms,
                    expires_at_epoch_ms: *expires_at_epoch_ms,
                }
            }
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
    ) -> Result<crate::SessionExecutionLeaseObservation, crate::store::StoreError> {
        #[cfg(test)]
        self.refuse_injected_counter_defect("session_lease_fencing_token")?;
        let observed_at_epoch_ms = self.clock.timestamp_ms();
        let leases = self.session_execution_leases.lock_recover();
        let lease = leases.get(session_id).and_then(|current| {
            // An unleased or released row keeps its generation but drops owner
            // and token; only a held row is reported. Expiry is not filtered:
            // a lapsed holder is the fact a triage read needs.
            current
                .is_held()
                .then(|| Self::in_memory_session_execution_lease(session_id, current))
        });
        Ok(crate::SessionExecutionLeaseObservation {
            observed_at_epoch_ms,
            lease,
        })
    }
}
