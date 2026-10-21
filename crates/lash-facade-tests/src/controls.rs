use super::*;
use lash::TurnInput;
use lash_vocabulary::identity::SessionId;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The bounded session's recorded budget.
pub const BOUNDED_TURNS: usize = 2;
/// Tool results a turn gathers before the model answers: past every bound
/// here, so only an unbounded turn gets there.
pub const TOOL_CALLS_BEFORE_ANSWER: usize = 5;

/// Tool results in the request since the turn's input: the turn's
/// iterations so far.
pub fn tool_results_this_turn(request: &LlmRequest) -> usize {
    let mut results = 0;
    for message in request.messages.iter().rev() {
        let answered = message
            .blocks
            .iter()
            .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
            .count();
        if answered > 0 {
            results += answered;
            continue;
        }
        if message.role == LlmRole::User {
            break;
        }
    }
    results
}

/// A model that calls `app_lookup` until the turn holds
/// [`TOOL_CALLS_BEFORE_ANSWER`] results, then answers. `calls` counts the
/// calls the provider served: a replay is served from the journal and never
/// reaches it, so the count is the iterations the turns ran.
pub fn looping_provider(calls: &Arc<AtomicUsize>) -> ProviderHandle {
    let calls = Arc::clone(calls);
    lash_core::testing::TestProvider::builder()
        .kind("recorded-controls")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let done = tool_results_this_turn(&request);
                if done >= TOOL_CALLS_BEFORE_ANSWER {
                    return Ok(text_response("answered"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("lookup-{done}"),
                        tool_name: "tools__app_lookup".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

pub fn core_over(
    backend: lash_execution_contracts::backend::Backend,
    provider: ProviderHandle,
) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(super::runtime_lease_owner())
}

/// Create `id` with `budget` recorded as its turn budget.
pub async fn create_with_budget(core: &LashCore, id: &str, budget: lash::TurnBudget) -> Result<()> {
    core.session(SessionId::fixture(id.to_string()))
        .create(lash::SessionCreation::root(
            mock_session_spec().turn_budget(budget),
        ))
        .await?;
    Ok(())
}

/// Send `text` to `id` through its Durable Session: no host runtime is open,
/// so the engine opens the session itself to execute the run.
pub async fn engine_executed_turn(
    core: &LashCore,
    id: &str,
    text: &str,
) -> Result<lash::TurnOutput> {
    core.session(SessionId::fixture(id.to_string()))
        .durable()
        .await?
        .send(TurnInput::text(text))
        .output()
        .await
}

/// Apply `transaction` to `session`, written against `revision` under `id`.
pub async fn apply(
    session: &lash::LashSession,
    id: &str,
    revision: u64,
    transaction: lash::config::ConfigTransaction,
) -> Result<lash::config::ConfigTransactionOutcome> {
    session
        .admin()
        .config()
        .apply(lash::config::ConfigWrite::new(id, revision), transaction)
        .await?
        .await_outcome(&session.admin().config())
        .await
}
