//! A host turn run inside the host's own Restate handler, on that handler's
//! controller.
//!
//! A host that runs a turn from its own handler hands the facade the
//! handler's controller (`stream_to_with_effects(.., &controller)`). The turn
//! runs through the session drive (FIG-3600), whose admission step is scoped
//! to the drive request rather than to the turn. That step must still run on
//! the handler's controller: the deployment's effect host serves no effect
//! outside a handler, so a step it is asked to scope is refused with
//! `engine_effect_host_requires_handler_scope`, which is what every live
//! Restate host E2E hit after S5a.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::{Arc, Mutex};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::{RestateTestBackend, ServerConfig};

const SESSION: &str = "handler-controller-turn";

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "handler-controller-turn")
}

/// A core over `backend` whose model answers every call with `answer`.
fn core(backend: &RestateTestBackend, answer: &'static str) -> lash::LashCore {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("handler-controller-turn")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: answer.into(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
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

/// A direct turn run on the handler's controller drives its admission, its
/// seal and its root in that handler, and answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_turn_on_the_handlers_controller_runs_its_whole_drive_in_the_handler() {
    let backend = lash_restate_test::backend(0x5a0e, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let core = core(&backend, "answered in the handler");
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    let turn_id = lash::TurnId::from("direct-turn");
    let admitted = lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
        .expect("admit the turn scope");
    let outcome = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let outcome = Arc::clone(&outcome);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let outcome = Arc::clone(&outcome);
            Box::pin(async move {
                let report = session
                    .turn(lash::TurnInput::text("answer in the handler"))
                    .turn_id(turn_id)
                    .stream_to_with_effects(
                        &lash::runtime::NoopTurnActivitySink,
                        scoped.controller(),
                    )
                    .await
                    .map(|report| report.assistant_output.safe_text.clone())
                    .map_err(|error| error.to_string());
                *outcome
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
            })
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        backend.run_in_handler(admitted, attempt),
    )
    .await
    .expect("the handler finishes")
    .expect("the handler runs its turn");
    let outcome = outcome
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the turn reported");
    assert_eq!(
        outcome,
        Ok("answered in the handler".to_string()),
        "the drive's steps run on the handler's controller"
    );
}
