//! Ingress obligations (ADR 0109 §3, FIG-3851): every admitted turn input
//! and queued-work batch owes its session a drive.
//!
//! The admission transaction arms the row it inserts, under an id derived
//! from the row's own id, so the producer attempts delivery right after its
//! commit without reading the id back. The ingress rows live in two tables —
//! `pending_turn_inputs` and `queued_work_batches` — until the ADR 0101
//! table cutover folds them into one, so the ingress ledger is the two
//! tables' ledgers composed: an id names a row in exactly one of them, and a
//! due page is the oldest due obligations of both, merged.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::StoreError;
use super::obligation::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, ObligationState, SettleOutcome, StalledObligation,
};

/// The obligation id an ingress row with item id `item_id` is armed under:
/// `ingress:{item_id}`. Item ids — a turn input's `ti:` id, a batch's `qwb:`
/// id — are unique across the store, so the id is too.
#[must_use]
pub fn ingress_obligation_id(item_id: &str) -> ObligationId {
    ObligationId::new(format!("ingress:{item_id}"))
}

/// The due instants and ids of one table's oldest due obligations, read
/// without claiming them: what the ingress ledger merges across its tables.
#[async_trait::async_trait]
pub trait DueObligationPeek: Send + Sync {
    /// At most `limit` obligations due at `now_ms`, a lapsed claim included,
    /// oldest due first, as `(due_at_ms, id)`.
    async fn peek_due(
        &self,
        now_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<(u64, ObligationId)>, StoreError>;
}

/// One table of the ingress ledger: its own obligation ledger and its due
/// read.
#[derive(Clone)]
pub struct IngressTable {
    pub ledger: Arc<dyn ObligationLedger>,
    pub peek: Arc<dyn DueObligationPeek>,
}

/// The ingress ledger: the turn-input table's and the queued-batch table's
/// ledgers, composed.
#[derive(Clone)]
pub struct IngressLedger {
    tables: Vec<IngressTable>,
}

impl IngressLedger {
    /// The ledger over `tables`, each of which holds ingress obligations.
    #[must_use]
    pub fn new(tables: Vec<IngressTable>) -> Self {
        Self { tables }
    }
}

#[async_trait::async_trait]
impl ObligationLedger for IngressLedger {
    fn kind(&self) -> ObligationKind {
        ObligationKind::Ingress
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        for table in &self.tables {
            if let Some(id) = table.ledger.arm(key, now_ms).await? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// The oldest `limit` due obligations across both tables: each table's
    /// due page is read first, the pages are merged, and each table is then
    /// claimed for exactly its share, so a page never exceeds `limit` and a
    /// backlog in one table never starves the other.
    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let mut due = Vec::new();
        for (index, table) in self.tables.iter().enumerate() {
            due.extend(
                table
                    .peek
                    .peek_due(now_ms, limit)
                    .await?
                    .into_iter()
                    .map(|(due_at, id)| (due_at, id, index)),
            );
        }
        due.sort();
        due.truncate(limit.get());
        let mut claimed = Vec::with_capacity(due.len());
        for (index, table) in self.tables.iter().enumerate() {
            let share = due.iter().filter(|(_, _, of)| *of == index).count();
            if let Some(share) = NonZeroUsize::new(share) {
                claimed.extend(table.ledger.claim_due(now_ms, claim_ttl_ms, share).await?);
            }
        }
        Ok(claimed)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        for table in &self.tables {
            if let Some(claimed) = table.ledger.claim(id, token, now_ms, claim_ttl_ms).await? {
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        for table in &self.tables {
            if table
                .ledger
                .settle(id, token, settlement.clone(), now_ms)
                .await?
                == SettleOutcome::Applied
            {
                return Ok(SettleOutcome::Applied);
            }
        }
        Ok(SettleOutcome::ClaimLost)
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        for table in &self.tables {
            if table.ledger.rearm(id, now_ms).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        let mut stalled = Vec::new();
        for table in &self.tables {
            stalled.extend(table.ledger.list_stalled(after, limit).await?);
        }
        stalled.sort_by(|left, right| left.id.cmp(&right.id));
        stalled.truncate(limit.get());
        Ok(stalled)
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        let mut count = 0_u64;
        for table in &self.tables {
            count = count.saturating_add(table.ledger.count_stalled().await?);
        }
        Ok(count)
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        for table in &self.tables {
            if let Some(standing) = table.ledger.standing(id).await? {
                return Ok(Some(standing));
            }
        }
        Ok(None)
    }
}

/// The stalled obligation `id` of `ledger`, if it is stalled: its state is
/// read first, and only a stalled one is looked up in the stalled listing.
///
/// # Errors
///
/// A store failure.
pub async fn stalled_obligation(
    ledger: &dyn ObligationLedger,
    id: &ObligationId,
) -> Result<Option<StalledObligation>, StoreError> {
    if ledger.state(id).await? != Some(ObligationState::Stalled) {
        return Ok(None);
    }
    const PAGE: NonZeroUsize = match NonZeroUsize::new(64) {
        Some(page) => page,
        None => NonZeroUsize::MIN,
    };
    let mut after: Option<ObligationId> = None;
    loop {
        let page = ledger.list_stalled(after.as_ref(), PAGE).await?;
        if let Some(found) = page.iter().find(|stalled| stalled.id == *id) {
            return Ok(Some(found.clone()));
        }
        // Listed in id order: past `id` without finding it, it is gone.
        match page.last() {
            Some(last) if page.len() == PAGE.get() && last.id < *id => {
                after = Some(last.id.clone());
            }
            _ => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ingress_obligation_is_named_after_its_row() {
        assert_eq!(ingress_obligation_id("ti:abc").as_str(), "ingress:ti:abc");
        assert_eq!(ingress_obligation_id("qwb:def").as_str(), "ingress:qwb:def");
    }
}
