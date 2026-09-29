use super::distribution::{Distribution, bytes_schema, history_schema};
use super::schema::{confidence_schema, positive_rate_schema, saturation_schema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkloadSpec {
    #[schemars(range(min = 1, max = 1))]
    pub format_version: u32,
    pub seed: u64,
    pub generator: GeneratorVersion,
    #[schemars(range(min = 1))]
    pub sessions: u32,
    #[schemars(schema_with = "positive_rate_schema")]
    pub turns_per_session_s: f64,
    #[schemars(regex(pattern = "^poisson_open_loop$"))]
    pub arrival: String,
    #[schemars(range(min = 1))]
    pub rotate_after_turns: u32,
    #[schemars(schema_with = "history_schema")]
    pub history_prefill_turns: Distribution,
    #[schemars(range(min = 1))]
    pub compact_every_turns: u32,
    #[schemars(range(min = 1))]
    pub warmup_s: u32,
    #[schemars(range(min = 1))]
    pub steady_s: u32,
    #[schemars(range(min = 1))]
    pub fault_window_s: u32,
    #[schemars(schema_with = "saturation_schema")]
    pub saturation_rates_per_session_s: Vec<f64>,
    #[schemars(range(min = 1))]
    pub saturation_step_s: u32,
    #[schemars(range(min = 1))]
    pub drain_timeout_s: u32,
    #[schemars(range(min = 1))]
    pub minimum_completed_turns: u32,
    #[schemars(range(min = 0))]
    pub llm_requests_per_turn: f64,
    #[schemars(range(min = 0))]
    pub host_process_starts_per_turn: f64,
    #[schemars(range(min = 0))]
    pub external_occurrences_per_turn: f64,
    #[schemars(range(min = 0))]
    pub trigger_edits_per_turn: f64,
    #[schemars(range(min = 0))]
    pub promotion_reads_per_turn: f64,
    #[schemars(schema_with = "bytes_schema")]
    pub prompt_bytes: Distribution,
    #[schemars(schema_with = "bytes_schema")]
    pub input_bytes: Distribution,
    #[schemars(schema_with = "bytes_schema")]
    pub tool_argument_bytes: Distribution,
    #[schemars(schema_with = "bytes_schema")]
    pub tool_result_bytes: Distribution,
    #[schemars(schema_with = "bytes_schema")]
    pub provider_output_bytes: Distribution,
    pub tools: Tools,
    pub processes: Processes,
    pub attachments: Attachments,
    pub queued: Queued,
    pub cron: Cron,
    #[schemars(range(min = 0, max = 1))]
    pub turn_cancel_share: f64,
    #[schemars(range(min = 0))]
    pub delete_per_turn: f64,
    pub observation: Observation,
    pub provider: Provider,
    pub faults: Faults,
    pub topology: Topology,
    pub model_code_pool: ModelCodePool,
    pub collection: Collection,
    pub provenance: Provenance,
    pub inventory: Inventory,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratorVersion {
    #[schemars(regex(pattern = "^ChaCha20$"))]
    pub algorithm: String,
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    #[schemars(range(min = 0, max = 1))]
    pub turn_share: f64,
    #[schemars(range(min = 1))]
    pub batches: u32,
    pub fanout: Distribution,
    #[schemars(range(min = 0, max = 1))]
    pub parallel_share: f64,
    pub callback_ms: Distribution,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Processes {
    #[schemars(range(min = 0, max = 1))]
    pub turn_share: f64,
    pub fanout: Distribution,
    #[schemars(range(min = 0, max = 1))]
    pub await_share: f64,
    #[schemars(range(min = 0, max = 1))]
    pub signal_share: f64,
    #[schemars(range(min = 0, max = 1))]
    pub park_share: f64,
    #[schemars(range(min = 1))]
    pub wake_delay_ms: u32,
    #[schemars(range(min = 0, max = 1))]
    pub cancel_share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attachments {
    #[schemars(range(min = 0, max = 1))]
    pub turn_share: f64,
    pub count: Distribution,
    #[schemars(schema_with = "bytes_schema")]
    pub aggregate_bytes: Distribution,
    #[schemars(range(min = 1))]
    pub explicit_reads_per_blob: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Queued {
    #[schemars(range(min = 0))]
    pub inputs_per_turn: f64,
    #[schemars(range(min = 0, max = 1))]
    pub active_turn_share: f64,
    #[schemars(range(min = 0, max = 1))]
    pub cancel_share: f64,
    #[schemars(range(min = 1))]
    pub maximum_drain_batch: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cron {
    #[schemars(range(min = 1))]
    pub subscriptions: u32,
    #[schemars(range(min = 1))]
    pub cadence_s: u32,
    #[schemars(regex(pattern = "^seeded_phase$"))]
    pub jitter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    #[schemars(range(min = 1))]
    pub streams_per_turn: u32,
    #[schemars(range(min = 0, max = 1))]
    pub reconnect_share: f64,
    #[schemars(range(min = 1))]
    pub poll_ms: u32,
    #[schemars(range(min = 1))]
    pub pending_reads_per_turn: u32,
    #[schemars(range(min = 1))]
    pub workflow_read_sets_per_start: u32,
    #[schemars(range(min = 1))]
    pub projector_poll_s: u32,
    #[schemars(range(min = 1))]
    pub projector_idle_s: u32,
    #[schemars(range(min = 1))]
    pub subscription_reconcile_s: u32,
    #[schemars(range(min = 1))]
    pub process_prune_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub latency_ms: Distribution,
    #[schemars(range(min = 0, max = 1))]
    pub stream_share: f64,
    #[schemars(range(min = 1))]
    pub chunks: u32,
    #[schemars(range(min = 0, max = 1))]
    pub retryable_first_attempt_share: f64,
    #[schemars(range(min = 1))]
    pub max_concurrent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Faults {
    #[schemars(range(min = 1))]
    pub worker_kill_s: u32,
    #[schemars(range(min = 1))]
    pub worker_restart_delay_s: u32,
    #[schemars(range(min = 1))]
    pub restate_restart_s: u32,
    #[schemars(range(min = 1))]
    pub restate_restart_delay_s: u32,
    #[schemars(range(min = 1))]
    pub rolling_deploy_s: u32,
    #[schemars(range(min = 1))]
    pub rolling_worker_pause_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Topology {
    #[schemars(range(min = 1))]
    pub workers: u32,
    #[schemars(range(min = 1))]
    pub worker_cpu: u32,
    #[schemars(range(min = 1))]
    pub worker_memory_mib: u32,
    #[schemars(range(min = 1))]
    pub pg_connections_per_worker: u32,
    #[schemars(range(min = 1))]
    pub restate_nodes: u32,
    #[schemars(range(min = 1))]
    pub restate_partitions: u32,
    #[schemars(range(min = 1))]
    pub replication: u32,
    #[schemars(range(min = 1))]
    pub restate_cpu: u32,
    #[schemars(range(min = 1))]
    pub restate_memory_mib: u32,
    #[schemars(range(min = 1))]
    pub pg_cpu: u32,
    #[schemars(range(min = 1))]
    pub pg_memory_mib: u32,
    #[schemars(range(min = 1))]
    pub garage_cpu: u32,
    #[schemars(range(min = 1))]
    pub garage_memory_mib: u32,
    #[schemars(range(min = 0))]
    pub one_way_delay_ms: f64,
    #[schemars(range(min = 0))]
    pub network_jitter_ms: f64,
    #[schemars(range(min = 1))]
    pub bandwidth_mbps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelCodePool {
    #[schemars(range(min = 1))]
    pub workers_per_lash_worker: u32,
    #[schemars(range(min = 1))]
    pub queue_limit: u32,
    #[schemars(range(min = 1))]
    pub execution_memory_mib: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    #[schemars(range(min = 1))]
    pub scrape_s: u32,
    #[schemars(range(min = 1))]
    pub journal_sample_s: u32,
    #[schemars(range(min = 1))]
    pub retention_s: u32,
    #[schemars(range(min = 1))]
    pub recovery_stable_s: u32,
    #[schemars(range(min = 1))]
    pub comparison_repetitions: u32,
    #[schemars(range(min = 1))]
    pub bootstrap_resamples: u32,
    #[schemars(schema_with = "confidence_schema")]
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    #[schemars(regex(pattern = "^I$"))]
    pub default: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub worker_shape: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub pg_connections: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub attachment_max: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub maximum_drain_batch: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub polling: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub process_prune: String,
    #[schemars(regex(pattern = "^[IV](: .+)?$"))]
    pub restate_partitions: String,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    #[schemars(regex(pattern = "^[0-9a-f]{40}$"))]
    pub lash_sha: String,
    #[schemars(regex(pattern = "^[0-9a-f]{40}$"))]
    pub figments_sha: String,
}
