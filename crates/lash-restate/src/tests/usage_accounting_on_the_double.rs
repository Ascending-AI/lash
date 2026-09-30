//! The engine-owned usage accounting laws (FIG-4236, ADR 0125) on the
//! in-process server double: every turn runs in the endpoint's turn probe
//! handler, each spending effect's settlement is a one-way send to the
//! endpoint's `LashUsageAccounting` object, and a crash kills the handler
//! where it stands and Restate's redelivery of the same invocation replays
//! its journal. The `live` module registers the same laws on a live server.

use std::sync::Arc;

use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer};

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// Kills the accounting continuation's next `settle` after its projection
/// committed: the handler dies before its output frame, and the double
/// retries the invocation.
pub(super) struct SettleCrashes {
    pub(super) server: RestateTestServer,
}

impl lash_conformance::UsageContinuationFaults for SettleCrashes {
    fn kill_next_settlement_after_projection(&self) -> Option<Arc<dyn Fn() -> u64 + Send + Sync>> {
        let before = self.server.stats().crashes;
        self.server.crash_on(
            CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            })
            .service("LashUsageAccounting")
            .handler("settle"),
        );
        let server = self.server.clone();
        Some(Arc::new(move || {
            server.stats().crashes.saturating_sub(before)
        }))
    }
}

async fn usage_accounting_tier() -> (
    LiveConformanceHarness,
    lash_conformance::UsageAccountingTier,
) {
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let server = harness
        .server_double()
        .expect("the in-process harness runs on the server double");
    let tier = lash_conformance::UsageAccountingTier {
        prefix: format!("restate-usage-{}", harness.run_nonce()),
        effect_host: harness.endpoint_host(),
        stores: harness.law_stores(),
        runner: harness.turn_runner(),
        continuation: Arc::new(SettleCrashes { server }),
    };
    (harness, tier)
}

lash_conformance::usage_accounting_engine_tests!({ usage_accounting_tier().await });

/// E1's facade read: another core opens no runtime and borrows no controller.
pub(super) async fn read_parked_usage_from_second_core(
    tier: &lash_conformance::UsageAccountingTier,
) {
    lash_conformance::usage_of_a_root_parked_forever_before_finalization_is_read_without_driving(
        tier,
    )
    .await;
    let session = lash_core::SessionId::from(format!("{}-fourth-envelope-drift", tier.prefix));
    let owner = lash_core::RuntimeOwner::Session(session.clone());
    let head = tier
        .stores
        .session_store_factory()
        .load_session_head_meta(&session)
        .await
        .expect("parked head")
        .map(|head| head.head_revision);
    let factory = tier.stores.session_store_factory();
    let fence = factory.drive_epoch(&session).await.expect("parked fence");
    let park = factory
        .load_turn_park(&session)
        .await
        .expect("park before facade read")
        .expect("still parked");
    let stores = Arc::clone(&tier.stores);
    let reader = lash_restate_test::backend_with(
        0x4440,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("second host over the same storage");
    let core = lash::LashCore::standard_builder(reader.lash_backend(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_model(
            lash_core::testing::TestProvider::builder()
                .kind("read-only-usage")
                .complete(|_| async { panic!("the second core reads without driving") })
                .build()
                .into_handle(),
            lash_core::testing::test_model_metadata("mock-model"),
        )
        .build(lash_core::testing::runtime_lease_owner())
        .expect("second core");
    let usage = tokio::time::timeout(std::time::Duration::from_secs(5), core.owner_usage(&owner))
        .await
        .expect("facade read within five seconds")
        .expect("parked owner remains readable");
    assert_eq!(
        usage,
        tier.stores
            .usage_accounting()
            .load_owner_usage(&owner)
            .await
            .expect("kernel usage")
    );
    assert_eq!(usage.completeness, lash_core::UsageCompleteness::default());
    assert_eq!(
        usage
            .rows
            .iter()
            .map(|row| row.reported_attempts)
            .sum::<u64>(),
        3
    );
    assert_eq!(
        tier.stores
            .session_store_factory()
            .load_session_head_meta(&session)
            .await
            .expect("head after facade read")
            .map(|head| head.head_revision),
        head
    );
    assert_eq!(
        factory
            .drive_epoch(&session)
            .await
            .expect("fence after facade read"),
        fence
    );
    assert_eq!(
        factory
            .load_turn_park(&session)
            .await
            .expect("park after facade read"),
        Some(park)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_core_reads_build_drift_parked_usage() {
    let (_guard, tier) = usage_accounting_tier().await;
    read_parked_usage_from_second_core(&tier).await;
}

/// The laws on a live Restate server: the same endpoint, the real
/// server's delivery. A live server takes no injected continuation fault.
mod live {
    use std::sync::Arc;

    use super::super::effect_group_conformance::LiveConformanceHarness;

    lash_conformance::usage_accounting_engine_tests!(
        #[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
        {
            let harness = LiveConformanceHarness::start_for_tool_children().await;
            let tier = lash_conformance::UsageAccountingTier {
                prefix: format!("restate-live-usage-{}", harness.run_nonce()),
                effect_host: harness.endpoint_host(),
                stores: harness.law_stores(),
                runner: harness.turn_runner(),
                continuation: Arc::new(lash_conformance::NoContinuationFaults),
            };
            (harness, tier)
        }
    );
}
