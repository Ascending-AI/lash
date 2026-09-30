//! Atomic usage accounting, independent of session heads and drive fences.
use crate::usage_accounting::*;
use crate::{RuntimeOwner, StoreError};
use async_trait::async_trait;
use std::num::NonZeroU32;
#[async_trait]
pub trait UsageAccountingStore: Send + Sync {
    /// One `open` row; idempotent on `(owner, effect, run)`. In the same
    /// transaction: refuse `OwnerRetired` when the owner has a retirement row.
    async fn admit_usage_run(
        &self,
        admission: &UsageRunAdmission,
    ) -> Result<UsageRunAdmitted, UsageAdmissionError>;

    /// One transaction:
    /// 1. every fact: insert by identity; an existing identity with the same
    ///    payload hash is a duplicate, with another hash the whole transaction
    ///    rolls back with `Conflict`;
    /// 2. the named run: `open`/`unknown` -> `settled` (or `unknown(call_without_record
    ///    | facts_unjournalable)` per `accounting`); a missing row is inserted
    ///    resolved (admission is not a precondition of settlement);
    ///    `settled` stays `settled`;
    /// 3. every other `open` run of `(owner, effect)` -> `unknown(superseded_run)`.
    /// A retired owner still accepts the settlement (I5).
    async fn settle_usage(
        &self,
        settlement: &UsageSettlement,
        now_ms: u64,
    ) -> Result<UsageSettleReceipt, UsageAppendError>;

    /// After `settle_usage` answered `Conflict`: the named run (and the other
    /// open runs of the effect) become `conflicted` with the conflict rendered
    /// into `conflict_detail`. No fact is written.
    async fn mark_usage_settlement_conflicted(
        &self,
        settlement: &UsageSettlement,
        conflict: &UsageFactConflict,
        now_ms: u64,
    ) -> Result<(), StoreError>;

    /// Corrections, one transaction, same identity rules; the target attempt
    /// fact must exist and be `unreported`.
    async fn append_usage_corrections(
        &self,
        owner: &RuntimeOwner,
        corrections: &[UsageCorrection],
        now_ms: u64,
    ) -> Result<UsageAppendReceipt, UsageAppendError>;

    /// Resolve every `open` run of `owner` under `execution_scope_key`
    /// `unknown(execution_ended)`. Returns the count.
    async fn retire_usage_execution(
        &self,
        owner: &RuntimeOwner,
        execution_scope_key: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError>;

    /// Insert the retirement row (idempotent) and resolve every `open` run
    /// `unknown(owner_retired)`, in one transaction.
    async fn retire_usage_owner(
        &self,
        owner: &RuntimeOwner,
        now_ms: u64,
    ) -> Result<UsageOwnerRetired, StoreError>;

    // Reads: select by owner only; no receipt, root or head join.
    async fn load_owner_usage(&self, owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError>;
    async fn load_usage_fact_page(
        &self,
        owner: &RuntimeOwner,
        after: Option<&UsageFactCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageFactPage, StoreError>;
    async fn load_usage_run_page(
        &self,
        owner: &RuntimeOwner,
        filter: UsageRunFilter,
        after: Option<&UsageRunCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageRunPage, StoreError>;
}
