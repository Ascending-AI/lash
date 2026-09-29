pub mod backend;
pub mod backend_fault;
#[cfg(test)]
mod cache_regression;
mod canonical_scripts;
pub mod chaos_soak;
mod clock;
pub mod content_oracle;
pub mod crash_matrix;
#[cfg(test)]
mod ingress_bound;
#[cfg(test)]
mod obligation_bounds;
#[cfg(test)]
mod oracle_coverage_tests;
#[cfg(test)]
mod recorded_reality;
#[cfg(test)]
mod request_snapshot;
#[cfg(test)]
mod tool_call_replay;

pub mod artifacts;
pub mod backend_contention;
#[cfg(test)]
mod generation_disposition_matrix;
pub mod generator;
pub mod invariants;
pub mod minimize;
pub mod oracles;
mod postgres_test_isolation;
#[cfg(test)]
mod process_terminal_bounds;
pub mod provider;
pub mod provider_mutations;
#[cfg(test)]
mod provider_variation_matrix;
pub mod provider_variations;
pub mod recording;
pub mod replay;
pub mod runner;
pub mod runtime_boundaries;
pub mod runtime_contracts;
pub mod runtime_providers;
pub mod scheduler;
pub mod sqlite_faults;
pub mod stack_policy;
pub mod state_checker;
pub mod store;
pub mod trace;
mod transcript;
mod usage_oracle;

fn sim_process_owner() -> lash_core::LeaseOwnerIdentity {
    static INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    lash_core::LeaseOwnerIdentity::opaque(
        "lash-sim",
        INCARNATION
            .get_or_init(|| {
                format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                )
            })
            .clone(),
    )
}

/// The simulator's one path to a session its world names (FIG-4112).
///
/// A simulated world reaches each session the same way on its first touch
/// and after every crash or retry, so it means create-or-use. Only `create`
/// creates, so the arm where the core's creation config does not apply — the
/// session already exists and keeps what it recorded — is written out here,
/// once, before the open. A deleted id is left for the open to report.
pub(crate) async fn open_created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::Result<lash::LashSession> {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => return Err(error),
    }
    core.session(session_id).open().await
}

pub use artifacts::{
    FixedScriptManifest, FixedScriptProof, FixedScriptSummary, GeneratedSimProfileReport,
    ScriptHashManifest,
};
pub use provider::{
    ProviderWireChunkPayload, ProviderWireEndpoint, ProviderWireEvent, ProviderWireProvenance,
    ProviderWireProvenanceKind, ProviderWireRequestMatch, ProviderWireScript,
    ScriptedLlmHttpTransport, ScriptedTransportSchedule,
};
pub use recording::{ProviderRecordingConfig, RecordingLlmHttpTransport};
pub use runner::{
    FIXED_SCRIPT_PROFILE, run_fixed_script_profile, run_generated_sim_profile,
    run_generated_sim_profile_for_seeds,
};
pub use stack_policy::{PRODUCT_STACK_BUDGET_BYTES, SIM_HARNESS_STACK_LIMIT_BYTES};

/// `LASH_QUICK`: the opt-in iteration knob for the heavy
/// generated-world lanes. When set -- any value but `0` -- every
/// count-based seed sweep in this crate shrinks to a quarter of its seeds,
/// at least one. An explicit `--seeds`/`--seed` or a named `LASH_*_SEEDS`
/// override still wins: the knob sizes defaults, not decisions. CI never
/// sets it; the full sweeps stay the gates.
pub fn quick_enabled() -> bool {
    std::env::var("LASH_QUICK").is_ok_and(|value| !value.is_empty() && value != "0")
}

/// The seed count a generated sweep runs under `quick_enabled`: a quarter of
/// the full count, at least one.
pub fn quick_seed_sweep(full: usize) -> usize {
    if quick_enabled() {
        (full / 4).max(1)
    } else {
        full
    }
}

#[cfg(test)]
mod runtime_feedback;
