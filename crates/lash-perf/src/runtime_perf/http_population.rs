//! Independently scheduled, finite loopback-provider HTTP populations.
use super::{
    openai_compat::{HttpFixturePlan, OpenAiCompatBenchServer},
    providers::BenchmarkStreamProfile,
};
use anyhow::{Result, ensure};
use clap::Args;
use lash_core::{
    llm::types::{LlmEventSender, LlmMessage, LlmRequestScope, LlmRole},
    provider::{ProviderHandle, ProviderOptions, ProviderRetryPolicy},
};
use lash_http_transport::{
    ReqwestHttpTransport,
    observation::{BodyEnd, HttpPhaseLedger, HttpPhaseRecord, ObservedHttpTransport},
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Args)]
pub struct HttpPopulationArgs {
    #[arg(long)]
    pub out: PathBuf,
    /// Configured logical requests per finite population.
    #[arg(long, default_value_t = 6)]
    pub requests: usize,
    /// Configured arrivals per second; arrivals do not await earlier completions.
    #[arg(long, value_delimiter = ',', default_value = "10,100,1000")]
    pub rates: Vec<u64>,
    /// User text and response text sizes; actual HTTP body bytes are measured separately.
    #[arg(long, value_delimiter = ',', default_value = "128,8192")]
    pub body_bytes: Vec<usize>,
    #[arg(long, default_value_t = 16)]
    pub max_inflight: usize,
    #[arg(long, default_value_t = 2)]
    pub server_parallel: usize,
    #[arg(long, default_value_t = 2)]
    pub first_chunk_delay_ms: u64,
    #[arg(long, default_value_t = 1)]
    pub chunk_delay_ms: u64,
    #[arg(long, default_value_t = 1024)]
    pub chunk_bytes: usize,
    /// Every Nth logical request gets one 503 followed by a successful SSE body; 0 disables.
    #[arg(long, default_value_t = 2)]
    pub retry_every: usize,
}

#[derive(Debug, Serialize)]
struct Attempt {
    ordinal: u32,
    request_built_ns: u64,
    headers_received_ns: Option<u64>,
    first_chunk_ns: Option<u64>,
    end_ns: Option<u64>,
    request_body_bytes: usize,
    response_body_bytes: usize,
    status: Option<u16>,
    end: &'static str,
    sealed: lash_core::llm::types::AttemptRecord,
}
#[derive(Debug, Serialize)]
struct Arrival {
    id: usize,
    scheduled_ns: u64,
    offered_ns: u64,
    admitted_ns: Option<u64>,
    complete_ns: Option<u64>,
    outcome: &'static str,
    error: Option<String>,
    call_id: Option<lash_core::llm::types::LlmCallId>,
    attempts: Vec<Attempt>,
}
#[derive(Debug, Serialize)]
struct Population {
    configured_text_bytes: usize,
    configured_arrival_rps: u64,
    scheduled_window_ns: u64,
    completion_window_ns: u64,
    offered_window_ns: u64,
    offered: usize,
    admitted: usize,
    completed: usize,
    errors: usize,
    generator_rejected: usize,
    unfinished: usize,
    offered_per_second: f64,
    achieved_per_second: f64,
    peak_inflight: usize,
    scheduled_to_complete_ms: Option<crate::perf_support::metrics::BasicMetricStats>,
    rows: Vec<Arrival>,
}
fn nanos(epoch: Instant) -> u64 {
    epoch.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn join_attempt(
    http: HttpPhaseRecord,
    sealed: lash_core::llm::types::AttemptRecord,
    origin_ns: u64,
) -> Result<Attempt> {
    ensure!(
        http.ordinal == sealed.ordinal as usize,
        "HTTP ordinal and sealed provider attempt disagree"
    );
    let end = match http.end {
        Some(BodyEnd::Eof) => "eof",
        Some(BodyEnd::Failed) => "failed",
        Some(BodyEnd::Aborted) => "aborted",
        None => "unfinished",
    };
    Ok(Attempt {
        ordinal: sealed.ordinal,
        request_built_ns: origin_ns + http.request_built_ns,
        headers_received_ns: http.headers_received_ns.map(|ns| origin_ns + ns),
        first_chunk_ns: http.first_chunk_ns.map(|ns| origin_ns + ns),
        end_ns: http.end_ns.map(|ns| origin_ns + ns),
        request_body_bytes: http.request_body_bytes,
        response_body_bytes: http.response_body_bytes,
        status: http.status,
        end,
        sealed,
    })
}

struct Call {
    id: usize,
    scheduled_ns: u64,
    offered_ns: u64,
    epoch: Instant,
    bytes: usize,
    base_url: String,
    inner: Arc<ReqwestHttpTransport>,
    retry: bool,
}
async fn call(call: Call) -> Result<Arrival> {
    let Call {
        id,
        scheduled_ns,
        offered_ns,
        epoch,
        bytes,
        base_url,
        inner,
        retry,
    } = call;
    let admitted_ns = nanos(epoch);
    // The ledger gets this very Instant, avoiding a join between two clock reads.
    let (ledger, ledger_epoch) = HttpPhaseLedger::with_epoch(8);
    let ledger_origin_ns = ledger_epoch.duration_since(epoch).as_nanos() as u64;
    let mut options = ProviderOptions::default();
    options.reliability.retry = ProviderRetryPolicy {
        max_attempts: Some(2),
        base_delay_ms: 1,
        max_delay_ms: 1,
        jitter_ms: 0,
        ..ProviderRetryPolicy::standard()
    };
    let mut adapter =
        lash_provider_openai::OpenAiCompatibleProvider::new("loopback-fixture", base_url)
            .with_options(options)
            .with_transport(Arc::new(ObservedHttpTransport::new(inner, ledger.clone())));
    if retry {
        adapter = adapter.with_extra_headers(vec![("x-lash-perf-retry".into(), id.to_string())]);
    }
    let mut provider = ProviderHandle::new(adapter.into_components());
    let mut request = super::providers::empty_request();
    request.scope = LlmRequestScope::new(
        "http-population",
        "http-population:frame",
        format!("http-population:{id}"),
    );
    request.messages = vec![LlmMessage::text(LlmRole::User, "x".repeat(bytes))];
    request.stream_events = Some(LlmEventSender::new(|_| {}));
    // This fixture has no billing; explicitly permit its one scripted 503 replay.
    let result = provider
        .complete(
            request,
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
            lash_sansio::ExecutionBudgets::recommended(),
            &lash_core::provider::NoSlotDeliveries,
        )
        .await;
    let complete_ns = nanos(epoch);
    let (record, error) = match result {
        Ok(completion) => (completion.call_record, None),
        Err(failure) => (*failure.call_record, Some(failure.error.to_string())),
    };
    let (http, dropped) = ledger.snapshot();
    ensure!(dropped == 0, "HTTP attempt ledger overflowed");
    ensure!(
        http.len() == record.attempts.len(),
        "sealed attempts and HTTP phases have different cardinalities"
    );
    let attempts = http
        .into_iter()
        .zip(record.attempts)
        .map(|(http, sealed)| join_attempt(http, sealed, ledger_origin_ns))
        .collect::<Result<Vec<_>>>()?;
    Ok(Arrival {
        id,
        scheduled_ns,
        offered_ns,
        admitted_ns: Some(admitted_ns),
        complete_ns: Some(complete_ns),
        outcome: if error.is_none() {
            "completed"
        } else {
            "error"
        },
        error,
        call_id: Some(record.call_id),
        attempts,
    })
}

async fn population(args: &HttpPopulationArgs, bytes: usize, rate: u64) -> Result<Population> {
    let text = "y".repeat(bytes);
    let server = OpenAiCompatBenchServer::start_paced(
        BenchmarkStreamProfile {
            full_text: text.clone(),
            deltas: vec![text],
            parts: vec![],
        },
        HttpFixturePlan {
            first_chunk_delay: Duration::from_millis(args.first_chunk_delay_ms),
            chunk_delay: Duration::from_millis(args.chunk_delay_ms),
            chunk_bytes: args.chunk_bytes,
            server_parallel: args.server_parallel,
        },
    )
    .await?;
    // One client per population; every response asks Connection: close. No TLS,
    // live provider, proxy, or DNS request belongs to this fixture.
    let inner = Arc::new(ReqwestHttpTransport::from_client(
        lash_http_transport::http_client_builder()
            .no_proxy()
            .build()?,
    ));
    let slots = Arc::new(tokio::sync::Semaphore::new(args.max_inflight));
    let epoch = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    let mut rows = Vec::new();
    let mut peak_inflight = 0;
    for id in 0..args.requests {
        let scheduled_ns = ((id as u128 * 1_000_000_000) / u128::from(rate)) as u64;
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            epoch + Duration::from_nanos(scheduled_ns),
        ))
        .await;
        let offered_ns = nanos(epoch);
        match slots.clone().try_acquire_owned() {
            Ok(permit) => {
                peak_inflight = peak_inflight.max(args.max_inflight - slots.available_permits());
                let inner = inner.clone();
                let base_url = server.base_url.clone();
                let retry = args.retry_every > 0 && id.is_multiple_of(args.retry_every);
                tasks.spawn(async move {
                    let _permit = permit;
                    call(Call {
                        id,
                        scheduled_ns,
                        offered_ns,
                        epoch,
                        bytes,
                        base_url,
                        inner,
                        retry,
                    })
                    .await
                });
            }
            Err(_) => rows.push(Arrival {
                id,
                scheduled_ns,
                offered_ns,
                admitted_ns: None,
                complete_ns: None,
                outcome: "generator_rejected",
                error: None,
                call_id: None,
                attempts: vec![],
            }),
        }
    }
    while let Some(row) = tasks.join_next().await {
        rows.push(row??);
    }
    let completion_window_ns = nanos(epoch);
    rows.sort_by_key(|row| row.id);
    let scheduled_window_ns = ((args.requests as u128 * 1_000_000_000) / u128::from(rate)) as u64;
    let completed = rows.iter().filter(|row| row.outcome == "completed").count();
    let errors = rows.iter().filter(|row| row.outcome == "error").count();
    let rejected = rows
        .iter()
        .filter(|row| row.outcome == "generator_rejected")
        .count();
    let offered_window_ns =
        rows.iter().map(|row| row.offered_ns).max().unwrap_or(0) + 1_000_000_000 / rate;
    Ok(Population {
        configured_text_bytes: bytes,
        configured_arrival_rps: rate,
        scheduled_window_ns,
        completion_window_ns,
        offered_window_ns,
        offered: rows.len(),
        admitted: completed + errors,
        completed,
        errors,
        generator_rejected: rejected,
        unfinished: 0,
        offered_per_second: rows.len() as f64 / (offered_window_ns as f64 / 1e9),
        achieved_per_second: completed as f64 / (completion_window_ns as f64 / 1e9),
        peak_inflight,
        scheduled_to_complete_ms: crate::perf_support::metrics::optional_basic_summary(
            rows.iter()
                .filter_map(|row| {
                    row.complete_ns
                        .map(|end| (end - row.scheduled_ns) as f64 / 1e6)
                })
                .collect(),
        ),
        rows,
    })
}

pub async fn run(args: &HttpPopulationArgs) -> Result<()> {
    ensure!(
        args.requests > 0 && args.requests <= 100_000,
        "requests must be in 1..=100000"
    );
    ensure!(
        !args.rates.is_empty()
            && args
                .rates
                .iter()
                .all(|rate| *rate > 0 && *rate <= 1_000_000_000),
        "invalid arrival rates"
    );
    ensure!(
        !args.body_bytes.is_empty() && args.body_bytes.iter().all(|bytes| *bytes <= 1024 * 1024),
        "text sizes must be <= 1 MiB"
    );
    ensure!(
        args.max_inflight > 0 && args.server_parallel > 0 && args.chunk_bytes > 0,
        "capacities must be positive"
    );
    let mut populations = Vec::new();
    for bytes in &args.body_bytes {
        for rate in &args.rates {
            populations.push(population(args, *bytes, *rate).await?);
        }
    }
    let receipt = serde_json::json!({"kind":"lash.provider-http-phases", "mode":"functional_noncertifying",
        "provider":"local loopback OpenAI-compatible SSE; configured usage is synthetic fixture data",
        "clock":"one monotonic population epoch; per-call ledger epochs translated using the same Instant; observer process",
        "observer_pid":std::process::id(),"connection_policy":"one client per population; no proxy; plain HTTP; Connection: close; OS caches uncontrolled",
        "configured": {"requests":args.requests,"rates":args.rates,"body_bytes":args.body_bytes,"max_inflight":args.max_inflight,
            "server_parallel":args.server_parallel,"first_chunk_delay_ms":args.first_chunk_delay_ms,"chunk_delay_ms":args.chunk_delay_ms,
            "chunk_bytes":args.chunk_bytes,"retry_every":args.retry_every,"retry_delay_ms":1,"max_attempts":2,"max_unsafe_retries":1},
        "definitions": {
            "configured":"input values only: requests/max_inflight/server_parallel/max_attempts/max_unsafe_retries/retry_every are counts; rates are arrivals/second; body_bytes/chunk_bytes are bytes; *_ms are prescribed milliseconds, not observed delays",
            "configured_text_bytes,configured_arrival_rps":"inputs; text bytes per request/response and scheduled logical arrivals per second",
            "*_ns":"point samples in nanoseconds since this population epoch, except *_window_ns which are single window durations",
            "scheduled_window_ns":"configured finite arrival horizon requests/rate in nanoseconds",
            "completion_window_ns":"observed nanoseconds from population start through all task joins",
            "request_built_ns":"already-built HttpRequest passed to transport; excludes earlier provider lowering; not socket write time",
            "headers_received_ns":"response delivered by HTTP transport",
            "first_chunk_ns":"first nonempty body chunk delivered to consumer; not first visible model token",
            "end_ns,end":"body observation terminal: eof, failed, aborted or unfinished; drop is never EOF",
            "request_body_bytes,response_body_bytes":"per HTTP attempt body bytes offered/delivered; totals; excludes headers/framing/TLS and kernel wire bytes",
            "offered,admitted,completed,errors,generator_rejected,unfinished":"counts of logical arrivals in this population; admitted enters bounded generator; generator rejection is distinct from provider error",
            "peak_inflight":"maximum admitted uncompleted calls observed at each generator admission; count; sampled, not continuous high-water",
            "offered_window_ns":"observed interval from population start to last actual offer plus one configured interarrival interval; nanoseconds",
            "offered_per_second":"offered logical count divided by offered window seconds; measured generator arrival rate",
            "achieved_per_second":"successful logical count divided by completion window seconds; finite completion throughput, not sustainable capacity",
            "scheduled_to_complete_ms":"milliseconds from scheduled arrival to caller completion including generator lateness and retries; min/median/mean/p50/p95/p99/max across admitted complete/error rows; rejected rows excluded and counted separately; percentiles interpolate rank p*(n-1)",
            "sealed.usage.*":"synthetic provider-reported per-attempt token counts: uncached input/output/cache-read/cache-write/reasoning; absent means unreported, not zero",
            "sealed.retry_decision":"existing sealed per-attempt retry policy evidence; attempt_number is a 1-based retry count, tokens_at_stake is the retry-policy token counter from reported usage (policy defaults to 0 when usage is absent; absent usage remains unknown), and delay.{secs,nanos} is the scheduled delay duration in whole seconds plus subsecond nanoseconds",
            "sealed.evidence.reasoning_output_tokens":"synthetic provider-reported per-attempt reasoning token count; optional, not measured CPU time",
            "sealed.error.retry_after":"optional provider-directed wait duration; secs plus subsecond nanos, not a measured latency",
            "id,ordinal,status,call_id,observer_pid,sealed.error.http_status":"arrival/attempt/process identifiers and HTTP status vocabulary, not measured quantities"
        }, "populations":populations});
    std::fs::write(&args.out, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn provider_attempt_phases_are_complete_ordered_and_join_usage() {
        let args = HttpPopulationArgs {
            out: PathBuf::new(),
            requests: 2,
            rates: vec![1000],
            body_bytes: vec![128, 8192],
            max_inflight: 4,
            server_parallel: 2,
            first_chunk_delay_ms: 1,
            chunk_delay_ms: 1,
            chunk_bytes: 512,
            retry_every: 2,
        };
        for bytes in &args.body_bytes {
            let receipt = population(&args, *bytes, 1000).await.unwrap();
            assert_eq!(receipt.completed, 2, "{receipt:?}");
            assert_eq!(
                receipt.errors + receipt.generator_rejected + receipt.unfinished,
                0
            );
            for row in receipt.rows {
                assert!(row.scheduled_ns <= row.offered_ns);
                assert!(row.offered_ns <= row.admitted_ns.unwrap());
                assert_eq!(row.attempts.len(), if row.id == 0 { 2 } else { 1 });
                for attempt in row.attempts {
                    assert_eq!(attempt.end, "eof");
                    assert!(row.admitted_ns.unwrap() <= attempt.request_built_ns);
                    assert!(attempt.request_built_ns <= attempt.headers_received_ns.unwrap());
                    assert!(
                        attempt.headers_received_ns.unwrap() <= attempt.first_chunk_ns.unwrap()
                    );
                    assert!(attempt.first_chunk_ns.unwrap() <= attempt.end_ns.unwrap());
                    assert!(attempt.end_ns.unwrap() <= row.complete_ns.unwrap());
                    assert!(attempt.request_body_bytes >= *bytes);
                    assert!(attempt.response_body_bytes > 0);
                    if attempt.status == Some(200) {
                        let usage = attempt.sealed.usage.unwrap();
                        assert_eq!(usage.input_tokens, 512);
                        assert_eq!(usage.cache_read_input_tokens, 512);
                        assert_eq!(usage.cache_write_input_tokens, 0);
                        assert_eq!(usage.output_tokens, 64);
                        assert_eq!(usage.reasoning_output_tokens, 48);
                    } else {
                        assert_eq!(attempt.status, Some(503));
                        assert!(attempt.sealed.usage.is_none());
                    }
                }
            }
        }
    }
}
