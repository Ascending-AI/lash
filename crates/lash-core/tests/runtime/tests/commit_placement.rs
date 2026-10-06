use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_c0aa;

/// A layer whose controllers own commit backpressure over the Restate server double.
struct EngineOwnedCommitLayer;

impl lash_core::testing::EffectLayer for EngineOwnedCommitLayer {
    fn owns_commit_backpressure(&self, _inner: &dyn lash_core::RuntimeEffectController) -> bool {
        true
    }
}

/// The host an engine-owned controller is lent through.
fn engine_commit_host(backend: &lash_core::Backend) -> Arc<dyn lash_core::EffectHost> {
    effect::layered_effect_host(backend, Arc::new(EngineOwnedCommitLayer))
}

/// Passes every operation through untouched.
struct PassThrough;

impl lash_core::testing::EffectLayer for PassThrough {}

#[tokio::test]
async fn commit_admission_ownership_survives_controller_wrappers() {
    let backend = sqlite_recording_backend().await;
    let hosts: [(Arc<dyn lash_core::EffectHost>, bool); 2] = [
        (engine_commit_host(&backend), true),
        (backend.effect_host(), false),
    ];
    for (inner, expected) in hosts {
        let host = lash_core::testing::LayeredEffectHost::new(inner, Arc::new(PassThrough));
        let admitted = lash_core::AdmittedScope::turn("ownership", "turn");
        let scoped = lash_core::EffectHost::scoped_static(&host, admitted.clone())
            .unwrap()
            .expect("both inner hosts lend owned controllers");
        assert_eq!(scoped.controller().owns_commit_backpressure(), expected);
        let (proxy, _requests) =
            lash_core::runtime::effect::EffectTaskController::scoped(scoped.controller(), admitted)
                .unwrap();
        assert_eq!(proxy.controller().owns_commit_backpressure(), expected);
    }
}
