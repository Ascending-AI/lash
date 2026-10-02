//! Atomic accounting over the durable-core catalog. No session head or drive lease is involved.
use crate::support::store_sqlx_error;
use crate::{PostgresStore, begin_guarded};
use async_trait::async_trait;
use lash_core_execution::RuntimeOwner;
use lash_core_execution::UsageAccountingStore;
use lash_core_execution::store_backend_support::{
    StoredOutstandingAttempt, StoredUsageAggregate, StoredUsageFact, StoredUsageRun,
    decode_usage_completeness, usage_corrupt, usage_integer, usage_run_resolution_columns,
    usage_unsigned,
};
use lash_core_execution::{
    OwnerUsage, UsageAdmissionError, UsageAppendError, UsageAppendReceipt, UsageCorrection,
    UsageFactBody, UsageFactConflict, UsageFactCursor, UsageFactIdentity, UsageFactKind,
    UsageFactPage, UsageFactRecord, UsageOwnerRetired, UsageReporting, UsageRunAdmission,
    UsageRunAdmitted, UsageRunCursor, UsageRunFilter, UsageRunId, UsageRunPage, UsageRunRecord,
    UsageRunResolution, UsageSettleReceipt, UsageSettlement, usage_correction_payload_hash,
    usage_fact_payload_hash, usage_owner_columns,
};
use lash_core_execution::{StoreError, TokenUsage};
use lash_store_sql::Dialect;
use lash_store_sql::usage::{
    usage_facts::UsageFactsStatements, usage_owner_retirements::UsageOwnerRetirementsStatements,
    usage_runs::UsageRunsStatements,
};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};
use std::num::NonZeroU32;
use std::sync::LazyLock;

struct Statements {
    facts: UsageFactsStatements,
    runs: UsageRunsStatements,
    owners: UsageOwnerRetirementsStatements,
    inserts: UsageInsertPostgresStatements,
}
lash_store_sql::statements! {
    pub(crate) struct UsageInsertPostgresStatements @ "usage_postgres" {
        fact = "INSERT INTO usage_facts (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind, disposition, run_id, llm_call_id, source, profile_key, requested_model, served_model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, generation_id, payload_hash, recorded_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21) ON CONFLICT ON CONSTRAINT uq_usage_facts_identity DO NOTHING RETURNING seq";
        run = "INSERT INTO usage_runs (owner_kind, owner_id, effect_key, run_id, execution_scope_key, source, profile_key, requested_model, admitted_at_ms, state, unknown_reason, resolved_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT (owner_kind, owner_id, effect_key, run_id) DO NOTHING";
        owner = "INSERT INTO usage_owner_retirements (owner_kind, owner_id, retired_at_ms) VALUES (?1, ?2, ?3) ON CONFLICT (owner_kind, owner_id) DO NOTHING";
        lock_owner = "SELECT pg_advisory_xact_lock(hashtextextended(?1, 0))";
        lock_writer = "SELECT pg_advisory_xact_lock_shared(hashtextextended('lash:usage:retention', 0))";
        lock_retention = "SELECT pg_advisory_xact_lock(hashtextextended('lash:usage:retention', 0))";
    }
}
pub(crate) async fn lock_retention(tx: &mut Transaction<'_, Postgres>) -> Result<(), StoreError> {
    sqlx::query(SQL.inserts.lock_retention.sql())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
static SQL: LazyLock<Statements> = LazyLock::new(|| Statements {
    facts: UsageFactsStatements::render(Dialect::postgres()),
    runs: UsageRunsStatements::render(Dialect::postgres()),
    owners: UsageOwnerRetirementsStatements::render(Dialect::postgres()),
    inserts: UsageInsertPostgresStatements::render(Dialect::postgres()),
});
pub(crate) fn retention_sql() -> (&'static str, &'static str, &'static str) {
    (
        SQL.facts.delete_retired.sql(),
        SQL.runs.delete_retired.sql(),
        SQL.owners.delete_retired.sql(),
    )
}
fn decode_fact(row: &PgRow) -> Result<UsageFactRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.try_get($n).map_err(store_sqlx_error)?
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
        run_id: get!(8),
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
fn decode_run(row: &PgRow, owner: &RuntimeOwner) -> Result<UsageRunRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.try_get($n).map_err(store_sqlx_error)?
        };
    }
    StoredUsageRun {
        effect_key: get!(0),
        run_id: get!(1),
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

/// The absent retirement row cannot carry a row lock. Serialize every owner mutation
/// before reading it so an admission cannot commit behind its retirement.
async fn lock_owner(
    tx: &mut Transaction<'_, Postgres>,
    owner: &RuntimeOwner,
) -> Result<(), StoreError> {
    sqlx::query(SQL.inserts.lock_writer.sql())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(SQL.inserts.lock_owner.sql())
        .bind(format!("lash:usage:{owner}"))
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
async fn insert_fact(
    tx: &mut Transaction<'_, Postgres>,
    record: &UsageFactRecord,
    hash: &str,
) -> Result<bool, UsageAppendError> {
    let (kind, id) = usage_owner_columns(&record.owner);
    let usage = record.usage();
    let inserted: Option<i64> = sqlx::query_scalar(SQL.inserts.fact.sql())
        .bind(kind)
        .bind(id)
        .bind(record.effect.as_str())
        .bind(i64::from(record.call_ordinal))
        .bind(i64::from(record.provider_attempt))
        .bind(record.body.kind().as_str())
        .bind(record.disposition().as_str())
        .bind(record.run().map(UsageRunId::as_str))
        .bind(record.llm_call_id.0.as_str())
        .bind(&record.source)
        .bind(record.profile_key.as_str())
        .bind(&record.requested_model)
        .bind(&record.served_model)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.cache_read_input_tokens)
        .bind(usage.cache_write_input_tokens)
        .bind(usage.reasoning_output_tokens)
        .bind(record.generation_id())
        .bind(hash)
        .bind(usage_integer(record.recorded_at_ms)?)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if inserted.is_some() {
        return Ok(true);
    }
    let stored: String = sqlx::query_scalar(SQL.facts.payload.sql())
        .bind(kind)
        .bind(id)
        .bind(record.effect.as_str())
        .bind(i64::from(record.call_ordinal))
        .bind(i64::from(record.provider_attempt))
        .bind(record.body.kind().as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
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
async fn ensure_settlement_run(
    tx: &mut Transaction<'_, Postgres>,
    s: &UsageSettlement,
    state: &str,
    reason: Option<&str>,
    resolved: Option<i64>,
) -> Result<(), StoreError> {
    let (kind, id) = usage_owner_columns(&s.owner);
    sqlx::query(SQL.inserts.run.sql())
        .bind(kind)
        .bind(id)
        .bind(s.effect.as_str())
        .bind(s.run.as_str())
        .bind(Option::<&str>::None)
        .bind(Option::<&str>::None)
        .bind(Option::<&str>::None)
        .bind(Option::<&str>::None)
        .bind(Option::<i64>::None)
        .bind(state)
        .bind(reason)
        .bind(resolved)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}
#[async_trait]
impl UsageAccountingStore for PostgresStore {
    async fn admit_usage_run(
        &self,
        a: &UsageRunAdmission,
    ) -> Result<UsageRunAdmitted, UsageAdmissionError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, &a.owner).await?;
        let (kind, id) = usage_owner_columns(&a.owner);
        let retired: Option<i64> = sqlx::query_scalar(SQL.owners.find.sql())
            .bind(kind)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if let Some(retired) = retired {
            return Err(UsageAdmissionError::OwnerRetired {
                owner: a.owner.clone(),
                retired_at_ms: usage_unsigned(retired)?,
            });
        }
        let inserted = sqlx::query(SQL.inserts.run.sql())
            .bind(kind)
            .bind(id)
            .bind(a.effect.as_str())
            .bind(a.run.as_str())
            .bind(&a.execution_scope_key)
            .bind(&a.source)
            .bind(a.profile_key.as_str())
            .bind(&a.requested_model)
            .bind(usage_integer(a.admitted_at_ms)?)
            .bind("open")
            .bind(Option::<&str>::None)
            .bind(Option::<i64>::None)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted == 0 {
            UsageRunAdmitted::AlreadyAdmitted
        } else {
            UsageRunAdmitted::Admitted
        })
    }
    async fn settle_usage(
        &self,
        s: &UsageSettlement,
        now_ms: u64,
    ) -> Result<UsageSettleReceipt, UsageAppendError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, &s.owner).await?;
        let (kind, id) = usage_owner_columns(&s.owner);
        let now = usage_integer(now_ms)?;
        let mut receipt = UsageSettleReceipt {
            inserted_facts: 0,
            duplicate_facts: 0,
            run: s.accounting.resolution(),
            superseded_runs: 0,
        };
        for fact in &s.facts {
            if insert_fact(
                &mut tx,
                &fact.record(&s.owner, &s.effect, &s.run, now_ms),
                &usage_fact_payload_hash(fact, &s.run),
            )
            .await?
            {
                receipt.inserted_facts += 1;
            } else {
                receipt.duplicate_facts += 1;
            }
        }
        let (state, reason) = usage_run_resolution_columns(&receipt.run);
        ensure_settlement_run(&mut tx, s, state, reason, Some(now)).await?;
        sqlx::query(SQL.runs.resolve.sql())
            .bind(kind)
            .bind(id)
            .bind(s.effect.as_str())
            .bind(s.run.as_str())
            .bind(state)
            .bind(reason)
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let actual = decode_run(
            &sqlx::query(SQL.runs.find.sql())
                .bind(kind)
                .bind(id)
                .bind(s.effect.as_str())
                .bind(s.run.as_str())
                .fetch_one(&mut **tx)
                .await
                .map_err(store_sqlx_error)?,
            &s.owner,
        )?;
        if actual.state.is_settled() {
            receipt.run = UsageRunResolution::Settled;
        }
        receipt.superseded_runs = u32::try_from(
            sqlx::query(SQL.runs.supersede.sql())
                .bind(kind)
                .bind(id)
                .bind(s.effect.as_str())
                .bind(s.run.as_str())
                .bind(now)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected(),
        )
        .map_err(|_| usage_corrupt("superseded run count exceeds u32"))?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(receipt)
    }
    async fn mark_usage_settlement_conflicted(
        &self,
        s: &UsageSettlement,
        conflict: &UsageFactConflict,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if conflict.identity.owner != s.owner || conflict.identity.effect != s.effect {
            return Err(usage_corrupt(
                "usage conflict belongs to another owner or effect",
            ));
        }
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, &s.owner).await?;
        let (kind, id) = usage_owner_columns(&s.owner);
        let now = usage_integer(now_ms)?;
        for statement in [SQL.runs.insert_conflict.sql(), SQL.runs.conflict.sql()] {
            sqlx::query(statement)
                .bind(kind)
                .bind(id)
                .bind(s.effect.as_str())
                .bind(s.run.as_str())
                .bind(i64::from(conflict.identity.call_ordinal))
                .bind(i64::from(conflict.identity.provider_attempt))
                .bind(conflict.identity.kind.as_str())
                .bind(&conflict.stored_payload_hash)
                .bind(&conflict.offered_payload_hash)
                .bind(now)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)
    }
    async fn append_usage_corrections(
        &self,
        owner: &RuntimeOwner,
        corrections: &[UsageCorrection],
        now_ms: u64,
    ) -> Result<UsageAppendReceipt, UsageAppendError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, owner).await?;
        let (kind, id) = usage_owner_columns(owner);
        let mut receipt = UsageAppendReceipt {
            inserted: 0,
            duplicates: 0,
        };
        for correction in corrections {
            let identity = UsageFactIdentity {
                owner: owner.clone(),
                effect: correction.effect.clone(),
                call_ordinal: correction.call_ordinal,
                provider_attempt: correction.provider_attempt,
                kind: UsageFactKind::Attempt,
            };
            let target = sqlx::query(SQL.facts.attempt.sql())
                .bind(kind)
                .bind(id)
                .bind(correction.effect.as_str())
                .bind(i64::from(correction.call_ordinal))
                .bind(i64::from(correction.provider_attempt))
                .bind("attempt")
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            let Some(target) = target else {
                return Err(UsageAppendError::CorrectionTargetMissing { identity });
            };
            let mut record = decode_fact(&target)?;
            if record.disposition() != UsageReporting::Unreported {
                return Err(UsageAppendError::CorrectionTargetReported { identity });
            }
            let hash = usage_correction_payload_hash(correction, &record);
            record.body = UsageFactBody::Correction {
                usage: correction.usage.clone(),
                generation_id: correction.generation_id.clone(),
            };
            record.recorded_at_ms = now_ms;
            if insert_fact(&mut tx, &record, &hash).await? {
                receipt.inserted += 1;
            } else {
                receipt.duplicates += 1;
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(receipt)
    }
    async fn retire_usage_execution(
        &self,
        owner: &RuntimeOwner,
        execution_scope_key: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, owner).await?;
        let (kind, id) = usage_owner_columns(owner);
        let count = sqlx::query(SQL.runs.retire_execution.sql())
            .bind(kind)
            .bind(id)
            .bind(execution_scope_key)
            .bind(usage_integer(now_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(count)
    }
    async fn retire_usage_owner(
        &self,
        owner: &RuntimeOwner,
        now_ms: u64,
    ) -> Result<UsageOwnerRetired, StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, owner).await?;
        let (kind, id) = usage_owner_columns(owner);
        let inserted = sqlx::query(SQL.inserts.owner.sql())
            .bind(kind)
            .bind(id)
            .bind(usage_integer(now_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        let retired: i64 = sqlx::query_scalar(SQL.owners.find.sql())
            .bind(kind)
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let count = sqlx::query(SQL.runs.retire_owner.sql())
            .bind(kind)
            .bind(id)
            .bind(usage_integer(now_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(UsageOwnerRetired {
            retired_at_ms: usage_unsigned(retired)?,
            resolved_open_runs: count,
            already_retired: inserted == 0,
        })
    }
    async fn load_owner_usage(&self, owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError> {
        // One snapshot for the totals, the outstanding attempts and the
        // completeness counts, so they describe the same ledger.
        let mut tx = crate::runtime_persistence::read_tx(self).await?;
        let (kind, id) = usage_owner_columns(owner);
        let rows = sqlx::query(SQL.facts.aggregate.sql())
            .bind(kind)
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .iter()
            .map(|row| {
                StoredUsageAggregate {
                    source: row.try_get(0).map_err(store_sqlx_error)?,
                    profile_key: row.try_get(1).map_err(store_sqlx_error)?,
                    requested_model: row.try_get(2).map_err(store_sqlx_error)?,
                    usage: TokenUsage {
                        input_tokens: row.try_get(3).map_err(store_sqlx_error)?,
                        output_tokens: row.try_get(4).map_err(store_sqlx_error)?,
                        cache_read_input_tokens: row.try_get(5).map_err(store_sqlx_error)?,
                        cache_write_input_tokens: row.try_get(6).map_err(store_sqlx_error)?,
                        reasoning_output_tokens: row.try_get(7).map_err(store_sqlx_error)?,
                    },
                    reported_attempts: row.try_get(8).map_err(store_sqlx_error)?,
                    unreported_attempts: row.try_get(9).map_err(store_sqlx_error)?,
                    reconciled_attempts: row.try_get(10).map_err(store_sqlx_error)?,
                }
                .decode()
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let outstanding = sqlx::query(SQL.facts.outstanding.sql())
            .bind(kind)
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .iter()
            .map(|row| {
                StoredOutstandingAttempt {
                    effect_key: row.try_get(0).map_err(store_sqlx_error)?,
                    call_ordinal: row.try_get(1).map_err(store_sqlx_error)?,
                    provider_attempt: row.try_get(2).map_err(store_sqlx_error)?,
                    llm_call_id: row.try_get(3).map_err(store_sqlx_error)?,
                    source: row.try_get(4).map_err(store_sqlx_error)?,
                    profile_key: row.try_get(5).map_err(store_sqlx_error)?,
                    requested_model: row.try_get(6).map_err(store_sqlx_error)?,
                    generation_id: row.try_get(7).map_err(store_sqlx_error)?,
                }
                .decode()
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let counts = sqlx::query(SQL.runs.completeness.sql())
            .bind(kind)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let oldest: Option<i64> = counts.try_get(1).map_err(store_sqlx_error)?;
        let retired = sqlx::query_scalar::<_, i64>(SQL.owners.find.sql())
            .bind(kind)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .is_some();
        let completeness = decode_usage_completeness(
            counts.try_get(0).map_err(store_sqlx_error)?,
            oldest,
            counts.try_get(2).map_err(store_sqlx_error)?,
            counts.try_get(3).map_err(store_sqlx_error)?,
            outstanding.len() as u64,
            retired,
        )?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(OwnerUsage {
            owner: owner.clone(),
            rows,
            outstanding,
            completeness,
        })
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
        let (kind, id) = usage_owner_columns(owner);
        let mut facts = sqlx::query(SQL.facts.page.sql())
            .bind(kind)
            .bind(id)
            .bind(usage_integer(after.map_or(0, UsageFactCursor::after_seq))?)
            .bind(i64::from(limit.get()) + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .iter()
            .map(decode_fact)
            .collect::<Result<Vec<_>, _>>()?;
        let more = facts.len() > limit.get() as usize;
        facts.truncate(limit.get() as usize);
        let next = if more {
            facts
                .last()
                .map(|f| UsageFactCursor::new(owner.clone(), f.seq))
        } else {
            None
        };
        Ok(UsageFactPage { facts, next })
    }
    async fn load_usage_run_page(
        &self,
        owner: &RuntimeOwner,
        filter: UsageRunFilter,
        after: Option<&UsageRunCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageRunPage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_owner(owner)?;
        }
        let (kind, id) = usage_owner_columns(owner);
        let filter = match filter {
            UsageRunFilter::Open => "open",
            UsageRunFilter::Unresolved => "unresolved",
            UsageRunFilter::All => "all",
        };
        let mut runs = sqlx::query(SQL.runs.page.sql())
            .bind(kind)
            .bind(id)
            .bind(filter)
            .bind(after.map_or("", |c| c.after_effect().as_str()))
            .bind(after.map_or("", |c| c.after_run().as_str()))
            .bind(i64::from(limit.get()) + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .iter()
            .map(|row| decode_run(row, owner))
            .collect::<Result<Vec<_>, _>>()?;
        let more = runs.len() > limit.get() as usize;
        runs.truncate(limit.get() as usize);
        let next = if more {
            runs.last()
                .map(|r| UsageRunCursor::new(owner.clone(), r.effect.clone(), r.run.clone()))
        } else {
            None
        };
        Ok(UsageRunPage { runs, next })
    }
}
