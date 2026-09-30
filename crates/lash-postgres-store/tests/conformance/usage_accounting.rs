//! The engine-owned usage accounting laws (FIG-4236, ADR 0125) on
//! PostgreSQL: a root's turns run inside the Restate double's handlers over
//! this test's PostgreSQL stores, each spending effect's settlement is a
//! one-way send to the deployment's `LashUsageAccounting` object, and a
//! crash kills the handler execution where it stands and the double's
//! redelivery replays its journal.

use std::sync::Arc;

use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer};

use super::{double_law_backend, reset, storage};

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

lash_conformance::usage_accounting_engine_tests!({
    let Some((lock, storage)) = storage().await else {
        panic!("Postgres usage-accounting laws require LASH_POSTGRES_DATABASE_URL");
    };
    reset(storage.pool()).await;
    let ((attachments, double), stores, effect_host, runner) = double_law_backend(&storage).await;
    let server = double.server().clone();
    (
        (lock, storage, attachments, double),
        lash_conformance::UsageAccountingTier {
            prefix: "postgres-usage".to_string(),
            effect_host,
            stores,
            runner,
            continuation: Arc::new(SettleCrashes { server }),
        },
    )
});
