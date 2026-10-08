//! A tool group wider than the PostgreSQL pool settles (FIG-5237).
//!
//! A turn's tool calls run concurrently, and each one's admission writes to
//! the store. A member must never hold a pooled connection, or an open
//! transaction, while the task that polls it waits on the pool itself: once
//! the waiting members hold every connection, nothing can release one, and
//! the turn never settles.
//!
//! The laws run one turn whose model opens a group of 64 calls to an
//! instant native tool, on a PostgreSQL pool of two connections, under a
//! wall-clock watchdog: a starved pool fails the law, whether it ends in the
//! pool's acquire timeout or never. The group runs as the standard
//! protocol's native tool round, and as a code cell that admits its last
//! call while the losers of a race over the others still run.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::rlm::Dialect as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::{
    Backend, BackendParts, DurableSettings, NoProjectionProviders, StoreSet,
};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, DurableStore, LeaseConfig};
use lash_durable_test::{Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

const SESSION: &str = "pool-width-session";
const RUN: &str = "pool-width-turn";
const TOOL: &str = "touch";
const MODEL: &str = "pool-width-model";
/// The group's width: four times the default pool, 32 times this one.
const WIDTH: usize = 64;
/// The pool every law runs on: any two members mid-transaction exhaust it.
const POOL: u32 = 2;
/// What marks a call's answer in the transcript.
const TOUCHED: &str = "touched";
const FINAL: &str = "every call answered";
/// How long a turn may take on the wall clock before the law calls it
/// deadlocked.
const WATCHDOG: Duration = Duration::from_secs(240);

/// The PostgreSQL server the laws run on, or `None` when the run was handed
/// none and they are skipped.
fn postgres_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which protocol runs the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// The standard protocol: one native tool round of `WIDTH` members.
    Tools,
    /// The RLM protocol: one cell that races `WIDTH - 1` calls, admits one
    /// more while the race's losers run, then awaits them all.
    Code,
}

/// `touch`'s body: an instant native answer that touches no store. Each
/// number it was given, in the order its bodies ran.
struct Touch {
    touched: Arc<Mutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl StaticToolExecute for Touch {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Some(x) = call.args["x"].as_u64() {
            self.touched.lock_recover().push(x);
        }
        ToolOutcome::ok(serde_json::json!({ TOUCHED: call.args["x"] })).into()
    }
}

fn touch(touched: Arc<Mutex<Vec<u64>>>) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Answers the number it is given.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("touch's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
    Arc::new(StaticToolProvider::new(vec![definition], Touch { touched }))
}

/// The cell the model writes: a race over every call but the last, the
/// last admitted while the race's losers run, then every answer awaited.
fn cell() -> String {
    let last = WIDTH - 1;
    format!(
        "<typescript>\nconst pending = Array.from({{ length: {last} }}, (_, x) => tools.{TOOL}({{ x }}));\nawait Promise.race(pending);\nconst last = await tools.{TOOL}({{ x: {last} }});\nprint([...(await Promise.all(pending)), last]);\n</typescript>"
    )
}

/// The scripted model: it opens the group, and once the transcript holds
/// its answers it ends the turn in prose. Each request it saw, rendered.
fn model(protocol: Protocol, seen: Arc<Mutex<Vec<String>>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("pool-width-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let seen = Arc::clone(&seen);
            async move {
                let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
                seen.lock_recover().push(rendered);
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == lash_core::llm::types::LlmRole::Assistant);
                Ok(match (answered, protocol) {
                    (true, _) => text(&request, FINAL),
                    (false, Protocol::Code) => text(&request, &cell()),
                    (false, Protocol::Tools) => LlmResponse {
                        parts: (0..WIDTH)
                            .map(|x| LlmOutputPart::ToolCall {
                                call_id: format!("call-{x}"),
                                tool_name: TOOL.to_owned(),
                                input_json: format!(r#"{{"x":{x}}}"#),
                                replay: None,
                            })
                            .collect(),
                        ..LlmResponse::default()
                    },
                })
            }
        })
        .build()
        .into_handle()
}

/// A text answer, streamed as one delta.
fn text(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Delta {
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        });
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// The dialect's worker service with its run deadlines off the clock: a
/// cell's guest is bounded by its instruction and memory budgets.
fn untimed_workers() -> lash::rlm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::rlm::WorkerService::new(config)
}

fn core(
    protocol: Protocol,
    backend: &Backend,
    seen: &Arc<Mutex<Vec<String>>>,
    touched: &Arc<Mutex<Vec<u64>>>,
) -> lash::LashCore {
    let builder = match protocol {
        Protocol::Code => lash::LashCore::rlm_builder(
            backend.clone(),
            lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build(),
                Arc::new(lash::rlm::TypescriptDialect),
                backend,
            )
            .with_worker_service(untimed_workers()),
        ),
        Protocol::Tools => lash::LashCore::standard_builder(backend.clone()),
    };
    builder
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(4 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(protocol, Arc::clone(seen)), metadata())
        .tools(touch(Arc::clone(touched)))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "pool-width-deployment",
            "pool-width-boot",
        ))
        .expect("the core builds")
}

/// A fresh isolated PostgreSQL database behind a pool of `POOL`
/// connections.
///
/// The database is made on a thread and runtime of its own: its future is
/// not `Send` for every lifetime, as a scenario's must be.
async fn small_pool(
    url: String,
    clock: Arc<SimClock>,
    keep: &Mutex<Vec<Box<dyn std::any::Any + Send>>>,
) -> Arc<dyn StoreSet> {
    let isolated = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a setup runtime")
            .block_on(lash_postgres_store::testing::IsolatedDatabase::create(&url))
    })
    .join()
    .expect("the isolated database is created");
    let storage = lash_postgres_store::testing::connect_with(
        isolated.url(),
        &lash_postgres_store::testing::work_pool_of(POOL),
    )
    .await
    .expect("the isolated database opens");
    let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
        &storage,
        Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        clock,
    );
    keep.lock_recover().push(Box::new(isolated));
    Arc::new(stores)
}

struct Wide {
    protocol: Protocol,
    postgres_url: String,
    seen: Arc<Mutex<Vec<String>>>,
    touched: Arc<Mutex<Vec<u64>>>,
    backend: Mutex<Option<Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Wide {
    fn new(protocol: Protocol, postgres_url: String) -> Self {
        Self {
            protocol,
            postgres_url,
            seen: Arc::default(),
            touched: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| core(self.protocol, &backend, &self.seen, &self.touched))
            .clone()
    }
}

#[async_trait::async_trait]
impl Scenario for Wide {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = small_pool(self.postgres_url.clone(), clock, &self.keep).await;
        let database = lash_core_execution::StoreSet::durable_store(stores.as_ref());
        *self.backend.lock_recover() = Some(
            Backend::assemble(BackendParts {
                stores,
                settings: DurableSettings::default(),
                engines: Vec::new(),
                providers: Arc::new(NoProjectionProviders),
                formats: lash::formats::actor_state_surfaces(),
            })
            .expect("the backend assembles"),
        );
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            // This uncut pool-pressure law keeps the production heartbeat
            // and reap cadence as part of the load on its two-connection pool.
            lease: LeaseConfig::default(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(SessionActivation::new(
            self.backend(),
            lash::testing::session_turn_services(&self.core()),
            Arc::new(Tripwire::default()) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                lash::SessionSpec::new(
                    MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(WIDTH),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            ))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        session
            .send(lash::TurnInput::text("touch every number at once"))
            .id(run())
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
        nodes.start("a");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, _cut: Option<&lash_durable_test::Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        match nodes.database().turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }
        // Every member's body ran once.
        let mut touched = self.touched.lock_recover().clone();
        touched.sort_unstable();
        let expected: Vec<u64> = (0..WIDTH as u64).collect();
        if touched != expected {
            violations.push(format!("the bodies ran for {touched:?}"));
        }
        // The model was told every answer, and no member failed.
        let told = self.seen.lock_recover().last().cloned().unwrap_or_default();
        if !told.contains(TOUCHED)
            || ["Tool execution failed", "tool was interrupted", "→ err"]
                .iter()
                .any(|failed| told.contains(failed))
        {
            violations.push(format!("the model was told a failed group: {told}"));
        }
        violations
    }
}

/// The uncut turn on `protocol` settles under the watchdog, every member
/// answered.
async fn settles(protocol: Protocol) {
    let Some(url) = postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let report = tokio::time::timeout(
        WATCHDOG,
        Matrix::new()
            .faults(&[])
            .run(|| Wide::new(protocol, url.clone())),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("pool deadlock: a {protocol:?} group of {WIDTH} on a pool of {POOL} never settled")
    });
    report.assert_held();
}

/// A native tool round of 64 members settles on a pool of two connections.
#[tokio::test]
async fn a_tool_round_wider_than_the_pool_settles_on_postgres() {
    settles(Protocol::Tools).await;
}

/// A code cell that admits a call while 62 losers of a race still run
/// settles on a pool of two connections.
#[tokio::test]
async fn a_cell_admitting_beside_running_calls_wider_than_the_pool_settles_on_postgres() {
    settles(Protocol::Code).await;
}
