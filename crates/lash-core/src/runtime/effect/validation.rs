use std::collections::BTreeSet;
use std::sync::Arc;

use lash_trace::{
    TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceEvent, TraceSink,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{RuntimeEffectControllerError, RuntimeEffectEnvelope};

/// Matches the whole-body bound used by extended provider-request tracing.
/// Values over this bound are omitted whole rather than prefix-truncated.
const MAX_DIFF_VALUE_JSON_BYTES: usize = 2_048;
const ERROR_SUMMARY_PATH_LIMIT: usize = 8;

/// The exact serialized envelope bytes and the SHA-256 verdict derived from
/// those same bytes.
///
/// Durable substrates record this value as one unit. Replay validation parses
/// `json` only to explain a mismatch; the hash is always over `json` itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CanonicalRuntimeEffectEnvelope {
    json: String,
    hash: String,
}

impl CanonicalRuntimeEffectEnvelope {
    pub(crate) fn capture(
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let json = crate::stable_hash::stable_json_string(envelope).map_err(|err| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectEnvelopeHash,
                format!("failed to serialize runtime effect envelope: {err}"),
            )
        })?;
        let hash = crate::stable_hash::sha256_hex(json.as_bytes());
        Ok(Self { json, hash })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    fn verify(&self, side: &str) -> Result<(), RuntimeEffectControllerError> {
        let actual = crate::stable_hash::sha256_hex(self.json.as_bytes());
        if actual == self.hash {
            return Ok(());
        }
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
            format!(
                "{side} envelope hash {} was not derived from its canonical serialized form (derived {actual})",
                self.hash
            ),
        ))
    }

    #[cfg(test)]
    fn with_forced_hash(mut self, hash: impl Into<String>) -> Self {
        self.hash = hash.into();
        self
    }
}

/// Compact, content-free mismatch evidence retained on the controller error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEffectReplayMismatchSummary {
    pub divergent_path_count: usize,
    pub first_divergent_paths: Vec<String>,
}

/// Trace capability dedicated to replay-divergence diagnostics.
///
/// Construction returns `None` only when no sink is configured. A divergence
/// is rare and always actionable, so its evidence reaches a configured sink at
/// the default standard trace level without enabling other extended events.
#[derive(Clone)]
pub struct RuntimeEffectReplayTrace {
    sink: Arc<dyn TraceSink>,
    base_context: TraceContext,
    context: TraceContext,
    clock: Arc<dyn crate::Clock>,
}

impl RuntimeEffectReplayTrace {
    pub(crate) fn for_divergence(
        sink: Option<&Arc<dyn TraceSink>>,
        base_context: TraceContext,
        context: TraceContext,
        clock: Arc<dyn crate::Clock>,
    ) -> Option<Self> {
        // Deliberately bypass the ordinary extended-level gate for this one
        // rare diagnostic; the sink remains the emission boundary.
        Some(Self {
            sink: Arc::clone(sink?),
            base_context,
            context,
            clock,
        })
    }

    fn emit(&self, event: TraceEffectEnvelopeDiffEvent) {
        crate::trace::emit_trace(
            &Some(Arc::clone(&self.sink)),
            &self.base_context,
            self.context.clone(),
            TraceEvent::EffectEnvelopeDiff { event },
            self.clock.as_ref(),
        );
    }
}

/// Validate a reconstructed effect envelope against a substrate-recorded
/// canonical envelope.
///
/// This is the shared replay-validation seam. Substrates supply only their
/// public mismatch code; canonical comparison, summary construction, and
/// divergence diagnostics remain identical for every consumer.
pub fn validate_replayed_effect_envelope(
    recorded: &CanonicalRuntimeEffectEnvelope,
    reconstructed: &CanonicalRuntimeEffectEnvelope,
    mismatch_code: crate::RuntimeErrorCode,
    trace: Option<&RuntimeEffectReplayTrace>,
) -> Result<(), RuntimeEffectControllerError> {
    debug_assert!(
        mismatch_code.is_replay_mismatch(),
        "replay-validation seam requires a classified replay-mismatch code: {mismatch_code}"
    );
    recorded.verify("recorded")?;
    reconstructed.verify("reconstructed")?;

    if recorded.hash == reconstructed.hash {
        return Ok(());
    }

    if recorded.json == reconstructed.json {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
            "envelope hashes differed even though their canonical serialized forms were identical",
        ));
    }

    let recorded_value: Value = serde_json::from_str(&recorded.json).map_err(|err| {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode,
            format!("failed to decode recorded canonical envelope: {err}"),
        )
    })?;
    let reconstructed_value: Value = serde_json::from_str(&reconstructed.json).map_err(|err| {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode,
            format!("failed to decode reconstructed canonical envelope: {err}"),
        )
    })?;
    let mut differences = Vec::new();
    collect_differences(
        "",
        Some(&recorded_value),
        Some(&reconstructed_value),
        &mut differences,
    );
    if differences.is_empty() {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
            "envelope hashes differed but their canonical forms had zero divergent structural paths",
        ));
    }

    let paths = differences
        .iter()
        .map(|difference| difference.path.clone())
        .collect::<Vec<_>>();
    let summary = RuntimeEffectReplayMismatchSummary {
        divergent_path_count: paths.len(),
        first_divergent_paths: paths
            .iter()
            .take(ERROR_SUMMARY_PATH_LIMIT)
            .cloned()
            .collect(),
    };
    if let Some(trace) = trace {
        trace.emit(TraceEffectEnvelopeDiffEvent {
            recorded_envelope_hash: recorded.hash.clone(),
            reconstructed_envelope_hash: reconstructed.hash.clone(),
            divergent_paths: differences,
        });
    }

    Err(RuntimeEffectControllerError::new(
        mismatch_code,
        format!(
            "recorded runtime effect hash {} did not match reconstructed envelope hash {}; divergent_path_count={}; divergent_paths=[{}]",
            recorded.hash,
            reconstructed.hash,
            summary.divergent_path_count,
            render_divergent_paths(&summary)
        ),
    )
    .with_summary(summary))
}

fn render_divergent_paths(summary: &RuntimeEffectReplayMismatchSummary) -> String {
    let mut paths = summary.first_divergent_paths.clone();
    let elided = summary.divergent_path_count.saturating_sub(paths.len());
    if elided > 0 {
        paths.push(format!("<{elided} more paths elided>"));
    }
    paths.join(", ")
}

fn collect_differences(
    path: &str,
    recorded: Option<&Value>,
    reconstructed: Option<&Value>,
    differences: &mut Vec<TraceEffectEnvelopeDiffEntry>,
) {
    match (recorded, reconstructed) {
        (Some(Value::Object(recorded)), Some(Value::Object(reconstructed))) => {
            let keys = recorded
                .keys()
                .chain(reconstructed.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                collect_differences(
                    &field_path(path, key),
                    recorded.get(key),
                    reconstructed.get(key),
                    differences,
                );
            }
        }
        (Some(Value::Array(recorded)), Some(Value::Array(reconstructed))) => {
            for index in 0..recorded.len().max(reconstructed.len()) {
                collect_differences(
                    &format!("{path}[{index}]"),
                    recorded.get(index),
                    reconstructed.get(index),
                    differences,
                );
            }
        }
        (Some(recorded), Some(reconstructed)) if recorded == reconstructed => {}
        (recorded, reconstructed) => differences.push(TraceEffectEnvelopeDiffEntry {
            path: path.to_string(),
            recorded: trace_value(recorded),
            reconstructed: trace_value(reconstructed),
        }),
    }
}

fn field_path(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        field.to_string()
    } else {
        format!("{parent}.{field}")
    }
}

fn trace_value(value: Option<&Value>) -> TraceEffectEnvelopeDiffValue {
    let Some(value) = value else {
        return TraceEffectEnvelopeDiffValue::Missing;
    };
    let json = serde_json::to_vec(value).expect("serde_json::Value always serializes");
    let omitted = json.len() > MAX_DIFF_VALUE_JSON_BYTES;
    TraceEffectEnvelopeDiffValue::Present {
        json_len: json.len(),
        json_sha256: crate::stable_hash::sha256_hex(&json),
        value_json: (!omitted).then(|| value.clone()),
        value_json_omitted_reason: omitted.then(|| "size_limit".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lash_sansio::sync::MutexExt;
    use lash_trace::{TraceRecord, TraceSink, TraceSinkError};
    use serde_json::json;

    use super::*;
    use crate::{RuntimeEffectCommand, RuntimeEffectKind, RuntimeInvocation, RuntimeScope};

    #[derive(Default)]
    struct RecordingSink {
        records: Mutex<Vec<TraceRecord>>,
    }

    impl TraceSink for RecordingSink {
        fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
            self.records.lock_recover().push(record.clone());
            Ok(())
        }
    }

    fn envelope(input: Value) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::for_turn("session", "turn", 0, 0),
                "tool-attempt:test",
                RuntimeEffectKind::ToolAttempt,
                "tool-attempt:test",
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: crate::PreparedToolCall::from_parts(
                    "validation-call",
                    "tool:validation",
                    "validation",
                    input,
                    None,
                    serde_json::Value::Null,
                ),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        )
    }

    fn canonical(input: Value) -> CanonicalRuntimeEffectEnvelope {
        CanonicalRuntimeEffectEnvelope::capture(&envelope(input)).expect("canonical envelope")
    }

    fn mismatch_paths(recorded: Value, reconstructed: Value) -> Vec<String> {
        let error = validate_replayed_effect_envelope(
            &canonical(recorded),
            &canonical(reconstructed),
            crate::RuntimeErrorCode::RestateEffectHashMismatch,
            None,
        )
        .expect_err("mismatch");
        error.summary.expect("summary").first_divergent_paths
    }

    #[test]
    fn reports_deep_tool_result_scalar_path() {
        assert_eq!(
            mismatch_paths(
                json!({"tool_results": [{"value": {"embedding_duration_ms": 1761}}]}),
                json!({"tool_results": [{"value": {"embedding_duration_ms": 532}}]}),
            ),
            ["command.call.args.tool_results[0].value.embedding_duration_ms"]
        );
    }

    #[test]
    fn reports_added_and_removed_field_paths() {
        assert_eq!(
            mismatch_paths(
                json!({"tool_results": [{"kept": true, "removed": 1}]}),
                json!({"tool_results": [{"kept": true, "added": 2}]}),
            ),
            [
                "command.call.args.tool_results[0].added",
                "command.call.args.tool_results[0].removed",
            ]
        );
    }

    #[test]
    fn reports_reordered_array_element_paths() {
        assert_eq!(
            mismatch_paths(
                json!({"tool_results": [{"value": ["first", "second"]}]}),
                json!({"tool_results": [{"value": ["second", "first"]}]}),
            ),
            [
                "command.call.args.tool_results[0].value[0]",
                "command.call.args.tool_results[0].value[1]",
            ]
        );
    }

    #[test]
    fn error_summary_counts_all_paths_and_keeps_first_eight() {
        let error = validate_replayed_effect_envelope(
            &canonical(json!({
                "f0": 0, "f1": 0, "f2": 0, "f3": 0, "f4": 0,
                "f5": 0, "f6": 0, "f7": 0, "f8": 0, "f9": 0
            })),
            &canonical(json!({
                "f0": 1, "f1": 1, "f2": 1, "f3": 1, "f4": 1,
                "f5": 1, "f6": 1, "f7": 1, "f8": 1, "f9": 1
            })),
            crate::RuntimeErrorCode::RestateEffectHashMismatch,
            None,
        )
        .expect_err("mismatch");
        assert_eq!(
            error.summary.as_ref(),
            Some(&RuntimeEffectReplayMismatchSummary {
                divergent_path_count: 10,
                first_divergent_paths: vec![
                    "command.call.args.f0".to_string(),
                    "command.call.args.f1".to_string(),
                    "command.call.args.f2".to_string(),
                    "command.call.args.f3".to_string(),
                    "command.call.args.f4".to_string(),
                    "command.call.args.f5".to_string(),
                    "command.call.args.f6".to_string(),
                    "command.call.args.f7".to_string(),
                ],
            })
        );
        assert!(
            error
                .message
                .ends_with("command.call.args.f6, command.call.args.f7, <2 more paths elided>]"),
            "bounded rendered summary must say how many paths were elided: {}",
            error.message
        );
    }

    #[test]
    fn large_divergent_value_is_whole_value_elided() {
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn TraceSink> = sink.clone();
        let trace = RuntimeEffectReplayTrace::for_divergence(
            Some(&sink_dyn),
            TraceContext::default(),
            TraceContext::default(),
            Arc::new(crate::SystemClock),
        )
        .expect("configured divergence trace");
        let error = validate_replayed_effect_envelope(
            &canonical(json!({"tool_results": [{"value": "a".repeat(3_000)}]})),
            &canonical(json!({"tool_results": [{"value": "b".repeat(3_000)}]})),
            crate::RuntimeErrorCode::RestateEffectHashMismatch,
            Some(&trace),
        )
        .expect_err("mismatch");
        assert_eq!(
            error.summary.expect("summary").first_divergent_paths,
            ["command.call.args.tool_results[0].value"]
        );

        let records = sink.records.lock_recover();
        let TraceEvent::EffectEnvelopeDiff { event } = &records[0].event else {
            panic!("expected effect-envelope diff trace");
        };
        assert_eq!(
            event.divergent_paths[0].recorded,
            TraceEffectEnvelopeDiffValue::Present {
                json_len: 3_002,
                json_sha256: crate::stable_hash::sha256_hex(
                    serde_json::to_string(&"a".repeat(3_000))
                        .expect("serialize literal")
                        .as_bytes()
                ),
                value_json: None,
                value_json_omitted_reason: Some("size_limit".to_string()),
            }
        );
    }

    #[test]
    fn forced_hash_mismatch_with_identical_canonical_forms_fails_loudly() {
        let recorded = canonical(json!({"tool_results": []})).with_forced_hash("forced");
        let reconstructed = canonical(json!({"tool_results": []}));
        let error = validate_replayed_effect_envelope(
            &recorded,
            &reconstructed,
            crate::RuntimeErrorCode::RestateEffectHashMismatch,
            None,
        )
        .expect_err("canonical invariant failure");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant
        );
        assert!(error.summary.is_none());
    }

    #[test]
    fn diff_trace_uses_default_wiring_when_a_sink_is_configured() {
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn TraceSink> = sink.clone();
        let trace = RuntimeEffectReplayTrace::for_divergence(
            Some(&sink_dyn),
            TraceContext::default(),
            TraceContext::default(),
            Arc::new(crate::SystemClock),
        )
        .expect("a configured sink is sufficient under default wiring");
        let error = validate_replayed_effect_envelope(
            &canonical(json!({"value": 1})),
            &canonical(json!({"value": 2})),
            crate::RuntimeErrorCode::RestateEffectHashMismatch,
            Some(&trace),
        )
        .expect_err("mismatch");
        assert_eq!(error.summary.expect("summary").divergent_path_count, 1);
        assert!(matches!(
            sink.records.lock_recover()[0].event,
            TraceEvent::EffectEnvelopeDiff { .. }
        ));

        assert!(
            RuntimeEffectReplayTrace::for_divergence(
                None,
                TraceContext::default(),
                TraceContext::default(),
                Arc::new(crate::SystemClock),
            )
            .is_none()
        );
    }
}
