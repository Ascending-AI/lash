use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash::scenario_contracts::AGENT_SCENARIO_CONTRACTS;
use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
use lash_core::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent,
    LlmTerminalReason, LlmToolChoice, LlmToolSpec,
};
use lash_core::provider::{
    DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier, ProviderHandle,
    ProviderOptions,
};
use lash_core::runtime::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
use lash_core::{SessionStoreFactory, TriggerStore};
use lash_llm_transport::LlmHttpTransport;
use lash_protocol_rlm::scenario_contracts::RLM_PROTOCOL_SCENARIO_CONTRACTS;
use lash_protocol_standard::scenario_contracts::STANDARD_PROTOCOL_SCENARIO_CONTRACTS;
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::{CodexProvider, OpenAiCompatibleProvider, OpenAiProvider};
use lash_rlm_types::{RlmCreateExtras, RlmProtocolEvent, RlmTermination};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use lash::rlm::RlmTurnBuilderExt as _;
use lash_lashlang_runtime::ToolDefinitionLashlangExt as _;

use crate::artifacts::*;
use crate::canonical_scripts::{
    ANTHROPIC_MESSAGES_TEXT, CANONICAL_SCRIPTS, CODEX_RESPONSES_DISCONNECT,
    CODEX_RESPONSES_RATE_LIMIT, CODEX_RESPONSES_TEXT, CODEX_RESPONSES_TOOL_CALL,
    GOOGLE_GENERATE_TEXT, GOOGLE_STREAM_GENERATE_TEXT, OPENAI_COMPAT_DISCONNECT,
    OPENAI_COMPAT_RATE_LIMIT, OPENAI_COMPAT_RESPONSE_START_TIMEOUT,
    OPENAI_COMPAT_STREAM_CHUNK_TIMEOUT, OPENAI_COMPAT_TOOL_CALL, OPENAI_COMPAT_VALIDATION,
    OPENAI_RESPONSES_TEXT,
};
use crate::clock::SimClock;
use crate::generator::{
    GENERATOR_VERSION, GeneratedWorkload, SimShard, WorkloadProfileError, generate_workload,
    validate_workload_profile,
};
use crate::minimize::{MinimizeError, minimize_trace};
use crate::oracles::{
    LiveProviderFailureFacts, abandoned_requires_evidence, backend_failure_observed,
    cancellation_observed, combine_oracles, cross_session_isolation, durable_effect_exactly_once,
    exec_code_observed, generated_final_value_semantic_channel,
    generated_runtime_provider_matrix as generated_runtime_provider_matrix_oracle,
    generated_suspend_resume, healthy_long_turn_liveness, ingress_sessions_opened,
    lease_time_monotonic, live_provider_failure_coverage, observer_convergence,
    observer_reconnect_observed, operational_coverage, peak_concurrent_live_turns,
    pending_tool_completion, process_never_double_started, process_wake_at_most_once,
    process_wake_observed, provider_mutation_rejected, provider_transport_mutation_classified,
    provider_turn_interleaving_depth, queued_ingress_observed, replay_determinism,
    runtime_final_value_semantic, runtime_graph_acyclic, runtime_provider_turn,
    runtime_session_graph_contract, runtime_single_active_agent_frame, runtime_usage_monotonic,
    scenario_contract_generated_facts, scenario_contract_mini_oracles, scenario_contract_oracles,
    scheduler_controlled_delivery, scheduler_owned_runtime_completions,
    state_machine_semantic_invariants, tool_boundary_observed, trigger_delivery_observed,
    worker_failover_continues_work, worker_stale_completion_rejected,
};
use crate::provider::{
    ProviderWireEvent, ProviderWireHeader, ProviderWireScript, ScriptedLlmHttpExchange,
    ScriptedLlmHttpTransport, ScriptedTransportSchedule,
};
use crate::provider_mutations::{ProviderMutationMatrixCache, is_transport_provider_mutation};
use crate::replay::{ReplayError, replay_trace};
use crate::runtime_boundaries::{RuntimeBoundaryHarness, RuntimeEffectReplayStore};
use crate::runtime_contracts::{
    RuntimeTurnObservation, require_passed, runtime_agent_frame_invariant_facts,
    runtime_final_value_invariant_facts, runtime_graph_invariant_facts, runtime_turn_contract,
    runtime_usage_invariant_facts,
};
use crate::runtime_providers::{
    ANTHROPIC, LIVE_FAILURE_LEAK_PROSE, OPENAI_COMPATIBLE, live_failure_script,
    runtime_provider_components, runtime_script_for_text,
    runtime_scripts_for_texts as runtime_provider_scripts_for_texts, suspend_roundtrip_scripts,
};
use crate::scheduler::{
    BoundaryDeliveryLog, BoundaryEvent, BoundaryKind, BoundaryScheduler, RuntimeCompletionQueue,
    RuntimeCompletionUnit,
};
use crate::sqlite_replay::SqliteReplayError;
use crate::stack_policy::{
    SIM_HARNESS_STACK_LIMIT_BYTES, run_on_product_stack, run_on_sim_harness_stack,
};
use crate::store::{
    CheckpointComponentWriteKind, CheckpointWriteCollector, CheckpointWriteEvent, ModelStore,
    ObservedSessionStoreFactory, backend_fault_observation,
};
use crate::trace::{
    AbstractWorldSummary, OracleStatus, OracleVerdict, SimulationTrace, TraceEventLine,
    TraceIoError, write_event_lines, write_replay_report, write_trace,
};
use crate::usage_oracle::checkpoint_usage_conservation;

pub const FIXED_SCRIPT_PROFILE: &str = "tiny-fixed-provider-scripts";
pub const FIXED_SCRIPT_EVENTS: &str = "events.jsonl";
pub const FIXED_SCRIPT_MANIFEST: &str = "fixed-script-manifest.json";
pub const FIXED_SCRIPT_SUMMARY: &str = "summary.json";
pub const FIXED_SCRIPT_TIMELINE_AT_SEMANTICS: &str =
    "validated-monotonic-metadata-reserved-for-future-scheduler";
pub const GENERATED_SIM_EVENTS: &str = "events.jsonl";
pub const GENERATED_SIM_SUMMARY: &str = "summary.json";
pub const GENERATED_SIM_PROVIDER_MANIFEST: &str = "provider-script-manifest.json";
pub const GENERATED_SIM_FAILURE_SHAPE: &str = "failures/_shape.json";
pub const GENERATED_SIM_SCENARIO_SLICES: &str = "scenario-contract-slices";
pub const GENERATED_SIM_SCENARIO_PACKAGES: &str = "scenario-contract-packages";
pub const GENERATED_SIM_BACKEND_REGRESSION_FIXTURES: &str = "backend-regression-fixtures";

#[derive(Debug)]
pub enum FixedScriptRunnerError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Provider(LlmTransportError),
    Trace(TraceIoError),
    Replay(ReplayError),
    SqliteReplay(SqliteReplayError),
    Minimize(MinimizeError),
    Profile(WorkloadProfileError),
    Runtime(String),
    Assertion(String),
}

impl fmt::Display for FixedScriptRunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "fixed-script artifact I/O failed: {err}"),
            Self::Json(err) => write!(f, "fixed-script artifact JSON failed: {err}"),
            Self::Provider(err) => write!(f, "fixed-script provider proof failed: {err}"),
            Self::Trace(err) => write!(f, "simulation trace artifact failed: {err}"),
            Self::Replay(err) => write!(f, "simulation replay failed: {err}"),
            Self::SqliteReplay(err) => write!(f, "SQLite simulation replay failed: {err}"),
            Self::Minimize(err) => write!(f, "simulation trace minimization failed: {err}"),
            Self::Profile(err) => write!(f, "workload profile rejected: {err}"),
            Self::Runtime(err) => write!(f, "runtime/facade proof failed: {err}"),
            Self::Assertion(message) => write!(f, "fixed-script proof assertion failed: {message}"),
        }
    }
}

impl std::error::Error for FixedScriptRunnerError {}

impl From<std::io::Error> for FixedScriptRunnerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for FixedScriptRunnerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<LlmTransportError> for FixedScriptRunnerError {
    fn from(value: LlmTransportError) -> Self {
        Self::Provider(value)
    }
}

impl From<TraceIoError> for FixedScriptRunnerError {
    fn from(value: TraceIoError) -> Self {
        Self::Trace(value)
    }
}

impl From<ReplayError> for FixedScriptRunnerError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<SqliteReplayError> for FixedScriptRunnerError {
    fn from(value: SqliteReplayError) -> Self {
        Self::SqliteReplay(value)
    }
}

impl From<MinimizeError> for FixedScriptRunnerError {
    fn from(value: MinimizeError) -> Self {
        Self::Minimize(value)
    }
}

impl From<WorkloadProfileError> for FixedScriptRunnerError {
    fn from(value: WorkloadProfileError) -> Self {
        Self::Profile(value)
    }
}

mod agent_contracts;
mod contract_support;
mod fixed_script;
mod generated_driver;
mod generated_profiles;
mod generated_world;
mod harness;
mod provider_proofs;
mod rlm_contracts;
mod runtime_completion;
mod runtime_proofs;
mod scenario_artifacts;
mod scenario_evidence;
mod scenario_facts;
mod standard_contracts;
#[cfg(test)]
mod tests;

pub use agent_contracts::{FIXED_AGENT_PRODUCT_CONTRACTS, run_agent_contract_product_stack_probe};
pub(crate) use contract_support::replay_contract_execution;
pub use fixed_script::run_fixed_script_profile;
pub(crate) use generated_driver::run_generated_workload_for_fixture;
pub use generated_driver::{
    replay_workload_on_postgres, replay_workload_on_sqlite, replay_workload_serialized_reference,
    run_generated_postgres_replay_for_seeds,
};
pub use generated_profiles::{
    SimRunMode, SimRunModeError, SimSeedSource, WEEKLY_REGRESSION_CORPUS,
    run_generated_sim_profile, run_generated_sim_profile_for_seeds,
};

use agent_contracts::*;
use contract_support::*;
use fixed_script::*;
use generated_driver::*;
use generated_profiles::*;
use generated_world::*;
use harness::*;
use provider_proofs::*;
use rlm_contracts::*;
use runtime_completion::*;
use runtime_proofs::*;
use scenario_artifacts::*;
use scenario_evidence::*;
use scenario_facts::*;
use standard_contracts::*;

fn file_sha256(path: &Path) -> Result<String, FixedScriptRunnerError> {
    Ok(sha256_hex(&std::fs::read(path)?))
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
