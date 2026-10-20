//! HEAP-METER: released-worker retained-state work, independent of wall
//! time, and the paged heap's counted laws over it: checkpoint work is flat
//! in H at a fixed change, a cold continuation is flat in H at a fixed
//! working set, a length read loads no payload, a cell that touches
//! nothing loads and seals O(1) pages, the bounded retirement steps a
//! park leaves are flat in H, a park's publication SQL is flat and within
//! a fixed budget, and the host's page checks after the last gate grow at
//! most linearly.
use anyhow::{Context as _, Result, ensure};
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::llm::types::{LlmRequest, LlmRole};
use lash_core_ids::perf_witness::Collector;
use lash_core_ids::perf_witness::Snapshot;
use lash_durable::domain::pages::{RetirementBudget, RetirementProgress};
use lash_execution_contracts::backend::StoreSet;
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
const COUNTERS: [&str; 21] = [
    "vm_steps",
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
    "pages_loaded",
    "pages_sealed",
    "spool_bytes",
    "retirement_batches",
    "publish_statements",
    "publish_rows_read",
    "publish_vm_steps",
];
/// Column indices of the counters the laws read.
const PAGES_LOADED: usize = 14;
const PAGES_SEALED: usize = 15;
const SPOOL_BYTES: usize = 16;
const RETIREMENT_BATCHES: usize = 17;
const PUBLISH_VM_STEPS: usize = 20;
const _: () = assert!(matches!(COUNTERS[PAGES_LOADED].as_bytes(), b"pages_loaded"));
const _: () = assert!(matches!(COUNTERS[SPOOL_BYTES].as_bytes(), b"spool_bytes"));
const _: () = assert!(matches!(
    COUNTERS[RETIREMENT_BATCHES].as_bytes(),
    b"retirement_batches"
));
const _: () = assert!(matches!(
    COUNTERS[PUBLISH_VM_STEPS].as_bytes(),
    b"publish_vm_steps"
));
const LIMIT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, serde::Serialize)]
pub struct Row {
    scenario: String,
    /// `sqlite` or `postgres`: SQL engine steps are SQLite's only.
    store: &'static str,
    h: usize,
    park: usize,
    counts: [Option<u64>; 21],
    wall_ns: u128,
    /// Building the retained state and committing its first park, up to
    /// gate 0: diagnostic wall time, never an oracle.
    seed_ns: u128,
    resets: u64,
    /// Owner-commit geometry from the shared declared-reads recorder.
    committed_parks: usize,
    /// From the last gate's resolution to the turn's outcome (the cell's
    /// end, its carry and the next prompt), on the last interval's row: the
    /// pages the host checked as it read them, and diagnostic wall time.
    tail_host_checks: u64,
    tail_ns: u128,
}
/// SQLite perf-witness SQL counters are not measured on PostgreSQL. Keep
/// accepted publication statements/rows and all common page counters.
fn available_counts(store: &str, counts: [u64; 21]) -> [Option<u64>; 21] {
    let mut counts = counts.map(Some);
    if store != "sqlite" {
        for index in [7, 8, 9, PUBLISH_VM_STEPS] {
            counts[index] = None;
        }
    }
    counts
}

fn counter_text(value: &Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

struct Mark {
    at_us: u64,
    store: Snapshot,
    pool: PoolCounters,
    at: Instant,
}
struct State {
    started: Instant,
    seed_ns: u128,
    mark: Option<Mark>,
    rows: Vec<Row>,
}
struct Meter {
    recorder: Arc<super::recorder::Recorder>,
    actor: String,
    collector: Collector,
    pool: WorkerPool,
    scenario: String,
    store: &'static str,
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
        if phase == 0 {
            state.seed_ns = state.started.elapsed().as_nanos();
        }
        if phase > 0
            && let Some(mark) = &state.mark
        {
            // The interval's accepted owner parks, and the SQL their
            // publishing transactions ran: background retirement and the
            // observer's reads are other transactions.
            let parks: Vec<_> = self
                .recorder
                .transactions_since(mark.at_us)
                .into_iter()
                .filter(|tx| tx.ok && tx.actor == self.actor && super::turns::is_park(tx.label))
                .collect();
            let published =
                |field: fn(&lash_core_execution::facade_support::sql::Receipt) -> u64| {
                    parks
                        .iter()
                        .filter_map(|tx| tx.sql.as_ref())
                        .map(field)
                        .sum::<u64>()
                };
            let a = mark.pool.worker_work;
            let b = pool.worker_work;
            let counts = [
                b.vm_steps - a.vm_steps,
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
                b.pages_loaded - a.pages_loaded,
                b.pages_sealed - a.pages_sealed,
                store.spool_bytes - mark.store.spool_bytes,
                store.retirement_batches - mark.store.retirement_batches,
                published(|sql| sql.statements),
                published(|sql| sql.rows_returned),
                0, // populated below from typed optional SQLite receipts
            ];
            let mut counts = available_counts(self.store, counts);
            counts[PUBLISH_VM_STEPS] = parks
                .iter()
                .map(|tx| tx.sql.as_ref()?.sqlite_work.map(|work| work.vm_steps))
                .sum();
            let committed_parks = parks.len();
            let row = Row {
                scenario: self.scenario.clone(),
                store: self.store,
                h: self.h,
                park: phase,
                counts,
                wall_ns: mark.at.elapsed().as_nanos(),
                seed_ns: state.seed_ns,
                tail_host_checks: 0,
                tail_ns: 0,
                resets: pool.resets - mark.pool.resets,
                committed_parks,
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
impl Meter {
    /// Closes the tail: what the host did since the last gate.
    fn tail(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = self.collector.snapshot();
        let Some(mark) = state.mark.take() else {
            return;
        };
        if let Some(row) = state.rows.last_mut() {
            row.tail_host_checks = store.hash_passes - mark.store.hash_passes;
            row.tail_ns = mark.at.elapsed().as_nanos();
        }
    }
}
struct Gate(tokio::sync::mpsc::Sender<usize>);
#[async_trait::async_trait]
impl StaticToolExecute for Gate {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let Some(phase) = call.args["phase"].as_f64().filter(|phase| {
            phase.is_finite() && phase.fract() == 0.0 && (0.0..=3.0).contains(phase)
        }) else {
            return lash::tools::ToolOutcome::err("invalid phase").into();
        };
        let phase = phase as usize;
        if self.0.send(phase).await.is_err() {
            return lash::tools::ToolOutcome::err("measurement receiver dropped").into();
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
            format!("{seed} await tools.heapGate({{phase:0}}); console.log(\"heap-seeded\");")
        } else {
            "await tools.heapGate({phase:1}); console.log(\"heap-ok\");".to_owned()
        }
    } else {
        let steps = if scenario == "worker-resets" { 3 } else { 1 };
        let mut body = format!("{seed} await tools.heapGate({{phase:0}});");
        for phase in 1..=steps {
            body.push_str(&format!(
                "{operation} await tools.heapGate({{phase:{phase}}});"
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
async fn sample(run: &super::Run<'_>, scenario: &str, h: usize) -> Result<Vec<Row>> {
    let (stores, store, _storage) = match &run.database {
        super::deploy::Database::Sqlite(base) => {
            let stores = lash_sqlite_store::SqliteStoreSet::open(
                &base.with_file_name(format!("heap-{scenario}-{h}.sqlite")),
                lash_sqlite_store::SqliteSynchronous::Full,
            )
            .await?;
            (
                Arc::new(stores) as Arc<dyn lash_execution_contracts::backend::StoreSet>,
                "sqlite",
                None,
            )
        }
        // One database: each sample's session is its own.
        super::deploy::Database::Postgres(_) => {
            let (stores, storage) = run.deployment().stores().await?;
            (stores, "postgres", storage)
        }
    };
    sample_with_stores(worker_service(run.worker), stores, store, scenario, h).await
}

fn worker_service(worker: &Path) -> lash::vm::WorkerService {
    let mut config = lash_perf::cell_bounds::subprocess(worker).config().clone();
    config.entry.args.push("--count-heap-work".to_owned());
    config.deadlines.compute = LIMIT;
    config.deadlines.serialization = LIMIT;
    config.deadlines.cumulative_cpu = LIMIT;
    lash::vm::WorkerService::new(config)
}

async fn sample_with_stores(
    service: lash::vm::WorkerService,
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    store: &'static str,
    scenario: &str,
    h: usize,
) -> Result<Vec<Row>> {
    let recorder = Arc::new(super::recorder::Recorder::default());
    let stores = Arc::new(super::recorder::RecordingStores::new(
        stores,
        "heap-meter",
        Arc::clone(&recorder),
        store,
    ));
    let retirement_stores = Arc::clone(&stores);
    let stores: Arc<dyn StoreSet> = stores;
    // No background sweep during a sample: the driver still sweeps once at
    // boot, then waits; each mark drains retirement itself, so an interval
    // counts the steps its own park left.
    let mut settings = lash::durable::DurableSettings::standard();
    settings.retirement.every = Duration::from_secs(3600);
    let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(&stores))
        .config(settings)
        .build()?;
    let retirement = stores
        .durable_store()
        .pages()
        .context("the durable store publishes heap pages")?;
    let pool = service.pool()?;
    let spools = service.spools()?;
    let session_id = lash::SessionId::try_from(format!("heap-{scenario}-{h}"))?;
    let meter = Arc::new(Meter {
        actor: super::turn::session_actor(&session_id)?.to_string(),
        recorder,
        collector: Collector::install_with_sql_receipts()?
            .with_spool_bytes(move || spools.spooled_bytes())
            .with_retirement_batches(move || retirement_stores.retirement_batches()),
        pool,
        scenario: scenario.to_owned(),
        store,
        h: if scenario == "string-length" {
            h * 256
        } else {
            h
        },
        state: Mutex::new(State {
            started: Instant::now(),
            seed_ns: 0,
            mark: None,
            rows: Vec::new(),
        }),
    });
    let (gate_sender, mut gates) = tokio::sync::mpsc::channel(4);
    let tool = lash::tools::ToolDefinition::raw("heap_gate", "tools.heap_gate", "Commit a measured park.",
        serde_json::json!({"type":"object","properties":{"phase":{"type":"integer","minimum":0}},"required":["phase"],"additionalProperties":false}),
        serde_json::json!({"type":"object"}))?
        .with_execution(LIMIT)
        .with_declaration(lash::tools::ToolDeclaration::deferring(), Some(lash::tools::ParkBound::Within(LIMIT)))?;
    let core = lash::LashCore::rlm_builder(
        backend,
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash_perf::cell_bounds::INSTRUCTION_LIMIT)
                .memory_limit(lash_perf::cell_bounds::MEMORY_LIMIT)
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
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "heap-meter",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash::NoProgressBudget::bounded(4),
        )))
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
                result = &mut output => anyhow::bail!("{scenario}/{h} ended before gate {phase}: {:?}", result?.outcome),
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
            // Drain what the park replaced before the mark: the interval
            // then counts the bounded steps, never a background sweep's
            // timing.
            while retirement
                .retire_step(RetirementBudget {
                    nodes: lash::durable::RetirementSettings::standard().batch_nodes,
                })
                .await?
                != RetirementProgress::default()
            {}
            meter.park(phase);
            ensure!(matches!(core.completions().resolve(call.key.as_str(), lash::Resolution::Ok(serde_json::json!({}))).await?,
                lash::durable::ResolveAnswer::Resolved), "gate resolution refused");
        }
        let output = output.await?;
        meter.tail();
        anyhow::Ok(output)
    }).await.context("heap cell never completed");
    core.shutdown().await?;
    let output = result??;
    ensure!(
        matches!(output.outcome, lash::TurnOutcome::Finished(_)),
        "heap turn failed: {:?}: {:?}",
        output.outcome,
        output.outcome.error_cause()
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
        // Every interval steps the machine, crosses frames, commits and
        // seals its park's changed pages; the rest may be zero.
        let sqlite: &[&str] = if store == "sqlite" {
            &["sql_statements", "publish_vm_steps"]
        } else {
            &[]
        };
        for &name in [
            "vm_steps",
            "ipc_messages",
            "retirement_batches",
            "pages_sealed",
            "publish_statements",
        ]
        .iter()
        .chain(sqlite)
        {
            let index = COUNTERS
                .iter()
                .position(|counter| *counter == name)
                .unwrap_or(0);
            ensure!(
                row.counts[index].is_some_and(|value| value > 0),
                "{scenario}/{h}/{} has no {name}",
                row.park
            );
        }
        ensure!(
            row.resets > 0,
            "{scenario}/{h} did not release/reset its worker"
        );
    }
    Ok(rows)
}

/// The most a counted law lets a count grow across the populations: the
/// least-squares exponent of log count against log H. Flat work grows only
/// with the trees' depth, which a 100x larger heap deepens by a level.
const FLAT: f64 = 0.25;

/// The counted laws over each scenario's measured intervals (`park` 1),
/// at two or more H. Counts, never wall time.
fn laws(rows: &[Row]) -> Result<()> {
    let interval = |scenario: &str| -> Vec<&Row> {
        rows.iter()
            .filter(|row| row.scenario == scenario && row.park == 1)
            .collect()
    };
    // Every law is checked, and every one broken is named.
    let broken = std::cell::RefCell::new(Vec::new());
    let flat = |scenario: &str, what: &str, counter: &dyn Fn(&Row) -> Option<u64>| {
        if let Err(error) = flat_law(&interval(scenario), scenario, what, counter) {
            broken.borrow_mut().push(error.to_string());
        }
    };
    // Checkpoint work at a fixed change D: the pages a park seals and the
    // bytes it spools.
    for scenario in [
        "list-update",
        "objects-update",
        "drop-subgraph",
        "worker-resets",
    ] {
        flat(scenario, "checkpoint pages", &|row| {
            row.counts[PAGES_SEALED]
        });
        flat(scenario, "spooled bytes", &|row| row.counts[SPOOL_BYTES]);
    }
    // Reclamation at a fixed change D: the bounded steps that retire what
    // a park replaced, drained at each mark.
    for scenario in ["list-update", "objects-update", "worker-resets"] {
        flat(scenario, "retirement steps a park leaves", &|row| {
            row.counts[RETIREMENT_BATCHES]
        });
    }
    // A cold continuation at a fixed working set W: a released worker
    // resumes and loads only the pages the run touches.
    for scenario in ["list-update", "objects-update", "worker-resets"] {
        flat(scenario, "pages a resume loads", &|row| {
            row.counts[PAGES_LOADED]
        });
    }
    // A length read loads no payload: the string's length is its summary.
    flat("string-length", "pages a length read loads", &|row| {
        row.counts[PAGES_LOADED]
    });
    // A cell that touches nothing loads and seals O(1) pages.
    flat("next-cell", "pages a no-op cell moves", &|row| {
        row.counts[PAGES_LOADED]
            .zip(row.counts[PAGES_SEALED])
            .map(|(loaded, sealed)| loaded + sealed)
    });
    // Publication follows the candidate's changed pages: the SQL engine
    // work of the park's fenced owner transaction, which a walk of the
    // committed closure (one statement, any rows) would grow with H.
    // SQLite's engine counts them; PostgreSQL's publication is measured,
    // not held to these two laws.
    let sqlite = rows.iter().all(|row| row.store == "sqlite");
    for scenario in SCENARIOS.into_iter().filter(|_| sqlite) {
        flat(scenario, "publication SQL steps", &|row| {
            row.counts[PUBLISH_VM_STEPS]
        });
    }
    // At these H a closure walk's cost is mostly the release graph every
    // root reaches, so a growth exponent alone dilutes it: a park's
    // publication also stays within a fixed budget at every H.
    for row in rows
        .iter()
        .filter(|row| row.counts[PUBLISH_VM_STEPS].is_some_and(|value| value > PUBLISH_STEPS))
    {
        broken.borrow_mut().push(format!(
            "{}/{}/{}: publication ran {} SQL steps, over its budget of {PUBLISH_STEPS}",
            row.scenario,
            row.h,
            row.park,
            counter_text(&row.counts[PUBLISH_VM_STEPS])
        ));
    }
    // The next prompt reads what the cell left a bounded number of times:
    // the host's page checks after the last gate grow at most linearly
    // with H, where a read that restarts at every missing page grows with
    // its square.
    for scenario in SCENARIOS {
        let rows: Vec<&Row> = rows
            .iter()
            .filter(|row| row.scenario == scenario && row.tail_ns > 0)
            .collect();
        if let Err(error) = bounded_law(
            &rows,
            scenario,
            "host page checks after the last gate",
            LINEAR,
            &|row| Some(row.tail_host_checks),
        ) {
            broken.borrow_mut().push(error.to_string());
        }
    }
    let broken = broken.into_inner();
    ensure!(broken.is_empty(), "{}", broken.join("\n"));
    Ok(())
}
/// The most SQLite VM steps one park's publishing transaction runs at a
/// fixed change: about twice the most measured (8,292, objects-update at
/// H = 10,000).
const PUBLISH_STEPS: u64 = 16_384;

/// The most a whole-value read lets a count grow: once per retained
/// element, plus the trees' depth.
const LINEAR: f64 = 1.25;

/// One counted law: `counter` over a scenario's intervals grows with H by
/// an exponent of at most [`FLAT`].
fn flat_law(
    rows: &[&Row],
    scenario: &str,
    what: &str,
    counter: &dyn Fn(&Row) -> Option<u64>,
) -> Result<()> {
    bounded_law(rows, scenario, what, FLAT, counter)
}

/// One counted law: `counter` grows with H by an exponent of at most
/// `limit`.
fn bounded_law(
    rows: &[&Row],
    scenario: &str,
    what: &str,
    limit: f64,
    counter: &dyn Fn(&Row) -> Option<u64>,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // A count that is absent or zero has no exponent: it measures
    // nothing, so it never passes as flat.
    let slope = exponent(rows, counter).with_context(|| {
        format!(
            "{scenario}: {what} has no exponent: {:?}",
            rows.iter()
                .map(|row| (row.h, counter(row)))
                .collect::<Vec<_>>()
        )
    })?;
    ensure!(
        slope <= limit,
        "{scenario}: {what} grows with H (exponent {slope:.3}): {:?}",
        rows.iter()
            .map(|row| (row.h, counter(row)))
            .collect::<Vec<_>>()
    );
    Ok(())
}
/// Least-squares exponent of log count against log H; zero/absent counts
/// have no exponent. Wall time is never an oracle.
fn exponent(rows: &[&Row], counter: impl Fn(&Row) -> Option<u64>) -> Option<f64> {
    let points: Vec<_> = rows
        .iter()
        .map(|row| {
            counter(row)
                .filter(|value| *value > 0)
                .map(|value| ((row.h as f64).ln(), (value as f64).ln()))
        })
        .collect::<Option<_>>()?;
    lash_perf::replay::receipt::least_squares_slope(&points)
}

pub async fn run(run: &super::Run<'_>, sizes: &[usize], only: &[String]) -> Result<()> {
    ensure!(
        sizes.len() >= 2 && sizes.iter().all(|h| *h > 0) && sizes.windows(2).all(|h| h[0] < h[1]),
        "H must be positive and strictly increasing"
    );
    ensure!(
        only.iter().all(|name| SCENARIOS.contains(&name.as_str())),
        "heap scenarios are {SCENARIOS:?}"
    );
    let scenarios: Vec<&str> = SCENARIOS
        .into_iter()
        .filter(|scenario| only.is_empty() || only.iter().any(|name| name == scenario))
        .collect();
    let mut all = Vec::new();
    for &scenario in &scenarios {
        for &h in sizes {
            let rows = sample(run, scenario, h).await?;
            for row in rows {
                run.report.write("heap_work", &row)?;
                all.push(row);
            }
        }
    }
    println!(
        "# released-worker paged heap baseline; main_sha={}; H=entries/objects (string: bytes); buffers=transport working allocations; SQL bytes=expanded statement literals; pages=worker page port; spool=parent staging",
        std::process::Command::new("git")
            .args(["merge-base", "HEAD", "origin/main"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|sha| sha.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
    );
    println!(
        "scenario H park {} wall_ms seed_ms resets tail_host_checks tail_ms",
        COUNTERS.join(" ")
    );
    for row in &all {
        println!(
            "{} {} {} {} {:.3} {:.3} {} {} {:.3}",
            row.scenario,
            row.h,
            row.park,
            row.counts
                .iter()
                .map(counter_text)
                .collect::<Vec<_>>()
                .join(" "),
            row.wall_ns as f64 / 1e6,
            row.seed_ns as f64 / 1e6,
            row.resets,
            row.tail_host_checks,
            row.tail_ns as f64 / 1e6
        );
    }
    for &scenario in &scenarios {
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
        let tail: Vec<_> = all
            .iter()
            .filter(|row| row.scenario == scenario && row.tail_ns > 0)
            .collect();
        println!(
            "{scenario} exponent - {} - - - {} -",
            slopes.join(" "),
            exponent(&tail, |row| Some(row.tail_host_checks))
                .map_or_else(|| "n/a".to_owned(), |slope| format!("{slope:.3}"))
        );
    }
    laws(&all)
}

#[cfg(test)]
mod tests {
    /// FIG-6620: unavailable SQLite work stays absent on PostgreSQL; genuine
    /// measured zero and accepted publication/page counts stay available.
    #[test]
    fn backend_availability_preserves_unknown_and_measured_zero() {
        let sqlite = super::available_counts("sqlite", [0; 21]);
        let postgres = super::available_counts("postgres", [0; 21]);
        for index in [7, 8, 9, super::PUBLISH_VM_STEPS] {
            assert_eq!(sqlite[index], Some(0));
            assert_eq!(postgres[index], None);
            assert_eq!(super::counter_text(&postgres[index]), "unknown");
        }
        for index in [super::PAGES_LOADED, super::PAGES_SEALED, 18, 19] {
            assert_eq!(postgres[index], Some(0));
        }
        println!(
            "backend-availability.json={}",
            serde_json::json!({
                "counters": super::COUNTERS, "sqlite": sqlite, "postgres": postgres
            })
        );
    }

    static HEAP_SAMPLES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    /// HEAP-RETIREMENT-ISOLATION: a second active store cannot change the
    /// measured store's retirement count, even while its collector is active.
    #[tokio::test]
    async fn another_stores_retirement_is_not_counted() {
        use super::super::recorder::{Recorder, RecordingStores};
        use lash_durable::domain::pages::RetirementBudget;
        use lash_execution_contracts::backend::StoreSet;
        use std::sync::Arc;
        let _sample = HEAP_SAMPLES.lock().await;
        // Even stores reporting transactions to the same recorder keep
        // independent retirement counts.
        let recorder = Arc::new(Recorder::default());
        let a = Arc::new(RecordingStores::new(
            Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
            "a",
            Arc::clone(&recorder),
            "sqlite",
        ));
        let b = RecordingStores::new(
            Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
            "b",
            recorder,
            "sqlite",
        );
        let read = Arc::clone(&a);
        let collector = super::Collector::install()
            .unwrap()
            .with_retirement_batches(move || read.retirement_batches());
        let a = a.durable_store().pages().unwrap();
        let b_pages = b.durable_store().pages().unwrap();
        a.retire_step(RetirementBudget { nodes: 1 }).await.unwrap();
        let before = collector.snapshot().retirement_batches;
        assert_eq!(before, 1);
        b_pages
            .retire_step(RetirementBudget { nodes: 1 })
            .await
            .unwrap();
        assert_eq!(b.retirement_batches(), 1, "the other store did real work");
        assert_eq!(
            collector.snapshot().retirement_batches,
            before,
            "another active store contaminated the measured store"
        );
        a.retire_step(RetirementBudget { nodes: 1 }).await.unwrap();
        assert_eq!(collector.snapshot().retirement_batches, before + 1);
    }

    /// HEAP-METER: every released-worker park/resume seam is measured for all
    /// six retained-state scenarios, at H 100 and 2,000 and objects-update at
    /// 1,000 and 10,000 (whose park once stalled past the sample limit), and
    /// the paged heap's counted laws hold over them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn six_scenarios_obey_the_paged_heaps_counted_laws() {
        let _sample = HEAP_SAMPLES.lock().await;
        let worker = std::env::var_os("LASH_VM_WORKER").expect("worker runfile");
        let started = std::time::Instant::now();
        // The laws count work at released-worker seams, independently of
        // fsync latency or cold process startup.
        let service = super::worker_service(Path::new(&worker));
        let mut rows = Vec::new();
        for scenario in super::SCENARIOS {
            let sizes = if scenario == "objects-update" {
                [1000, 10_000]
            } else {
                [100, 2000]
            };
            for h in sizes {
                rows.extend(
                    super::sample_with_stores(
                        service.clone(),
                        std::sync::Arc::new(
                            lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
                        ),
                        "sqlite",
                        scenario,
                        h,
                    )
                    .await
                    .unwrap(),
                );
            }
        }
        eprintln!("HEAP-METER: 12 populations in {:?}", started.elapsed());
        for row in &rows {
            eprintln!(
                "HEAP-METER {} H={} park={} spool_bytes={} seed_ms={:.0} wall_ms={:.0}",
                row.scenario,
                row.h,
                row.park,
                super::counter_text(&row.counts[super::SPOOL_BYTES]),
                row.seed_ns as f64 / 1e6,
                row.wall_ns as f64 / 1e6
            );
        }
        super::laws(&rows).unwrap();
    }
    use std::path::Path;
}
