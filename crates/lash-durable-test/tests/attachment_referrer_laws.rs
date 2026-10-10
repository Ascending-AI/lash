//! Attachment referrer laws for ADR 0124 on a served node (ported by
//! FIG-5310 from the deleted `crates/lash/tests/attachment_referrers_evidence.rs`).
//!
//! An RLM core's cells and processes put attachments through the law's
//! tools; the laws read each attachment's durable referrers, prune
//! processes, run the artifact-cleanup relay's due pass and sweep, and check
//! what holds each put and when it is reclaimed. Every store set runs on a
//! controllable clock, so an upload's expiry is moved past rather than
//! waited for.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{
    ArtifactReferrer, AttachmentId, ClockWallTime as _, ToolDefinitionBindingExt as _, ToolProvider,
};
use lash_core_execution::StoreSet;
use lash_sansio::sync::MutexExt as _;

use served::{Keep, Tier, WATCHDOG};

const PUT_BLOB: &str = "put_blob";
const HOLD: &str = "hold";
const START_TURN_CHILD: &str = "start_turn_child";

/// How long a law waits for a referrer state, running the cleanup relay's due
/// pass as it goes.
const SETTLE: Duration = Duration::from_secs(30);

/// What the tools of one law observed.
struct Witness {
    /// Backend puts the law's tools made.
    puts: AtomicUsize,
    /// Signalled each time a `hold` body starts.
    held: tokio::sync::Notify,
    holds: AtomicUsize,
    /// Released once the law has checked the held state.
    release: tokio::sync::Semaphore,
    /// A permit each time `start_turn_child` has stored its input, while its
    /// turn still runs.
    stored: tokio::sync::Semaphore,
    /// Released once the law has run a cleanup pass inside that turn.
    resume: tokio::sync::Semaphore,
}

impl Witness {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            puts: AtomicUsize::new(0),
            held: tokio::sync::Notify::new(),
            holds: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            stored: tokio::sync::Semaphore::new(0),
            resume: tokio::sync::Semaphore::new(0),
        })
    }

    /// Wait until `hold` has started `count` times.
    async fn held_times(&self, count: usize) {
        tokio::time::timeout(SETTLE, async {
            loop {
                let notified = self.held.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.holds.load(Ordering::SeqCst) >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the tool reached its hold");
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }

    /// Wait until `start_turn_child` has stored its input and parked.
    async fn stored(&self) {
        tokio::time::timeout(SETTLE, self.stored.acquire())
            .await
            .expect("the tool stored its input")
            .expect("the stored semaphore stays open")
            .forget();
    }
}

fn tool_definition(name: &str, input: serde_json::Value) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        format!("The {name} law tool."),
        input,
        serde_json::json!({}),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

fn text_input() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"],
        "additionalProperties": false
    })
}

fn put_blob_definition() -> lash_core::ToolDefinition {
    tool_definition(PUT_BLOB, text_input())
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

fn start_turn_child_definition() -> lash_core::ToolDefinition {
    tool_definition(START_TURN_CHILD, text_input())
        .with_declaration(
            lash_core::ToolDeclaration::default()
                .with_intents([lash_core::ToolIntentKind::StartProcess]),
            None,
        )
        .expect("a valid tool declaration")
}

fn text_meta(name: &str) -> lash_core::AttachmentCreateMeta {
    lash_core::AttachmentCreateMeta::new(
        lash_core::MediaType::parse("text/plain").expect("text MIME"),
        None,
        Some(name.to_owned()),
    )
}

/// `put_blob({ text })` stores `text` and returns it; `hold({})` parks until
/// released or cooperatively cancelled; `start_turn_child({ text })` stores
/// `text`, parks until the law resumes it, and starts a detached SessionTurn
/// child whose turn input carries it, answering only `"started"`.
struct BlobTools {
    witness: Arc<Witness>,
}

impl BlobTools {
    async fn start_turn_child(
        &self,
        context: &lash_core::AttemptContext<'_>,
        text: String,
    ) -> Result<lash_core::ToolAttemptOutcome, String> {
        self.witness.puts.fetch_add(1, Ordering::SeqCst);
        let stored = context
            .attachments()
            .put(text.into_bytes(), text_meta("child-input.txt"))
            .await
            .map_err(|error| error.to_string())?;
        // Park inside the turn until the law has run a cleanup pass over the
        // put's guard, as the core's own due pass may while the turn runs.
        self.witness.stored.add_permits(1);
        self.witness
            .resume
            .acquire()
            .await
            .map_err(|error| error.to_string())?
            .forget();
        let session_id = context
            .session_id()
            .map_err(|error| error.to_string())?
            .clone();
        let mut create_request = lash_core::SessionCreateRequest::child_session(
            lash::plugins::SessionToolAccess::ambient(),
            session_id.clone(),
            lash_core::SessionStartPoint::Empty,
            lash_core::PluginOptions::default(),
        )
        .with_spec(&served::spec(1024))
        .map_err(|error| error.to_string())?;
        // The child runs in the session its process id derives.
        create_request.session_id = None;
        let declaration = lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::SessionTurn {
                definition_key: "law-session-turn:v1".to_owned(),
                create_request: Box::new(create_request),
                turn_input: Box::new(
                    lash::TurnInput::text("read the attached input")
                        .with_attachment(stored),
                ),
                result: lash_core::SessionTurnOutcome::Turn,
            },
            lash_core::ProcessOriginator::Session {
                session_id,
                agent_frame_id: Some(
                    context
                        .agent_frame_id()
                        .map_err(|error| error.to_string())?
                        .clone(),
                ),
            },
            lash_core::Lifetime::Detached,
        )
        .with_declared_identity(lash_core::DeclaredProcessIdentity::labelled(
            "law-session-turn",
            None::<String>,
        ))
        // A session-turn start runs under the environment its declaring
        // attempt captured (FIG-4396).
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
}

#[async_trait::async_trait]
impl ToolProvider for BlobTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            put_blob_definition().manifest(),
            hold_definition().manifest(),
            start_turn_child_definition().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            PUT_BLOB => Some(Arc::new(put_blob_definition().contract())),
            HOLD => Some(Arc::new(hold_definition().contract())),
            START_TURN_CHILD => Some(Arc::new(start_turn_child_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let text = || {
            call.args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match call.name() {
            START_TURN_CHILD => {
                return self
                    .start_turn_child(call.context, text())
                    .await
                    .unwrap_or_else(|error| lash_core::ToolOutcome::err_fmt(error).into());
            }
            PUT_BLOB => {
                self.witness.puts.fetch_add(1, Ordering::SeqCst);
                match call
                    .context
                    .attachments()
                    .put(text().into_bytes(), text_meta("blob.txt"))
                    .await
                {
                    Ok(reference) => lash_core::ToolOutcome::from_output(
                        lash_core::ToolCallOutput::success_tool_value(
                            lash_core::ToolValue::Attachment(reference),
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

type Responses = Arc<Mutex<VecDeque<Scripted>>>;

/// The model: it answers each request with the next scripted response.
fn model(queue: &Responses) -> lash_core::facade_support::ProviderHandle {
    let queue = Arc::clone(queue);
    lash_core::testing::TestProvider::builder()
        .kind("attachment-referrers")
        .requires_streaming(true)
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let queue = Arc::clone(&queue);
            async move {
                let next = queue
                    .lock_recover()
                    .pop_front()
                    .expect("the scripted response queue is exhausted");
                Ok(match next {
                    Scripted::Cell(source) => served::cell(&source),
                    Scripted::Text(text) => served::text(&request, &text),
                })
            }
        })
        .build()
        .into_handle()
}

/// One scripted model answer, rendered on the request it answers.
enum Scripted {
    Cell(String),
    Text(String),
}

fn cell(source: impl Into<String>) -> Scripted {
    Scripted::Cell(source.into())
}

fn text(text: &str) -> Scripted {
    Scripted::Text(text.to_owned())
}

/// A fresh store set of `tier` on `clock`, with a real attachment store;
/// `None` for a PostgreSQL leg the run was handed no server for.
async fn stores(tier: Tier, clock: Arc<dyn lash_core::Clock>) -> Option<(Arc<dyn StoreSet>, Keep)> {
    match tier {
        Tier::SqliteMemory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                .await
                .expect("an in-memory store set opens");
            Some((Arc::new(stores), Vec::new()))
        }
        Tier::SqliteFile => {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open_with_clock(
                dir.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
                clock,
            )
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
            let attachments = tempfile::tempdir().expect("an attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                lash::sqlite::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                    lash::sqlite::SqliteSynchronous::Normal,
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
                clock,
            );
            Some((
                Arc::new(stores),
                vec![Box::new(isolated), Box::new(attachments)],
            ))
        }
    }
}

/// One law's deployment: its stores on a controllable clock, the scripted
/// model, the tools' witness, and a core serving its node.
struct Law {
    backend: lash::Backend,
    clock: Arc<lash_core::testing::TestClock>,
    /// How far past `clock` the law's own cleanup passes run: a pass there
    /// claims a row deferred until then, while the node's heartbeat and
    /// leases keep reading `clock`.
    relay_lead: AtomicU64,
    queue: Responses,
    witness: Arc<Witness>,
    core: lash::LashCore,
    _keep: Keep,
}

impl Law {
    async fn new(tier: Tier, scripted: Vec<Scripted>) -> Option<Self> {
        Self::layered(tier, scripted, |stores| stores).await
    }

    /// [`Self::new`] over `layer` of the tier's store set.
    async fn layered(
        tier: Tier,
        scripted: Vec<Scripted>,
        layer: impl FnOnce(Arc<dyn StoreSet>) -> Arc<dyn StoreSet>,
    ) -> Option<Self> {
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the epoch is past")
                .as_millis(),
        )
        .expect("the epoch fits");
        let clock = Arc::new(lash_core::testing::TestClock::new(now));
        let (stores, keep) = stores(tier, Arc::clone(&clock) as Arc<dyn lash_core::Clock>).await?;
        let backend = served::backend(layer(stores));
        let queue: Responses = Arc::new(Mutex::new(scripted.into()));
        let witness = Witness::new();
        let core = Self::core(&backend, &queue, &witness, "first-build");
        Some(Self {
            backend,
            clock,
            relay_lead: AtomicU64::new(0),
            queue,
            witness,
            core,
            _keep: keep,
        })
    }

    fn core(
        backend: &lash::Backend,
        queue: &Responses,
        witness: &Arc<Witness>,
        build: &'static str,
    ) -> lash::LashCore {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, None, sim::untimed_workers()),
        )
        .serve_test_llm_profile(model(queue), served::metadata())
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .tools(Arc::new(BlobTools {
            witness: Arc::clone(witness),
        }))
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("attachment-referrers-deployment"),
            lash::persistence::LeaseIncarnationId::new(build),
        ))
        .expect("the RLM core builds")
    }

    fn script(&self, scripted: Scripted) {
        self.queue.lock_recover().push_back(scripted);
    }

    /// Create the root session `name` on `core` and open it.
    async fn session_on(core: &lash::LashCore, name: &str) -> lash::LashSession {
        let session_id = lash::SessionId::try_from(name.to_owned()).expect("a session id");
        core.session(session_id.clone())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                served::spec(1024),
            ))
            .await
            .expect("the law's session is created");
        core.session(session_id)
            .open()
            .await
            .expect("the law's session opens")
    }

    async fn session(&self, name: &str) -> lash::LashSession {
        Self::session_on(&self.core, name).await
    }

    async fn referrers(&self, id: &AttachmentId) -> Vec<ArtifactReferrer> {
        self.backend
            .attachment_referrers()
            .attachment_referrers(id)
            .await
            .expect("read attachment referrers")
    }

    /// One due pass of the artifact-cleanup relay at `relay_lead` past the
    /// law's clock, over every engine the law's protocol contributes.
    async fn cleanup_pass(&self) {
        let host = lash_core::facade_support::PluginHost::new(
            vec![Arc::new(served::rlm(
                &self.backend,
                None,
                sim::untimed_workers(),
            ))],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        );
        let engines = host
            .install_process_engine_contributions(
                lash_core::facade_support::RuntimeHostConfig::new(
                    self.backend.clone(),
                    lash::CommitBudget::bounded(16 * 1024 * 1024, 4096),
                    lash::QueuedWorkBatchingConfig::new(1),
                    lash_core::ToolSourcePolicy::Tolerate,
                    lash::ExecutionBudgets::recommended(),
                    lash::DeltaCoalescing::recommended(),
                    lash_core::facade_support::DataRetentionConfig::standard(),
                ),
                true,
            )
            .expect("the law's engines install")
            .process_engines;
        let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
            &self.backend,
            engines,
        );
        let at = lash_core::testing::TestClock::new(
            self.clock.timestamp_ms() + self.relay_lead.load(Ordering::SeqCst),
        );
        lash_core::runtime::obligations::relay::relay_due(
            &relay,
            &at,
            std::num::NonZeroUsize::new(256).expect("a page"),
        )
        .await
        .expect("the cleanup relay's due pass");
    }

    /// Run cleanup passes until `id`'s durable referrers satisfy `predicate`.
    async fn wait_referrers(
        &self,
        id: &AttachmentId,
        what: &str,
        predicate: impl Fn(&[ArtifactReferrer]) -> bool,
    ) -> Vec<ArtifactReferrer> {
        let found = tokio::time::timeout(SETTLE, async {
            loop {
                let found = self.referrers(id).await;
                if predicate(&found) {
                    return found;
                }
                self.cleanup_pass().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        match found {
            Ok(found) => found,
            Err(_) => panic!(
                "referrers of `{id}` never reached {what}: {:?}",
                self.referrers(id).await
            ),
        }
    }

    /// Prune terminal processes, as a host's retention pass does, until
    /// `process_id`'s record is gone. A process's output is published before
    /// its record is retired for prune (its scope cascade and consumer hold
    /// settle after), so one pass may find nothing to prune.
    async fn prune(&self, process_id: &lash_core::ProcessId) {
        tokio::time::timeout(SETTLE, async {
            loop {
                self.core
                    .processes()
                    .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
                    .await
                    .expect("prune terminal processes");
                match self
                    .backend
                    .process_registry()
                    .get_process(process_id)
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(lash_core::PluginError::ProcessNoLongerRetained { .. }) => {
                        return;
                    }
                    Err(error) => panic!("read the pruned process: {error}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the terminal process's record is pruned");
    }

    /// One collecting sweep: grace 0, and an empty root set may delete.
    async fn sweep(&self) -> lash::persistence::AttachmentReclamationReport {
        lash::persistence::reclaim_unreferenced_attachments(
            self.backend.session_store_factory().as_ref(),
            self.backend.attachment_store().as_ref(),
            lash_core::AttachmentReclamationPolicy::new(
                0,
                lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
            ),
        )
        .await
        .expect("sweep attachments")
    }

    async fn blob_present(&self, id: &AttachmentId) -> bool {
        self.backend
            .attachment_store()
            .get(id, 32 * 1024 * 1024)
            .await
            .is_ok()
    }

    /// Every live session the catalog lists.
    async fn sessions(&self) -> Vec<String> {
        let mut sessions = self
            .backend
            .session_store_factory()
            .list_sessions(&lash_core::SessionListFilter::default())
            .await
            .expect("list sessions")
            .into_iter()
            .map(|view| view.session_id.to_string())
            .collect::<Vec<_>>();
        sessions.sort();
        sessions
    }

    async fn process_terminal(&self, process_id: &lash_core::ProcessId) {
        tokio::time::timeout(SETTLE, self.core.processes().await_output(process_id))
            .await
            .expect("the process reaches its terminal")
            .expect("read the process terminal");
    }

    async fn record(&self, process_id: &lash_core::ProcessId) -> lash_core::ProcessRecord {
        self.backend
            .process_registry()
            .get_process(process_id)
            .await
            .expect("read the process")
            .expect("the process is retained")
    }

    async fn shutdown(self) {
        self.core.shutdown().await.expect("the core shuts down");
    }
}

/// Send `input` and wait for its settled output.
async fn run(session: &lash::LashSession, input: lash::TurnInput) -> lash::TurnOutput {
    let output = tokio::time::timeout(WATCHDOG, session.send(input).output())
        .await
        .expect("deadlock watchdog: the turn never settled")
        .expect("the turn answers");
    assert!(output.is_success(), "the turn: {output:?}");
    output
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

fn started_process_id(output: &lash::TurnOutput) -> lash_core::ProcessId {
    serde_json::from_value(
        output
            .result
            .finished()
            .map(|(_, value)| value)
            .cloned()
            .expect("the cell finished with the process id"),
    )
    .expect("the final value is a process id")
}

/// A cell that starts a TypeScript process of `body` with `args` and
/// finishes with its id.
fn start_process_cell(params: &str, body: &str, args: &str) -> Scripted {
    cell(format!(
        "const child = async ({params}) => {body};
const handle = await processes.start({{ definition: child, args: {args} }});
await control.finish(handle.process_id);"
    ))
}

/// Law 2 (ADR 0124 §6): an engine process runs under a runtime keyed by its
/// minted id. It admits no session, and what it puts is held by its record
/// alone.
async fn engine_and_session_turn_create_only_real_sessions(tier: Tier) {
    let put = "law-2-engine-put";
    let Some(law) = Law::new(
        tier,
        vec![start_process_cell(
            "text: string",
            "await tools.put_blob({ text: text })",
            &format!("{{ text: {put:?} }}"),
        )],
    )
    .await
    else {
        return;
    };
    let session = law.session("law-2-session").await;
    let output = run(&session, lash::TurnInput::text("start the engine process")).await;
    let engine = started_process_id(&output);
    law.process_terminal(&engine).await;

    let id = blob_id(put);
    let held = law
        .wait_referrers(&id, "the engine's record alone", |found| found.len() == 1)
        .await;
    assert_eq!(
        held,
        vec![ArtifactReferrer::ProcessRecord(engine.clone())],
        "an engine's put is held by its record, never by a session"
    );
    assert_eq!(law.witness.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        law.sessions().await,
        vec!["law-2-session".to_owned()],
        "a process runtime admits no session of its own"
    );

    law.prune(&engine).await;
    law.wait_referrers(&id, "no referrer after prune", <[_]>::is_empty)
        .await;
    let report = law.sweep().await;
    assert!(report.deleted_while_referenced.is_empty(), "{report:?}");
    assert!(
        !law.blob_present(&id).await,
        "the pruned engine's put is reclaimed"
    );
    assert_eq!(
        law.sessions().await,
        vec!["law-2-session".to_owned()],
        "prune deletes no process session, because none exists"
    );
    law.shutdown().await;
}

/// Law 3, `start_input` (ADR 0124 §4): `T1` puts `R` and starts a detached
/// SessionTurn child `K` whose turn input carries it, answering nothing
/// that names `R`. `K`'s registration acquires its record, so `R` outlives
/// `T1`'s execution and a sweep, and `K`'s commit holds it on `K`'s own
/// session. `T1`'s edge ends even when a due pass reached its guard while
/// `T1` ran (FIG-5370).
async fn delivered_attachment_survives_prune_and_replay_start_input(tier: Tier) {
    let put = "law-3-start-input";
    let Some(law) = Law::new(
        tier,
        vec![
            cell(format!(
                "const started = await tools.start_turn_child({{ text: {put:?} }});
await control.finish(started);"
            )),
            cell("await tools.hold({});\nawait control.finish(\"child done\");"),
        ],
    )
    .await
    else {
        return;
    };
    let session_id = "law-3-start-input";
    let session = law.session(session_id).await;
    let id = blob_id(put);
    // A due pass that reaches `T1`'s guard while `T1` runs finds the
    // execution unsettled and defers the row.
    tokio::join!(
        async {
            law.witness.stored().await;
            law.cleanup_pass().await;
            law.witness.resume.add_permits(1);
        },
        run(&session, lash::TurnInput::text("start the turn child")),
    );
    law.witness.held_times(1).await;
    // `T1` committed without naming `R`, so its execution's edge ends; `K`'s
    // registration acquired its record before `K` ran. Settlement does not
    // shorten the deferral: the row is owed again at the relay's maximum
    // backoff (ADR 0113 §2.5), so the law's passes run at that instant.
    law.relay_lead.store(
        lash_core::runtime::obligations::relay::RelayPolicy::default().max_backoff_ms,
        Ordering::SeqCst,
    );
    let held = law
        .wait_referrers(&id, "the child's record without T1", |found| {
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
    let report = law.sweep().await;
    assert!(
        law.blob_present(&id).await,
        "the child's record keeps its start input past T1: {report:?}"
    );

    law.witness.release_one();
    law.process_terminal(&child).await;
    law.prune(&child).await;
    let child_session = lash_core::SessionId::fixture(format!("session:process:{child}"));
    let committed = law
        .wait_referrers(&id, "the child session's edge alone", |found| {
            found == [ArtifactReferrer::Session(child_session.clone())]
        })
        .await;
    let report = law.sweep().await;
    assert!(
        law.blob_present(&id).await,
        "`R` reads back on the child's session after its record was pruned: {committed:?} {report:?}"
    );
    assert_eq!(
        law.sessions().await,
        {
            let mut sessions = vec![session_id.to_owned(), child_session.to_string()];
            sessions.sort();
            sessions
        },
        "a SessionTurn child creates exactly its own session"
    );
    assert_eq!(
        law.witness.puts.load(Ordering::SeqCst),
        1,
        "nothing puts `R` again"
    );
    law.shutdown().await;
}

/// Law 4 (ADR 0124 §1, ADR 0113 §3.2): pruning an engine process ends its
/// record's attachment edges through the cleanup relay, fences the record,
/// and leaves its puts to the sweep. No session is involved.
async fn process_referrer_cleanup_is_complete_without_sessions(tier: Tier) {
    let (first, second) = ("law-4-first", "law-4-second");
    let Some(law) = Law::new(
        tier,
        vec![start_process_cell(
            "first: string, second: string",
            "{\n  const a = await tools.put_blob({ text: first });\n  const b = await tools.put_blob({ text: second });\n  return [a, b];\n}",
            &format!("{{ first: {first:?}, second: {second:?} }}"),
        )],
    )
    .await
    else {
        return;
    };
    let session = law.session("law-4-session").await;
    let output = run(&session, lash::TurnInput::text("start the engine process")).await;
    let engine = started_process_id(&output);
    law.process_terminal(&engine).await;
    let record = ArtifactReferrer::ProcessRecord(engine.clone());
    for text in [first, second] {
        law.wait_referrers(&blob_id(text), "the record's edge", |found| {
            found.contains(&record)
        })
        .await;
    }

    law.prune(&engine).await;
    for text in [first, second] {
        law.wait_referrers(&blob_id(text), "no edge", <[_]>::is_empty)
            .await;
    }
    let attachments = law.backend.attachment_referrers();
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
    assert_eq!(
        law.sessions().await,
        vec!["law-4-session".to_owned()],
        "no process session exists to delete"
    );
    law.sweep().await;
    for text in [first, second] {
        assert!(
            !law.blob_present(&blob_id(text)).await,
            "`{text}` is reclaimed once its record ended"
        );
    }
    law.shutdown().await;
}

/// A host's attachment store for session `name`, its uploads expiring
/// `expiry_ms` after their put on the law's clock.
fn upload_store(
    law: &Law,
    name: &str,
    expiry_ms: u64,
) -> lash_core::facade_support::RuntimeAttachmentStore {
    lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        law.backend.attachment_store(),
        law.backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session(lash_core::SessionId::fixture(name)),
        Arc::clone(&law.clock) as Arc<dyn lash_core::Clock>,
        lash_core::facade_support::AttachmentPolicy::standard(),
    )
    .with_upload_expiry_ms(expiry_ms)
}

async fn host_put(
    store: &lash_core::facade_support::RuntimeAttachmentStore,
    text: &str,
) -> lash_core::AttachmentRef {
    store
        .put(text.as_bytes().to_vec(), text_meta(&format!("{text}.txt")))
        .await
        .expect("host put")
}

fn upload_of(found: &[ArtifactReferrer]) -> Option<lash_core::UploadReferrerId> {
    found.iter().find_map(|referrer| match referrer {
        ArtifactReferrer::Upload(upload) => Some(upload.clone()),
        _ => None,
    })
}

/// Law 5 (ADR 0124 §1, §5): each unbound put is held by its own upload,
/// which ends at its expiry and fences nothing else. A committed reference
/// outlives it on the session's edge.
async fn upload_expiry_is_local(tier: Tier) {
    let Some(law) = Law::new(tier, vec![text("noted")]).await else {
        return;
    };
    let session_id = "law-5-session";
    let session = law.session(session_id).await;
    let uploads = upload_store(&law, session_id, 1000);
    let (a, b) = (
        host_put(&uploads, "law-5-a").await,
        host_put(&uploads, "law-5-b").await,
    );
    let upload_a = upload_of(&law.referrers(&a.id).await).expect("A is held by an upload");
    run(
        &session,
        lash::TurnInput::text("look at B").with_attachment(b.clone()),
    )
    .await;
    let held_b = law.referrers(&b.id).await;
    let upload_b = upload_of(&held_b).expect("B's upload still holds it");
    assert_ne!(upload_a, upload_b, "each put mints its own upload");
    assert!(
        held_b.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
            session_id
        ))),
        "B's commit acquired the session: {held_b:?}"
    );

    law.clock.advance(1001);
    law.wait_referrers(&a.id, "A's upload ended", <[_]>::is_empty)
        .await;
    law.wait_referrers(&b.id, "B's upload ended", |found| {
        upload_of(found).is_none() && !found.is_empty()
    })
    .await;
    let c = host_put(&uploads, "law-5-c").await;
    let upload_c = upload_of(&law.referrers(&c.id).await).expect("C has a fresh upload");
    assert!(upload_c != upload_a && upload_c != upload_b);

    law.sweep().await;
    assert!(!law.blob_present(&a.id).await, "A is reclaimed");
    assert!(law.blob_present(&b.id).await, "B stays on the session");
    assert!(law.blob_present(&c.id).await, "C's upload has not expired");
    law.shutdown().await;
}

/// Ruling 7 (ADR 0124 §4, queued inputs): an input that waits longer than its
/// put's upload expiry still resolves its bytes when its turn runs, because
/// its enqueue acquired the session's edge. The input waits in a session no
/// node serves: the creating core drained its node before the send, and a
/// second core serves the turn once the upload has expired and been swept.
async fn queued_input_outlives_its_upload_expiry(tier: Tier) {
    let Some(law) = Law::new(tier, vec![text("noted")]).await else {
        return;
    };
    let session_id = "law-queued-session";
    let session = law.session(session_id).await;
    let uploads = upload_store(&law, session_id, 1000);
    let queued = host_put(&uploads, "law-queued-input").await;
    law.core
        .drain()
        .await
        .expect("the creating core's node drains");
    let accepted = Box::pin(
        session
            .send(lash::TurnInput::text("look at this").with_attachment(queued.clone()))
            .into_future(),
    )
    .await
    .expect("the drained core still accepts the input");
    law.wait_referrers(&queued.id, "the enqueue's session edge", |found| {
        found.contains(&ArtifactReferrer::Session(lash_core::SessionId::from(
            session_id,
        )))
    })
    .await;
    law.clock.advance(1001);
    law.wait_referrers(&queued.id, "the upload ended", |found| {
        upload_of(found).is_none()
    })
    .await;
    law.sweep().await;
    assert!(
        law.blob_present(&queued.id).await,
        "the queued input keeps its bytes past the upload expiry"
    );

    let serving = Law::core(&law.backend, &law.queue, &law.witness, "second-build");
    let _ = serving.session(lash::SessionId::from(session_id));
    let output = tokio::time::timeout(WATCHDOG, accepted.output())
        .await
        .expect("deadlock watchdog: the queued turn never settled")
        .expect("the queued turn answers");
    assert!(
        output.is_success(),
        "the queued turn resolves its input: {output:?}"
    );
    assert!(law.blob_present(&queued.id).await);
    serving.shutdown().await.expect("the core shuts down");
    law.shutdown().await;
}

/// Cancellation (ADR 0113 case 11, with an attachment): a child cancelled
/// while it runs keeps its record's edges until it is terminal and pruned.
async fn a_cancelled_child_keeps_its_puts_until_pruned(tier: Tier) {
    let put = "cancelled-child-put";
    let Some(law) = Law::new(
        tier,
        vec![start_process_cell(
            "text: string",
            "{\n  const value = await tools.put_blob({ text: text });\n  await tools.hold({});\n  return value;\n}",
            &format!("{{ text: {put:?} }}"),
        )],
    )
    .await
    else {
        return;
    };
    let session = law.session("cancel-session").await;
    let output = run(&session, lash::TurnInput::text("start the engine process")).await;
    let engine = started_process_id(&output);
    law.witness.held_times(1).await;
    let record = ArtifactReferrer::ProcessRecord(engine.clone());
    let id = blob_id(put);
    assert_eq!(law.referrers(&id).await, vec![record.clone()]);

    law.script(cell(format!(
        "const cancelled = await processes.cancel({{ process_id: {:?} }});\nawait control.finish(cancelled.status);",
        engine.to_string()
    )));
    run(&session, lash::TurnInput::text("cancel the running child")).await;
    let admitted = law.record(&engine).await;
    assert!(admitted.cancel_request.is_some(), "child: {admitted:?}");
    law.process_terminal(&engine).await;
    let terminal = law.record(&engine).await;
    assert_eq!(
        terminal.status(),
        lash_core::ProcessStatus::Cancelled,
        "a cancelled child reaches its typed terminal: {terminal:?}"
    );
    law.witness.release_one();
    assert!(law.blob_present(&id).await);
    assert_eq!(
        law.referrers(&id).await,
        vec![record],
        "a cancelled child's record holds its puts until prune"
    );
    law.prune(&engine).await;
    law.wait_referrers(&id, "no edge after prune", <[_]>::is_empty)
        .await;
    law.shutdown().await;
}

tokio::task_local! {
    /// Set while the law's host start runs: what tells the starter's own
    /// adoption from the cleanup relay's hold of the same record.
    static STARTER: ();
}

/// The store set's attachment referrers, except that the host starter's
/// adoption of its start input, its acquisition under the process record it
/// just registered, meets `fault`: the cut between a start's registration
/// and its adoption (ADR 0113 §3.3). Every other acquisition, the cleanup
/// relay's included, reaches the store as it is.
struct AdoptionCut {
    inner: Arc<dyn lash_core::AttachmentReferrers>,
    fault: lash_durable_test::Fault,
    /// A permit once the adoption met its fault.
    reached: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl lash_core::AttachmentReferrers for AdoptionCut {
    async fn begin_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
    ) -> Result<lash_core::store::AttachmentWriteFence, lash_core::StoreError> {
        self.inner.begin_attachment_write(write).await
    }

    async fn complete_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
        permit: lash_core::store::AttachmentWritePermit,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.complete_attachment_write(write, permit).await
    }

    async fn abort_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
        permit: lash_core::store::AttachmentWritePermit,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abort_attachment_write(write, permit).await
    }

    async fn acquire_attachment_refs(
        &self,
        claim: &lash_core::ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), lash_core::StoreError> {
        if !matches!(claim.referrer(), ArtifactReferrer::ProcessRecord(_))
            || STARTER.try_with(|()| ()).is_err()
        {
            return self.inner.acquire_attachment_refs(claim, ids).await;
        }
        if self.fault.commits() {
            self.inner.acquire_attachment_refs(claim, ids).await?;
        }
        self.reached.add_permits(1);
        if self.fault.kills() {
            // The starter's node is gone: its call never returns.
            std::future::pending::<()>().await;
        }
        Err(lash_core::StoreError::Contended)
    }

    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.forget_attachment_ref(referrer, id).await
    }

    async fn end_attachment_referrer(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }

    async fn session_referrer_state(
        &self,
        id: &lash_core::SessionId,
    ) -> Result<lash_core::store::SessionReferrerState, lash_core::StoreError> {
        self.inner.session_referrer_state(id).await
    }

    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, lash_core::StoreError> {
        self.inner.attachment_referrers(id).await
    }
}

/// A host's detached SessionTurn start under host key `key`, whose child
/// turn reads `input`.
fn host_start(key: &str, input: lash_core::AttachmentRef) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "law-host-start-input:v1".to_owned(),
            create_request: Box::new(
                lash_core::SessionCreateRequest::root(
                    lash::plugins::SessionToolAccess::ambient(),
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )
                .with_spec(&served::spec(1024))
                .expect("a root spec states its model and turn budget"),
            ),
            turn_input: Box::new(
                lash::TurnInput::text("read the host upload").with_attachment(input),
            ),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_host_start_key(key)
}

/// FIG-5388 (ADR 0113 §3.3, ADR 0124 §4): a host start stages its uploaded
/// input under `StartInput(key, starter)` before it registers, and adopts
/// it onto the process record after. The law cuts a real start between the
/// two: the starter's adoption write meets each fault a host call's
/// unfenced store write can meet. It fails before the store (the host is
/// answered a failure and gives up), the starter dies before it, the write
/// lands and the starter dies, or the write lands and the host is answered
/// a failure. (A paused, zombie or delayed write is an actor's commit under
/// an epoch fence, and a lost wake a mailbox write's; the adoption is
/// neither.) No node serves the start. The upload then expires, and the
/// cleanup relay and a collecting sweep run: the input's only referrer is
/// the process record, the staging referrer has ended, and its bytes are
/// kept. Another build then takes the process over, and its turn reads the
/// input.
async fn a_host_start_cut_between_registration_and_adoption_keeps_its_uploaded_input(tier: Tier) {
    use lash_durable_test::Fault;
    for fault in [
        Fault::FailBefore,
        Fault::Abort,
        Fault::CommitThenAbort,
        Fault::AckHidden,
    ] {
        let reached = Arc::new(tokio::sync::Semaphore::new(0));
        let Some(law) = Law::layered(tier, vec![text("noted")], |stores| {
            lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_attachment_referrers(|inner| -> Arc<dyn lash_core::AttachmentReferrers> {
                    Arc::new(AdoptionCut {
                        inner,
                        fault,
                        reached: Arc::clone(&reached),
                    })
                })
                .into_store_set()
        })
        .await
        else {
            return;
        };
        let session_id = "cut-start-input-session";
        let _session = law.session(session_id).await;
        let uploads = upload_store(&law, session_id, 1000);
        let bytes = format!("{fault}: the host's uploaded start input");
        let input = host_put(&uploads, &bytes).await;
        law.core
            .drain()
            .await
            .expect("the starting core's node drains");

        let request = host_start(&format!("cut-start-input-{fault}"), input.clone());
        let start_key = request.start_key().cloned().expect("a host start key");
        let core = law.core.clone();
        let starter = tokio::spawn(STARTER.scope((), async move {
            core.processes().start(request, core.effect_host()).await
        }));
        tokio::time::timeout(SETTLE, reached.acquire())
            .await
            .unwrap_or_else(|_| panic!("{fault}: the start never reached its adoption"))
            .expect("the cut semaphore stays open")
            .forget();
        if fault.kills() {
            starter.abort();
        } else {
            let answered = starter.await.expect("the start task joins");
            assert!(
                answered.is_err(),
                "{fault}: the host is answered the adoption's failure: {answered:?}"
            );
        }
        let record = law
            .backend
            .process_registry()
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key")
            .unwrap_or_else(|| panic!("{fault}: the start registered before the cut"));
        assert_eq!(record.input.stored_attachment_ids(), vec![input.id.clone()]);

        law.clock.advance(1001);
        law.relay_lead.store(
            lash_core::runtime::obligations::relay::RelayPolicy::default().max_backoff_ms,
            Ordering::SeqCst,
        );
        let held = law
            .wait_referrers(&input.id, "the process record alone", |found| {
                found == [ArtifactReferrer::ProcessRecord(record.id.clone())]
            })
            .await;
        let staging = lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::StartInput {
            start_key: start_key.clone(),
            starter: lash_core::runtime::start_operation_journal(&start_key)
                .expect("the start's own journal"),
        });
        let late = law
            .backend
            .attachment_referrers()
            .acquire_attachment_refs(&staging, std::slice::from_ref(&input.id))
            .await;
        assert!(
            matches!(
                late,
                Err(lash_core::StoreError::ArtifactReferrerEnded { .. })
            ),
            "{fault}: the start's input staging ended: {late:?}"
        );
        let report = law.sweep().await;
        assert!(
            law.blob_present(&input.id).await,
            "{fault}: the process record keeps the input past its upload: {held:?} {report:?}"
        );

        let serving = Law::core(&law.backend, &law.queue, &law.witness, "second-build");
        tokio::time::timeout(SETTLE, serving.processes().await_output(&record.id))
            .await
            .unwrap_or_else(|_| panic!("{fault}: the taken-over process never ended"))
            .expect("read the process terminal");
        let terminal = law.record(&record.id).await;
        assert_eq!(
            terminal.status(),
            lash_core::ProcessStatus::Completed,
            "{fault}: the taken-over turn reads its input: {terminal:?}"
        );
        assert_eq!(
            law.backend
                .attachment_store()
                .get(&input.id, 32 * 1024)
                .await
                .expect("the input reads back")
                .bytes,
            bytes.into_bytes()
        );
        serving.shutdown().await.expect("the core shuts down");
        law.shutdown().await;
    }
}

tiered_laws!(
    a_host_start_cut_between_registration_and_adoption_keeps_its_uploaded_input,
    engine_and_session_turn_create_only_real_sessions,
    delivered_attachment_survives_prune_and_replay_start_input,
    process_referrer_cleanup_is_complete_without_sessions,
    upload_expiry_is_local,
    queued_input_outlives_its_upload_expiry,
    a_cancelled_child_keeps_its_puts_until_pruned,
);
