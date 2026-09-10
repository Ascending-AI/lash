//! FIG-2765: an aborted cell call never looks free.
//!
//! The RLM protocol aborts the provider stream as soon as the cell boundary
//! is parsed. If the provider's usage frame does not land inside the abort
//! drain grace, the attempt is sealed with a typed unreported disposition and
//! the session ledger records a zero-usage hole for it. Hosts that route
//! through OpenRouter can later ask the runtime to reconcile those holes
//! against OpenRouter's generation endpoint; the recovered usage lands as
//! append-only correction rows that the totals sum.
//!
//! Every byte on the wire here is a recorded fixture: no live network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lash_http_transport::{
    ByteStream, HttpMethod, HttpRequest, HttpResponse, HttpResponseBody, HttpTransport,
    HttpTransportError,
};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

const GENERATION_ID: &str = "gen-fig2765";

/// One chat completion chunk carrying the whole cell, then silence: the
/// usage frame never arrives before the runtime aborts the stream.
fn cell_chunk() -> String {
    let payload = serde_json::json!({
        "id": GENERATION_ID,
        "model": "example/model",
        "choices": [{ "index": 0, "delta": { "content": "<lashlang>\nfinish \"reconciled\"\n</lashlang>\n" } }],
    });
    format!("data: {payload}\n\n")
}

/// The recorded `GET /generation` answer for that cancelled generation:
/// OpenRouter still billed it and still knows the native token counts.
fn recorded_generation() -> serde_json::Value {
    serde_json::json!({
        "data": {
            "id": GENERATION_ID,
            "model": "example/model",
            "streamed": true,
            "cancelled": true,
            "total_cost": 0.00042,
            "native_tokens_prompt": 310,
            "native_tokens_completion": 24,
            "native_tokens_reasoning": 0,
            "native_tokens_cached": 100
        }
    })
}

/// A body that yields the cell chunk once and then stays open forever, the
/// way a provider stream looks while the runtime is masking it.
#[derive(Debug)]
struct HangingCellBody {
    chunk: Option<Bytes>,
}

#[async_trait]
impl ByteStream for HangingCellBody {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, HttpTransportError> {
        if let Some(chunk) = self.chunk.take() {
            return Ok(Some(chunk));
        }
        std::future::pending::<()>().await;
        Ok(None)
    }
}

#[derive(Debug, Default)]
struct RecordedOpenRouter {
    lookups: Mutex<Vec<String>>,
}

#[async_trait]
impl HttpTransport for RecordedOpenRouter {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        match request.method {
            HttpMethod::Post => Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: HttpResponseBody::streamed(HangingCellBody {
                    chunk: Some(Bytes::from(cell_chunk())),
                }),
            }),
            HttpMethod::Get => {
                self.lookups
                    .lock()
                    .expect("lookup log")
                    .push(request.url.clone());
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: HttpResponseBody::buffered(recorded_generation().to_string()),
                })
            }
            other => Err(HttpTransportError::new(format!(
                "unexpected {other:?} in the recorded OpenRouter fixture"
            ))),
        }
    }
}

fn reconciling_core(transport: Arc<RecordedOpenRouter>) -> lash::Result<lash::LashCore> {
    let provider = lash::provider::ProviderHandle::new(
        OpenAiCompatibleProvider::new("recorded-key", OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_transport(transport)
            .into_components(),
    );
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .channel(lash::rlm::RlmChannel::Cell)
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .without_queued_work()
        .plugins(lash::plugins::runtime_plugin_stack())
        .provider(provider)
        .model(
            lash::ModelSpec::builder("example/model")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        // docs:start:abort-drain-grace
        // How long an aborted provider stream may keep draining for its usage
        // frame before the attempt is sealed as unreported (default 2 s).
        .abort_drain_grace(Duration::from_millis(50))
        // docs:end:abort-drain-grace
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())
}

#[tokio::test]
async fn aborted_cell_call_is_a_typed_hole_until_the_host_reconciles_it() -> anyhow::Result<()> {
    let transport = Arc::new(RecordedOpenRouter::default());
    let core = reconciling_core(transport.clone())?;
    let session = core.session("fig2765").open().await?;

    let output = session.turn(lash::TurnInput::text("finish")).run().await?;
    assert_eq!(output.final_value(), Some(&serde_json::json!("reconciled")));

    // docs:start:unreported-usage
    // The aborted attempt reported no usage; the turn's own counters are
    // zero, but the call is not free: the ledger carries a typed hole.
    let report = session.usage_report();
    assert_eq!(report.usage.total_tokens, 0);
    assert_eq!(report.usage.unreported_attempts, 1);
    let holes = session.unreported_usage_attempts().await;
    assert_eq!(holes.len(), 1);
    assert_eq!(holes[0].generation_id.as_deref(), Some("gen-fig2765"));

    // Ask the provider for the generation's final accounting and append
    // correction rows for whatever it can vouch for.
    let reconciliation = session.reconcile_unreported_usage().await?;
    assert_eq!(reconciliation.reconciled.len(), 1);
    assert!(reconciliation.unresolved.is_empty());
    let report = session.usage_report();
    assert_eq!(report.usage.unreported_attempts, 0);
    assert_eq!(report.usage.reconciled_attempts, 1);
    assert_eq!(report.usage.usage.input_tokens, 210);
    assert_eq!(report.usage.usage.cache_read_input_tokens, 100);
    assert_eq!(report.usage.usage.output_tokens, 24);
    // docs:end:unreported-usage

    let correction = &reconciliation.reconciled[0];
    assert_eq!(correction.attempt, holes[0]);
    assert_eq!(
        correction.provider_usage["cancelled"],
        serde_json::json!(true)
    );
    assert_eq!(
        correction.provider_usage["total_cost"],
        serde_json::json!(0.00042)
    );
    let correction_row = report
        .by_source_model
        .iter()
        .find(|row| row.usage.reconciled_attempts == 1)
        .expect("the correction is its own ledger row");
    assert_eq!(correction_row.source, "turn");
    assert_eq!(correction_row.usage.usage.output_tokens, 24);

    let lookups = transport.lookups.lock().expect("lookup log");
    assert_eq!(
        lookups.as_slice(),
        ["https://openrouter.ai/api/v1/generation?id=gen-fig2765"]
    );
    Ok(())
}
