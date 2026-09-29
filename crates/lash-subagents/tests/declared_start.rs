//! The in-process and double registrations of the declared-start and
//! `spawn_agent` laws (ADR 0116 §7.3).
//!
//! The laws live in lash-conformance; this file supplies what that crate
//! cannot construct — the subagent plugin and the RLM protocol plugin — and
//! the tiers this crate can open. A spawned child is a process, and processes
//! run in the Restate server double's process workflow, so both tiers run on
//! the double: in process, the double answers every await in-stream; the
//! double tier replays the handler at every await it cannot answer from its
//! journal, the `INACTIVITY_TIMEOUT=0s` mode.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the registration helpers around them in this target are test code too"
)]

use std::sync::Arc;

use lash_core::EffectHost;

/// The tier's [`lash_conformance::ConformanceTurnRunner`]: `run_in_handler`
/// lends the attempt the scoped controller the invocation's journal owns, a
/// crashed attempt fails its invocation retryably and the double replays it
/// into the redrive, and children's segments run in the double's process
/// workflow on the law's worker.
struct DoubleTurnRunner {
    backend: lash_restate_test::RestateTestBackend,
}

/// `attempt` as a `HandlerAttempt`: the law reads the attempt's answer off
/// its own channel.
fn into_handler_attempt(
    attempt: lash_conformance::ConformanceTurnAttempt,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(
        move |controller| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            let attempt = Arc::clone(&attempt);
            Box::pin(async move {
                attempt(controller).await;
            })
        },
    )
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_in_handler(admitted, into_handler_attempt(attempt))
            .await
            .expect("the double's handler runs the law's turn");
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_crashed_then_redriven(
                admitted,
                into_handler_attempt(crashing),
                into_handler_attempt(redrive),
            )
            .await
            .expect("the double crashes and redrives the law's turn");
    }

    /// The double cancels the invocation whose run named `…{step}` proposes
    /// its result, just before storing it.
    fn cancel_at_step_answer(&self, step: &str) -> Option<lash_conformance::ScriptedCancels> {
        let server = self.backend.server();
        server.cancel_on(lash_restate_test::CrashRule::new(
            lash_restate_test::CrashPoint::BeforeRunResultEnding {
                suffix: step.to_owned(),
            },
        ));
        let backend = self.backend.clone();
        Some(Arc::new(move || backend.server().stats().scripted_cancels))
    }

    /// Children run in the double's process workflow: the worker is
    /// installed there, and the runtime's own port only observes the
    /// registry that workflow writes terminals into.
    fn process_work(
        &self,
        watched: lash_core::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> lash_core::ProcessWorkWiring {
        self.backend.install_process_worker(worker);
        let port = Arc::new(lash_core::NoProcessWork::new(&watched));
        lash_core::ProcessWorkWiring::new(watched, port)
    }
}

/// The subagent plugin: one `default` capability, children living until the
/// scope that started them ends.
fn subagents(
    timeout: Option<std::time::Duration>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    let registry = lash_subagents::CapabilityRegistry::new().with(Arc::new(
        lash_subagents::StaticCapability::new(
            "default",
            lash_core::facade_support::SessionSpec::inherit(),
        ),
    ));
    let factory = lash_subagents::SubagentsPluginFactory::new(
        Arc::new(registry),
        lash_core::lifetime::starter,
    );
    Arc::new(match timeout {
        Some(timeout) => factory.with_timeout(timeout),
        None => factory,
    })
}

/// The RLM protocol plugin the `Promise.all` width's cells run under.
fn rlm(backend: &lash_core::Backend) -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            backend,
        )
        .with_process_lifecycle(true),
    )]
}

#[expect(
    clippy::disallowed_methods,
    reason = "test fixture: `LASH_RESTATE_TEST_SEED` replays one printed seed of the server double"
)]
async fn tier(
    prefix: &str,
    always_replay: bool,
) -> (
    lash_restate_test::RestateTestBackend,
    lash_conformance::DeclaredStartTier,
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
    eprintln!("declared-start {prefix}: LASH_RESTATE_TEST_SEED={seed}");
    let double = lash_restate_test::backend(
        seed,
        lash_restate_test::ServerConfig {
            always_replay,
            // A pending deadline is an absolute wall-clock instant the
            // engine's durable wait measures against the system clock, so
            // virtual time starts at wall time.
            start_time_ms: u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or(0),
            )
            .unwrap_or(0),
            ..lash_restate_test::ServerConfig::default()
        },
    )
    .await
    .expect("start the Restate server double");
    let backend = double.lash_backend();
    let tier = lash_conformance::DeclaredStartTier {
        prefix: format!("{prefix}-{seed}"),
        effect_host: backend.effect_host() as Arc<dyn EffectHost>,
        stores: Arc::clone(double.engine_stores()),
        runner: Arc::new(DoubleTurnRunner {
            backend: double.clone(),
        }),
        rlm: rlm(&backend),
        subagents: Arc::new(subagents),
        delivery: Arc::clone(backend.process_work().port()),
    };
    (double, tier)
}

mod in_process {
    lash_conformance::declared_start_tests!({ super::tier("in-process", false).await });
}

mod double {
    lash_conformance::declared_start_tests!({ super::tier("double", true).await });
}
