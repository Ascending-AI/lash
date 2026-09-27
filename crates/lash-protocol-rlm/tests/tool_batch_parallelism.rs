//! The two RLM-driven registrations of the cross-tier tool-batch parallelism
//! law (FIG-3400).
//!
//! `Promise.all` over n tool calls is the product surface a user reaches for
//! when they want the calls to overlap. It reaches `call_tool_batch` twice over,
//! through two independently written callers: this crate's cell host bridge
//! (`src/executor/host_bridge.rs`) when the aggregate is awaited in the cell,
//! and the process host bridge (`lash-lashlang-runtime/src/process.rs`) when the
//! same aggregate is the body of a started process. Both are registered here.
//!
//! The law itself, the leaves, the named-leaves failure message and every
//! assertion live in lash-conformance; this file supplies only what that crate
//! cannot construct — the RLM protocol plugin factory, the process-controls
//! plugin that puts `processes.*` in a cell, and the tiers this crate can open.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the registration helpers around them in this target are test code too"
)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        .with_process_lifecycle(process_lifecycle),
    )
}

/// The cell-bridge producer's factories: the RLM protocol and nothing else.
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

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

mod sqlite_memory {
    use super::*;

    lash_conformance::tool_batch_parallelism_tests!({
        let backend = lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open the memory tool-batch parallelism backend");
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        let process_factories = process_bridge_factories(&backend.clone().into());
        let law_stores: Arc<dyn lash_core::StoreSet> = Arc::new(backend.stores().clone());
        (
            backend,
            "sqlite-memory",
            Arc::clone(&host),
            law_stores,
            vec![
                // The process bridge's aggregate runs on the native process
                // substrate (`WorkerProcessWork::SelfNative`), which B3
                // removes; the cell bridge's producer runs on the Restate
                // server double in `restate_double` below.
                // Each scenario opens its own session, so it also opens its
                // own registry, on a memory backend of its own.
                lash_conformance::lashlang_process_aggregate_producer(
                    process_factories,
                    Arc::new(|| {
                        sync_await(async {
                            lash_sqlite_store::SqliteBackend::memory()
                                .await
                                .expect("open the tool-batch process registry backend")
                                .process_registry()
                        }) as Arc<dyn lash_core::ProcessRegistry>
                    }),
                ),
            ],
            lash_conformance::HostTurnRunner::shared(host),
        )
    });
}

mod sqlite {
    use super::*;

    lash_conformance::tool_batch_parallelism_tests!({
        let dir = tempfile::tempdir().expect("tempdir");
        let host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(
                &dir.path().join("rlm-tool-batch-parallelism.db"),
            )
            .await
            .expect("open the SQLite tool-batch parallelism effect host"),
        ) as Arc<dyn EffectHost>;
        // Each scenario opens its own session, so it also opens its own
        // registry: a durable registry carried across scenarios would let one
        // scenario's rows decide the next one's admission.
        let registry_root = dir.path().to_path_buf();
        let opened = Arc::new(AtomicUsize::new(0));
        // The law's host is a bare effect host with no backend; the RLM
        // factories keep their artifacts in a memory backend of their own.
        let artifacts = lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open the artifact backend");
        (
            dir,
            "sqlite",
            Arc::clone(&host),
            // The law's runtime takes its storage from the artifact backend's
            // store set.
            Arc::new(artifacts.stores().clone()) as Arc<dyn lash_core::StoreSet>,
            vec![
                // The process bridge's aggregate runs on the native process
                // substrate (`WorkerProcessWork::SelfNative`), which B3
                // removes; the cell bridge's producer runs on the Restate
                // server double in `restate_double` below.
                lash_conformance::lashlang_process_aggregate_producer(
                    process_bridge_factories(&artifacts.clone().into()),
                    Arc::new(move || {
                        let ordinal = opened.fetch_add(1, Ordering::SeqCst);
                        let path = registry_root.join(format!("processes-{ordinal}.db"));
                        let sessions = registry_root.join(format!("sessions-{ordinal}"));
                        Arc::new(sync_await(async move {
                            lash_sqlite_store::SqliteProcessRegistry::open(&path, sessions)
                                .await
                                .expect("open the SQLite tool-batch process registry")
                        })) as Arc<dyn lash_core::ProcessRegistry>
                    }),
                ),
            ],
            lash_conformance::HostTurnRunner::shared(host),
        )
    });
}

/// The cell bridge's producer on the Restate server double: the scenario's
/// turn runs inside a live `LashTestHandlerHost` handler, where its tool
/// batch's leaves are group children on the invocation's journal. The
/// process bridge's producer stays on the SQLite mounts: the law drives its
/// aggregate on the native process substrate (`WorkerProcessWork::SelfNative`),
/// a subject B3 removes rather than the double serves.
mod restate_double {
    use super::*;

    /// The tier's [`lash_conformance::ConformanceTurnRunner`]:
    /// `run_in_handler` lends the attempt the scoped controller the
    /// invocation's journal owns — the double's answer to the in-process
    /// `HostTurnRunner`. The suite asks for `run_turn` alone; the crash,
    /// cut and process-segment routes keep the trait's panicking defaults.
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
    }

    lash_conformance::tool_batch_parallelism_tests!({
        let double =
            lash_restate_test::backend(0x7001_ba7c, lash_restate_test::ServerConfig::default())
                .await
                .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        let mut producer =
            lash_conformance::rlm_promise_all_producer(cell_bridge_factories(&backend));
        // The relay entry's nested batch rides the relay child's invocation
        // journal, which Restate replays serially (FIG-3671): the direct
        // entry — one parallel model response dispatched as one effect
        // group — is the coverage this tier can carry.
        producer.reaches_relay = false;
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
}
