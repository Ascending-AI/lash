//! The worker-broker laws (FIG-4159) on the Restate server double.
//!
//! Two tiers over `lash-restate-test`'s in-process server: in process, the
//! double answers every await in-stream; the replaying tier replays the
//! handler at every await it cannot answer from its journal. A worker's loss
//! fails the law's attempt retryably, and the double replays the invocation
//! into the re-drive over its journal. The live tier is registered beside the
//! other live laws in `conformance_and_poison.rs`.

use std::sync::Arc;

use super::tool_call_identity_on_the_double::DoubleTurnRunner;

#[expect(
    clippy::disallowed_methods,
    reason = "test fixture: `LASH_RESTATE_TEST_SEED` replays one printed seed of the server double"
)]
async fn tier(
    label: &str,
    always_replay: bool,
) -> (
    lash_restate_test::RestateTestBackend,
    String,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
) {
    let seed = std::env::var("LASH_RESTATE_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos() & u128::from(u64::MAX))
                    .unwrap_or(0),
            )
            .unwrap_or(0)
        });
    eprintln!("vm-broker {label}: LASH_RESTATE_TEST_SEED={seed}");
    let double = lash_restate_test::backend(
        seed,
        lash_restate_test::ServerConfig {
            always_replay,
            ..lash_restate_test::ServerConfig::default()
        },
    )
    .await
    .unwrap_or_else(|error| panic!("start the Restate server double: {error}"));
    let runner: Arc<dyn lash_conformance::ConformanceTurnRunner> = Arc::new(DoubleTurnRunner {
        backend: double.clone(),
    });
    (double, format!("vm-broker-{label}-{seed}"), runner)
}

mod in_process {
    lash_conformance::vm_broker_tests!({ super::tier("in-process", false).await });
}

mod replaying {
    lash_conformance::vm_broker_tests!({ super::tier("replaying", true).await });
}
