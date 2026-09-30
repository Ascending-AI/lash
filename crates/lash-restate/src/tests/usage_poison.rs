//! A poisoned spending effect keeps what it spent (FIG-4236, ADR 0125):
//! the poison substitutes only the recorded outcome, so the run's usage
//! still rides the entry and its settlement is sent; and a run whose facts
//! cannot fit beside the poison either keeps its stamp, so its settlement
//! resolves it `unknown(facts_unjournalable)` rather than leaving it open.

use super::*;
use lash_core::core_internal::RuntimeEffectLocalRunner;

const SESSION: &str = "usage-poison-session";

/// A `Direct` body that makes one paid provider call under its usage run,
/// the way the session's direct completions do.
struct PaidCall {
    provider: lash_core::provider::ProviderHandle,
    accounting: lash_core::UsageAccountingBinding,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for PaidCall {
    fn usage_accounting(&self) -> Option<lash_core::UsageAccountingBinding> {
        Some(self.accounting.clone())
    }

    async fn execute(
        mut self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        usage_run: Option<lash_core::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let RuntimeEffectCommand::Direct {
            request,
            usage_source,
        } = envelope.command
        else {
            panic!("the paid-call body runs direct completions only");
        };
        let request = (*request).into_request(None, None);
        let call = usage_run
            .expect("a direct completion runs under its usage run")
            .call(
                lash_core::RuntimeOwner::Session(SessionId::from(SESSION)),
                usage_source,
                request.model.clone(),
            )
            .expect("the run's one call");
        let completion = self
            .provider
            .complete(request, &call)
            .await
            .expect("the scripted provider answers");
        call.record(&completion.call_record);
        Ok(RuntimeEffectOutcome::Direct {
            result: Box::new(Ok(completion.response)),
            call_record: Some(completion.call_record),
        })
    }
}

fn reported_usage() -> lash_core::llm::types::LlmUsage {
    lash_core::llm::types::LlmUsage {
        input_tokens: 40,
        output_tokens: 9,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 2,
    }
}

/// One paid call whose response outgrows the journal budget; the provider
/// names the generation `generation_id`.
async fn poisoned_paid_call(
    effect: &str,
    generation_id: String,
) -> (
    Arc<RecordingContext>,
    lash_core::RuntimeEffectControllerError,
    Arc<dyn lash_core::UsageAccountingStore>,
    usize,
) {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the ledger's SQLite memory store set");
    let usage = lash_core::StoreSet::usage_accounting(&stores);
    let invocations = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&invocations);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            let counted = Arc::clone(&counted);
            let generation_id = generation_id.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        // Far past the budget: the outcome cannot be journaled.
                        text: "x".repeat(64 * 1024),
                        response_meta: None,
                    }],
                    usage: reported_usage(),
                    provider_usage: Some(serde_json::json!({ "billed": true })),
                    execution_evidence: Some(lash_core::llm::types::ExecutionEvidence {
                        provider_response_id: Some(generation_id),
                        ..lash_core::llm::types::ExecutionEvidence::default()
                    }),
                    ..lash_core::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(&context),
        // Clears the envelope, the poison and a one-fact usage stamp; far
        // below the response.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    );
    let envelope = RuntimeEffectEnvelope::new(
        test_turn_effect_invocation(SESSION, "usage-poison-turn", 0, 0, effect, effect),
        RuntimeEffectCommand::Direct {
            request: Box::new(llm_spec()),
            usage_source: "turn".to_string(),
        },
    );
    let error = controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::owned_runner(
                Box::new(PaidCall {
                    provider,
                    accounting: lash_core::UsageAccountingBinding::new(
                        Arc::clone(&usage),
                        Arc::new(lash_core::facade_support::SystemClock),
                    ),
                }),
                None,
            ),
        )
        .await
        .expect_err("an unjournalable paid response gives up with the poison");
    (context, error, usage, invocations.load(Ordering::SeqCst))
}

/// Projects every settlement the controller sent, each twice: a replayed
/// send is the same settlement again.
async fn project_sent(
    context: &RecordingContext,
    usage: &dyn lash_core::UsageAccountingStore,
) -> Vec<lash_core::UsageSettlement> {
    let sent = context
        .usage_settlements
        .lock_recover()
        .iter()
        .map(|settle| settle.settlement.clone())
        .collect::<Vec<_>>();
    for settlement in sent.iter().chain(sent.iter()) {
        lash_core::project_usage_settlement(usage, settlement, 1)
            .await
            .expect("project the sent settlement");
    }
    sent
}

fn owner() -> lash_core::RuntimeOwner {
    lash_core::RuntimeOwner::Session(SessionId::from(SESSION))
}

/// E4: the poisoned entry keeps the run's facts, so the paid call is one
/// reported fact, settled, however often its settlement is delivered.
#[tokio::test]
async fn a_poisoned_model_call_keeps_its_usage() {
    let (context, error, usage, invocations) =
        poisoned_paid_call("usage-poison-kept", "gen-kept".to_string()).await;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EngineJournaledEffectPoisoned
    );
    assert_eq!(invocations, 1, "the provider was asked once");
    let sent = project_sent(&context, usage.as_ref()).await;
    assert_eq!(
        sent.len(),
        1,
        "the poisoned entry's settlement is sent once"
    );
    assert_eq!(sent[0].facts.len(), 1, "the poison kept the call's fact");
    assert_eq!(sent[0].accounting, lash_core::RunAccounting::Complete);
    let owner_usage = usage
        .load_owner_usage(&owner())
        .await
        .expect("read the owner's usage");
    assert!(owner_usage.completeness.is_settled());
    assert_eq!(owner_usage.completeness.unknown_runs, 0);
    assert_eq!(owner_usage.rows.len(), 1, "one (source, model) row");
    let row = &owner_usage.rows[0];
    assert_eq!(row.reported_attempts, 1, "the paid call counts once");
    assert_eq!(
        row.usage,
        lash_core::TokenUsage {
            input_tokens: 40,
            output_tokens: 9,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 2,
        }
    );
}

/// E4's companion: facts that cannot fit beside the poison are dropped, the
/// stamp is kept, and the run resolves `unknown(facts_unjournalable)`:
/// explicit, never an open run, never an invented fact.
#[tokio::test]
async fn poisoned_usage_over_budget_becomes_an_unknown_run() {
    let (context, error, usage, invocations) =
        // The generation id rides the fact, so the fact alone outgrows the
        // budget while the envelope and a fact-less stamp still fit.
        poisoned_paid_call("usage-poison-dropped", "g".repeat(8 * 1024)).await;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EngineJournaledEffectPoisoned
    );
    assert_eq!(invocations, 1, "the provider was asked once");
    let sent = project_sent(&context, usage.as_ref()).await;
    assert_eq!(sent.len(), 1, "the stamp's settlement is sent once");
    assert!(sent[0].facts.is_empty(), "the facts were dropped");
    assert_eq!(
        sent[0].accounting,
        lash_core::RunAccounting::FactsUnjournalable { dropped_facts: 1 }
    );
    let owner_usage = usage
        .load_owner_usage(&owner())
        .await
        .expect("read the owner's usage");
    assert_eq!(owner_usage.completeness.open_runs, 0, "nothing is open");
    assert_eq!(
        owner_usage.completeness.unknown_runs, 1,
        "the run is one explicit unknown"
    );
    assert!(owner_usage.rows.is_empty(), "no fact is invented");
}
