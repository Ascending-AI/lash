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
//!   `lash::tests::deleted_session_root_replay`).

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
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, HandlerAttempt, RestateTestBackend, ServerConfig};
use restate_sdk::context::{
    ContextPromises, ContextReadState, ContextWriteState, ObjectContext, SharedObjectContext,
    SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::HandlerResult;
use restate_sdk::serde::Json;
use tokio::sync::Notify;

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
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .build(owner())
        .expect("build the lash core")
}

/// What a host run answers: the outcome's status and its reply.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Answer {
    answered: bool,
    reply: Option<String>,
    input_id: String,
}

fn answer(input_id: &lash::InputId, outcome: &lash::SendOutcome) -> Answer {
    Answer {
        answered: outcome.status == lash::TurnStatus::Answered,
        reply: outcome
            .output
            .as_ref()
            .and_then(|output| output.assistant_message().map(str::to_owned)),
        input_id: input_id.to_string(),
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

struct Chat {
    session: lash::LashSession,
}

impl ChatObject for Chat {
    async fn submit(&self, ctx: ObjectContext<'_>, text: String) -> HandlerResult<String> {
        let handle = self
            .session
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
            .session
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
            .create_or_use_restate(&ctx, lash::SessionCreation::default())
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
    backend: RestateTestBackend,
    barrier: Arc<Barrier>,
    session: lash::LashSession,
    core: lash::LashCore,
}

async fn world(config: ServerConfig) -> World {
    let backend = lash_restate_test::backend(0xd5_3837, config)
        .await
        .expect("build the Restate test backend");
    let barrier = Arc::new(Barrier::default());
    let core = core(backend.lash_backend(), &barrier);
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
                        session: session.clone(),
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
    World {
        backend,
        barrier,
        session,
        core,
    }
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
fn journaled_runs(backend: &RestateTestBackend, service: &str) -> Vec<String> {
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
fn assert_host_never_drove(backend: &RestateTestBackend, service: &str) {
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
) -> (HandlerAttempt, Arc<Mutex<Option<String>>>) {
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
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("{deletion:?}"));
        })
    });
    (attempt, answered)
}

/// Delete `session_id` in handlers `run` runs, until the store records the
/// deletion: the session gone, or closed with its physical delete owed to
/// the recovery relay. Either way no attempt may use it again. A turn that
/// just answered can still pin the session for its cancellation closure, and
/// the store refuses the delete until the engine consumes that pin.
async fn delete_session<F, Fut>(core: &lash::LashCore, session_id: &str, run: F)
where
    F: Fn(HandlerAttempt) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut answered = String::new();
    for _ in 0..200 {
        let (attempt, slot) = deletion(core, session_id).await;
        run(attempt).await.expect("the delete handler runs");
        answered = slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_default();
        if ["Ok(Deleted(", "Ok(AlreadyDeleted", "Ok(Closing("]
            .iter()
            .any(|deleted| answered.starts_with(deleted))
        {
            return;
        }
        assert!(
            answered.contains("TurnCancelClosureLifecyclePinned"),
            "`{session_id}` could not be deleted: {answered}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{session_id}` stayed pinned: {answered}");
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

    delete_session(&world.core, key, |attempt| {
        world.backend.run_in_handler(
            lash_core::AdmittedScope::session_delete(lash::SessionId::from(key)),
            attempt,
        )
    })
    .await;
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
    assert!(backend.on_crash(Arc::new({
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
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
