//! The standard protocol's registrations of the barrier laws (ADR 0116 §7.1)
//! and the `batch` sugar laws (§7.2) on the in-process tier: lash-restate's
//! engine on the Restate server double this crate opens, each law turn
//! driven inside a live handler.
//!
//! The laws, their tools and every assertion live in lash-conformance; this
//! file supplies what that crate cannot construct — the standard protocol,
//! with `batch` offered and withheld — and the tier.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the registration helpers around them in this target are test code too"
)]

use std::sync::Arc;

use lash_core::EffectHost;
use lash_core::facade_support::PluginFactory;
use lash_protocol_standard::{BatchSugar, StandardProtocolConfig, StandardProtocolPluginFactory};

fn offered() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(StandardProtocolPluginFactory::new())]
}

fn withheld() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(StandardProtocolPluginFactory::with_config(
        StandardProtocolConfig::default().batch(BatchSugar::Disabled),
    ))]
}

/// The tier's [`lash_conformance::ConformanceTurnRunner`]: every turn runs
/// inside a handler on the double, on the controller its invocation's journal
/// owns. A crashed turn's invocation stays open, as on a Restate server: the
/// double replays it, and the law's next run of the same scope is that
/// redelivery, replaying the crashed execution's journal.
struct DoubleTurnRunner {
    backend: lash_restate_test::RestateTestBackend,
    crashed: std::sync::Mutex<std::collections::HashMap<String, CrashedTurn>>,
}

/// The open invocation of a turn a crash killed.
struct CrashedTurn {
    /// Where the law's next run of the scope hands the redelivery its attempt.
    redelivery: Arc<Redelivery>,
    handler: tokio::task::JoinHandle<Result<(), String>>,
}

#[derive(Default)]
struct Redelivery {
    attempt: std::sync::Mutex<Option<lash_conformance::ConformanceTurnAttempt>>,
    handed: tokio::sync::Notify,
}

impl Redelivery {
    async fn attempt(&self) -> lash_conformance::ConformanceTurnAttempt {
        loop {
            let handed = self.handed.notified();
            if let Some(attempt) = self
                .attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return attempt;
            }
            handed.await;
        }
    }
}

fn handler_attempt(
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
    async fn scenario_finished(&self) {
        self.backend.server().drop_completed_journals();
    }

    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        let crashed = self
            .crashed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&format!("{:?}", admitted.scope()));
        let ran = match crashed {
            Some(crashed) => {
                *crashed
                    .redelivery
                    .attempt
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attempt);
                crashed.redelivery.handed.notify_waiters();
                crashed
                    .handler
                    .await
                    .expect("the crashed turn's handler task")
            }
            None => {
                self.backend
                    .run_in_handler(admitted, handler_attempt(attempt))
                    .await
            }
        };
        ran.unwrap_or_else(|error| panic!("the law's turn did not run in its handler: {error}"));
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
                handler_attempt(crashing),
                handler_attempt(redrive),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("the law's crashed turn did not redrive in its handler: {error}")
            });
    }

    async fn run_turn_until_crash(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        // Inside the handler the crash kills the attempt where it stands,
        // failing it retryably; the double replays the invocation into a
        // redelivery that waits for the law's next run of the scope and runs
        // its attempt.
        let redelivery = Arc::new(Redelivery::default());
        let crashing: lash_restate_test::HandlerAttempt = {
            let crash = crash.clone();
            Arc::new(move |scoped| {
                let attempt = Arc::clone(&attempt);
                let crash = crash.clone();
                Box::pin(async move {
                    tokio::select! {
                        biased;
                        () = crash.fired() => {
                            panic!("the conformance crash killed the attempt")
                        }
                        end = attempt(scoped) => {
                            panic!("the crashing attempt ended ({end:?}) before its crash fired")
                        }
                    }
                })
            })
        };
        let redrive: lash_restate_test::HandlerAttempt = {
            let redelivery = Arc::clone(&redelivery);
            Arc::new(move |scoped| {
                let redelivery = Arc::clone(&redelivery);
                Box::pin(async move {
                    redelivery.attempt().await(scoped).await;
                })
            })
        };
        let scope = format!("{:?}", admitted.scope());
        let mut handler = {
            let backend = self.backend.clone();
            tokio::spawn(async move {
                backend
                    .run_crashed_then_redriven(admitted, crashing, redrive)
                    .await
            })
        };
        tokio::select! {
            biased;
            () = crash.fired() => {}
            result = &mut handler => {
                panic!("the crashing turn's handler ended ({result:?}) before its crash fired")
            }
        }
        self.crashed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                scope,
                CrashedTurn {
                    redelivery,
                    handler,
                },
            );
    }
}

async fn double() -> (
    lash_restate_test::RestateTestBackend,
    Arc<dyn EffectHost>,
    Arc<dyn lash_core::StoreSet>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
) {
    double_with_config(lash_restate_test::ServerConfig::default()).await
}

async fn double_with_config(
    config: lash_restate_test::ServerConfig,
) -> (
    lash_restate_test::RestateTestBackend,
    Arc<dyn EffectHost>,
    Arc<dyn lash_core::StoreSet>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = 0x5a6a_0000 + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let double = lash_restate_test::backend(seed, config)
        .await
        .expect("start the Restate server double");
    let host = double.lash_backend().effect_host() as Arc<dyn EffectHost>;
    let stores = Arc::clone(double.engine_stores());
    let runner = Arc::new(DoubleTurnRunner {
        backend: double.clone(),
        crashed: std::sync::Mutex::default(),
    }) as Arc<dyn lash_conformance::ConformanceTurnRunner>;
    (double, host, stores, runner)
}

lash_conformance::tool_batch_parallelism_tests!({
    let (double, host, stores, runner) = double().await;
    (
        double,
        "in-process",
        host,
        stores,
        vec![
            lash_conformance::batch_sugar_producer(offered()),
            lash_conformance::batch_wrappers_beside_native_calls_producer(offered()),
            lash_conformance::parallel_model_tool_calls_producer(offered()),
        ],
        runner,
    )
});

lash_conformance::batch_sugar_tests!({
    let (double, host, stores, runner) = double().await;
    (
        double,
        "in-process",
        host,
        stores,
        runner,
        lash_conformance::BatchSugarFactories {
            enabled: offered(),
            disabled: withheld(),
        },
    )
});

/// A width-64 `batch` must not resume its dispatch or opener once per member.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn batch_scales_linearly() {
    let budget = lash_conformance::ToolBatchScalingBudget::from_perf_guard_budgets(include_str!(
        "../../../scripts/perf_guard_budgets.json"
    ));
    let (double, host, stores, runner) =
        double_with_config(lash_restate_test::ServerConfig::default().always_replay(true)).await;
    let producer = lash_conformance::batch_sugar_producer(offered());
    let mut measured = Vec::new();
    for width in [budget.small_width, budget.large_width] {
        measured.push(
            lash_conformance::measure_tool_batch_resumptions(
                "batch-scaling",
                Arc::clone(&host),
                Arc::clone(&stores),
                Arc::clone(&runner),
                &producer,
                width,
                budget.large_width,
                || async {
                    lash_restate_test::tool_batch_resumption_counts(double.server())
                        .await
                        .into()
                },
            )
            .await,
        );
    }
    lash_conformance::assert_tool_batch_resumptions_bounded(
        "in-process/batch",
        measured[0],
        measured[1],
        budget,
    );
}
