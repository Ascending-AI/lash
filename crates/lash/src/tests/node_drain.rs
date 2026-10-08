//! FIG-5218, ADR 0106 §1: a host drains its core's node by release. The
//! drain hands each session the node owns to the next build at a committed
//! phase and answers which ones it released; a node of the next build
//! claims them and resumes each from its rows, with nothing lost and
//! nothing run twice.

use super::*;

use std::future::Future;
use std::task::Poll;

use crate::provider::LlmTransportError;
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
    crate::testing::TestProvider::builder()
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
        events.send(LlmStreamEvent::Delta {
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            text: reply.clone(),
        });
    }
    text_response(&reply)
}

/// The fixture echo tool, counting its runs.
struct CountedEcho(Arc<Script>);

#[async_trait]
impl ToolProvider for CountedEcho {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        lash_core::testing::FixtureTools.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        lash_core::testing::FixtureTools.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.tool_runs.fetch_add(1, Ordering::SeqCst);
        lash_core::testing::FixtureTools.execute(call).await
    }
}

/// One build's core over `stores`, serving as node `owner`.
fn build_core(stores: Arc<dyn lash_core::StoreSet>, owner: &str, script: &Arc<Script>) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
        stores,
    )))
    .serve_test_llm_profile(
        scripted_provider(Arc::clone(script)),
        mock_llm_profile_spec(),
    )
    .tools(Arc::new(CountedEcho(Arc::clone(script))))
    .build(lash_core::LeaseOwnerIdentity::opaque(owner, "boot-1"))
    .expect("core")
}

fn session_id(id: &str) -> lash_sansio::SessionId {
    lash_sansio::SessionId::try_from(id.to_owned()).expect("session id")
}

async fn answered(core: &LashCore, id: &str, text: &str) -> crate::TurnOutput {
    core.session(session_id(id))
        .open()
        .await
        .expect("opened")
        .send(crate::TurnInput::text(text))
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
    stores: Arc<dyn lash_core::StoreSet>,
) {
    let script = Arc::new(Script::default());
    let old = build_core(Arc::clone(&stores), "drain-old-build", &script);
    for id in ["mid-turn", "idle"] {
        old.session(session_id(id))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
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
        .send(crate::TurnInput::text(TOOL_TURN));
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
