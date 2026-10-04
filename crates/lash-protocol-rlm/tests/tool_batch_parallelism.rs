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
            std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
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

/// The producers the `max_tool_calls` laws run over (FIG-4546): a cell's
/// `Promise.all` and `Promise.allSettled`, whose limit is the cell's total,
/// and the process bridge's aggregate, whose limit is what the process holds
/// at once.
fn limit_producers(
    backend: &lash_core::Backend,
    engine_stores: Arc<dyn lash_core::StoreSet>,
) -> Vec<lash_conformance::ToolBatchProducer> {
    let mut in_process = lash_conformance::lashlang_process_aggregate_producer(
        process_bridge_factories(backend),
        Arc::new(move || engine_stores.process_registry()),
    );
    // The handler already lends the turn the controller a Restate host hands
    // it; the task proxy models that shape for in-process tiers.
    in_process.through_task_proxy = false;
    vec![
        lash_conformance::rlm_promise_all_producer(cell_bridge_factories(backend), false),
        lash_conformance::rlm_promise_all_settled_producer(cell_bridge_factories(backend), false),
        in_process,
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
    pub(super) struct DoubleTurnRunner<
        Stores: lash_core::StoreSet + ?Sized = lash_sqlite_store::SqliteStoreSet,
    > {
        pub(super) backend: lash_restate_test::RestateTestBackend<Stores>,
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
    impl<Stores: lash_core::StoreSet + ?Sized> lash_conformance::ConformanceTurnRunner
        for DoubleTurnRunner<Stores>
    {
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

        /// The double runs a segment as a `run` invocation of its process
        /// workflow: crashing the attempt drops it where it stands, and the
        /// server replays the invocation. The workflow's other handlers only
        /// wait on the segment's promises; they run nothing of the process.
        async fn kill_process_workers(&self) -> usize {
            let server = self.backend.server();
            let workflow = self.backend.service_name("LashProcessWorkflow");
            server
                .invocations()
                .into_iter()
                .filter(|invocation| {
                    invocation.status == "running"
                        && invocation.target.starts_with(&workflow)
                        && invocation.target.ends_with("/run")
                })
                .filter(|invocation| server.crash(&invocation.id))
                .count()
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

    // FIG-4546 on the double over SQLite: the session's recorded
    // `max_tool_calls` is a cell's total and what a process holds at once.
    lash_conformance::tool_call_limit_tests!({
        let double =
            lash_restate_test::backend(0x7001_4546, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        (
            double.clone(),
            "restate-double",
            host,
            Arc::clone(double.engine_stores()),
            limit_producers(&backend, Arc::clone(double.engine_stores())),
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
        )
    });

    lash_conformance::tool_call_limit_process_tests!({
        let double =
            lash_restate_test::backend(0x7001_4547, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        (
            double.clone(),
            "restate-double",
            host,
            Arc::clone(double.engine_stores()),
            limit_producers(&backend, Arc::clone(double.engine_stores())),
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
        )
    });


}

/// The `max_tool_calls` laws on the Restate server double over PostgreSQL
/// (FIG-4546).
mod restate_double_postgres {
    use super::*;

    #[expect(
        clippy::disallowed_methods,
        reason = "service fixture reads its PostgreSQL connection and mints fresh session ids"
    )]
    async fn fixture() -> (
        (
            tempfile::TempDir,
            lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
        ),
        &'static str,
        Arc<dyn EffectHost>,
        Arc<dyn lash_core::StoreSet>,
        Vec<lash_conformance::ToolBatchProducer>,
        Arc<dyn lash_conformance::ConformanceTurnRunner>,
    ) {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
            .expect("the PostgreSQL RLM laws require a provisioned PostgreSQL service");
        let attachments = tempfile::tempdir().expect("attachment byte store");
        let bytes = Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        ));
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let double = lash_restate_test::backend_with_store_set(
            (nonce & u128::from(u64::MAX)) as u64,
            lash_restate_test::ServerConfig::default(),
            lash_restate_test::DeploymentHooks::default(),
            move |clock| async move {
                let storage = lash_postgres_store::PostgresStorage::connect(&url)
                    .await
                    .map_err(|error| lash_restate_test::BackendError::Stores(error.to_string()))?;
                Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                    &storage,
                    bytes,
                    lash_core::WakeDeliveryConfig::default(),
                    clock,
                )) as Arc<dyn lash_core::StoreSet>)
            },
        )
        .await
        .expect("start the Restate double over PostgreSQL");
        let backend = double.lash_backend();
        let prefix: &'static str =
            Box::leak(format!("rlm-tool-call-limit-pg-{nonce}").into_boxed_str());
        eprintln!("RLM max_tool_calls tier: PostgreSQL, session prefix {prefix}");
        (
            (attachments, double.clone()),
            prefix,
            backend.effect_host() as Arc<dyn EffectHost>,
            Arc::clone(double.engine_stores()),
            limit_producers(&backend, Arc::clone(double.engine_stores())),
            Arc::new(restate_double::DoubleTurnRunner { backend: double }),
        )
    }

    lash_conformance::tool_call_limit_tests!(
        #[ignore = "requires PostgreSQL; run scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-rlm-tool-call-limit"]
        {
            fixture().await
        }
    );
    lash_conformance::tool_call_limit_process_tests!(
        #[ignore = "requires PostgreSQL; run scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-rlm-tool-call-limit"]
        {
            fixture().await
        }
    );
}
