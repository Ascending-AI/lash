use std::path::PathBuf;

use lash_core::runtime::ScenarioContractSpec;
use serde::Serialize;
use serde_json::Value;

use crate::runtime_contracts::RuntimeFinalValueInvariantFacts;
use crate::scheduler::BoundaryKind;
use crate::trace::{OracleStatus, OracleVerdict, TraceEventLine};

#[derive(Clone, Debug, Serialize)]
pub struct FixedScriptManifest {
    pub schema: &'static str,
    pub profile: &'static str,
    pub timeline_at_semantics: &'static str,
    pub script_bundle_hash: String,
    pub events_path: &'static str,
    pub events_sha256: String,
    pub event_count: usize,
    pub scripts: Vec<ScriptHashManifest>,
    pub provider_matrix: Vec<ProviderMatrixRow>,
    pub provider_transport_exclusions: Vec<ProviderTransportExclusion>,
    pub proofs: Vec<FixedScriptProof>,
    pub summary: FixedScriptSummary,
    #[serde(skip)]
    pub manifest_path: PathBuf,
    #[serde(skip)]
    pub summary_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScriptHashManifest {
    pub path: String,
    pub name: String,
    pub provider_kind: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderMatrixRow {
    pub provider_kind: String,
    pub script_names: Vec<String>,
    pub proof_names: Vec<String>,
    pub endpoints: Vec<String>,
    pub success_proofs: usize,
    pub error_proofs: usize,
    pub cancelled_proofs: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderTransportExclusion {
    pub path: &'static str,
    pub status: &'static str,
    pub reason: &'static str,
    pub replacement_lane: &'static str,
    pub review_owner: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct FixedScriptProof {
    pub name: String,
    pub provider_kind: String,
    pub endpoint: String,
    pub outcome: String,
    pub transcript_path: String,
    pub transcript_sha256: String,
    pub observed: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct FixedScriptSummary {
    pub total_scripts: usize,
    pub total_proofs: usize,
    pub total_events: usize,
    pub passed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedSimProfileReport {
    pub schema: &'static str,
    pub profile: String,
    /// `<index>/<total>` seed-index shard this summary covers; `1/1` when unsharded.
    pub shard: String,
    /// Seed count configured before shard filtering; equals the executed seed
    /// count for unsharded and explicit-seed runs.
    pub configured_seeds: usize,
    /// `evidence` for full per-seed artifact runs, `search` for high-volume
    /// runs that only persist failure packages.
    pub mode: &'static str,
    pub generator_version: &'static str,
    pub script_bundle_hash: String,
    pub provider_manifest_path: &'static str,
    pub provider_matrix: Vec<ProviderMatrixRow>,
    pub generated_runtime_provider_matrix: Vec<GeneratedRuntimeProviderMatrixRow>,
    /// `None` in search mode: high-volume runs do not retain the full
    /// delivered-boundary log; failure packages carry their own traces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_path: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_sha256: Option<String>,
    pub replay_reports: Vec<GeneratedReplayArtifact>,
    pub runtime_proof: RuntimeFacadeProof,
    pub scenario_contracts: Vec<ScenarioContractManifest>,
    pub scenario_contract_slices: Vec<ScenarioContractSliceManifest>,
    pub scenario_contract_packages: Vec<ScenarioContractPackageManifest>,
    pub generated_backend_regression_fixtures: Vec<GeneratedBackendRegressionManifest>,
    pub model_only_boundary_reviews: Vec<ModelOnlyBoundaryReview>,
    pub provider_transport_exclusions: Vec<ProviderTransportExclusion>,
    pub counts: GeneratedSimCounts,
    pub oracle_verdicts: Vec<OracleVerdict>,
    pub failure_artifact_shape: &'static str,
    #[serde(skip)]
    pub summary_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedRuntimeProviderMatrixRow {
    pub provider_kind: String,
    pub script_names: Vec<String>,
    pub runtime_session_count: usize,
    pub runtime_provider_turn_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioContractManifest {
    pub suite: &'static str,
    pub oracle_id: &'static str,
    pub contract_count: usize,
    pub required_sim_evidence: Vec<&'static str>,
    pub contracts: Vec<ScenarioContractSpec>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioContractSliceManifest {
    pub schema: &'static str,
    pub suite: &'static str,
    pub test_name: &'static str,
    pub semantic_oracle: &'static str,
    pub oracle_id: String,
    /// Always `per_contract_oracle`: every generated scenario package is backed
    /// by the specific contract verdict it claims.
    pub classification: &'static str,
    pub status: &'static str,
    pub artifact_path: String,
    pub generated_shape: ScenarioGeneratedShape,
    pub selected_event_count: usize,
    pub selected_evidence: Vec<ScenarioEvidenceSelection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioContractPackageManifest {
    pub schema: &'static str,
    pub package_id: String,
    pub suite: &'static str,
    pub test_name: &'static str,
    pub semantic_oracle: &'static str,
    pub transition_kind: String,
    pub oracle_id: String,
    /// Always `per_contract_oracle`: every generated scenario package is backed
    /// by the specific contract verdict it claims.
    pub classification: &'static str,
    pub status: &'static str,
    pub package_path: String,
    pub operational_cases: Vec<String>,
    pub positive: ScenarioPositiveEvidence,
    pub negative: ScenarioNegativeEvidence,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioGeneratedShape {
    pub schema: &'static str,
    pub transition_kind: String,
    pub semantic_oracle: &'static str,
    pub required_evidence: Vec<ScenarioRequiredEvidence>,
    pub transition_facts: Vec<ScenarioTransitionFact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_backend_regression: Option<ScenarioBackendRegressionReference>,
    pub negative_fixture: ScenarioNegativeFixture,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioTransitionFact {
    pub fact: String,
    pub status: &'static str,
    pub assertion: &'static str,
    pub boundary_ids: Vec<String>,
    pub observed: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioBackendRegressionReference {
    pub fixture_id: &'static str,
    pub status: &'static str,
    pub regression_contract: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioRequiredEvidence {
    pub evidence: String,
    pub boundary_kind: String,
    pub assertion: &'static str,
    pub selected_event_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioNegativeFixture {
    pub fixture_id: &'static str,
    pub fixture_path: &'static str,
    pub expected_oracle_id: &'static str,
    pub expected_reason_contains: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioEvidenceSelection {
    pub evidence: String,
    pub trace_alias: String,
    pub boundary_id: String,
    pub boundary_kind: BoundaryKind,
    pub sequence: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioPositiveEvidence {
    pub schema: &'static str,
    pub source_trace_aliases: Vec<String>,
    pub source_trace_paths: Vec<String>,
    pub replay_report_paths: Vec<String>,
    pub sqlite_replay_report_paths: Vec<String>,
    pub selected_boundary_ids: Vec<String>,
    pub selected_event_count: usize,
    pub oracle_status: OracleStatus,
    pub oracle_reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioNegativeEvidence {
    pub schema: &'static str,
    pub fixture_id: &'static str,
    pub fixture_path: &'static str,
    pub expected_oracle_id: &'static str,
    pub expected_reason_contains: &'static str,
    pub minimize_command: String,
    pub minimized_package_path: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ScenarioContractSliceArtifact {
    pub(crate) schema: &'static str,
    pub(crate) contract: ScenarioContractSpec,
    pub(crate) oracle_id: String,
    pub(crate) classification: &'static str,
    pub(crate) semantic_oracle: &'static str,
    pub(crate) generated_shape: ScenarioGeneratedShape,
    pub(crate) selected_evidence: Vec<ScenarioEvidenceSelection>,
    pub(crate) events: Vec<TraceEventLine>,
    pub(crate) verdicts: Vec<OracleVerdict>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ScenarioContractPackageArtifact {
    pub(crate) schema: &'static str,
    pub(crate) package_id: String,
    pub(crate) contract: ScenarioContractSpec,
    pub(crate) oracle_id: String,
    pub(crate) classification: &'static str,
    pub(crate) semantic_oracle: &'static str,
    pub(crate) operational_cases: Vec<String>,
    pub(crate) generated_shape: ScenarioGeneratedShape,
    pub(crate) positive: ScenarioPositiveEvidence,
    pub(crate) negative: ScenarioNegativeEvidence,
    pub(crate) selected_evidence: Vec<ScenarioEvidenceSelection>,
    pub(crate) events: Vec<TraceEventLine>,
    pub(crate) verdicts: Vec<OracleVerdict>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedBackendRegressionManifest {
    pub schema: &'static str,
    pub fixture_id: &'static str,
    pub status: &'static str,
    pub package_path: String,
    pub trace_path: String,
    pub source_trace_path: String,
    pub source_trace_sha256: String,
    pub source_sqlite_replay_report_path: String,
    pub source_sqlite_replay_report_sha256: String,
    pub required_boundary_kinds: Vec<&'static str>,
    pub semantic_oracles: Vec<&'static str>,
    pub replay_backends: Vec<&'static str>,
    pub static_backend_replay_policy: &'static str,
    pub backend_equivalence_contract: &'static str,
    pub regression_contract: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GeneratedBackendRegressionPackage {
    pub(crate) schema: &'static str,
    pub(crate) fixture_id: &'static str,
    pub(crate) status: &'static str,
    pub(crate) trace: &'static str,
    pub(crate) source_trace_path: String,
    pub(crate) source_trace_sha256: String,
    pub(crate) source_sqlite_replay_report_path: String,
    pub(crate) source_sqlite_replay_report_sha256: String,
    pub(crate) required_boundary_kinds: Vec<&'static str>,
    pub(crate) semantic_oracles: Vec<&'static str>,
    pub(crate) replay_backends: Vec<&'static str>,
    pub(crate) static_backend_replay_policy: &'static str,
    pub(crate) backend_equivalence_contract: &'static str,
    pub(crate) regression_contract: &'static str,
    pub(crate) replay_command: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelOnlyBoundaryReview {
    pub boundary_kind: &'static str,
    pub status: &'static str,
    pub production_abstraction_used: &'static str,
    pub model_only_scope: &'static str,
    pub oracle_id: &'static str,
    pub artifact_evidence: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedReplayArtifact {
    pub seed: u64,
    pub trace_path: String,
    pub trace_sha256: String,
    pub transcript_path: String,
    pub transcript_sha256: String,
    pub replay_report_path: String,
    pub replay_report_sha256: String,
    pub minimized_trace_path: String,
    pub minimized_trace_sha256: String,
    pub failure_package_path: String,
    pub minimize_report_path: String,
    pub minimize_report_sha256: String,
    pub sqlite_database_path: String,
    pub sqlite_replay_report_path: String,
    pub sqlite_replay_report_sha256: String,
    pub replay_command: String,
    pub sqlite_replay_command: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedPostgresReplayReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub profile: String,
    pub configured_max_boundaries: usize,
    pub database_url_redacted: String,
    pub cases: Vec<GeneratedPostgresReplayCase>,
    pub counts: GeneratedPostgresReplayCounts,
    #[serde(skip)]
    pub summary_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedPostgresReplayCase {
    pub seed: u64,
    pub trace_alias: String,
    pub status: &'static str,
    pub report_path: String,
    pub report_sha256: String,
    pub reference_digest: String,
    pub actual_digest: String,
    pub verdict: OracleVerdict,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedPostgresReplayCounts {
    pub seeds: usize,
    pub passed: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct GeneratedSimCounts {
    pub generated_seeds: usize,
    pub boundary_events: usize,
    pub scheduler_controlled_boundaries: usize,
    pub runtime_completion_registrations: usize,
    pub scheduler_owned_runtime_completions: usize,
    pub fixed_provider_proofs: usize,
    pub runtime_proofs: usize,
    pub replay_reports: usize,
    pub minimized_replays: usize,
    pub backend_replays: usize,
    pub scenario_contract_oracles: usize,
    pub scenario_contract_mini_oracles: usize,
    pub scenario_contract_slices: usize,
    pub scenario_contract_packages: usize,
    pub generated_backend_regression_fixtures: usize,
    pub oracle_passes: usize,
    pub oracle_failures: usize,
    pub model_store_sessions: usize,
    /// Largest interleaving highwater observed across every generated seed: how
    /// many provider turns the scheduler drove concurrently at the busiest
    /// point of the busiest seed. The broad/full confidence lane fails if this
    /// never reaches the required interleaving depth.
    pub interleaving_depth_max: usize,
    /// Smallest per-seed interleaving highwater, so a single non-interleaving
    /// seed in an otherwise-deep run is still visible in the artifact.
    pub interleaving_depth_min: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeFacadeProof {
    pub schema: &'static str,
    pub name: &'static str,
    pub provider_kind: String,
    pub session_id: String,
    pub turn_index: usize,
    pub assistant_message: String,
    pub provider_exchange_count: usize,
    pub runtime_invariant: OracleVerdict,
    pub provider_output_invariant: OracleVerdict,
    pub pending_tool_completion: PendingToolCompletionProof,
    pub final_value_semantic_channel: FinalValueSemanticProof,
}

#[derive(Clone, Debug, Serialize)]
pub struct PendingToolCompletionProof {
    pub schema: &'static str,
    pub name: &'static str,
    pub session_id: String,
    pub turn_index: usize,
    pub assistant_message: String,
    pub tool_name: String,
    pub completion_boundary_id: String,
    pub scheduler_controlled: bool,
    pub scheduler_sequence: usize,
    pub turn_suspended_before_completion: bool,
    pub completed_event_count_before_resolution: usize,
    pub completed_event_count_after_resolution: usize,
    pub resolved_payload: Value,
    pub completion_outcome: lash_core::ResolveOutcome,
    pub duplicate_completion_outcome: lash_core::ResolveOutcome,
    pub turn_suspension_invariant: OracleVerdict,
    pub scheduler_resolution_invariant: OracleVerdict,
    pub final_result_invariant: OracleVerdict,
}

#[derive(Clone, Debug, Serialize)]
pub struct FinalValueSemanticProof {
    pub schema: &'static str,
    pub name: &'static str,
    pub session_id: String,
    pub turn_index: usize,
    pub final_value: Value,
    pub assistant_output_text: String,
    pub final_value_event_count: usize,
    pub assistant_prose_delta_count: usize,
    pub facts: RuntimeFinalValueInvariantFacts,
    pub semantic_channel_invariant: OracleVerdict,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProviderScriptProfileManifest {
    pub(crate) schema: &'static str,
    pub(crate) fixed_script_manifest: String,
    pub(crate) fixed_script_summary: String,
    pub(crate) script_bundle_hash: String,
    pub(crate) scripts: Vec<ScriptHashManifest>,
    pub(crate) provider_matrix: Vec<ProviderMatrixRow>,
    pub(crate) provider_transport_exclusions: Vec<ProviderTransportExclusion>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FailureArtifactShape {
    pub(crate) schema: &'static str,
    pub(crate) directory: &'static str,
    pub(crate) trace: &'static str,
    pub(crate) replay_report: &'static str,
    pub(crate) oracle: &'static str,
    pub(crate) final_summary: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FixedScriptLaneSummary {
    pub(crate) schema: &'static str,
    pub(crate) profile: &'static str,
    pub(crate) generator_version: &'static str,
    pub(crate) replay_schema_version: &'static str,
    pub(crate) script_bundle_hash: String,
    pub(crate) provider_set: Vec<String>,
    pub(crate) backend_set: Vec<String>,
    pub(crate) counts: FixedScriptLaneCounts,
    pub(crate) fixed_script_manifest: &'static str,
    pub(crate) fixed_script_events: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FixedScriptLaneCounts {
    pub(crate) generated_seeds: usize,
    pub(crate) fixed_script_events: usize,
    pub(crate) fixed_replays: usize,
    pub(crate) minimized_replays: usize,
    pub(crate) backend_replays: usize,
    pub(crate) boundary_events: usize,
    pub(crate) oracle_passes: usize,
    pub(crate) oracle_failures: usize,
    pub(crate) divergences: usize,
}

pub(crate) fn provider_transport_exclusions() -> Vec<ProviderTransportExclusion> {
    vec![
        ProviderTransportExclusion {
            path: "crates/lash-provider-openai/src/codex.rs",
            status: "reviewed_non_dst_exclusion",
            reason: "Codex HTTP/SSE execution rides the injectable LlmHttpTransport and is in the scripted matrix; the provider-native websocket transport (session cache, reservation, and retry over tokio-tungstenite) cannot be driven by scripted HTTP transports and stays outside the LLM DST.",
            replacement_lane: "codex websocket transport lane: provider-layer websocket tests plus the opt-in scripts/codex-websocket-live.sh check",
            review_owner: "lash-sim provider matrix",
        },
        ProviderTransportExclusion {
            path: "crates/lash-provider-openai/src/codex/oauth.rs",
            status: "reviewed_non_dst_exclusion",
            reason: "OAuth device-code polling and token exchange are auth flows, not LLM provider execution; they use reqwest directly and require separate OAuth script fixtures.",
            replacement_lane: "auth-flow conformance lane, not live LLM DST",
            review_owner: "lash-sim provider matrix",
        },
        ProviderTransportExclusion {
            path: "crates/lash-provider-google/src/oauth.rs",
            status: "reviewed_non_dst_exclusion",
            reason: "Google OAuth authorization-code and refresh-token HTTP calls remain outside the LLM transport DST because the generated harness scripts provider execution, not credential issuance.",
            replacement_lane: "auth-flow conformance lane, not live LLM DST",
            review_owner: "lash-sim provider matrix",
        },
        ProviderTransportExclusion {
            path: "crates/lash-core/src/runtime/session_manager/direct.rs",
            status: "reviewed_non_dst_exclusion",
            reason: "Direct completion effects are runtime effect-controller behavior; provider execution behind the direct request is covered through scripted provider transports, while direct effect planning is covered by runtime tests.",
            replacement_lane: "runtime direct-effect scenario contracts plus scripted provider transport proofs",
            review_owner: "lash-sim runtime/provider boundary",
        },
    ]
}

pub(crate) fn model_only_boundary_reviews() -> Vec<ModelOnlyBoundaryReview> {
    vec![
        ModelOnlyBoundaryReview {
            boundary_kind: "durable_effect",
            status: "runtime_effect_controller_backed_with_reviewed_host_history_ceiling",
            production_abstraction_used: "RuntimeEffectEnvelope, RuntimeEffectCommand::ToolAttempt, RuntimeEffectLocalExecutor::testing, SqliteRuntimeEffectController, and PostgresRuntimeEffectController",
            model_only_scope: "workflow-host crash history outside store-backed effect replay remains excluded; generated memory runs and generated SQLite dynamic reruns execute production runtime effect replay controllers, while Postgres conformance/contention lanes cover native Postgres replay storage in lash_runtime_effect_replay",
            oracle_id: "sim.oracle.durable-effect-exactly-once.v1",
            artifact_evidence: "durable-effect observations include runtime_effect.controller=sqlite_runtime_effect_controller or postgres_runtime_effect_controller, local_executor_called false on replay, first completion, replay for the same durable key, Postgres effect_history_replay.status=native_postgres_runtime_effect_controller, and generated SQLite divergence artifacts on mismatch",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "worker",
            status: "runtime_session_and_process_registry_backed_with_reviewed_worker_task_ceiling",
            production_abstraction_used: "SessionExecutionLeaseStore claim/reclaim/renew/release plus ProcessRegistry claim_process_lease and complete_process_with_lease",
            model_only_scope: "DurableProcessWorker task body launch remains excluded; generated memory runs and generated SQLite dynamic reruns execute production ProcessRegistry fencing, while Postgres backend contention covers the session-lease boundary",
            oracle_id: "sim.oracle.worker-stale-completion-rejected.v1",
            artifact_evidence: "worker observed payload records session lease takeover plus process_stale_completion_rejected=true, process_stale_output_absent=true, process_terminal_writer=successor, and process_terminal_event_count=1",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "backend_failure",
            status: "production_store_error_classified_fault_injection_boundary",
            production_abstraction_used: "lash_core::StoreError variants plus generated SQLite divergence artifacts and Postgres backend contention lanes",
            model_only_scope: "connection corruption and live database fault injection remain excluded; generated boundaries classify retryable and terminal backend faults as concrete StoreError variants and fail on generated SQLite dynamic rerun divergence",
            oracle_id: "sim.oracle.backend-failure-observed.v1",
            artifact_evidence: "backend failure events include production_store_error.type=lash_core::StoreError, variant, message, retryable_class, retry attempt counts, and generated SQLite divergence artifacts on mismatch",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "provider_mutation",
            status: "runtime_backed_script_mutation_boundary_with_reviewed_live_call_exclusion",
            production_abstraction_used: "ProviderWireScript, ScriptedLlmHttpTransport, and provider failure envelopes",
            model_only_scope: "live provider calls remain excluded; generated mutation boundaries execute malformed/rate-limit scripts through OpenAI-compatible, direct OpenAI, Anthropic, and Google provider parsers using ScriptedLlmHttpTransport",
            oracle_id: "sim.oracle.provider-mutation-rejected.v1",
            artifact_evidence: "provider mutation events include provider_parser_matrix proofs with real_provider_parser_execution=true and all migrated provider kinds",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "tool",
            status: "runtime_effect_controller_backed_with_reviewed_tool_provider_ceiling",
            production_abstraction_used: "RuntimeEffectEnvelope, RuntimeEffectCommand::ToolAttempt, RuntimeEffectLocalExecutor, ToolAttemptLaunch, ToolCallRecord, and ToolCallOutput",
            model_only_scope: "app-specific ToolProvider implementation bodies remain excluded; generated memory runs and generated SQLite dynamic reruns execute the production runtime effect-controller boundary with scripted no-network tool outcomes",
            oracle_id: "sim.oracle.tool-boundary-observed.v1",
            artifact_evidence: "tool events carry runtime_effect.controller=sqlite_runtime_effect_controller or postgres_runtime_effect_controller, runtime_tool_record, runtime_tool_output, and generated SQLite divergence artifacts on mismatch",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "exec_code",
            status: "runtime_effect_controller_backed_with_reviewed_shell_launch_ceiling",
            production_abstraction_used: "RuntimeEffectEnvelope, RuntimeEffectCommand::ExecCode, RuntimeEffectLocalExecutor, RuntimeEffectOutcome::ExecCode, and ExecResponse",
            model_only_scope: "host shell/kernel process launch remains excluded; generated memory runs and generated SQLite dynamic reruns execute the production runtime effect-controller boundary with scripted no-shell ExecResponse outcomes",
            oracle_id: "sim.oracle.exec-code-observed.v1",
            artifact_evidence: "exec-code events carry runtime_effect.controller=sqlite_runtime_effect_controller or postgres_runtime_effect_controller, runtime_effect_outcome, exit-code data, and generated SQLite divergence artifacts on mismatch",
        },
        ModelOnlyBoundaryReview {
            boundary_kind: "process_wake",
            status: "runtime_persistence_queued_work_backed_with_reviewed_process_body_ceiling",
            production_abstraction_used: "process_wake_delivery, QueuedWorkBatchDraft, QueuedWorkPayload::process_wake, QueuedWorkStore::enqueue_queued_work, and claim_ready_queued_work_by_batch_ids",
            model_only_scope: "the eventual process body that consumes the wake remains excluded; generated memory runs, generated SQLite dynamic reruns, and Postgres backend contention enqueue or claim wake-adjacent queued work through real queued-work/session-lease backend paths",
            oracle_id: "sim.oracle.process-wake-observed.v1",
            artifact_evidence: "process wake events include runtime_process_wake, runtime_queued_work claim evidence, claimed_once=true, and duplicate claimed_once=false dedupe evidence from real queued-work claims",
        },
    ]
}
