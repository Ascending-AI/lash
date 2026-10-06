//! Live Restate/PostgreSQL comparison fixtures (FIG-5168).
//!
//! Each case has a fresh database and a private service namespace. Model
//! calls return immediately; round intervals include the real engine, store,
//! tool and provider path. SQL write *statements* (including zero-row writes)
//! and returned rows come from pg_stat_statements, not a fake store counter.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart};
use lash_core::testing::TestProvider;
use lash_core::{ToolAttemptOutcome, ToolContract, ToolDefinition, ToolManifest, ToolProvider};
use lash_sansio::sync::MutexExt;
use serde::Serialize;

use super::provider::response;
use super::restate::LocalRestate;

#[derive(Clone, Copy, Serialize)]
struct Scenario {
    name: &'static str,
    rounds: usize,
    tools_per_round: usize,
    sessions: usize,
    suspend: bool,
    process: bool,
}

fn scenarios() -> Vec<Scenario> {
    let mut cases = Vec::new();
    for rounds in [1, 5, 20] {
        cases.push(Scenario {
            name: "rounds",
            rounds,
            tools_per_round: 1,
            sessions: 1,
            suspend: false,
            process: false,
        });
        cases.push(Scenario {
            name: "resume",
            rounds,
            tools_per_round: 1,
            sessions: 1,
            suspend: true,
            process: false,
        });
    }
    for sessions in [1, 10, 100] {
        cases.push(Scenario {
            name: "concurrent",
            rounds: 5,
            tools_per_round: 3,
            sessions,
            suspend: false,
            process: false,
        });
    }
    cases.push(Scenario {
        name: "parked-process",
        rounds: 0,
        tools_per_round: 1,
        sessions: 1,
        suspend: true,
        process: true,
    });
    cases
}

#[derive(Default)]
struct Fixture {
    model_calls: Mutex<HashMap<String, Vec<Instant>>>,
    keys: Mutex<Vec<lash_core::AwaitEventKey>>,
}

struct Tools {
    echo: crate::runtime_perf::providers::BenchmarkEchoTool,
    fixture: Arc<Fixture>,
    park: ToolDefinition,
}

impl Tools {
    fn new(backend: &lash::Backend, fixture: Arc<Fixture>) -> Result<Self> {
        use lash::tools::{ToolBinding, ToolDefinitionBindingExt};
        Ok(Self {
            echo: crate::runtime_perf::providers::BenchmarkEchoTool::new(backend.effect_host()),
            fixture,
            park: ToolDefinition::raw(
                "tool:baseline_park", "baseline_park", "Wait for the benchmark's external completion.",
                serde_json::json!({"type":"object","additionalProperties":false}),
                serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}),
            )?.with_tool_binding(ToolBinding::new(["tools"], "baseline_park").with_authority_type("Tools"))
                .with_declaration(lash_core::ToolDeclaration::deferring()),
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![self.echo.tool_manifests().remove(0), self.park.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        if name == "baseline_park" {
            Some(Arc::new(self.park.contract()))
        } else {
            self.echo.resolve_contract(name)
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolAttemptOutcome {
        if call.name() != "baseline_park" {
            return self.echo.execute(call).await;
        }
        match call.context.completion_key() {
            Ok(key) => {
                self.fixture.keys.lock_recover().push(key);
                lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
            }
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

fn provider(case: Scenario, fixture: Arc<Fixture>) -> lash::provider::ProviderHandle {
    TestProvider::builder()
        .kind("restate-baseline")
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..Default::default()
        })
        .serialize_config(move || serde_json::json!({"scenario":case}))
        .complete(move |request| {
            let fixture = Arc::clone(&fixture);
            async move {
                fixture.model_calls.lock_recover()
                    .entry(request.session_id().to_string()).or_default().push(Instant::now());
                let results = request.messages.iter().flat_map(|message| message.blocks.iter())
                    .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. })).count();
                let round = results / case.tools_per_round;
                let parts = if case.process {
                    vec![LlmOutputPart::Text {
                        text: "<typescript>\nconst work = async () => { return await tools.baseline_park({}); };\nconst handle = await processes.start({ definition: work });\nconst result = await handle;\nfinish(result);\n</typescript>".to_string(),
                        response_meta: None,
                    }]
                } else if round < case.rounds {
                    (0..case.tools_per_round).map(|tool| LlmOutputPart::ToolCall {
                        call_id: format!("round-{round}-tool-{tool}"),
                        tool_name: "benchmark_echo".to_string(),
                        input_json: serde_json::json!({"value":"baseline","ordinal":round}).to_string(),
                        replay: None,
                    }).collect()
                } else if case.suspend && results == case.rounds * case.tools_per_round {
                    vec![LlmOutputPart::ToolCall {
                        call_id: "park".to_string(), tool_name: "baseline_park".to_string(),
                        input_json: "{}".to_string(), replay: None,
                    }]
                } else {
                    vec![LlmOutputPart::Text {text:"baseline complete".to_string(), response_meta:None}]
                };
                Ok(response(parts))
            }
        }).build().into_handle()
}

#[derive(Default, Clone, Serialize)]
struct Writes {
    statements: i64,
    returned_rows: i64,
}

async fn sql_writes(storage: &lash_postgres_store::PostgresStorage) -> Result<Writes> {
    let (statements, returned_rows): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(calls),0)::bigint, COALESCE(SUM(rows),0)::bigint \
         FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         AND query NOT ILIKE '%pg_stat_statements%' AND query ~* '\\m(INSERT|UPDATE|DELETE)\\M'",
    ).fetch_one(storage.pool()).await?;
    Ok(Writes {
        statements,
        returned_rows,
    })
}

#[derive(serde::Deserialize, Serialize)]
struct Invocation {
    id: String,
    target_service_name: String,
    target_handler_name: String,
    status: String,
    journal_size: u64,
}

async fn invocations(
    admin: &lash_restate::RestateAdminClient,
    namespace: &str,
) -> Result<Vec<Invocation>> {
    let mut result = Vec::new();
    let mut cursor = String::new();
    loop {
        // Loaded cases retain many completed invocations. Page within the
        // admin client's response bound rather than lifting that bound.
        let page: Vec<Invocation> = admin.query_json(&format!(
            "SELECT id, target_service_name, target_handler_name, status, COALESCE(journal_size, 0) AS journal_size \
             FROM sys_invocation WHERE target_service_name LIKE '{namespace}.%' \
             AND id > '{cursor}' ORDER BY id LIMIT 1000",
        )).await?;
        let Some(last) = page.last() else {
            break;
        };
        cursor.clone_from(&last.id);
        result.extend(page);
    }
    Ok(result)
}

#[derive(Serialize)]
struct Sample {
    session: String,
    send_to_completion_ms: f64,
    round_ms: Vec<f64>,
    first_model_ms: f64,
    completion_tail_ms: f64,
}

#[derive(Serialize)]
struct Batch {
    scenario: Scenario,
    pool_max_connections: u32,
    repetition: usize,
    wall_ms: f64,
    samples: Vec<Sample>,
    sql_writes: Writes,
    journal_entries: u64,
    invocations: Vec<Invocation>,
    suspended_journal_entries: Option<u64>,
    suspended_invocations: Vec<Invocation>,
    parked_ms: Option<f64>,
    external_completion_to_outcome_ms: Option<f64>,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn build_core(
    backend: lash::Backend,
    case: Scenario,
    fixture: Arc<Fixture>,
) -> Result<lash::LashCore> {
    let provider = provider(case, Arc::clone(&fixture));
    let tools = Arc::new(Tools::new(&backend, fixture)?);
    let mut plugins = lash::PluginStack::new();
    plugins.push(Arc::new(lash::plugins::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("baseline_tools"),
        lash::plugins::PluginSpec::new().with_tool_provider(tools),
    )));
    let builder = if case.process {
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash_protocol_rlm::TypescriptDialect),
            &backend,
        );
        plugins.push(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ));
        lash::LashCore::rlm_builder(backend, factory)
    } else {
        lash::LashCore::standard_builder(backend)
    };
    Ok(builder
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("baseline-model")
                .context_window_tokens(200_000)
                .build()?,
        )
        .plugins(plugins)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "baseline", "host",
        ))?)
}

struct CaseContext<'a> {
    core: &'a lash::LashCore,
    storage: &'a lash_postgres_store::PostgresStorage,
    admin: &'a lash_restate::RestateAdminClient,
    namespace: &'a str,
    fixture: &'a Arc<Fixture>,
}

async fn measure_batch(
    context: &CaseContext<'_>,
    case: Scenario,
    repetition: usize,
    wait_seconds: u64,
) -> Result<Batch> {
    let CaseContext {
        core,
        storage,
        admin,
        namespace,
        fixture,
    } = *context;
    let mut sessions = Vec::new();
    for lane in 0..case.sessions {
        let id = lash::SessionId::fixture(format!("{namespace}-{repetition}-{lane}"));
        core.session(id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                lash::LlmProfileMetadata::builder("baseline-model")
                    .context_window_tokens(200_000)
                    .build()?
                    .wire_model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await?;
        sessions.push(core.session(id).open().await?);
    }
    // All sessions are ready before the barrier: concurrency includes exactly
    // case.sessions simultaneous sends, without timing session creation.
    let before = sql_writes(storage).await?;
    let journal_before: u64 = invocations(admin, namespace)
        .await?
        .iter()
        .map(|v| v.journal_size)
        .sum();
    let started = Instant::now();
    let turns = sessions.iter().cloned().map(|session| async move {
        let sent = Instant::now();
        let outcome = session
            .send(lash::TurnInput::text("run baseline"))
            .await?
            .outcome()
            .await?;
        anyhow::ensure!(
            matches!(outcome.status(), lash::TurnStatus::Answered),
            "turn did not answer: {:?}",
            outcome.status()
        );
        let ended = Instant::now();
        let times = fixture
            .model_calls
            .lock_recover()
            .remove(session.session_id().as_str())
            .context("model timestamps missing")?;
        anyhow::ensure!(
            case.process || times.len() == case.rounds + 1 + usize::from(case.suspend),
            "wrong model round count: {}",
            times.len()
        );
        Ok::<_, anyhow::Error>(Sample {
            session: session.session_id().to_string(),
            send_to_completion_ms: ms(ended.duration_since(sent)),
            round_ms: times
                .windows(2)
                .take(case.rounds)
                .map(|pair| ms(pair[1].duration_since(pair[0])))
                .collect(),
            first_model_ms: ms(times[0].duration_since(sent)),
            completion_tail_ms: ms(
                ended.duration_since(*times.last().context("empty model timestamps")?)
            ),
        })
    });
    let complete = futures_util::future::try_join_all(turns);
    tokio::pin!(complete);
    let mut suspended_invocations = Vec::new();
    let mut parked_ms = None;
    let mut wake = None;
    if case.suspend {
        let park_started = Instant::now();
        loop {
            tokio::select! {
                result = &mut complete => { result?; anyhow::bail!("turn finished before suspension"); }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            let rows = invocations(admin, namespace).await?;
            if !fixture.keys.lock_recover().is_empty()
                && rows.iter().any(|v| {
                    v.status == "suspended"
                        && if case.process {
                            v.target_service_name.contains("LashProcess")
                                && v.target_handler_name == "run"
                        } else {
                            v.target_service_name.contains("LashTurn")
                                && v.target_handler_name == "run"
                        }
                })
            {
                suspended_invocations = rows;
                break;
            }
            anyhow::ensure!(
                park_started.elapsed() < Duration::from_secs(120),
                "no suspended execution: {}",
                serde_json::to_string(&rows)?
            );
        }
        let parked = Instant::now();
        tokio::time::sleep(Duration::from_secs(if case.process {
            wait_seconds
        } else {
            1
        }))
        .await;
        parked_ms = Some(ms(parked.elapsed()));
        wake = Some(Instant::now());
        let keys = std::mem::take(&mut *fixture.keys.lock_recover());
        for key in keys {
            let verdict = core
                .completions()
                .resolve(
                    key,
                    lash_core::Resolution::Ok(serde_json::json!({"ok":true})),
                )
                .await?;
            anyhow::ensure!(
                verdict == lash_core::ResolveOutcome::Accepted,
                "external completion refused: {verdict:?}"
            );
        }
    }
    let samples = tokio::time::timeout(Duration::from_secs(180), &mut complete).await??;
    let wall_ms = ms(started.elapsed());
    let external_completion_to_outcome_ms = wake.map(|instant| ms(instant.elapsed()));
    // Wait for handler tails before counting durable writes; never count the
    // session-close/delete teardown. The counted idle shift may remain suspended.
    let deadline = Instant::now() + Duration::from_secs(30);
    let rows = loop {
        let rows = invocations(admin, namespace).await?;
        if rows
            .iter()
            .all(|v| matches!(v.status.as_str(), "completed" | "suspended"))
        {
            break rows;
        }
        anyhow::ensure!(Instant::now() < deadline, "handler tails did not settle");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let after = sql_writes(storage).await?;
    let batch = Batch {
        scenario: case,
        pool_max_connections: storage.pool().options().get_max_connections(),
        repetition,
        wall_ms,
        samples,
        sql_writes: Writes {
            statements: after.statements - before.statements,
            returned_rows: after.returned_rows - before.returned_rows,
        },
        journal_entries: rows.iter().map(|v| v.journal_size).sum::<u64>() - journal_before,
        invocations: rows,
        suspended_journal_entries: case
            .suspend
            .then(|| suspended_invocations.iter().map(|v| v.journal_size).sum()),
        suspended_invocations,
        parked_ms,
        external_completion_to_outcome_ms,
    };
    for session in sessions {
        session.close().await?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rows = invocations(admin, namespace).await?;
        if rows.iter().all(|v| v.status == "completed") {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "session teardown did not quiesce"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(batch)
}

/// Run only against caller-provided live services. The PostgreSQL database
/// URL is never serialized; a fresh provisioned database isolates each case.
pub async fn run(out: &Path, samples: usize, wait_seconds: u64, selected: &[String]) -> Result<()> {
    anyhow::ensure!(
        samples > 0 && wait_seconds > 0,
        "samples and wait-seconds must be positive"
    );
    let ingress_url =
        std::env::var("RESTATE_INGRESS_URL").context("live RESTATE_INGRESS_URL required")?;
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").context("live RESTATE_ADMIN_URL required")?;
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .context("LASH_POSTGRES_DATABASE_URL required")?;
    let gate = std::env::var("KILN_GATE_ID").context("run through kiln gate")?;
    let admin = lash_restate::RestateAdminClient::new(admin_url.clone());
    let cases = scenarios();
    let names: Vec<_> = cases
        .iter()
        .map(|case| {
            format!(
                "{}-{}",
                case.name,
                if case.name == "concurrent" {
                    case.sessions
                } else {
                    case.rounds
                }
            )
        })
        .collect();
    anyhow::ensure!(
        selected.iter().all(|name| names.contains(name)),
        "unknown case; known: {names:?}"
    );
    let mut batches = Vec::new();
    for (ordinal, case) in cases.into_iter().enumerate() {
        if !selected.is_empty() && !selected.contains(&names[ordinal]) {
            continue;
        }
        // Live-session observers retain connections: 32 cannot open 100
        // live sessions. Size the loaded fixture's pool for readers + writers.
        let pool_max_connections = if case.sessions == 100 { 256 } else { 32 };
        let database = lash_postgres_store::testing::IsolatedDatabase::create(&database_url).await;
        let storage = lash_postgres_store::PostgresStorage::connect_with(
            database.url(),
            lash_postgres_store::PostgresStoreConfig {
                max_connections: pool_max_connections,
                min_connections: 4,
                ..Default::default()
            },
        )
        .await?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .execute(storage.pool())
            .await?;
        let namespace = format!("baseline-{}-{ordinal}", std::process::id());
        let restate = LocalRestate {
            ingress_url: ingress_url.clone(),
            admin_url: admin_url.clone(),
            authority: lash::restate::RestateAuthorityId::new(format!("{gate}-{ordinal}"))?,
            source: "env",
        };
        let stores = lash_postgres_store::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core::attachments::UnavailableAttachmentStore),
        );
        let engine = Arc::new(lash::restate::RestateEngine::new(
            Arc::new(stores),
            lash::restate::RestateConfig::new(
                ingress_url.clone(),
                admin_url.clone(),
                restate.authority.clone(),
            )
            .with_namespace(lash::restate::RestateNamespace::new(&namespace)?),
        ));
        let fixture = Arc::new(Fixture::default());
        let core = build_core(
            lash::Backend::new(engine.clone()),
            case,
            Arc::clone(&fixture),
        )?;
        let worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .context("worker config")?,
        )?;
        let _deployment = restate
            .serve(&engine, engine.endpoint_builder(worker)?.build())
            .await?;
        let repetitions = if case.suspend { 1 } else { samples };
        for repetition in 0..repetitions {
            let context = CaseContext {
                core: &core,
                storage: &storage,
                admin: &admin,
                namespace: &namespace,
                fixture: &fixture,
            };
            let batch = measure_batch(&context, case, repetition, wait_seconds).await?;
            println!(
                "baseline {} rounds={} tools={} sessions={} sample={} wall_ms={:.3} sql_writes={} journal_entries={}",
                case.name,
                case.rounds,
                case.tools_per_round,
                case.sessions,
                repetition,
                batch.wall_ms,
                batch.sql_writes.statements,
                batch.journal_entries
            );
            batches.push(batch);
            // Persist each completed batch so a service failure never loses
            // prior actual measurements. No fabricated or partial sample rows.
            std::fs::write(
                out,
                serde_json::to_string_pretty(&serde_json::json!({
                    "gate":gate, "engine":"restate-server", "store":"postgresql",
                    "turn_batches_per_case":samples, "suspension_batches_per_case":1, "batches":batches,
                }))? + "\n",
            )?;
        }
        storage.pool().close().await;
    }
    Ok(())
}
