//! Every registered first-party tool succeeds from a production RLM cell on
//! the durable engine (ported by FIG-5308 from the deleted engine crate's
//! `tool_context_conformance.rs`).
//!
//! Each tool the first-party registry ships needs a conformance fixture: its
//! arguments and the TypeScript a model writes to call it. A cell calling it
//! answers the tool's result, and the tool body and its direct completion
//! each run once.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::types::{LlmRequest, LlmRole};
use lash_core::{ToolCall, ToolProvider};

use served::{Tier, World};

/// The `llm_query` direct completion's answer, as the provider returns it.
const LLM_QUERY_ANSWER: &str = r#"{"kind":"value","value":{"answer":"covered"},"error":null}"#;

fn args_for(tool_name: &str) -> serde_json::Value {
    match tool_name {
        "llm_query" => serde_json::json!({
            "task": "Return the covered answer",
            "inputs": {"answer": "covered"},
            "output": {"answer": "str"}
        }),
        other => panic!(
            "first-party tool `{other}` was registered without a conformance fixture; add its arguments before merging"
        ),
    }
}

fn typescript_source_for(tool_name: &str) -> &'static str {
    match tool_name {
        "llm_query" => {
            r#"const result = await llm.query({
  task: "Return the covered answer",
  inputs: { answer: "covered" },
  output: { answer: "str" }
});
finish(result);"#
        }
        other => panic!(
            "first-party tool `{other}` was registered without a production TypeScript fixture; add its caller path before merging"
        ),
    }
}

/// Counts the first-party provider's executions.
struct CountingFirstPartyProvider {
    inner: Arc<dyn ToolProvider>,
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for CountingFirstPartyProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.inner.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(call).await
    }
}

/// The model: a turn's request (it holds `input`) is answered with the
/// tool's cell, then, once the cell ran, in prose; any other request is the
/// tool's direct completion, answered with [`LLM_QUERY_ANSWER`] and counted.
fn model(
    input: String,
    cell: String,
    direct_calls: Arc<AtomicUsize>,
) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("first-party-conformance")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let input = input.clone();
            let cell = cell.clone();
            let direct_calls = Arc::clone(&direct_calls);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                if !rendered.contains(&input) {
                    direct_calls.fetch_add(1, Ordering::SeqCst);
                    return Ok(served::text(&request, LLM_QUERY_ANSWER));
                }
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                Ok(if answered {
                    served::text(&request, "done")
                } else {
                    served::cell(&cell)
                })
            }
        })
        .build()
        .into_handle()
}

async fn every_registered_first_party_tool_succeeds_from_a_production_cell(tier: Tier) {
    let provider: Arc<dyn ToolProvider> = Arc::new(lash_llm_tools::llm_query_provider());
    let manifests = provider.tool_manifests();
    assert!(
        !manifests.is_empty(),
        "the first-party tool registry must not be empty"
    );
    for manifest in manifests {
        let _ = args_for(&manifest.name);
        let input = format!("first-party conformance: {}", manifest.name);
        let executions = Arc::new(AtomicUsize::new(0));
        let direct_calls = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(CountingFirstPartyProvider {
            inner: Arc::clone(&provider),
            executions: Arc::clone(&executions),
        });
        let Some(world) = World::with_model(
            tier,
            Vec::new(),
            model(
                input.clone(),
                typescript_source_for(&manifest.name).to_owned(),
                Arc::clone(&direct_calls),
            ),
            move |backend| {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(backend, None, sim::untimed_workers()),
                )
                .tools(tools)
            },
        )
        .await
        else {
            return;
        };
        let session = world.session("first-party", served::spec(64)).await;
        let output = tokio::time::timeout(
            served::WATCHDOG,
            session.send(lash::TurnInput::text(input)).output(),
        )
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("the turn answers");
        served::assert_answered(&manifest.name, &output);
        assert_eq!(
            output.final_value(),
            Some(&serde_json::json!({ "answer": "covered" })),
            "the production cell answers {}'s result",
            manifest.name
        );
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "{} runs once from the production cell",
            manifest.name
        );
        assert_eq!(
            direct_calls.load(Ordering::SeqCst),
            1,
            "{} issues its direct completion once",
            manifest.name
        );
        world.shutdown().await;
    }
}

tiered_laws!(every_registered_first_party_tool_succeeds_from_a_production_cell);
