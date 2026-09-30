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
struct SettleCrashes {
    server: RestateTestServer,
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
