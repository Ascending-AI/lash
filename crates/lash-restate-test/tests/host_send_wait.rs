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
//!   handlers never queue behind a waiting host.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]
#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash::restate::RestateWait;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use restate_sdk::context::{
    ContextReadState, ContextWriteState, ObjectContext, SharedObjectContext, WorkflowContext,
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

fn core(backend: &RestateTestBackend, barrier: &Arc<Barrier>) -> lash::LashCore {
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
    lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
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

struct World {
    backend: RestateTestBackend,
    barrier: Arc<Barrier>,
    session: lash::LashSession,
    _core: lash::LashCore,
}

async fn world(config: ServerConfig) -> World {
    let backend = lash_restate_test::backend(0xd5_3837, config)
        .await
        .expect("build the Restate test backend");
    let barrier = Arc::new(Barrier::default());
    let core = core(&backend, &barrier);
    let session = core
        .session(SESSION)
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
                .build(),
        )
        .await
        .expect("register the host endpoint");
    World {
        backend,
        barrier,
        session,
        _core: core,
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
