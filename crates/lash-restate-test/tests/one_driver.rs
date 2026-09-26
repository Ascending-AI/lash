//! One driver (FIG-3837, ruling D5): the engine's session drive is the only
//! executor of a turn, and a host's submission executes none of it.
//!
//! A host submits from its own Restate handler through `send()` and waits on
//! its journal (`accept_restate`, `outcome_restate`). The law holds the turn's
//! model call on a barrier and reads the double's journals while it is held:
//! the turn's work (drive admission, the root's steps, the model call) is
//! journaled only on the engine's own invocations, never on the host's, whose
//! journal holds its binding steps alone. Once released, the turn ran exactly
//! once: one model call, one root invocation, one applied input.
//!
//! Before the cutover a host could lend the drive its handler's controller
//! (`stream_to_with_effects(.., &controller)`, #2325's `engine_scoped`): the
//! drive's admission and root then ran on the host's journal while the
//! acceptance scheduled the engine's own drive of the same session, so two
//! drives of one session were in flight at once.

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
use lash_restate_test::{
    RestateTestBackend, SESSION_DRIVER_SERVICE, ServerConfig, TURN_DRIVER_SERVICE,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::HandlerResult;
use tokio::sync::Notify;

const HOST: &str = "OneDriverHost";

/// Every model call counts itself in flight and waits here until released.
#[derive(Default)]
struct Barrier {
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
    release: Notify,
}

fn core(backend: &RestateTestBackend, barrier: &Arc<Barrier>) -> lash::LashCore {
    let barrier = Arc::clone(barrier);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("one-driver")
        .complete(move |_request: LlmRequest| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.calls.fetch_add(1, Ordering::SeqCst);
                let now = barrier.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                barrier.most_in_flight.fetch_max(now, Ordering::SeqCst);
                barrier.release.notified().await;
                barrier.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "answered once".into(),
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
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "one-driver",
        ))
        .expect("build the lash core")
}

#[restate_sdk::workflow]
trait OneDriverHost {
    async fn run() -> HandlerResult<bool>;
}

struct Host {
    session: lash::LashSession,
}

impl OneDriverHost for Host {
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<bool> {
        let outcome = self
            .session
            .send(lash::TurnInput::text("drive me once"))
            .id("one-driver-root")
            .accept_restate(&ctx)
            .await?
            .outcome_restate(
                &ctx,
                RestateWait::new().probe_window(Duration::from_millis(20)),
            )
            .await?;
        Ok(outcome.status == lash::TurnStatus::Answered)
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

/// The named steps each invocation of `service` journaled.
fn journaled_steps(backend: &RestateTestBackend, service: &str) -> Vec<(String, Vec<String>)> {
    let server = backend.server();
    server
        .invocations()
        .into_iter()
        .filter(|invocation| invocation.target.starts_with(service))
        .map(|invocation| {
            let steps = server
                .journal(&invocation.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| entry.name)
                .filter(|name| !name.is_empty())
                .collect();
            (invocation.target, steps)
        })
        .collect()
}

/// A host that submits from its own handler executes nothing: while the turn
/// is held, only the engine's invocations carry the turn's steps, and one
/// model call is in flight; once released, the turn ran exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_submission_leaves_the_engine_the_only_driver() {
    let backend = lash_restate_test::backend(0x0d5_0001, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let barrier = Arc::new(Barrier::default());
    let core = core(&backend, &barrier);
    let session = core
        .session("one-driver")
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
                .build(),
        )
        .await
        .expect("register the host");
    let run = tokio::spawn({
        let ingress = backend.ingress();
        async move {
            ingress
                .call_workflow_empty::<bool>(HOST, "host", "run")
                .await
                .expect("the host answers")
        }
    });
    until("the engine calls the model", || {
        barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    // Give a second driver every chance to start while the first is held.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let host = journaled_steps(&backend, HOST);
    assert!(!host.is_empty(), "the host ran");
    for (target, steps) in &host {
        let foreign = steps
            .iter()
            .filter(|step| !step.starts_with("lash.host."))
            .collect::<Vec<_>>();
        assert!(
            foreign.is_empty(),
            "the host {target} executed the turn's steps {foreign:?}"
        );
    }
    let engine = journaled_steps(&backend, TURN_DRIVER_SERVICE);
    assert_eq!(
        engine.len(),
        1,
        "exactly one root invocation runs the turn: {engine:?}"
    );
    assert!(
        !journaled_steps(&backend, SESSION_DRIVER_SERVICE).is_empty(),
        "the engine's session drive admitted the turn"
    );
    assert_eq!(barrier.most_in_flight.load(Ordering::SeqCst), 1);

    barrier.release.notify_one();
    let answered = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the host finishes")
        .expect("join");
    assert!(answered);
    assert_eq!(barrier.calls.load(Ordering::SeqCst), 1, "the turn ran once");
    assert_eq!(
        session
            .durable()
            .turn_input_applications()
            .await
            .expect("applications")
            .len(),
        1
    );
    assert_eq!(
        journaled_steps(&backend, TURN_DRIVER_SERVICE).len(),
        1,
        "no second root invocation ran the turn again"
    );
}
