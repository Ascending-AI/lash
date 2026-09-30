//! The dialect seam, proven by a second front end that exists only here.
//!
//! `SeamProofDialect` is a test fixture, not a supported language: it is
//! selected through the same public constructor a host uses for TypeScript,
//! runs a real code-mode turn on the Restate server double, and keeps its
//! selection across a cold reopen (ADR 0096).

#![cfg(all(
    feature = "rlm",
    feature = "restate",
    feature = "sqlite",
    feature = "testing"
))]
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

struct Double {
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

async fn double(tier: Tier, seed: u64) -> Option<Double> {
    let config = lash_restate_test::ServerConfig::default();
    let hooks = lash_restate_test::DeploymentHooks::default;
    match tier {
        Tier::SqliteMemory => {
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                            .await
                            .expect("SQLite memory stores"),
                    ) as Arc<dyn lash_core::StoreSet>)
                })
                .await
                .expect("SQLite memory Restate double");
            Some(Double {
                double,
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let path = root.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::open_with_clock(&path, clock)
                            .await
                            .expect("SQLite file stores"),
                    ) as Arc<dyn lash_core::StoreSet>)
                })
                .await
                .expect("SQLite file Restate double");
            Some(Double {
                double,
                _keep: vec![Box::new(root)],
            })
        }
        Tier::Postgres => {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty());
            assert!(
                url.is_some() || std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1"
            );
            let url = url?;
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let attachment_path = attachments.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(
                            &attachment_path,
                        )),
                        lash_core::WakeDeliveryConfig::default(),
                        clock,
                    )) as Arc<dyn lash_core::StoreSet>)
                })
                .await
                .expect("PostgreSQL Restate double");
            Some(Double {
                double,
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
    let backend = double.double.lash_backend();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
        &backend,
    );
    LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .serve_test_model(
            provider,
            lash::ModelMetadata::builder("seam-proof-dialect")
                .context_window_tokens(64_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
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
        .session(id)
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("create session `{id}`: {error:?}"),
    }
    core.session(id).open().await.expect("open the session")
}

fn final_value(output: &TurnOutput) -> Option<serde_json::Value> {
    output
        .result
        .state
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } if event.plugin_id == "rlm_protocol" => event
                .payload
                .get("RlmTrajectoryEntry")?
                .get("final_output")
                .cloned(),
            _ => None,
        })
        .next_back()
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
    let double = double(Tier::SqliteMemory, 0x4319_0001)
        .await
        .expect("SQLite tier");
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
        request.contains("probe.echo WITH Rec GIVES Rec"),
        "{request}"
    );
    assert!(request.contains("Seam proof execution"), "{request}");
    assert_no_typescript(request);
}

/// The law: a session resumes only under the dialect it was created with, and
/// resumes under it with its bindings and its own notation.
async fn a_suspended_session_keeps_its_selected_dialect(tier: Tier, seed: u64) {
    let Some(double) = double(tier, seed).await else {
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
        resumed_request.contains("value -> Text;"),
        "{resumed_request}"
    );
    assert_no_typescript(resumed_request);
    drop(resumed);

    let substituted = core(
        &double,
        Arc::new(lash::rlm::TypescriptDialect),
        &Script::new(&[]),
    );
    let refused = substituted.session(session_id).open().await;
    let error = format!(
        "{:?}",
        refused
            .err()
            .expect("a TypeScript host refuses the session")
    );
    assert!(error.contains("RecordedSessionConfigConflict"), "{error}");
    assert!(error.contains("dialect"), "{error}");
    assert!(error.contains("seam-proof"), "{error}");
}

#[tokio::test]
async fn a_suspended_session_keeps_its_selected_dialect_on_sqlite_memory() {
    a_suspended_session_keeps_its_selected_dialect(Tier::SqliteMemory, 0x4319_0002).await;
}

#[tokio::test]
async fn a_suspended_session_keeps_its_selected_dialect_on_sqlite_file() {
    a_suspended_session_keeps_its_selected_dialect(Tier::SqliteFile, 0x4319_0003).await;
}

#[tokio::test]
async fn a_suspended_session_keeps_its_selected_dialect_on_postgres() {
    a_suspended_session_keeps_its_selected_dialect(Tier::Postgres, 0x4319_0004).await;
}
