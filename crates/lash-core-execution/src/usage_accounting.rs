//! Model usage is engine-owned accounting delivered per call (ADR 0125): the
//! kernel half.
//!
//! A *spending effect* is a journaled effect whose body may dispatch a
//! provider call (`LlmCall`, `Direct`, `ToolAttempt`). One execution of that
//! body is a [`UsageRun`]. The engine's controller begins the run when the
//! body starts and hands it to the body's dispatch sites; each dispatch takes a
//! [`UsageCall`] from it, which admits the run to storage before the first
//! provider attempt (the accounting obligation exists before anything can be
//! billed) and seals the call's attempt facts when its record is sealed.
//!
//! The controller journals the run's [`EffectUsage`] beside the effect's
//! outcome and delivers its settlement to storage through
//! [`project_usage_settlement`], the only production writer of settled usage.
//! Nothing in the drive, the turn loop or a commit carries usage.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

use crate::provider::{DispatchAdmission, DispatchRefused, ProviderDispatch};
use crate::{
    AttemptFactOutcome, AttemptUsageOutcome, Clock, LlmCallRecord, RunAccounting, RuntimeOwner,
    StoreError, UsageAccountingStore, UsageAdmissionError, UsageAppendError, UsageAttemptFact,
    UsageEffectKey, UsageFactConflict, UsageRunAdmission, UsageRunId, UsageSettleReceipt,
    UsageSettlement,
};

/// One execution of a spending effect's body.
///
/// Created by the engine's controller when the body starts and cloned into
/// every dispatch site of the body. The run is admitted to storage once, by
/// the first provider attempt any of its calls makes.
#[derive(Clone)]
pub struct UsageRun {
    inner: Arc<UsageRunInner>,
}

struct UsageRunInner {
    effect: UsageEffectKey,
    execution_scope_key: String,
    run: UsageRunId,
    store: Arc<dyn UsageAccountingStore>,
    clock: Arc<dyn Clock>,
    /// Serializes the run's one admission across concurrent calls.
    admission: tokio::sync::Mutex<RunAdmission>,
    progress: Mutex<RunProgress>,
}

#[derive(Clone)]
enum RunAdmission {
    Pending,
    Admitted,
    Refused(DispatchRefused),
    Faulted(DispatchRefused),
}

#[derive(Default)]
struct RunProgress {
    owner: Option<RuntimeOwner>,
    next_call_ordinal: u32,
    calls: BTreeMap<u32, CallProgress>,
    facts: Vec<UsageAttemptFact>,
    admitted: bool,
    admission_fault: Option<String>,
}

#[derive(Default)]
struct CallProgress {
    /// `AttemptRecord::ordinal`s this call dispatched: only an admitted
    /// attempt can have been billed.
    admitted_attempts: std::collections::BTreeSet<u32>,
    recorded: bool,
}

/// A dispatch site asked a run for a call it cannot own.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UsageRunError {
    /// One run is attributed to one owner; a call under another owner would
    /// split one effect's spend across two ledgers.
    #[error("usage run for {run_owner} cannot account a call owned by {call_owner}")]
    OwnerMismatch {
        run_owner: RuntimeOwner,
        call_owner: RuntimeOwner,
    },
}

impl UsageRun {
    pub fn begin(
        effect: UsageEffectKey,
        execution_scope_key: String,
        store: Arc<dyn UsageAccountingStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(UsageRunInner {
                effect,
                execution_scope_key,
                run: UsageRunId::mint(),
                store,
                clock,
                admission: tokio::sync::Mutex::new(RunAdmission::Pending),
                progress: Mutex::new(RunProgress::default()),
            }),
        }
    }

    pub fn effect(&self) -> &UsageEffectKey {
        &self.inner.effect
    }

    pub fn run_id(&self) -> &UsageRunId {
        &self.inner.run
    }

    /// A call slot: its `call_ordinal` is the next one, and `owner` must
    /// equal every other call's owner in this run.
    pub fn call(
        &self,
        owner: RuntimeOwner,
        source: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<UsageCall, UsageRunError> {
        let mut progress = self.inner.progress.lock_recover();
        match &progress.owner {
            Some(run_owner) if *run_owner != owner => {
                return Err(UsageRunError::OwnerMismatch {
                    run_owner: run_owner.clone(),
                    call_owner: owner,
                });
            }
            Some(_) => {}
            None => progress.owner = Some(owner.clone()),
        }
        let call_ordinal = progress.next_call_ordinal;
        progress.next_call_ordinal = call_ordinal.saturating_add(1);
        progress.calls.insert(call_ordinal, CallProgress::default());
        Ok(UsageCall {
            inner: Arc::new(UsageCallInner {
                run: self.clone(),
                owner,
                source: source.into(),
                model: model.into(),
                call_ordinal,
            }),
        })
    }

    /// A store fault met by admission. The controller ends the attempt
    /// retryably and journals nothing: a fault is never a recorded outcome.
    pub fn admission_fault(&self) -> Option<String> {
        self.inner.progress.lock_recover().admission_fault.clone()
    }

    /// What the recorded entry carries beside the outcome. `None` iff no call
    /// was ever admitted, so nothing was dispatched.
    pub fn finish(self) -> Option<EffectUsage> {
        let progress = self.inner.progress.lock_recover();
        if !progress.admitted {
            return None;
        }
        let owner = progress.owner.clone()?;
        let calls_without_record = progress
            .calls
            .values()
            .filter(|call| !call.admitted_attempts.is_empty() && !call.recorded)
            .count();
        let mut facts = progress.facts.clone();
        facts.sort_by_key(|fact| (fact.call_ordinal, fact.provider_attempt));
        Some(EffectUsage {
            owner,
            run: self.inner.run.clone(),
            facts,
            accounting: if calls_without_record == 0 {
                RunAccounting::Complete
            } else {
                RunAccounting::CallWithoutRecord {
                    calls: u32::try_from(calls_without_record).unwrap_or(u32::MAX),
                }
            },
        })
    }

    async fn admit(&self, call: &UsageCallInner) -> Result<(), DispatchRefused> {
        let mut admission = self.inner.admission.lock().await;
        match &*admission {
            RunAdmission::Admitted => return Ok(()),
            RunAdmission::Refused(refused) | RunAdmission::Faulted(refused) => {
                return Err(refused.clone());
            }
            RunAdmission::Pending => {}
        }
        let request = UsageRunAdmission {
            owner: call.owner.clone(),
            effect: self.inner.effect.clone(),
            execution_scope_key: self.inner.execution_scope_key.clone(),
            run: self.inner.run.clone(),
            source: call.source.clone(),
            model: call.model.clone(),
            admitted_at_ms: self.inner.clock.timestamp_ms(),
        };
        let verdict = match self.inner.store.admit_usage_run(&request).await {
            Ok(_) => {
                self.inner.progress.lock_recover().admitted = true;
                RunAdmission::Admitted
            }
            Err(UsageAdmissionError::OwnerRetired {
                owner,
                retired_at_ms,
            }) => RunAdmission::Refused(DispatchRefused {
                code: crate::TurnFailureCode::UsageOwnerRetired,
                message: format!(
                    "usage owner {owner} was retired at {retired_at_ms} ms; no provider \
                     attempt may spend under it"
                ),
                retryable: false,
            }),
            Err(UsageAdmissionError::Store(error)) => {
                let message = format!("usage run admission failed: {error}");
                self.inner.progress.lock_recover().admission_fault = Some(message.clone());
                RunAdmission::Faulted(DispatchRefused {
                    code: crate::TurnFailureCode::from_wire(
                        crate::RuntimeErrorCode::UsageAdmissionFault.as_str(),
                    ),
                    message,
                    retryable: true,
                })
            }
        };
        *admission = verdict.clone();
        match verdict {
            RunAdmission::Admitted => Ok(()),
            RunAdmission::Refused(refused) | RunAdmission::Faulted(refused) => Err(refused),
            RunAdmission::Pending => unreachable!("an admission verdict is never pending"),
        }
    }
}

/// One provider call of a run: the dispatch gate its attempts pass, and the
/// seal of its record.
///
/// Clones share one call, so a provider task can hold the gate while the
/// body that spawned it keeps the call to seal.
#[derive(Clone)]
pub struct UsageCall {
    inner: Arc<UsageCallInner>,
}

struct UsageCallInner {
    run: UsageRun,
    owner: RuntimeOwner,
    source: String,
    model: String,
    call_ordinal: u32,
}

#[async_trait::async_trait]
impl DispatchAdmission for UsageCall {
    async fn admit_dispatch(&self, dispatch: &ProviderDispatch<'_>) -> Result<(), DispatchRefused> {
        self.inner.run.admit(&self.inner).await?;
        let mut progress = self.inner.run.inner.progress.lock_recover();
        progress
            .calls
            .entry(self.inner.call_ordinal)
            .or_default()
            .admitted_attempts
            .insert(dispatch.attempt_ordinal);
        Ok(())
    }
}

impl UsageCall {
    pub fn owner(&self) -> &RuntimeOwner {
        &self.inner.owner
    }

    /// Seal the call: one fact per admitted attempt whose usage disposition
    /// is `Reported`, `UnreportedAfterAbort` or `UnreportedAfterFailure`. An
    /// attempt the provider completed without reporting usage records no
    /// fact (ADR 0031). An admitted attempt the record does not describe —
    /// a call cut off before its provider task returned — was dispatched and
    /// may be billed, so it is an unreported fact.
    pub fn record(self, record: &LlmCallRecord) {
        let call = &self.inner;
        let mut progress = call.run.inner.progress.lock_recover();
        let admitted = {
            let slot = progress.calls.entry(call.call_ordinal).or_default();
            if slot.recorded {
                return;
            }
            slot.recorded = true;
            slot.admitted_attempts.clone()
        };
        let mut described = std::collections::BTreeSet::new();
        let mut facts = Vec::new();
        for attempt in &record.attempts {
            if !admitted.contains(&attempt.ordinal) || !described.insert(attempt.ordinal) {
                continue;
            }
            let generation_id = attempt
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.provider_response_id.clone());
            let outcome = match (attempt.usage_disposition, attempt.usage.as_ref()) {
                (AttemptUsageOutcome::Reported, Some(usage)) => AttemptFactOutcome::Reported {
                    usage: crate::runtime::effect::token_usage_from_llm(usage),
                    generation_id,
                },
                (AttemptUsageOutcome::Reported, None)
                | (AttemptUsageOutcome::UnreportedByProvider, _) => continue,
                (
                    AttemptUsageOutcome::UnreportedAfterAbort
                    | AttemptUsageOutcome::UnreportedAfterFailure,
                    _,
                ) => AttemptFactOutcome::Unreported { generation_id },
            };
            facts.push(self.fact(record, attempt.ordinal, outcome));
        }
        for ordinal in admitted.difference(&described) {
            facts.push(self.fact(
                record,
                *ordinal,
                AttemptFactOutcome::Unreported {
                    generation_id: None,
                },
            ));
        }
        progress.facts.extend(facts);
    }

    fn fact(
        &self,
        record: &LlmCallRecord,
        attempt_ordinal: u32,
        outcome: AttemptFactOutcome,
    ) -> UsageAttemptFact {
        UsageAttemptFact {
            call_ordinal: self.inner.call_ordinal,
            provider_attempt: attempt_ordinal,
            llm_call_id: record.call_id.clone(),
            source: self.inner.source.clone(),
            model: self.inner.model.clone(),
            outcome,
        }
    }
}

/// What a recorded spending effect's journal entry carries beside its
/// outcome: the run's owner and identity and the facts it sealed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectUsage {
    pub owner: RuntimeOwner,
    pub run: UsageRunId,
    pub facts: Vec<UsageAttemptFact>,
    pub accounting: RunAccounting,
}

impl EffectUsage {
    /// The settlement the engine delivers for `effect`.
    pub fn settlement(&self, effect: &UsageEffectKey) -> UsageSettlement {
        UsageSettlement {
            owner: self.owner.clone(),
            effect: effect.clone(),
            run: self.run.clone(),
            facts: self.facts.clone(),
            accounting: self.accounting.clone(),
        }
    }

    /// The stamp without the facts, for a record whose facts cannot be
    /// journaled: the run resolves `unknown(facts_unjournalable)` instead of
    /// staying open or losing its identity.
    pub fn without_facts(self) -> Self {
        let dropped_facts = u32::try_from(self.facts.len()).unwrap_or(u32::MAX);
        Self {
            owner: self.owner,
            run: self.run,
            facts: Vec::new(),
            accounting: RunAccounting::FactsUnjournalable { dropped_facts },
        }
    }
}

/// How a settlement landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Projected {
    Settled(UsageSettleReceipt),
    /// The settlement disagreed with stored facts. The runs are marked
    /// `conflicted`; delivery is complete and is never retried.
    Conflicted(Box<UsageFactConflict>),
}

/// The one projector: the only production caller of
/// [`UsageAccountingStore::settle_usage`].
///
/// A conflict is marked on the runs and answered `Ok`, so a continuation
/// never retries a conflict forever. A store fault is `Err`: the caller
/// retries the identical settlement, which is idempotent.
pub async fn project_usage_settlement(
    store: &dyn UsageAccountingStore,
    settlement: &UsageSettlement,
    now_ms: u64,
) -> Result<Projected, StoreError> {
    match store.settle_usage(settlement, now_ms).await {
        Ok(receipt) => Ok(Projected::Settled(receipt)),
        Err(UsageAppendError::Conflict(conflict)) => {
            store
                .mark_usage_settlement_conflicted(settlement, &conflict, now_ms)
                .await?;
            Ok(Projected::Conflicted(conflict))
        }
        Err(UsageAppendError::Store(error)) => Err(error),
        Err(
            error @ (UsageAppendError::CorrectionTargetMissing { .. }
            | UsageAppendError::CorrectionTargetReported { .. }),
        ) => Err(StoreError::Backend(format!(
            "a usage settlement answered a correction refusal: {error}"
        ))),
    }
}

/// Where a spending body's usage is admitted and settled: the ledger store
/// and the clock that stamps it. A local runner whose body may dispatch a
/// provider call offers it, and the executor begins the body's run with it.
#[derive(Clone)]
pub struct UsageAccountingBinding {
    pub store: Arc<dyn UsageAccountingStore>,
    pub clock: Arc<dyn Clock>,
}

impl UsageAccountingBinding {
    pub fn new(store: Arc<dyn UsageAccountingStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// The run of `envelope`'s body, when the envelope is a spending effect:
    /// `LlmCall`, `Direct` or `ToolAttempt`.
    pub fn begin(&self, envelope: &crate::RuntimeEffectEnvelope) -> Option<UsageRun> {
        if !is_spending_effect(envelope.command.kind()) {
            return None;
        }
        let address = envelope.invocation.address();
        let execution_scope_key = address
            .execution_scope
            .journal_identity()
            .ok()?
            .key()
            .to_string();
        Some(UsageRun::begin(
            UsageEffectKey::for_effect(address),
            execution_scope_key,
            Arc::clone(&self.store),
            Arc::clone(&self.clock),
        ))
    }
}

/// Whether an effect of `kind` is a spending effect: its body may dispatch a
/// provider call, so its execution is a usage run.
pub fn is_spending_effect(kind: crate::RuntimeEffectKind) -> bool {
    matches!(
        kind,
        crate::RuntimeEffectKind::LlmCall
            | crate::RuntimeEffectKind::Direct
            | crate::RuntimeEffectKind::ToolAttempt
    )
}

/// One execution of an effect body, with what an engine journals beside its
/// outcome.
pub struct RecordedEffectExecution {
    pub outcome: Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError>,
    /// The spending body's usage; `None` when nothing was dispatched.
    pub usage: Option<EffectUsage>,
    /// A store fault met by the run's admission: the engine ends the attempt
    /// retryably and journals nothing.
    pub admission_fault: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AttemptOutcome, AttemptRecord, LlmCallId, OwnerUsage, TokenUsage, UsageAppendReceipt,
        UsageCorrection, UsageFactCursor, UsageFactPage, UsageOwnerRetired, UsageRunAdmitted,
        UsageRunCursor, UsageRunFilter, UsageRunPage,
    };
    use std::num::NonZeroU32;

    /// A store that answers admissions and nothing else: the projection under
    /// test never reaches the other operations.
    #[derive(Default)]
    struct AdmissionStore {
        admissions: Mutex<Vec<UsageRunAdmission>>,
        retired: bool,
        faulted: bool,
    }

    #[async_trait::async_trait]
    impl UsageAccountingStore for AdmissionStore {
        async fn admit_usage_run(
            &self,
            admission: &UsageRunAdmission,
        ) -> Result<UsageRunAdmitted, UsageAdmissionError> {
            if self.faulted {
                return Err(UsageAdmissionError::Store(StoreError::Backend(
                    "admission store is down".to_string(),
                )));
            }
            if self.retired {
                return Err(UsageAdmissionError::OwnerRetired {
                    owner: admission.owner.clone(),
                    retired_at_ms: 7,
                });
            }
            self.admissions.lock_recover().push(admission.clone());
            Ok(UsageRunAdmitted::Admitted)
        }
        async fn settle_usage(
            &self,
            _settlement: &UsageSettlement,
            _now_ms: u64,
        ) -> Result<UsageSettleReceipt, UsageAppendError> {
            unreachable!("the projection tests never settle")
        }
        async fn mark_usage_settlement_conflicted(
            &self,
            _settlement: &UsageSettlement,
            _conflict: &UsageFactConflict,
            _now_ms: u64,
        ) -> Result<(), StoreError> {
            unreachable!("the projection tests never settle")
        }
        async fn append_usage_corrections(
            &self,
            _owner: &RuntimeOwner,
            _corrections: &[UsageCorrection],
            _now_ms: u64,
        ) -> Result<UsageAppendReceipt, UsageAppendError> {
            unreachable!("the projection tests never correct")
        }
        async fn retire_usage_execution(
            &self,
            _owner: &RuntimeOwner,
            _execution_scope_key: &str,
            _now_ms: u64,
        ) -> Result<u64, StoreError> {
            unreachable!("the projection tests never retire")
        }
        async fn retire_usage_owner(
            &self,
            _owner: &RuntimeOwner,
            _now_ms: u64,
        ) -> Result<UsageOwnerRetired, StoreError> {
            unreachable!("the projection tests never retire")
        }
        async fn load_owner_usage(&self, _owner: &RuntimeOwner) -> Result<OwnerUsage, StoreError> {
            unreachable!("the projection tests never read")
        }
        async fn load_usage_fact_page(
            &self,
            _owner: &RuntimeOwner,
            _after: Option<&UsageFactCursor>,
            _limit: NonZeroU32,
        ) -> Result<UsageFactPage, StoreError> {
            unreachable!("the projection tests never read")
        }
        async fn load_usage_run_page(
            &self,
            _owner: &RuntimeOwner,
            _filter: UsageRunFilter,
            _after: Option<&UsageRunCursor>,
            _limit: NonZeroU32,
        ) -> Result<UsageRunPage, StoreError> {
            unreachable!("the projection tests never read")
        }
    }

    fn owner() -> RuntimeOwner {
        RuntimeOwner::Session(crate::SessionId::from("usage-owner"))
    }

    fn run(store: Arc<AdmissionStore>) -> UsageRun {
        let address = crate::EffectAddress::new(
            crate::ExecutionScope::runtime_operation("usage-projection"),
            "effect".to_string(),
        )
        .expect("a valid effect address");
        UsageRun::begin(
            UsageEffectKey::for_effect(&address),
            "scope".to_string(),
            store,
            Arc::new(crate::SystemClock),
        )
    }

    fn attempt(
        ordinal: u32,
        outcome: AttemptOutcome,
        usage_disposition: AttemptUsageOutcome,
        input_tokens: Option<i64>,
    ) -> AttemptRecord {
        AttemptRecord {
            ordinal,
            outcome,
            protocol_position: crate::ProtocolPosition::ResponseObserved,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: input_tokens.map(|input_tokens| crate::llm::types::LlmUsage {
                input_tokens,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
            usage_disposition,
        }
    }

    fn record(attempts: Vec<AttemptRecord>) -> LlmCallRecord {
        LlmCallRecord {
            call_id: LlmCallId("call-a".to_string()),
            label: None,
            replay_drops: Vec::new(),
            attempts,
        }
    }

    async fn dispatch(call: &UsageCall, ordinal: u32) -> Result<(), DispatchRefused> {
        call.admit_dispatch(&ProviderDispatch {
            call_id: &LlmCallId("call-a".to_string()),
            attempt_ordinal: ordinal,
            model: "model",
        })
        .await
    }

    fn reported(input_tokens: i64) -> AttemptFactOutcome {
        AttemptFactOutcome::Reported {
            usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
            generation_id: None,
        }
    }

    /// A billed failed attempt and the retry that succeeded are two facts,
    /// and the run is admitted once for both.
    #[tokio::test]
    async fn a_billed_failure_and_its_retry_are_two_facts_under_one_admission() {
        let store = Arc::new(AdmissionStore::default());
        let run = run(Arc::clone(&store));
        let call = run.call(owner(), "turn", "model").expect("a call slot");
        dispatch(&call, 1)
            .await
            .expect("the first attempt is admitted");
        dispatch(&call, 2).await.expect("the retry is admitted");
        call.record(&record(vec![
            attempt(
                1,
                AttemptOutcome::Failed,
                AttemptUsageOutcome::Reported,
                Some(10),
            ),
            attempt(
                2,
                AttemptOutcome::Completed,
                AttemptUsageOutcome::Reported,
                Some(41),
            ),
        ]));
        let usage = run.finish().expect("an admitted run has usage");
        assert_eq!(store.admissions.lock_recover().len(), 1);
        assert_eq!(usage.accounting, RunAccounting::Complete);
        assert_eq!(
            usage
                .facts
                .iter()
                .map(|fact| (fact.provider_attempt, fact.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![(1, reported(10)), (2, reported(41))]
        );
    }

    /// ADR 0031 and 0032 in one record: an explicit zero is a fact, an abort
    /// is an unreported fact, and an attempt the provider never reported
    /// records nothing.
    #[tokio::test]
    async fn zero_is_a_fact_an_abort_is_unreported_and_unreported_by_provider_is_nothing() {
        let run = run(Arc::new(AdmissionStore::default()));
        let call = run.call(owner(), "turn", "model").expect("a call slot");
        for ordinal in 1..=3 {
            dispatch(&call, ordinal).await.expect("admitted");
        }
        call.record(&record(vec![
            attempt(
                1,
                AttemptOutcome::Completed,
                AttemptUsageOutcome::Reported,
                Some(0),
            ),
            attempt(
                2,
                AttemptOutcome::Aborted,
                AttemptUsageOutcome::UnreportedAfterAbort,
                None,
            ),
            attempt(
                3,
                AttemptOutcome::Completed,
                AttemptUsageOutcome::UnreportedByProvider,
                None,
            ),
        ]));
        let usage = run.finish().expect("an admitted run has usage");
        assert_eq!(
            usage
                .facts
                .iter()
                .map(|fact| (fact.provider_attempt, fact.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, reported(0)),
                (
                    2,
                    AttemptFactOutcome::Unreported {
                        generation_id: None
                    }
                ),
            ]
        );
    }

    /// An admitted attempt the record does not describe was dispatched and
    /// may be billed, so it is an unreported fact; a call admitted and never
    /// sealed makes the run's accounting incomplete.
    #[tokio::test]
    async fn an_undescribed_attempt_is_unreported_and_an_unsealed_call_is_counted() {
        let run = run(Arc::new(AdmissionStore::default()));
        let sealed = run.call(owner(), "turn", "model").expect("a call slot");
        dispatch(&sealed, 1).await.expect("admitted");
        dispatch(&sealed, 2).await.expect("admitted");
        sealed.record(&record(vec![attempt(
            1,
            AttemptOutcome::Completed,
            AttemptUsageOutcome::Reported,
            Some(5),
        )]));
        let unsealed = run
            .call(owner(), "turn", "model")
            .expect("a second call slot");
        dispatch(&unsealed, 1).await.expect("admitted");
        drop(unsealed);
        let usage = run.finish().expect("an admitted run has usage");
        assert_eq!(usage.facts.len(), 2);
        assert_eq!(
            usage.facts[1].outcome,
            AttemptFactOutcome::Unreported {
                generation_id: None
            }
        );
        assert_eq!(
            usage.accounting,
            RunAccounting::CallWithoutRecord { calls: 1 }
        );
    }

    /// A run that dispatched nothing has no usage, and a second owner cannot
    /// join a run.
    #[tokio::test]
    async fn a_run_that_dispatched_nothing_has_no_usage_and_one_owner() {
        let run = run(Arc::new(AdmissionStore::default()));
        let _call = run.call(owner(), "turn", "model").expect("a call slot");
        assert!(matches!(
            run.call(
                RuntimeOwner::Session(crate::SessionId::from("someone-else")),
                "turn",
                "model",
            ),
            Err(UsageRunError::OwnerMismatch { .. })
        ));
        assert!(run.finish().is_none());
    }

    /// A retired owner refuses the dispatch, not retryably; a store fault
    /// refuses it retryably and is the run's admission fault.
    #[tokio::test]
    async fn a_retired_owner_refuses_and_a_store_fault_is_an_admission_fault() {
        let retired = run(Arc::new(AdmissionStore {
            retired: true,
            ..AdmissionStore::default()
        }));
        let call = retired.call(owner(), "turn", "model").expect("a call slot");
        let refused = dispatch(&call, 1)
            .await
            .expect_err("a retired owner refuses");
        assert_eq!(refused.code, crate::TurnFailureCode::UsageOwnerRetired);
        assert!(!refused.retryable);
        assert!(retired.admission_fault().is_none());
        assert!(retired.finish().is_none());

        let faulted = run(Arc::new(AdmissionStore {
            faulted: true,
            ..AdmissionStore::default()
        }));
        let call = faulted.call(owner(), "turn", "model").expect("a call slot");
        let refused = dispatch(&call, 1).await.expect_err("a store fault refuses");
        assert!(refused.retryable);
        assert!(faulted.admission_fault().is_some());
    }
}
