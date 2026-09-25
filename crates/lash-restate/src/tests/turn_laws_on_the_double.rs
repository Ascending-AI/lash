//! The turn-running conformance laws on the in-process server double
//! (FIG-3600 S5c): direct-turn acceptance (ADR 0069), restored-claim cede
//! (FIG-3552), the cross-tier tool-batch laws (FIG-3400, FIG-3397, ADR 0099)
//! and the session read-view, failure-evidence and fresh-admission laws —
//! each against `lash-restate-test`'s in-process Restate server.
//!
//! `cancelled_turn_withheld_input_tests!` is deliberately absent: the B0-cov
//! batch registers it separately.
//!
//! `drain_end_tests!` is deliberately absent too: its laws drive a drain's
//! effect work on a runtime over the world's effect host, which on this tier
//! is the deployment host — a boundary that refuses effects outside a handler
//! — so most of the suite is red, and the macro is defined inside
//! `conformance/drain_end.rs` (a shared law file this batch does not edit)
//! with no `#[ignore]` form to park it under. S8's QueueDrain work owns the
//! Restate drain-end shape.
//!
//! The `turn_crash_matrix_tests!` catalogue's arms are already registered
//! beside this module in `turn_crash_on_the_double.rs` through the split
//! single-law macros.

use std::sync::Arc;

use lash_sansio::SessionId;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// A per-fixture seed/discriminator for the standalone `RestateTestBackend`
/// fixtures, kept in step with `effect_group_conformance::nonce`.
fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos()
}

/// A root session store for `session_id` over `factory`: the handle the law's
/// runtime commits through and the law reads and stamps back.
async fn law_session_store(
    factory: Arc<dyn lash_core::SessionStoreFactory>,
    session_id: &str,
) -> Arc<dyn lash_core::RuntimePersistence> {
    factory
        .create_store(&lash_core::testing::store_fixtures::session_store_request(
            &SessionId::from(session_id),
            "restate-turn-law-model",
            lash_core::SessionRelation::Root,
        ))
        .await
        .expect("create the conformance session store")
}

// ADR 0069 direct-turn acceptance on the double: one durable acceptance per
// turn over the deployment host's journaled ingress.
lash_conformance::direct_turn_acceptance_tests!(
    #[ignore = "parked: the laws drive stream_turn on a runtime over the deployment effect host, which refuses effects outside a handler (RestateEffectHostRequiresHandlerScope); FIG-3600 S5a-q3 or S8"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let backend = harness.backend_factory()().await;
        let store = law_session_store(backend.session_store_factory(), "root").await;
        let prefix: &'static str =
            Box::leak(format!("restate-direct-turn-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, backend, store)
    }
);

// FIG-3552: a redrive whose journal-restored claim another driver answered
// cedes instead of committing the same words again.
lash_conformance::restored_claim_cede_tests!(
    #[ignore = "parked: the laws drive stream_turn on a runtime over the deployment effect host, which refuses effects outside a handler (RestateEffectHostRequiresHandlerScope); FIG-3600 S5a-q3 or S8"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let backend = harness.backend_factory()().await;
        let store = law_session_store(
            backend.session_store_factory(),
            lash_conformance::RESTORED_CLAIM_CEDE_SESSION_ID,
        )
        .await;
        let prefix: &'static str =
            Box::leak(format!("restate-restored-claim-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, backend, store)
    }
);

// FIG-3400 on the double: a width-n tool batch's leaves really overlap. The
// turns run inside the probe handler through the endpoint's turn runner; the
// batch leaves are group children the endpoint's dispatch invocations run.
lash_conformance::tool_batch_parallelism_tests!(
    #[ignore = "parked: every width-8 leaf starts through the endpoint's dispatch but the probe turn never settles inside the law's budget; FIG-3600 S5a-q3 or S8"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
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
            vec![lash_conformance::parallel_model_tool_calls_producer()],
            runner,
        )
    }
);

// FIG-3397 batch-group laws on the same tool-child fixture the invocation
// laws take: a `call_tool_batch` consumer opens one effect group of
// `ToolInvocation` children and the endpoint's dispatch invocations run them.
lash_conformance::tool_batch_group_tests!(
    #[ignore = "parked: the batch consumer's group children dispatch but their replies and the opener's-end incorporation diverge on the double; FIG-3600 S5a-q3 or S8"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let fixture = harness.tool_child_law_fixture();
        (harness, "restate", fixture)
    }
);

// The session read-view law is storage-shaped: the endpoint's session-store
// factory answers it over the shared catalog.
lash_conformance::session_read_view_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let factory = harness.session_catalog_factory()();
    (harness, factory)
});

// The mid-stream failure-evidence law runs turns on `backend`'s own host and
// catalog, over a store set on the fixture's virtual clock: the commit clock
// advance orders the two settlements before the reopened read view is
// asserted.
lash_conformance::session_failure_evidence_tests!(
    #[ignore = "parked: the law drives a turn on the deployment effect host, which refuses effects outside a handler (RestateEffectHostRequiresHandlerScope); FIG-3600 S5a-q3 or S8"]
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
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let make = harness.law_persistence();
    (
        harness,
        move |session_id: &str| -> Arc<dyn lash_core::RuntimePersistence> { make(session_id) },
    )
});
