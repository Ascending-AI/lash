//! The turn-running conformance laws on the in-process server double
//! (FIG-3600 S5c): direct-turn acceptance (ADR 0069), the cross-tier
//! tool-batch laws (FIG-3400, FIG-3397, ADR 0099) and the session read-view,
//! failure-evidence and fresh-admission laws —
//! each against `lash-restate-test`'s in-process Restate server.
//!
//! `cancelled_turn_withheld_input_tests!` is deliberately absent: the B0-cov
//! batch registers it separately.
//!
//! The `turn_crash_matrix_tests!` catalogue's arms are already registered
//! beside this module in `turn_crash_on_the_double.rs` through the split
//! single-law macros.

use std::sync::Arc;

use lash_sansio::SessionId;

use super::conformance_harness::{HarnessServer, LiveConformanceHarness};

/// A per-fixture seed/discriminator for the standalone `RestateTestBackend`
/// fixtures, kept in step with `conformance_harness::nonce`.
fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos()
}

/// A root session store for `session_id` over `factory`: the handle the law's
/// runtime commits through and the law reads and stamps back.
async fn law_session_store(
    factory: Arc<dyn lash_core::DeploymentStore>,
    session_id: &str,
) -> Arc<dyn lash_core::RuntimeStore> {
    let view = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::testing::store_fixtures::session_store_request(
            &SessionId::fixture(session_id),
            "restate-turn-law-model",
            lash_core::SessionRelation::Root,
        ),
    )
    .await
    .expect("create the conformance session store");
    Arc::clone(view.store())
}

// ADR 0069 direct-turn acceptance on the double: one durable acceptance per
// turn over the deployment host's journaled ingress.
lash_conformance::direct_turn_acceptance_tests!(
    #[ignore = "parked: the laws execute their turns on a runtime over the deployment effect host, which refuses effects outside a handler (RestateEffectHostRequiresHandlerScope); FIG-3600 S5a-q3 or S8"]
    {
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let backend = harness.backend_factory()().await;
        let store = law_session_store(backend.session_store_factory(), "root").await;
        let prefix: &'static str =
            Box::leak(format!("restate-direct-turn-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, backend, store)
    }
);

// FIG-3400 on the double: a width-n tool batch's leaves really overlap. The
// turns run inside the probe handler through the endpoint's turn runner; the
// batch leaves are group children the endpoint's dispatch invocations run.
lash_conformance::tool_batch_parallelism_tests!(
    #[ignore = "parked: every width-8 leaf starts through the endpoint's dispatch but the probe turn never settles inside the law's budget; FIG-3600 S5a-q3 or S8"]
    {
        let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
        let host = harness.endpoint_host();
        let runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-tool-batch-{}", harness.run_nonce()).into_boxed_str());
        (
            harness,
            prefix,
            host,
            stores,
            vec![lash_conformance::parallel_model_tool_calls_producer(vec![
                std::sync::Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
            ])],
            runner,
        )
    }
);

// The session read-view law is storage-shaped: the endpoint's session-store
// factory answers it over the shared catalog.
lash_conformance::session_read_view_tests!({
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
    let factory = harness.session_catalog_factory()();
    (harness, factory)
});

// The mid-stream failure-evidence law runs turns on `backend`'s own host and
// catalog, over a store set on the fixture's virtual clock: the commit clock
// advance orders the two settlements before the reopened read view is
// asserted.
lash_conformance::session_failure_evidence_tests!(
    #[ignore = "parked: the law executes a turn on the deployment effect host, which refuses effects outside a handler (RestateEffectHostRequiresHandlerScope); FIG-3600 S5a-q3 or S8"]
    {
        let backend = lash_restate_test::backend(
            u64::try_from(nonce() & u128::from(u64::MAX)).unwrap_or(0),
            lash_restate_test::ServerConfig::default(),
        )
        .await
        .expect("start the failure-evidence server double");
        let clock = backend.test_clock();
        let law_backend = backend.lash_backend();
        (backend, law_backend, move || clock.advance(1))
    }
);

// Fresh-session admission is storage-shaped: a fresh handle on the endpoint's
// session catalog admits its session as created.
lash_conformance::fresh_session_admission_tests!({
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::in_process()).await;
    let make = harness.law_persistence();
    (
        harness,
        move |session_id: &str| -> Arc<dyn lash_core::RuntimeStore> { make(session_id) },
    )
});
