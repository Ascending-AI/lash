//! Fixed-history idle populations in the existing growturn instrument.
use super::{Args, LIMIT, MODEL, measure, native};
use anyhow::{Context as _, Result, ensure};
use clap::ValueEnum;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmRole};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct ScriptCounts {
    model_calls: AtomicUsize,
    tool_calls: AtomicUsize,
}
use std::time::Duration;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub(super) enum Mode {
    Held,
    Parked,
    DurableOnly,
    Pending,
}
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub(super) enum Protocol {
    Standard,
    Code,
}
#[derive(clap::Args)]
pub(super) struct Options {
    #[arg(long, value_enum, default_value = "held")]
    idle_mode: Mode,
    #[arg(long, value_enum, default_value = "standard")]
    idle_protocol: Protocol,
    /// Completed turns per session, each with two model calls and one tool round.
    #[arg(long, default_value_t = 20)]
    pub(super) history_turns: usize,
    /// Build this many sessions before each population sample.
    #[arg(long, default_value_t = 25)]
    wave_size: usize,
}

struct Tools {
    bytes: usize,
    counts: Arc<ScriptCounts>,
}
#[async_trait::async_trait]
impl StaticToolExecute for Tools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.counts.tool_calls.fetch_add(1, Ordering::Relaxed);
        if call.name() == "tools.idle_wait" {
            lash::tools::ToolOutcome::pending(lash::tools::PendingCompletion::new()).into()
        } else {
            lash::tools::ToolOutcome::ok(json!("x".repeat(self.bytes))).into()
        }
    }
}
fn tools(bytes: usize, counts: Arc<ScriptCounts>) -> Result<Arc<dyn lash::tools::ToolProvider>> {
    let definition = |name, id, description, output| {
        lash::tools::ToolDefinition::raw(
            name,
            id,
            description,
            json!({"type":"object","properties":{},"additionalProperties":false}),
            output,
        )
    };
    Ok(Arc::new(StaticToolProvider::new(
        vec![
            definition(
                "idle_payload",
                "tools.idle_payload",
                "Return the fixed history payload.",
                json!({"type":"string"}),
            )?
            .with_execution(Duration::from_secs(120))
            .with_execution_policy(lash::tools::ExecutionPolicy::Once),
            definition(
                "idle_wait",
                "tools.idle_wait",
                "Wait for a durable completion.",
                json!({"type":"object"}),
            )?
            .with_execution(Duration::from_secs(120))
            .with_declaration(
                lash::tools::ToolDeclaration::deferring(),
                Some(lash::tools::ParkBound::Within(LIMIT)),
            )?,
        ],
        Tools { bytes, counts },
    )))
}
fn provider(protocol: Protocol, counts: Arc<ScriptCounts>) -> lash_core::provider::ProviderHandle {
    let builder = lash_core::testing::TestProvider::builder()
        .kind("idle-population-script")
        .options(super::ProviderOptions {
            reliability: super::ProviderReliability::disabled(),
            ..Default::default()
        });
    builder.complete(move |request: LlmRequest| {
        let counts = Arc::clone(&counts);
        async move {
            counts.model_calls.fetch_add(1, Ordering::Relaxed);
            let current = request.messages.iter().rposition(|m| {
                m.role == LlmRole::User && m.blocks.iter().any(|b| matches!(b, LlmContentBlock::Text { .. }))
            }).unwrap_or(0);
            let messages = &request.messages[current..];
            let result = messages.iter().flat_map(|m| m.blocks.iter())
                .any(|b| matches!(b, LlmContentBlock::ToolResult { .. }));
            let pending = messages.first().is_some_and(|m| m.blocks.iter().any(|b| {
                matches!(b, LlmContentBlock::Text { text, .. } if text.as_ref() == "idlepop-pending")
            }));
            let part = if result {
                lash::direct::LlmOutputPart::Text {
                    text: "idle population turn complete".into(), response_meta: None,
                }
            } else if protocol == Protocol::Code {
                lash::testing::execute_code(if pending {
                    "await tools.idleWait({});".to_owned()
                } else {
                    "console.log(await tools.idlePayload({}));".to_owned()
                })
            } else {
                lash::direct::LlmOutputPart::ToolCall {
                    call_id: "idlepop-call".into(),
                    tool_name: if pending { "idle_wait" } else { "idle_payload" }.into(),
                    input_json: "{}".into(), replay: None,
                }
            };
            Ok(lash::provider::LlmResponse { parts: vec![part], ..Default::default() })
        }
    }).build().into_handle()
}

async fn boot(
    args: &Args,
    counts: Arc<ScriptCounts>,
) -> Result<(lash::LashCore, Option<lash::vm::WorkerPool>)> {
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &args.database,
        lash_sqlite_store::SqliteSynchronous::Full,
    )
    .await?;
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .config(lash::durable::DurableSettings::standard())
        .build()?;
    let (builder, pool) = if args.idle.idle_protocol == Protocol::Code {
        let service = lash_perf::cell_bounds::subprocess(&args.vm_worker);
        let pool = service.pool()?;
        (
            lash::LashCore::rlm_builder(
                backend,
                lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .instruction_limit(lash_perf::cell_bounds::INSTRUCTION_LIMIT)
                        .memory_limit(lash_perf::cell_bounds::MEMORY_LIMIT)
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(service),
            ),
            Some(pool),
        )
    } else {
        (lash::LashCore::standard_builder(backend), None)
    };
    let core = builder
        .commit_budget(lash::CommitBudget::bounded(256 * 1024 * 1024, 1_000_000))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            provider(args.idle.idle_protocol, Arc::clone(&counts)),
            super::metadata()?,
        )
        .tools(
            lash::tool_provider_key!("idlepop-tools"),
            tools(args.payload_bytes, counts)?,
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("idlepop"),
            lash::persistence::LeaseIncarnationId::new(format!("idlepop-{}", std::process::id())),
        ))?;
    Ok((core, pool))
}
fn id(ordinal: usize) -> Result<lash::SessionId> {
    Ok(lash::SessionId::try_from(format!("idlepop-{ordinal:08}"))?)
}
async fn finished(handle: lash::SendHandle) -> Result<()> {
    let output = tokio::time::timeout(LIMIT, handle.output()).await??;
    ensure!(
        matches!(output.outcome, lash::TurnOutcome::Finished(_)),
        "turn failed: {:?}",
        output.outcome
    );
    Ok(())
}
async fn pending(core: &lash::LashCore, id: lash::SessionId) -> Result<lash::admin::ParkedCall> {
    tokio::time::timeout(LIMIT, async {
        loop {
            let calls = core
                .completions()
                .parked(lash::admin::CallOwner::Session(id.clone()))
                .await?;
            if let Some(call) = calls.into_iter().next() {
                return anyhow::Ok(call);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("pending tool never committed")?
}

// Observe both durable scheduling ownership and weak plugin materializations.
// A dropped facade alone proves neither release boundary.
async fn quiescent(
    core: &lash::LashCore,
    pool: Option<&lash::vm::WorkerPool>,
    count: usize,
    mode: Mode,
) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let mut owned = 0;
            let mut materialized = 0;
            let mut states = BTreeMap::<String, usize>::new();
            let mut clean = true;
            for ordinal in 1..=count {
                let id = id(ordinal)?;
                let actor = core.backend().durable().actor(&lash::durable::ActorKey::session(id.as_str())?).await?
                    .context("population actor disappeared")?;
                owned += usize::from(actor.owner.is_some());
                *states.entry(actor.state.as_str().to_owned()).or_default() += 1;
                clean &= !actor.has_mail && matches!(actor.state, lash::durable::ActorState::Idle | lash::durable::ActorState::Waiting);
                let observation = core.plugin_diagnostics().installed(&lash::plugins::InventoryScope::Owner(lash::RuntimeOwner::Session(id)));
                match observation {
                    lash::plugins::diagnostics::InstalledObservation::Present(_) => materialized += 1,
                    lash::plugins::diagnostics::InstalledObservation::NotMaterialized { .. } => {},
                    other => anyhow::bail!("materialization observation unavailable: {other:?}"),
                }
            }
            let stats = pool.map(lash::vm::WorkerPool::stats);
            let workers_idle = stats.is_none_or(|s| s.workers == s.idle && s.queued_items == 0 && s.queued_bytes == 0 && !s.restart_storm);
            let materializations_released = mode == Mode::Held || materialized == 0;
            if clean && owned == 0 && materializations_released && workers_idle {
                return Ok(json!({"actor_owners":owned,"materialized_session_owners":materialized,
                    "held_facade_handles":if mode == Mode::Held {count} else {0},
                    "actor_states":states,"workers":stats.map_or(0, |s|s.workers),
                    "idle_workers":stats.map_or(0, |s|s.idle),"queued_worker_items":stats.map_or(0, |s|s.queued_items)}));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("population did not reach released actors and idle workers")?
}

fn command(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(args[0])
        .args(&args[1..])
        .output()?;
    ensure!(output.status.success(), "identity command failed: {args:?}");
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn identity(args: &Args) -> Result<Value> {
    let hash = |path: &std::path::Path| -> Result<String> {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path)?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(format!("{:x}", digest.finalize()))
    };
    let executable = std::env::current_exe()?;
    Ok(
        json!({"host":command(&["hostname"])? ,"kernel":command(&["uname","-srmo"] )?,
        "cpu":std::fs::read_to_string("/proc/cpuinfo")?.lines().find(|l| l.starts_with("model name")).unwrap_or("unavailable"),
        "source_commit":command(&["git","rev-parse","HEAD"] )?,
        "source_dirty":!command(&["git","status","--porcelain"] )?.is_empty(),
        "source_diff_sha256":format!("{:x}", Sha256::digest(command(&["git","diff","HEAD"] )?.as_bytes())),
        "executable":executable,"executable_sha256":hash(&executable)?,
        "worker_sha256":if args.idle.idle_protocol == Protocol::Code { Some(hash(&args.vm_worker)?) } else {None},
        "store":"SQLite file, synchronous FULL", "database":args.database,
        "allocator":lash_perf::ALLOCATION_MODE, "trim_scope":"host glibc only; workers untrimmed",
        "history_turns":args.idle.history_turns,"model_calls_per_completed_turn":2,"tool_rounds_per_completed_turn":1,
        "payload_bytes":args.payload_bytes,"pending_extra_turn":args.idle.idle_mode == Mode::Pending,
        "wave_size":args.idle.wave_size,"maximum_sessions":args.sessions,
        "mode":format!("{:?}",args.idle.idle_mode),"protocol":format!("{:?}",args.idle.idle_protocol)}),
    )
}
fn history(args: &Args, count: usize) -> Result<Value> {
    let db = rusqlite::Connection::open_with_flags(
        &args.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare("SELECT session_id, sum(body_bytes) FROM graph_nodes GROUP BY session_id ORDER BY session_id")?;
    let bytes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
    ensure!(bytes.len() == count, "history population differs");
    let mut statement = db.prepare("SELECT session_id, count(*) FROM session_runs WHERE terminal_kind='answered' GROUP BY session_id ORDER BY session_id")?;
    let turns = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
    ensure!(
        turns.len() == count && turns.values().all(|n| *n == args.idle.history_turns),
        "completed history differs from fixed workload"
    );
    Ok(
        json!({"graph_body_bytes_per_session":bytes,"completed_turns_per_session":turns,
        "scope":"durable graph body bytes; encoded checkpoint/blob bytes are reported separately by store; pending includes extra unfinished input"}),
    )
}

pub(super) async fn run(args: &Args, meter: &measure::Meter) -> Result<()> {
    ensure!(
        args.sessions <= 2000 && args.idle.history_turns > 0 && args.idle.wave_size > 0,
        "idle population requires 1..=2000 sessions, positive history and wave size"
    );
    ensure!(
        !args.retain_sessions && args.profile_from_round == 0,
        "idle population uses idle-mode and full-lifetime profiling"
    );
    let identity = identity(args)?;
    let counts = Arc::new(ScriptCounts::default());
    let (core, pool) = boot(args, Arc::clone(&counts)).await?;
    let baseline = lash_perf::GLOBAL_ALLOCATOR.stats();
    let mut held = Vec::new();
    let mut parks = Vec::new();
    let mut sample_pending = None;
    let mut previous_count = 0;
    for count in (0..=args.sessions)
        .filter(|n| *n == 0 || n.is_multiple_of(args.idle.wave_size) || *n == args.sessions)
    {
        for ordinal in previous_count + 1..=count {
            core.session(id(ordinal)?)
                .create(lash::SessionCreation::root(lash::SessionSpec::new(
                    MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(8),
                    lash::NoProgressBudget::bounded(12),
                )))
                .await?;
            let session = core.session(id(ordinal)?).open().await?;
            for _ in 0..args.idle.history_turns {
                finished(
                    session
                        .send(lash::TurnInput::text("idlepop fixed history"))
                        .await?,
                )
                .await?;
            }
            drop(session.observe().snapshot().await?);
            match args.idle.idle_mode {
                Mode::Held => held.push(session),
                Mode::Parked => {
                    parks.push(session.park().await.map_err(|e| anyhow::anyhow!("{e}"))?)
                }
                Mode::DurableOnly => drop(session),
                Mode::Pending => {
                    let handle = session
                        .send(lash::TurnInput::text("idlepop-pending"))
                        .await?;
                    let call = pending(&core, id(ordinal)?).await?;
                    if ordinal == 1 {
                        sample_pending = Some((call, handle));
                    }
                    drop(session);
                }
            }
        }
        previous_count = count;
        let residency = quiescent(&core, pool.as_ref(), count, args.idle.idle_mode).await?;
        let expected_tools =
            count * (args.idle.history_turns + usize::from(args.idle.idle_mode == Mode::Pending));
        let expected_models = count
            * (args.idle.history_turns * 2 + usize::from(args.idle.idle_mode == Mode::Pending));
        ensure!(
            counts.model_calls.load(Ordering::Relaxed) == expected_models
                && counts.tool_calls.load(Ordering::Relaxed) == expected_tools,
            "script did not execute exactly two model calls and one tool round per history turn"
        );
        let current = lash_perf::GLOBAL_ALLOCATOR.stats();
        let retained = (current.bytes_allocated as i128 - current.bytes_deallocated as i128)
            - (baseline.bytes_allocated as i128 - baseline.bytes_deallocated as i128);
        let mut population = json!({"identity":identity,"sessions":count,"residency":residency,
            "actual_model_calls":counts.model_calls.load(Ordering::Relaxed),"actual_tool_calls":counts.tool_calls.load(Ordering::Relaxed),
            "history":history(args,count)?,"facade_live_bytes":if args.idle.idle_mode == Mode::Held {Some(retained.max(0))} else {None},
            "net_live_host_rust_bytes":retained,"allocator_policy":"without-trim"});
        meter.population_sample(
            &format!("idle-{count}"),
            args.idle.history_turns,
            population.clone(),
        )?;
        if args.trim_at_idle {
            native::trim()?;
            population["allocator_policy"] = json!("after-malloc-trim-0");
            meter.population_sample(
                &format!("idle-{count}-trimmed"),
                args.idle.history_turns,
                population,
            )?;
        }
    }
    // End the heap window while the measured population is still retained.
    meter.finish_profile(true)?;
    if let Some((call, handle)) = sample_pending {
        super::resolve(&core, &call).await?;
        finished(handle).await?;
    }
    drop(held);
    let sample = if parks.is_empty() {
        core.session(id(1)?).open().await?
    } else {
        core.resume(parks.remove(0)).await?
    };
    finished(
        sample
            .send(lash::TurnInput::text("idlepop usability probe"))
            .await?,
    )
    .await?;
    meter.write(&json!({"kind":"idle-population-usability","session":id(1)?,"pending_resolved":args.idle.idle_mode == Mode::Pending,"reopened_turn_finished":true}))?;
    drop(sample);
    drop(parks);
    core.shutdown().await?;
    Ok(())
}
