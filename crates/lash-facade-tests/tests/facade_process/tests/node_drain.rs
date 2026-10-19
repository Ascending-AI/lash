//! FIG-5218, ADR 0106 §1: a host drains its core's node by release. The
//! drain hands each session the node owns to the next build at a committed
//! phase and answers which ones it released; a node of the next build
//! claims them and resumes each from its rows, with nothing lost and
//! nothing run twice.

use super::*;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::future::Future;
use std::task::Poll;

use lash::provider::LlmTransportError;
use tokio::sync::Notify;

/// The user text whose turn calls the tool: its first model call is held
/// until the law lets it answer.
const TOOL_TURN: &str = "use the tool";

/// What the scripted model and tool did, shared by both builds.
#[derive(Default)]
struct Script {
    /// Model calls of the tool turn before its tool result exists.
    tool_turn_first_calls: AtomicUsize,
    /// Model calls of the tool turn after its tool result exists.
    tool_turn_second_calls: AtomicUsize,
    /// Tool bodies run.
    tool_runs: AtomicUsize,
    /// Rung when the tool turn's first model call starts.
    entered: Notify,
    /// Lets the tool turn's first model call answer.
    gate: Notify,
    /// The user and assistant texts the last plain turn's model call saw.
    last_plain_context: StdMutex<Vec<String>>,
}

fn scripted_provider(script: Arc<Script>) -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(move |request| {
            let script = Arc::clone(&script);
            async move { Ok::<_, LlmTransportError>(answer(&script, &request).await) }
        })
        .build()
        .into_handle()
}

/// Echo a plain turn; call the tool once in the tool turn, then report the
/// tool's result.
async fn answer(script: &Script, request: &LlmRequest) -> LlmResponse {
    // The last user text: a tool result's message carries none.
    let user_text = conversation_of(request, LlmRole::User)
        .pop()
        .unwrap_or_default();
    if user_text != TOOL_TURN {
        *script.last_plain_context.lock_recover() = conversation(request);
        return streamed_text(request, format!("echo: {user_text}"));
    }
    let tool_result = request.messages.iter().find_map(|message| {
        message.blocks.iter().find_map(|block| match block {
            LlmContentBlock::ToolResult { .. } => Some(format!("{block:?}")),
            _ => None,
        })
    });
    match tool_result {
        None => {
            script.tool_turn_first_calls.fetch_add(1, Ordering::SeqCst);
            let gate = script.gate.notified();
            script.entered.notify_one();
            gate.await;
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-kept".to_owned(),
                    tool_name: lash_core::testing::FIXTURE_ECHO_TOOL.to_owned(),
                    input_json: r#"{"value":"kept"}"#.to_owned(),
                    replay: None,
                }],
                ..LlmResponse::default()
            }
        }
        Some(result) => {
            script.tool_turn_second_calls.fetch_add(1, Ordering::SeqCst);
            assert!(result.contains("kept"), "the tool result reached the model");
            streamed_text(request, "done: kept".to_owned())
        }
    }
}

/// The texts of `request`'s user and assistant messages, in order.
fn conversation(request: &LlmRequest) -> Vec<String> {
    let mut texts = Vec::new();
    for message in &request.messages {
        if matches!(message.role, LlmRole::User | LlmRole::Assistant) {
            texts.extend(texts_of(message));
        }
    }
    texts
}

/// The texts of `request`'s `role` messages, in order.
fn conversation_of(request: &LlmRequest, role: LlmRole) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == role)
        .flat_map(texts_of)
        .collect()
}

fn texts_of(message: &lash_core::llm::types::LlmMessage) -> Vec<String> {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

fn streamed_text(request: &LlmRequest, reply: String) -> LlmResponse {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            text: reply.clone(),
        }));
    }
    text_response(&reply)
}

/// The fixture echo tool, counting its runs.
struct CountedEcho(Arc<Script>);

#[async_trait]
impl ToolProvider for CountedEcho {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        lash_core::testing::FixtureTools.tool_manifests(config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        lash_core::testing::FixtureTools.resolve_contract(name, config)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.tool_runs.fetch_add(1, Ordering::SeqCst);
        lash_core::testing::FixtureTools.execute(call).await
    }
}

/// One build's core over `stores`, serving as node `owner`.
fn build_core(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    owner: &str,
    script: &Arc<Script>,
) -> LashCore {
    build_boot(stores, owner, "boot-1", script)
}

/// `owner`'s boot `boot` over `stores`.
fn build_boot(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    owner: &str,
    boot: &str,
    script: &Arc<Script>,
) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
        stores,
    )))
    .serve_test_llm_profile(
        scripted_provider(Arc::clone(script)),
        mock_llm_profile_spec(),
    )
    .tools(
        lash::tool_provider_key!("tools"),
        Arc::new(CountedEcho(Arc::clone(script))),
    )
    .build(lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new(owner),
        lash_core::LeaseIncarnationId::new(boot),
    ))
    .expect("core")
}

fn session_id(id: &str) -> lash_protocol_types::SessionId {
    lash_protocol_types::SessionId::try_from(id.to_owned()).expect("session id")
}

async fn answered(core: &LashCore, id: &str, text: &str) -> lash::TurnOutput {
    core.session(session_id(id))
        .open()
        .await
        .expect("opened")
        .send(lash::TurnInput::text(text))
        .output()
        .await
        .expect("the turn answers")
}

/// Bounded so a law that regresses fails rather than hangs.
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish"))
}

async fn a_drained_nodes_sessions_resume_on_the_next_builds_node(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
) {
    let script = Arc::new(Script::default());
    let old = build_core(Arc::clone(&stores), "drain-old-build", &script);
    for id in ["mid-turn", "idle"] {
        old.session(session_id(id))
            .create(lash::SessionCreation::root(mock_session_spec()))
            .await
            .expect("created");
        let first = answered(&old, id, "hello").await;
        assert_eq!(first.assistant_message(), Some("echo: hello"));
    }

    // The tool turn's first model call is in flight on the old build when
    // its drain starts.
    let entered = script.entered.notified();
    let held = old
        .session(session_id("mid-turn"))
        .open()
        .await
        .expect("opened")
        .send(lash::TurnInput::text(TOOL_TURN));
    let held = tokio::spawn(held.output());
    within("the tool turn's first model call", entered).await;
    let drain = old.drain();
    tokio::pin!(drain);
    let first = std::future::poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx))).await;
    assert!(
        first.is_pending(),
        "the old build drains only once its turns reach a committed phase"
    );
    script.gate.notify_one();
    let report = within("the drain", drain).await.expect("the node drained");

    // The in-flight call finished and its tool ran on the old build, which
    // then stopped before the next model call and released both sessions.
    let mut released = report.sessions.clone();
    released.sort();
    assert_eq!(released, vec![session_id("idle"), session_id("mid-turn")]);
    assert!(report.processes.is_empty(), "{report:?}");
    assert_eq!(script.tool_turn_first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.tool_runs.load(Ordering::SeqCst), 1);
    assert_eq!(script.tool_turn_second_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        old.drain().await.expect("drained again"),
        report,
        "a drained core answers the same report"
    );

    // The next build's node claims the released turn and finishes it from
    // its rows: the answered call is not sent again, the tool does not run
    // again, and the host's handle on the old build answers the reply.
    let new = build_core(stores, "drain-new-build", &script);
    let output = within("the resumed turn", held)
        .await
        .expect("the handle task")
        .expect("the resumed turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("done: kept"));
    assert_eq!(script.tool_turn_first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.tool_runs.load(Ordering::SeqCst), 1);
    assert_eq!(script.tool_turn_second_calls.load(Ordering::SeqCst), 1);

    // The idle session keeps its history: the next build answers it, and
    // work sent through the drained core reaches the next build too.
    let next = within(
        "the idle session's next turn",
        answered(&new, "idle", "again"),
    )
    .await;
    assert_eq!(next.assistant_message(), Some("echo: again"));
    let via_old = within(
        "a send through the drained core",
        answered(&old, "idle", "later"),
    )
    .await;
    assert_eq!(via_old.assistant_message(), Some("echo: later"));
    assert_eq!(
        *script.last_plain_context.lock_recover(),
        ["hello", "echo: hello", "again", "echo: again", "later"],
        "the idle session's history survived the drain"
    );

    new.shutdown().await.expect("shutdown");
    old.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drained_nodes_sessions_resume_on_the_next_builds_node_on_sqlite_memory() {
    a_drained_nodes_sessions_resume_on_the_next_builds_node(sqlite_memory_store_set().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drained_nodes_sessions_resume_on_the_next_builds_node_on_sqlite_file() {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            directory.path().join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open SQLite store set"),
    );
    a_drained_nodes_sessions_resume_on_the_next_builds_node(stores).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the optional PostgreSQL service URL"
)]
async fn a_drained_nodes_sessions_resume_on_the_next_builds_node_on_postgres() {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open(
            (attachments.path()).join("attachments.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite attachment store")
        .attachment_store(),
    ));
    a_drained_nodes_sessions_resume_on_the_next_builds_node(stores).await;
}

/// FIG-5193: a host learns why its core's node stopped, so a host whose node
/// stopped on its own can restart as a new boot. A newer boot of the same
/// owner fences the old one, whose node stops `LeaseLost` while its core
/// still admits work for the new boot to serve; a shut-down core's node
/// stopped `Requested`; a core that serves no sessions runs no node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_learns_why_its_cores_node_stopped() {
    let stores: Arc<dyn lash_execution_contracts::backend::StoreSet> =
        sqlite_memory_store_set().await;
    let script = Arc::new(Script::default());
    let old = build_boot(Arc::clone(&stores), "restarted-owner", "boot-1", &script);
    old.session(session_id("served"))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    // The old boot's node answered a turn, so it registered first.
    let first = within("the old boot's turn", answered(&old, "served", "hello")).await;
    assert_eq!(first.assistant_message(), Some("echo: hello"));

    let new = build_boot(Arc::clone(&stores), "restarted-owner", "boot-2", &script);
    let stopped = within("the old boot's node stopping", old.node_stopped()).await;
    assert!(
        matches!(stopped, Some(Ok(lash::durable::runner::Stopped::LeaseLost))),
        "the replaced boot's node stops LeaseLost: {stopped:?}"
    );
    let next = within(
        "a send through the replaced boot",
        answered(&old, "served", "again"),
    )
    .await;
    assert_eq!(next.assistant_message(), Some("echo: again"));

    new.shutdown().await.expect("shutdown");
    assert!(
        matches!(
            new.node_stopped().await,
            Some(Ok(lash::durable::runner::Stopped::Requested))
        ),
        "a shut-down core's node stopped Requested"
    );
    let detached = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(stores),
    ))
    .serve_test_llm_profile(scripted_provider(script), mock_llm_profile_spec())
    .serve_sessions(false)
    .build(lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("detached-owner"),
        lash_core::LeaseIncarnationId::new("boot-1"),
    ))
    .expect("core");
    assert!(
        detached.node_stopped().await.is_none(),
        "a core that serves no sessions runs no node"
    );
    old.shutdown().await.expect("shutdown");
}

/// A clock a law stalls: each reading is shifted by the time the law
/// skipped, and each sleep whose deadline a skip passed wakes at once, as a
/// starved process's timers all fire late together.
#[derive(Debug)]
struct StallClock {
    skipped: tokio::sync::watch::Sender<std::time::Duration>,
}

impl StallClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            skipped: tokio::sync::watch::Sender::new(std::time::Duration::ZERO),
        })
    }

    /// Stall the process for `by`: no timer fires during it.
    fn stall(&self, by: std::time::Duration) {
        self.skipped.send_modify(|skipped| *skipped += by);
    }

    fn skipped(&self) -> std::time::Duration {
        *self.skipped.borrow()
    }
}

#[async_trait]
impl lash_core::Clock for StallClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now() + self.skipped()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::TimeDelta::from_std(self.skipped()).expect("a short skip")
    }

    async fn sleep(&self, duration: std::time::Duration) {
        self.sleep_until(self.now() + duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        let mut skipped = self.skipped.subscribe();
        loop {
            let Some(real) = deadline.checked_sub(*skipped.borrow_and_update()) else {
                return;
            };
            tokio::select! {
                () = tokio::time::sleep_until(real.into()) => return,
                _ = skipped.changed() => {}
            }
        }
    }
}

/// FIG-6887: a process stalled past its node's `self_stop_after` (a starved
/// host: no heartbeat lands in time) loses nothing. Its node stops itself,
/// dropping the turn it ran mid-call, then serves again as a new boot of the
/// same owner, holding its liveness lock again: the dropped turn resumes from
/// its committed phase and answers, and an input sent across the stall is
/// admitted and answered. The core never reports its node stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_stalled_past_its_self_stop_serves_its_sessions_again() {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let clock = StallClock::new();
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open_with_clock(
            directory.path().join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
        )
        .await
        .expect("open SQLite store set"),
    );
    let self_stop_after = lash_conformance::backend_over(Arc::clone(&stores) as _)
        .config()
        .lease()
        .settings()
        .self_stop_after;
    let script = Arc::new(Script::default());
    let core = build_core(Arc::clone(&stores) as _, "stalled-owner", &script);
    for id in ["mid-turn", "idle"] {
        core.session(session_id(id))
            .create(lash::SessionCreation::root(mock_session_spec()))
            .await
            .expect("created");
        let first = within("a turn before the stall", answered(&core, id, "hello")).await;
        assert_eq!(first.assistant_message(), Some("echo: hello"));
    }

    // The tool turn's first model call is in flight when the process stalls
    // past the node's self-stop deadline.
    let entered = script.entered.notified();
    let held = core
        .session(session_id("mid-turn"))
        .open()
        .await
        .expect("opened")
        .send(lash::TurnInput::text(TOOL_TURN));
    let held = tokio::spawn(held.output());
    within("the tool turn's first model call", entered).await;
    clock.stall(self_stop_after + std::time::Duration::from_secs(1));
    let across = tokio::spawn({
        let core = core.clone();
        async move { answered(&core, "idle", "across the stall").await }
    });

    // The new boot runs the dropped model call again, and the turn finishes.
    // Every held call answers, the dropped boot's too (a provider task that
    // ignores its cancel): the tool still runs once, so only the new boot's
    // turn acts.
    within("the resumed model call", script.entered.notified()).await;
    script.gate.notify_waiters();
    let output = within("the resumed turn", held)
        .await
        .expect("the handle task")
        .expect("the resumed turn answers");
    assert_eq!(output.assistant_message(), Some("done: kept"));
    assert_eq!(script.tool_turn_first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(script.tool_runs.load(Ordering::SeqCst), 1);
    assert_eq!(script.tool_turn_second_calls.load(Ordering::SeqCst), 1);
    let next = within("the input sent across the stall", across)
        .await
        .expect("the send task");
    assert_eq!(next.assistant_message(), Some("echo: across the stall"));

    // One boot of the owner holds its liveness lock, and the node serves on.
    let node_wakes = lash_execution_contracts::backend::StoreSet::node_wakes(stores.as_ref())
        .expect("a SQLite file has node wakes");
    let held_boots: Vec<_> = node_wakes
        .liveness()
        .await
        .expect("probe the liveness locks")
        .into_iter()
        .filter(|boot| boot.held)
        .map(|boot| boot.boot.node.as_str().to_owned())
        .collect();
    assert_eq!(held_boots, ["stalled-owner"]);
    assert!(
        futures_util::FutureExt::now_or_never(core.node_stopped()).is_none(),
        "the core's node still serves"
    );
    core.shutdown().await.expect("shutdown");
}
