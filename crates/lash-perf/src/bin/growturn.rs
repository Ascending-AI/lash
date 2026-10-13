//! One durable turn grows across two owner handoffs and closes.
use anyhow::{Context as _, Result, ensure};
use clap::{Parser, ValueEnum};
use lash::tools::ToolDefinitionBindingExt as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::llm::types::{
    LlmContentBlock, LlmRequest, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core::provider::{ProviderOptions, ProviderReliability};
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[path = "growturn/measure.rs"]
mod measure;
#[path = "growturn/native.rs"]
mod native;

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOCATOR: &lash_perf::DhatStatsAllocator = &lash_perf::GLOBAL_ALLOCATOR;

const MODEL: &str = "growturn-script";
const SESSION: &str = "growturn-session";
const TURN: &str = "growturn-turn";
const LIMIT: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Phase {
    Start,
    Resume,
    /// Grow and durably delete K sessions in one core, without handoffs.
    Sessions,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)]
    phase: Phase,
    /// Total model/tool rounds in the single turn, including both handoffs.
    #[arg(long, default_value_t = 10)]
    rounds: usize,
    /// Number of sequential sessions for --phase sessions.
    #[arg(long, default_value_t = 1)]
    sessions: usize,
    /// Close facade handles but retain durable sessions in --phase sessions.
    #[arg(long)]
    retain_sessions: bool,
    #[arg(long, default_value_t = 1024)]
    payload_bytes: usize,
    #[arg(long, default_value_t = 10)]
    checkpoint_every: usize,
    /// Start refuses an existing database; resume requires one.
    #[arg(long)]
    database: PathBuf,
    #[arg(long)]
    vm_worker: PathBuf,
    /// Append both processes' samples to this JSONL file.
    #[arg(long)]
    out: PathBuf,
    #[command(flatten)]
    profile: lash_perf::perf_support::dhat::ProfileArgs,
    /// Start host profiling at this completed round; earlier allocations
    /// remain outside the profile. Zero profiles the full host lifetime.
    #[arg(long, default_value_t = 0)]
    profile_from_round: usize,
    /// End the host heap window at durable close, before dropping the core.
    #[arg(long)]
    profile_at_close: bool,
    /// A dhat worker's finite bootstrap-to-first-reset windows, per PID.
    #[arg(long)]
    worker_profile_dir: Option<PathBuf>,
    /// Linux/glibc allocator and raw /proc snapshots, before the store scanner.
    #[arg(long)]
    native_memory_dir: Option<PathBuf>,
    /// Experimental host policy: trim glibc after close and core shutdown.
    #[arg(long, requires = "native_memory_dir")]
    trim_at_idle: bool,
}

fn metadata() -> Result<lash::LlmProfileMetadata> {
    Ok(lash::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(100_000_000)
        .build()?)
}

fn provider(args: &Args, meter: Arc<measure::Meter>) -> lash_core::provider::ProviderHandle {
    let rounds = args.rounds;
    let every = args.checkpoint_every;
    let handoffs = !matches!(args.phase, Phase::Sessions);
    lash_core::testing::TestProvider::builder().kind("growturn-deterministic")
        .requires_streaming(true)
        .options(ProviderOptions { reliability: ProviderReliability::disabled(), ..Default::default() })
        .complete(move |request: LlmRequest| {
            let meter = Arc::clone(&meter);
            async move {
                // Derive progress from recorded assistant cells, never a live
                // provider counter: this survives both kinds of owner handoff.
                let done = request.messages.iter().filter(|m| m.role == LlmRole::Assistant)
                    .flat_map(|m| m.blocks.iter()).filter_map(|b| match b {
                        LlmContentBlock::Text { text, .. } => text.split("/*growturn:").nth(1)
                            .and_then(|s| s.split("*/").next()).and_then(|s| s.parse::<usize>().ok()),
                        _ => None,
                    }).max().unwrap_or(0);
                if done > 0 && !request.messages.iter().filter(|m| m.role != LlmRole::Assistant)
                    .flat_map(|m| m.blocks.iter()).any(|b| matches!(b, LlmContentBlock::Text { text, .. } if text.contains(&format!("grow-done:{done}:")))) {
                    return Err(lash::provider::LlmTransportError::new("cell did not emit its successful payload receipt"));
                }
                meter.model_request(&request);
                meter.start_profile(done).map_err(|e| lash::provider::LlmTransportError::new(e.to_string()))?;
                if done.is_multiple_of(every) || done == rounds || done == rounds / 2
                    || done == rounds * 3 / 4 || done == meter.profile_from_round {
                    let phase = if handoffs && done == rounds / 2 { "resumed-in-process" }
                        else if handoffs && done == rounds * 3 / 4 { "resumed-process-restart" }
                        else { "round" };
                    meter.sample(phase, done).map_err(|e| lash::provider::LlmTransportError::new(e.to_string()))?;
                }
                let text = if done == rounds { "growing turn complete".to_owned() } else {
                    let next = done + 1;
                    let park = handoffs && (next == rounds / 2 || next == rounds * 3 / 4);
                    format!("<typescript>\n/*growturn:{next}*/\nconst payload = await tools.grow_payload({{round:{next}}});\nif (payload.length !== {bytes}) throw new Error(\"wrong payload size\");\nconsole.log(\"grow-done:{next}:\");\nconsole.log(payload);\n{}\n</typescript>",
                        if park { "await tools.grow_wait({});" } else { "" }, bytes = meter.bytes)
                };
                if let Some(stream) = &request.stream_events {
                    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: StreamBlockIdentity::new("text:0", 0), text: text.clone(),
                    }));
                }
                Ok(lash::provider::LlmResponse { parts: vec![lash::direct::LlmOutputPart::Text { text, response_meta: None }], ..Default::default() })
            }
        }).build().into_handle()
}

struct Tools {
    bytes: usize,
    meter: Arc<measure::Meter>,
}
#[async_trait::async_trait]
impl StaticToolExecute for Tools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "grow_wait" {
            return lash::tools::ToolOutcome::pending(lash::tools::PendingCompletion::new()).into();
        }
        let Some(round) = call.args["round"].as_f64().filter(|n| {
            n.is_finite() && n.fract() == 0.0 && *n >= 1.0 && *n <= self.meter.rounds as f64
        }) else {
            return lash::tools::ToolOutcome::err(serde_json::json!("invalid round ordinal"))
                .into();
        };
        let round = round as usize;
        // Observe the worker while it is awaiting host projection, as well as
        // at the following model checkpoint (when it may have reset).
        if round.is_multiple_of(self.meter.every)
            && let Err(error) = self.meter.sample("worker-await", round - 1)
        {
            return lash::tools::ToolOutcome::err(serde_json::json!(error.to_string())).into();
        }
        let prefix = format!("grow-payload:{round:08}:");
        let payload = prefix.clone() + &"x".repeat(self.bytes.saturating_sub(prefix.len()));
        lash::tools::ToolOutcome::ok(serde_json::json!(payload)).into()
    }
}

fn tools(args: &Args, meter: Arc<measure::Meter>) -> Result<Arc<dyn lash::tools::ToolProvider>> {
    let payload = lash::tools::ToolDefinition::raw("grow_payload", "grow_payload", "Return one sized payload.",
        serde_json::json!({"type":"object","properties":{"round":{"type":"integer"}},"required":["round"],"additionalProperties":false}),
        serde_json::json!({"type":"string"}))?
        .with_execution(Duration::from_secs(120)).with_execution_policy(lash::tools::ExecutionPolicy::Once)
        .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "grow_payload"));
    let wait = lash::tools::ToolDefinition::raw(
        "grow_wait",
        "grow_wait",
        "Park until explicit completion.",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )?
    .with_execution(Duration::from_secs(120))
    .with_declaration(
        lash::tools::ToolDeclaration::deferring(),
        Some(lash::tools::ParkBound::Within(LIMIT)),
    )?
    .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "grow_wait"));
    Ok(Arc::new(StaticToolProvider::new(
        vec![payload, wait],
        Tools {
            bytes: args.payload_bytes,
            meter,
        },
    )))
}

async fn boot(args: &Args, meter: Arc<measure::Meter>, owner: &str) -> Result<lash::LashCore> {
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &args.database,
        lash_sqlite_store::SqliteSynchronous::Full,
    )
    .await?;
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores)).build()?;
    let mut config = lash::vm::WorkerService::subprocess(&args.vm_worker)
        .config()
        .clone();
    if let Some(dir) = &args.worker_profile_dir {
        config
            .entry
            .args
            .extend(["--heap-profile-dir".into(), dir.display().to_string()]);
    }
    Ok(lash::LashCore::rlm_builder(
        backend,
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(100_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(512))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(lash::vm::WorkerService::new(config)),
    )
    .commit_budget(lash::CommitBudget::bounded(256 * 1024 * 1024, 1_000_000))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(provider(args, Arc::clone(&meter)), metadata()?)
    .tools(
        lash::tool_provider_key!("growturn-tools"),
        tools(args, meter)?,
    )
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new(owner),
        lash::persistence::LeaseIncarnationId::new(format!("{owner}-{}", std::process::id())),
    ))?)
}

fn park_identity(call: &lash::admin::ParkedCall) -> serde_json::Value {
    serde_json::json!({"key":call.key.as_str(), "owner":format!("{:?}", call.owner),
        "call":call.call_id.as_str(), "tool":call.tool_id.as_str(), "deadline_ms":call.deadline.map(|d| d.0)})
}

fn session_id() -> Result<lash::SessionId> {
    Ok(lash::SessionId::try_from(SESSION.to_owned())?)
}

async fn parked(core: &lash::LashCore) -> Result<lash::admin::ParkedCall> {
    tokio::time::timeout(LIMIT, async {
        loop {
            let calls = core
                .completions()
                .parked(lash::admin::CallOwner::Session(session_id()?))
                .await?;
            if let Some(call) = calls.into_iter().next() {
                return anyhow::Ok(call);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("turn never reached its deferred call")?
}
async fn resolve(core: &lash::LashCore, call: &lash::admin::ParkedCall) -> Result<()> {
    ensure!(
        matches!(
            core.completions()
                .resolve(
                    call.key.as_str(),
                    lash::Resolution::Ok(serde_json::json!({}))
                )
                .await?,
            lash::durable::ResolveAnswer::Resolved
        ),
        "park resolution refused"
    );
    Ok(())
}

async fn run(args: &Args, meter: Arc<measure::Meter>) -> Result<()> {
    let mut core = boot(args, Arc::clone(&meter), "growturn-first").await?;
    meter.sample("boot", 0)?;
    match args.phase {
        Phase::Sessions => {
            for ordinal in 1..=args.sessions {
                meter.opened();
                let id = lash::SessionId::try_from(format!("growturn-session-{ordinal}"))?;
                let session = core
                    .session(id.clone())
                    .create(lash::SessionCreation::root(
                        lash::SessionSpec::new(
                            MODEL,
                            lash::TurnBudget::Unbounded,
                            lash::MaxToolCalls::new(args.rounds * 2 + 10),
                        )
                        .no_progress_budget(lash::NoProgressBudget::bounded(12)),
                    ))
                    .await?;
                meter.sample(&format!("opened-{ordinal}"), 0)?;
                let handle = session
                    .send(lash::TurnInput::text("measure closed session retention"))
                    .await?;
                let output = tokio::time::timeout(LIMIT, handle.output()).await??;
                ensure!(
                    matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
                    "session {ordinal} failed: {:?}",
                    output.result.outcome
                );
                drop(output);
                meter.sample(&format!("completed-{ordinal}"), args.rounds)?;
                if args.retain_sessions {
                    drop(session);
                    let live = core.session(id.clone()).open().await?;
                    live.close()
                        .await
                        .map_err(|refused| anyhow::anyhow!("{refused}"))?;
                    // Match the shutdown endpoint's documented teardown delay:
                    // the engine finishes the idle pass after publishing the answer.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    meter.sample(&format!("retained-{ordinal}"), args.rounds)?;
                    if args.trim_at_idle {
                        native::trim()?;
                        meter.sample(&format!("retained-{ordinal}-trimmed"), args.rounds)?;
                    }
                    continue;
                }
                drop(session);
                let administration = core.session_administration().await;
                lash::LashCore::delete_session(administration.delete_context(id.as_str())?).await?;
                drop(administration);
                ensure!(
                    tokio::time::timeout(LIMIT, core.await_session_deletion(&id)).await??
                        == lash::SessionDeleteCompletion::Deleted,
                    "session {ordinal} close did not publish its tombstone"
                );
                meter.closed();
                meter.sample(&format!("closed-{ordinal}"), args.rounds)?;
                if args.trim_at_idle {
                    native::trim()?;
                    meter.sample(&format!("closed-{ordinal}-trimmed"), args.rounds)?;
                }
            }
            if args.profile_at_close {
                meter.finish_profile(true)?;
            }
        }
        Phase::Start => {
            let session = core
                .session(session_id()?)
                .create(lash::SessionCreation::root(
                    lash::SessionSpec::new(
                        MODEL,
                        lash::TurnBudget::Unbounded,
                        lash::MaxToolCalls::new(args.rounds * 2 + 10),
                    )
                    .no_progress_budget(lash::NoProgressBudget::bounded(12)),
                ))
                .await?;
            let handle = session
                .send(lash::TurnInput::text("measure one growing turn"))
                .id(lash::TurnId::try_from(TURN.to_owned())?)
                .await?;
            // Handles observe; the durable engine owns the entire drive.
            drop(handle);
            drop(session);
            let first = parked(&core).await?;
            meter.sample("park-in-process", args.rounds / 2)?;
            core.shutdown().await?;
            drop(core);
            meter.sample("runtime-dropped", args.rounds / 2)?;
            core = boot(args, Arc::clone(&meter), "growturn-second").await?;
            let restored = parked(&core).await?;
            ensure!(
                first == restored,
                "in-process handoff changed the pinned park"
            );
            meter.sample("resume-in-process", args.rounds / 2)?;
            resolve(&core, &restored).await?;
            let second = parked(&core).await?;
            ensure!(second.key != first.key, "did not reach the second park");
            meter.sample("park-process-restart", args.rounds * 3 / 4)?;
            std::fs::write(
                args.database.with_extension("park.json"),
                serde_json::to_vec(
                    &serde_json::json!({"park":park_identity(&second),"rounds":args.rounds,"payload_bytes":args.payload_bytes}),
                )?,
            )?;
        }
        Phase::Resume => {
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(args.database.with_extension("park.json"))?)?;
            let restored = parked(&core).await?;
            ensure!(
                saved
                    == serde_json::json!({"park":park_identity(&restored),"rounds":args.rounds,"payload_bytes":args.payload_bytes}),
                "process restart changed the pinned park"
            );
            meter.sample("resume-process-restart", args.rounds * 3 / 4)?;
            let session = core.session(session_id()?).durable().await?;
            let handle =
                session.attach(session.input_id(&lash::TurnId::try_from(TURN.to_owned())?));
            resolve(&core, &restored).await?;
            let output = tokio::time::timeout(LIMIT, handle.output()).await??;
            ensure!(
                matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
                "turn failed: {:?}",
                output.result.outcome
            );
            drop(output);
            drop(session);
            meter.sample("completed", args.rounds)?;
            // This queues the production close mail; the engine owns closing.
            let administration = core.session_administration().await;
            lash::LashCore::delete_session(administration.delete_context(SESSION)?).await?;
            drop(administration);
            ensure!(
                tokio::time::timeout(LIMIT, core.await_session_deletion(&session_id()?)).await??
                    == lash::SessionDeleteCompletion::Deleted,
                "session close did not publish its tombstone"
            );
            meter.closed();
            meter.sample("closed", args.rounds)?;
            if args.trim_at_idle {
                native::trim()?;
                meter.sample("closed-trimmed", args.rounds)?;
            }
            if args.profile_at_close {
                meter.finish_profile(true)?;
            }
        }
    }
    core.shutdown().await?;
    drop(core);
    // Let detached background teardown release its ownership. This is a
    // documented endpoint delay, not an assertion of allocator reclamation.
    tokio::time::sleep(Duration::from_millis(250)).await;
    meter.sample(
        "shutdown",
        match args.phase {
            Phase::Start => args.rounds * 3 / 4,
            Phase::Resume | Phase::Sessions => args.rounds,
        },
    )?;
    if args.trim_at_idle {
        native::trim()?;
        meter.sample(
            "shutdown-trimmed",
            match args.phase {
                Phase::Start => args.rounds * 3 / 4,
                Phase::Resume | Phase::Sessions => args.rounds,
            },
        )?;
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.rounds >= 4
            && args.payload_bytes >= 32
            && args.checkpoint_every > 0
            && args.sessions > 0,
        "rounds >= 4, payload >= 32, checkpoint interval > 0 and sessions > 0 required"
    );
    ensure!(
        args.database.exists() == matches!(args.phase, Phase::Resume),
        "start requires a fresh database; resume requires its parked database"
    );
    ensure!(
        args.profile_from_round <= args.rounds,
        "profile start exceeds population"
    );
    let profile = if args.profile_from_round == 0 {
        Some(lash_perf::perf_support::dhat::ProfileWindow::start(
            "growturn-host",
            if args.profile_at_close {
                "process-setup-through-durable-close-before-core-shutdown"
            } else {
                "process-setup-through-core-shutdown-before-runtime-drop"
            },
            &args.profile,
        )?)
    } else {
        None
    };
    let meter = Arc::new(measure::Meter::new(&args)?);
    meter.install_profile(profile);
    meter.sample("process-baseline", 0)?;
    let result = run(&args, Arc::clone(&meter)).await;
    if let Err(error) = &result {
        meter.failure(error)?;
    }
    meter.finish_profile(result.is_ok())?;
    result
}
