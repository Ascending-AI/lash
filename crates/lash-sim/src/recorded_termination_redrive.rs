//! FIG-4389's recorded-termination redrive law on the simulator's effect
//! host: the run executes in a handler of [`SimEngine`]'s server double, over
//! its SQLite memory store set, and the crash is a failed handler attempt the
//! double redelivers.

use std::sync::Arc;

use crate::backend::SimEngine;

/// Runs a law's turns in handlers of the simulator's engine.
struct SimTurnRunner(lash_restate_test::RestateTestBackend);

/// `attempt` as a `HandlerAttempt`: the law reads the attempt's answer off
/// its own channel.
fn into_handler_attempt(
    attempt: lash_conformance::ConformanceTurnAttempt,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(move |scoped| {
        let attempt = Arc::clone(&attempt);
        Box::pin(async move {
            attempt(scoped).await;
        })
    })
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for SimTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.0
            .run_in_handler(admitted, into_handler_attempt(attempt))
            .await
            .unwrap_or_else(|error| panic!("the simulator's handler runs the law's turn: {error}"));
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.0
            .run_crashed_then_redriven(
                admitted,
                into_handler_attempt(crashing),
                into_handler_attempt(redrive),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("the simulator crashes and redrives the law's turn: {error}")
            });
    }
}

lash_conformance::turn_config_tests!(@law [] {
    let engine = SimEngine::new(0x4389_0001)
        .await
        .expect("boot the simulator's engine");
    let double = engine.restate().clone();
    let effect_host: Arc<dyn lash_core::EffectHost> = double.restate().restate_effect_host();
    let stores = Arc::clone(double.engine_stores());
    let runner = Arc::new(SimTurnRunner(double)) as Arc<dyn lash_conformance::ConformanceTurnRunner>;
    (engine, "sim-recorded-termination", effect_host, stores, runner)
}; (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));
