/// version_surface = "coexist"
/// version_guard(items(LASH_RUNTIME_EFFECT_ENVELOPE_DOMAIN_VERSION, capture, verify))
const LASH_RUNTIME_EFFECT_ENVELOPE_DOMAIN_VERSION: &str = "lash-runtime-effect-envelope/v3";

pub use lash_core_store::runtime_error::*;
use std::collections::BTreeSet;

use lash_trace::{
    TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::RuntimeEffectEnvelope;

/// Matches the whole-body bound used by extended provider-request tracing.
/// Values over this bound are omitted whole rather than prefix-truncated.
const MAX_DIFF_VALUE_JSON_BYTES: usize = 2_048;
const ERROR_SUMMARY_PATH_LIMIT: usize = 8;

/// The exact serialized envelope bytes and the BLAKE3 verdict derived from
/// those same bytes.
///
/// Durable substrates record this value as one unit. Replay validation parses
/// `json` only to explain a mismatch; the hash is always over `json` itself.
///
/// # Moving a command encoding
///
/// This type owns the journaled effect encoding, so the decision made for
/// FIG-2968 and executed by FIG-2983 is recorded here rather than in a store
/// crate that only sees one backend.
///
/// The hash domain is `lash-runtime-effect-envelope/v3` and it did **not** move
/// when the sleep command's serialized shape did (resolved
/// `Sleep { duration_ms }` became `Sleep { spec }`). Bumping the domain rehashes
/// every journaled envelope, not only the ones whose bytes changed: non-sleep
/// rows that still replay correctly would stop reconstructing, and the
/// historical-fixture contracts that reconstruct recorded hashes would fail. The
/// domain names the hashing construction, not the vocabulary of commands inside
/// it; it moves only when the construction itself changes.
///
/// A model request's content is journaled by digest, not verbatim (FIG-3980,
/// changed in place under the pre-1.0 version freeze): see
/// `request_digest`. The bytes are still this envelope's canonical form for
/// every command, so the hash fixpoint below holds; a journaled model request
/// just cannot be decoded back into its command, and nothing needs it to.
///
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CanonicalRuntimeEffectEnvelope {
    json: String,
    hash: String,
}

impl CanonicalRuntimeEffectEnvelope {
    pub(crate) fn capture(
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let json = super::request_digest::journaled_envelope_json(envelope).map_err(|err| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectEnvelopeHash,
                format!("failed to serialize runtime effect envelope: {err}"),
            )
        })?;
        let hash = crate::stable_hash::blake3_hex(
            LASH_RUNTIME_EFFECT_ENVELOPE_DOMAIN_VERSION,
            json.as_bytes(),
        );
        Ok(Self { json, hash })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The exact serialized envelope bytes this value's hash was derived from.
    ///
    /// The bytes are the envelope's canonical form, so decoding them and
    /// re-capturing the result reproduces this same hash. That fixpoint is what
    /// lets a reader that holds only the journal — the group drain, which was
    /// not the process that built the envelope — rebuild the effect and have it
    /// pass its own replay fence rather than be refused as a mismatch.
    ///
    /// Deliberately not a typed decode: the recorded bytes may be older than
    /// this build, and the one consumer that must decode them owns the error
    /// vocabulary the failure is reported in.
    pub fn json(&self) -> &str {
        &self.json
    }

    fn verify(&self, side: &str) -> Result<(), RuntimeEffectControllerError> {
        let actual = crate::stable_hash::blake3_hex(
            LASH_RUNTIME_EFFECT_ENVELOPE_DOMAIN_VERSION,
            self.json.as_bytes(),
        );
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

/// Trace capability dedicated to replay-divergence diagnostics.
///
/// Construction returns `None` only when nothing observes the runtime. A
/// divergence is rare and always actionable, so its evidence reaches a
/// configured observer at the default standard trace level without enabling
/// other extended events. It is met while a recorded step is served, so it is
/// a diagnostic of the attempt that met it, not a replayed lifecycle record:
/// each attempt that meets the divergence reports it under its own identity.
#[derive(Clone)]
pub struct RuntimeEffectReplayTrace {
    standing: crate::trace::TraceStanding,
    context: TraceContext,
}

impl RuntimeEffectReplayTrace {
    pub fn for_divergence(
        tracing: &crate::trace::TraceRuntime,
        scope: Option<lash_trace::DurableTraceScope>,
        context: TraceContext,
    ) -> Option<Self> {
        // Deliberately bypass the ordinary extended-level gate for this one
        // rare diagnostic; the observer remains the emission boundary.
        tracing.is_observed().then(|| Self {
            standing: tracing.unreplayed(scope),
            context,
        })
    }

    fn emit(&self, event: TraceEffectEnvelopeDiffEvent) {
        self.standing.observe(|| {
            (
                self.context.clone(),
                TraceEvent::EffectEnvelopeDiff { event },
            )
        });
    }
}

/// Validate a reconstructed effect envelope against a substrate-recorded
/// canonical envelope.
///
/// This is the shared replay-validation seam. Substrates supply only their
/// public mismatch code; recorded-shape compatibility runs before integrity or
/// equality checks, and comparison, summary construction, and divergence
/// diagnostics remain identical for every consumer.
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
    let summary = RuntimeEffectReplayMismatchReport {
        divergent_path_count: paths.len(),
        first_divergent_paths: paths
            .iter()
            .take(ERROR_SUMMARY_PATH_LIMIT)
            .cloned()
            .collect(),
        effect_kind: reconstructed_value
            .pointer("/command/type")
            .and_then(Value::as_str)
            .map(str::to_string),
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
            "recorded runtime effect hash {} did not match reconstructed envelope hash {}; divergent_path_count={}; divergent_paths=[{}]; effect_kind={}",
            recorded.hash,
            reconstructed.hash,
            summary.divergent_path_count,
            render_divergent_paths(&summary),
            summary.effect_kind.as_deref().unwrap_or("unknown")
        ),
    )
    .with_summary(summary))
}

fn render_divergent_paths(summary: &RuntimeEffectReplayMismatchReport) -> String {
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

#[expect(
    clippy::expect_used,
    reason = "a `serde_json::Value` re-encodes into an in-memory buffer"
)]
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
    use crate::{RuntimeEffectCommand, RuntimeEffectInvocation};
    use std::sync::Arc;

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
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn("session", "turn"),
                    "tool-attempt:test",
                )
                .expect("valid validation address"),
                crate::RuntimeAttribution::for_turn("session", "turn", 0, 0),
                "tool-attempt:test",
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: Box::new(crate::PreparedToolCall {
                    call_id: crate::ToolCallId::fixture("validation-call"),
                    provider_call_id: None,
                    tool_id: "tool:validation".into(),
                    tool_name: "validation".into(),
                    args: input,
                    replay: None,
                    prepared_payload: serde_json::Value::Null,
                }),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        )
    }

    fn canonical(input: Value) -> CanonicalRuntimeEffectEnvelope {
        CanonicalRuntimeEffectEnvelope::capture(&envelope(input)).expect("canonical envelope")
    }

    fn session_list_envelope() -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn("session-blue", "turn-blue"),
                    "trigger:list",
                )
                .expect("valid trigger-list address"),
                crate::RuntimeAttribution::for_session("session-blue"),
                "trigger:list",
            ),
            RuntimeEffectCommand::Trigger {
                command: Box::new(crate::TriggerCommand::List {
                    owner_scope: crate::TriggerOwnerScope::session("session-blue"),
                    filter: crate::TriggerSubscriptionFilter::for_session("session-blue"),
                }),
            },
        )
    }

    /// The last shape that carried the batch command (FIG-3397): a one-call
    /// `ToolBatch` envelope in its canonical form.
    const PREDECESSOR_TOOL_BATCH_ENVELOPE: &str = r#"{"json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"session-blue\",\"turn_id\":\"turn-blue\"},\"replay_key\":\"turn-blue:tool-batch:batch-blue\"},\"effect_id\":\"tool-batch:batch-blue\",\"attribution\":{\"session_id\":\"session-blue\"}},\"command\":{\"type\":\"tool_batch\",\"batch\":{\"batch_id\":\"batch-blue\",\"calls\":[{\"call\":{\"call_id\":\"call-blue\",\"tool_id\":\"tool:blue\",\"tool_name\":\"blue\",\"args\":{\"q\":1}},\"replay_suffix\":\"child:0:call-blue\"}]}}}","hash":"36052881d682e556eb511c93336435ca2192d8e1a1020ef6032eeca81ae1f0ab"}"#;

    /// The last shape whose trigger-list filter carried `session_id`
    /// (FIG-2886).
    const PREDECESSOR_SESSION_LIST_ENVELOPE: &str = r#"{"json":"{\"invocation\":{\"address\":{\"execution_scope\":{\"type\":\"turn\",\"session_id\":\"session-blue\",\"turn_id\":\"turn-blue\"},\"replay_key\":\"trigger:list\"},\"effect_id\":\"trigger:list\",\"attribution\":{\"session_id\":\"session-blue\"}},\"command\":{\"type\":\"trigger\",\"command\":{\"op\":\"list\",\"owner_scope\":{\"type\":\"session\",\"session_id\":\"session-blue\"},\"filter\":{\"session_id\":\"session-blue\"}}}}","hash":"51ba8b5ff2d3fe2ff5f5009d4f8fc42946b11e86575901a64393b3e01c912db1"}"#;

    /// A recorded envelope this build never writes has no refusal of its own:
    /// it is what any other divergent journal row is, a replay mismatch under
    /// the substrate's code.
    #[test]
    fn a_recorded_shape_this_build_never_writes_is_an_ordinary_replay_divergence() {
        for retired in [
            PREDECESSOR_TOOL_BATCH_ENVELOPE,
            PREDECESSOR_SESSION_LIST_ENVELOPE,
        ] {
            let recorded: CanonicalRuntimeEffectEnvelope =
                serde_json::from_str(retired).expect("recorded envelope");
            let reconstructed = session_list_envelope()
                .canonical_form()
                .expect("reconstructed envelope");
            let error = validate_replayed_effect_envelope(
                &recorded,
                &reconstructed,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                None,
            )
            .expect_err("a journal row this build did not write must not replay");
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                "{retired}"
            );
            assert!(error.summary.is_some(), "{retired}");
        }
    }

    fn mismatch_paths(recorded: Value, reconstructed: Value) -> Vec<String> {
        let error = validate_replayed_effect_envelope(
            &canonical(recorded),
            &canonical(reconstructed),
            crate::RuntimeErrorCode::EffectReplayDivergence,
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
            crate::RuntimeErrorCode::EffectReplayDivergence,
            None,
        )
        .expect_err("mismatch");
        assert_eq!(
            error.summary.as_deref(),
            Some(&RuntimeEffectReplayMismatchReport {
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
                effect_kind: Some("tool_attempt".to_string()),
            })
        );
        assert!(
            error.message.ends_with(
                "command.call.args.f6, command.call.args.f7, <2 more paths elided>]; \
                     effect_kind=tool_attempt"
            ),
            "bounded rendered summary must say how many paths were elided: {}",
            error.message
        );
    }

    #[test]
    fn large_divergent_value_is_whole_value_elided() {
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn TraceSink> = sink.clone();
        let trace = RuntimeEffectReplayTrace::for_divergence(
            &crate::trace::TraceRuntime::default().with_trace_sink(sink_dyn),
            None,
            TraceContext::default(),
        )
        .expect("configured divergence trace");
        let error = validate_replayed_effect_envelope(
            &canonical(json!({"tool_results": [{"value": "a".repeat(3_000)}]})),
            &canonical(json!({"tool_results": [{"value": "b".repeat(3_000)}]})),
            crate::RuntimeErrorCode::EffectReplayDivergence,
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
            crate::RuntimeErrorCode::EffectReplayDivergence,
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
    fn emitted_effect_diff_keeps_authoritative_projection_and_host_metadata() {
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn TraceSink> = sink.clone();
        let mut base = TraceContext::default()
            .for_session("ambient-session")
            .for_turn("ambient-turn")
            .for_turn_index(12)
            .for_protocol_iteration(7);
        base.run_id = Some("host-run".to_string());
        base.parent_graph_node_id = Some("host:explicit-parent".to_string());
        base.metadata
            .insert("host_key".to_string(), serde_json::json!("kept"));
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                crate::ExecutionScope::process(crate::process_id_for_test("effect-trace-process")),
                "effect-trace",
            )
            .expect("valid effect address"),
            crate::RuntimeAttribution::none(),
            "effect-trace",
        )
        .with_caused_by(Some(crate::CausalRef::Process {
            process_id: crate::process_id_for_test("cause-process"),
        }));
        let trace = RuntimeEffectReplayTrace::for_divergence(
            &crate::trace::TraceRuntime::default()
                .with_trace_sink(sink_dyn)
                .with_base_context(base),
            None,
            crate::trace::trace_context_from_effect_invocation(&invocation),
        )
        .expect("configured divergence trace");

        validate_replayed_effect_envelope(
            &canonical(json!({"value": 1})),
            &canonical(json!({"value": 2})),
            crate::RuntimeErrorCode::EffectReplayDivergence,
            Some(&trace),
        )
        .expect_err("mismatch emits the relevant effect event");

        let records = sink.records.lock_recover();
        let context = &records[0].context;
        assert_eq!(context.session_id, None);
        assert_eq!(context.turn_id, None);
        assert_eq!(context.turn_index, None);
        assert_eq!(context.protocol_iteration, None);
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("host:explicit-parent")
        );
        assert_eq!(context.run_id.as_deref(), Some("host-run"));
        assert_eq!(
            context.metadata.get("host_key"),
            Some(&serde_json::json!("kept"))
        );
    }
}
