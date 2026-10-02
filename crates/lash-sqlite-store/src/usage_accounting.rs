//! Atomic accounting over the durable-core catalog. No session head or shift lease is involved.
use crate::conn::TxOutcome;
use crate::schema_layout::Schema;
use crate::{SqliteStore, sqlite_error};
use async_trait::async_trait;
use lash_core_execution::store_backend_support::{
    StoredOutstandingAttempt, StoredUsageAggregate, StoredUsageFact, StoredUsageMeter,
    decode_usage_completeness, usage_corrupt, usage_integer, usage_meter_resolution_columns,
    usage_unsigned,
};
use lash_core_execution::{StoreError, TokenUsage};
use lash_core_store::RuntimeOwner;
use lash_core_store::store::usage_accounting::UsageAccountingStore;
use lash_core_store::usage_accounting::*;
use lash_store_sql::usage::{
    usage_facts::UsageFactsStatements, usage_meters::UsageMetersStatements,
    usage_owner_retirements::UsageOwnerRetirementsStatements,
};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::num::NonZeroU32;
use std::sync::LazyLock;

struct Statements {
    facts: UsageFactsStatements,
    meters: UsageMetersStatements,
    owners: UsageOwnerRetirementsStatements,
    inserts: UsageInsertStatements,
}
lash_store_sql::statements! {
    pub(crate) struct UsageInsertStatements @ "usage_sqlite" {
        fact = "INSERT OR IGNORE INTO usage_facts (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind, disposition, meter_id, llm_call_id, source, profile_key, requested_model, served_model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, generation_id, payload_hash, recorded_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)";
        meter = "INSERT OR IGNORE INTO usage_meters (owner_kind, owner_id, effect_key, meter_id, execution_scope_key, source, profile_key, requested_model, admitted_at_ms, state, unknown_reason, resolved_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";
        owner = "INSERT OR IGNORE INTO usage_owner_retirements (owner_kind, owner_id, retired_at_ms) VALUES (?1, ?2, ?3)";
    }
}
static SQL: LazyLock<Statements> = LazyLock::new(|| Statements {
    facts: UsageFactsStatements::render(Schema::Main.dialect()),
    meters: UsageMetersStatements::render(Schema::Main.dialect()),
    owners: UsageOwnerRetirementsStatements::render(Schema::Main.dialect()),
    inserts: UsageInsertStatements::render(Schema::Main.dialect()),
});
pub(crate) fn retention_sql() -> (&'static str, &'static str, &'static str) {
    (
        SQL.facts.delete_retired.sql(),
        SQL.meters.delete_retired.sql(),
        SQL.owners.delete_retired.sql(),
    )
}
fn decode_fact(row: &Row<'_>) -> Result<UsageFactRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.get($n).map_err(sqlite_error)?
        };
    }
    StoredUsageFact {
        seq: get!(0),
        owner_kind: get!(1),
        owner_id: get!(2),
        effect_key: get!(3),
        call_ordinal: get!(4),
        provider_attempt: get!(5),
        fact_kind: get!(6),
        disposition: get!(7),
        meter_id: get!(8),
        llm_call_id: get!(9),
        source: get!(10),
        profile_key: get!(11),
        requested_model: get!(12),
        served_model: get!(13),
        usage: TokenUsage {
            input_tokens: get!(14),
            output_tokens: get!(15),
            cache_read_input_tokens: get!(16),
            cache_write_input_tokens: get!(17),
            reasoning_output_tokens: get!(18),
        },
        generation_id: get!(19),
        // Column 20 is `payload_hash`: the conflict path's own read, never
        // part of the domain record.
        recorded_at_ms: get!(21),
    }
    .decode()
}
fn decode_meter(row: &Row<'_>, owner: &RuntimeOwner) -> Result<UsageMeterRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.get($n).map_err(sqlite_error)?
        };
    }
    StoredUsageMeter {
        effect_key: get!(0),
        meter_id: get!(1),
        execution_scope_key: get!(2),
        source: get!(3),
        profile_key: get!(4),
        requested_model: get!(5),
        admitted_at_ms: get!(6),
        state: get!(7),
        unknown_reason: get!(8),
        conflict_call_ordinal: get!(9),
        conflict_provider_attempt: get!(10),
        conflict_fact_kind: get!(11),
        conflict_stored_payload_hash: get!(12),
        conflict_offered_payload_hash: get!(13),
        resolved_at_ms: get!(14),
    }
    .decode(owner)
}

fn insert_fact(
    tx: &Transaction<'_>,
    record: &UsageFactRecord,
    hash: &str,
) -> Result<bool, UsageAppendError> {
    let (kind, id) = usage_owner_columns(&record.owner);
    let usage = record.usage();
    let inserted = tx
        .execute(
            SQL.inserts.fact.sql(),
            params![
                kind,
                id,
                record.effect.as_str(),
                i64::from(record.call_ordinal),
                i64::from(record.provider_attempt),
                record.body.kind().as_str(),
                record.disposition().as_str(),
                record.meter().map(UsageMeterId::as_str),
                record.llm_call_id.0.as_str(),
                record.source,
                record.profile_key.as_str(),
                record.requested_model,
                record.served_model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_write_input_tokens,
                usage.reasoning_output_tokens,
                record.generation_id(),
                hash,
                usage_integer(record.recorded_at_ms)?
            ],
        )
        .map_err(sqlite_error)?;
    if inserted != 0 {
        return Ok(true);
    }
    let stored: String = tx
        .query_row(
            SQL.facts.payload.sql(),
            params![
                kind,
                id,
                record.effect.as_str(),
                i64::from(record.call_ordinal),
                i64::from(record.provider_attempt),
                record.body.kind().as_str()
            ],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if stored == hash {
        Ok(false)
    } else {
        Err(UsageAppendError::Conflict(Box::new(UsageFactConflict {
            identity: record.identity(),
            stored_payload_hash: stored,
            offered_payload_hash: hash.to_owned(),
        })))
    }
}
impl SqliteStore {
    async fn usage_write<T, E, F>(&self, action: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    {
        self.conn
            .write_flow(move |tx| {
                Ok(match action(tx) {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(|e| E::from(sqlite_error(e)))?
    }
}
#[async_trait]
impl UsageAccountingStore for SqliteStore {
    async fn admit_usage_meter(
        &self,
        admission: &UsageMeterAdmission,
    ) -> Result<UsageMeterAdmitted, UsageAdmissionError> {
        let a = admission.clone();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&a.owner);
            let retired: Option<i64> = tx
                .query_row(SQL.owners.find.sql(), params![kind, id], |row| row.get(0))
                .optional()
                .map_err(sqlite_error)?;
            if let Some(retired) = retired {
                return Err(UsageAdmissionError::OwnerRetired {
                    owner: a.owner.clone(),
                    retired_at_ms: usage_unsigned(retired)?,
                });
            }
            let inserted = tx
                .execute(
                    SQL.inserts.meter.sql(),
                    params![
                        kind,
                        id,
                        a.effect.as_str(),
                        a.meter.as_str(),
                        a.execution_scope_key,
                        a.source,
                        a.profile_key.as_str(),
                        a.requested_model,
                        usage_integer(a.admitted_at_ms)?,
                        "open",
                        Option::<&str>::None,
                        Option::<i64>::None
                    ],
                )
                .map_err(sqlite_error)?;
            Ok(if inserted == 0 {
                UsageMeterAdmitted::AlreadyAdmitted
            } else {
                UsageMeterAdmitted::Admitted
            })
        })
        .await
    }
    async fn settle_usage(
        &self,
        settlement: &UsageSettlement,
        now_ms: u64,
    ) -> Result<UsageSettleReceipt, UsageAppendError> {
        let s = settlement.clone();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&s.owner);
            let now = usage_integer(now_ms)?;
            let mut receipt = UsageSettleReceipt {
                inserted_facts: 0,
                duplicate_facts: 0,
                meter: s.accounting.resolution(),
                superseded_runs: 0,
            };
            for fact in &s.facts {
                if insert_fact(
                    tx,
                    &fact.record(&s.owner, &s.effect, &s.meter, now_ms),
                    &usage_fact_payload_hash(fact, &s.meter),
                )? {
                    receipt.inserted_facts += 1;
                } else {
                    receipt.duplicate_facts += 1;
                }
            }
            let (state, reason) = usage_meter_resolution_columns(&receipt.meter);
            tx.execute(
                SQL.inserts.meter.sql(),
                params![
                    kind,
                    id,
                    s.effect.as_str(),
                    s.meter.as_str(),
                    Option::<&str>::None,
                    Option::<&str>::None,
                    Option::<&str>::None,
                    Option::<&str>::None,
                    Option::<i64>::None,
                    state,
                    reason,
                    now
                ],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                SQL.meters.resolve.sql(),
                params![
                    kind,
                    id,
                    s.effect.as_str(),
                    s.meter.as_str(),
                    state,
                    reason,
                    now
                ],
            )
            .map_err(sqlite_error)?;
            let actual = tx
                .query_row(
                    SQL.meters.find.sql(),
                    params![kind, id, s.effect.as_str(), s.meter.as_str()],
                    |row| Ok(decode_meter(row, &s.owner)),
                )
                .map_err(sqlite_error)??;
            if actual.state.is_settled() {
                receipt.meter = UsageMeterResolution::Settled;
            }
            receipt.superseded_runs = u32::try_from(
                tx.execute(
                    SQL.meters.supersede.sql(),
                    params![kind, id, s.effect.as_str(), s.meter.as_str(), now],
                )
                .map_err(sqlite_error)?,
            )
            .map_err(|_| usage_corrupt("superseded meter count exceeds u32"))?;
            Ok(receipt)
        })
        .await
    }
    async fn mark_usage_settlement_conflicted(
        &self,
        settlement: &UsageSettlement,
        conflict: &UsageFactConflict,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if conflict.identity.owner != settlement.owner
            || conflict.identity.effect != settlement.effect
        {
            return Err(usage_corrupt(
                "usage conflict belongs to another owner or effect",
            ));
        }
        let s = settlement.clone();
        let conflict = conflict.clone();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&s.owner);
            let now = usage_integer(now_ms)?;
            for statement in [SQL.meters.insert_conflict.sql(), SQL.meters.conflict.sql()] {
                tx.execute(
                    statement,
                    params![
                        kind,
                        id,
                        s.effect.as_str(),
                        s.meter.as_str(),
                        i64::from(conflict.identity.call_ordinal),
                        i64::from(conflict.identity.provider_attempt),
                        conflict.identity.kind.as_str(),
                        conflict.stored_payload_hash,
                        conflict.offered_payload_hash,
                        now
                    ],
                )
                .map_err(sqlite_error)?;
            }
            Ok(())
        })
        .await
    }
    async fn append_usage_corrections(
        &self,
        owner: &RuntimeOwner,
        corrections: &[UsageCorrection],
        now_ms: u64,
    ) -> Result<UsageAppendReceipt, UsageAppendError> {
        let owner = owner.clone();
        let corrections = corrections.to_vec();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&owner);
            let mut receipt = UsageAppendReceipt {
                inserted: 0,
                duplicates: 0,
            };
            for correction in &corrections {
                let identity = UsageFactIdentity {
                    owner: owner.clone(),
                    effect: correction.effect.clone(),
                    call_ordinal: correction.call_ordinal,
                    provider_attempt: correction.provider_attempt,
                    kind: UsageFactKind::Attempt,
                };
                let target = tx
                    .query_row(
                        SQL.facts.attempt.sql(),
                        params![
                            kind,
                            id,
                            correction.effect.as_str(),
                            i64::from(correction.call_ordinal),
                            i64::from(correction.provider_attempt),
                            "attempt"
                        ],
                        |row| Ok(decode_fact(row)),
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .transpose()?;
                let Some(mut record) = target else {
                    return Err(UsageAppendError::CorrectionTargetMissing { identity });
                };
                if record.disposition() != UsageReporting::Unreported {
                    return Err(UsageAppendError::CorrectionTargetReported { identity });
                }
                let hash = usage_correction_payload_hash(correction, &record);
                record.body = UsageFactBody::Correction {
                    usage: correction.usage.clone(),
                    generation_id: correction.generation_id.clone(),
                };
                record.recorded_at_ms = now_ms;
                if insert_fact(tx, &record, &hash)? {
                    receipt.inserted += 1;
                } else {
                    receipt.duplicates += 1;
                }
            }
            Ok(receipt)
        })
        .await
    }
    async fn retire_usage_execution(
        &self,
        owner: &RuntimeOwner,
        execution_scope_key: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        let owner = owner.clone();
        let scope = execution_scope_key.to_owned();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&owner);
            let count = tx
                .execute(
                    SQL.meters.retire_execution.sql(),
                    params![kind, id, scope, usage_integer(now_ms)?],
                )
                .map_err(sqlite_error)?;
            Ok(count as u64)
        })
        .await
    }
    async fn retire_usage_owner(
        &self,
        owner: &RuntimeOwner,
        now_ms: u64,
    ) -> Result<UsageOwnerRetired, StoreError> {
        let owner = owner.clone();
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&owner);
            let inserted = tx
                .execute(
                    SQL.inserts.owner.sql(),
                    params![kind, id, usage_integer(now_ms)?],
                )
                .map_err(sqlite_error)?;
            let retired = usage_unsigned(
                tx.query_row(SQL.owners.find.sql(), params![kind, id], |row| row.get(0))
                    .map_err(sqlite_error)?,
            )?;
            let count = tx
                .execute(
                    SQL.meters.retire_owner.sql(),
                    params![kind, id, usage_integer(now_ms)?],
                )
                .map_err(sqlite_error)?;
            Ok(UsageOwnerRetired {
                retired_at_ms: retired,
                resolved_open_runs: count as u64,
                already_retired: inserted == 0,
            })
        })
        .await
    }
    async fn load_owner_usage(&self, owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError> {
        let owner = owner.clone();
        self.read_connection()
            .read(move |tx| {
                let read = || -> Result<OwnerUsage, StoreError> {
                    let (kind, id) = usage_owner_columns(&owner);
                    let rows = tx
                        .prepare_cached(SQL.facts.aggregate.sql())
                        .map_err(sqlite_error)?
                        .query_map(params![kind, id], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                TokenUsage {
                                    input_tokens: row.get(3)?,
                                    output_tokens: row.get(4)?,
                                    cache_read_input_tokens: row.get(5)?,
                                    cache_write_input_tokens: row.get(6)?,
                                    reasoning_output_tokens: row.get(7)?,
                                },
                                row.get::<_, i64>(8)?,
                                row.get::<_, i64>(9)?,
                                row.get::<_, i64>(10)?,
                            ))
                        })
                        .map_err(sqlite_error)?
                        .map(|row| {
                            let (
                                source,
                                profile_key,
                                requested_model,
                                usage,
                                reported,
                                unreported,
                                reconciled,
                            ) = row.map_err(sqlite_error)?;
                            StoredUsageAggregate {
                                source,
                                profile_key,
                                requested_model,
                                usage,
                                reported_attempts: reported,
                                unreported_attempts: unreported,
                                reconciled_attempts: reconciled,
                            }
                            .decode()
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let outstanding = tx
                        .prepare_cached(SQL.facts.outstanding.sql())
                        .map_err(sqlite_error)?
                        .query_map(params![kind, id], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, String>(6)?,
                                row.get::<_, Option<String>>(7)?,
                            ))
                        })
                        .map_err(sqlite_error)?
                        .map(|row| {
                            let (
                                key,
                                call,
                                attempt,
                                llm_call,
                                source,
                                profile_key,
                                requested_model,
                                generation_id,
                            ) = row.map_err(sqlite_error)?;
                            StoredOutstandingAttempt {
                                effect_key: key,
                                call_ordinal: call,
                                provider_attempt: attempt,
                                llm_call_id: llm_call,
                                source,
                                profile_key,
                                requested_model,
                                generation_id,
                            }
                            .decode()
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let (open, oldest, unknown, conflicted): (i64, Option<i64>, i64, i64) = tx
                        .query_row(SQL.meters.completeness.sql(), params![kind, id], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        })
                        .map_err(sqlite_error)?;
                    let retired = tx
                        .query_row(SQL.owners.find.sql(), params![kind, id], |row| {
                            row.get::<_, i64>(0)
                        })
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    Ok(OwnerUsage {
                        owner: owner.clone(),
                        rows,
                        completeness: decode_usage_completeness(
                            open,
                            oldest,
                            unknown,
                            conflicted,
                            outstanding.len() as u64,
                            retired,
                        )?,
                        outstanding,
                    })
                };
                Ok(read())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn load_usage_fact_page(
        &self,
        owner: &RuntimeOwner,
        after: Option<&UsageFactCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageFactPage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_owner(owner)?;
        }
        let owner = owner.clone();
        let after = usage_integer(after.map_or(0, UsageFactCursor::after_seq))?;
        self.read_connection()
            .read(move |tx| {
                let (kind, id) = usage_owner_columns(&owner);
                let records = tx
                    .prepare_cached(SQL.facts.page.sql())?
                    .query_map(
                        params![kind, id, after, i64::from(limit.get()) + 1],
                        |row| Ok(decode_fact(row)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let page = || -> Result<UsageFactPage, StoreError> {
                    let mut facts = records.into_iter().collect::<Result<Vec<_>, _>>()?;
                    let more = facts.len() > limit.get() as usize;
                    facts.truncate(limit.get() as usize);
                    let next = if more {
                        facts
                            .last()
                            .map(|fact| UsageFactCursor::new(owner, fact.seq))
                    } else {
                        None
                    };
                    Ok(UsageFactPage { facts, next })
                };
                Ok(page())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn load_usage_meter_page(
        &self,
        owner: &RuntimeOwner,
        filter: UsageMeterFilter,
        after: Option<&UsageMeterCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageMeterPage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_owner(owner)?;
        }
        let owner = owner.clone();
        let after = after.cloned();
        let filter = match filter {
            UsageMeterFilter::Open => "open",
            UsageMeterFilter::Unresolved => "unresolved",
            UsageMeterFilter::All => "all",
        };
        self.read_connection()
            .read(move |tx| {
                let (kind, id) = usage_owner_columns(&owner);
                let records = tx
                    .prepare_cached(SQL.meters.page.sql())?
                    .query_map(
                        params![
                            kind,
                            id,
                            filter,
                            after.as_ref().map_or("", |c| c.after_effect().as_str()),
                            after.as_ref().map_or("", |c| c.after_meter().as_str()),
                            i64::from(limit.get()) + 1
                        ],
                        |row| Ok(decode_meter(row, &owner)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let page = || -> Result<UsageMeterPage, StoreError> {
                    let mut meters = records.into_iter().collect::<Result<Vec<_>, _>>()?;
                    let more = meters.len() > limit.get() as usize;
                    meters.truncate(limit.get() as usize);
                    let next = if more {
                        meters.last().map(|meter| {
                            UsageMeterCursor::new(owner, meter.effect.clone(), meter.meter.clone())
                        })
                    } else {
                        None
                    };
                    Ok(UsageMeterPage { meters, next })
                };
                Ok(page())
            })
            .await
            .map_err(sqlite_error)?
    }
}
