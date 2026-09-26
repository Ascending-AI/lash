//! Kernel turns on the Restate server double: a lash-core runtime over the
//! double's backend runs a turn inside a handler on the handler's scoped
//! controller.
//!
//! lash-restate-test depends on lash-core, so this dev-dependency is a cycle
//! through lash-core. It holds because an integration test links the one
//! lash-core rlib the double links, and every type the double's API names
//! (`Backend`, `AdmittedScope`, `ScopedEffectController`) lives below
//! lash-core, so even a lash-core unit test shares it.

#![expect(
    clippy::expect_used,
    reason = "test target: the shared turn helper asserts its setup, and clippy exempts only #[test] functions"
)]

use std::sync::Arc;

use lash_core::facade_support::{TurnFinish, TurnOptions, TurnOutcome};
use lash_core::testing::runtime_helpers::{
    EmptyTools, MockCall, mock_provider, runtime_with_plugins_and_tools_and_host,
    test_runtime_host_config,
};
use lash_core::{AdmittedScope, LlmOutputPart, LlmResponse, TurnInput};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn a_kernel_turn_finishes_inside_a_handler_on_the_double() {
    let double = lash_restate_test::backend(0x29, lash_restate_test::ServerConfig::default())
        .await
        .expect("server double");
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;
    let session_id = runtime.session_id().to_string();
    let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
    let admitted = AdmittedScope::turn(session_id.as_str(), "double-turn");
    let outcome: Arc<std::sync::Mutex<Option<Result<TurnOutcome, String>>>> = Arc::default();
    let attempt: lash_restate_test::HandlerAttempt = {
        let runtime = Arc::clone(&runtime);
        let outcome = Arc::clone(&outcome);
        Arc::new(move |scoped| {
            let runtime = Arc::clone(&runtime);
            let outcome = Arc::clone(&outcome);
            Box::pin(async move {
                let turn = runtime
                    .lock()
                    .await
                    .stream_turn(
                        TurnInput::text("hello"),
                        TurnOptions::new(CancellationToken::new(), scoped),
                    )
                    .await
                    .map(|turn| turn.outcome)
                    .map_err(|error| error.to_string());
                *outcome
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn);
            })
        })
    };
    double
        .run_in_handler(admitted, attempt)
        .await
        .expect("the handler ran the turn");
    let outcome = outcome
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the handler recorded the turn's outcome")
        .expect("the turn ran");
    assert!(
        matches!(
            &outcome,
            TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) if text == "Done"
        ),
        "{outcome:?}"
    );
}

/// The kernel door: the test keeps its runtime by `&mut` and runs the turn in
/// its own task on the scoped controller of a handler it opened (D1 F2).
#[tokio::test(flavor = "multi_thread")]
async fn a_kernel_turn_finishes_in_an_open_handler() {
    let double = lash_restate_test::backend(0x2a, lash_restate_test::ServerConfig::default())
        .await
        .expect("server double");
    Box::pin(a_turn_finishes_in_an_open_handler(&double)).await;
}

/// The same door under serial scheduling: the lent handler parks on its
/// release, which the server sees, so the turn's own frames keep the turn
/// and no stall preemption is needed.
#[tokio::test(flavor = "current_thread")]
async fn a_kernel_turn_finishes_in_an_open_handler_under_serial_scheduling() {
    let double = lash_restate_test::backend(
        0x2b,
        lash_restate_test::ServerConfig::default()
            .scheduling(lash_restate_test::Scheduling::Serial),
    )
    .await
    .expect("server double");
    Box::pin(a_turn_finishes_in_an_open_handler(&double)).await;
    assert_eq!(
        double.server().stats().stall_preemptions,
        0,
        "the open handler ran fully sequenced"
    );
}

async fn a_turn_finishes_in_an_open_handler(double: &lash_restate_test::RestateTestBackend) {
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;
    let session_id = runtime.session_id().to_string();
    let handler = double
        .open_handler(AdmittedScope::turn(
            session_id.as_str(),
            "open-handler-turn",
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the turn runs in the open handler");
    handler.close().await.expect("close the turn's handler");
    assert!(
        matches!(
            &turn.outcome,
            TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) if text == "Done"
        ),
        "{:?}",
        turn.outcome
    );
    // The model call was journaled in the lent handler's invocation.
    let lent = double
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerLender/"))
        .expect("the lent handler's invocation");
    assert_eq!(lent.status, "completed", "{lent:?}");
    let journal = double
        .server()
        .journal(&lent.id)
        .expect("the lent handler's journal");
    assert!(
        journal.iter().any(|entry| entry
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with("lash:"))),
        "the turn's effects were journaled in the lent handler: {journal:?}"
    );
}
