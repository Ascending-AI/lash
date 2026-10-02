//! The stored shape of the usage ledger's rows, and the one decoder both SQL
//! backends run over them.
//!
//! `usage_facts` and `usage_runs` project their record columns in the order
//! `lash_store_sql`'s `RECORD_COLUMNS` lists them; each backend reads a row
//! through its own driver into one of the `Stored*` shapes here, and this
//! module owns every rule that is not row access: the enum spellings, the
//! signed/unsigned column conversions, the owner-kind grammar, and which
//! column combinations a record may hold.

use crate::{
    ModelKey, OutstandingUsageAttempt, OwnerUsageRow, RuntimeOwner, StoreError, UsageCompleteness,
    UsageEffectKey, UsageFactBody, UsageFactConflict, UsageFactIdentity, UsageFactKind,
    UsageFactRecord, UsageRunDispatch, UsageRunId, UsageRunRecord, UsageRunResolution,
    UsageRunState,
};
use lash_sansio::llm::types::LlmCallId;
use lash_sansio::{ProcessId, SessionId, TokenUsage};

/// One row of the `usage_facts` record projection, minus `payload_hash`: the
/// hash is the conflict path's own read, never part of the domain record.
pub struct StoredUsageFact {
    pub seq: i64,
    pub owner_kind: String,
    pub owner_id: String,
    pub effect_key: String,
    pub call_ordinal: i64,
    pub provider_attempt: i64,
    pub fact_kind: String,
    pub disposition: String,
    pub run_id: Option<String>,
    pub llm_call_id: String,
    pub source: String,
    pub model_key: String,
    pub requested_model: String,
    pub served_model: Option<String>,
    pub usage: TokenUsage,
    pub generation_id: Option<String>,
    pub recorded_at_ms: i64,
}

/// One row of the `usage_runs` record projection.
pub struct StoredUsageRun {
    pub effect_key: String,
    pub run_id: String,
    pub execution_scope_key: Option<String>,
    pub source: Option<String>,
    pub model_key: Option<String>,
    pub requested_model: Option<String>,
    pub admitted_at_ms: Option<i64>,
    pub state: String,
    pub unknown_reason: Option<String>,
    pub conflict_call_ordinal: Option<i64>,
    pub conflict_provider_attempt: Option<i64>,
    pub conflict_fact_kind: Option<String>,
    pub conflict_stored_payload_hash: Option<String>,
    pub conflict_offered_payload_hash: Option<String>,
    pub resolved_at_ms: Option<i64>,
}

/// One `(source, model_key, requested_model)` aggregate row of `usage_facts`.
pub struct StoredUsageAggregate {
    pub source: String,
    pub model_key: String,
    pub requested_model: String,
    pub usage: TokenUsage,
    pub reported_attempts: i64,
    pub unreported_attempts: i64,
    pub reconciled_attempts: i64,
}

/// One outstanding-attempts row of `usage_facts`.
pub struct StoredOutstandingAttempt {
    pub effect_key: String,
    pub call_ordinal: i64,
    pub provider_attempt: i64,
    pub llm_call_id: String,
    pub source: String,
    pub model_key: String,
    pub requested_model: String,
    pub generation_id: Option<String>,
}

impl StoredUsageFact {
    /// Decode the stored row into its domain record, rejecting every field
    /// combination no fact can represent.
    pub fn decode(self) -> Result<UsageFactRecord, StoreError> {
        let StoredUsageFact {
            seq,
            owner_kind,
            owner_id,
            effect_key,
            call_ordinal,
            provider_attempt,
            fact_kind,
            disposition,
            run_id,
            llm_call_id,
            source,
            model_key,
            requested_model,
            served_model,
            usage,
            generation_id,
            recorded_at_ms,
        } = self;
        Ok(UsageFactRecord {
            seq: usage_unsigned(seq)?,
            owner: usage_owner(&owner_kind, owner_id)?,
            effect: usage_effect_key(effect_key)?,
            call_ordinal: usage_ordinal(call_ordinal)?,
            provider_attempt: usage_ordinal(provider_attempt)?,
            llm_call_id: LlmCallId(llm_call_id),
            source,
            model_key: ModelKey::new(model_key),
            requested_model,
            served_model,
            body: UsageFactBody::from_stored(
                &fact_kind,
                &disposition,
                run_id.map(UsageRunId::try_from).transpose()?,
                usage,
                generation_id,
            )?,
            recorded_at_ms: usage_unsigned(recorded_at_ms)?,
        })
    }
}

impl StoredUsageRun {
    /// Decode the stored row into its domain record. `owner` is the row's
    /// owner columns already decoded by the caller's query parameters: a run
    /// conflict's fact identity names it, and the columns are not part of
    /// the projection.
    pub fn decode(self, owner: &RuntimeOwner) -> Result<UsageRunRecord, StoreError> {
        let StoredUsageRun {
            effect_key,
            run_id,
            execution_scope_key,
            source,
            model_key,
            requested_model,
            admitted_at_ms,
            state,
            unknown_reason,
            conflict_call_ordinal,
            conflict_provider_attempt,
            conflict_fact_kind,
            conflict_stored_payload_hash,
            conflict_offered_payload_hash,
            resolved_at_ms,
        } = self;
        let effect = usage_effect_key(effect_key)?;
        let admission = match (
            execution_scope_key,
            source,
            model_key,
            requested_model,
            admitted_at_ms,
        ) {
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
                admitted_at_ms: usage_unsigned(at_ms)?,
            }),
            _ => return Err(usage_corrupt("partial usage run admission")),
        };
        let conflict = match (
            conflict_call_ordinal,
            conflict_provider_attempt,
            conflict_fact_kind,
            conflict_stored_payload_hash,
            conflict_offered_payload_hash,
        ) {
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
                    call_ordinal: usage_ordinal(call)?,
                    provider_attempt: usage_ordinal(attempt)?,
                    kind: match kind.as_str() {
                        "attempt" => UsageFactKind::Attempt,
                        "correction" => UsageFactKind::Correction,
                        _ => return Err(usage_corrupt("invalid conflict fact kind")),
                    },
                },
                stored_payload_hash,
                offered_payload_hash,
            }),
            _ => return Err(usage_corrupt("partial usage run conflict")),
        };
        let state = UsageRunState::from_stored(
            &state,
            unknown_reason.as_deref(),
            conflict,
            resolved_at_ms.map(usage_unsigned).transpose()?,
        )?;
        if state == UsageRunState::Open && admission.is_none() {
            return Err(usage_corrupt("open usage run has no admission"));
        }
        Ok(UsageRunRecord {
            effect,
            run: UsageRunId::try_from(run_id)?,
            admission,
            state,
        })
    }
}

impl StoredUsageAggregate {
    pub fn decode(self) -> Result<OwnerUsageRow, StoreError> {
        let StoredUsageAggregate {
            source,
            model_key,
            requested_model,
            usage,
            reported_attempts,
            unreported_attempts,
            reconciled_attempts,
        } = self;
        Ok(OwnerUsageRow {
            source,
            model_key: ModelKey::new(model_key),
            requested_model,
            usage,
            reported_attempts: usage_unsigned(reported_attempts)?,
            unreported_attempts: usage_unsigned(unreported_attempts)?,
            reconciled_attempts: usage_unsigned(reconciled_attempts)?,
        })
    }
}

impl StoredOutstandingAttempt {
    pub fn decode(self) -> Result<OutstandingUsageAttempt, StoreError> {
        let StoredOutstandingAttempt {
            effect_key,
            call_ordinal,
            provider_attempt,
            llm_call_id,
            source,
            model_key,
            requested_model,
            generation_id,
        } = self;
        Ok(OutstandingUsageAttempt {
            effect: usage_effect_key(effect_key)?,
            call_ordinal: usage_ordinal(call_ordinal)?,
            provider_attempt: usage_ordinal(provider_attempt)?,
            llm_call_id: LlmCallId(llm_call_id),
            source,
            model_key: ModelKey::new(model_key),
            requested_model,
            generation_id,
        })
    }
}

/// The `usage_runs.completeness` aggregate row plus the caller's unreported
/// count and retirement read.
pub fn decode_usage_completeness(
    open_runs: i64,
    oldest_open_admitted_at_ms: Option<i64>,
    unknown_runs: i64,
    conflicted_runs: i64,
    unreported_attempts: u64,
    retired: bool,
) -> Result<UsageCompleteness, StoreError> {
    Ok(UsageCompleteness {
        open_runs: usage_unsigned(open_runs)?,
        oldest_open_admitted_at_ms: oldest_open_admitted_at_ms.map(usage_unsigned).transpose()?,
        unknown_runs: usage_unsigned(unknown_runs)?,
        conflicted_runs: usage_unsigned(conflicted_runs)?,
        unreported_attempts,
        retired,
    })
}

/// The `(state, unknown_reason)` columns a resolution writes.
pub fn usage_run_resolution_columns(
    resolution: &UsageRunResolution,
) -> (&'static str, Option<&'static str>) {
    match resolution {
        UsageRunResolution::Settled => ("settled", None),
        UsageRunResolution::Unknown(reason) => ("unknown", Some(reason.as_str())),
    }
}

/// The `(owner_kind, owner_id)` pair decodes back to the owner.
pub fn usage_owner(owner_kind: &str, owner_id: String) -> Result<RuntimeOwner, StoreError> {
    match owner_kind {
        "session" => Ok(RuntimeOwner::Session(SessionId::from(owner_id))),
        "process" => Ok(RuntimeOwner::Process(
            ProcessId::parse(&owner_id).map_err(|error| usage_corrupt(error.to_string()))?,
        )),
        other => Err(usage_corrupt(format!("invalid owner kind {other}"))),
    }
}

pub fn usage_corrupt(message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: "usage accounting",
        message: message.into(),
    }
}

/// The `u64` a `*at_ms` or count column stores, into the signed SQL range.
pub fn usage_integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| usage_corrupt("usage integer exceeds SQL range"))
}

/// The `u64` a sequence, timestamp or count column carries.
pub fn usage_unsigned(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| usage_corrupt("negative usage sequence, timestamp or count"))
}

/// The `u32` a call- or attempt-ordinal column carries.
pub fn usage_ordinal(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| usage_corrupt("usage ordinal exceeds u32"))
}

/// The `UsageEffectKey` an `effect_key` column carries.
pub fn usage_effect_key(value: String) -> Result<UsageEffectKey, StoreError> {
    serde_json::from_value(serde_json::Value::String(value))
        .map_err(|error| usage_corrupt(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UsageReporting, UsageRunOutcome, UsageUnknownReason};

    fn fact(overrides: impl FnOnce(&mut StoredUsageFact)) -> StoredUsageFact {
        let mut fact = StoredUsageFact {
            seq: 7,
            owner_kind: "session".into(),
            owner_id: "owner".into(),
            effect_key: "effect".into(),
            call_ordinal: 0,
            provider_attempt: 0,
            fact_kind: "attempt".into(),
            disposition: "reported".into(),
            run_id: Some("run:00000000000040008000000000000001".into()),
            llm_call_id: "call".into(),
            source: "turn".into(),
            model_key: "key".into(),
            requested_model: "model".into(),
            served_model: None,
            usage: TokenUsage::default(),
            generation_id: None,
            recorded_at_ms: 10,
        };
        overrides(&mut fact);
        fact
    }

    #[test]
    fn fact_rows_decode_through_the_shared_rules() {
        let record = fact(|_| {}).decode().expect("a reported attempt decodes");
        assert_eq!(record.seq, 7);
        assert_eq!(record.disposition(), UsageReporting::Reported);
        assert_eq!(
            record.owner,
            RuntimeOwner::Session(SessionId::from("owner"))
        );
    }

    #[test]
    fn fact_rows_reject_an_unknown_owner_kind() {
        let error = fact(|fact| fact.owner_kind = "ghost".into())
            .decode()
            .expect_err("an owner kind outside the grammar is corrupt");
        assert!(matches!(
            error,
            StoreError::StoredDataCorrupt { ref message, .. } if message == "invalid owner kind ghost"
        ));
    }

    #[test]
    fn fact_rows_reject_negative_sequences_and_ordinals() {
        for corrupt in [
            |fact: &mut StoredUsageFact| fact.seq = -1,
            |fact: &mut StoredUsageFact| fact.call_ordinal = -1,
            |fact: &mut StoredUsageFact| fact.recorded_at_ms = -1,
        ] {
            assert!(matches!(
                fact(corrupt).decode(),
                Err(StoreError::StoredDataCorrupt { .. })
            ));
        }
    }

    #[test]
    fn fact_rows_reject_an_invalid_run_id() {
        fact(|fact| fact.run_id = Some("run:nope".into()))
            .decode()
            .expect_err("a malformed run id is corrupt");
    }

    fn run(overrides: impl FnOnce(&mut StoredUsageRun)) -> StoredUsageRun {
        let mut run = StoredUsageRun {
            effect_key: "effect".into(),
            run_id: "run:00000000000040008000000000000001".into(),
            execution_scope_key: Some("scope".into()),
            source: Some("turn".into()),
            model_key: Some("key".into()),
            requested_model: Some("model".into()),
            admitted_at_ms: Some(10),
            state: "open".into(),
            unknown_reason: None,
            conflict_call_ordinal: None,
            conflict_provider_attempt: None,
            conflict_fact_kind: None,
            conflict_stored_payload_hash: None,
            conflict_offered_payload_hash: None,
            resolved_at_ms: None,
        };
        overrides(&mut run);
        run
    }

    fn owner() -> RuntimeOwner {
        RuntimeOwner::Session(SessionId::from("owner"))
    }

    #[test]
    fn open_runs_require_their_admission_evidence() {
        assert_eq!(
            run(|_| {}).decode(&owner()).expect("open").state,
            UsageRunState::Open
        );
        run(|run| run.admitted_at_ms = None)
            .decode(&owner())
            .expect_err("a partial admission is corrupt");
        run(|run| {
            run.execution_scope_key = None;
            run.source = None;
            run.model_key = None;
            run.requested_model = None;
            run.admitted_at_ms = None;
        })
        .decode(&owner())
        .expect_err("an open run without any admission is corrupt");
    }

    #[test]
    fn conflict_columns_decode_as_a_unit() {
        let resolved = run(|run| {
            run.state = "conflicted".into();
            run.conflict_call_ordinal = Some(0);
            run.conflict_provider_attempt = Some(1);
            run.conflict_fact_kind = Some("attempt".into());
            run.conflict_stored_payload_hash = Some("stored".into());
            run.conflict_offered_payload_hash = Some("offered".into());
            run.resolved_at_ms = Some(20);
        })
        .decode(&owner())
        .expect("a conflicted run decodes");
        match resolved.state {
            UsageRunState::Resolved {
                outcome: UsageRunOutcome::Conflicted(conflict),
                ..
            } => {
                assert_eq!(conflict.identity.kind, UsageFactKind::Attempt);
                assert_eq!(conflict.stored_payload_hash, "stored");
            }
            other => panic!("expected a conflicted outcome, got {other:?}"),
        }
        run(|run| {
            run.state = "conflicted".into();
            run.conflict_call_ordinal = Some(0);
            run.conflict_provider_attempt = Some(1);
            run.conflict_fact_kind = Some("invented".into());
            run.conflict_stored_payload_hash = Some("stored".into());
            run.conflict_offered_payload_hash = Some("offered".into());
            run.resolved_at_ms = Some(20);
        })
        .decode(&owner())
        .expect_err("an unknown conflict fact kind is corrupt");
        run(|run| {
            run.state = "conflicted".into();
            run.conflict_stored_payload_hash = Some("stored".into());
            run.resolved_at_ms = Some(20);
        })
        .decode(&owner())
        .expect_err("a partial conflict is corrupt");
    }

    #[test]
    fn unknown_runs_decode_their_reason() {
        let resolved = run(|run| {
            run.state = "unknown".into();
            run.unknown_reason = Some("superseded_run".into());
            run.resolved_at_ms = Some(30);
        })
        .decode(&owner())
        .expect("an unknown run decodes");
        assert!(matches!(
            resolved.state.outcome(),
            Some(UsageRunOutcome::Unknown(UsageUnknownReason::SupersededRun))
        ));
    }

    #[test]
    fn aggregate_and_outstanding_rows_share_the_scalar_rules() {
        let aggregate = StoredUsageAggregate {
            source: "turn".into(),
            model_key: "key".into(),
            requested_model: "model".into(),
            usage: TokenUsage::default(),
            reported_attempts: 2,
            unreported_attempts: 1,
            reconciled_attempts: 3,
        };
        let row = aggregate.decode().expect("aggregate row");
        assert_eq!(row.reported_attempts + row.unreported_attempts, 3);
        assert!(matches!(
            StoredUsageAggregate {
                reconciled_attempts: -1,
                ..row_fields()
            }
            .decode(),
            Err(StoreError::StoredDataCorrupt { .. })
        ));
        let attempt = StoredOutstandingAttempt {
            effect_key: "effect".into(),
            call_ordinal: 0,
            provider_attempt: 2,
            llm_call_id: "call".into(),
            source: "turn".into(),
            model_key: "key".into(),
            requested_model: "model".into(),
            generation_id: None,
        }
        .decode()
        .expect("outstanding attempt");
        assert_eq!(attempt.provider_attempt, 2);
    }

    fn row_fields() -> StoredUsageAggregate {
        StoredUsageAggregate {
            source: "turn".into(),
            model_key: "key".into(),
            requested_model: "model".into(),
            usage: TokenUsage::default(),
            reported_attempts: 0,
            unreported_attempts: 0,
            reconciled_attempts: 0,
        }
    }

    #[test]
    fn completeness_decodes_its_counts() {
        let completeness =
            decode_usage_completeness(1, Some(5), 2, 3, 4, true).expect("completeness");
        assert_eq!(completeness.open_runs, 1);
        assert_eq!(completeness.oldest_open_admitted_at_ms, Some(5));
        assert!(!completeness.is_settled());
        assert!(matches!(
            decode_usage_completeness(-1, None, 0, 0, 0, false),
            Err(StoreError::StoredDataCorrupt { .. })
        ));
    }

    #[test]
    fn resolution_columns_stay_typed() {
        assert_eq!(
            usage_run_resolution_columns(&UsageRunResolution::Settled),
            ("settled", None)
        );
        assert_eq!(
            usage_run_resolution_columns(&UsageRunResolution::Unknown(
                UsageUnknownReason::ExecutionEnded
            )),
            ("unknown", Some("execution_ended"))
        );
    }
}
