//! The dialect seam, proven by a second front end that exists only here.
//!
//! `SeamProofDialect` is a test fixture, not a supported language: it is
//! selected through the same public constructor a host uses for TypeScript,
//! runs a real code-mode turn on the core's durable node, and keeps its
//! selection across a cold reopen (ADR 0096).

#![cfg(all(feature = "rlm", feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "test target: the registration helpers around the tests are test code too"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::rlm::Dialect;
use lash::{LashCore, TurnInput, TurnOutput};
use lash_core::{ToolDefinitionBindingExt as _, ToolProvider};
use lash_sansio::sync::MutexExt;

#[path = "seam_proof_dialect/dialect.rs"]
mod dialect;
use dialect::SeamProofDialect;

fn worker_executable() -> &'static str {
    env!("CARGO_BIN_EXE_lash-seam-proof-worker")
}

/// Words and forms a prompt served to this dialect must never carry: they
/// belong to TypeScript and its JavaScript runtime.
const TYPESCRIPT_TEXT: &[&str] = &[
    "TypeScript",
    "typescript",
    "JavaScript",
    "console.log",
    "await ",
    "Promise",
    "finish(",
    "const ",
    "=>",
    "HistoryItem",
    "undefined",
];

// ---- store tiers ------------------------------------------------------------

enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// One tier's store set, which every core a law builds runs over.
struct Double {
    stores: Arc<dyn lash::StoreSet>,
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

impl Double {
    /// A backend over the tier's stores: each core gets its own, as each
    /// build of a deployment does.
    fn backend(&self) -> lash::Backend {
        lash::durable::DurableBackendBuilder::new(Arc::clone(&self.stores))
            .build()
            .expect("the durable backend builds")
    }
}

async fn double(tier: Tier) -> Option<Double> {
    match tier {
        Tier::SqliteMemory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite memory stores");
            Some(Double {
                stores: Arc::new(stores),
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(root.path().join("lash.db"))
                .await
                .expect("SQLite file stores");
            Some(Double {
                stores: Arc::new(stores),
                _keep: vec![Box::new(root)],
            })
        }
        Tier::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            );
            Some(Double {
                stores: Arc::new(stores),
                _keep: vec![Box::new(database), Box::new(storage), Box::new(attachments)],
            })
        }
    }
}

// ---- the tool the cells call ------------------------------------------------

struct Probe {
    calls: Arc<AtomicUsize>,
}

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:probe_echo",
        "probe_echo",
        "Answer with the text it was given.",
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["probe"], "echo"))
}

#[async_trait::async_trait]
impl ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "probe_echo").then(|| Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = call
            .args
            .get("text")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        lash_core::ToolOutcome::ok(serde_json::json!({ "value": text })).into()
    }
}

// ---- the host ---------------------------------------------------------------

struct Script {
    replies: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    tool_calls: Arc<AtomicUsize>,
}

impl Script {
    fn new(replies: &[&str]) -> Self {
        Self {
            replies: Arc::new(Mutex::new(
                replies.iter().map(|reply| (*reply).to_string()).collect(),
            )),
            requests: Arc::default(),
            tool_calls: Arc::default(),
        }
    }
}

fn core(double: &Double, dialect: Arc<dyn Dialect>, script: &Script) -> LashCore {
    let replies = Arc::clone(&script.replies);
    let requests = Arc::clone(&script.requests);
    let provider = lash::testing::TestProvider::builder()
        .kind("seam-proof-dialect")
        .complete(move |request| {
            let replies = Arc::clone(&replies);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
                    .push(serde_json::to_string(&request).expect("serialize the request"));
                let text = replies
                    .lock_recover()
                    .pop_front()
                    .expect("scripted reply queue is exhausted");
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let backend = double.backend();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
        &backend,
    );
    LashCore::rlm_builder(backend, factory)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("seam-proof-dialect")
                .context_window_tokens(64_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .tools(Arc::new(Probe {
            calls: Arc::clone(&script.tool_calls),
        }))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "seam-proof-worker",
            "seam-proof-boot",
        ))
        .expect("RLM core")
}

fn cell(lines: &str) -> String {
    format!("<seam>\n{lines}\n</seam>")
}

async fn session(core: &LashCore, id: &str) -> lash::LashSession {
    match core
        .session(lash_core::SessionId::fixture(id))
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                "seam-proof-dialect",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("create session `{id}`: {error:?}"),
    }
    core.session(lash_core::SessionId::fixture(id.to_string()))
        .open()
        .await
        .expect("open the session")
}

fn final_value(output: &TurnOutput) -> Option<serde_json::Value> {
    output.final_value().cloned()
}

fn assert_no_typescript(request: &str) {
    for word in TYPESCRIPT_TEXT {
        assert!(
            !request.contains(word),
            "a seam-proof prompt carries TypeScript text `{word}`: {request}"
        );
    }
}

// ---- the tests --------------------------------------------------------------

#[tokio::test]
async fn a_seam_proof_dialect_runs_a_real_turn_through_the_host() {
    let double = double(Tier::SqliteMemory).await.expect("SQLite tier");
    let script = Script::new(&[&cell(
        "take reply from probe.echo WITH {\"text\": \"seam\"}\ngive reply.value",
    )]);
    let core = core(&double, Arc::new(SeamProofDialect), &script);
    let session = session(&core, "seam-proof-turn").await;
    let output = session
        .send(TurnInput::text("echo seam"))
        .output()
        .await
        .expect("the turn runs");

    assert!(output.is_success(), "{output:?}");
    assert_eq!(final_value(&output), Some(serde_json::json!("seam")));
    assert_eq!(script.tool_calls.load(Ordering::SeqCst), 1);
    let requests = script.requests.lock_recover().clone();
    assert_eq!(requests.len(), 1, "one model call ends the turn");
    let request = &requests[0];
    assert!(request.contains("<seam>"), "{request}");
    assert!(
        request.contains("probe.echo WITH record{text: str} GIVES record{value: str}"),
        "{request}"
    );
    assert!(request.contains("Seam proof execution"), "{request}");
    assert_no_typescript(request);
}

/// The law: a session resumes only under the dialect it was created with, and
/// resumes under it with its bindings and its own notation.
async fn a_suspended_session_keeps_its_selected_dialect(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let long = "x".repeat(4_000);
    let script = Script::new(&[
        &cell(&format!(
            "take kept from probe.echo WITH {{\"text\": \"{long}\"}}\ngive \"bound\""
        )),
        &cell("give kept.value"),
    ]);
    let session_id = "seam-proof-resume";

    let first = core(&double, Arc::new(SeamProofDialect), &script);
    let output = session(&first, session_id)
        .await
        .send(TurnInput::text("bind"))
        .output()
        .await
        .expect("first turn");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(final_value(&output), Some(serde_json::json!("bound")));
    drop(first);

    let resumed = core(&double, Arc::new(SeamProofDialect), &script);
    let output = session(&resumed, session_id)
        .await
        .send(TurnInput::text("read"))
        .output()
        .await
        .expect("resumed turn");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(final_value(&output), Some(serde_json::json!(long)));
    let requests = script.requests.lock_recover().clone();
    let resumed_request = requests.last().expect("the resumed turn called the model");
    assert!(
        resumed_request.contains("bound in Seam proof"),
        "{resumed_request}"
    );
    assert!(
        resumed_request.contains("shape Kept is record{value: str}"),
        "{resumed_request}"
    );
    assert_no_typescript(resumed_request);
    drop(resumed);

    // A session's runtime is built at its first turn, so the host's
    // refusal comes at open or at the first send; either is typed.
    let substitute_script = Script::new(&[]);
    let substituted = core(
        &double,
        Arc::new(lash::rlm::TypescriptDialect),
        &substitute_script,
    );
    let refused = match substituted
        .session(lash::SessionId::parse(session_id).expect("nonblank host identity"))
        .open()
        .await
    {
        Err(error) => format!("open: {error:?}"),
        Ok(session) => {
            let sent = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                session.send(TurnInput::text("again")).output(),
            )
            .await;
            format!("send: {sent:?}")
        }
    };
    let error = refused;
    assert!(
        substitute_script.requests.lock_recover().is_empty(),
        "the refused session calls no model"
    );
    assert!(error.contains("RecordedSessionConfigConflict"), "{error}");
    assert!(error.contains("dialect"), "{error}");
    assert!(error.contains("seam-proof"), "{error}");
}

#[tokio::test]
#[ignore = "FIG-5324: the dialect refusal reaches the host as RuntimeError { code: Plugin } with no typed cause"]
async fn a_suspended_session_keeps_its_selected_dialect_on_sqlite_memory() {
    a_suspended_session_keeps_its_selected_dialect(Tier::SqliteMemory).await;
}

#[tokio::test]
#[ignore = "FIG-5324: the dialect refusal reaches the host as RuntimeError { code: Plugin } with no typed cause"]
async fn a_suspended_session_keeps_its_selected_dialect_on_sqlite_file() {
    a_suspended_session_keeps_its_selected_dialect(Tier::SqliteFile).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_suspended_session_keeps_its_selected_dialect_on_postgres() {
    a_suspended_session_keeps_its_selected_dialect(Tier::Postgres).await;
}
