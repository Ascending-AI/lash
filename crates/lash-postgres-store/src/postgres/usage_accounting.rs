//! Atomic accounting over the durable-core catalog. No session head or drive lease is involved.
use crate::support::store_sqlx_error;
use crate::{PostgresStore, begin_guarded};
use async_trait::async_trait;
use lash_core_execution::RuntimeOwner;
use lash_core_execution::UsageAccountingStore;
use lash_core_execution::{LlmCallId, ModelKey, StoreError, TokenUsage};
use lash_core_execution::{
    OutstandingUsageAttempt, OwnerUsage, OwnerUsageRow, UsageAdmissionError, UsageAppendError,
    UsageAppendReceipt, UsageCompleteness, UsageCorrection, UsageEffectKey, UsageFactBody,
    UsageFactConflict, UsageFactCursor, UsageFactIdentity, UsageFactKind, UsageFactPage,
    UsageFactRecord, UsageOwnerRetired, UsageReporting, UsageRunAdmission, UsageRunAdmitted,
    UsageRunCursor, UsageRunDispatch, UsageRunFilter, UsageRunId, UsageRunPage, UsageRunRecord,
    UsageRunResolution, UsageRunState, UsageSettleReceipt, UsageSettlement,
    usage_correction_payload_hash, usage_fact_payload_hash, usage_owner_columns,
};
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
        fact = "INSERT INTO usage_facts (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind, disposition, run_id, llm_call_id, source, model_key, requested_model, served_model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, generation_id, payload_hash, recorded_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21) ON CONFLICT ON CONSTRAINT uq_usage_facts_identity DO NOTHING RETURNING seq";
        run = "INSERT INTO usage_runs (owner_kind, owner_id, effect_key, run_id, execution_scope_key, source, model_key, requested_model, admitted_at_ms, state, unknown_reason, resolved_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT (owner_kind, owner_id, effect_key, run_id) DO NOTHING";
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
fn corrupt(message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: "usage accounting",
        message: message.into(),
    }
}
fn integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| corrupt("usage integer exceeds SQL range"))
}
fn unsigned(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt("negative usage sequence, timestamp or count"))
}
fn ordinal(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| corrupt("usage ordinal exceeds u32"))
}
fn effect(value: String) -> Result<UsageEffectKey, StoreError> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(|e| corrupt(e.to_string()))
}
fn decode_fact(row: &PgRow) -> Result<UsageFactRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.try_get($n).map_err(store_sqlx_error)?
        };
    }
    let owner_kind: String = get!(1);
    let owner_id: String = get!(2);
    let owner = match owner_kind.as_str() {
        "session" => RuntimeOwner::Session(lash_sansio::SessionId::from(owner_id)),
        "process" => RuntimeOwner::Process(
            lash_sansio::ProcessId::parse(&owner_id).map_err(|e| corrupt(e.to_string()))?,
        ),
        other => return Err(corrupt(format!("invalid owner kind {other}"))),
    };
    let kind: String = get!(6);
    let disposition: String = get!(7);
    let run: Option<String> = get!(8);
    Ok(UsageFactRecord {
        seq: unsigned(get!(0))?,
        owner,
        effect: effect(get!(3))?,
        call_ordinal: ordinal(get!(4))?,
        provider_attempt: ordinal(get!(5))?,
        llm_call_id: LlmCallId(row.try_get::<String, _>(9).map_err(store_sqlx_error)?),
        source: get!(10),
        model_key: ModelKey::new(row.try_get::<String, _>(11).map_err(store_sqlx_error)?),
        requested_model: get!(12),
        served_model: get!(13),
        body: UsageFactBody::from_stored(
            &kind,
            &disposition,
            run.map(UsageRunId::try_from).transpose()?,
            TokenUsage {
                input_tokens: get!(14),
                output_tokens: get!(15),
                cache_read_input_tokens: get!(16),
                cache_write_input_tokens: get!(17),
                reasoning_output_tokens: get!(18),
            },
            get!(19),
        )?,
        recorded_at_ms: unsigned(get!(21))?,
    })
}
fn decode_run(row: &PgRow, owner: &RuntimeOwner) -> Result<UsageRunRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.try_get($n).map_err(store_sqlx_error)?
        };
    }
    let effect = effect(get!(0))?;
    let scope: Option<String> = get!(2);
    let source: Option<String> = get!(3);
    let model: Option<String> = get!(4);
    let requested: Option<String> = get!(5);
    let admitted: Option<i64> = get!(6);
    let admission = match (scope, source, model, requested, admitted) {
        (None, None, None, None, None) => None,
        (
            Some(execution_scope_key),
            Some(source),
            Some(model),
            Some(requested_model),
            Some(at_ms),
        ) => Some(UsageRunDispatch {
            execution_scope_key,
            source,
            model_key: ModelKey::new(model),
            requested_model,
            admitted_at_ms: unsigned(at_ms)?,
        }),
        _ => return Err(corrupt("partial usage run admission")),
    };
    let call: Option<i64> = get!(9);
    let attempt: Option<i64> = get!(10);
    let fact_kind: Option<String> = get!(11);
    let stored: Option<String> = get!(12);
    let offered: Option<String> = get!(13);
    let conflict = match (call, attempt, fact_kind, stored, offered) {
        (None, None, None, None, None) => None,
        (
            Some(call),
            Some(attempt),
            Some(kind),
            Some(stored_payload_hash),
            Some(offered_payload_hash),
        ) => Some(UsageFactConflict {
            identity: UsageFactIdentity {
                owner: owner.clone(),
                effect: effect.clone(),
                call_ordinal: ordinal(call)?,
                provider_attempt: ordinal(attempt)?,
                kind: match kind.as_str() {
                    "attempt" => UsageFactKind::Attempt,
                    "correction" => UsageFactKind::Correction,
                    _ => return Err(corrupt("invalid conflict fact kind")),
                },
            },
            stored_payload_hash,
            offered_payload_hash,
        }),
        _ => return Err(corrupt("partial usage run conflict")),
    };
    let state: String = get!(7);
    let reason: Option<String> = get!(8);
    let resolved: Option<i64> = get!(14);
    let state = UsageRunState::from_stored(
        &state,
        reason.as_deref(),
        conflict,
        resolved.map(unsigned).transpose()?,
    )?;
    if state == UsageRunState::Open && admission.is_none() {
        return Err(corrupt("open usage run has no admission"));
    }
    let run: String = get!(1);
    Ok(UsageRunRecord {
        effect,
        run: UsageRunId::try_from(run)?,
        admission,
        state,
    })
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
        .bind(record.model_key.as_str())
        .bind(&record.requested_model)
        .bind(&record.served_model)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.cache_read_input_tokens)
        .bind(usage.cache_write_input_tokens)
        .bind(usage.reasoning_output_tokens)
        .bind(record.generation_id())
        .bind(hash)
        .bind(integer(record.recorded_at_ms)?)
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
fn resolution_columns(resolution: &UsageRunResolution) -> (&'static str, Option<&'static str>) {
    match resolution {
        UsageRunResolution::Settled => ("settled", None),
        UsageRunResolution::Unknown(reason) => ("unknown", Some(reason.as_str())),
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
                retired_at_ms: unsigned(retired)?,
            });
        }
        let inserted = sqlx::query(SQL.inserts.run.sql())
            .bind(kind)
            .bind(id)
            .bind(a.effect.as_str())
            .bind(a.run.as_str())
            .bind(&a.execution_scope_key)
            .bind(&a.source)
            .bind(a.model_key.as_str())
            .bind(&a.requested_model)
            .bind(integer(a.admitted_at_ms)?)
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
        let now = integer(now_ms)?;
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
        let (state, reason) = resolution_columns(&receipt.run);
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
        .map_err(|_| corrupt("superseded run count exceeds u32"))?;
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
            return Err(corrupt("usage conflict belongs to another owner or effect"));
        }
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_owner(&mut tx, &s.owner).await?;
        let (kind, id) = usage_owner_columns(&s.owner);
        let now = integer(now_ms)?;
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
            .bind(integer(now_ms)?)
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
            .bind(integer(now_ms)?)
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
            .bind(integer(now_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(UsageOwnerRetired {
            retired_at_ms: unsigned(retired)?,
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
                Ok(OwnerUsageRow {
                    source: row.try_get(0).map_err(store_sqlx_error)?,
                    model_key: ModelKey::new(
                        row.try_get::<String, _>(1).map_err(store_sqlx_error)?,
                    ),
                    requested_model: row.try_get(2).map_err(store_sqlx_error)?,
                    usage: TokenUsage {
                        input_tokens: row.try_get(3).map_err(store_sqlx_error)?,
                        output_tokens: row.try_get(4).map_err(store_sqlx_error)?,
                        cache_read_input_tokens: row.try_get(5).map_err(store_sqlx_error)?,
                        cache_write_input_tokens: row.try_get(6).map_err(store_sqlx_error)?,
                        reasoning_output_tokens: row.try_get(7).map_err(store_sqlx_error)?,
                    },
                    reported_attempts: unsigned(row.try_get(8).map_err(store_sqlx_error)?)?,
                    unreported_attempts: unsigned(row.try_get(9).map_err(store_sqlx_error)?)?,
                    reconciled_attempts: unsigned(row.try_get(10).map_err(store_sqlx_error)?)?,
                })
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
                Ok(OutstandingUsageAttempt {
                    effect: effect(row.try_get(0).map_err(store_sqlx_error)?)?,
                    call_ordinal: ordinal(row.try_get(1).map_err(store_sqlx_error)?)?,
                    provider_attempt: ordinal(row.try_get(2).map_err(store_sqlx_error)?)?,
                    llm_call_id: LlmCallId(row.try_get::<String, _>(3).map_err(store_sqlx_error)?),
                    source: row.try_get(4).map_err(store_sqlx_error)?,
                    model_key: ModelKey::new(
                        row.try_get::<String, _>(5).map_err(store_sqlx_error)?,
                    ),
                    requested_model: row.try_get(6).map_err(store_sqlx_error)?,
                    generation_id: row.try_get(7).map_err(store_sqlx_error)?,
                })
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
        let completeness = UsageCompleteness {
            open_runs: unsigned(counts.try_get(0).map_err(store_sqlx_error)?)?,
            oldest_open_admitted_at_ms: oldest.map(unsigned).transpose()?,
            unknown_runs: unsigned(counts.try_get(2).map_err(store_sqlx_error)?)?,
            conflicted_runs: unsigned(counts.try_get(3).map_err(store_sqlx_error)?)?,
            unreported_attempts: outstanding.len() as u64,
            retired,
        };
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
            .bind(integer(after.map_or(0, UsageFactCursor::after_seq))?)
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
