//! Process-lease writes for the SQLite process registry.
//!
//! The lease methods of the `ProcessRegistry` impl delegate here so the trait
//! impl stays a readable index of the surface rather than a wall of
//! transaction bodies. Each function is the whole of one lease transition.

use super::*;

pub(super) async fn claim_process_lease(
    registry: &SqliteProcessRegistry,
    process_id: &str,
    owner: &LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
    let process_id = process_id.to_string();
    let owner = owner.clone();
    let now = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                let current = SqliteProcessRegistry::load_process_lease_conn(tx, &process_id)?;
                let fencing_token = match registry_transitions::decide_process_lease_claim(
                    current.as_ref(),
                    &owner,
                    now,
                    lease_ttl_ms,
                ) {
                    registry_transitions::ProcessLeaseClaimDecision::ExtendHeldLease { lease } => {
                        // Same incarnation re-enters its own live lease:
                        // extend the expiry, keep token and fencing token.
                        tx.execute(
                            "UPDATE process_leases
                             SET lease_expires_at_ms = ?2
                             WHERE process_id = ?1",
                            params![process_id, lease.expires_at_epoch_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                        return Ok(ProcessLeaseClaimOutcome::Acquired(lease));
                    }
                    registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
                        return Ok(ProcessLeaseClaimOutcome::Busy { holder });
                    }
                    registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
                        // Read the raw fencing token directly: a
                        // completed/abandoned lease nulls the owner/token
                        // columns but retains the monotonically-increasing
                        // `lease_fencing_token`, so a re-claim never reuses
                        // a stale writer's token.
                        let retained =
                            SqliteProcessRegistry::retained_process_lease_fencing_token_conn(
                                tx,
                                &process_id,
                            )?;
                        registry_transitions::next_process_lease_fencing_token(retained)?
                    }
                };
                Ok(ProcessLeaseClaimOutcome::Acquired(
                    SqliteProcessRegistry::acquire_process_lease_conn(
                        tx,
                        &process_id,
                        &owner,
                        fencing_token,
                        now,
                        lease_ttl_ms,
                    )?,
                ))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn reclaim_process_lease(
    registry: &SqliteProcessRegistry,
    process_id: &str,
    owner: &LeaseOwnerIdentity,
    _observed_holder: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
    let process_id = process_id.to_string();
    let owner = owner.clone();
    let now = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                let current = SqliteProcessRegistry::load_process_lease_conn(tx, &process_id)?;
                let fencing_token = match registry_transitions::decide_process_lease_reclaim(
                    current.as_ref(),
                    now,
                )? {
                    registry_transitions::ProcessLeaseReclaimDecision::AcquireOnRetainedFence => {
                        // Free (or released) lease: acquire on the retained
                        // fencing token like a plain claim would.
                        let retained =
                            SqliteProcessRegistry::retained_process_lease_fencing_token_conn(
                                tx,
                                &process_id,
                            )?;
                        registry_transitions::next_process_lease_fencing_token(retained)?
                    }
                    registry_transitions::ProcessLeaseReclaimDecision::AcquireOnObservedFence {
                        fencing_token,
                    } => fencing_token,
                    registry_transitions::ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                        return Ok(ProcessLeaseClaimOutcome::Busy { holder });
                    }
                };
                Ok(ProcessLeaseClaimOutcome::Acquired(
                    SqliteProcessRegistry::acquire_process_lease_conn(
                        tx,
                        &process_id,
                        &owner,
                        fencing_token,
                        now,
                        lease_ttl_ms,
                    )?,
                ))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn renew_process_lease(
    registry: &SqliteProcessRegistry,
    lease: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLease, lash_core::PluginError> {
    let lease = lease.clone();
    let now = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let current =
                    SqliteProcessRegistry::load_process_lease_conn(tx, &lease.process_id)?;
                registry_transitions::authorize_process_lease_write(
                    &lease.process_id,
                    &lease,
                    current.as_ref(),
                    now,
                )?;
                let renewed = ProcessLease {
                    expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
                    ..lease.clone()
                };
                tx.execute(
                    "UPDATE process_leases
                     SET lease_expires_at_ms = ?2
                     WHERE process_id = ?1 AND lease_token = ?3",
                    params![
                        renewed.process_id.as_str(),
                        renewed.expires_at_epoch_ms as i64,
                        renewed.lease_token.as_str(),
                    ],
                )
                .map_err(process_sqlite_error)?;
                Ok(renewed)
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn get_process_lease(
    registry: &SqliteProcessRegistry,
    process_id: &str,
) -> Result<Option<ProcessLease>, lash_core::PluginError> {
    let process_id = process_id.to_string();
    registry
        .conn
        .call(move |conn| {
            Ok(SqliteProcessRegistry::load_process_lease_conn(
                conn,
                &process_id,
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete_process_lease(
    registry: &SqliteProcessRegistry,
    completion: &ProcessLeaseCompletion,
) -> Result<(), lash_core::PluginError> {
    let process_id = completion.process_id.clone();
    let lease_token = completion.lease_token.clone();
    registry
        .conn
        .call(move |conn| {
            conn.execute(
                "UPDATE process_leases
                 SET lease_owner_id = NULL,
                     lease_token = NULL,
                     lease_claimed_at_ms = 0,
                     lease_expires_at_ms = 0
                 WHERE process_id = ?1 AND lease_token = ?2",
                params![process_id, lease_token],
            )
        })
        .await
        .map_err(process_sqlite_error)?;
    Ok(())
}
