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
    async fn await_group_quiescence(&self, group_keys: &[String]) {
        loop {
            let open = self.backend.server().invocations().into_iter().any(|view| {
                view.status != "completed"
                    && group_keys.iter().any(|key| {
                        view.target.contains(&format!("/{key}/"))
                            || view.target.contains(&format!("/{key}:"))
                    })
            });
            if !open {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

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

// FIG-4546: the session's recorded `max_tool_calls` is each step's limit on
// the standard protocol, whether the step spells its calls natively or through
// `batch`.
lash_conformance::tool_call_limit_tests!({
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

struct PostReportFailureRunner {
    inner: Arc<dyn lash_conformance::ConformanceTurnRunner>,
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for PostReportFailureRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner.run_turn(admitted, attempt).await;
        panic!("injected runner failure after the turn reported");
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cancellation_oracle_rejects_post_report_runner_failure() {
    let (_double, host, stores, runner) = double().await;
    let law = lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
        "post-report-failure",
        host,
        stores,
        Arc::new(PostReportFailureRunner { inner: runner }),
        lash_conformance::BatchSugarFactories {
            enabled: offered(),
            disabled: withheld(),
        },
    );
    let failed = tokio::spawn(law)
        .await
        .expect_err("the cancellation law must reject a runner that fails after reporting")
        .into_panic();
    let message = failed
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failed.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(
        message.contains("the cancelled runner completes successfully"),
        "the runner completion oracle rejected the mutation: {message}"
    );
}

#[derive(Default)]
struct HeldFinalWrite {
    entered: tokio_util::sync::CancellationToken,
    released: tokio_util::sync::CancellationToken,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for HeldFinalWrite {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if matches!(&envelope.command, lash_core::RuntimeEffectCommand::ToolAttempt { call, .. }
            if call.tool_name == "echo" && (call.args["value"] == "settled" || call.args["value"] == "committed"))
        {
            let entered = self.entered.clone();
            let released = self.released.clone();
            return inner
                .execute_effect(
                    envelope,
                    lash_core::RuntimeEffectLocalExecutor::testing(move |envelope| async move {
                        let output = local.execute(envelope).await;
                        entered.cancel();
                        released.cancelled().await;
                        output
                    }),
                )
                .await;
        }
        inner.execute_effect(envelope, local).await
    }
}

struct BoundaryCheckingRunner {
    inner: Arc<dyn lash_conformance::ConformanceTurnRunner>,
    write: Arc<HeldFinalWrite>,
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for BoundaryCheckingRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner.run_turn(admitted, attempt).await;
    }
    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }
    async fn run_turn_until_crash(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        self.inner
            .run_turn_until_crash(admitted, attempt, crash)
            .await;
        assert!(
            self.write.released.is_cancelled(),
            "the crash crossed a held final write"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn batch_crash_waits_for_the_held_final_write() {
    let (_double, host, stores, runner) = double().await;
    let write = Arc::new(HeldFinalWrite::default());
    let host = Arc::new(lash_core::testing::LayeredEffectHost::new(
        host,
        write.clone(),
    ));
    let runner = Arc::new(BoundaryCheckingRunner {
        inner: runner,
        write: write.clone(),
    });
    let mut law = tokio::spawn(
        lash_conformance::registration_macro_support::batch_redrive_reuses_children(
            "held-final",
            host,
            stores,
            runner,
            lash_conformance::BatchSugarFactories {
                enabled: offered(),
                disabled: withheld(),
            },
        ),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        write.entered.cancelled(),
    )
    .await
    .expect("the selected member reaches its held final write");
    tokio::select! {
        ended = &mut law => panic!("the crash law ended before the final write was released: {ended:?}"),
        () = tokio::time::sleep(std::time::Duration::from_secs(3)) => write.released.cancel(),
    }
    tokio::time::timeout(std::time::Duration::from_secs(180), law)
        .await
        .expect("the released final permits recovery")
        .expect("the law succeeds after the final is durable");
}

#[derive(Default)]
struct IgnoreRecordedFinal {
    selected: std::sync::Mutex<Option<String>>,
    recovering: std::sync::atomic::AtomicBool,
    ignored: std::sync::atomic::AtomicBool,
}

impl IgnoreRecordedFinal {
    fn selects(&self, group_key: &str) -> bool {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            == Some(group_key)
    }

    fn discards(&self, group_key: &str) -> bool {
        self.recovering.load(std::sync::atomic::Ordering::SeqCst) && self.selects(group_key)
    }

    fn ignored_error(&self) -> lash_core::RuntimeEffectControllerError {
        self.ignored
            .store(true, std::sync::atomic::Ordering::SeqCst);
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
            "injected recorded final ignored",
        )
    }
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for IgnoreRecordedFinal {
    async fn open_effect_group(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        if group.children().iter().any(|child| {
            matches!(&child.command,
            lash_core::RuntimeEffectCommand::ToolInvocation { request }
                if request.call.tool_name == "echo" && request.call.args["value"] == "settled")
        }) {
            *self
                .selected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(group.group_key().to_owned());
        }
        inner.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::runtime::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        let mut settled = inner.await_next_settlement(handle, cancel).await?;
        if self.selects(handle.group_key()) && settled.position == 0 {
            let recorded = inner
                .read_group_settlement(handle.group_key(), settled.sequence)
                .await?
                .expect("the selected child already has a durable final");
            assert!(
                recorded.outcome.is_ok(),
                "the final being ignored was recorded successfully"
            );
            if !self.recovering.load(std::sync::atomic::Ordering::SeqCst) {
                // Keep the opener from incorporating the selected final before the crash.
                std::future::pending::<()>().await;
            }
            settled.outcome = Err(self.ignored_error());
        }
        Ok(settled)
    }

    async fn read_group_settlement(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<lash_core::RankedGroupSettlement>, lash_core::RuntimeEffectControllerError>
    {
        let mut recorded = inner.read_group_settlement(group_key, rank).await?;
        if self.discards(group_key)
            && rank == 1
            && let Some(recorded) = &mut recorded
        {
            assert!(
                recorded.outcome.is_ok(),
                "the final being ignored was recorded successfully"
            );
            recorded.outcome = Err(self.ignored_error());
        }
        Ok(recorded)
    }
}

struct IgnoreFinalRunner {
    inner: Arc<dyn lash_conformance::ConformanceTurnRunner>,
    layer: Arc<IgnoreRecordedFinal>,
}

impl IgnoreFinalRunner {
    fn attempt(
        &self,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) -> lash_conformance::ConformanceTurnAttempt {
        let layer = self.layer.clone();
        Arc::new(move |scope| {
            let layer = layer.clone();
            let attempt = attempt.clone();
            Box::pin(async move {
                let scope = lash_core::testing::LayeredEffectHost::layer_scoped(scope, layer)
                    .expect("install the recovery mutation on the turn's controller");
                attempt(scope).await
            })
        })
    }
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for IgnoreFinalRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner.run_turn(admitted, self.attempt(attempt)).await;
    }
    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.inner
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }
    async fn run_turn_until_crash(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        self.inner
            .run_turn_until_crash(admitted, self.attempt(attempt), crash)
            .await;
        self.layer
            .recovering
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn recovery_oracle_rejects_ignoring_a_recorded_final() {
    let (_double, host, stores, runner) = double().await;
    let layer = Arc::new(IgnoreRecordedFinal::default());
    let runner = Arc::new(IgnoreFinalRunner {
        inner: runner,
        layer: layer.clone(),
    });
    let law = lash_conformance::registration_macro_support::batch_redrive_reuses_children(
        "ignored-final",
        host,
        stores,
        runner,
        lash_conformance::BatchSugarFactories {
            enabled: offered(),
            disabled: withheld(),
        },
    );
    assert!(
        tokio::spawn(law).await.is_err(),
        "ignoring a recorded final makes the recovery law fail"
    );
    assert!(
        layer.ignored.load(std::sync::atomic::Ordering::SeqCst),
        "the mutation ignored an actually recorded final on recovery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn batch_cancel_waits_for_the_held_final_write() {
    let (_double, host, stores, runner) = double().await;
    let write = Arc::new(HeldFinalWrite::default());
    let host = Arc::new(lash_core::testing::LayeredEffectHost::new(
        host,
        write.clone(),
    ));
    let mut law = tokio::spawn(
        lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
            "held-cancel-final",
            host,
            stores,
            runner,
            lash_conformance::BatchSugarFactories {
                enabled: offered(),
                disabled: withheld(),
            },
        ),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        write.entered.cancelled(),
    )
    .await
    .expect("the selected member reaches its held final write");
    tokio::select! {
        ended = &mut law => panic!("the cancellation law ended before the final write was released: {ended:?}"),
        () = tokio::time::sleep(std::time::Duration::from_secs(3)) => write.released.cancel(),
    }
    tokio::time::timeout(std::time::Duration::from_secs(180), law)
        .await
        .expect("the released final permits cancellation")
        .expect("the committed member survives cancellation");
}
