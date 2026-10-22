//! One code cell through the code-cell path a host runs: a served core on
//! a SQLite file store, the RLM cell protocol, the VM worker subprocess and
//! the host's cell bounds. The model answers the turn's first call with the
//! cell `--source` and its second with text; the cell's feedback is printed.
//!
//! The defaults are Figments' cell bounds (FIG-6886): 60 s worker CPU, 60 s
//! compute, a 10 billion instruction budget with the pool's charge ceiling
//! raised to it, 64 MiB of memory and a 1 MiB / 512-node commit budget.
//! Wall time is the turn's, from send to settled output; run the process
//! under `perf stat` and `/usr/bin/time -v` for instructions and CPU, which
//! cover the host and its worker.
use anyhow::{Context as _, Result};
use clap::{Parser, ValueEnum};
use lash_core::LlmOutputPart;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_protocol_types::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODEL: &str = "cellpath-model";

#[derive(Clone, Copy, ValueEnum)]
enum Dialect {
    Typescript,
    Python,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)]
    dialect: Dialect,
    /// The cell's source.
    #[arg(long)]
    source: PathBuf,
    /// A SQLite database file that does not exist yet.
    #[arg(long)]
    database: PathBuf,
    #[arg(long)]
    vm_worker: PathBuf,
    #[arg(long, default_value_t = 10_000_000_000)]
    instructions: u64,
    #[arg(long, default_value_t = 64)]
    memory_mib: u64,
    #[arg(long, default_value_t = 60)]
    cpu_secs: u64,
    #[arg(long, default_value_t = 60)]
    compute_secs: u64,
    #[arg(long, default_value_t = 1_048_576)]
    commit_bytes: usize,
    #[arg(long, default_value_t = 512)]
    commit_nodes: usize,
}

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(Args::parse()))
}

async fn run(args: Args) -> Result<()> {
    let source = std::fs::read_to_string(&args.source)
        .with_context(|| format!("read {}", args.source.display()))?;
    let (tag, dialect) = match args.dialect {
        Dialect::Typescript => ("typescript", lash::rlm::CellDialect::typescript()),
        Dialect::Python => ("python", lash::rlm::CellDialect::python()),
    };
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            &args.database,
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .with_context(|| format!("open {}", args.database.display()))?,
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        lash_execution_contracts::backend::StoreSet::process_env_store(stores.as_ref()).as_ref(),
    )
    .await;
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(lash::durable::DurableSettings {
            max_ingress_bytes: Some(const { std::num::NonZeroU64::new(64 * 1024).unwrap() }),
            owner_retention: Some(lash::durable::OwnerRetentionSettings {
                idle: std::time::Duration::from_secs(60),
                max_resident_owners: const { std::num::NonZeroUsize::new(256).unwrap() },
            }),
            ..lash::durable::DurableSettings::standard()
        })
        .build()
        .map_err(|error| anyhow::anyhow!("the backend: {error}"))?;
    let mut workers = lash::vm::WorkerService::subprocess(&args.vm_worker)
        .config()
        .clone();
    workers.deadlines = lash::vm::WorkerDeadlines {
        cumulative_cpu: Duration::from_secs(args.cpu_secs),
        compute: Duration::from_secs(args.compute_secs),
        execution_wall: Duration::from_secs(600),
        ..lash::vm::WorkerDeadlines::standard()
    };
    workers.run_bounds.charge_limit = args.instructions;
    let feedback = Arc::new(Mutex::new(String::new()));
    let cell = format!("<{tag}>\n{source}\n</{tag}>");
    let core = lash::LashCore::rlm_builder(
        backend,
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(args.instructions))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(args.memory_mib))
                .build(),
            dialect,
        )
        .with_worker_service(lash::vm::WorkerService::new(workers)),
    )
    .commit_budget(lash::CommitBudget::bounded(
        args.commit_bytes,
        args.commit_nodes,
    ))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(model(cell, Arc::clone(&feedback)), metadata()?)
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("cellpath"),
        lash::persistence::LeaseIncarnationId::new("cellpath-boot"),
    ))
    .map_err(|error| anyhow::anyhow!("build the core: {error}"))?;
    let session = core
        .session(lash_vocabulary::identity::SessionId::try_from(
            "cellpath".to_owned(),
        )?)
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(64),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .map_err(|error| anyhow::anyhow!("create the session: {error}"))?;
    let began = Instant::now();
    let output = session
        .send(lash::TurnInput::text("run the cell"))
        .output()
        .await
        .map_err(|error| anyhow::anyhow!("the turn: {error}"))?;
    let elapsed = began.elapsed();
    core.shutdown()
        .await
        .map_err(|error| anyhow::anyhow!("shut down: {error}"))?;
    let feedback = feedback.lock().map(|text| text.clone()).unwrap_or_default();
    println!(
        "{}",
        serde_json::json!({
            "source": args.source.display().to_string(),
            "turn_ms": elapsed.as_secs_f64() * 1000.0,
            "status": format!("{:?}", output.status()),
            "answer": format!("{:?}", output.outcome).chars().take(600).collect::<String>(),
            "feedback": feedback.chars().take(2000).collect::<String>(),
        })
    );
    Ok(())
}

fn metadata() -> Result<lash_core::LlmProfileMetadata> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(1_000_000)
        .build()
        .map_err(|error| anyhow::anyhow!("the model's metadata: {error}"))
}

/// The model: the turn's first call is answered with `cell`; a later call,
/// which carries the cell's feedback, with text, keeping that feedback.
fn model(cell: String, feedback: Arc<Mutex<String>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("cellpath")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let cell = cell.clone();
            let feedback = Arc::clone(&feedback);
            async move {
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                let text = if answered {
                    if let (Ok(mut kept), Some(last)) = (feedback.lock(), request.messages.last()) {
                        *kept = serde_json::to_string(last).unwrap_or_default();
                    }
                    "done".to_owned()
                } else {
                    cell
                };
                Ok(streamed(&request, &text))
            }
        })
        .build()
        .into_handle()
}

fn streamed(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        }));
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}
