//! Process-lease transitions for the in-memory registry double.
//!
//! Extracted so the `ProcessRegistry` impl reads as an index of the surface.
//! The double deliberately consults the same pure `registry_transitions`
//! decision tables the durable backends use.

use super::*;

pub(super) async fn claim_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, PluginError> {
    if let Some(error) = registry.process_lease_claim_error.lock().await.clone() {
        return Err(error);
    }
    let _transaction = registry.transaction.lock().await;
    // A lease is authority over a retained row: the SQL backends gate every
    // lease write on `require_process_conn`; same guard here (FIG-953).
    if !registry.managed.lock().await.contains_key(process_id) {
        return Err(registry.process_miss(process_id).await);
    }
    let mut leases = registry.leases.lock().await;
    let now = registry.clock.timestamp_ms();
    // The same pure tables the durable backends consult (this double's
    // hand-rolled copy drifted once — FIG-953). An empty-token row is a
    // retained fence, not an observable lease, per `ProcessLeaseRow::project`.
    let observed = leases
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
            leases.insert(process_id.to_string(), lease.clone());
            Ok(ProcessLeaseClaimOutcome::Acquired(lease))
        }
        registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
            Ok(ProcessLeaseClaimOutcome::Busy { holder })
        }
        registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
            // A released lease retains its fencing token for its successor.
            let fencing_token = registry_transitions::next_process_lease_fencing_token(
                leases
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
            leases.insert(process_id.to_string(), lease.clone());
            Ok(ProcessLeaseClaimOutcome::Acquired(lease))
        }
    }
}

pub(super) async fn reclaim_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    observed_holder: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, PluginError> {
    let _transaction = registry.transaction.lock().await;
    let mut leases = registry.leases.lock().await;
    let now = registry.clock.timestamp_ms();
    let observed = leases
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
                    leases
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
    leases.insert(process_id.to_string(), lease.clone());
    Ok(ProcessLeaseClaimOutcome::Acquired(lease))
}

pub(super) async fn renew_process_lease(
    registry: &TestLocalProcessRegistry,
    lease: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLease, PluginError> {
    if let Some(error) = registry.process_lease_renew_error.lock().await.clone() {
        return Err(error);
    }
    let mut leases = registry.leases.lock().await;
    let now = registry.clock.timestamp_ms();
    let live = leases.get(&lease.process_id).filter(|current| {
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
    leases.insert(lease.process_id.clone(), renewed.clone());
    Ok(renewed)
}

pub(super) async fn get_process_lease(
    registry: &TestLocalProcessRegistry,
    process_id: &str,
) -> Result<Option<ProcessLease>, PluginError> {
    Ok(registry
        .leases
        .lock()
        .await
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
    let mut leases = registry.leases.lock().await;
    // Release (don't drop) the lease, fenced by the completion token, so a
    // stale completion cannot release a newer owner's lease and the
    // `fencing_token` is preserved for the next claim.
    if let Some(current) = leases.get_mut(&completion.process_id)
        && current.lease_token == completion.lease_token
    {
        current.owner = crate::LeaseOwnerIdentity::opaque("", "");
        current.lease_token = String::new();
        current.claimed_at_epoch_ms = 0;
        current.expires_at_epoch_ms = 0;
    }
    Ok(())
}
