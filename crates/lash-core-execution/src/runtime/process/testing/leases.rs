//! Process-lease transitions for the in-memory registry double.
//!
//! Extracted so the `ProcessRegistry` impl reads as an index of the surface.
//! The double deliberately consults the same pure `registry_transitions`
//! decision tables the durable backends use.

use super::*;

pub(super) async fn claim_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &ProcessId,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, PluginError> {
    if let Some(error) = registry.process_lease_claim_error.lock().await.clone() {
        return Err(error);
    }
    registry
        .write(async |state| {
            // A lease is authority over a retained row: the SQL backends gate
            // every lease write on `require_process_conn`; same guard here
            // (FIG-953).
            if !state.managed.contains_key(process_id) {
                return Err(process_miss(state, process_id));
            }
            let now = registry.clock.timestamp_ms();
            // The same pure tables the durable backends consult (this double's
            // hand-rolled copy drifted once — FIG-953). An empty-token row is a
            // retained fence, not an observable lease, per `ProcessLeaseRow::project`.
            let observed = state
                .leases
                .get(process_id)
                .filter(|current| !current.lease_token.is_empty())
                .cloned();
            match registry_transitions::decide_process_lease_claim(
                observed.as_ref(),
                owner,
                now,
                lease_ttl_ms,
            ) {
                registry_transitions::ProcessLeaseClaimDecision::ExtendHeldLease { lease } => {
                    state
                        .leases
                        .insert(ProcessId::from(process_id.to_string()), lease.clone());
                    Ok(ProcessLeaseClaimOutcome::Acquired(lease))
                }
                registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
                    Ok(ProcessLeaseClaimOutcome::Busy { holder })
                }
                registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
                    // A released lease retains its fencing token for its successor.
                    let fencing_token = registry_transitions::next_process_lease_fencing_token(
                        state
                            .leases
                            .get(process_id)
                            .map_or(0, |current| current.fencing_token),
                    )?;
                    let lease = registry_transitions::acquired_process_lease(
                        process_id,
                        owner,
                        fencing_token,
                        now,
                        lease_ttl_ms,
                    );
                    state
                        .leases
                        .insert(ProcessId::from(process_id.to_string()), lease.clone());
                    Ok(ProcessLeaseClaimOutcome::Acquired(lease))
                }
            }
        })
        .await
}

pub(super) async fn reclaim_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &ProcessId,
    owner: &crate::LeaseOwnerIdentity,
    observed_holder: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, PluginError> {
    registry
        .write(async |state| {
            let now = registry.clock.timestamp_ms();
            let observed = state
                .leases
                .get(process_id)
                .filter(|current| !current.lease_token.is_empty())
                .cloned();
            let _ = observed_holder;
            let fencing_token =
                match registry_transitions::decide_process_lease_reclaim(observed.as_ref(), now)? {
                    registry_transitions::ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                        return Ok(ProcessLeaseClaimOutcome::Busy { holder });
                    }
                    registry_transitions::ProcessLeaseReclaimDecision::AcquireOnRetainedFence => {
                        registry_transitions::next_process_lease_fencing_token(
                            state
                                .leases
                                .get(process_id)
                                .map_or(0, |current| current.fencing_token),
                        )?
                    }
                    registry_transitions::ProcessLeaseReclaimDecision::AcquireOnObservedFence {
                        fencing_token,
                    } => fencing_token,
                };
            let lease = registry_transitions::acquired_process_lease(
                process_id,
                owner,
                fencing_token,
                now,
                lease_ttl_ms,
            );
            state
                .leases
                .insert(ProcessId::from(process_id.to_string()), lease.clone());
            Ok(ProcessLeaseClaimOutcome::Acquired(lease))
        })
        .await
}

pub(super) async fn renew_process_lease(
    registry: &TestLocalProcessRegistry,
    lease: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLease, PluginError> {
    if let Some(error) = registry.process_lease_renew_error.lock().await.clone() {
        return Err(error);
    }
    registry
        .write(async |state| {
            let now = registry.clock.timestamp_ms();
            let live = state.leases.get(&lease.process_id).filter(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            });
            if live.is_none() {
                return Err(process_lease_expired(&lease.process_id));
            }
            let renewed = ProcessLease {
                expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
                ..lease.clone()
            };
            state.leases.insert(
                ProcessId::from(lease.process_id.clone().to_string()),
                renewed.clone(),
            );
            Ok(renewed)
        })
        .await
}

pub(super) async fn get_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &ProcessId,
) -> Result<Option<ProcessLease>, PluginError> {
    Ok(registry
        .state
        .lock()
        .await
        .leases
        .get(process_id)
        .filter(|lease| !lease.lease_token.is_empty())
        .cloned())
}

pub(super) async fn complete_process_lease(
    registry: &TestLocalProcessRegistry,
    completion: &ProcessLeaseCompletion,
) -> Result<(), PluginError> {
    if let Some(error) = registry.process_lease_release_error.lock().await.clone() {
        return Err(error);
    }
    registry
        .write(async |state| {
            // Release (don't drop) the lease, fenced by the completion token, so a
            // stale completion cannot release a newer owner's lease and the
            // `fencing_token` is preserved for the next claim.
            if let Some(current) = state.leases.get_mut(&completion.process_id)
                && current.lease_token == completion.lease_token
            {
                current.owner = crate::LeaseOwnerIdentity::opaque("", "");
                current.lease_token = String::new();
                current.claimed_at_epoch_ms = 0;
                current.expires_at_epoch_ms = 0;
            }
            Ok(())
        })
        .await
}

#[async_trait::async_trait]
impl crate::runtime::process::registry::ProcessLeases for TestLocalProcessRegistry {
    async fn claim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        leases::claim_process_lease(self, process_id, owner, lease_ttl_ms).await
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError> {
        leases::reclaim_process_lease(self, process_id, owner, observed_holder, lease_ttl_ms).await
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        leases::renew_process_lease(self, lease, lease_ttl_ms).await
    }

    async fn get_process_lease(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessLease>, PluginError> {
        *self.process_lease_point_reads.lock().await += 1;
        leases::get_process_lease(self, process_id).await
    }

    async fn get_process_leases(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<Option<ProcessLease>>, PluginError> {
        *self.process_lease_batch_reads.lock().await += 1;
        let state = self.state.lock().await;
        Ok(process_ids
            .iter()
            .map(|process_id| {
                state
                    .leases
                    .get(process_id)
                    .filter(|lease| !lease.lease_token.is_empty())
                    .cloned()
            })
            .collect())
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError> {
        leases::complete_process_lease(self, completion).await
    }
}
