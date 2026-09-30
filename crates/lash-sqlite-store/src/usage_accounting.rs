//! Atomic accounting over the durable-core catalog. No session head or drive lease is involved.
use crate::conn::TxOutcome;
use crate::schema_layout::Schema;
use crate::{SqliteStore, sqlite_error};
use async_trait::async_trait;
use lash_core_execution::{LlmCallId, StoreError, TokenUsage};
use lash_core_store::RuntimeOwner;
use lash_core_store::store::usage_accounting::UsageAccountingStore;
use lash_core_store::usage_accounting::*;
use lash_store_sql::usage::{
    usage_facts::UsageFactsStatements, usage_owner_retirements::UsageOwnerRetirementsStatements,
    usage_runs::UsageRunsStatements,
};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::num::NonZeroU32;
use std::sync::LazyLock;

struct Statements {
    facts: UsageFactsStatements,
    runs: UsageRunsStatements,
    owners: UsageOwnerRetirementsStatements,
    inserts: UsageInsertStatements,
}
lash_store_sql::statements! {
    pub(crate) struct UsageInsertStatements @ "usage_sqlite" {
        fact = "INSERT OR IGNORE INTO usage_facts (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind, disposition, run_id, llm_call_id, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, generation_id, payload_hash, recorded_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)";
        run = "INSERT OR IGNORE INTO usage_runs (owner_kind, owner_id, effect_key, run_id, execution_scope_key, source, model, admitted_at_ms, state, unknown_reason, resolved_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";
        owner = "INSERT OR IGNORE INTO usage_owner_retirements (owner_kind, owner_id, retired_at_ms) VALUES (?1, ?2, ?3)";
    }
}
static SQL: LazyLock<Statements> = LazyLock::new(|| Statements {
    facts: UsageFactsStatements::render(Schema::Main.dialect()),
    runs: UsageRunsStatements::render(Schema::Main.dialect()),
    owners: UsageOwnerRetirementsStatements::render(Schema::Main.dialect()),
    inserts: UsageInsertStatements::render(Schema::Main.dialect()),
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
fn decode_fact(row: &Row<'_>) -> Result<UsageFactRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.get($n).map_err(sqlite_error)?
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
        identity: UsageFactIdentity {
            owner,
            effect: effect(get!(3))?,
            call_ordinal: ordinal(get!(4))?,
            provider_attempt: ordinal(get!(5))?,
            kind: match kind.as_str() {
                "attempt" => UsageFactKind::Attempt,
                "correction" => UsageFactKind::Correction,
                _ => return Err(corrupt("invalid fact kind")),
            },
        },
        disposition: match disposition.as_str() {
            "reported" => UsageDisposition::Reported,
            "unreported" => UsageDisposition::Unreported,
            "reconciled" => UsageDisposition::Reconciled,
            _ => return Err(corrupt("invalid disposition")),
        },
        run: run.map(UsageRunId::try_from).transpose()?,
        llm_call_id: LlmCallId(row.get::<_, String>(9).map_err(sqlite_error)?),
        source: get!(10),
        model: get!(11),
        usage: TokenUsage {
            input_tokens: get!(12),
            output_tokens: get!(13),
            cache_read_input_tokens: get!(14),
            cache_write_input_tokens: get!(15),
            reasoning_output_tokens: get!(16),
        },
        generation_id: get!(17),
        recorded_at_ms: unsigned(get!(19))?,
    })
}
fn decode_run(row: &Row<'_>) -> Result<UsageRunRecord, StoreError> {
    macro_rules! get {
        ($n:expr) => {
            row.get($n).map_err(sqlite_error)?
        };
    }
    let state: String = get!(6);
    let reason: Option<String> = get!(7);
    let detail: Option<String> = get!(8);
    let resolved: Option<i64> = get!(9);
    Ok(UsageRunRecord {
        effect: effect(get!(0))?,
        run: UsageRunId::try_from(row.get::<_, String>(1).map_err(sqlite_error)?)?,
        execution_scope_key: get!(2),
        source: get!(3),
        model: get!(4),
        admitted_at_ms: unsigned(get!(5))?,
        state: match state.as_str() {
            "open" => UsageRunState::Open,
            "settled" => UsageRunState::Settled,
            "unknown" => UsageRunState::Unknown(UsageUnknownReason::from_stored(
                reason
                    .as_deref()
                    .ok_or_else(|| corrupt("missing unknown reason"))?,
            )?),
            "conflicted" => UsageRunState::Conflicted {
                detail: detail.ok_or_else(|| corrupt("missing conflict detail"))?,
            },
            _ => return Err(corrupt("invalid run state")),
        },
        resolved_at_ms: resolved.map(unsigned).transpose()?,
    })
}
fn insert_fact(
    tx: &Transaction<'_>,
    record: &UsageFactRecord,
    hash: &str,
) -> Result<bool, UsageAppendError> {
    let (kind, id) = usage_owner_columns(&record.identity.owner);
    let inserted = tx
        .execute(
            SQL.inserts.fact.sql(),
            params![
                kind,
                id,
                record.identity.effect.as_str(),
                i64::from(record.identity.call_ordinal),
                i64::from(record.identity.provider_attempt),
                record.identity.kind.as_str(),
                record.disposition.as_str(),
                record.run.as_ref().map(UsageRunId::as_str),
                record.llm_call_id.0.as_str(),
                record.source,
                record.model,
                record.usage.input_tokens,
                record.usage.output_tokens,
                record.usage.cache_read_input_tokens,
                record.usage.cache_write_input_tokens,
                record.usage.reasoning_output_tokens,
                record.generation_id,
                hash,
                integer(record.recorded_at_ms)?
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
                record.identity.effect.as_str(),
                i64::from(record.identity.call_ordinal),
                i64::from(record.identity.provider_attempt),
                record.identity.kind.as_str()
            ],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if stored == hash {
        Ok(false)
    } else {
        Err(UsageAppendError::Conflict(Box::new(UsageFactConflict {
            identity: record.identity.clone(),
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
    async fn admit_usage_run(
        &self,
        admission: &UsageRunAdmission,
    ) -> Result<UsageRunAdmitted, UsageAdmissionError> {
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
                    retired_at_ms: unsigned(retired)?,
                });
            }
            let inserted = tx
                .execute(
                    SQL.inserts.run.sql(),
                    params![
                        kind,
                        id,
                        a.effect.as_str(),
                        a.run.as_str(),
                        a.execution_scope_key,
                        a.source,
                        a.model,
                        integer(a.admitted_at_ms)?,
                        "open",
                        Option::<&str>::None,
                        Option::<i64>::None
                    ],
                )
                .map_err(sqlite_error)?;
            Ok(if inserted == 0 {
                UsageRunAdmitted::AlreadyAdmitted
            } else {
                UsageRunAdmitted::Admitted
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
            let now = integer(now_ms)?;
            let mut receipt = UsageSettleReceipt {
                inserted_facts: 0,
                duplicate_facts: 0,
                run: s.accounting.resolution(),
                superseded_runs: 0,
            };
            for fact in &s.facts {
                if insert_fact(
                    tx,
                    &fact.record(&s.owner, &s.effect, &s.run, now_ms),
                    &usage_fact_payload_hash(fact, &s.run),
                )? {
                    receipt.inserted_facts += 1;
                } else {
                    receipt.duplicate_facts += 1;
                }
            }
            let (state, reason) = resolution_columns(&receipt.run);
            let first = s.facts.first();
            tx.execute(
                SQL.inserts.run.sql(),
                params![
                    kind,
                    id,
                    s.effect.as_str(),
                    s.run.as_str(),
                    "",
                    first.map_or("", |f| f.source.as_str()),
                    first.map_or("", |f| f.model.as_str()),
                    now,
                    state,
                    reason,
                    now
                ],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                SQL.runs.resolve.sql(),
                params![
                    kind,
                    id,
                    s.effect.as_str(),
                    s.run.as_str(),
                    state,
                    reason,
                    now
                ],
            )
            .map_err(sqlite_error)?;
            let actual = tx
                .query_row(
                    SQL.runs.find.sql(),
                    params![kind, id, s.effect.as_str(), s.run.as_str()],
                    |row| Ok(decode_run(row)),
                )
                .map_err(sqlite_error)??;
            if actual.state == UsageRunState::Settled {
                receipt.run = UsageRunResolution::Settled;
            }
            receipt.superseded_runs = u32::try_from(
                tx.execute(
                    SQL.runs.supersede.sql(),
                    params![kind, id, s.effect.as_str(), s.run.as_str(), now],
                )
                .map_err(sqlite_error)?,
            )
            .map_err(|_| corrupt("superseded run count exceeds u32"))?;
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
        let s = settlement.clone();
        let detail = format!("{conflict:?}");
        self.usage_write(move |tx| {
            let (kind, id) = usage_owner_columns(&s.owner);
            let now = integer(now_ms)?;
            let first = s.facts.first();
            tx.execute(
                SQL.inserts.run.sql(),
                params![
                    kind,
                    id,
                    s.effect.as_str(),
                    s.run.as_str(),
                    "",
                    first.map_or("", |f| f.source.as_str()),
                    first.map_or("", |f| f.model.as_str()),
                    now,
                    "open",
                    Option::<&str>::None,
                    Option::<i64>::None
                ],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                SQL.runs.conflict.sql(),
                params![kind, id, s.effect.as_str(), s.run.as_str(), detail, now],
            )
            .map_err(sqlite_error)?;
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
                if record.disposition != UsageDisposition::Unreported {
                    return Err(UsageAppendError::CorrectionTargetReported { identity });
                }
                let hash = usage_correction_payload_hash(
                    correction,
                    &record.llm_call_id,
                    &record.source,
                    &record.model,
                );
                record.identity.kind = UsageFactKind::Correction;
                record.disposition = UsageDisposition::Reconciled;
                record.run = None;
                record.usage = correction.usage.clone();
                record.generation_id = Some(correction.generation_id.clone());
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
                    SQL.runs.retire_execution.sql(),
                    params![kind, id, scope, integer(now_ms)?],
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
                .execute(SQL.inserts.owner.sql(), params![kind, id, integer(now_ms)?])
                .map_err(sqlite_error)?;
            let retired = unsigned(
                tx.query_row(SQL.owners.find.sql(), params![kind, id], |row| row.get(0))
                    .map_err(sqlite_error)?,
            )?;
            let count = tx
                .execute(
                    SQL.runs.retire_owner.sql(),
                    params![kind, id, integer(now_ms)?],
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
                                TokenUsage {
                                    input_tokens: row.get(2)?,
                                    output_tokens: row.get(3)?,
                                    cache_read_input_tokens: row.get(4)?,
                                    cache_write_input_tokens: row.get(5)?,
                                    reasoning_output_tokens: row.get(6)?,
                                },
                                row.get::<_, i64>(7)?,
                                row.get::<_, i64>(8)?,
                                row.get::<_, i64>(9)?,
                            ))
                        })
                        .map_err(sqlite_error)?
                        .map(|row| {
                            let (source, model, usage, reported, unreported, reconciled) =
                                row.map_err(sqlite_error)?;
                            Ok(OwnerUsageRow {
                                source,
                                model,
                                usage,
                                reported_attempts: unsigned(reported)?,
                                unreported_attempts: unsigned(unreported)?,
                                reconciled_attempts: unsigned(reconciled)?,
                            })
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
                                row.get::<_, Option<String>>(6)?,
                            ))
                        })
                        .map_err(sqlite_error)?
                        .map(|row| {
                            let (key, call, attempt, llm_call, source, model, generation_id) =
                                row.map_err(sqlite_error)?;
                            Ok(OutstandingUsageAttempt {
                                effect: effect(key)?,
                                call_ordinal: ordinal(call)?,
                                provider_attempt: ordinal(attempt)?,
                                llm_call_id: LlmCallId(llm_call),
                                source,
                                model,
                                generation_id,
                            })
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let (open, oldest, unknown, conflicted): (i64, Option<i64>, i64, i64) = tx
                        .query_row(SQL.runs.completeness.sql(), params![kind, id], |row| {
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
                        completeness: UsageCompleteness {
                            open_runs: unsigned(open)?,
                            oldest_open_admitted_at_ms: oldest.map(unsigned).transpose()?,
                            unknown_runs: unsigned(unknown)?,
                            conflicted_runs: unsigned(conflicted)?,
                            unreported_attempts: outstanding.len() as u64,
                            retired,
                        },
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
        let after = integer(after.map_or(0, UsageFactCursor::after_seq))?;
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
        let owner = owner.clone();
        let after = after.cloned();
        let filter = match filter {
            UsageRunFilter::Open => "open",
            UsageRunFilter::Unresolved => "unresolved",
            UsageRunFilter::All => "all",
        };
        self.read_connection()
            .read(move |tx| {
                let (kind, id) = usage_owner_columns(&owner);
                let records = tx
                    .prepare_cached(SQL.runs.page.sql())?
                    .query_map(
                        params![
                            kind,
                            id,
                            filter,
                            after.as_ref().map_or("", |c| c.after_effect().as_str()),
                            after.as_ref().map_or("", |c| c.after_run().as_str()),
                            i64::from(limit.get()) + 1
                        ],
                        |row| Ok(decode_run(row)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let page = || -> Result<UsageRunPage, StoreError> {
                    let mut runs = records.into_iter().collect::<Result<Vec<_>, _>>()?;
                    let more = runs.len() > limit.get() as usize;
                    runs.truncate(limit.get() as usize);
                    let next = if more {
                        runs.last().map(|run| {
                            UsageRunCursor::new(owner, run.effect.clone(), run.run.clone())
                        })
                    } else {
                        None
                    };
                    Ok(UsageRunPage { runs, next })
                };
                Ok(page())
            })
            .await
            .map_err(sqlite_error)?
    }
}
