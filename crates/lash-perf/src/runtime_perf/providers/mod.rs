use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmOutputSpec, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvent, LlmUsage,
};
use lash_core::testing::TestProvider;
use lash_core::{
    Resolution, ToolAttemptOutcome, ToolContract, ToolDefinition, ToolManifest, ToolOutcome,
    ToolOutcomeDone, ToolOutputContract, ToolProvider, TriggerOccurrenceRequest,
    facade_support::DirectJsonSchema, facade_support::DirectRequest,
    facade_support::empty_trigger_source_key,
};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use lash_sansio::sync::MutexExt;

use super::scenarios::RuntimePerfScenario;

const OPENAI_COMPAT_STREAM_CHUNK_COUNT: usize = 256;
const OPENAI_COMPAT_STREAM_CHUNK_BYTES: usize = 96;

pub(crate) struct BenchmarkStreamProfile {
    pub(crate) full_text: String,
    pub(crate) deltas: Vec<String>,
    pub(crate) parts: Vec<LlmOutputPart>,
}

pub(crate) struct BenchmarkProviderControl {
    pub(crate) provider_started: tokio::sync::Notify,
    pub(crate) release_provider: tokio::sync::Notify,
    armed: AtomicBool,
}

impl BenchmarkProviderControl {
    fn new() -> Self {
        Self {
            provider_started: tokio::sync::Notify::new(),
            release_provider: tokio::sync::Notify::new(),
            armed: AtomicBool::new(false),
        }
    }

    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn take_armed(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }
}

pub(crate) struct BenchmarkSettlementControl {
    pending: AtomicUsize,
    pending_changed: tokio::sync::Notify,
    releases: tokio::sync::Semaphore,
    pending_durations_ms: Mutex<Vec<f64>>,
}

impl BenchmarkSettlementControl {
    pub(crate) fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            pending_changed: tokio::sync::Notify::new(),
            releases: tokio::sync::Semaphore::new(0),
            pending_durations_ms: Mutex::new(Vec::new()),
        }
    }

    async fn hold_completion(&self) -> f64 {
        let started = Instant::now();
        self.pending.fetch_add(1, Ordering::SeqCst);
        self.pending_changed.notify_waiters();
        self.releases
            .acquire()
            .await
            .expect("settlement release semaphore remains open")
            .forget();
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.pending_durations_ms
            .lock()
            .expect("settlement duration lock")
            .push(duration_ms);
        duration_ms
    }

    pub(crate) async fn wait_for_pending(&self, expected: usize) {
        loop {
            let changed = self.pending_changed.notified();
            if self.pending.load(Ordering::SeqCst) >= expected {
                return;
            }
            changed.await;
        }
    }

    pub(crate) fn release(&self, count: usize) {
        self.releases.add_permits(count);
    }

    pub(crate) fn pending_durations_ms(&self) -> Vec<f64> {
        self.pending_durations_ms
            .lock()
            .expect("settlement duration lock")
            .clone()
    }
}

pub(crate) fn benchmark_provider(scenario: RuntimePerfScenario) -> TestProvider {
    benchmark_provider_with_control(scenario).0
}

pub(crate) fn benchmark_provider_with_control(
    scenario: RuntimePerfScenario,
) -> (TestProvider, Option<Arc<BenchmarkProviderControl>>) {
    let control = matches!(
        scenario,
        RuntimePerfScenario::TurnCancelRoundTrip | RuntimePerfScenario::IngressClaimProjection
    )
    .then(|| Arc::new(BenchmarkProviderControl::new()))
    .or_else(|| {
        scenario
            .contention_workers()
            .map(|_| Arc::new(BenchmarkProviderControl::new()))
    });
    let completion_control = control.clone();
    let provider = TestProvider::builder()
        .kind("benchmark")
        .serialize_config(move || {
            serde_json::json!({
                "scenario": scenario.name(),
            })
        })
        .requires_streaming(true)
        .complete(move |req| {
            let completion_control = completion_control.clone();
            async move {
                if matches!(scenario, RuntimePerfScenario::TurnCancelRoundTrip) {
                    completion_control
                        .as_ref()
                        .expect("cancel round-trip control")
                        .provider_started
                        .notify_one();
                    return std::future::pending().await;
                }
                if matches!(scenario, RuntimePerfScenario::IngressClaimProjection)
                    && !latest_request_item_contains(&req, "ingress projection marker")
                {
                    let control = completion_control
                        .as_ref()
                        .expect("ingress projection control");
                    control.provider_started.notify_one();
                    control.release_provider.notified().await;
                }
                if scenario.contention_workers().is_some()
                    && completion_control
                        .as_ref()
                        .expect("writer contention provider control")
                        .take_armed()
                {
                    let control = completion_control
                        .as_ref()
                        .expect("writer contention provider control");
                    control.provider_started.notify_one();
                    control.release_provider.notified().await;
                }
                let profile = benchmark_stream_profile_for_request(scenario, &req);
                let usage = LlmUsage {
                    input_tokens: 1_024,
                    output_tokens: 64,
                    cache_read_input_tokens: 512,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 48,
                };
                if let Some(tx) = req.stream_events.as_ref() {
                    if profile.deltas.is_empty() {
                        for part in &profile.parts {
                            tx.send(LlmStreamEvent::Part(part.clone()));
                        }
                    } else {
                        for delta in &profile.deltas {
                            tx.send(LlmStreamEvent::Delta(delta.clone()));
                        }
                    }
                    tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
                let parts = if profile.parts.is_empty() {
                    vec![LlmOutputPart::Text {
                        text: profile.full_text.clone(),
                        response_meta: None,
                    }]
                } else {
                    profile.parts
                };
                Ok(LlmResponse {
                    parts,
                    usage,
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    terminal_diagnostic: None,
                    provider_usage: None,
                    request_body: None,
                    http_summary: None,
                    execution_evidence: None,
                    generation_disposition: None,
                    response_metadata: Default::default(),
                })
            }
        })
        .build();
    (provider, control)
}

#[derive(Clone)]
pub(crate) struct BenchmarkEchoTool {
    completion_resolver: Arc<dyn lash_core::EffectHost>,
    settlement_control: Option<Arc<BenchmarkSettlementControl>>,
    completion_witness: Option<Arc<super::smoke::CompletionWitness>>,
}

mod profiles;
mod tools;

#[cfg(test)]
mod tests;

pub(crate) use profiles::benchmark_stream_profile;
use profiles::{benchmark_stream_profile_for_request, latest_request_item_contains};
pub(crate) use tools::{
    BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE, BenchmarkLargeToolCatalog, BenchmarkObliqueTools,
    BenchmarkToolCatalogObservation, BenchmarkToolCatalogObserver, BenchmarkWorkbenchMailTool,
};
