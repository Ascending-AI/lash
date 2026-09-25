pub mod backend;
pub mod backend_fault;
#[cfg(test)]
mod cache_regression;
mod canonical_scripts;
mod clock;
pub mod content_oracle;
mod lease;
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
pub mod generator;
pub mod minimize;
pub mod oracles;
mod postgres_test_isolation;
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
pub mod slow_alive;
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

/// `LASH_QUICK` (AGENTS.md): the opt-in iteration knob for the heavy
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
