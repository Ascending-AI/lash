mod backend_fault;
#[cfg(test)]
mod cache_regression;
mod canonical_scripts;
mod clock;
mod lease;
#[cfg(test)]
mod recorded_reality;
#[cfg(test)]
mod request_snapshot;

pub mod artifacts;
pub mod backend_contention;
pub mod generator;
pub mod minimize;
pub mod oracles;
pub mod postgres_replay;
pub mod provider;
pub mod provider_mutations;
pub mod provider_variations;
pub mod recording;
pub mod replay;
pub mod runner;
pub mod runtime_boundaries;
pub mod runtime_contracts;
pub mod runtime_providers;
pub mod scheduler;
pub mod sqlite_faults;
pub mod sqlite_replay;
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
    FixedScriptManifest, FixedScriptProof, FixedScriptSummary, GeneratedPostgresReplayReport,
    GeneratedSimProfileReport, ScriptHashManifest,
};
pub use provider::{
    ProviderWireEndpoint, ProviderWireEvent, ProviderWireProvenance, ProviderWireProvenanceKind,
    ProviderWireRequestMatch, ProviderWireScript, ScriptedLlmHttpTransport,
    ScriptedTransportSchedule,
};
pub use recording::{ProviderRecordingConfig, RecordingLlmHttpTransport};
pub use runner::{
    FIXED_SCRIPT_PROFILE, run_fixed_script_profile, run_generated_postgres_replay_for_seeds,
    run_generated_sim_profile, run_generated_sim_profile_for_seeds,
};
pub use stack_policy::{PRODUCT_STACK_BUDGET_BYTES, SIM_HARNESS_STACK_LIMIT_BYTES};
