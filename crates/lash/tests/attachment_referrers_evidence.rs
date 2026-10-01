//! Attachment referrer laws for ADR 0124. The Restate double runs the real
//! engine over SQLite, and over PostgreSQL when a database is configured.

#![cfg(all(
    feature = "rlm",
    feature = "restate",
    feature = "sqlite",
    feature = "testing"
))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "acceptance laws establish each step's result"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::{LashCore, TurnInput};
use lash_core::{ArtifactReferrer, AttachmentId, ToolDefinitionBindingExt as _, ToolProvider};
use lash_sansio::sync::MutexExt;

#[path = "attachment_referrers_evidence/fixture.rs"]
mod fixture;
use fixture::Fixture;

const PUT_BLOB: &str = "put_blob";
const HOLD: &str = "hold";
const DECLARE_EXTERNAL: &str = "declare_external";
const START_TURN_CHILD: &str = "start_turn_child";

/// What the tools of one law observed.
struct Witness {
    /// Backend puts the `put_blob` body made: a replay serves the recorded
    /// result, so a redrive adds none.
    puts: AtomicUsize,
    /// Signalled each time a `hold` body starts.
    held: tokio::sync::Notify,
    holds: AtomicUsize,
    /// Released once the law has checked the held state.
    release: tokio::sync::Semaphore,
}

impl Witness {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            puts: AtomicUsize::new(0),
            held: tokio::sync::Notify::new(),
            holds: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        })
    }

    /// Reconcile until `hold` has started `count` times. External terminal
    /// publication is an obligation too, so waiting only on the tool's
    /// notification would leave the PostgreSQL terminal relay undriven.
    async fn held_times(&self, fixture: &Fixture, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let observed = async {
                loop {
                    let notified = self.held.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.holds.load(Ordering::SeqCst) >= count {
                        return;
                    }
                    notified.await;
                }
            };
            let reconcile = async {
                while self.holds.load(Ordering::SeqCst) < count {
                    fixture.reconcile().await;
                }
            };
            tokio::join!(observed, reconcile);
        })
        .await
        .expect("the cell reached its hold");
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

/// `put_blob({ text })` stores `text` as an attachment and returns it;
/// `hold({})` parks the cell until released or cooperatively cancelled;
/// `declare_external({})` declares one externally owned child and parks on
/// its terminal; `start_turn_child({ text })` stores `text` and starts a
/// detached SessionTurn child whose turn input carries it, answering only
/// `"started"`.
struct BlobTools {
    witness: Arc<Witness>,
}

fn tool_definition(name: &str, input: serde_json::Value) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        format!("The {name} law tool."),
        input,
        serde_json::json!({}),
    )
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

fn put_blob_definition() -> lash_core::ToolDefinition {
    tool_definition(
        PUT_BLOB,
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
}

fn hold_definition() -> lash_core::ToolDefinition {
    tool_definition(
        HOLD,
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )
}

fn declare_external_definition() -> lash_core::ToolDefinition {
    tool_definition(
        DECLARE_EXTERNAL,
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )
}

fn start_turn_child_definition() -> lash_core::ToolDefinition {
    tool_definition(
        START_TURN_CHILD,
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
}

fn text_meta(name: &str) -> lash_core::AttachmentCreateMeta {
    lash_core::AttachmentCreateMeta::new(
        lash_core::MediaType::parse("text/plain").expect("text MIME"),
        None,
        Some(name.to_owned()),
    )
}

/// The session frame a law tool's call runs in, as the originator of the
/// child it starts.
fn session_originator(
    context: &lash_core::AttemptContext<'_>,
) -> Result<lash_core::ProcessOriginator, String> {
    Ok(lash_core::ProcessOriginator::Session {
        session_id: context
            .session_id()
            .map_err(|error| error.to_string())?
            .clone(),
        agent_frame_id: Some(
            context
                .agent_frame_id()
                .map_err(|error| error.to_string())?
                .clone(),
        ),
    })
}

fn declare_external(
    context: &lash_core::AttemptContext<'_>,
) -> Result<lash_core::ToolAttemptOutcome, String> {
    let lifetime =
        lash_core::lifetime::starter(&context.start_cx().map_err(|error| error.to_string())?);
    let declaration = lash_core::ProcessStartDeclaration::external(
        session_originator(context)?,
        serde_json::json!({ "law": DECLARE_EXTERNAL }),
        lifetime,
    )
    .with_declared_identity(lash_core::DeclaredProcessIdentity::labelled(
        "law-external",
        None::<String>,
    ));
    let start = lash_core::DeclaredStart::new(
        context,
        lash_core::StartProcessIntent {
            owner: context.owner().runtime_owner(),
            declaration,
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(lash_core::ToolAttemptOutcome::pending(
        lash_core::PendingCompletion::new().resolved_by_declared_start(start),
    ))
}

async fn start_turn_child(
    context: &lash_core::AttemptContext<'_>,
    text: String,
    witness: &Witness,
) -> Result<lash_core::ToolAttemptOutcome, String> {
    witness.puts.fetch_add(1, Ordering::SeqCst);
    let stored = context
        .attachments()
        .put(text.into_bytes(), text_meta("child-input.txt"))
        .await
        .map_err(|error| error.to_string())?;
    let session_id = context
        .session_id()
        .map_err(|error| error.to_string())?
        .clone();
    let mut create_request = lash_core::SessionCreateRequest::child(
        session_id,
        lash_core::SessionStartPoint::Empty,
        lash_core::SessionPolicy {
            model: Some(lash_core::testing::test_model_config(
                law_model().wire_model,
                law_model(),
            )),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
        },
        lash_core::PluginOptions::default(),
    );
    // The child runs in the session its process id derives.
    create_request.session_id = None;
    let declaration = lash_core::ProcessStartDeclaration::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "law-session-turn:v1".to_owned(),
            create_request: Box::new(create_request),
            turn_input: Box::new(
                TurnInput::text("read the attached input")
                    .with_attachment(lash_core::AttachmentSource::stored(stored)),
            ),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        session_originator(context)?,
        lash_core::Lifetime::Detached,
    )
    .with_declared_identity(lash_core::DeclaredProcessIdentity::labelled(
        "law-session-turn",
        None::<String>,
    ))
    // A session-turn start runs under the environment its declaring attempt
    // captured (FIG-4396).
    .with_env_ref(
        context
            .process_execution_env_ref()
            .map_err(|error| error.to_string())?,
    );
    Ok(lash_core::ToolAttemptOutcome::done(
        lash_core::ToolOutcomeDone::ok(serde_json::json!("started")),
        lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
            lash_core::StartProcessIntent {
                owner: context.owner().runtime_owner(),
                declaration,
            },
        ))]),
    ))
}

#[async_trait::async_trait]
impl ToolProvider for BlobTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            put_blob_definition().manifest(),
            hold_definition().manifest(),
            declare_external_definition().manifest(),
            start_turn_child_definition().manifest(),
        ]
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == declare_external_definition().id()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            PUT_BLOB => Some(Arc::new(put_blob_definition().contract())),
            HOLD => Some(Arc::new(hold_definition().contract())),
            DECLARE_EXTERNAL => Some(Arc::new(declare_external_definition().contract())),
            START_TURN_CHILD => Some(Arc::new(start_turn_child_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            DECLARE_EXTERNAL => {
                return declare_external(call.context)
                    .unwrap_or_else(|error| lash_core::ToolOutcome::err_fmt(error).into());
            }
            START_TURN_CHILD => {
                let text = call
                    .args
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                return start_turn_child(call.context, text, &self.witness)
                    .await
                    .unwrap_or_else(|error| lash_core::ToolOutcome::err_fmt(error).into());
            }
            PUT_BLOB => {
                let text = call
                    .args
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.witness.puts.fetch_add(1, Ordering::SeqCst);
                let stored = call
                    .context
                    .attachments()
                    .put(text.into_bytes(), text_meta("blob.txt"))
                    .await;
                match stored {
                    Ok(reference) => lash_core::ToolOutcome::from_output(
                        lash_core::ToolCallOutput::success_tool_value(
                            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(
                                reference,
                            )),
                        ),
                    ),
                    Err(error) => lash_core::ToolOutcome::err_fmt(error),
                }
            }
            HOLD => {
                self.witness.holds.fetch_add(1, Ordering::SeqCst);
                self.witness.held.notify_waiters();
                let release = self.witness.release.acquire();
                let permit = if let Some(stop) = call.context.cancellation_token() {
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => {
                            return lash_core::ToolOutcome::cancelled("the held tool was cancelled")
                                .into();
                        }
                        permit = release => permit,
                    }
                } else {
                    release.await
                };
                permit.expect("the release semaphore stays open").forget();
                lash_core::ToolOutcome::ok(serde_json::json!({ "released": true }))
            }
            other => lash_core::ToolOutcome::err_fmt(format!("unknown law tool `{other}`")),
        }
        .into()
    }
}

fn response(code: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<typescript>\n{code}\n</typescript>"),
            response_meta: None,
        }],
        ..Default::default()
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..Default::default()
    }
}

type Responses = Arc<Mutex<VecDeque<LlmResponse>>>;

fn responses(scripted: Vec<LlmResponse>) -> Responses {
    Arc::new(Mutex::new(VecDeque::from(scripted)))
}

fn law_model() -> lash::ModelMetadata {
    lash::ModelMetadata::builder("attachment-referrers")
        .context_window_tokens(16_000)
        .build()
        .expect("model spec")
}

fn law_core(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    witness: &Arc<Witness>,
    scripted: Vec<LlmResponse>,
) -> LashCore {
    law_core_over(double, witness, responses(scripted), None)
}

fn law_core_over(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    witness: &Arc<Witness>,
    queue: Responses,
    upload_expiry: Option<std::time::Duration>,
) -> LashCore {
    let provider = lash::testing::TestProvider::builder()
        .kind("attachment-referrers")
        .complete(move |_request| {
            let queue = Arc::clone(&queue);
            async move {
                Ok(queue
                    .lock_recover()
                    .pop_front()
                    .expect("scripted response queue is exhausted"))
            }
        })
        .build()
        .into_handle();
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(double.lash_backend())
        .with_session_work(double.explicit_reconcile_session_work())
        .into_backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let mut builder = LashCore::rlm_builder(backend, factory)
        .serve_test_model(provider, law_model())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tools(Arc::new(BlobTools {
            witness: Arc::clone(witness),
        }))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ));
    if let Some(expiry) = upload_expiry {
        builder = builder.attachment_upload_expiry(expiry);
    }
    let core = builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "attachment-referrers-worker",
            "attachment-referrers-boot",
        ))
        .expect("RLM core");
    let worker = lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("process-worker config"),
    )
    .expect("process worker");
    double.install_process_worker(worker);
    core
}

async fn created_session(core: &LashCore, session_id: &str) -> lash::LashSession {
    core.session(session_id)
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            law_model().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create the law's session");
    core.session(session_id)
        .open()
        .await
        .expect("open the law's session")
}

async fn referrers(fixture: &Fixture, id: &AttachmentId) -> Vec<ArtifactReferrer> {
    fixture
        .double
        .lash_backend()
        .attachment_referrers()
        .attachment_referrers(id)
        .await
        .expect("read attachment referrers")
}

/// Run cleanup passes until `id`'s durable referrers satisfy `predicate`.
async fn wait_referrers(
    fixture: &Fixture,
    id: &AttachmentId,
    what: &str,
    predicate: impl Fn(&[ArtifactReferrer]) -> bool,
) -> Vec<ArtifactReferrer> {
    let found = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let found = referrers(fixture, id).await;
            if predicate(&found) {
                return found;
            }
            fixture.reconcile().await;
        }
    })
    .await;
    match found {
        Ok(found) => found,
        Err(_) => panic!(
            "referrers of `{id}` never reached {what}: {:?}",
            referrers(fixture, id).await
        ),
    }
}

fn kinds(found: &[ArtifactReferrer]) -> Vec<&'static str> {
    found
        .iter()
        .map(|referrer| referrer.kind().as_str())
        .collect()
}

fn blob_id(text: &str) -> AttachmentId {
    lash_core::attachments::content_id(text.as_bytes())
}

/// Prune every terminal process, as a host's retention pass does.
async fn prune_processes(core: &LashCore) -> lash_core::ProcessPruneReport {
    core.processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes")
}

/// One collecting sweep: grace 0, and an empty root set may delete.
async fn sweep(fixture: &Fixture) -> lash::persistence::AttachmentReclamationReport {
    let backend = fixture.double.lash_backend();
    lash::persistence::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep attachments")
}

async fn blob_present(fixture: &Fixture, id: &AttachmentId) -> bool {
    fixture
        .double
        .lash_backend()
        .attachment_store()
        .get(id, 32 * 1024 * 1024)
        .await
        .is_ok()
}

async fn process_terminal(core: &LashCore, process_id: &lash_core::ProcessId) {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(process_id),
    )
    .await
    .expect("the process reaches its terminal")
    .expect("read the process terminal");
}

fn final_value(output: &lash::TurnOutput) -> Option<serde_json::Value> {
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

fn started_process_id(output: &lash::TurnOutput) -> lash_core::ProcessId {
    let value = final_value(output).expect("the cell finished with the process id");
    serde_json::from_value(value).expect("the final value is a process id")
}

/// Law 2 (ADR 0124 §6): an Engine process runs under a runtime keyed by its
/// minted id. It admits no session, and what it puts is held by its record
/// alone.
#[tokio::test]
async fn engine_and_session_turn_create_only_real_sessions() {
    let fixture = Fixture::new(0x4215_0002).await;
    let witness = Witness::new();
    let text = "law-2-engine-put";
    let core = law_core(
        &fixture.double,
        &witness,
        vec![response(&format!(
            "const child = async () => await tools.put_blob({{ text: {text:?} }});
const handle = await processes.start({{ definition: child }});
finish(handle.process_id);"
        ))],
    );
    let session = created_session(&core, "law-2-session").await;
    let output = session
        .send(TurnInput::text("start the engine process"))
        .output()
        .await
        .expect("the starting turn");
    assert!(output.is_success(), "starting turn: {output:?}");
    let engine = started_process_id(&output);
    process_terminal(&core, &engine).await;

    let id = blob_id(text);
    let held = wait_referrers(&fixture, &id, "the engine's record alone", |found| {
        found.len() == 1
    })
    .await;
    assert_eq!(
        held,
        vec![ArtifactReferrer::ProcessRecord(engine.clone())],
        "an engine's put is held by its record, never by a session"
    );
    assert_eq!(witness.puts.load(Ordering::SeqCst), 1);
    let sessions = fixture.catalog_session_ids().await;
    assert_eq!(
        sessions,
        vec!["law-2-session".to_owned()],
        "a process runtime admits no session of its own"
    );

    prune_processes(&core).await;
    wait_referrers(&fixture, &id, "no referrer after prune", <[_]>::is_empty).await;
    let report = sweep(&fixture).await;
    assert!(report.deleted_while_referenced.is_empty(), "{report:?}");
    assert!(
        !blob_present(&fixture, &id).await,
        "the pruned engine's put is reclaimed"
    );
    assert!(
        fixture
            .catalog_session_ids()
            .await
            .iter()
            .all(|session| session == "law-2-session"),
        "prune deletes no process session, because none exists"
    );
}

/// A delivery law's shape (ADR 0124 §4): the child `C` puts `R` and
/// terminates with it, nothing else references `R`, and the cell holds
/// after the delivery is recorded. While it holds, the law prunes every
/// terminal process and sweeps: `R` survives on the receiver's edge. Then the
/// cell finishes with `R`, and the commit holds it on the session.
async fn delivered_attachment_survives_prune(seed: u64, text: &str, cell: String, crash: bool) {
    let fixture = Fixture::new(seed).await;
    let witness = Witness::new();
    let session_id = format!("law-3-{seed:x}");
    let core = law_core(&fixture.double, &witness, vec![response(&cell)]);
    let session = created_session(&core, &session_id).await;
    let turn = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .send(TurnInput::text("deliver the child's attachment"))
                .output()
                .await
        })
    };
    witness.held_times(&fixture, 1).await;
    let id = blob_id(text);
    let delivered = referrers(&fixture, &id).await;
    assert!(
        kinds(&delivered).contains(&"execution"),
        "the receiving turn acquired before it recorded: {delivered:?}"
    );
    if crash {
        // The turn's attempt dies before the hold's result is journaled:
        // the redrive replays the recorded delivery and reaches the hold
        // again without a second child run.
        fixture
            .double
            .crash_turn_drive(lash_restate_test::server::CrashPoint::BeforeRunResult {
                name: None,
            });
        witness.release_one();
        witness.held_times(&fixture, 2).await;
    }

    prune_processes(&core).await;
    wait_referrers(&fixture, &id, "no process record", |found| {
        !kinds(found).contains(&"process_record")
    })
    .await;
    let report = sweep(&fixture).await;
    assert!(
        blob_present(&fixture, &id).await,
        "the receiver's edge keeps `R` through prune and sweep: {report:?}"
    );

    witness.release_one();
    let output = turn
        .await
        .expect("the turn task")
        .expect("the receiving turn");
    assert!(output.is_success(), "receiving turn: {output:?}");
    let committed = wait_referrers(&fixture, &id, "the session's edge", |found| {
        found.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
            session_id.as_str(),
        )))
    })
    .await;
    assert!(
        blob_present(&fixture, &id).await,
        "`R` reads back after the commit: {committed:?}"
    );
    assert_eq!(
        witness.puts.load(Ordering::SeqCst),
        1,
        "no redrive puts `R` again"
    );
}

fn direct_await_cell(text: &str) -> String {
    format!(
        "const child = async () => await tools.put_blob({{ text: {text:?} }});
const handle = await processes.start({{ definition: child }});
const value = await handle;
await tools.hold({{}});
finish(value);"
    )
}

fn process_to_process_cell(text: &str) -> String {
    format!(
        "const child = async () => await tools.put_blob({{ text: {text:?} }});
const parent = async () => {{
  const inner = await processes.start({{ definition: child }});
  return await inner;
}};
const handle = await processes.start({{ definition: parent }});
const value = await handle;
await tools.hold({{}});
finish(value);"
    )
}

/// Law 3, `direct_await` (ADR 0124 §4): an RLM cell awaits its child's
/// handle directly.
#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_direct_await() {
    let text = "law-3-direct-await";
    delivered_attachment_survives_prune(0x4215_0031, text, direct_await_cell(text), false).await;
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_direct_await_crash_after_record() {
    let text = "law-3-direct-await-crash";
    delivered_attachment_survives_prune(0x4215_0032, text, direct_await_cell(text), true).await;
}

/// Law 3, `process_to_process` (ADR 0124 §4): an Engine parent awaits the
/// child and returns `R`, and the cell awaits the parent.
#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_process_to_process() {
    let text = "law-3-process-to-process";
    delivered_attachment_survives_prune(0x4215_0033, text, process_to_process_cell(text), false)
        .await;
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_process_to_process_crash_after_record() {
    let text = "law-3-process-to-process-crash";
    delivered_attachment_survives_prune(0x4215_0034, text, process_to_process_cell(text), true)
        .await;
}

/// The externally owned child a `declare_external` call declared, once it
/// is registered. A turn that ends first reports why.
async fn declared_external_child<T: std::fmt::Debug>(
    core: &LashCore,
    turn: &mut tokio::task::JoinHandle<T>,
) -> lash_core::ProcessId {
    let found = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let processes = core
                .processes()
                .list(&lash_core::ProcessListFilter::default())
                .await
                .expect("list processes");
            if let Some(process) = processes
                .iter()
                .find(|process| process.identity.kind.as_str() == "law-external")
            {
                return process.process_id.clone();
            }
            if turn.is_finished() {
                let output = (&mut *turn).await;
                panic!("the turn ended before its child registered: {output:?}; {processes:?}");
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    match found {
        Ok(process_id) => process_id,
        Err(_) => panic!(
            "the declared child never registered: {:#?}",
            core.processes()
                .list(&lash_core::ProcessListFilter::default())
                .await
        ),
    }
}

/// Law 3, `parked_declared_start` (ADR 0124 §4): a tool of `T1` declares an
/// externally owned child `C` and parks on it. The host completes `C` with
/// `R` from its own upload. The parked resolver acquires `T1`'s execution
/// before it resolves, so neither the upload's expiry nor `C`'s prune nor a
/// sweep reaches `R`, and `T1`'s commit holds it on the session.
async fn parked_declared_start_survives(seed: u64, text: &str, crash: bool) {
    let fixture = Fixture::new(seed).await;
    let witness = Witness::new();
    let session_id = format!("law-3-parked-{seed:x}");
    let core = law_core(
        &fixture.double,
        &witness,
        vec![response(
            "const value = await tools.declare_external({});
await tools.hold({});
finish(value);",
        )],
    );
    let session = created_session(&core, &session_id).await;
    let mut turn = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .send(TurnInput::text("park on the external child"))
                .output()
                .await
        })
    };
    let child = declared_external_child(&core, &mut turn).await;
    let uploads = upload_store(&fixture, &session_id, 1000);
    let delivered = host_put(&uploads, text).await;
    core.process_registry()
        .complete_process(
            &child,
            lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success_tool_value(lash_core::ToolValue::Attachment(
                    lash_core::AttachmentSource::stored(delivered.clone()),
                )),
            ),
            lash_core::ProcessCompletionAuthority::ExternalOwner,
        )
        .await
        .expect("the host completes the external child");
    witness.held_times(&fixture, 1).await;
    let id = delivered.id.clone();
    let held = referrers(&fixture, &id).await;
    assert!(
        kinds(&held).contains(&"execution"),
        "the parked resolver acquired before it resolved: {held:?}"
    );
    if crash {
        fixture
            .double
            .crash_turn_drive(lash_restate_test::server::CrashPoint::BeforeRunResult {
                name: None,
            });
        witness.release_one();
        witness.held_times(&fixture, 2).await;
    }

    fixture.double.test_clock().advance(1001);
    wait_referrers(&fixture, &id, "the upload ended", |found| {
        upload_of(found).is_none()
    })
    .await;
    prune_processes(&core).await;
    let report = sweep(&fixture).await;
    assert!(
        blob_present(&fixture, &id).await,
        "the receiver's edge keeps `R` past the upload, the prune and the sweep: {report:?}"
    );

    witness.release_one();
    let output = turn
        .await
        .expect("the turn task")
        .expect("the receiving turn");
    assert!(output.is_success(), "receiving turn: {output:?}");
    let committed = wait_referrers(&fixture, &id, "the session's edge", |found| {
        found.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
            session_id.as_str(),
        )))
    })
    .await;
    assert!(
        blob_present(&fixture, &id).await,
        "`R` reads back after the commit: {committed:?}"
    );
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_parked_declared_start() {
    parked_declared_start_survives(0x4215_0035, "law-3-parked", false).await;
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_parked_declared_start_crash_after_record() {
    parked_declared_start_survives(0x4215_0036, "law-3-parked-crash", true).await;
}

/// Wait for the SessionTurn child to reach its hold, or report the records
/// of every process when it never does.
async fn child_reached_its_hold(core: &LashCore, witness: &Witness) {
    let reached = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let notified = witness.held.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if witness.holds.load(Ordering::SeqCst) >= 1 {
                return;
            }
            notified.await;
        }
    })
    .await;
    if reached.is_ok() {
        return;
    }
    let registry = core.process_registry();
    let mut records = Vec::new();
    for process in core
        .processes()
        .list(&lash_core::ProcessListFilter::default())
        .await
        .expect("list processes")
    {
        records.push(registry.get_process(&process.process_id).await);
    }
    panic!("the child never reached its hold: {records:#?}");
}

/// Law 3, `start_input` (ADR 0124 §4): `T1` puts `R` and starts a detached
/// SessionTurn child `K` whose turn input carries it, answering nothing
/// that names `R`. `K`'s registration acquires its record, so `R`
/// outlives `T1`'s execution and a sweep, and `K`'s commit holds it on
/// `K`'s own session.
async fn start_input_survives(seed: u64, text: &str, crash: bool) {
    let fixture = Fixture::new(seed).await;
    let witness = Witness::new();
    let session_id = format!("law-3-start-input-{seed:x}");
    let core = law_core(
        &fixture.double,
        &witness,
        vec![
            response(&format!(
                "const started = await tools.start_turn_child({{ text: {text:?} }});
finish(started);"
            )),
            response(
                "await tools.hold({});
finish(\"child done\");",
            ),
        ],
    );
    let session = created_session(&core, &session_id).await;
    let output = session
        .send(TurnInput::text("start the turn child"))
        .output()
        .await
        .expect("the starting turn");
    assert!(output.is_success(), "starting turn: {output:?}");
    child_reached_its_hold(&core, &witness).await;
    let id = blob_id(text);
    // `T1` committed without naming `R`, so its execution's edge ends; `K`'s
    // registration acquired its record before `K` ran.
    let held = wait_referrers(&fixture, &id, "the child's record without T1", |found| {
        kinds(found).contains(&"process_record") && !kinds(found).contains(&"execution")
    })
    .await;
    let child = held
        .iter()
        .find_map(|referrer| match referrer {
            ArtifactReferrer::ProcessRecord(child) => Some(child.clone()),
            _ => None,
        })
        .expect("the child's record holds its start input");
    if crash {
        // The child's attempt dies before its hold's result is journaled:
        // the redrive replays its registration and admission and reaches
        // the hold again.
        fixture.double.server().crash_on(
            lash_restate_test::server::CrashRule::new(
                lash_restate_test::server::CrashPoint::BeforeRunResult { name: None },
            )
            .handler("run")
            .within_attempts(1),
        );
        witness.release_one();
        witness.held_times(&fixture, 2).await;
    }
    let report = sweep(&fixture).await;
    assert!(
        blob_present(&fixture, &id).await,
        "the child's record keeps its start input past T1: {report:?}"
    );

    witness.release_one();
    process_terminal(&core, &child).await;
    prune_processes(&core).await;
    let child_session = lash_core::SessionId::from(format!("session:process:{child}"));
    let committed = wait_referrers(&fixture, &id, "the child session's edge alone", |found| {
        found == [ArtifactReferrer::Session(child_session.clone())]
    })
    .await;
    let report = sweep(&fixture).await;
    assert!(
        blob_present(&fixture, &id).await,
        "`R` reads back on the child's session after its record was pruned: {committed:?} {report:?}"
    );
    assert_eq!(
        fixture.catalog_session_ids().await,
        {
            let mut sessions = vec![session_id.clone(), child_session.to_string()];
            sessions.sort();
            sessions
        },
        "a SessionTurn child creates exactly its own session"
    );
    assert_eq!(
        witness.puts.load(Ordering::SeqCst),
        1,
        "no redrive puts `R` again"
    );
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_start_input() {
    start_input_survives(0x4215_0037, "law-3-start-input", false).await;
}

#[tokio::test]
async fn delivered_attachment_survives_prune_and_replay_start_input_crash_after_record() {
    start_input_survives(0x4215_0038, "law-3-start-input-crash", true).await;
}

/// Law 4 (ADR 0124 §1, ADR 0113 §3.2): pruning an Engine process ends its
/// record's attachment edges through the cleanup executor, fences the
/// record, and leaves its puts to the sweep. No session is involved.
#[tokio::test]
async fn process_referrer_cleanup_is_complete_without_sessions() {
    let fixture = Fixture::new(0x4215_0004).await;
    let witness = Witness::new();
    let (first, second) = ("law-4-first", "law-4-second");
    let core = law_core(
        &fixture.double,
        &witness,
        vec![response(&format!(
            "const child = async () => {{
  const a = await tools.put_blob({{ text: {first:?} }});
  const b = await tools.put_blob({{ text: {second:?} }});
  return [a, b];
}};
const handle = await processes.start({{ definition: child }});
finish(handle.process_id);"
        ))],
    );
    let session = created_session(&core, "law-4-session").await;
    let output = session
        .send(TurnInput::text("start the engine process"))
        .output()
        .await
        .expect("the starting turn");
    assert!(output.is_success(), "starting turn: {output:?}");
    let engine = started_process_id(&output);
    process_terminal(&core, &engine).await;
    let record = ArtifactReferrer::ProcessRecord(engine.clone());
    for text in [first, second] {
        wait_referrers(&fixture, &blob_id(text), "the record's edge", |found| {
            found.contains(&record)
        })
        .await;
    }

    prune_processes(&core).await;
    for text in [first, second] {
        wait_referrers(&fixture, &blob_id(text), "no edge", <[_]>::is_empty).await;
    }
    let attachments = fixture.double.lash_backend().attachment_referrers();
    let claim = lash_core::ReferrerClaim::unguarded(record.clone()).expect("record claim");
    let late = attachments
        .begin_attachment_write(&lash_core::AttachmentWrite {
            attachment_id: blob_id(first),
            claim: claim.clone(),
        })
        .await;
    assert!(
        matches!(
            late,
            Err(lash_core::StoreError::ArtifactReferrerEnded { .. })
        ),
        "a pruned record takes no new write: {late:?}"
    );
    let acquired = attachments
        .acquire_attachment_refs(&claim, &[blob_id(first)])
        .await;
    assert!(
        matches!(
            acquired,
            Err(lash_core::StoreError::ArtifactReferrerEnded { .. })
        ),
        "a pruned record acquires nothing: {acquired:?}"
    );
    assert!(
        fixture
            .catalog_session_ids()
            .await
            .iter()
            .all(|session| session == "law-4-session"),
        "no process session exists to delete"
    );
    sweep(&fixture).await;
    for text in [first, second] {
        assert!(
            !blob_present(&fixture, &blob_id(text)).await,
            "`{text}` is reclaimed once its record ended"
        );
    }
}

fn upload_store(
    fixture: &Fixture,
    session_id: &str,
    expiry_ms: u64,
) -> lash_core::facade_support::RuntimeAttachmentStore {
    let backend = fixture.double.lash_backend();
    lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session(lash_core::SessionId::from(session_id)),
        fixture.double.test_clock() as Arc<dyn lash_core::Clock>,
    )
    .with_upload_expiry_ms(expiry_ms)
}

async fn host_put(
    store: &lash_core::facade_support::RuntimeAttachmentStore,
    text: &str,
) -> lash_core::AttachmentRef {
    store
        .put(
            text.as_bytes().to_vec(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("text/plain").expect("text MIME"),
                None,
                Some(format!("{text}.txt")),
            ),
        )
        .await
        .expect("host put")
}

fn upload_of(found: &[ArtifactReferrer]) -> Option<lash_core::UploadReferrerId> {
    found.iter().find_map(|referrer| match referrer {
        ArtifactReferrer::Upload(upload) => Some(upload.clone()),
        _ => None,
    })
}

/// Law 5 (ADR 0124 §1, §5): each unbound put is held by its own upload, which
/// ends at its expiry and fences nothing else. A committed reference outlives
/// it on the session's edge.
#[tokio::test]
async fn upload_expiry_is_local() {
    let fixture = Fixture::new(0x4215_0005).await;
    let witness = Witness::new();
    let session_id = "law-5-session";
    let core = law_core(&fixture.double, &witness, vec![text_response("noted")]);
    let session = created_session(&core, session_id).await;
    let uploads = upload_store(&fixture, session_id, 1000);
    let (a, b) = (
        host_put(&uploads, "law-5-a").await,
        host_put(&uploads, "law-5-b").await,
    );
    let upload_a = upload_of(&referrers(&fixture, &a.id).await).expect("A is held by an upload");
    let output = session
        .send(
            TurnInput::text("look at B")
                .with_attachment(lash_core::AttachmentSource::stored(b.clone())),
        )
        .output()
        .await
        .expect("the turn that commits B");
    assert!(output.is_success(), "B's turn: {output:?}");
    let held_b = referrers(&fixture, &b.id).await;
    let upload_b = upload_of(&held_b).expect("B's upload still holds it");
    assert_ne!(upload_a, upload_b, "each put mints its own upload");
    assert!(
        held_b.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
            session_id
        ))),
        "B's commit acquired the session: {held_b:?}"
    );

    fixture.double.test_clock().advance(1001);
    wait_referrers(&fixture, &a.id, "A's upload ended", <[_]>::is_empty).await;
    wait_referrers(&fixture, &b.id, "B's upload ended", |found| {
        upload_of(found).is_none() && !found.is_empty()
    })
    .await;
    let c = host_put(&uploads, "law-5-c").await;
    let upload_c = upload_of(&referrers(&fixture, &c.id).await).expect("C has a fresh upload");
    assert!(upload_c != upload_a && upload_c != upload_b);

    sweep(&fixture).await;
    assert!(!blob_present(&fixture, &a.id).await, "A is reclaimed");
    assert!(
        blob_present(&fixture, &b.id).await,
        "B stays on the session"
    );
    assert!(
        blob_present(&fixture, &c.id).await,
        "C's upload has not expired"
    );
}

/// Ruling 7 (ADR 0124 §4, queued inputs): an input that waits in the queue
/// longer than its put's upload expiry still resolves its bytes when its turn
/// commits, because the enqueue acquired the session's edge.
#[tokio::test]
async fn queued_input_outlives_its_upload_expiry() {
    let fixture = Fixture::new(0x4215_0006).await;
    let witness = Witness::new();
    let session_id = "law-queued-session";
    let core = law_core(&fixture.double, &witness, vec![text_response("noted")]);
    let session = created_session(&core, session_id).await;
    let uploads = upload_store(&fixture, session_id, 1000);
    let queued = host_put(&uploads, "law-queued-input").await;

    let hold = fixture
        .double
        .hold_session_drive(&lash_core::SessionId::from(session_id))
        .await;
    let accepted = session
        .send(
            TurnInput::text("look at this")
                .with_attachment(lash_core::AttachmentSource::stored(queued.clone())),
        )
        .await
        .expect("the held session accepts the input");
    wait_referrers(
        &fixture,
        &queued.id,
        "the enqueue's session edge",
        |found| {
            found.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
                session_id,
            )))
        },
    )
    .await;
    fixture.double.test_clock().advance(1001);
    wait_referrers(&fixture, &queued.id, "the upload ended", |found| {
        upload_of(found).is_none()
    })
    .await;
    sweep(&fixture).await;
    assert!(
        blob_present(&fixture, &queued.id).await,
        "the queued input keeps its bytes past the upload expiry"
    );

    hold.release();
    let outcome = accepted.outcome().await.expect("the queued turn");
    assert!(
        matches!(outcome.status(), lash::TurnStatus::Answered),
        "the queued turn resolves its input: {:?}",
        outcome.status()
    );
    assert!(blob_present(&fixture, &queued.id).await);
}

/// P1 (ADR 0124 §1): an Engine process whose segment dies after its tool's
/// result is journaled replays that result on the redrive: no second put, no
/// second pending write, and the record holds `R`.
#[tokio::test]
async fn redrive_replays_recorded_attachment_result() {
    let fixture = Fixture::new(0x4215_0101).await;
    let witness = Witness::new();
    let text = "p1-recorded-put";
    let core = law_core(
        &fixture.double,
        &witness,
        vec![response(&format!(
            "const child = async () => await tools.put_blob({{ text: {text:?} }});
const handle = await processes.start({{ definition: child }});
finish(handle.process_id);"
        ))],
    );
    fixture.double.server().crash_on(
        lash_restate_test::server::CrashRule::new(
            lash_restate_test::server::CrashPoint::BeforeRunResult {
                name: Some("lash.process.complete".to_owned()),
            },
        )
        .handler("run")
        .within_attempts(1),
    );
    let session = created_session(&core, "p1-session").await;
    let output = session
        .send(TurnInput::text("start the engine process"))
        .output()
        .await
        .expect("the starting turn");
    assert!(output.is_success(), "starting turn: {output:?}");
    let engine = started_process_id(&output);
    process_terminal(&core, &engine).await;
    assert_eq!(
        witness.puts.load(Ordering::SeqCst),
        1,
        "the redrive serves the recorded tool result"
    );
    assert_eq!(
        referrers(&fixture, &blob_id(text)).await,
        vec![ArtifactReferrer::ProcessRecord(engine)]
    );
}

/// Cancellation (ADR 0113 case 11, with an attachment): a child cancelled
/// while it runs keeps its record's edges until it is terminal and pruned.
#[tokio::test]
async fn a_cancelled_child_keeps_its_puts_until_pruned() {
    cancelled_child_keeps_its_puts_until_pruned(false).await;
}

#[tokio::test]
async fn a_cancelled_child_keeps_its_puts_until_pruned_with_cancel_contention() {
    cancelled_child_keeps_its_puts_until_pruned(true).await;
}

async fn cancelled_child_keeps_its_puts_until_pruned(delay_cancellation: bool) {
    let fixture = Fixture::new(0x4215_0102).await;
    let witness = Witness::new();
    let text = "cancelled-child-put";
    let queue = responses(vec![response(&format!(
        "const child = async () => {{
  const value = await tools.put_blob({{ text: {text:?} }});
  await tools.hold({{}});
  return value;
}};
const handle = await processes.start({{ definition: child }});
finish(handle.process_id);"
    ))]);
    let core = law_core_over(&fixture.double, &witness, Arc::clone(&queue), None);
    let session = created_session(&core, "cancel-session").await;
    let output = session
        .send(TurnInput::text("start the engine process"))
        .output()
        .await
        .expect("the starting turn");
    assert!(output.is_success(), "starting turn: {output:?}");
    let engine = started_process_id(&output);
    witness.held_times(&fixture, 1).await;
    let record = ArtifactReferrer::ProcessRecord(engine.clone());
    let id = blob_id(text);
    assert_eq!(referrers(&fixture, &id).await, vec![record.clone()]);

    queue.lock_recover().push_back(response(&format!(
        "const cancelled = await processes.cancel({{ process_id: {:?} }});
finish(cancelled.status);",
        engine.to_string()
    )));
    let cancellation_hold = if delay_cancellation {
        Some(
            fixture
                .double
                .server()
                .hold_service(&fixture.double.service_name("LashTurn"))
                .await,
        )
    } else {
        None
    };
    let cancelling = session
        .send(TurnInput::text("cancel the running child"))
        .await
        .expect("accept the cancelling input");
    if let Some(hold) = cancellation_hold {
        let held = core
            .process_registry()
            .get_process(&engine)
            .await
            .expect("read the held child")
            .expect("the child is retained");
        assert!(held.status.is_live(), "held child: {held:?}");
        assert!(held.cancel_request.is_none(), "held child: {held:?}");
        assert_eq!(referrers(&fixture, &id).await, vec![record.clone()]);
        hold.release();
    }
    // The turn proves durable cancel admission; the terminal below proves the
    // held tool observed cancellation before the law releases it.
    let cancelling = cancelling.output().await.expect("the cancelling turn");
    assert!(cancelling.is_success(), "cancelling turn: {cancelling:?}");
    let admitted = core
        .process_registry()
        .get_process(&engine)
        .await
        .expect("read the cancellation")
        .expect("the child is retained");
    assert!(admitted.cancel_request.is_some(), "child: {admitted:?}");
    process_terminal(&core, &engine).await;
    let terminal = core
        .process_registry()
        .get_process(&engine)
        .await
        .expect("read the terminal child")
        .expect("the child is retained until prune");
    assert_eq!(terminal.status, lash_core::ProcessStatus::Cancelled);
    witness.release_one();
    assert!(blob_present(&fixture, &id).await);
    eprintln!(
        "seed=0x4215_0102, cancel_contention={delay_cancellation}, child={:?}",
        terminal.status
    );
    assert_eq!(
        referrers(&fixture, &id).await,
        vec![record],
        "a cancelled child's record holds its puts until prune"
    );
    prune_processes(&core).await;
    wait_referrers(&fixture, &id, "no edge after prune", <[_]>::is_empty).await;
}
