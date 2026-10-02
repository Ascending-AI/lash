//! Atomic usage accounting, independent of session heads and shift fences.
use crate::usage_accounting::*;
use crate::{RuntimeOwner, StoreError};
use async_trait::async_trait;
use std::num::NonZeroU32;
#[async_trait]
pub trait UsageAccountingStore: Send + Sync {
    /// One `open` row; idempotent on `(owner, effect, meter)`. In the same
    /// transaction: refuse `OwnerRetired` when the owner has a retirement row.
    async fn admit_usage_meter(
        &self,
        admission: &UsageMeterAdmission,
    ) -> Result<UsageMeterAdmitted, UsageAdmissionError>;

    /// One transaction:
    /// 1. every fact: insert by identity; an existing identity with the same
    ///    payload hash is a duplicate, with another hash the whole transaction
    ///    rolls back with `Conflict`;
    /// 2. the named meter: `open`/`unknown` -> `settled` (or `unknown(call_without_record
    ///    | facts_unjournalable)` per `accounting`); a missing row is inserted
    ///    resolved (admission is not a precondition of settlement);
    ///    `settled` stays `settled`;
    /// 3. every other `open` meter of `(owner, effect)` -> `unknown(superseded_meter)`.
    /// A retired owner still accepts the settlement (I5).
    async fn settle_usage(
        &self,
        settlement: &UsageSettlement,
        now_ms: u64,
    ) -> Result<UsageSettleReceipt, UsageAppendError>;

    /// After `settle_usage` answered `Conflict`: the named meter (and the other
    /// open meters of the effect) become `conflicted` with the typed fact identity and both payload hashes. No fact is written.
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

    /// Resolve every `open` meter of `owner` under `execution_scope_key`
    /// `unknown(execution_ended)`. Returns the count.
    async fn retire_usage_execution(
        &self,
        owner: &RuntimeOwner,
        execution_scope_key: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError>;

    /// Insert the retirement row (idempotent) and resolve every `open` meter
    /// `unknown(owner_retired)`, in one transaction.
    async fn retire_usage_owner(
        &self,
        owner: &RuntimeOwner,
        now_ms: u64,
    ) -> Result<UsageOwnerRetired, StoreError>;

    // Reads: select by owner only; no receipt, run or head join.
    async fn load_owner_usage(&self, owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError>;
    async fn load_usage_fact_page(
        &self,
        owner: &RuntimeOwner,
        after: Option<&UsageFactCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageFactPage, StoreError>;
    async fn load_usage_meter_page(
        &self,
        owner: &RuntimeOwner,
        filter: UsageMeterFilter,
        after: Option<&UsageMeterCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageMeterPage, StoreError>;
}
