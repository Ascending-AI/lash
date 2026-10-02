//! The Restate host contract of D5 (FIG-3837): a host handler submits
//! through `send()` and waits on its journal; the session's engine is the
//! only executor.
//!
//! A host journals a stable input id, its acceptance, and a wait made of
//! bounded probes (`lash::restate`'s binding). These laws run such a host on
//! the double and hold the turn's model call on a barrier, so the wait spans
//! many probes and handler attempts:
//!
//! * a replayed handler never submits twice: a crash after the acceptance
//!   committed but before its journal entry did re-runs it under the same id;
//! * the wait outlives the handler's inactivity timer: with the timeout at
//!   zero, the handler suspends at every step and replays its whole journal;
//! * the host's journal holds its binding steps only: no drive admission,
//!   model call or commit of the turn is ever journaled on the host;
//! * an exclusive object handler accepts and returns, and a shared handler of
//!   the same object waits, so the turn's own calls to the object's exclusive
//!   handlers never queue behind a waiting host;
//! * a host reaches its session inside its journal: its session deleted while
//!   it was parked, killed and replayed from the top, it replays exactly the
//!   journal it recorded and answers (FIG-4277). This law also runs against a
//!   live `restate-server` (the `host-send-wait` Restate suite);
//! * the engine's own root does too: its session deleted after the root
//!   journaled its admission, the killed root's replay follows its journal
//!   and ends with the typed retirement (FIG-4346, live leg of
//!   `lash::tests::deleted_session_root_replay`), and so does a follow-on
//!   recovery root killed after its seal and before its recorded recovery
//!   decision (FIG-4361);
//! * a committed root answers its follower from the store alone: with the
//!   session's durable-wait index held after the commit, a follower that
//!   attaches then still answers, and no terminal key ever holds more than
//!   one server-side `await_resolution` waiter (FIG-4345);
//! * a dropped terminal attach leaves no second server invocation: a
//!   re-attach joins the one waiter, and an attach after the terminal
//!   resolved reads it without registering (FIG-4345).
//!
//! The FIG-4345 laws run on the double over SQLite memory, SQLite file and
//! PostgreSQL stores, with and without always-replay, and against a live
//! `restate-server`.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]
#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]
// The live leg reads the suite runner's env (RESTATE_INGRESS_URL, endpoint
// binds); ambient env access is sanctioned in test targets.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use lash::restate::RestateWait;
use lash_core::StoreSet;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    CrashCount, CrashPoint, CrashRule, HandlerAttempt, RestateTestBackend, ServerConfig,
};
use restate_sdk::context::{
    ContextPromises, ContextReadState, ContextWriteState, ObjectContext, SharedObjectContext,
    SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::HandlerResult;
use restate_sdk::serde::Json;
use tokio::sync::Notify;

#[path = "host_send_wait/session_delete.rs"]
mod session_delete;

const SESSION: &str = "durable-host";
/// A probe window short enough that a held turn spans many probes.
const PROBE: Duration = Duration::from_millis(20);

/// Every model call waits here until the law releases it.
#[derive(Default)]
struct Barrier {
    calls: AtomicUsize,
    release: Notify,
}

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "host-send-wait")
}

fn core(backend: lash_core::Backend, barrier: &Arc<Barrier>) -> lash::LashCore {
    let barrier = Arc::clone(barrier);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("host-send-wait")
        .complete(move |_request: LlmRequest| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.calls.fetch_add(1, Ordering::SeqCst);
                barrier.release.notified().await;
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "answered by the engine".into(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_model(
            provider,
            lash_core::ModelMetadata::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .build(owner())
        .expect("build the lash core")
}

/// What a host run answers: the outcome's status, its reply and its root.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Answer {
    answered: bool,
    reply: Option<String>,
    input_id: String,
    root: Option<String>,
}

fn answer(input_id: &lash::InputId, outcome: &lash::SendOutcome) -> Answer {
    Answer {
        answered: outcome.status() == lash::TurnStatus::Answered,
        reply: outcome
            .output()
            .and_then(|output| output.assistant_message().map(str::to_owned)),
        input_id: input_id.to_string(),
        root: outcome.root().map(ToString::to_string),
    }
}

#[restate_sdk::workflow]
trait DurableSendHost {
    async fn run() -> HandlerResult<Json<Answer>>;
}

struct Host {
    session: lash::LashSession,
}

impl DurableSendHost for Host {
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<Answer>> {
        let handle = self
            .session
            .send(lash::TurnInput::text("held host input"))
            // No host id: the binding mints one and journals it, so a
            // replayed handler resubmits under the same id.
            .accept_restate(&ctx)
            .await?;
        let input_id = handle.input_id().clone();
        let outcome = handle
            .outcome_restate(&ctx, RestateWait::new().probe_window(PROBE))
            .await?;
        Ok(Json(answer(&input_id, &outcome)))
    }
}

/// An object whose exclusive handler submits and whose shared handler waits.
#[restate_sdk::object]
trait ChatObject {
    async fn submit(text: String) -> HandlerResult<String>;
    async fn touch(note: String) -> HandlerResult<u64>;
    #[shared]
    async fn wait(input_id: String) -> HandlerResult<Json<Answer>>;
}

/// The session is set once the backend it runs over is up.
struct Chat {
    session: Arc<OnceLock<lash::LashSession>>,
}

impl Chat {
    fn session(&self) -> &lash::LashSession {
        self.session
            .get()
            .expect("the session is open before the object serves")
    }
}

impl ChatObject for Chat {
    async fn submit(&self, ctx: ObjectContext<'_>, text: String) -> HandlerResult<String> {
        let handle = self
            .session()
            .send(lash::TurnInput::text(text))
            .accept_restate(&ctx)
            .await?;
        Ok(handle.input_id().to_string())
    }

    async fn touch(&self, ctx: ObjectContext<'_>, _note: String) -> HandlerResult<u64> {
        let touches = ctx.get::<u64>("touches").await?.unwrap_or(0) + 1;
        ctx.set("touches", touches);
        Ok(touches)
    }

    async fn wait(
        &self,
        ctx: SharedObjectContext<'_>,
        input_id: String,
    ) -> HandlerResult<Json<Answer>> {
        let input_id = lash::InputId::from(input_id);
        let outcome = self
            .session()
            .attach(input_id.clone())
            .outcome_restate(&ctx, RestateWait::new().probe_window(PROBE))
            .await?;
        Ok(Json(answer(&input_id, &outcome)))
    }
}

/// A host that reaches its session inside its own handler, as the load
/// workload's turn does (FIG-4277): it creates or uses the session named by
/// its workflow key, sends to it and waits for the answer, then parks on its
/// `resume` promise so a law can delete the session under it, and on its
/// `finish` promise so the law can read the replayed journal before the
/// invocation completes. It keeps its answer in its state as well, where a
/// law reads it without holding a call open across the host's replays.
#[restate_sdk::workflow]
trait SessionHost {
    async fn run() -> HandlerResult<Json<Answer>>;
    #[shared]
    async fn release(promise: String) -> HandlerResult<String>;
    #[shared]
    async fn answer() -> HandlerResult<Json<Option<Answer>>>;
}

const SESSION_HOST: &str = "SessionHost";
const RESUME: &str = "resume";
const FINISH: &str = "finish";
const ANSWER: &str = "answer";

/// The core is set once the backend it runs over is up.
struct SessionHostService {
    core: Arc<OnceLock<lash::LashCore>>,
}

impl SessionHost for SessionHostService {
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<Answer>> {
        let core = self
            .core
            .get()
            .expect("the core is built before the host runs");
        let session = core
            .session(ctx.key())
            .create_or_use_restate(
                &ctx,
                lash::SessionCreation::root(lash::SessionSpec::new(
                    "mock-model",
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                )),
            )
            .await?;
        let handle = session
            .send(lash::TurnInput::text("sent before the session is deleted"))
            .accept_restate(&ctx)
            .await?;
        let input_id = handle.input_id().clone();
        let outcome = handle
            .outcome_restate(&ctx, RestateWait::new().probe_window(PROBE))
            .await?;
        ctx.set(ANSWER, Json(answer(&input_id, &outcome)));
        ctx.promise::<String>(RESUME).await?;
        ctx.promise::<String>(FINISH).await?;
        Ok(Json(answer(&input_id, &outcome)))
    }

    async fn release(
        &self,
        ctx: SharedWorkflowContext<'_>,
        promise: String,
    ) -> HandlerResult<String> {
        ctx.resolve_promise(&promise, "released".to_owned());
        Ok(promise)
    }

    async fn answer(&self, ctx: SharedWorkflowContext<'_>) -> HandlerResult<Json<Option<Answer>>> {
        Ok(Json(
            ctx.get::<Json<Answer>>(ANSWER)
                .await?
                .map(|Json(answer)| answer),
        ))
    }
}

struct World {
    backend: RestateTestBackend<dyn StoreSet>,
    barrier: Arc<Barrier>,
    session: lash::LashSession,
    core: lash::LashCore,
    /// What the stores live in: kept open for the world's life.
    _storage: Storage,
}

/// The store set a world's engine and hosts run over.
#[derive(Clone, Copy, Debug)]
enum Stores {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// What a world's stores live in, beyond the backend's own handles.
enum Storage {
    Memory,
    Files {
        _root: tempfile::TempDir,
    },
    Postgres {
        _storage: lash_postgres_store::PostgresStorage,
        _database: lash_postgres_store::testing::IsolatedDatabase,
        _attachments: tempfile::TempDir,
    },
}

const SEED: u64 = 0xd5_3837;

/// The PostgreSQL server required by a selected PostgreSQL law.
fn postgres_url() -> String {
    lash_postgres_store::testing::required_database_url()
}

/// A backend on the double over `stores`, requiring the PostgreSQL service
/// when the PostgreSQL tier is selected.
async fn backend_over(
    config: ServerConfig,
    stores: Stores,
) -> Option<(RestateTestBackend<dyn StoreSet>, Storage)> {
    match stores {
        Stores::SqliteMemory => {
            let backend = lash_restate_test::backend(SEED, config)
                .await
                .expect("build the Restate test backend");
            Some((backend.erase_store_type(), Storage::Memory))
        }
        Stores::SqliteFile => {
            let root = tempfile::tempdir().expect("the store directory");
            let backend = lash_restate_test::backend_with_store_set(
                SEED,
                config,
                lash_restate_test::DeploymentHooks::default(),
                |clock| {
                    let root = root.path().to_owned();
                    async move {
                        let stores =
                            lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
                                root,
                                lash_sqlite_store::SqliteStoreSetOptions {
                                    process_id_mint:
                                        lash_core::ProcessIdMint::sequential_for_testing(),
                                    ..lash_sqlite_store::SqliteStoreSetOptions::default()
                                },
                                clock,
                            )
                            .await
                            .map_err(|error| {
                                lash_restate_test::BackendError::Stores(error.to_string())
                            })?;
                        Ok(Arc::new(stores) as Arc<dyn StoreSet>)
                    }
                },
            )
            .await
            .expect("build the Restate test backend over SQLite files");
            Some((backend, Storage::Files { _root: root }))
        }
        Stores::Postgres => {
            let url = postgres_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("the attachment directory");
            let backend = lash_restate_test::backend_with_store_set(
                SEED,
                config,
                lash_restate_test::DeploymentHooks::default(),
                |clock| {
                    let stores = lash_postgres_store::PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(
                            attachments.path(),
                        )),
                        lash_core::WakeDeliveryConfig::default(),
                        clock,
                    );
                    async move { Ok(Arc::new(stores) as Arc<dyn StoreSet>) }
                },
            )
            .await
            .expect("build the Restate test backend over PostgreSQL");
            Some((
                backend,
                Storage::Postgres {
                    _storage: storage,
                    _database: database,
                    _attachments: attachments,
                },
            ))
        }
    }
}

async fn world(config: ServerConfig) -> World {
    world_over(config, Stores::SqliteMemory)
        .await
        .expect("SQLite memory stores are always available")
}

/// A world over `stores`, or `None` for PostgreSQL when no server is
/// configured.
async fn world_over(config: ServerConfig, stores: Stores) -> Option<World> {
    world_over_gated(config, stores, None).await
}

async fn world_over_gated(
    config: ServerConfig,
    stores: Stores,
    gate: Option<Arc<session_delete::LifecycleGate>>,
) -> Option<World> {
    let (backend, storage) = backend_over(config, stores).await?;
    let barrier = Arc::new(Barrier::default());
    let runtime_backend = match gate {
        Some(gate) => session_delete::gated_backend(backend.lash_backend(), gate),
        None => backend.lash_backend(),
    };
    let core = core(runtime_backend, &barrier);
    let session = created_session(&core, SESSION)
        .await
        .open()
        .await
        .expect("open the session");
    backend
        .server()
        .register(
            Endpoint::builder()
                .bind(
                    Host {
                        session: session.clone(),
                    }
                    .serve(),
                )
                .bind(
                    Chat {
                        session: Arc::new(OnceLock::from(session.clone())),
                    }
                    .serve(),
                )
                .bind(
                    SessionHostService {
                        core: Arc::new(OnceLock::from(core.clone())),
                    }
                    .serve(),
                )
                .build(),
        )
        .await
        .expect("register the host endpoint");
    Some(World {
        backend,
        barrier,
        session,
        core,
        _storage: storage,
    })
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// The names of the `ctx.run` steps journaled on `service`'s invocations.
fn journaled_runs(backend: &RestateTestBackend<dyn StoreSet>, service: &str) -> Vec<String> {
    let server = backend.server();
    server
        .invocations()
        .into_iter()
        .filter(|invocation| invocation.target.starts_with(service))
        .flat_map(|invocation| server.journal(&invocation.id).unwrap_or_default())
        .filter_map(|entry| entry.name)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The host's journal holds its binding steps, never the turn: no drive
/// admission, seal, model call or commit ran on the host's handler.
fn assert_host_never_drove(backend: &RestateTestBackend<dyn StoreSet>, service: &str) {
    let runs = journaled_runs(backend, service);
    assert!(
        runs.iter().any(|name| name == "lash.host.accept"),
        "the host journaled its acceptance: {runs:?}"
    );
    let foreign = runs
        .iter()
        .filter(|name| !name.starts_with("lash.host."))
        .collect::<Vec<_>>();
    assert!(
        foreign.is_empty(),
        "host submission never executes a turn, yet its journal holds {foreign:?}"
    );
}

async fn run_host(world: &World) -> tokio::task::JoinHandle<Answer> {
    let ingress = world.backend.ingress();
    tokio::spawn(async move {
        ingress
            .call_workflow_empty::<Answer>("DurableSendHost", "host", "run")
            .await
            .expect("the host answers")
    })
}

/// A handler that dies after its acceptance committed, before the journal
/// kept it, re-runs the acceptance under the same journaled id: the store
/// answers the original acceptance, and the turn runs once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_replayed_after_acceptance_submits_once() {
    let world = world(ServerConfig::default()).await;
    world.backend.server().crash_on(
        CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some("lash.host.accept".into()),
        })
        .service("DurableSendHost")
        .within_attempts(1),
    );
    let run = run_host(&world).await;
    until("the engine calls the model while the host waits", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
            && world.backend.server().stats().crashes == 1
    })
    .await;
    let pending = world
        .session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("pending inputs");
    assert_eq!(pending.len(), 1, "one acceptance, however many attempts");
    world.barrier.release.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the host finishes")
        .expect("join");
    assert!(answer.answered, "{answer:?}");
    assert_eq!(answer.reply.as_deref(), Some("answered by the engine"));
    assert_eq!(answer.input_id, pending[0].input.input_id.to_string());
    assert_eq!(world.barrier.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        world
            .session
            .durable()
            .turn_input_applications()
            .await
            .expect("applications")
            .len(),
        1
    );
    assert!(world.backend.server().stats().replays > 0);
    assert_host_never_drove(&world.backend, "DurableSendHost");
}

/// With the inactivity timeout at zero, the host suspends at every await its
/// journal cannot answer and replays its whole journal on each resume; a turn
/// held across many probes outlives the handler's timer many times over, and
/// the host still answers once, with the one turn the engine ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_waits_out_a_turn_longer_than_its_handler_timeouts() {
    let world = world(ServerConfig::default().always_replay(true)).await;
    let run = run_host(&world).await;
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    until("the host has waited through several probes", || {
        journaled_runs(&world.backend, "DurableSendHost")
            .iter()
            .filter(|name| *name == "lash.host.outcome")
            .count()
            >= 3
    })
    .await;
    world.barrier.release.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the host finishes")
        .expect("join");
    assert!(answer.answered, "{answer:?}");
    assert_eq!(world.barrier.calls.load(Ordering::SeqCst), 1);
    let host = world
        .backend
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| invocation.target.starts_with("DurableSendHost"))
        .expect("the host invocation");
    assert!(
        host.suspensions >= 3,
        "the host suspended and replayed while it waited: {host:?}"
    );
    assert_host_never_drove(&world.backend, "DurableSendHost");
}

/// An exclusive handler accepts and returns the receipt; a shared handler
/// waits. While it waits, the object's exclusive handlers stay callable (a
/// turn's own calls to its host object would never queue behind the wait),
/// and the waiter answers once the engine's turn settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exclusive_handler_accepts_and_a_shared_handler_waits() {
    let world = world(ServerConfig::default()).await;
    let ingress = world.backend.ingress();
    let input_id = ingress
        .call_object_json::<_, String>("ChatObject", "chat", "submit", &"object input")
        .await
        .expect("the exclusive handler accepts and returns");
    let waiter = tokio::spawn({
        let ingress = world.backend.ingress();
        let input_id = input_id.clone();
        async move {
            ingress
                .call_object_json::<_, Answer>("ChatObject", "chat", "wait", &input_id)
                .await
                .expect("the shared handler answers")
        }
    });
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    let touches = tokio::time::timeout(
        Duration::from_secs(10),
        ingress.call_object_json::<_, u64>("ChatObject", "chat", "touch", &"from the test"),
    )
    .await
    .expect("the object's lock is free while its shared handler waits")
    .expect("touch");
    assert_eq!(touches, 1);
    world.barrier.release.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("the waiter finishes")
        .expect("join");
    assert!(answer.answered, "{answer:?}");
    assert_eq!(answer.input_id, input_id);
    assert_host_never_drove(&world.backend, "ChatObject");
}

/// Deletes a session inside a handler attempt, as the load workload's cleanup
/// does.
struct DeleteExecution<'a> {
    administration: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl lash_core::SessionDeleteExecution for DeleteExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.administration
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

/// A handler attempt that deletes `session_id`, and where the last attempt
/// records what the deletion answered.
async fn deletion(
    core: &lash::LashCore,
    session_id: &str,
) -> (
    HandlerAttempt,
    Arc<Mutex<Option<lash::Result<lash::SessionDeletion>>>>,
) {
    let administration = core.session_administration().await;
    let answered = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&answered);
    let session_id = session_id.to_owned();
    let attempt: HandlerAttempt = Arc::new(move |scoped| {
        let execution = DeleteExecution {
            administration: administration.clone(),
            scoped,
        };
        let session_id = session_id.clone();
        let slot = Arc::clone(&slot);
        Box::pin(async move {
            let context = lash_core::SessionDeleteContext::from_execution(&execution, &session_id)
                .expect("the delete context");
            let deletion = lash::LashCore::delete_session(context).await;
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(deletion);
        })
    });
    (attempt, answered)
}

/// Delete once after each observed closure change, then await physical
/// completion. A retained pin is read without reissuing delete, and Closing
/// leaves delivery to the finalizer rather than repeating the close.
async fn delete_session<F, Fut>(core: &lash::LashCore, session_id: &str, run: F)
where
    F: Fn(HandlerAttempt) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let id = lash::SessionId::from(session_id);
    loop {
        let (attempt, slot) = deletion(core, session_id).await;
        run(attempt).await.expect("the delete handler runs");
        let answer = slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("the deletion answered");
        match answer {
            Err(lash::EmbedError::Store(
                lash_core::StoreError::TurnCancelClosureLifecyclePinned { .. },
            )) => {
                core.await_turn_cancel_closures(&id)
                    .await
                    .expect("the closure ends");
            }
            Ok(
                lash::SessionDeletion::Deleted(_) | lash::SessionDeletion::AlreadyDeleted { .. },
            ) => return,
            Ok(lash::SessionDeletion::Closing(_)) => {
                let completed = core
                    .await_session_deletion(&id)
                    .await
                    .expect("observe deletion");
                assert_eq!(
                    completed,
                    lash::SessionDeleteCompletion::Deleted,
                    "`{session_id}` delete did not complete"
                );
                return;
            }
            Ok(lash::SessionDeletion::Absent { .. }) => {
                panic!("`{session_id}` was never created, so nothing was deleted")
            }
            Err(error) => panic!("`{session_id}` could not be deleted: {error:?}"),
        }
    }
}

/// How a law makes the parked host replay after its session is deleted.
#[derive(Clone, Copy, Debug)]
enum Replay {
    /// The server suspends the host at every await and replays its whole
    /// journal on each resume (inactivity timeout zero).
    EveryAwait,
    /// The host's attempt dies where it is parked, as a killed worker's
    /// does, and the server replays the invocation into the deployment.
    Kill,
}

fn diverged(failure: &str) -> bool {
    failure.contains("RT0016") || failure.contains("Journal mismatch") || failure.contains("570")
}

/// The FIG-4277 law on the double: kill mid-turn, replay after the session
/// is gone, identical journal. The host answered its input and parked; its
/// session is deleted under it; its replay reaches the session through the
/// journal, so it reads back every step it recorded, parks again and answers.
async fn a_host_replayed_after_its_session_was_deleted_keeps_its_journal(replay: Replay) {
    let config = match replay {
        Replay::EveryAwait => ServerConfig::default().always_replay(true),
        Replay::Kill => ServerConfig::default(),
    };
    let world = world(config).await;
    let key = "deleted-under-its-host";
    let target = format!("{SESSION_HOST}/{key}/run");
    let ingress = world.backend.ingress();
    let run = tokio::spawn({
        let ingress = world.backend.ingress();
        async move {
            ingress
                .call_workflow_empty::<Answer>(SESSION_HOST, key, "run")
                .await
                .expect("the host answers")
        }
    });
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    world.barrier.release.notify_one();
    let server = world.backend.server();
    let host = || {
        server
            .invocations()
            .into_iter()
            .find(|invocation| invocation.target == target)
    };
    let promises = |id: &str| {
        server
            .journal(id)
            .unwrap_or_default()
            .iter()
            .filter(|entry| entry.ty == MessageType::GetPromiseCommand)
            .count()
    };
    until("the host parks on its first promise", || {
        host().is_some_and(|host| promises(&host.id) == 1)
    })
    .await;
    let parked = host().expect("the host invocation");
    let recorded = server.journal(&parked.id).expect("the host's journal");
    assert_eq!(
        recorded
            .iter()
            .find_map(|entry| entry.name.clone().filter(|name| !name.is_empty()))
            .as_deref(),
        Some("lash.host.session"),
        "the host reached its session inside its journal"
    );

    let deleting = tokio::spawn({
        let core = world.core.clone();
        let backend = world.backend.clone();
        async move {
            delete_session(&core, key, |attempt| {
                backend.run_in_handler(
                    lash_core::AdmittedScope::session_delete(lash::SessionId::from(key)),
                    attempt,
                )
            })
            .await;
        }
    });
    // A parked host keeps the double's automatic clock fixed. Once cleanup
    // settles, make the finalizer's already owed retry eligible explicitly.
    session_delete::finish_session_cleanup(&world, key).await;
    deleting.await.expect("physical deletion completed");
    if let Replay::Kill = replay {
        assert!(
            server.crash(&parked.id),
            "the parked host's attempt was running"
        );
    }
    ingress
        .call_workflow_json::<_, String>(SESSION_HOST, key, "release", &RESUME)
        .await
        .expect("resume the host");

    let reparked = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let host = host().expect("the host invocation");
            if let Some((code, message)) = &host.last_failure
                && (*code == 570 || diverged(message))
            {
                panic!("the host's replay diverged from its journal: [{code}] {message}");
            }
            if promises(&host.id) == 2 {
                return host;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the replayed host parks on its second promise");
    let replayed = server.journal(&reparked.id).expect("the host's journal");
    assert_eq!(
        replayed.get(..recorded.len()),
        Some(&recorded[..]),
        "the replay kept every entry the host recorded"
    );
    match replay {
        Replay::EveryAwait => assert!(reparked.suspensions >= 2, "{reparked:?}"),
        Replay::Kill => assert!(reparked.attempts >= 2, "{reparked:?}"),
    }

    ingress
        .call_workflow_json::<_, String>(SESSION_HOST, key, "release", &FINISH)
        .await
        .expect("finish the host");
    let answer = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the host finishes")
        .expect("join");
    assert!(answer.answered, "{answer:?}");
    assert_eq!(answer.reply.as_deref(), Some("answered by the engine"));
    assert_eq!(world.barrier.calls.load(Ordering::SeqCst), 1);
    let finished = host().expect("the host invocation");
    assert_eq!(finished.status, "completed", "{finished:?}");
    assert_host_never_drove(&world.backend, SESSION_HOST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_replayed_at_every_await_after_its_session_was_deleted_keeps_its_journal() {
    a_host_replayed_after_its_session_was_deleted_keeps_its_journal(Replay::EveryAwait).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_killed_after_its_session_was_deleted_replays_its_journal() {
    a_host_replayed_after_its_session_was_deleted_keeps_its_journal(Replay::Kill).await;
}

async fn live_host(
    backend: &LiveRestateBackend,
    target: &str,
) -> Option<lash_restate_test::live::LiveInvocation> {
    backend
        .invocations()
        .await
        .expect("read the server's invocations")
        .into_iter()
        .find(|invocation| invocation.target == target)
}

/// The same law against a live `restate-server`: the deployment dies where
/// the host is parked, after its session was deleted, and the server's own
/// retry replays the invocation into the deployment that comes back. On the
/// suite's replay leg the host also suspends and replays at every await.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait Restate suite runs it"]
async fn live_restate_host_killed_after_its_session_was_deleted_replays_its_journal() {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment names {name}"))
    };
    let key = format!(
        "deleted-under-its-host-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let cell = Arc::new(OnceLock::new());
    let backend = LiveRestateBackend::start_with_services(
        LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("HSW_BIND").parse().expect("endpoint bind"),
            endpoint_url: env("HSW_URL"),
            run_tag: key.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        },
        {
            let cell = Arc::clone(&cell);
            move |builder| builder.bind(SessionHostService { core: cell }.serve())
        },
    )
    .await
    .expect("start the live backend");
    let barrier = Arc::new(Barrier::default());
    let core = core(backend.lash_backend(), &barrier);
    assert!(
        cell.set(core.clone()).is_ok(),
        "the host's core is set once"
    );
    let target = format!("{SESSION_HOST}/{key}/run");
    let ingress = backend.ingress();
    // The call only starts the host: under the replay leg its answer can
    // take longer than the ingress client's call deadline, so the law reads
    // the answer from the host's state and its completion from the server.
    tokio::spawn({
        let ingress = backend.ingress();
        let key = key.clone();
        async move {
            let _started = ingress
                .call_workflow_empty::<Answer>(SESSION_HOST, &key, "run")
                .await;
        }
    });
    until("the engine calls the model", || {
        barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    barrier.release.notify_one();
    let promises = |journal: &[String]| {
        journal
            .iter()
            .filter(|entry| entry.contains("Command: GetPromise"))
            .count()
    };
    let parked = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(host) = live_host(&backend, &target).await {
                let journal = backend.journal(&host.id).await.expect("the host's journal");
                if promises(&journal) == 1 {
                    return (host, journal);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the host parks on its first promise");
    let (parked, recorded) = parked;
    assert!(
        recorded
            .iter()
            .find(|entry| entry.contains("Run"))
            .is_some_and(|entry| entry.ends_with(":lash.host.session")),
        "the host reached its session inside its journal: {recorded:?}"
    );

    delete_session(&core, &key, |attempt| {
        backend.run_in_handler(
            lash_core::AdmittedScope::session_delete(lash::SessionId::from(key.as_str())),
            attempt,
        )
    })
    .await;
    // The worker dies where the host is parked (on the replay leg the host
    // is suspended there) and comes back; the server replays the host into it.
    backend.stop_serving(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    backend
        .start_serving()
        .await
        .expect("the killed deployment serves again");
    ingress
        .call_workflow_json::<_, String>(SESSION_HOST, &key, "release", &RESUME)
        .await
        .expect("resume the host");

    let replayed = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let host = live_host(&backend, &target)
                .await
                .expect("the host invocation");
            if let Some(failure) = &host.last_failure
                && diverged(failure)
            {
                panic!("the host's replay diverged from its journal: {failure}");
            }
            let journal = backend.journal(&host.id).await.expect("the host's journal");
            if promises(&journal) == 2 {
                return journal;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let Ok(replayed) = replayed else {
        let host = live_host(&backend, &target).await;
        let journal = match &host {
            Some(host) => backend.journal(&host.id).await.unwrap_or_default(),
            None => Vec::new(),
        };
        panic!(
            "the replayed host never parked on its second promise: {host:?}\nrecorded {recorded:?}\nnow {journal:?}"
        );
    };
    assert_eq!(
        replayed.get(..recorded.len()),
        Some(&recorded[..]),
        "the replay kept every entry the host recorded"
    );

    ingress
        .call_workflow_json::<_, String>(SESSION_HOST, &key, "release", &FINISH)
        .await
        .expect("finish the host");
    let outcome = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(outcome) = backend
                .outcome(&parked.id)
                .await
                .expect("read the host's outcome")
            {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the host completes");
    assert_eq!(outcome, Ok(()), "the host completed");
    let answer = ingress
        .call_workflow_empty::<Option<Answer>>(SESSION_HOST, &key, "answer")
        .await
        .expect("read the host's answer")
        .expect("the host kept its answer");
    assert!(answer.answered, "{answer:?}");
    assert_eq!(answer.reply.as_deref(), Some("answered by the engine"));
    assert_eq!(barrier.calls.load(Ordering::SeqCst), 1);
    backend.finish().await;
}

/// FIG-4346 against a live `restate-server`: the engine's root run dies
/// after it journaled its admission (`drive-admit`) and before its head
/// inspection (`drive-head`), the session's storage delete commits while it
/// is down, and the server's retry replays the run into the deployment that
/// comes back. The host still holds the session, so the drive runs on its
/// resident runtime, whose head refresh before the admission meets the
/// tombstone. The replay follows the journal (never `RT0016`), its head
/// inspection records the retirement, and the run ends with the typed
/// `SessionDeleted` refusal. On the suite's replay leg the run also
/// suspends and replays at every await.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait Restate suite runs it"]
async fn live_restate_root_killed_after_its_session_was_deleted_ends_typed() {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment names {name}"))
    };
    let key = format!(
        "deleted-under-its-root-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let backend = LiveRestateBackend::start(LiveConfig {
        ingress_url: env("RESTATE_INGRESS_URL"),
        admin_url: env("RESTATE_ADMIN_URL"),
        endpoint_bind: env("HSW_BIND").parse().expect("endpoint bind"),
        endpoint_url: env("HSW_URL"),
        run_tag: key.clone(),
        namespace: lash_restate::RestateNamespace::default(),
    })
    .await
    .expect("start the live backend");
    let barrier = Arc::new(Barrier::default());
    let core = core(backend.lash_backend(), &barrier);
    let session_id = lash::SessionId::from(key.as_str());
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await
        .expect("open the session");
    let root = lash_core::TurnId::from("deleted-replay-root");
    let turn_key = lash_restate::turn_workflow_key(&session_id, &root);
    // The run dies with its admission journaled and its head inspection not.
    backend.crash_on(
        CrashRule::new(CrashPoint::BeforeRun {
            name: format!("lash:drive-head:{root}"),
        })
        .service(backend.service_name(lash_restate_test::TURN_DRIVER_SERVICE))
        .key(turn_key.clone()),
    );
    // The storage delete commits while the dead run is down: the listener
    // runs as the deployment dies, and the delete is the store's alone.
    let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = backend.lash_backend().session_store_factory();
    assert!(backend.on_crash(CrashCount::new().listener_with({
        let deleted = Arc::clone(&deleted);
        let session_id = session_id.clone();
        move |_target: &str| {
            let factory = Arc::clone(&factory);
            let session_id = session_id.clone();
            let delete = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the delete")
                    .block_on(async {
                        // The dead run's writer can still hold the session for
                        // a moment: a contended delete is retried until a
                        // bounded deadline.
                        let deadline = std::time::Instant::now() + Duration::from_secs(30);
                        loop {
                            match factory.delete_session(&session_id).await {
                                Ok(_) => return,
                                Err(error)
                                    if format!("{error:?}").contains("Contended")
                                        && std::time::Instant::now() < deadline =>
                                {
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                                Err(error) => panic!("the storage delete commits: {error:?}"),
                            }
                        }
                    });
            })
            .join();
            deleted.store(delete.is_ok(), Ordering::SeqCst);
        }
    })));
    let _handle = session
        .send(lash::TurnInput::text("delete me mid-root"))
        .id(root.clone())
        .await
        .expect("the input is accepted");
    until("the run dies before its head inspection", || {
        deleted.load(Ordering::SeqCst)
    })
    .await;
    backend
        .start_serving()
        .await
        .expect("the killed deployment serves again");

    let target = format!(
        "{}/{turn_key}/run",
        backend.service_name(lash_restate_test::TURN_DRIVER_SERVICE)
    );
    let ended = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if let Some(run) = live_host(&backend, &target).await {
                if let Some(failure) = &run.last_failure
                    && diverged(failure)
                {
                    panic!("the root's replay diverged from its journal: {failure}");
                }
                if let Some(outcome) = backend.outcome(&run.id).await.expect("the run's outcome") {
                    let journal = backend.journal(&run.id).await.expect("the run's journal");
                    return (outcome, journal);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let Ok((outcome, journal)) = ended else {
        let run = live_host(&backend, &target).await;
        let journal = match &run {
            Some(run) => backend.journal(&run.id).await.unwrap_or_default(),
            None => Vec::new(),
        };
        panic!("the replayed root never ended: {run:?}\njournal {journal:?}");
    };
    for step in [
        "drive-root-start:",
        "drive-seal:",
        "drive-admit:",
        "drive-head:",
    ] {
        assert!(
            journal.iter().any(|entry| entry.contains(step)),
            "the replay issued the recorded steps and the head inspection after them, \
             missing `{step}`: {journal:?}"
        );
    }
    assert!(
        matches!(&outcome, Err(failure) if failure.contains("session_deleted")),
        "the root's run ends with the typed retirement: {outcome:?}\njournal {journal:?}"
    );
    assert_eq!(
        barrier.calls.load(Ordering::SeqCst),
        0,
        "the deleted session's root called no model"
    );
    drop(session);
    backend.finish().await;
}

/// Where root `root`'s first physical turn in `session` publishes its
/// terminal: the durable wait's address, which names the wait's workflow key
/// and its session's wait index.
async fn terminal_wait(
    backend: &lash_core::Backend,
    session: &str,
    root: &lash_core::TurnId,
) -> lash_restate::RestateDurableWaitAddress {
    let address = lash_core::facade_support::TurnAddress::new(
        lash_core::SessionId::from(session),
        lash_core::store::PhysicalTurn::derive_turn_id(root, 0),
    );
    let key = lash_core::AwaitEventResolver::await_event_key(
        backend.effect_host().as_ref(),
        &address.execution_scope(),
        lash_core::AwaitEventWaitIdentity::TurnTerminal,
    )
    .await
    .expect("the turn's terminal key");
    lash_restate::RestateDurableWaitAddress::for_key(&key)
}

/// The `Service/key/handler` of an `await_resolution` invocation on `wait`,
/// under `durable_wait_workflow`, the wait workflow's name in the backend's
/// namespace.
fn await_resolution_target(
    durable_wait_workflow: &str,
    wait: &lash_restate::RestateDurableWaitAddress,
) -> String {
    format!(
        "{durable_wait_workflow}/{}/await_resolution",
        wait.workflow_key
    )
}

/// The server-side `await_resolution` invocations the double holds on
/// `wait`, finished ones included.
fn terminal_attaches(
    backend: &RestateTestBackend<dyn StoreSet>,
    wait: &lash_restate::RestateDurableWaitAddress,
) -> usize {
    let target = await_resolution_target(&backend.service_name("LashDurableWaitWorkflow"), wait);
    backend
        .server()
        .invocations()
        .into_iter()
        .filter(|invocation| invocation.target == target)
        .count()
}

/// The root `input_id` is bound to, once a drive admitted it.
async fn root_of_input(
    backend: &lash_core::Backend,
    session: &str,
    input_id: &lash::InputId,
) -> lash_core::TurnId {
    let store = backend.session_store_factory();
    let session_id = lash_core::SessionId::from(session);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(root) =
                lash_core::store::RootStore::root_of_input(store.as_ref(), &session_id, input_id)
                    .await
                    .expect("read the input's root")
            {
                return root;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a drive admits the input")
}

fn skipped_without_postgres() {
    eprintln!("skipped: no PostgreSQL server is configured (LASH_POSTGRES_DATABASE_URL)");
}

/// Law A1 (FIG-4345) on the double. A committed root answers its follower
/// from the store alone: the root commits and its first follower answers;
/// then the session's durable-wait index is held, as a backlog of exclusive
/// calls holds it under load. A second follower of the same input, which no
/// run in this process can hand a report, still answers the committed
/// outcome while the hold is in place, and the root's terminal key never
/// holds more than one server-side `await_resolution` waiter, however many
/// resolve passes ran.
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
    stores: Stores,
    config: ServerConfig,
) {
    let Some(world) = world_over(config, stores).await else {
        skipped_without_postgres();
        return;
    };
    let ingress = world.backend.ingress();
    let input_id = ingress
        .call_object_json::<_, String>(
            "ChatObject",
            "chat",
            "submit",
            &"committed, then backlogged",
        )
        .await
        .expect("the exclusive handler accepts and returns");
    let first = tokio::spawn({
        let ingress = world.backend.ingress();
        let input_id = input_id.clone();
        async move {
            ingress
                .call_object_json::<_, Answer>("ChatObject", "chat", "wait", &input_id)
                .await
                .expect("the first follower answers")
        }
    });
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    world.barrier.release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(30), first)
        .await
        .expect("the first follower finishes")
        .expect("join");
    assert!(first.answered, "{first:?}");
    let root = lash_core::TurnId::from(first.root.clone().expect("a settled input names its root"));

    let wait = terminal_wait(&world.backend.lash_backend(), SESSION, &root).await;
    let hold = world
        .backend
        .server()
        .hold(
            &world.backend.service_name("LashDurableWaitIndex"),
            &wait.index_key(),
        )
        .await;
    let second = tokio::time::timeout(
        Duration::from_secs(20),
        ingress.call_object_json::<_, Answer>("ChatObject", "chat", "wait", &input_id),
    )
    .await
    .expect("a committed root answers its follower while its session's wait index is held")
    .expect("the second follower answers");
    assert!(second.answered, "{second:?}");
    assert_eq!(second.reply.as_deref(), Some("answered by the engine"));
    assert_eq!(second.root, first.root);
    let attaches = terminal_attaches(&world.backend, &wait);
    assert!(
        attaches <= 1,
        "the root's terminal key holds at most one server-side waiter, not {attaches}"
    );
    drop(hold);
    assert_eq!(world.barrier.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_sqlite_memory()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::SqliteMemory,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_sqlite_memory_replaying()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::SqliteMemory,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_sqlite_file()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::SqliteFile,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_sqlite_file_replaying()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::SqliteFile,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_postgres()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::Postgres,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged_on_postgres_replaying()
 {
    a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged(
        Stores::Postgres,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

/// The committed outcome a terminal carries, as JSON: a terminal has no
/// equality of its own.
fn committed_outcome(terminal: &lash_core::facade_support::TurnTerminal) -> serde_json::Value {
    match terminal {
        lash_core::facade_support::TurnTerminal::Committed { outcome, .. } => {
            serde_json::to_value(outcome).expect("encode the outcome")
        }
        lash_core::facade_support::TurnTerminal::Failed { error } => {
            panic!("the turn committed, yet its terminal failed: {error:?}")
        }
    }
}

/// Law A2 (FIG-4345) on the double. A dropped terminal attach leaves no
/// second server invocation: an attach to a running turn's terminal is
/// dropped after 250 ms, as a follower's bounded read drops it; a second
/// attach joins the one server-side waiter the first opened; once the turn
/// commits, both that attach and a third made after the terminal resolved
/// read the same terminal, and the terminal key holds one `await_resolution`
/// invocation in all.
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation(
    stores: Stores,
    config: ServerConfig,
) {
    let Some(world) = world_over(config, stores).await else {
        skipped_without_postgres();
        return;
    };
    let input_id = world
        .backend
        .ingress()
        .call_object_json::<_, String>("ChatObject", "chat", "submit", &"attached twice")
        .await
        .expect("the exclusive handler accepts and returns");
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    let backend = world.backend.lash_backend();
    let root = root_of_input(&backend, SESSION, &lash::InputId::from(input_id)).await;
    let wait = terminal_wait(&backend, SESSION, &root).await;
    let attach = backend
        .effect_host()
        .turn_attach()
        .expect("a Restate host attaches to turns");
    let address = lash_core::facade_support::TurnAddress::new(
        lash_core::SessionId::from(SESSION),
        lash_core::store::PhysicalTurn::derive_turn_id(&root, 0),
    );
    let attached = |attach: Arc<dyn lash_core::facade_support::TurnAttach>| {
        let address = address.clone();
        tokio::spawn(async move { attach.await_terminal(&address).await })
    };

    let dropped = attached(Arc::clone(&attach));
    until("the first attach opens its server-side waiter", || {
        terminal_attaches(&world.backend, &wait) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    dropped.abort();
    assert!(
        dropped.await.is_err_and(|error| error.is_cancelled()),
        "the first attach was still waiting when it was dropped"
    );
    let reattached = attached(Arc::clone(&attach));
    tokio::time::sleep(Duration::from_millis(250)).await;
    world.barrier.release.notify_one();
    let terminal = tokio::time::timeout(Duration::from_secs(30), reattached)
        .await
        .expect("the re-attach answers once the turn commits")
        .expect("join")
        .expect("the re-attach reads the terminal");
    let after = attach
        .await_terminal(&address)
        .await
        .expect("an attach after the terminal resolved reads it");
    assert_eq!(committed_outcome(&terminal), committed_outcome(&after));
    assert_eq!(
        terminal_attaches(&world.backend, &wait),
        1,
        "the terminal key holds one server-side waiter however often it was attached"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_sqlite_memory() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::SqliteMemory,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_sqlite_memory_replaying() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::SqliteMemory,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_sqlite_file() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::SqliteFile,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_sqlite_file_replaying() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::SqliteFile,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_postgres() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::Postgres,
        ServerConfig::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_dropped_terminal_attach_leaves_no_second_server_invocation_on_postgres_replaying() {
    a_dropped_terminal_attach_leaves_no_second_server_invocation(
        Stores::Postgres,
        ServerConfig::default().always_replay(true),
    )
    .await;
}

/// A live `restate-server` world for the FIG-4345 laws: the suite's server,
/// this binary's endpoint serving `ChatObject` over a fresh session named
/// `key`, which also keys the object.
struct LiveWorld {
    backend: LiveRestateBackend,
    barrier: Arc<Barrier>,
    key: String,
    _session: lash::LashSession,
    _core: lash::LashCore,
}

async fn live_world(name: &str) -> LiveWorld {
    live_world_gated(name, None).await
}

async fn live_world_gated(
    name: &str,
    gate: Option<Arc<session_delete::LifecycleGate>>,
) -> LiveWorld {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment names {name}"))
    };
    let key = format!(
        "{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let cell = Arc::new(OnceLock::new());
    let backend = LiveRestateBackend::start_with_services(
        LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("HSW_BIND").parse().expect("endpoint bind"),
            endpoint_url: env("HSW_URL"),
            run_tag: key.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        },
        {
            let cell = Arc::clone(&cell);
            move |builder| builder.bind(Chat { session: cell }.serve())
        },
    )
    .await
    .expect("start the live backend");
    let barrier = Arc::new(Barrier::default());
    let runtime_backend = match gate {
        Some(gate) => session_delete::gated_backend(backend.lash_backend(), gate),
        None => backend.lash_backend(),
    };
    let core = core(runtime_backend, &barrier);
    let session = created_session(&core, key.as_str())
        .await
        .open()
        .await
        .expect("open the session");
    assert!(
        cell.set(session.clone()).is_ok(),
        "the object's session is set once"
    );
    LiveWorld {
        backend,
        barrier,
        key,
        _session: session,
        _core: core,
    }
}

/// The server-side `await_resolution` invocations the live server holds on
/// `wait`, finished ones included: the `sys_invocation` census.
async fn live_terminal_attaches(
    backend: &LiveRestateBackend,
    wait: &lash_restate::RestateDurableWaitAddress,
) -> usize {
    let target = await_resolution_target(&backend.service_name("LashDurableWaitWorkflow"), wait);
    backend
        .invocations()
        .await
        .expect("read the server's invocations")
        .into_iter()
        .filter(|invocation| invocation.target == target)
        .count()
}

/// Law A1 against a live `restate-server`, its session's wait index held
/// through [`LiveRestateBackend::hold`].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait Restate suite runs it"]
async fn live_restate_a_committed_root_answers_its_follower_while_the_session_wait_index_is_backlogged()
 {
    let world = live_world("backlogged-wait-index").await;
    let ingress = world.backend.ingress();
    let input_id = ingress
        .call_object_json::<_, String>(
            "ChatObject",
            &world.key,
            "submit",
            &"committed, then backlogged",
        )
        .await
        .expect("the exclusive handler accepts and returns");
    let first = tokio::spawn({
        let ingress = world.backend.ingress();
        let key = world.key.clone();
        let input_id = input_id.clone();
        async move {
            ingress
                .call_object_json::<_, Answer>("ChatObject", &key, "wait", &input_id)
                .await
                .expect("the first follower answers")
        }
    });
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    world.barrier.release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(60), first)
        .await
        .expect("the first follower finishes")
        .expect("join");
    assert!(first.answered, "{first:?}");
    let root = lash_core::TurnId::from(first.root.clone().expect("a settled input names its root"));

    let wait = terminal_wait(&world.backend.lash_backend(), &world.key, &root).await;
    let hold = world.backend.hold(
        &world.backend.service_name("LashDurableWaitIndex"),
        Some(&wait.index_key()),
    );
    let second = tokio::time::timeout(
        Duration::from_secs(60),
        ingress.call_object_json::<_, Answer>("ChatObject", &world.key, "wait", &input_id),
    )
    .await
    .expect("a committed root answers its follower while its session's wait index is held")
    .expect("the second follower answers");
    assert!(second.answered, "{second:?}");
    assert_eq!(second.reply.as_deref(), Some("answered by the engine"));
    assert_eq!(second.root, first.root);
    let attaches = live_terminal_attaches(&world.backend, &wait).await;
    assert!(
        attaches <= 1,
        "the root's terminal key holds at most one server-side waiter, not {attaches}"
    );
    hold.release();
    world.backend.finish().await;
}

/// Law A2 against a live `restate-server`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait Restate suite runs it"]
async fn live_restate_a_dropped_terminal_attach_leaves_no_second_server_invocation() {
    let world = live_world("dropped-terminal-attach").await;
    let input_id = world
        .backend
        .ingress()
        .call_object_json::<_, String>("ChatObject", &world.key, "submit", &"attached twice")
        .await
        .expect("the exclusive handler accepts and returns");
    until("the engine calls the model", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    let backend = world.backend.lash_backend();
    let root = root_of_input(&backend, &world.key, &lash::InputId::from(input_id)).await;
    let wait = terminal_wait(&backend, &world.key, &root).await;
    let attach = backend
        .effect_host()
        .turn_attach()
        .expect("a Restate host attaches to turns");
    let address = lash_core::facade_support::TurnAddress::new(
        lash_core::SessionId::from(world.key.as_str()),
        lash_core::store::PhysicalTurn::derive_turn_id(&root, 0),
    );
    let attached = |attach: Arc<dyn lash_core::facade_support::TurnAttach>| {
        let address = address.clone();
        tokio::spawn(async move { attach.await_terminal(&address).await })
    };

    let dropped = attached(Arc::clone(&attach));
    tokio::time::timeout(Duration::from_secs(60), async {
        while live_terminal_attaches(&world.backend, &wait).await == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the first attach opens its server-side waiter");
    tokio::time::sleep(Duration::from_millis(250)).await;
    dropped.abort();
    assert!(
        dropped.await.is_err_and(|error| error.is_cancelled()),
        "the first attach was still waiting when it was dropped"
    );
    let reattached = attached(Arc::clone(&attach));
    tokio::time::sleep(Duration::from_millis(250)).await;
    world.barrier.release.notify_one();
    let terminal = tokio::time::timeout(Duration::from_secs(60), reattached)
        .await
        .expect("the re-attach answers once the turn commits")
        .expect("join")
        .expect("the re-attach reads the terminal");
    let after = attach
        .await_terminal(&address)
        .await
        .expect("an attach after the terminal resolved reads it");
    assert_eq!(committed_outcome(&terminal), committed_outcome(&after));
    assert_eq!(
        live_terminal_attaches(&world.backend, &wait).await,
        1,
        "the terminal key holds one server-side waiter however often it was attached"
    );
    world.backend.finish().await;
}

/// The task the frame switch of the follow-on leg hands its follow-on; the
/// follow-on frame's context carries it and the first frame's does not.
const FOLLOW_ON_TASK: &str = "answer from the switched frame";

fn switch_frame_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Hand the task to another agent frame.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type": "object"}),
    )
}

/// Switches agent frame, handing the follow-on [`FOLLOW_ON_TASK`].
struct SwitchFrameTool;

#[async_trait::async_trait]
impl lash_core::ToolProvider for SwitchFrameTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![switch_frame_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "switch_frame").then(|| Arc::new(switch_frame_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({"switched": true}))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("follow-on-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(FOLLOW_ON_TASK.to_string()),
            })
            .into()
    }
}

/// A core whose first turn switches agent frame and whose inline follow-on
/// fails before its commit, so the session's drive admits the owed follow-on
/// as a recovery root of its own. `follow_on_calls` counts the model calls
/// made in the follow-on's frame.
fn follow_on_core(
    backend: lash_core::Backend,
    follow_on_calls: &Arc<AtomicUsize>,
) -> lash::LashCore {
    let calls = Arc::clone(follow_on_calls);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("host-send-wait-follow-on")
        .complete(move |request: LlmRequest| {
            let in_follow_on = serde_json::to_string(&request.messages)
                .unwrap_or_default()
                .contains(FOLLOW_ON_TASK);
            if in_follow_on {
                calls.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                let part = if in_follow_on {
                    LlmOutputPart::Text {
                        text: "follow-on done".into(),
                        response_meta: None,
                    }
                } else {
                    LlmOutputPart::ToolCall {
                        call_id: "switch-call".into(),
                        tool_name: "switch_frame".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }
                };
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![part],
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    // The inline follow-on runs in the switched frame while the head owes
    // it at recovery count zero; the recovery root raises the count first.
    let catalog = backend.session_store_factory();
    let hook: lash_core::plugin::BeforeTurnHook = Arc::new(move |context| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            let owed =
                match lash_core::runtime::live_session_view(&catalog, &context.session_id).await {
                    Ok(Some(store)) => store.load_pending_follow_on().await.ok().flatten(),
                    _ => None,
                };
            let frame = context.state.to_snapshot().current_frame_node_id;
            if owed.is_some_and(|owed| owed.attempts == 0 && frame.as_ref() == Some(&owed.frame_id))
            {
                return Err(lash_core::PluginError::Invoke(
                    "the inline follow-on fails before its commit".to_owned(),
                ));
            }
            Ok(Vec::new())
        }) as lash_core::plugin::PluginFuture<_>
    });
    lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .models(Arc::new(
            lash::ModelRegistry::new()
                .register(
                    "mock-model",
                    lash::RegisteredModel::new(
                        lash::ModelMetadata::builder("mock-model")
                            .context_window_tokens(200_000)
                            .build()
                            .expect("model metadata"),
                        provider,
                    ),
                )
                .expect("one key registers"),
        ))
        .tools(Arc::new(SwitchFrameTool) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("host-send-wait-follow-on-failure"),
            lash_core::facade_support::PluginSpec::new().with_before_turn(hook),
        )))
        .build(owner())
        .expect("build the lash core")
}

/// FIG-4361 against a live `restate-server`: a follow-on recovery root's run
/// dies after its seal and before its recorded recovery decision
/// (`drive-follow-on`), the session's storage delete commits while it is
/// down, and the server's retry replays the run into the deployment that
/// comes back. The input root before it switched agent frame and its inline
/// follow-on failed before its commit, so the session's drive admitted the
/// owed follow-on as a root of its own. The host still holds the session, so
/// the drive runs on its resident runtime, whose head refresh meets the
/// tombstone. The replay follows the journal (never `RT0016`), its recovery
/// decision records the retirement, and the run ends with the typed
/// `SessionDeleted` refusal. On the suite's replay leg the run also suspends
/// and replays at every await.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait Restate suite runs it"]
async fn live_restate_follow_on_root_killed_after_its_session_was_deleted_ends_typed() {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment names {name}"))
    };
    let key = format!(
        "deleted-under-its-follow-on-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let backend = LiveRestateBackend::start(LiveConfig {
        ingress_url: env("RESTATE_INGRESS_URL"),
        admin_url: env("RESTATE_ADMIN_URL"),
        endpoint_bind: env("HSW_BIND").parse().expect("endpoint bind"),
        endpoint_url: env("HSW_URL"),
        run_tag: key.clone(),
        namespace: lash_restate::RestateNamespace::default(),
    })
    .await
    .expect("start the live backend");
    let follow_on_calls = Arc::new(AtomicUsize::new(0));
    let core = follow_on_core(backend.lash_backend(), &follow_on_calls);
    let session_id = lash::SessionId::from(key.as_str());
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await
        .expect("open the session");
    let root = lash_core::TurnId::from("deleted-replay-root");
    let recovery = lash_core::TurnId::from(format!("follow-on:{root}:agent-frame:1#0"));
    let turn_key = lash_restate::turn_workflow_key(&session_id, &recovery);
    // The recovery root's run dies with its seal journaled and its recovery
    // decision not.
    backend.crash_on(
        CrashRule::new(CrashPoint::BeforeRun {
            name: format!("lash:drive-follow-on:{recovery}"),
        })
        .service(backend.service_name(lash_restate_test::TURN_DRIVER_SERVICE))
        .key(turn_key.clone()),
    );
    // The storage delete commits while the dead run is down: the listener
    // runs as the deployment dies, and the delete is the store's alone.
    let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = backend.lash_backend().session_store_factory();
    assert!(backend.on_crash(CrashCount::new().listener_with({
        let deleted = Arc::clone(&deleted);
        let session_id = session_id.clone();
        move |_target: &str| {
            let factory = Arc::clone(&factory);
            let session_id = session_id.clone();
            let delete = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the delete")
                    .block_on(async {
                        // The dead run's writer can still hold the session for
                        // a moment: a contended delete is retried until a
                        // bounded deadline.
                        let deadline = std::time::Instant::now() + Duration::from_secs(30);
                        loop {
                            match factory.delete_session(&session_id).await {
                                Ok(_) => return,
                                Err(error)
                                    if format!("{error:?}").contains("Contended")
                                        && std::time::Instant::now() < deadline =>
                                {
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                                Err(error) => panic!("the storage delete commits: {error:?}"),
                            }
                        }
                    });
            })
            .join();
            deleted.store(delete.is_ok(), Ordering::SeqCst);
        }
    })));
    let _handle = session
        .send(lash::TurnInput::text(
            "hand this off, then delete me mid-recovery",
        ))
        .id(root.clone())
        .await
        .expect("the input is accepted");
    until("the recovery root dies before its decision", || {
        deleted.load(Ordering::SeqCst)
    })
    .await;
    backend
        .start_serving()
        .await
        .expect("the killed deployment serves again");

    let target = format!(
        "{}/{turn_key}/run",
        backend.service_name(lash_restate_test::TURN_DRIVER_SERVICE)
    );
    let ended = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if let Some(run) = live_host(&backend, &target).await {
                if let Some(failure) = &run.last_failure
                    && diverged(failure)
                {
                    panic!("the root's replay diverged from its journal: {failure}");
                }
                if let Some(outcome) = backend.outcome(&run.id).await.expect("the run's outcome") {
                    let journal = backend.journal(&run.id).await.expect("the run's journal");
                    return (outcome, journal);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let Ok((outcome, journal)) = ended else {
        let run = live_host(&backend, &target).await;
        let journal = match &run {
            Some(run) => backend.journal(&run.id).await.unwrap_or_default(),
            None => Vec::new(),
        };
        panic!("the replayed recovery root never ended: {run:?}\njournal {journal:?}");
    };
    for step in ["drive-root-start:", "drive-seal:", "drive-follow-on:"] {
        assert!(
            journal.iter().any(|entry| entry.contains(step)),
            "the replay issued the recorded steps and the recovery decision after them, \
             missing `{step}`: {journal:?}"
        );
    }
    assert!(
        matches!(&outcome, Err(failure) if failure.contains("session_deleted")),
        "the recovery root's run ends with the typed retirement: {outcome:?}\njournal {journal:?}"
    );
    assert_eq!(
        follow_on_calls.load(Ordering::SeqCst),
        0,
        "the deleted session's follow-on called no model"
    );
    drop(session);
    backend.finish().await;
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
