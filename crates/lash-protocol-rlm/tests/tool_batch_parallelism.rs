//! The RLM bridge's registration of the cross-tier tool-batch parallelism law
//! (FIG-3400).
//!
//! `Promise.all` over n tool calls is the product surface a user reaches for
//! when they want the calls to overlap, and it reaches `call_tool_batch`
//! through this crate's host bridge. The law itself, the leaves and every
//! assertion live in lash-conformance; this file supplies the two things that
//! crate cannot construct — the RLM protocol plugin factory, and the tiers this
//! crate can open.

use std::sync::Arc;

use lash_core::{EffectHost, RuntimeEffectController};

fn rlm_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    vec![Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new()),
    )
    // The law builds a plugin host directly rather than through a core, so
    // nothing else records whether this deployment offers process lifecycle.
    // It does not: the law's fixture is one turn over one effect host, with no
    // process engines, so declaring `false` is the honest answer and the
    // prompt's advertised abilities match what the engine offers.
    .with_process_lifecycle(false),
    )]
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
            host,
            vec![lash_conformance::rlm_promise_all_producer(rlm_factories())],
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
        (
            dir,
            "sqlite",
            host,
            vec![lash_conformance::rlm_promise_all_producer(rlm_factories())],
        )
    });
}
