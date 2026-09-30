//! The artifact-cleanup ledger (ADR 0113 §2.4, §2.5): the obligation ledger
//! of [`ObligationKind::ArtifactCleanup`](super::ObligationKind::ArtifactCleanup)
//! and the verbs that ledger has beyond ADR 0109's.
//!
//! One row per referrer that owes a cleanup, keyed by the referrer's stored
//! pair. A row exists only while it owes: `settle(Delivered)` deletes it,
//! because the fences in every artifact store are the permanent evidence.

use crate::artifact_referrer::{ArtifactCleanup, ArtifactReferrer};

use super::{ObligationId, ObligationLedger, StoreError};

/// The store half of artifact cleanup.
#[async_trait::async_trait]
pub trait ArtifactCleanupLedger: ObligationLedger {
    /// Upsert under the rule of ADR 0113 §2.4, arming the row `due` at
    /// `now_ms`: a guard plan inserts only when no row exists, and `Ended`
    /// replaces a guard. Callers arm a guard here before acquiring in an
    /// engine store.
    ///
    /// Arming an `Ended` cleanup also inserts the referrer's fence in the
    /// same transaction, in the store set's artifact database (SQLite's
    /// durable core; PostgreSQL's one database): this is how an end fact
    /// with no transaction of its own there — a host pin's release (§3.5) —
    /// fences and records its end at once, so a publish after it is refused
    /// before the relay severs anything.
    async fn arm_cleanup(
        &self,
        cleanup: &ArtifactCleanup,
        now_ms: u64,
    ) -> Result<ObligationId, StoreError>;

    /// Make an existing row due now. `false` if there is none. An end fact
    /// outside the artifact database calls this after its commit; it only
    /// shortens a guard's wait and never decides anything.
    async fn nudge(&self, referrer: &ArtifactReferrer, now_ms: u64) -> Result<bool, StoreError>;

    /// The cleanup record obligation `id` carries, or `None` if no row
    /// carries it.
    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError>;
}

/// What [`ArtifactCleanupLedger::arm_cleanup`] does to the row a cleanup's
/// referrer already has, if any: the one upsert rule both backends apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupUpsert {
    /// No row: insert the cleanup, due now.
    Insert,
    /// An `Ended` cleanup over a guard: replace the body and re-arm it due
    /// now, whatever the guard's obligation state.
    ReplaceGuard,
    /// Keep the row as it is: a guard never replaces a row, and `Ended` never
    /// replaces `Ended`.
    Keep,
}

impl CleanupUpsert {
    /// The rule for arming `incoming` over `existing`.
    #[must_use]
    pub fn decide(existing: Option<&ArtifactCleanup>, incoming: &ArtifactCleanup) -> Self {
        match existing {
            None => Self::Insert,
            Some(existing) if incoming.plan.is_ended() && !existing.plan.is_ended() => {
                Self::ReplaceGuard
            }
            Some(_) => Self::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_referrer::{ArtifactCleanupPlan, HostArtifactPin};

    #[test]
    fn ended_replaces_a_guard_and_nothing_else_replaces_a_row() {
        let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
        let journal = lash_sansio::ExecutionScope::runtime_operation("op")
            .journal_identity()
            .expect("journal");
        let guard = ArtifactCleanup {
            referrer: referrer.clone(),
            plan: ArtifactCleanupPlan::AwaitStart { starter: journal },
            gate: None,
        };
        let ended = ArtifactCleanup::ended(referrer, Vec::new(), None);
        assert_eq!(CleanupUpsert::decide(None, &guard), CleanupUpsert::Insert);
        assert_eq!(CleanupUpsert::decide(None, &ended), CleanupUpsert::Insert);
        assert_eq!(
            CleanupUpsert::decide(Some(&guard), &ended),
            CleanupUpsert::ReplaceGuard
        );
        assert_eq!(
            CleanupUpsert::decide(Some(&ended), &guard),
            CleanupUpsert::Keep
        );
        assert_eq!(
            CleanupUpsert::decide(Some(&ended), &ended),
            CleanupUpsert::Keep
        );
        assert_eq!(
            CleanupUpsert::decide(Some(&guard), &guard),
            CleanupUpsert::Keep
        );
    }
}
