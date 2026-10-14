//! HEAP-METER: released-worker retained-state work, independent of wall time.
use anyhow::{Context as _, Result, ensure};
use lash::tools::{StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt as _};
use lash_core::llm::types::{LlmRequest, LlmRole};
use lash_core::perf_witness::{Collector, Snapshot};
use lash_vm_client::{PoolCounters, WorkerPool};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SCENARIOS: [&str; 6] = [
    "list-update",
    "objects-update",
    "string-length",
    "drop-subgraph",
    "next-cell",
    "worker-resets",
];
const COUNTERS: [&str; 17] = [
    "vm_steps",
    "checkpoint_values",
    "resume_values",
    "carry_values",
    "encoded_nodes",
    "encoded_bytes",
    "decoded_nodes",
    "decoded_bytes",
    "ipc_messages",
    "ipc_bytes",
    "sql_statements",
    "sql_rows_written",
    "sql_expanded_bytes",
    "worker_peak_buffer_bytes",
    "broker_peak_buffer_bytes",
    "serialized_body_bytes",
    "decoded_body_bytes",
];
const LIMIT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, serde::Serialize)]
pub struct Row {
    scenario: String,
    h: usize,
    park: usize,
    counts: [u64; 17],
    probe_units: u64,
    wall_ns: u128,
    resets: u64,
    /// Owner-commit geometry from the shared declared-reads recorder.
    committed_parks: usize,
    /// The integrator fills these using page instrumentation after cutover.
    pages_loaded: Option<u64>,
    pages_sealed: Option<u64>,
    spool_bytes: Option<u64>,
    retirement_batches: Option<u64>,
}
struct Mark {
    at_us: u64,
    store: Snapshot,
    pool: PoolCounters,
    at: Instant,
}
struct State {
    mark: Option<Mark>,
    rows: Vec<Row>,
}
struct Meter {
    recorder: Arc<super::recorder::Recorder>,
    actor: String,
    collector: Collector,
    pool: WorkerPool,
    scenario: String,
    h: usize,
    state: Mutex<State>,
}
impl Meter {
    fn park(&self, phase: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = self.collector.snapshot();
        let pool = self.pool.measurements().counters;
        if phase > 0
            && let Some(mark) = &state.mark
        {
            let a = mark.pool.worker_work;
            let b = pool.worker_work;
            let counts = [
                b.vm_steps - a.vm_steps,
                b.checkpoint_values - a.checkpoint_values,
                b.resume_values - a.resume_values,
                b.carry_values - a.carry_values,
                b.encoded_nodes - a.encoded_nodes + store.graph_envelopes
                    - mark.store.graph_envelopes,
                b.encoded_bytes - a.encoded_bytes + store.graph_encoded_bytes
                    - mark.store.graph_encoded_bytes,
                b.decoded_nodes - a.decoded_nodes + store.graph_decoded_nodes
                    - mark.store.graph_decoded_nodes,
                b.decoded_bytes - a.decoded_bytes + store.graph_decoded_bytes
                    - mark.store.graph_decoded_bytes,
                pool.ipc_sent_messages + pool.ipc_received_messages
                    - mark.pool.ipc_sent_messages
                    - mark.pool.ipc_received_messages,
                pool.ipc_sent_bytes + pool.ipc_received_bytes
                    - mark.pool.ipc_sent_bytes
                    - mark.pool.ipc_received_bytes,
                store.sql_statements - mark.store.sql_statements,
                store.sql_rows_written - mark.store.sql_rows_written,
                store.sql_bytes - mark.store.sql_bytes,
                b.peak_buffer_bytes,
                pool.peak_buffer_bytes,
                b.serialized_body_bytes - a.serialized_body_bytes
                    + store.graph_serialized_body_bytes
                    - mark.store.graph_serialized_body_bytes,
                b.decoded_body_bytes - a.decoded_body_bytes + store.graph_decoded_body_bytes
                    - mark.store.graph_decoded_body_bytes,
            ];
            let committed_parks = self
                .recorder
                .transactions_since(mark.at_us)
                .iter()
                .filter(|tx| tx.ok && tx.actor == self.actor && super::turns::is_park(tx.label))
                .count();
            let row = Row {
                scenario: self.scenario.clone(),
                h: self.h,
                park: phase,
                counts,
                probe_units: b.probe_units - a.probe_units,
                wall_ns: mark.at.elapsed().as_nanos(),
                resets: pool.resets - mark.pool.resets,
                committed_parks,
                pages_loaded: None,
                pages_sealed: None,
                spool_bytes: None,
                retirement_batches: None,
            };
            state.rows.push(row);
        }
        self.pool.clear_buffer_peaks();
        state.mark = Some(Mark {
            at_us: self.recorder.now_us(),
            store,
            pool,
            at: Instant::now(),
        });
    }
}
struct Gate(tokio::sync::mpsc::Sender<usize>);
#[async_trait::async_trait]
impl StaticToolExecute for Gate {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let Some(phase) = call.args["phase"].as_f64().filter(|phase| {
            phase.is_finite() && phase.fract() == 0.0 && (0.0..=3.0).contains(phase)
        }) else {
            return lash::tools::ToolOutcome::err(serde_json::json!("invalid phase")).into();
        };
        let phase = phase as usize;
        if self.0.send(phase).await.is_err() {
            return lash::tools::ToolOutcome::err(serde_json::json!(
                "measurement receiver dropped"
            ))
            .into();
        }
        lash::tools::ToolOutcome::pending(lash::tools::PendingCompletion::new()).into()
    }
}
fn source(scenario: &str, h: usize, cell: usize) -> String {
    let seed = match scenario {
        "objects-update" => format!("let kept = []; for (let i=0; i<{h}; i++) kept.push({{v:i}});"),
        "string-length" => format!("let kept = \"x\".repeat({});", h * 256),
        "drop-subgraph" => format!(
            "let kept = []; let dropped = []; for (let i=0; i<{h}; i++) {{kept.push(i); dropped.push({{v:i}});}}"
        ),
        _ => format!("let kept = []; for (let i=0; i<{h}; i++) kept.push(i);"),
    };
    let operation = match scenario {
        "objects-update" => {
            "kept[0].v = -1; if (kept[0].v !== -1) throw new Error(\"mutation\");".to_owned()
        }
        "string-length" => format!(
            "if (kept.length !== {}) throw new Error(\"length\");",
            h * 256
        ),
        "drop-subgraph" => "dropped = null;".to_owned(),
        "next-cell" => String::new(),
        _ => "kept[0] = -1; if (kept[0] !== -1) throw new Error(\"mutation\");".to_owned(),
    };
    let body = if scenario == "next-cell" {
        if cell == 0 {
            format!("{seed} await tools.heap_gate({{phase:0}}); console.log(\"heap-seeded\");")
        } else {
            "await tools.heap_gate({phase:1}); console.log(\"heap-ok\");".to_owned()
        }
    } else {
        let steps = if scenario == "worker-resets" { 3 } else { 1 };
        let mut body = format!("{seed} await tools.heap_gate({{phase:0}});");
        for phase in 1..=steps {
            body.push_str(&format!(
                "{operation} await tools.heap_gate({{phase:{phase}}});"
            ));
        }
        body.push_str("console.log(\"heap-ok\");");
        body
    };
    format!("<typescript>\n/*heap-cell:{cell}*/\n{body}\n</typescript>")
}
fn provider(scenario: &str, h: usize) -> lash_core::provider::ProviderHandle {
    let scenario = scenario.to_owned();
    lash_core::testing::TestProvider::builder().kind("heap-meter").requires_streaming(true)
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::disabled(), ..Default::default()
        }).complete(move |request: LlmRequest| {
            let scenario = scenario.clone();
            async move {
                let cells = request.messages.iter().filter(|message| message.role == LlmRole::Assistant)
                    .flat_map(|message| message.blocks.iter()).filter(|block| matches!(block,
                        lash_core::llm::types::LlmContentBlock::Text { text, .. } if text.contains("/*heap-cell:"))).count();
                let total = if scenario == "next-cell" { 2 } else { 1 };
                let text = if cells < total { source(&scenario, h, cells) } else {
                    let feedback = serde_json::to_string(&request.messages.iter().filter(|message| message.role == LlmRole::User).collect::<Vec<_>>()).unwrap_or_default();
                    if !feedback.contains("heap-ok") { eprintln!("heap feedback {scenario}/{h}: {feedback}"); }
                    "heap complete".to_owned()
                };
                Ok(super::cells::streamed(&request, &text))
            }
        }).build().into_handle()
}
async fn sample(
    worker: &Path,
    database: &Path,
    scenario: &str,
    h: usize,
    probe: Option<&str>,
) -> Result<Vec<Row>> {
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        database,
        lash_sqlite_store::SqliteSynchronous::Full,
    )
    .await?;
    sample_with_stores(worker_service(worker, probe), stores, scenario, h, probe).await
}

fn worker_service(worker: &Path, probe: Option<&str>) -> lash::vm::WorkerService {
    let mut config = lash::vm::WorkerService::subprocess(worker).config().clone();
    config.entry.args.push("--count-heap-work".to_owned());
    if let Some(probe) = probe {
        config.entry.args.push(format!("--heap-walk={probe}"));
    }
    config.deadlines.compute = LIMIT;
    config.deadlines.serialization = LIMIT;
    config.deadlines.cumulative_cpu = LIMIT;
    lash::vm::WorkerService::new(config)
}

async fn sample_with_stores(
    service: lash::vm::WorkerService,
    stores: lash_sqlite_store::SqliteStoreSet,
    scenario: &str,
    h: usize,
    probe: Option<&str>,
) -> Result<Vec<Row>> {
    let recorder = Arc::new(super::recorder::Recorder::default());
    let stores = super::recorder::RecordingStores::new(
        Arc::new(stores),
        "heap-meter",
        Arc::clone(&recorder),
        "sqlite",
    );
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores)).build()?;
    let pool = service.pool()?;
    let session_id = lash::SessionId::try_from(format!("heap-{scenario}-{h}"))?;
    let meter = Arc::new(Meter {
        actor: super::turn::session_actor(&session_id)?.to_string(),
        recorder,
        collector: Collector::install_with_sql_receipts()?,
        pool,
        scenario: scenario.to_owned(),
        h: if scenario == "string-length" {
            h * 256
        } else {
            h
        },
        state: Mutex::new(State {
            mark: None,
            rows: Vec::new(),
        }),
    });
    let (gate_sender, mut gates) = tokio::sync::mpsc::channel(4);
    let tool = lash::tools::ToolDefinition::raw("heap_gate", "heap_gate", "Commit a measured park.",
        serde_json::json!({"type":"object","properties":{"phase":{"type":"integer","minimum":0}},"required":["phase"],"additionalProperties":false}),
        serde_json::json!({"type":"object"}))?
        .with_execution(LIMIT)
        .with_declaration(lash::tools::ToolDeclaration::deferring(), Some(lash::tools::ParkBound::Within(LIMIT)))?
        .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "heap_gate"));
    let core = lash::LashCore::rlm_builder(
        backend,
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(100_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(512))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(service),
    )
    .commit_budget(lash::CommitBudget::bounded(256 * 1024 * 1024, 1_000_000))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(
        provider(scenario, h),
        lash::LlmProfileMetadata::builder("heap-meter")
            .cache_retention(lash::provider::CacheRetention::Short)
            .context_window_tokens(1_000_000)
            .build()?,
    )
    .tools(
        lash::tool_provider_key!("heap-meter"),
        Arc::new(StaticToolProvider::new(vec![tool], Gate(gate_sender))),
    )
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("heap-meter"),
        lash::persistence::LeaseIncarnationId::new(format!("{scenario}-{h}")),
    ))?;
    let session = core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                "heap-meter",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(16),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(4)),
        ))
        .await?;
    let handle = session
        .send(lash::TurnInput::text("measure retained state"))
        .await?;
    let expected = if scenario == "worker-resets" { 3 } else { 1 };
    let result = tokio::time::timeout(LIMIT, async {
        let mut output = std::pin::pin!(handle.output());
        for phase in 0..=expected {
            let received = tokio::select! {
                phase = gates.recv() => phase.context("gate sender closed")?,
                result = &mut output => anyhow::bail!("{scenario}/{h} ended before gate {phase}: {:?}", result?.result.outcome),
            };
            ensure!(received == phase, "gate {received} arrived instead of {phase}");
            // The tool merely offers a pending outcome. The engine commits
            // its wait and releases/resets the machine before this observer
            // supplies the external completion through the production API.
            let call = loop {
                let calls = core.completions().parked(lash::admin::CallOwner::Session(session_id.clone())).await?;
                let stats = meter.pool.stats();
                if stats.workers > 0 && stats.idle == stats.workers
                    && let Some(call) = calls.into_iter().next() {
                    break call;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };
            meter.park(phase);
            ensure!(matches!(core.completions().resolve(call.key.as_str(), lash::Resolution::Ok(serde_json::json!({}))).await?,
                lash::durable::ResolveAnswer::Resolved), "gate resolution refused");
        }
        anyhow::Ok(output.await?)
    }).await.context("heap cell never completed");
    core.shutdown().await?;
    let output = result??;
    ensure!(
        matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
        "heap turn failed: {:?}",
        output.result.outcome
    );
    let rows = meter
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .rows
        .clone();
    let expected = if scenario == "worker-resets" { 3 } else { 1 };
    ensure!(
        rows.len() == expected,
        "{scenario} measured {} of {expected} parks",
        rows.len()
    );
    for row in &rows {
        ensure!(
            row.committed_parks == 1,
            "{scenario}/{h} interval did not contain exactly one committed owner park: {row:?}"
        );
        for (name, count) in COUNTERS.iter().zip(row.counts) {
            if *name == "carry_values" {
                continue;
            }
            ensure!(count > 0, "{scenario}/{h}/{} has no {name}", row.park);
        }
        ensure!(
            row.resets > 0,
            "{scenario}/{h} did not release/reset its worker"
        );
        if scenario == "next-cell" {
            ensure!(row.counts[3] > 0, "cell carried no retained bindings");
        }
        if probe.is_some() && (probe != Some("carry") || scenario == "next-cell") {
            ensure!(
                row.probe_units >= h as u64,
                "whole-heap probe was invisible: {row:?}"
            );
        }
    }
    Ok(rows)
}
/// Least-squares exponent of log count against log H; zero/absent counts
/// have no exponent. Wall time is never an oracle.
fn exponent(rows: &[&Row], counter: impl Fn(&Row) -> u64) -> Option<f64> {
    if rows.len() < 2 || rows.iter().any(|row| counter(row) == 0) {
        return None;
    }
    let n = rows.len() as f64;
    let x = rows.iter().map(|row| (row.h as f64).ln()).sum::<f64>() / n;
    let y = rows
        .iter()
        .map(|row| (counter(row) as f64).ln())
        .sum::<f64>()
        / n;
    let covariance = rows
        .iter()
        .map(|row| ((row.h as f64).ln() - x) * ((counter(row) as f64).ln() - y))
        .sum::<f64>();
    let variance = rows
        .iter()
        .map(|row| ((row.h as f64).ln() - x).powi(2))
        .sum::<f64>();
    (variance > 0.0).then_some(covariance / variance)
}
pub async fn run(run: &super::Run<'_>, sizes: &[usize], probe: Option<&str>) -> Result<()> {
    let super::deploy::Database::Sqlite(base) = &run.database else {
        anyhow::bail!("heap-scaling uses SQLite");
    };
    ensure!(
        sizes.len() >= 2 && sizes.iter().all(|h| *h > 0) && sizes.windows(2).all(|h| h[0] < h[1]),
        "H must be positive and strictly increasing"
    );
    ensure!(
        probe.is_none_or(|p| ["checkpoint", "resume", "carry"].contains(&p)),
        "unknown heap-walk location"
    );
    let mut all = Vec::new();
    for scenario in SCENARIOS {
        for &h in sizes {
            let rows = sample(
                run.worker,
                &base.with_file_name(format!("heap-{scenario}-{h}.sqlite")),
                scenario,
                h,
                probe,
            )
            .await?;
            for row in rows {
                run.report.write("heap_work", &row)?;
                all.push(row);
            }
        }
    }
    println!(
        "# released-worker heap baseline; main_sha={}; probe={}; H=entries/objects (string: bytes); buffers=transport working allocations; SQL bytes=expanded statement literals; page counters=n/a",
        std::process::Command::new("git")
            .args(["merge-base", "HEAD", "origin/main"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|sha| sha.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
        probe.unwrap_or("off")
    );
    println!(
        "scenario H park {} probe_units wall_ms resets pages_loaded pages_sealed spool_bytes retirement_batches",
        COUNTERS.join(" ")
    );
    for row in &all {
        println!(
            "{} {} {} {} {} {:.3} {} n/a n/a n/a n/a",
            row.scenario,
            row.h,
            row.park,
            row.counts
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(" "),
            row.probe_units,
            row.wall_ns as f64 / 1e6,
            row.resets
        );
    }
    for scenario in SCENARIOS {
        let rows: Vec<_> = all
            .iter()
            .filter(|row| row.scenario == scenario && row.park == 1)
            .collect();
        let slopes: Vec<_> = (0..COUNTERS.len())
            .map(|index| {
                exponent(&rows, |row| row.counts[index])
                    .map_or_else(|| "n/a".to_owned(), |slope| format!("{slope:.3}"))
            })
            .collect();
        println!(
            "{scenario} exponent - {} {} - - n/a n/a n/a n/a",
            slopes.join(" "),
            exponent(&rows, |row| row.probe_units)
                .map_or_else(|| "n/a".to_owned(), |slope| format!("{slope:.3}"))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// HEAP-METER: every released-worker park/resume seam is measured for all
    /// six retained-state scenarios; each slowdown location sees a heap walk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn six_scenarios_count_released_worker_work_and_see_heap_walks() {
        let worker = std::env::var_os("LASH_VM_WORKER").expect("worker runfile");
        let started = std::time::Instant::now();
        // The law counts work at released-worker seams, independently of
        // fsync latency or cold process startup. Keep the full size matrix.
        let service = super::worker_service(Path::new(&worker), None);
        for scenario in super::SCENARIOS {
            for h in [100, 1000] {
                super::sample_with_stores(
                    service.clone(),
                    lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
                    scenario,
                    h,
                    None,
                )
                .await
                .unwrap();
            }
        }
        for (probe, scenario) in [
            ("checkpoint", "list-update"),
            ("resume", "list-update"),
            ("carry", "next-cell"),
        ] {
            let service = super::worker_service(Path::new(&worker), Some(probe));
            let mut rows = Vec::new();
            for h in [100, 1000] {
                rows.extend(
                    super::sample_with_stores(
                        service.clone(),
                        lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
                        scenario,
                        h,
                        Some(probe),
                    )
                    .await
                    .unwrap(),
                );
            }
            let samples: Vec<_> = rows.iter().collect();
            let exponent = super::exponent(&samples, |row| row.probe_units).unwrap();
            assert!(
                exponent > 0.9,
                "{probe} walk was not H-proportional: {rows:?}"
            );
        }
        eprintln!("HEAP-METER: 18 populations in {:?}", started.elapsed());
    }
    use std::path::Path;
}
