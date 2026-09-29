//! Synthetic, versioned inputs for the FIG-3790 multi-node load test.
//! Generation plans work without services; later lanes execute them through public APIs.

mod distribution;
mod generator;
mod payload;
mod provider;
mod schema;
mod spec;

pub use distribution::Distribution;
pub use generator::{
    AttachmentPlan, Generator, LlmRequestPlan, OperationId, ProcessPlan, QueuedInputPlan,
    ToolCallPlan, TurnPlan,
};
pub use payload::tool_schema;
pub use payload::{SyntheticAttachment, SyntheticPayloads};
pub use provider::{CallCounts, CallKind, ProviderChunk, ProviderResponse};
pub use spec::{
    Attachments, Collection, Cron, Faults, GeneratorVersion, Inventory, ModelCodePool, Observation,
    Processes, Provenance, Provider, Queued, Tools, Topology, WorkloadSpec,
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::BTreeMap, sync::OnceLock};

pub const V1_JSON: &str = include_str!("../../workloads/figments-v1.json");
pub const V1_SCHEMA_JSON: &str = include_str!("../../workloads/workload-v1.schema.json");

pub fn schema() -> schemars::schema::RootSchema {
    schema::generate()
}

/// Settings can only enter the generator after structural and semantic validation.
#[derive(Debug, Clone)]
pub struct Workload(WorkloadSpec);

impl Workload {
    pub fn v1() -> Result<Self> {
        Self::parse(V1_JSON)
    }

    pub fn parse(json: &str) -> Result<Self> {
        // Deserialize first so duplicate typed fields cannot disappear into a Value map.
        let spec: WorkloadSpec = serde_json::from_str(json).context("decode workload v1")?;
        let value = serde_json::to_value(&spec)?;
        static VALIDATOR: OnceLock<Result<jsonschema::JSONSchema, String>> = OnceLock::new();
        let validator = VALIDATOR
            .get_or_init(|| {
                serde_json::to_value(schema())
                    .map_err(|e| e.to_string())
                    .and_then(|value| {
                        jsonschema::JSONSchema::compile(&value).map_err(|e| e.to_string())
                    })
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("workload schema: {e}"))?;
        if let Err(errors) = validator.validate(&value) {
            anyhow::bail!(
                "invalid workload: {}",
                errors.map(|e| e.to_string()).collect::<Vec<_>>().join("; ")
            );
        }
        ensure!(spec.turns_per_session_s > 0.0, "turn rate must be positive");
        ensure!(
            spec.collection.confidence > 0.0 && spec.collection.confidence < 1.0,
            "confidence must be inside (0, 1)"
        );
        let distributions = [
            ("history_prefill_turns", &spec.history_prefill_turns, 0),
            ("prompt_bytes", &spec.prompt_bytes, 128),
            ("input_bytes", &spec.input_bytes, 128),
            ("tool_argument_bytes", &spec.tool_argument_bytes, 128),
            ("tool_result_bytes", &spec.tool_result_bytes, 128),
            ("provider_output_bytes", &spec.provider_output_bytes, 128),
            ("tools.fanout", &spec.tools.fanout, 1),
            ("tools.callback_ms", &spec.tools.callback_ms, 1),
            ("processes.fanout", &spec.processes.fanout, 1),
            ("attachments.count", &spec.attachments.count, 1),
            (
                "attachments.aggregate_bytes",
                &spec.attachments.aggregate_bytes,
                128,
            ),
            ("provider.latency_ms", &spec.provider.latency_ms, 1),
        ];
        for (name, distribution, minimum) in distributions {
            distribution.validate(name, minimum)?;
        }
        ensure!(
            spec.saturation_rates_per_session_s
                .iter()
                .all(|x| x.is_finite() && *x > 0.0)
                && spec
                    .saturation_rates_per_session_s
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "saturation rates must be positive and strictly increasing"
        );
        let smallest_aggregate = spec
            .attachments
            .aggregate_bytes
            .buckets()
            .iter()
            .map(|x| x.0)
            .min()
            .unwrap_or(0);
        let largest_count = spec
            .attachments
            .count
            .buckets()
            .iter()
            .map(|x| x.0)
            .max()
            .unwrap_or(0);
        ensure!(
            smallest_aggregate / largest_count >= 128,
            "attachment split cannot fit a valid PNG"
        );
        ensure!(
            spec.topology.replication <= spec.topology.restate_nodes,
            "replication exceeds node count"
        );
        ensure!(
            u64::from(spec.faults.worker_kill_s) + u64::from(spec.faults.worker_restart_delay_s)
                < u64::from(spec.faults.restate_restart_s)
                && u64::from(spec.faults.restate_restart_s)
                    + u64::from(spec.faults.restate_restart_delay_s)
                    < u64::from(spec.faults.rolling_deploy_s)
                && u64::from(spec.faults.rolling_deploy_s)
                    + u64::from(spec.faults.rolling_worker_pause_s)
                    < u64::from(spec.fault_window_s),
            "faults must finish in sequence inside the fault window"
        );
        let smallest_output = spec
            .provider_output_bytes
            .buckets()
            .iter()
            .map(|x| x.0)
            .min()
            .unwrap_or(0);
        ensure!(
            spec.provider.chunks <= smallest_output,
            "provider chunks exceed output bytes"
        );
        let mut expected = BTreeMap::new();
        for (key, child) in value.as_object().into_iter().flatten() {
            if key != "provenance" {
                field_paths(child, &format!("/{key}"), &mut expected);
            }
        }
        ensure!(
            expected.keys().eq(spec.provenance.fields.keys()),
            "provenance must name every field, with no unknown paths"
        );
        for (path, source) in &spec.provenance.fields {
            ensure!(
                source == "I"
                    || source == "V"
                    || source.starts_with("I: ")
                    || source.starts_with("V: "),
                "{path}: invalid provenance"
            );
        }
        Ok(Self(spec))
    }

    pub fn spec(&self) -> &WorkloadSpec {
        &self.0
    }

    pub fn provenance(&self, field: &str) -> Option<&str> {
        self.0.provenance.fields.get(field).map(String::as_str)
    }
}

fn field_paths(value: &Value, path: &str, fields: &mut BTreeMap<String, ()>) {
    if let Value::Object(object) = value {
        for (key, child) in object {
            field_paths(child, &format!("{path}/{key}"), fields);
        }
    } else {
        fields.insert(path.to_owned(), ());
    }
}
