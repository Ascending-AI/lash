//! Diagnostic read over a session's execution lease.
//!
//! The process surface already exposes its lease read-side as raw facts
//! ([`ObservedProcess::lease_holder`](lash_core::facade_support::ObservedProcess)
//! / `lease_expires_at_ms`). This is the session-execution equivalent, for the
//! one question a stuck turn raises: is somebody actually working on this
//! session, and since when?

use lash_core::LeaseOwnerIdentity;

use crate::{EmbedError, LashCore, Result};

/// A point-in-time reading of one session's execution-lease row.
///
/// **This is a snapshot and it is stale by design.** It can be superseded the
/// moment it is read: a peer may claim, renew, or release the lane between the
/// store read and the caller's next line. Hosts must never gate behavior on it:
/// not admission, not retries, not cancellation, not "is it safe to write". The
/// commit compare-and-set is the only authority on who may publish (ADR 0029);
/// the lease is advisory and this read is advisory about the advisory thing.
///
/// **A lost lease does not mean the turn failed.** A runner can be displaced
/// from the lane and still commit successfully. The commit CAS decides, and it
/// is entirely legitimate for the losing lease-holder to be the winning writer.
/// Reading [`SessionLeaseRenewal::Lapsed`] here, or seeing a different holder
/// than the one you expected, is therefore never grounds to kill a turn, fail a
/// request, or fabricate a terminal state. Pair it with the
/// `session_execution_lease.*` trace timeline
/// (`claimed` → `renew_failed` → `taken_over` → `commit_cas_rejected`) and with
/// the turn's own committed outcome before concluding anything.
///
/// Obtained from
/// [`LashCore::session_lease_diagnostics`](crate::LashCore::session_lease_diagnostics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLeaseDiagnostics {
    /// The session whose lane was read.
    pub session_id: String,
    /// Host-clock reading taken alongside the row, so `renewal` classifies
    /// expiry against one consistent instant instead of a later "now".
    pub observed_at_epoch_ms: u64,
    /// The row's holder facts, or `None` when no owner holds the lane: never
    /// claimed, or released by its last holder (typically at commit).
    pub holder: Option<SessionLeaseHolder>,
}

/// The owner facts recorded on a held session-execution-lease row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLeaseHolder {
    /// Which runner holds the lane: a stable `owner_id` per replica plus the
    /// `incarnation_id` that replica bumps each boot.
    pub owner: LeaseOwnerIdentity,
    /// The lane's monotonic fencing token. ADR 0029 calls this the session-lease
    /// *generation*: queued-work and turn-input claims pin it instead of
    /// carrying a TTL of their own, and every takeover advances it.
    pub generation: u64,
    /// When this holder acquired the lane.
    pub claimed_at_epoch_ms: u64,
    /// When the lane lapses unless the holder renews first.
    pub expires_at_epoch_ms: u64,
}

/// Whether the lane's renewals were current at
/// [`observed_at_epoch_ms`](SessionLeaseDiagnostics::observed_at_epoch_ms).
///
/// Derived from the row, not stored on it: the raw facts on
/// [`SessionLeaseHolder`] remain the evidence. Like everything else here it is a
/// reading, not a verdict: `Lapsed` says renewals stopped, not that the turn
/// failed or that the lane is safe to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionLeaseRenewal {
    /// Nobody holds the lane. Expected between turns and after a committing
    /// turn released it.
    Unheld,
    /// A holder's expiry is still ahead of the observation, so its renewal loop
    /// was alive. A turn that is not progressing under this reading is stuck
    /// somewhere inside itself (a provider hang, a tool, a wait), not on the
    /// lane.
    Current { expires_in_ms: u64 },
    /// A holder's expiry is already behind the observation: its renewals
    /// stopped, and a peer may claim the lane with a higher generation. The
    /// holder may nonetheless still be running and may still win the commit CAS.
    Lapsed { expired_for_ms: u64 },
}

impl SessionLeaseDiagnostics {
    /// Classify the row's renewal state against the instant it was observed.
    pub fn renewal(&self) -> SessionLeaseRenewal {
        let Some(holder) = self.holder.as_ref() else {
            return SessionLeaseRenewal::Unheld;
        };
        if holder.expires_at_epoch_ms > self.observed_at_epoch_ms {
            SessionLeaseRenewal::Current {
                expires_in_ms: holder.expires_at_epoch_ms - self.observed_at_epoch_ms,
            }
        } else {
            SessionLeaseRenewal::Lapsed {
                expired_for_ms: self.observed_at_epoch_ms - holder.expires_at_epoch_ms,
            }
        }
    }

    pub(crate) fn from_row(
        session_id: String,
        observed_at_epoch_ms: u64,
        row: Option<lash_core::SessionExecutionLease>,
    ) -> Self {
        Self {
            session_id,
            observed_at_epoch_ms,
            holder: row.map(|lease| SessionLeaseHolder {
                owner: lease.owner,
                generation: lease.fencing_token,
                claimed_at_epoch_ms: lease.claimed_at_epoch_ms,
                expires_at_epoch_ms: lease.expires_at_epoch_ms,
            }),
        }
    }
}

impl LashCore {
    /// Read one session's execution-lease row for triage.
    ///
    /// Answers the first question a stuck turn raises: is a runner holding this
    /// session's lane, which one, since when, and were its renewals current?
    /// Returns `None` when the session has no durable store, meaning never
    /// created or already deleted. That is a different answer from a lane merely
    /// unheld ([`SessionLeaseRenewal::Unheld`](crate::persistence::SessionLeaseRenewal::Unheld)).
    ///
    /// **Diagnostics only.** The returned
    /// [`SessionLeaseDiagnostics`](crate::persistence::SessionLeaseDiagnostics)
    /// is a snapshot, stale by design, and can be superseded the moment it is
    /// read. The commit compare-and-set is the only authority on who may publish
    /// (ADR 0029); never gate host behavior on this read. In particular a lost or
    /// lapsed lease does not mean the turn failed, because the displaced holder
    /// may still commit, so this is never grounds to kill a turn or fabricate a
    /// terminal state. Read it alongside the `session_execution_lease.*` trace
    /// events, which are what order a takeover and report a rejected commit.
    ///
    /// This read never claims, renews, or releases the lane, so running it
    /// against a healthy session is safe.
    pub async fn session_lease_diagnostics(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<Option<crate::session_lease::SessionLeaseDiagnostics>> {
        let session_id = session_id.as_ref().to_string();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        let request = lash_core::SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy::default(),
        };
        let Some(store) = store_factory
            .open_existing_store(&request)
            .await
            .map_err(|message| EmbedError::StoreFactory {
                session_id: session_id.clone(),
                message,
            })?
        else {
            return Ok(None);
        };
        let row = store.get_session_execution_lease(&session_id).await?;
        Ok(Some(
            crate::session_lease::SessionLeaseDiagnostics::from_row(
                session_id,
                self.env.core.clock.timestamp_ms(),
                row,
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder_expiring_at(expires_at_epoch_ms: u64) -> Option<SessionLeaseHolder> {
        Some(SessionLeaseHolder {
            owner: LeaseOwnerIdentity::opaque("worker-a", "worker-a:boot-1"),
            generation: 4,
            claimed_at_epoch_ms: 1_000,
            expires_at_epoch_ms,
        })
    }

    #[test]
    fn renewal_classifies_unheld_current_and_lapsed_against_the_observation() {
        let unheld = SessionLeaseDiagnostics {
            session_id: "s".to_string(),
            observed_at_epoch_ms: 5_000,
            holder: None,
        };
        assert_eq!(unheld.renewal(), SessionLeaseRenewal::Unheld);

        let current = SessionLeaseDiagnostics {
            session_id: "s".to_string(),
            observed_at_epoch_ms: 5_000,
            holder: holder_expiring_at(7_500),
        };
        assert_eq!(
            current.renewal(),
            SessionLeaseRenewal::Current {
                expires_in_ms: 2_500
            }
        );

        let lapsed = SessionLeaseDiagnostics {
            session_id: "s".to_string(),
            observed_at_epoch_ms: 5_000,
            holder: holder_expiring_at(4_250),
        };
        assert_eq!(
            lapsed.renewal(),
            SessionLeaseRenewal::Lapsed {
                expired_for_ms: 750
            }
        );
    }

    #[test]
    fn an_expiry_exactly_at_the_observation_has_already_lapsed() {
        // The store treats `expires_at <= now` as expired when it decides
        // whether a lane is claimable; the diagnostic reading must not disagree
        // with the substrate at the boundary.
        let boundary = SessionLeaseDiagnostics {
            session_id: "s".to_string(),
            observed_at_epoch_ms: 5_000,
            holder: holder_expiring_at(5_000),
        };
        assert_eq!(
            boundary.renewal(),
            SessionLeaseRenewal::Lapsed { expired_for_ms: 0 }
        );
    }

    #[test]
    fn a_released_row_reads_as_unheld() {
        let diagnostics = SessionLeaseDiagnostics::from_row("s".to_string(), 5_000, None);
        assert_eq!(diagnostics.holder, None);
        assert_eq!(diagnostics.renewal(), SessionLeaseRenewal::Unheld);
    }

    #[test]
    fn a_held_row_projects_the_generation_and_holder_identity() {
        let diagnostics = SessionLeaseDiagnostics::from_row(
            "s".to_string(),
            5_000,
            Some(lash_core::SessionExecutionLease {
                session_id: "s".to_string(),
                owner: LeaseOwnerIdentity::opaque("worker-b", "worker-b:boot-2"),
                lease_token: "token".to_string(),
                fencing_token: 9,
                claimed_at_epoch_ms: 4_000,
                expires_at_epoch_ms: 9_000,
            }),
        );
        let holder = diagnostics.holder.expect("held row reports a holder");
        assert_eq!(holder.owner.owner_id, "worker-b");
        assert_eq!(holder.owner.incarnation_id, "worker-b:boot-2");
        assert_eq!(holder.generation, 9);
        assert_eq!(holder.claimed_at_epoch_ms, 4_000);
        assert_eq!(holder.expires_at_epoch_ms, 9_000);
    }
}
