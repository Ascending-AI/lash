//! Process-lease writes for the SQLite process registry.
//!
//! The lease methods of the `ProcessRegistry` impl delegate here so the trait
//! impl stays a readable index of the surface rather than a wall of
//! transaction bodies. Each function is the whole of one lease transition.

use super::*;
use lash_sansio::ProcessId;

pub(super) async fn claim_process_lease(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
    owner: &LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, lash_core_execution::PluginError> {
    let process_id = process_id.clone();
    let owner = owner.clone();
    let now = registry.clock.timestamp_ms();
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                let current =
                    SqliteProcessRegistry::load_process_lease_conn(tx, &process_id, fleet_format)?;
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
                            process_sql().lease.extend_unfenced.sql(),
                            params![process_id.as_str(), lease.expires_at_epoch_ms as i64],
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
                        fleet_format,
                    )?,
                ))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn reclaim_process_lease(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
    owner: &LeaseOwnerIdentity,
    _observed_holder: &ProcessLease,
    lease_ttl_ms: u64,
) -> Result<ProcessLeaseClaimOutcome, lash_core_execution::PluginError> {
    let process_id = process_id.clone();
    let owner = owner.clone();
    let now = registry.clock.timestamp_ms();
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                SqliteProcessRegistry::require_process_conn(tx, &process_id)?;
                let current =
                    SqliteProcessRegistry::load_process_lease_conn(tx, &process_id, fleet_format)?;
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
                        fleet_format,
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
) -> Result<ProcessLease, lash_core_execution::PluginError> {
    let lease = lease.clone();
    let now = registry.clock.timestamp_ms();
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let current = SqliteProcessRegistry::load_process_lease_conn(
                    tx,
                    &lease.process_id,
                    fleet_format,
                )?;
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
                    process_sql().lease.renew_fenced.sql(),
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
    process_id: &ProcessId,
) -> Result<Option<ProcessLease>, lash_core_execution::PluginError> {
    let process_id = process_id.clone();
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .call(move |conn| {
            Ok(SqliteProcessRegistry::load_process_lease_conn(
                conn,
                &process_id,
                fleet_format,
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn get_process_leases(
    registry: &SqliteProcessRegistry,
    process_ids: &[ProcessId],
) -> Result<Vec<Option<ProcessLease>>, lash_core_execution::PluginError> {
    if process_ids.is_empty() {
        return Ok(Vec::new());
    }
    let process_ids = process_ids.to_vec();
    let process_ids_json = serde_json::to_string(&process_ids).map_err(process_decode_error)?;
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let mut stmt = conn
                    .prepare(process_sql().lease_sqlite.list_by_process_ids.sql())
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_ids_json], |row| {
                        let process_id = crate::row_process_id(row, 0)?;
                        let lease = registry_transitions::ProcessLeaseRow {
                            owner_id: row.get(1)?,
                            incarnation_id: row.get(6)?,
                            lease_token: row.get(2)?,
                            fencing_token: row.get(3)?,
                            claimed_at_ms: row.get(4)?,
                            expires_at_ms: row.get(5)?,
                        }
                        .project(&process_id, fleet_format);
                        Ok((process_id, lease))
                    })
                    .map_err(process_sqlite_error)?;
                let leases_by_id = rows
                    .collect::<Result<std::collections::HashMap<_, _>, _>>()
                    .map_err(process_sqlite_error)?;
                Ok(process_ids
                    .iter()
                    .map(|process_id| leases_by_id.get(process_id).cloned().flatten())
                    .collect())
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete_process_lease(
    registry: &SqliteProcessRegistry,
    completion: &ProcessLeaseCompletion,
) -> Result<(), lash_core_execution::PluginError> {
    // The same release decision `complete_process_with_lease` makes
    // (FIG-3388): read the row under the write flow's lock, run the shared
    // verdict, and let the one release statement's predicate backstop it. A
    // stale or superseded presentation is a no-op, not an error — release is
    // idempotent.
    let completion = completion.clone();
    let now = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let process_id = completion.process_id.clone();
                let current = SqliteProcessRegistry::load_process_lease_row_conn(tx, &process_id)?;
                let verdict = lash_core_execution::store_backend_support::process_lease_verdict(
                    current
                        .as_ref()
                        .map(registry_transitions::ProcessLeaseRow::facts),
                    lash_core_execution::store_backend_support::ProcessLeaseAuthority {
                        lease_token: &completion.lease_token,
                        fencing_token: completion.fencing_token,
                    },
                    now,
                );
                if !matches!(
                    verdict,
                    lash_core_execution::store_backend_support::ProcessLeaseVerdict::Current
                        | lash_core_execution::store_backend_support::ProcessLeaseVerdict::Expired
                ) {
                    return Ok(());
                }
                // An expired lease still clears: the holder fields belong to
                // the lapsed claim and the retained fencing token is what a
                // re-claim builds on.
                let released = tx
                    .execute(
                        process_sql().lease.release.sql(),
                        params![
                            process_id.as_str(),
                            completion.lease_token.as_str(),
                            completion.fencing_token as i64,
                        ],
                    )
                    .map_err(process_sqlite_error)? as u64;
                lash_core_execution::store_backend_support::require_fenced_write_applied(
                    lash_core_execution::store_backend_support::FencedWrite::ProcessLeaseRelease,
                    crate::SQLITE_BACKEND,
                    process_id.as_str(),
                    released,
                    || lash_core_execution::PluginError::ProcessLeaseSuperseded {
                        process_id: process_id.clone(),
                    },
                )
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessLeases for SqliteProcessRegistry {
    async fn claim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core_execution::PluginError> {
        leases::claim_process_lease(self, process_id, owner, lease_ttl_ms).await
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core_execution::PluginError> {
        leases::reclaim_process_lease(self, process_id, owner, observed_holder, lease_ttl_ms).await
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, lash_core_execution::PluginError> {
        leases::renew_process_lease(self, lease, lease_ttl_ms).await
    }

    async fn get_process_lease(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessLease>, lash_core_execution::PluginError> {
        leases::get_process_lease(self, process_id).await
    }

    async fn get_process_leases(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<Option<ProcessLease>>, lash_core_execution::PluginError> {
        leases::get_process_leases(self, process_ids).await
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), lash_core_execution::PluginError> {
        leases::complete_process_lease(self, completion).await
    }
}
