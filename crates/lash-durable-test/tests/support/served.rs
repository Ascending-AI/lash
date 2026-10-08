//! A lash core serving its own node over one tier's database: the host's
//! side of the tool-semantics laws (FIG-5210).
//!
//! A law builds its core through the public facade (`DurableBackendBuilder`,
//! `LashCore`), creates a session per scenario and `send()`s it an input;
//! the core's node claims the session's actor and runs the turn on the
//! durable path with the law's scripted model and host tools. One core
//! serves every scenario of a law on a tier: the model answers each
//! session from the script registered under the input that session was
//! sent.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::LlmOutputPart;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core_execution::StoreSet;
use lash_sansio::sync::MutexExt as _;

/// The database a law's core runs over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// How long a law waits for a turn that can only hang: a deadlock watchdog,
/// no part of any law.
pub const WATCHDOG: Duration = Duration::from_secs(240);

/// The model key every law's session runs on.
pub const MODEL: &str = "tool-semantics-model";

/// What a store set must outlive: its temporary database or attachment
/// directory, and its isolated PostgreSQL database.
pub type Keep = Vec<Box<dyn std::any::Any + Send>>;

/// A fresh store set of `tier`, or `None` for a PostgreSQL leg the run was
/// handed no server for.
pub async fn stores(tier: Tier) -> Option<(Arc<dyn StoreSet>, Keep)> {
    match tier {
        Tier::SqliteMemory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("an in-memory store set opens");
            Some((Arc::new(stores), Vec::new()))
        }
        Tier::SqliteFile => {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(dir.path().join("lash.db"))
                .await
                .expect("a file store set opens");
            Some((Arc::new(stores), vec![Box::new(dir)]))
        }
        Tier::Postgres => {
            // Test code: the PostgreSQL leg reads its server from the
            // environment the target's runner hands it.
            #[allow(clippy::disallowed_methods)]
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty())?;
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .expect("the isolated database opens");
            // PostgreSQL stores attachment references, while the host supplies
            // the bytes port. Keep it alive beside the isolated database.
            let attachments = tempfile::tempdir().expect("an attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            );
            Some((
                Arc::new(stores),
                vec![Box::new(isolated), Box::new(attachments)],
            ))
        }
    }
}

/// The durable backend over `stores`, through the facade's builder.
pub fn backend(stores: Arc<dyn StoreSet>) -> lash::Backend {
    backend_with(stores, Vec::new())
}

/// The durable backend over `stores` advancing the host's `engines`.
pub fn backend_with(
    stores: Arc<dyn StoreSet>,
    engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
) -> lash::Backend {
    build(lash::durable::DurableBackendBuilder::new(stores), engines)
}

/// [`backend_with`] on `settings`: a simulated deployment's.
pub fn configured_backend(
    stores: Arc<dyn StoreSet>,
    settings: lash_core_execution::DurableSettings,
    engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
) -> lash::Backend {
    build(
        lash::durable::DurableBackendBuilder::new(stores).config(settings),
        engines,
    )
}

fn build(
    builder: lash::durable::DurableBackendBuilder,
    engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
) -> lash::Backend {
    engines
        .into_iter()
        .fold(
            builder,
            lash::durable::DurableBackendBuilder::process_engine,
        )
        .build()
        .expect("the durable backend builds")
}

pub fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(1_000_000)
        .build()
        .expect("the model's metadata")
}

/// A root session spec on the law's model with `max_tool_calls`.
pub fn spec(max_tool_calls: usize) -> lash::SessionSpec {
    lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(max_tool_calls),
    )
    .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
}

/// Every scenario's script and the requests the model was asked in it,
/// keyed by the input that starts the scenario's turn.
#[derive(Default)]
pub struct Scripts {
    scripts: Mutex<BTreeMap<String, Vec<LlmResponse>>>,
    seen: Mutex<BTreeMap<String, Vec<String>>>,
}

impl Scripts {
    /// Register `script` under the scenario input `name`.
    pub fn register(&self, name: &str, script: Vec<LlmResponse>) {
        self.scripts.lock_recover().insert(name.to_owned(), script);
    }

    /// The requests the model was asked in scenario `name`, rendered.
    pub fn requests(&self, name: &str) -> Vec<String> {
        self.seen
            .lock_recover()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// The answer to `request`: the scenario is the latest user input a
    /// script is registered under, and the step is how many assistant
    /// messages follow it. The step is read off the request, not counted
    /// per call, so a step a resumed turn asks again gets that step's
    /// answer.
    fn respond(&self, request: &LlmRequest) -> LlmResponse {
        let scripts = self.scripts.lock_recover().clone();
        let mut scenario = None;
        for (index, message) in request.messages.iter().enumerate() {
            if message.role != LlmRole::User {
                continue;
            }
            let rendered = serde_json::to_value(message).expect("a message encodes");
            let mut texts = Vec::new();
            texts_in(&rendered, &mut texts);
            if let Some(name) = texts.into_iter().find(|text| scripts.contains_key(text)) {
                scenario = Some((name, index));
            }
        }
        let rendered = serde_json::to_string(&request.messages).expect("a request encodes");
        let Some((name, at)) = scenario else {
            return text(request, "done");
        };
        self.seen
            .lock_recover()
            .entry(name.clone())
            .or_default()
            .push(rendered);
        let step = request.messages[at..]
            .iter()
            .filter(|message| message.role == LlmRole::Assistant)
            .count();
        scripts
            .get(&name)
            .and_then(|script| script.get(step).cloned())
            .unwrap_or_else(|| text(request, "done"))
    }
}

/// Every `text` string in `value`.
fn texts_in(value: &serde_json::Value, into: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match value {
                    serde_json::Value::String(text) if key == "text" => into.push(text.clone()),
                    value => texts_in(value, into),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| texts_in(item, into)),
        _ => {}
    }
}

/// The scripted model over `scripts`.
pub fn model(scripts: Arc<Scripts>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("tool-semantics-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let scripts = Arc::clone(&scripts);
            async move { Ok(scripts.respond(&request)) }
        })
        .build()
        .into_handle()
}

/// A text answer, streamed as one delta when the request streams.
pub fn text(request: &LlmRequest, text: &str) -> LlmResponse {
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

/// One model response of `parts`.
pub fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        ..LlmResponse::default()
    }
}

/// A native tool call.
pub fn call(call_id: &str, tool: &str, args: serde_json::Value) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_owned(),
        tool_name: tool.to_owned(),
        input_json: args.to_string(),
        replay: None,
    }
}

/// One RLM cell of `source`.
pub fn cell(source: &str) -> LlmResponse {
    response(vec![LlmOutputPart::Text {
        text: format!("<typescript>\n{source}\n</typescript>"),
        response_meta: None,
    }])
}

/// The RLM protocol factory a code law's core runs on `workers`, its cell
/// budgets generous.
pub fn rlm(
    backend: &lash::Backend,
    resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    workers: lash::rlm::WorkerService,
) -> lash::rlm::RlmProtocolPluginFactory {
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::rlm::TypescriptDialect),
        backend,
    )
    .with_worker_service(workers);
    match resolver {
        Some(resolver) => factory.with_deferred_tool_resolver(resolver),
        None => factory,
    }
}

/// One law's core on one tier, serving its own node.
pub struct World {
    pub tier: Tier,
    pub core: lash::LashCore,
    pub backend: lash::Backend,
    pub scripts: Arc<Scripts>,
    keep: Keep,
}

impl World {
    /// The core `builder` makes over `tier`'s backend, with the law's model
    /// and runtime settings; `None` for a PostgreSQL leg with no server.
    pub async fn new(
        tier: Tier,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        Self::with_engines(tier, Vec::new(), builder).await
    }

    /// [`World::new`] over a backend that also advances the host's
    /// `engines`.
    pub async fn with_engines(
        tier: Tier,
        engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        let scripts = Arc::new(Scripts::default());
        Self::serving(
            tier,
            None,
            engines,
            model(Arc::clone(&scripts)),
            scripts,
            lash::QueuedWorkBatchingConfig::new(1),
            builder,
        )
        .await
    }

    /// [`World::new`] over a backend configured with `settings`.
    pub async fn configured(
        tier: Tier,
        settings: lash_core_execution::DurableSettings,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        let scripts = Arc::new(Scripts::default());
        Self::serving(
            tier,
            Some(settings),
            Vec::new(),
            model(Arc::clone(&scripts)),
            scripts,
            lash::QueuedWorkBatchingConfig::new(1),
            builder,
        )
        .await
    }

    /// [`World::with_engines`] whose sessions are served by `model`, a law's
    /// own, rather than by the registered scripts.
    pub async fn with_model(
        tier: Tier,
        engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
        model: ProviderHandle,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        Self::serving(
            tier,
            None,
            engines,
            model,
            Arc::default(),
            lash::QueuedWorkBatchingConfig::new(1),
            builder,
        )
        .await
    }

    /// [`World::with_model`] whose core drains its queued work under
    /// `batching`, the host's batching policy, rather than one row a run.
    pub async fn with_batching(
        tier: Tier,
        batching: lash::QueuedWorkBatchingConfig,
        model: ProviderHandle,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        Self::serving(
            tier,
            None,
            Vec::new(),
            model,
            Arc::default(),
            batching,
            builder,
        )
        .await
    }

    async fn serving(
        tier: Tier,
        settings: Option<lash_core_execution::DurableSettings>,
        engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
        model: ProviderHandle,
        scripts: Arc<Scripts>,
        batching: lash::QueuedWorkBatchingConfig,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) -> Option<Self> {
        let Some((stores, keep)) = stores(tier).await else {
            eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
            return None;
        };
        let backend = match settings {
            Some(settings) => configured_backend(stores, settings, engines),
            None => backend_with(stores, engines),
        };
        let core = builder(&backend)
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(batching)
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .serve_test_llm_profile(model, metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "tool-semantics-deployment",
                "tool-semantics-boot",
            ))
            .expect("the core builds");
        Some(Self {
            tier,
            core,
            backend,
            scripts,
            keep,
        })
    }

    /// Register `script` under the scenario `name`.
    pub fn script(&self, name: &str, script: Vec<LlmResponse>) {
        self.scripts.register(name, script);
    }

    /// The requests the model was asked in scenario `name`, rendered.
    pub fn requests(&self, name: &str) -> Vec<String> {
        self.scripts.requests(name)
    }

    /// Create the session `name` under `spec`.
    pub async fn session(&self, name: &str, spec: lash::SessionSpec) -> lash::DurableSession {
        self.core
            .session(lash::SessionId::try_from(name.to_owned()).expect("a session id"))
            .create(lash::SessionCreation::root(spec))
            .await
            .expect("the session is created")
    }

    /// Send `session` the input `name`, a registered script's; the core's
    /// node runs the turn and the handle answers its settled output.
    pub async fn send(&self, session: &lash::DurableSession, name: &str) -> lash::TurnOutput {
        tokio::time::timeout(WATCHDOG, session.send(lash::TurnInput::text(name)).output())
            .await
            .unwrap_or_else(|_| panic!("deadlock watchdog: the turn of `{name}` never settled"))
            .expect("the turn answers")
    }

    /// Run scenario `name`: register its script, create its session under
    /// `spec` and send it its input.
    pub async fn run(
        &self,
        name: &str,
        spec: lash::SessionSpec,
        script: Vec<LlmResponse>,
    ) -> lash::TurnOutput {
        self.script(name, script);
        let session = self.session(name, spec).await;
        self.send(&session, name).await
    }

    /// The serving node dies and a new core `builder` makes over the same
    /// database serves in its place, under the boot `boot`: what it knows of
    /// any session it reads back from the store.
    pub async fn restart(
        &mut self,
        boot: &str,
        builder: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
    ) {
        self.core.shutdown().await.expect("the core shuts down");
        self.core = builder(&self.backend)
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .serve_test_llm_profile(model(Arc::clone(&self.scripts)), metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "tool-semantics-deployment",
                boot,
            ))
            .expect("the restarted core builds");
    }

    /// Stop the core's node before the database goes away.
    pub async fn shutdown(self) {
        self.core.shutdown().await.expect("the core shuts down");
        drop(self.keep);
    }
}

/// A tool call the transcript holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Called {
    pub provider_call_id: String,
    pub tool: String,
    pub call_id: Option<lash_core::ToolCallId>,
    pub replay_item: Option<String>,
    /// The call's arguments as the model wrote them.
    pub args: serde_json::Value,
}

/// A tool result the transcript holds, under the provider correlation of
/// the call it pairs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    pub provider_call_id: String,
    pub tool: String,
    pub call_id: Option<lash_core::ToolCallId>,
    pub content: String,
}

impl Answered {
    /// The content as JSON, or `Null` when it is not JSON.
    pub fn value(&self) -> serde_json::Value {
        serde_json::from_str(&self.content).unwrap_or(serde_json::Value::Null)
    }
}

/// Every tool call of the settled session's transcript, in order.
pub fn calls(output: &lash::TurnOutput) -> Vec<Called> {
    let view = output.result.state.read_view();
    view.messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == lash_core::PartKind::ToolCall)
        .map(|part| Called {
            provider_call_id: part.provider_call_id().unwrap_or_default().to_owned(),
            tool: part.tool_name().unwrap_or_default().to_owned(),
            call_id: part.call_id().cloned(),
            replay_item: part.tool_replay().and_then(|replay| replay.item_id.clone()),
            args: serde_json::from_str(&part.content()).unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

/// Every tool result of the settled session's transcript, in order.
pub fn results(output: &lash::TurnOutput) -> Vec<Answered> {
    let called = calls(output);
    let view = output.result.state.read_view();
    view.messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == lash_core::PartKind::ToolResult)
        .map(|part| Answered {
            provider_call_id: called
                .iter()
                .find(|call| call.call_id.is_some() && call.call_id.as_ref() == part.call_id())
                .map(|call| call.provider_call_id.clone())
                .unwrap_or_default(),
            tool: part.tool_name().unwrap_or_default().to_owned(),
            call_id: part.call_id().cloned(),
            content: part.content().into_owned(),
        })
        .collect()
}

/// The turn answered, and its report names no issue.
pub fn assert_answered(context: &str, output: &lash::TurnOutput) {
    assert!(
        output.is_success(),
        "{context}: the turn must answer: {:?}; issues: {:?}",
        output.status(),
        output.result.errors
    );
}

/// Register `laws` once per tier, conformance-suite style: each law is an
/// `async fn(Tier)`, instantiated under `sqlite_memory::`, `sqlite_file::`
/// and `postgres::`. A law run on simulated nodes takes `current_thread:`:
/// the simulation observes quiescence on its one thread.
#[macro_export]
macro_rules! tiered_laws {
    (current_thread: $($law:ident),+ $(,)?) => {
        $crate::tiered_laws!(@all tokio::test; $($law),+);
    };
    ($($law:ident),+ $(,)?) => {
        $crate::tiered_laws!(
            @all tokio::test(flavor = "multi_thread", worker_threads = 8); $($law),+
        );
    };
    (@all $test:meta; $($law:ident),+) => {
        $crate::tiered_laws!(@tier $test; sqlite_memory, SqliteMemory; $($law),+);
        $crate::tiered_laws!(@tier $test; sqlite_file, SqliteFile; $($law),+);
        $crate::tiered_laws!(@tier $test; postgres, Postgres; $($law),+);
    };
    (@tier $test:meta; $module:ident, $tier:ident; $($law:ident),+) => {
        mod $module {
            $(
                #[$test]
                async fn $law() {
                    super::$law(super::served::Tier::$tier).await;
                }
            )+
        }
    };
}
