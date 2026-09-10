//! Dev-only, token-free browser fixture for a Standard valid empty completion.

use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use lash::provider::ProviderHandle;
use serde::Serialize;

const SCRIPT: &str = include_str!(
    "../../../crates/lash-sim/provider-scripts/runtime/openai-compatible.chat-valid-empty-stop.json"
);

pub(crate) fn provider() -> Result<ProviderHandle, lash::provider::LlmTransportError> {
    let transport = Arc::new(lash_sim::ScriptedLlmHttpTransport::from_json_str(SCRIPT)?);
    let (provider, _, _) = lash_sim::runtime_providers::runtime_provider_components(
        lash_sim::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .map_err(|error| lash::provider::LlmTransportError::new(error.to_string()))?;
    Ok(provider)
}

fn fixture_transport()
-> Result<Arc<lash_sim::ScriptedLlmHttpTransport>, lash::provider::LlmTransportError> {
    Ok(Arc::new(lash_sim::ScriptedLlmHttpTransport::from_json_str(
        SCRIPT,
    )?))
}

#[derive(Serialize)]
pub(crate) struct ValidEmptyReport {
    status: &'static str,
    protocol: &'static str,
    provider: &'static str,
    assistant_bytes: usize,
    llm_calls: usize,
    provider_exchanges: usize,
    attempt_outcome: String,
    protocol_position: String,
    provider_finish_reason: String,
    input_tokens: i64,
    output_tokens: i64,
}

pub(crate) async fn run() -> Response {
    match run_fixture().await {
        Ok(report) => Json(report).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

async fn run_fixture() -> Result<ValidEmptyReport, String> {
    let transport = fixture_transport().map_err(|error| error.to_string())?;
    let (provider, model, _) = lash_sim::runtime_providers::runtime_provider_components(
        lash_sim::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .map_err(|error| error.to_string())?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::bounded(1))
        .without_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .provider(provider)
        .model(model)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "agent-workbench-valid-empty",
            uuid::Uuid::new_v4().to_string(),
        ))
        .map_err(|error| error.to_string())?;
    let session = core
        .session(format!("valid-empty-{}", uuid::Uuid::new_v4()))
        .open()
        .await
        .map_err(|error| error.to_string())?;
    let output = session
        .turn(lash::TurnInput::text(
            "Complete successfully without producing assistant content.",
        ))
        .run()
        .await
        .map_err(|error| error.to_string())?;

    let assistant = output
        .assistant_message()
        .ok_or_else(|| "Standard turn did not finish with an assistant message".to_string())?;
    let [call] = output.result.llm_calls.as_slice() else {
        return Err(format!(
            "expected exactly one LLM call, observed {}",
            output.result.llm_calls.len()
        ));
    };
    let [attempt] = call.attempts.as_slice() else {
        return Err(format!(
            "expected exactly one provider attempt, observed {}",
            call.attempts.len()
        ));
    };
    let finish_reason = attempt
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_finish_reason.as_deref())
        .ok_or_else(|| "provider finish evidence was absent".to_string())?;
    let usage = attempt
        .usage
        .as_ref()
        .ok_or_else(|| "provider usage was absent".to_string())?;
    let exchanges = transport.exchanges().map_err(|error| error.to_string())?;

    if !output.is_success()
        || !assistant.is_empty()
        || format!("{:?}", attempt.outcome) != "Completed"
        || format!("{:?}", attempt.protocol_position) != "TerminalObserved"
        || finish_reason != "stop"
        || usage.input_tokens != 7
        || usage.output_tokens != 0
        || exchanges.len() != 1
    {
        return Err("valid empty completion did not preserve its success, terminal, usage, and single-attempt invariants".to_string());
    }

    Ok(ValidEmptyReport {
        status: "valid empty completion finished successfully",
        protocol: "standard",
        provider: "openai-compatible Provider Wire Script",
        assistant_bytes: assistant.len(),
        llm_calls: output.result.llm_calls.len(),
        provider_exchanges: exchanges.len(),
        attempt_outcome: format!("{:?}", attempt.outcome),
        protocol_position: format!("{:?}", attempt.protocol_position),
        provider_finish_reason: finish_reason.to_string(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    })
}

pub(crate) async fn page() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Workbench valid empty completion</title>
  <style>
    :root { color-scheme: dark; font-family: ui-sans-serif, system-ui, sans-serif; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #101418; color: #edf3f7; }
    main { width: min(42rem, calc(100vw - 3rem)); padding: 2rem; border: 1px solid #33404a; border-radius: 1rem; background: #182027; }
    h1 { margin-top: 0; font-size: 1.6rem; }
    button { border: 0; border-radius: .6rem; padding: .75rem 1rem; background: #79d5b3; color: #092018; font-weight: 700; cursor: pointer; }
    dl { display: grid; grid-template-columns: 1fr 1fr; gap: .7rem 1rem; margin-top: 1.5rem; }
    dt { color: #9eb0bd; } dd { margin: 0; font-family: ui-monospace, monospace; }
    #status { margin: 1rem 0 0; font-weight: 700; }
  </style>
</head>
<body>
<main>
  <p>Agent Workbench · deterministic provider fixture</p>
  <h1>Valid empty completion</h1>
  <p>This token-free probe sends one Standard turn through the OpenAI-compatible adapter using a recorded Provider Wire Script.</p>
  <button id="run-fixture" type="button">Run valid empty completion</button>
  <p id="status">Ready</p>
  <dl id="evidence" hidden>
    <dt>Protocol</dt><dd id="protocol"></dd>
    <dt>Assistant bytes</dt><dd id="assistant-bytes"></dd>
    <dt>LLM calls</dt><dd id="llm-calls"></dd>
    <dt>Provider exchanges</dt><dd id="provider-exchanges"></dd>
    <dt>Attempt</dt><dd id="attempt-outcome"></dd>
    <dt>Protocol position</dt><dd id="protocol-position"></dd>
    <dt>Finish reason</dt><dd id="finish-reason"></dd>
    <dt>Usage</dt><dd id="usage"></dd>
  </dl>
</main>
<script>
const button = document.querySelector('#run-fixture');
button.addEventListener('click', async () => {
  button.disabled = true;
  document.querySelector('#status').textContent = 'Running…';
  try {
    const response = await fetch('/dev/valid-empty-completion', { method: 'POST' });
    const report = await response.json();
    if (!response.ok) throw new Error(report.error || `HTTP ${response.status}`);
    document.querySelector('#status').textContent = report.status;
    document.querySelector('#protocol').textContent = report.protocol;
    document.querySelector('#assistant-bytes').textContent = String(report.assistant_bytes);
    document.querySelector('#llm-calls').textContent = String(report.llm_calls);
    document.querySelector('#provider-exchanges').textContent = String(report.provider_exchanges);
    document.querySelector('#attempt-outcome').textContent = report.attempt_outcome;
    document.querySelector('#protocol-position').textContent = report.protocol_position;
    document.querySelector('#finish-reason').textContent = report.provider_finish_reason;
    document.querySelector('#usage').textContent = `${report.input_tokens} input / ${report.output_tokens} output`;
    document.querySelector('#evidence').hidden = false;
  } catch (error) {
    document.querySelector('#status').textContent = `Failed: ${error.message}`;
  } finally {
    button.disabled = false;
  }
});
</script>
</body>
</html>"#;
