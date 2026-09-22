//! The native tier's registration of the laws that drive a real turn through
//! a [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
//! signal-intent wake and the turn-control laws for tool calls running as
//! effect-group children (FIG-3397).

use std::sync::Arc;

use crate::*;

type NativeTurnRunnerFixture = (
    (),
    &'static str,
    Arc<dyn crate::EffectHost>,
    Arc<dyn crate::ProcessRegistry>,
    Arc<dyn crate::ProcessWorkSubstrate>,
    Arc<dyn crate::ConformanceTurnRunner>,
    fn(&'static str) -> std::future::Ready<()>,
);

fn native_turn_runner_fixture() -> NativeTurnRunnerFixture {
    // A deferred tool call parks on a completion key, and the native host
    // issues one only for an embedding that accepts that such a key dies with
    // the process — which a single-process conformance run is.
    let host: Arc<dyn crate::EffectHost> = Arc::new(
        crate::NativeEffectHost::with_native_controller(Arc::new(
            NativeRuntimeEffectController::default(),
        ))
        .allow_process_lifetime_completion_keys(),
    );
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let process_work: Arc<dyn crate::ProcessWorkSubstrate> = Arc::new(
        crate::NativeProcessWork::for_registry(Arc::clone(&registry)),
    );
    let runner = crate::HostTurnRunner::new(Arc::clone(&host));
    (
        (),
        "native-turn-runner",
        host,
        registry,
        process_work,
        runner,
        // The native host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    )
}

crate::turn_runner_tests!({ native_turn_runner_fixture() });
