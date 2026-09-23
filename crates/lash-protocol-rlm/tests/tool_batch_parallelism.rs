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

use lash_core::{EffectHost, RuntimeEffectController};

/// The RLM protocol plugin, and with it the Lashlang process engine it
/// contributes.
///
/// `process_lifecycle` is the deployment's honest answer to "can a cell start a
/// process here", and it differs between the two producers: the cell-bridge
/// registration runs the law's plain one-turn fixture with no process substrate
/// at all, while the process-bridge registration stands one up. Declaring it
/// wrongly either advertises an ability the engine does not offer or hides one
/// it does.
fn rlm_factory(process_lifecycle: bool) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new()),
        )
        .with_process_lifecycle(process_lifecycle),
    )
}

/// The cell-bridge producer's factories: the RLM protocol and nothing else.
fn cell_bridge_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![rlm_factory(false)]
}

/// The process-bridge producer's factories.
///
/// `processes.*` is rendered from the tool catalogue, so a cell that starts a
/// process needs the plugin that supplies that surface; without it the cell
/// dies on an unknown `processes` module long before any batch is issued.
fn process_bridge_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![
        rlm_factory(true),
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
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

mod native {
    use super::*;

    lash_conformance::tool_batch_parallelism_tests!({
        // The deferred route parks on a completion key, which the native host
        // issues only for an embedding that accepts that such a key dies with
        // the process. A single-process conformance run is that embedding.
        let host: Arc<dyn EffectHost> = Arc::new(
            lash_core::facade_support::NativeEffectHost::new(Arc::new(
                lash_core::facade_support::NativeRuntimeEffectController::default(),
            )
                as Arc<dyn RuntimeEffectController>)
            .allow_process_lifetime_completion_keys(),
        );
        (
            (),
            "native",
            Arc::clone(&host),
            vec![
                lash_conformance::rlm_promise_all_producer(cell_bridge_factories()),
                lash_conformance::lashlang_process_aggregate_producer(
                    process_bridge_factories(),
                    Arc::new(|| {
                        Arc::new(lash_core::TestLocalProcessRegistry::default())
                            as Arc<dyn lash_core::ProcessRegistry>
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
        (
            dir,
            "sqlite",
            Arc::clone(&host),
            vec![
                lash_conformance::rlm_promise_all_producer(cell_bridge_factories()),
                lash_conformance::lashlang_process_aggregate_producer(
                    process_bridge_factories(),
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
