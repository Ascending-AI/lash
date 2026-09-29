//! The RLM-driven registrations of the barrier laws (FIG-3400, ADR 0116 §7.1)
//! on the in-process tier: the Restate server double this crate opens.
//!
//! `Promise.all` and `Promise.allSettled` over n tool calls are the product
//! surfaces a user reaches for when they want the calls to overlap. They reach
//! a group through two independently written callers: this crate's cell host
//! bridge (`src/executor/host_bridge.rs`) when the aggregate is awaited in the
//! cell, and the process host bridge (`lash-lashlang-runtime/src/process.rs`)
//! when the same aggregate is the body of a started process. All three are
//! registered here.
//!
//! The laws themselves, the leaves, the named-members failure message and
//! every assertion live in lash-conformance; this file supplies only what that
//! crate cannot construct — the RLM protocol plugin factory with a deferred
//! tool resolver that grants the laws' granted leaves, the process-controls
//! plugin that puts `processes.*` in a cell, and the tier this crate can open.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the registration helpers around them in this target are test code too"
)]

use std::sync::Arc;

use lash_core::EffectHost;

/// The RLM protocol plugin, and with it the Lashlang process engine it
/// contributes.
///
/// `process_lifecycle` is the backend's honest answer to "can a cell start a
/// process here", and it differs between the two producers: the cell-bridge
/// registration runs the law's plain one-turn fixture with no process substrate
/// at all, while the process-bridge registration stands one up. Declaring it
/// wrongly either advertises an ability the engine does not offer or hides one
/// it does.
fn rlm_factory(
    backend: &lash_core::Backend,
    process_lifecycle: bool,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            backend,
        )
        .with_process_lifecycle(process_lifecycle)
        .with_deferred_tool_resolver(Arc::new(GrantedLeaves)),
    )
}

/// Grants the barrier laws' granted leaves: call-paths the catalogue does not
/// list, which a cell reaches only through deferred tool resolution, so its
/// group child carries a `ToolExecutionGrant`.
struct GrantedLeaves;

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for GrantedLeaves {
    async fn resolve(
        &self,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, lash_lashlang_runtime::Resolution> {
        paths
            .iter()
            .map(|path| {
                let resolution = match lash_conformance::tool_batch_granted_leaf(path) {
                    Some((definition, source_id)) => {
                        lash_lashlang_runtime::Resolution::Resolved(Box::new(
                            lash_lashlang_runtime::ToolGrant::new(definition)
                                .with_source_id(source_id),
                        ))
                    }
                    None => lash_lashlang_runtime::Resolution::NotAvailable,
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }
}

/// The cell-bridge producers' factories: the RLM protocol and nothing else.
fn cell_bridge_factories(
    backend: &lash_core::Backend,
) -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![rlm_factory(backend, false)]
}

/// The process-bridge producer's factories.
///
/// `processes.*` is rendered from the tool catalogue, so a cell that starts a
/// process needs the plugin that supplies that surface; without it the cell
/// dies on an unknown `processes` module long before any batch is issued.
fn process_bridge_factories(
    backend: &lash_core::Backend,
) -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![
        rlm_factory(backend, true),
        Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ),
    ]
}

/// Both producers on the Restate server double: the scenario's turn runs
/// inside a live `LashTestHandlerHost` handler, where its tool batch's
/// leaves are group children on the invocation's journal, and the process
/// bridge's aggregate runs as a segment of the double's process workflow,
/// served by the worker the law builds.
mod restate_double {
    use super::*;

    /// The tier's [`lash_conformance::ConformanceTurnRunner`]:
    /// `run_in_handler` lends the attempt the scoped controller the
    /// invocation's journal owns — the double's answer to the in-process
    /// `HostTurnRunner`. The suite asks for `run_turn`, `process_work` and
    /// `scenario_finished`;
    /// the crash, cut and segment-recovery routes keep the trait's panicking
    /// defaults.
    struct DoubleTurnRunner {
        backend: lash_restate_test::RestateTestBackend,
    }

    /// `attempt` as a `HandlerAttempt`: the same factory, its
    /// `ConformanceTurnEnd` report dropped — the runner waits the
    /// invocation, the law reads the report off the attempt's own channel.
    fn into_handler_attempt(
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) -> lash_restate_test::HandlerAttempt {
        Arc::new(
            move |controller| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = ()> + Send + '_>,
            > {
                let attempt = Arc::clone(&attempt);
                Box::pin(async move {
                    attempt(controller).await;
                })
            },
        )
    }

    #[async_trait::async_trait]
    impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
        /// The double keeps every journal it records; a finished scenario's
        /// completed ones are dead weight to the next.
        async fn scenario_finished(&self) {
            self.backend.server().drop_completed_journals();
        }

        async fn run_turn(
            &self,
            admitted: lash_core::AdmittedScope,
            attempt: lash_conformance::ConformanceTurnAttempt,
        ) {
            self.backend
                .run_in_handler(admitted, into_handler_attempt(attempt))
                .await
                .expect("the double's handler runs the scenario's turn");
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
                .expect("the double crashes and redrives the scenario's turn");
        }

        /// Process segments run in the double's process workflow: the
        /// worker is installed there, and the runtime's own port only
        /// observes the registry that workflow writes terminals into.
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

    lash_conformance::tool_batch_parallelism_tests!({
        let double =
            lash_restate_test::backend(0x7001_ba7c, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        // The process bridge's aggregate runs in a segment of the double's
        // process workflow, over the engine's own registry: the registry the
        // workflow writes the process's terminal into.
        let engine_stores = Arc::clone(double.engine_stores());
        let mut in_process = lash_conformance::lashlang_process_aggregate_producer(
            process_bridge_factories(&backend),
            Arc::new(move || engine_stores.process_registry()),
        );
        // The handler already lends the turn the controller a Restate host
        // hands it; the task proxy models that shape for in-process tiers.
        in_process.through_task_proxy = false;
        (
            double.clone(),
            "restate-double",
            host,
            Arc::clone(double.engine_stores()),
            vec![
                lash_conformance::rlm_promise_all_producer(cell_bridge_factories(&backend), true),
                lash_conformance::rlm_promise_all_settled_producer(
                    cell_bridge_factories(&backend),
                    true,
                ),
                in_process,
            ],
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
        )
    });

    // FIG-4064 on the double: a `Promise.all` cell's batch with one member
    // settled and one in flight, its turn's handler crashed and the
    // invocation replayed. The cell re-executes on the replay, and its batch
    // must reuse the settled member's recorded completion.
    lash_conformance::tool_batch_crash_redrive_tests!({
        let double =
            lash_restate_test::backend(0x7001_c4a5, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        let producer =
            lash_conformance::rlm_promise_all_producer(cell_bridge_factories(&backend), false);
        (
            double.clone(),
            "restate-double",
            host,
            Arc::clone(double.engine_stores()),
            vec![producer],
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
        )
    });

    /// The perf guard (FIG-4068): a width-64 batch of native parallel calls
    /// on the double costs linear time and peak RSS in its width, held to
    /// `scripts/perf_guard_budgets.json`.
    #[test]
    fn tool_batch_scales_linearly() {
        lash_conformance::assert_tool_batch_scales_linearly(
            "restate-double/parallel-model-tool-calls",
            module_path!(),
            "tool_batch_scaling_child",
            lash_conformance::ToolBatchScalingBudget::from_perf_guard_budgets(include_str!(
                "../../../scripts/perf_guard_budgets.json"
            )),
        );
    }

    /// One width of [`tool_batch_scales_linearly`], on a fresh double in a
    /// process of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "a width child of tool_batch_scales_linearly: only its re-execution runs it"]
    async fn tool_batch_scaling_child() {
        let child = lash_conformance::tool_batch_scaling_child()
            .expect("the parent names the width to measure");
        let double =
            lash_restate_test::backend(0x7001_ba7c, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let host = double.lash_backend().effect_host() as Arc<dyn EffectHost>;
        let stores = Arc::clone(double.engine_stores());
        lash_conformance::run_tool_batch_scaling_child(
            "scaling",
            host,
            stores,
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
            &lash_conformance::parallel_model_tool_calls_producer(
                lash_core::testing::test_standard_protocol_factories(),
            ),
            child.width,
            child.catalog,
        )
        .await;
    }
}
