//! READ-COST-MODEL: fifty dependent observations through send(), with
//! exact owner-commit and fact geometry. Timing is evidence, never a gate.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use lash::tools::{StaticToolExecute, StaticToolProvider, ToolProviderRegistration, ToolReads};
use lash::workflow::document::{Datum, ErrorDatum, Handle};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core_execution::StoreSet;
use lash_sansio::sync::MutexExt as _;
use serde_json::{Value, json};

use crate::Run;
use crate::deploy::Database;
use crate::recorder::{Recorder, RecordingStores, Transaction};
use crate::support::Distribution;

const READS: usize = 50;
const MODEL: &str = "declared-read-benchmark";
const PAYLOADS: [usize; 2] = [32, 16_384];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Recorded,
    Read,
    Projection,
    Mixed,
}
impl Variant {
    const ALL: [Self; 4] = [Self::Recorded, Self::Read, Self::Projection, Self::Mixed];
    fn name(self) -> &'static str {
        match self {
            Self::Recorded => "repeatable",
            Self::Read => "read",
            Self::Projection => "projection",
            Self::Mixed => "read-write-read",
        }
    }
    fn count(self) -> usize {
        if self == Self::Mixed { 2 } else { READS }
    }
    fn recorded(self) -> usize {
        match self {
            Self::Recorded => READS,
            Self::Mixed => 1,
            _ => 0,
        }
    }
}

/// Every adapter uses the same fixed local provider, including the projection
/// control. The previous answer is the next request: reordering, skipping, or
/// losing a value refuses the benchmark. The write observes the first answer.
struct Answers {
    payload: usize,
    calls: Mutex<usize>,
    writes: Mutex<Vec<String>>,
}
impl Answers {
    fn value(&self, ordinal: usize) -> String {
        format!("{ordinal}:{}", "x".repeat(self.payload))
    }
    fn answer(&self, previous: &str) -> String {
        let mut calls = self.calls.lock_recover();
        let expected = if *calls == 0 {
            "seed".to_owned()
        } else {
            self.value(*calls)
        };
        assert_eq!(previous, expected, "each read depends on its predecessor");
        *calls += 1;
        self.value(*calls)
    }
    #[expect(
        clippy::expect_used,
        reason = "the fixture input schema requires a string previous answer"
    )]
    fn read_args(&self, args: &Value) -> String {
        self.answer(args["previous"].as_str().expect("fixture string argument"))
    }
}
struct Adapter(Arc<Answers>);
#[async_trait::async_trait]
impl StaticToolExecute for Adapter {
    #[expect(
        clippy::expect_used,
        reason = "the fixture write schema requires a string previous answer"
    )]
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let value = if call.name() == "bench_write" {
            let previous = call.args["previous"].as_str().expect("write argument");
            assert_eq!(previous, self.0.value(1));
            self.0.writes.lock_recover().push(previous.to_owned());
            previous.to_owned()
        } else {
            self.0.read_args(call.args)
        };
        lash_core::ToolOutcome::ok(json!(value)).into()
    }
}
#[async_trait::async_trait]
impl ToolReads for Adapter {
    async fn read(&self, call: lash::tools::ReadCall<'_>) -> Result<Value, lash::tools::ReadError> {
        Ok(json!(self.0.read_args(call.args)))
    }
}
#[async_trait::async_trait]
impl lash::vm::ProjectionProvider for Adapter {
    fn kind(&self) -> &str {
        "benchmark"
    }
    async fn read(&self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        assert_eq!(handle.id, "fixed");
        let Datum::Text(previous) = request else {
            panic!("projection string request")
        };
        Ok(Datum::Text(self.0.answer(previous)))
    }
}

fn definition(
    name: &str,
    policy: lash::tools::ExecutionPolicy,
) -> Result<lash_core::ToolDefinition> {
    Ok(lash_core::ToolDefinition::raw(
        name,
        name,
        "Fixed local benchmark observation.",
        json!({"type":"object", "properties":{"previous":{"type":"string"}},
            "required":["previous"], "additionalProperties":false}),
        json!({"type":"string"}),
    )?
    .with_execution(Duration::from_secs(30))
    .with_execution_policy(policy)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name)))
}

fn source(python: bool, variant: Variant, payload: usize) -> String {
    let call = if variant == Variant::Projection {
        "projection_read(r)"
    } else if python {
        "await tools_bench_read({\"previous\": r})"
    } else {
        "await tools.bench_read({previous: r})"
    };
    let final_value = format!("{}:{}", variant.count(), "x".repeat(payload));
    let body = if python {
        (if variant == Variant::Mixed {
            format!(
                "r = \"seed\"\nr = {call}\nawait tools_bench_write({{\"previous\": r}})\nr = {call}\n"
            )
        } else {
            format!("r = \"seed\"\nfor i in range({READS}):\n    r = {call}\n")
        }) + &format!("assert r == {}\nprint(len(r))", json!(final_value))
    } else {
        (if variant == Variant::Mixed {
            format!(
                "let r = \"seed\";\nr = {call};\nawait tools.bench_write({{previous: r}});\nr = {call};\n"
            )
        } else {
            format!("let r = \"seed\";\nfor (let i = 0; i < {READS}; i++) {{ r = {call}; }}\n")
        }) + &format!(
            "if (r !== {}) {{ throw new Error(\"bad chain\"); }}\nconsole.log(r.length);",
            json!(final_value)
        )
    };
    let dialect = if python { "python" } else { "typescript" };
    format!("<{dialect}>\n{body}\n</{dialect}>")
}

/// A host-seeded kernel function is the projection handle interface. It is
/// called from each dialect under its normal saved-function calling convention.
/// Seeding is outside the window; no tool is recorded to mint a handle.
fn projection_seed(python: bool) -> Result<lash::rlm::RlmSeed> {
    use lash::workflow::document::parse_document;
    let (params, request) = if python {
        ("previous", "previous")
    } else {
        ("this, args", "args[0]")
    };
    let numbers = if python { "by_spelling" } else { "float" };
    let resource = resource_definition()?.identity()?;
    let document = parse_document(&format!(
        "kernel 1\nnumbers {numbers}\nuse bench.resource = @{resource}\nentry projection_read({params_types}) -> Text\n\
         fn projection_read({params}) {{ let resource = bench.resource() return read(resource, {request}) }}\nmain {{}}",
        params_types = if python {
            "previous: Any"
        } else {
            "this: Any, args: Any"
        },
    ))?;
    let token = (!python).then(|| {
        json!({"tag":"ts.function",
        "before":[Datum::Text("".into()), Datum::Text("projection_read".into()),
            Datum::Float(lash::workflow::document::Float::new(1.0))],
        "after":[Datum::Record(Vec::new())]})
    });
    let saved = json!({"name":"projection_read", "document":document,
        "captures":{},
        "helper_owners":[], "token":token});
    Ok(lash::rlm::RlmSeed {
        functions: serde_json::Map::from_iter([("projection_read".into(), saved)]),
        ..Default::default()
    })
}

type ReadStores = (
    Arc<dyn StoreSet>,
    Option<lash_postgres_store::PostgresStorage>,
);

async fn stores(run: &Run<'_>) -> Result<ReadStores> {
    match &run.database {
        Database::Sqlite(path) => Ok((
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(
                    path,
                    lash_sqlite_store::SqliteSynchronous::Full,
                )
                .await?,
            ),
            None,
        )),
        Database::Postgres(url) => {
            let endpoints = lash_postgres_store::PostgresEndpoints::from_url(url)?;
            let storage = lash_postgres_store::PostgresStorage::connect(
                &endpoints,
                &lash_postgres_store::PostgresHostConfig::default(),
                Default::default(),
            )
            .await?;
            lash_perf::perf_support::require_durable_postgres(storage.pool()).await?;
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
            );
            Ok((Arc::new(stores), Some(storage)))
        }
    }
}

fn owner_transactions(transactions: Vec<Transaction>, actor: &str) -> Vec<Transaction> {
    let mut owned = Vec::new();
    for tx in transactions
        .into_iter()
        .filter(|tx| tx.actor == actor && tx.ok)
    {
        let terminal = tx.label == "turn.commit";
        owned.push(tx);
        if terminal {
            break;
        }
    }
    owned
}
fn facts(transactions: &[Transaction]) -> BTreeMap<String, usize> {
    let mut facts = BTreeMap::new();
    for tx in transactions {
        for (kind, count) in &tx.run_facts {
            *facts.entry(kind.clone()).or_default() += count;
        }
    }
    facts
}

/// READ-COST-MODEL pins the entire owning geometry, not a baseline learned
/// from this invocation. A new label or fact fails even on the first sample.
fn assert_geometry(
    variant: Variant,
    traced: bool,
    payload: usize,
    transactions: &[Transaction],
) -> Result<()> {
    let labels = crate::support::by_label(transactions);
    let actual_facts = facts(transactions);
    let recorded = variant.recorded();
    let mut expected = BTreeMap::from([("model.start", 2), ("turn.admit", 1), ("turn.commit", 1)]);
    if variant != Variant::Projection {
        expected.insert("model.done+cell.snapshot+admit", 1);
    } else if payload == PAYLOADS[1] {
        // ADR 0142: the large projection chain outlasts its first compute
        // slice, so the held model response commits alone. A Read plan
        // already committed it before the first statement.
        expected.insert("model.done", 1);
    }
    let admissions = match variant {
        Variant::Recorded => READS - 1,
        Variant::Mixed => 1,
        _ => 0,
    };
    if admissions > 0 {
        expected.insert("cell.snapshot+admit", admissions);
    }
    let mut expected_facts = BTreeMap::new();
    if recorded > 0 {
        expected.insert("round.outcome", recorded);
        for kind in ["admit", "x_start", "x_outcome"] {
            expected_facts.insert(kind.to_owned(), recorded);
        }
        expected_facts.insert("cell_report".to_owned(), 1);
        if traced {
            expected.insert("round.traced", recorded);
            expected_facts.insert("decide".to_owned(), recorded);
        }
    }
    ensure!(
        labels == expected,
        "READ-COST-MODEL {variant:?}: expected {expected:?}, got {labels:?}"
    );
    ensure!(
        actual_facts == expected_facts,
        "READ-COST-MODEL {variant:?}: expected {expected_facts:?}, got {actual_facts:?}"
    );
    Ok(())
}

/// Pure fixture factory: the host projection still answers through the
/// production read exchange. This function does no I/O and creates one fixed
/// handle; no shipped helper or kernel-library definition is changed.
fn resource_definition() -> Result<lash::workflow::document::FunctionDefinition> {
    Ok(lash::workflow::document::parse_definition(
        "function bench.resource() -> Handle(\"benchmark\")\nkernel 1\ncharge 1\nnative\n",
    )?)
}
struct Resource;
impl lash::workflow::document::NativeFunction for Resource {
    fn call(
        &self,
        call: lash::workflow::document::NativeCall<'_>,
    ) -> Result<lash::workflow::document::Value, lash::workflow::document::NativeError> {
        call.heap.reserve(1, 32)?;
        Ok(lash::workflow::document::Value::Handle(Arc::new(Handle {
            kind: "benchmark".into(),
            id: "fixed".into(),
        })))
    }
}
fn embedding(
    tuning: &lash::vm::WorkerTuning,
) -> Result<lash_vm_worker::Embedding, lash_vm_worker::EmbedError> {
    let mut embedder = lash_vm_worker::Embedder::kernel()?
        .dialect(lash_vm_worker::typescript(tuning))?
        .dialect(lash_vm_worker::python())?;
    embedder
        .registry()
        .register(
            resource_definition()
                .map_err(|e| lash_vm_worker::EmbedError::Registry(e.to_string()))?,
            Some(Arc::new(Resource)),
        )
        .map_err(|e| lash_vm_worker::EmbedError::Registry(e.to_string()))?;
    embedder.finish()
}
pub fn worker_entry() -> Result<bool, lash::vm::PoolError> {
    lash_vm_worker::worker_entry(&embedding)
}

pub async fn run(run: &Run<'_>, populations: &[String]) -> Result<()> {
    ensure!(
        run.samples > 0 && run.nodes == 1,
        "read matrix needs samples and one node"
    );
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.entry = lash::vm::WorkerEntry::reexec()?;
    let workers = lash::vm::WorkerService::new(config);
    let mut selected = 0;
    for python in [false, true] {
        for traced in [false, true] {
            for payload in PAYLOADS {
                for variant in Variant::ALL {
                    let dialect = if python { "python" } else { "typescript" };
                    let id = format!("{dialect}-{}-{payload}-{traced}", variant.name());
                    if !populations.is_empty() && !populations.contains(&id) {
                        continue;
                    }
                    selected += 1;
                    measure(run, python, traced, payload, variant, workers.clone()).await?;
                }
            }
        }
    }
    ensure!(
        populations.is_empty() || selected == populations.len(),
        "unknown or duplicate read population"
    );
    Ok(())
}

async fn measure(
    run: &Run<'_>,
    python: bool,
    traced: bool,
    payload: usize,
    variant: Variant,
    workers: lash::vm::WorkerService,
) -> Result<()> {
    measure_with_stores(
        run,
        python,
        traced,
        payload,
        variant,
        workers,
        stores(run).await?,
    )
    .await
}

async fn measure_with_stores(
    run: &Run<'_>,
    python: bool,
    traced: bool,
    payload: usize,
    variant: Variant,
    workers: lash::vm::WorkerService,
    (inner, storage): ReadStores,
) -> Result<()> {
    let dialect = if python { "python" } else { "typescript" };
    let id = format!("{dialect}-{}-{payload}-{traced}", variant.name());
    eprintln!(
        "read population {id}, {} fresh-session samples",
        run.samples
    );
    let recorder = Arc::new(Recorder::default());
    let recorded = Arc::new(RecordingStores::new(
        inner,
        &id,
        recorder.clone(),
        if storage.is_some() {
            "postgres"
        } else {
            "sqlite"
        },
    ));
    let answers = Arc::new(Answers {
        payload,
        calls: Mutex::new(0),
        writes: Mutex::new(Vec::new()),
    });
    let backend = lash::durable::DurableBackendBuilder::new(recorded)
        .projection_provider(Arc::new(Adapter(answers.clone())))
        .build()?;
    let pool = workers.pool()?; // warm before measurement, reused by all samples
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(100_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(128))
            .build(),
        if python {
            lash::rlm::CellDialect::python()
        } else {
            lash::rlm::CellDialect::typescript()
        },
    )
    .with_worker_service(workers);
    let policy = if variant == Variant::Recorded {
        lash::tools::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(1).context("one attempt")?,
            0,
            0,
        )
    } else {
        lash::tools::ExecutionPolicy::Read
    };
    let tools = Arc::new(StaticToolProvider::new(
        vec![
            definition("bench_read", policy)?,
            definition("bench_write", lash::tools::ExecutionPolicy::Once)?,
        ],
        Adapter(answers.clone()),
    ));
    let code = source(python, variant, payload);
    let model = lash_core::testing::TestProvider::builder()
        .kind(MODEL)
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let code = code.clone();
            async move {
                let answered = request
                    .messages
                    .iter()
                    .any(|m| m.role == lash_core::llm::types::LlmRole::Assistant);
                Ok(lash_core::llm::types::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text: if answered { "Done.".into() } else { code },
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let mut builder = lash::LashCore::rlm_builder(backend, factory)
        .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("read-benchmark"),
            lash::plugins::PluginSpec::new().with_tool_provider_registration(
                lash::tool_provider_key!("observations"),
                ToolProviderRegistration::new(tools).with_reads(Arc::new(Adapter(answers.clone()))),
            ),
        )))
        .serve_sessions(true)
        .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            model,
            lash_core::LlmProfileMetadata::builder(MODEL)
                .cache_retention(lash_core::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()?,
        );
    let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(opentelemetry_sdk::trace::InMemorySpanExporter::default())
        .build();
    let meters = opentelemetry_sdk::metrics::SdkMeterProvider::default();
    if traced {
        builder = builder.telemetry(lash::tracing::OtelTelemetry::new(
            &tracer,
            &meters,
            lash::tracing::OtelOptions::standard(),
        ));
    }
    let core = builder.build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new(id.clone()),
        lash::persistence::LeaseIncarnationId::new(format!("{}", std::process::id())),
    ))?;
    let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
    let mut latencies = Vec::new();
    let mut plans = Vec::new();
    for sample in 0..run.samples {
        *answers.calls.lock_recover() = 0;
        answers.writes.lock_recover().clear();
        let session_id = lash::SessionId::try_from(format!("{id}-{sample}"))?;
        let creation = lash::SessionCreation::root(
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(64),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        );
        core.session(session_id.clone()).create(creation).await?;
        let session = core.session(session_id.clone()).open().await?;
        if variant == Variant::Projection {
            let seed = projection_seed(python)?;
            let admin = session.admin();
            admin
                .protocol()
                .apply_session_extension(
                    lash_core::ProtocolSessionExtension::new(move |fleet| {
                        lash::rlm::rlm_seed_initial_nodes(seed.clone(), fleet)
                    }),
                    "projection-seed",
                )
                .await?
                .settle_with(&admin.commands(), lash::testing::admin_fixture_outcome)
                .await?;
        }
        cleanup.settle().await?;
        let from = recorder.now_us();
        let ipc_before = pool.measurements().counters;
        let started = Instant::now();
        let handle = session
            .send(lash::TurnInput::text("Run the dependent observations."))
            .await?;
        let outcome = tokio::time::timeout(Duration::from_secs(120), handle.outcome()).await??;
        let elapsed_us = u64::try_from(started.elapsed().as_micros())?;
        let ipc_after = pool.measurements().counters;
        let transactions = owner_transactions(
            recorder.transactions_since(from),
            &lash_durable::ActorKey::session(session_id.as_str())?.to_string(),
        );
        let labels = crate::support::by_label(&transactions);
        let facts = facts(&transactions);
        let plan = if matches!(variant, Variant::Read | Variant::Mixed) {
            transactions
                .iter()
                .find(|tx| tx.label == "model.done+cell.snapshot+admit")
        } else {
            None
        };
        run.report.write("read_sample", &json!({
            "store":run.database.dialect(), "dialect":dialect, "traced":traced,
            "payload_bytes":payload, "variant":variant.name(), "sample":sample,
            "reads":variant.count(), "elapsed_us":elapsed_us, "owner_commits":labels,
            "run_facts":facts, "transactions":transactions, "plan_commit_us":plan.map(|tx| tx.micros),
            "staged_graph_bytes":transactions.iter().map(|tx| tx.graph_bytes).sum::<usize>(),
            "installed_graph_bytes":transactions.iter().map(|tx| tx.installed_bytes).sum::<usize>(),
            "ipc_sent_frames":ipc_after.ipc_sent_messages - ipc_before.ipc_sent_messages,
            "ipc_received_frames":ipc_after.ipc_received_messages - ipc_before.ipc_received_messages,
            "ipc_sent_bytes":ipc_after.ipc_sent_bytes - ipc_before.ipc_sent_bytes,
            "ipc_received_bytes":ipc_after.ipc_received_bytes - ipc_before.ipc_received_bytes,
            "host_load":std::fs::read_to_string("/proc/loadavg").ok(),
        }))?;
        let output = outcome
            .output()
            .with_context(|| format!("{id}: turn has no output: {:?}", outcome.status()))?;
        ensure!(
            output.is_success(),
            "{id}: {:?}, {:?}",
            output.status(),
            output.result.errors
        );
        ensure!(
            *answers.calls.lock_recover() == variant.count(),
            "{id}: incomplete read chain"
        );
        ensure!(
            answers.writes.lock_recover().len() == usize::from(variant == Variant::Mixed),
            "{id}: mixed write count"
        );
        assert_geometry(variant, traced, payload, &transactions)?;
        eprintln!("READ_COUNTS {id}: commits={labels:?}; facts={facts:?}");
        latencies.push(elapsed_us);
        if let Some(plan) = plan {
            plans.push(plan.micros);
        }
        let administration = core.session_administration().await;
        lash::LashCore::delete_session(administration.delete_context(session_id.as_str())?).await?;
        tokio::time::timeout(
            Duration::from_secs(60),
            core.await_session_deletion(&session_id),
        )
        .await??;
    }
    run.report.write(
        "read_distribution",
        &json!({"store":run.database.dialect(),
            "dialect":dialect,"traced":traced,"payload_bytes":payload,"variant":variant.name(),
            "samples_us":latencies,"latency":Distribution::of(&latencies)?,
            "plan_commit_samples_us":plans,
            "plan_commit":if plans.is_empty() { None } else { Some(Distribution::of(&plans)?) },
        }),
    )?;
    core.shutdown().await?;
    drop(cleanup);
    if let Some(storage) = storage {
        storage.pool().close().await;
    }
    tracer.shutdown()?;
    meters.shutdown()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// READ-COST-MODEL: the maintained production-facade matrix asserts the
    /// exact per-call commit/fact counts on SQLite memory in both dialects.
    /// Worker preparation is shared, as in the command-line matrix; every
    /// population still has its own fresh store and owning transactions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fifty_dependent_reads_obey_the_cost_model() {
        let fixture = tempfile::tempdir().unwrap();
        let report = crate::support::Report::open(&fixture.path().join("reads.jsonl")).unwrap();
        let worker =
            std::path::PathBuf::from(std::env::var_os("LASH_VM_WORKER").expect("worker runfile"));
        let run = crate::Run {
            report: &report,
            database: crate::deploy::Database::Sqlite(fixture.path().join("reads.db")),
            nodes: 1,
            samples: 1,
            pool: crate::deploy::PoolSize { min: 1, max: 4 },
            worker: &worker,
            prior_turns: 0,
            cell_parks: 0,
            value_bytes: 0,
        };
        let benchmark = std::path::PathBuf::from(
            std::env::var_os("LASH_READ_BENCH_WORKER").expect("benchmark worker runfile"),
        );
        let workers = lash::vm::WorkerService::subprocess(&benchmark);
        let started = std::time::Instant::now();
        for python in [false, true] {
            for traced in [false, true] {
                for payload in super::PAYLOADS {
                    for variant in super::Variant::ALL {
                        super::measure_with_stores(
                            &run,
                            python,
                            traced,
                            payload,
                            variant,
                            workers.clone(),
                            (
                                std::sync::Arc::new(
                                    lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
                                ),
                                None,
                            ),
                        )
                        .await
                        .unwrap();
                    }
                }
            }
        }
        eprintln!("READ-COST-MODEL: 32 populations in {:?}", started.elapsed());
    }
}
