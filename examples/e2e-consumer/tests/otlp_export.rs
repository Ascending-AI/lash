//! The consumer's OTLP export across a real socket outage, on a durable
//! SQLite core (ported by FIG-5308 from the deleted upgrade harness's R8
//! law). The host's SDK crosses an actual HTTP socket to a local receiver.

#[path = "support/otlp.rs"]
mod otlp;
#[allow(dead_code)]
#[path = "../src/telemetry.rs"]
mod telemetry;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use otlp::OtlpReceiver;
use telemetry::HostTelemetry;

const DEADLINE: Duration = Duration::from_secs(30);

fn provider() -> lash::provider::ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("otlp-socket-fixture")
        .complete(|_| async {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "socket fixture answer".into(),
                    response_meta: None,
                }],
                usage: lash::direct::LlmUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
        .build()
        .into_handle()
}

/// One answered turn on a fresh session `id`.
async fn send(core: &lash::LashCore, id: &str) -> Result<()> {
    let session = core
        .session(lash::SessionId::fixture(id))
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                "socket",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(16),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await?;
    let handle = session
        .send(lash::TurnInput::text("export this answer"))
        .into_future()
        .await?;
    let outcome = tokio::time::timeout(DEADLINE, handle.outcome()).await??;
    ensure!(
        matches!(outcome.status(), lash::TurnStatus::Answered),
        "telemetry failure changed the business terminal: {:?}",
        outcome.status()
    );
    ensure!(
        outcome
            .output()
            .and_then(|output| output.assistant_message())
            == Some("socket fixture answer"),
        "telemetry failure changed the answer"
    );
    Ok(())
}

/// A socket outage is counted as dropped spans, never acknowledged, and
/// never changes a turn's answer; after reconnect, shutdown drains the
/// acknowledged export, and the collector's spans match the host's
/// acknowledgement and the JSONL trace's provider attempts one for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_outage_is_counted_and_shutdown_drains_acknowledged_export() -> Result<()> {
    let receiver = OtlpReceiver::bind("127.0.0.1:0".parse()?).await?;
    let telemetry = HostTelemetry::new(&receiver.endpoint())?;
    let trace = tempfile::NamedTempFile::new()?;
    let stores = lash::sqlite::SqliteStoreSet::memory().await?;
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores)).build()?;
    let metadata = lash::LlmProfileMetadata::builder("otlp-fixture")
        .context_window_tokens(8192)
        .build()?;
    let registry = lash::LlmProfileRegistry::new().register(
        "socket",
        lash::RegisteredLlmProfile::new(metadata, provider()),
    )?;
    let core = telemetry
        .install(
            lash::LashCore::standard_builder(backend)
                .llm_profiles(Arc::new(registry))
                .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
                .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
                .trace_jsonl_path(trace.path()),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "otlp", "socket",
        ))?;

    send(&core, "otlp-connected").await?;
    let before = telemetry.flush();
    ensure!(
        before.dropped_spans == 0 && before.acknowledged_spans > 0 && before.flush_error.is_none(),
        "initial export was not acknowledged: {before:?}"
    );
    receiver
        .wait_for(DEADLINE, |receipt| {
            receipt.spans.len() as u64 == before.acknowledged_spans
        })
        .await?;

    receiver.disconnect();
    send(&core, "otlp-disconnected").await?;
    let outage = telemetry.flush();
    receiver
        .wait_for(DEADLINE, |receipt| receipt.disconnected_requests > 0)
        .await?;
    ensure!(
        outage.dropped_spans > 0,
        "outage silently lost telemetry: {outage:?}"
    );
    ensure!(
        outage.acknowledged_spans == before.acknowledged_spans,
        "disconnected socket was acknowledged"
    );

    receiver.reconnect();
    send(&core, "otlp-reconnected").await?;
    core.flush_trace_sink()?;
    core.shutdown().await?;
    drop(core);
    let final_export = telemetry.shutdown();
    let (collector, cleanup) = receiver.finish().await?;
    ensure!(cleanup.closed, "collector listener leaked");
    ensure!(
        final_export.flush_error.is_none() && final_export.shutdown_error.is_none(),
        "orderly host shutdown failed: {final_export:?}"
    );
    ensure!(
        final_export.acknowledged_spans > before.acknowledged_spans,
        "reconnected export never drained"
    );
    ensure!(
        final_export.attempted_spans
            == final_export.acknowledged_spans + final_export.dropped_spans,
        "unreported export loss"
    );
    ensure!(
        collector.spans.len() as u64 == final_export.acknowledged_spans,
        "shutdown acknowledgement disagrees with socket delivery"
    );

    let records: Vec<lash::tracing::TraceRecord> =
        lash::tracing::parse_jsonl_records(&std::fs::read_to_string(trace.path())?)?;
    for session in ["otlp-connected", "otlp-disconnected", "otlp-reconnected"] {
        let attempts: Vec<_> = records
            .iter()
            .filter(|record| {
                record
                    .context
                    .session_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == session)
                    && matches!(
                        record.event,
                        lash::tracing::TraceEvent::LlmAttemptCompleted { .. }
                    )
            })
            .collect();
        ensure!(
            attempts.len() == 1,
            "actual provider execution missing or repeated in JSONL for {session}"
        );
        let exported: Vec<_> = collector
            .spans
            .iter()
            .filter(|span| span.attribute("lash.record.id") == Some(attempts[0].id.as_str()))
            .collect();
        ensure!(
            exported.len() == usize::from(session != "otlp-disconnected"),
            "socket records disagree with outage evidence for {session}"
        );
        if let Some(span) = exported.first() {
            ensure!(
                span.integer("lash.model.attempt.ordinal") == Some(1),
                "actual attempt ordinal missing"
            );
            ensure!(
                span.integer("gen_ai.usage.input_tokens") == Some(7)
                    && span.integer("gen_ai.usage.output_tokens") == Some(3),
                "recorded provider usage changed during export"
            );
        }
    }
    ensure!(
        collector
            .spans
            .iter()
            .map(|span| (&span.trace_id, &span.span_id))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == collector.spans.len(),
        "socket delivery duplicated a span identity"
    );
    Ok(())
}
