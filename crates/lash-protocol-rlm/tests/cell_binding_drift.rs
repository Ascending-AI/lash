//! The SQLite registrations of the FIG-3587 laws: the cell binding-drift law
//! and the model-call drift park law.
//!
//! The law lives in lash-conformance; this file supplies what that crate
//! cannot construct — the RLM protocol plugin factory whose cells the law
//! redrives — over a file-backed SQLite effect host and its journal fault
//! injector.

use std::sync::Arc;

use lash_core::EffectHost;

/// The RLM factory, its Lashlang artifacts in `artifacts`: the law's host is a
/// bare effect host with no backend, so the artifacts get a memory backend of
/// their own.
fn rlm_factory(
    artifacts: &dyn lash_lashlang_runtime::LashlangArtifactBackend,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        artifacts,
        )
        // The law's turn starts no process: there is no process substrate.
        .with_process_lifecycle(false),
    )
}

lash_conformance::cell_binding_drift_tests!({
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let host = Arc::new(
        lash_sqlite_store::SqliteEffectHost::open(&dir.path().join("cell-binding-drift.db"))
            .await
            .unwrap_or_else(|error| panic!("open the SQLite binding-drift effect host: {error}")),
    );
    let faults = host.effect_journal_faults();
    let host = host as Arc<dyn EffectHost>;
    let artifacts = lash_sqlite_store::SqliteBackend::memory()
        .await
        .unwrap_or_else(|error| panic!("open the artifact backend: {error}"));
    // The law's runtime takes its storage from the artifact backend's store set.
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(artifacts.stores().clone());
    (
        dir,
        "sqlite",
        Arc::clone(&host),
        stores,
        lash_conformance::HostTurnRunner::with_journal_faults(host, faults),
        vec![rlm_factory(&artifacts)],
    )
});

lash_conformance::model_call_drift_park_tests!({
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let host = Arc::new(
        lash_sqlite_store::SqliteEffectHost::open(&dir.path().join("model-call-drift.db"))
            .await
            .unwrap_or_else(|error| panic!("open the SQLite model-drift effect host: {error}")),
    ) as Arc<dyn EffectHost>;
    let artifacts = lash_sqlite_store::SqliteBackend::memory()
        .await
        .unwrap_or_else(|error| panic!("open the artifact backend: {error}"));
    // The law's runtime takes its storage from the artifact backend's store set.
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(artifacts.stores().clone());
    (
        dir,
        "sqlite",
        Arc::clone(&host),
        stores,
        lash_conformance::HostTurnRunner::shared(host),
        vec![rlm_factory(&artifacts)],
    )
});
