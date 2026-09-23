//! Compile-time witnesses for observation- and admin-area facade and integrator contracts.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices: at the
//! dispatch-time recount this area held 235 rows. The 219 rows whose item
//! still exists are type-checked here through the path a host or integrator
//! would name — `lash::` for facade surface, `lash_core::` for internal seams
//! the integrator classes consume directly. The 16 rows whose item no longer
//! exists anywhere in this workspace are listed in the pull request rather
//! than witnessed here. The seven `otel-trace`-gated rows live in
//! `otel_trace_evidence.rs` instead: the feature-coverage contract forbids a
//! predicate spanning coverage owners, and `testing` + `otel-trace` are owned
//! by different lanes.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0004: lash::TurnInput::trace_turn_id [field]
    field_witness(|value: &lash::TurnInput| {
        let _ = &value.trace_turn_id;
    });
    // W0005: lash::direct::DirectCompletion::usage [field]
    field_witness(|value: &lash::direct::DirectCompletion| {
        let _ = &value.usage;
    });
    // W0006: lash::direct::DirectLlmCompletion::usage [field]
    field_witness(|value: &lash::direct::DirectLlmCompletion| {
        let _ = &value.usage;
    });
    // W0007: lash::observe::LiveReplayGapReason [enum]
    type_witness::<lash::observe::LiveReplayGapReason>();
    // W0008: lash::observe::LiveReplayStore [trait]
    fn trait_witness_0008<T: lash::observe::LiveReplayStore>() {}
    // W0009: lash::observe::LiveReplayEventDraft [struct]
    type_witness::<lash::observe::LiveReplayEventDraft>();
    // W0010: lash::observe::LiveReplayEventDraft::new [function]
    let _: fn(
        Option<lash::TurnId>,
        lash::observe::SessionObservationEventPayload,
    ) -> lash::observe::LiveReplayEventDraft = lash::observe::LiveReplayEventDraft::new;
    // W0011: lash::observe::LiveReplayEventDraft::payload [field]
    field_witness(|value: &lash::observe::LiveReplayEventDraft| {
        let _ = &value.payload;
    });
    // W0012: lash::observe::LiveReplayEventDraft::turn_id [field]
    field_witness(|value: &lash::observe::LiveReplayEventDraft| {
        let _ = &value.turn_id;
    });
    // W0013: lash::observe::LiveReplayStore::prepare_publication [function]
    fn meth_0013<T: lash::observe::LiveReplayStore>(_: &T) {
        let _ = T::prepare_publication;
    }
    // W0014: lash::observe::LiveReplayStore::publish_prepared [function]
    fn meth_0014<T: lash::observe::LiveReplayStore>(_: &T) {
        let _ = T::publish_prepared;
    }
    // W0015: lash::observe::PreparedLiveReplayPublication [struct]
    type_witness::<lash::observe::PreparedLiveReplayPublication>();
    // W0016: lash::observe::PreparedLiveReplayPublication::events [function]
    let _ = lash::observe::PreparedLiveReplayPublication::events;
    // W0017: lash::observe::PreparedLiveReplayPublication::into_parts [function]
    let _ = lash::observe::PreparedLiveReplayPublication::into_parts;
    // W0018: lash::observe::PreparedLiveReplayPublication::latest_cursor [function]
    let _ = lash::observe::PreparedLiveReplayPublication::latest_cursor;
    // W0019: lash::observe::PreparedLiveReplayPublication::new [function]
    let _: fn(
        String,
        Vec<std::sync::Arc<lash::observe::SessionObservationEvent>>,
        fn(&str),
    ) -> Result<
        lash::observe::PreparedLiveReplayPublication,
        lash::observe::LiveReplayStoreError,
    > = lash::observe::PreparedLiveReplayPublication::new;
    // W0020: lash::observe::LiveReplayStore::trim_session [function]
    fn meth_0020<T: lash::observe::LiveReplayStore>(_: &T) {
        let _ = T::trim_session;
    }
    // W0021: lash::observe::LiveReplayStoreError::Store [variant]
    variant_witness(|value: &lash::observe::LiveReplayStoreError| {
        matches!(value, lash::observe::LiveReplayStoreError::Store(..))
    });
    // W0022: lash::observe::LiveReplayStoreError::Store::0 [field]
    field_witness(|value: &lash::observe::LiveReplayStoreError| {
        if let lash::observe::LiveReplayStoreError::Store(f0) = value {
            let _ = f0;
        }
    });
    // W0023: lash::observe::LiveReplayStoreError::SubscriberLagged [variant]
    variant_witness(|value: &lash::observe::LiveReplayStoreError| {
        matches!(
            value,
            lash::observe::LiveReplayStoreError::SubscriberLagged(..)
        )
    });
    // W0024: lash::observe::LiveReplayStoreError::SubscriberLagged::0 [field]
    field_witness(|value: &lash::observe::LiveReplayStoreError| {
        if let lash::observe::LiveReplayStoreError::SubscriberLagged(f0) = value {
            let _ = f0;
        }
    });
    // W0025: lash::observe::SessionCursor::from_store_token [function]
    let _: fn(
        String,
    ) -> Result<lash::observe::SessionCursor, lash::persistence::SessionCursorError> =
        lash::observe::SessionCursor::from_store_token;
    // W0026: lash::observe::SessionObservationEvent::new [function]
    let _ = lash::observe::SessionObservationEvent::new;
    // W0027: lash::observe::SessionQueueEventKind [enum]
    type_witness::<lash::observe::SessionQueueEventKind>();
    // W0028: lash::observe::SessionRevision [struct]
    type_witness::<lash::observe::SessionRevision>();
    // W0030: lash::plugins::SessionGraphService::emit_trace_event [function]
    fn meth_0030<T: lash::plugins::SessionGraphService>(_: &T) {
        let _ = T::emit_trace_event;
    }
    // W0031: lash::remote::observations::RemoteSessionObservationEvent::decode_json [function]
    let _ = lash::remote::observations::RemoteSessionObservationEvent::decode_json;
    // W0032: lash::remote::usage::RemoteTurnEvent::TurnStarted [variant]
    variant_witness(|value: &lash::remote::usage::RemoteTurnEvent| {
        matches!(
            value,
            lash::remote::usage::RemoteTurnEvent::TurnStarted { .. }
        )
    });
    // W0033: lash::remote::usage::RemoteTurnEvent::TurnStarted::turn_id [field]
    field_witness(|value: &lash::remote::usage::RemoteTurnEvent| {
        if let lash::remote::usage::RemoteTurnEvent::TurnStarted { turn_id, .. } = value {
            let _ = turn_id;
        }
    });
    // W0035: lash::runtime::AssembledTurn::token_usage [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.token_usage;
    });
    // W0036: lash::runtime::ExecutionScope::validates_turn_trace_id [function]
    let _ = lash::runtime::ExecutionScope::validates_turn_trace_id;
    // W0037: lash::runtime::RuntimeEffectLocalExecutor::replay_validation_trace [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::replay_validation_trace;
    // W0038: lash::runtime::RuntimeHandle::publish_resident_from [function]
    let _ = lash::runtime::RuntimeHandle::publish_resident_from;
    // W0039: lash::runtime::SessionSnapshot::token_usage [field]
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.token_usage;
    });
    // W0040-W0046: otel-trace-gated rows witnessed in otel_trace_evidence.rs.
    // W0047: lash::tracing::TraceContentBlock [enum]
    type_witness::<lash::tracing::TraceContentBlock>();
    // W0048: lash::tracing::TraceContentBlock::Reasoning [variant]
    variant_witness(|value: &lash::tracing::TraceContentBlock| {
        matches!(value, lash::tracing::TraceContentBlock::Reasoning { .. })
    });
    // W0049: lash::tracing::TraceContentBlock::Reasoning::has_encrypted [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Reasoning { has_encrypted, .. } = value {
            let _ = has_encrypted;
        }
    });
    // W0050: lash::tracing::TraceContentBlock::Reasoning::item_id [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Reasoning { item_id, .. } = value {
            let _ = item_id;
        }
    });
    // W0051: lash::tracing::TraceContentBlock::Reasoning::redacted [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Reasoning { redacted, .. } = value {
            let _ = redacted;
        }
    });
    // W0052: lash::tracing::TraceContentBlock::Reasoning::summary [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Reasoning { summary, .. } = value {
            let _ = summary;
        }
    });
    // W0053: lash::tracing::TraceContentBlock::Reasoning::text [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Reasoning { text, .. } = value {
            let _ = text;
        }
    });
    // W0054: lash::tracing::TraceContentBlock::Text [variant]
    variant_witness(|value: &lash::tracing::TraceContentBlock| {
        matches!(value, lash::tracing::TraceContentBlock::Text { .. })
    });
    // W0055: lash::tracing::TraceContentBlock::Text::cache_breakpoint [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Text {
            cache_breakpoint, ..
        } = value
        {
            let _ = cache_breakpoint;
        }
    });
    // W0056: lash::tracing::TraceContentBlock::Text::text [field]
    field_witness(|value: &lash::tracing::TraceContentBlock| {
        if let lash::tracing::TraceContentBlock::Text { text, .. } = value {
            let _ = text;
        }
    });
    // W0057: lash::tracing::TraceContext::candidate_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.candidate_id;
    });
    // W0058: lash::tracing::TraceContext::candidate_parent_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.candidate_parent_id;
    });
    // W0059: lash::tracing::TraceContext::effect_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.effect_id;
    });
    // W0060: lash::tracing::TraceContext::example_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.example_id;
    });
    // W0061: lash::tracing::TraceContext::experiment_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.experiment_id;
    });
    // W0062: lash::tracing::TraceContext::for_llm_call [function]
    let _: fn(lash::tracing::TraceContext, String) -> lash::tracing::TraceContext =
        lash::tracing::TraceContext::for_llm_call;
    // W0063: lash::tracing::TraceContext::for_protocol_iteration [function]
    let _ = lash::tracing::TraceContext::for_protocol_iteration;
    // W0064: lash::tracing::TraceContext::for_turn [function]
    let _: fn(lash::tracing::TraceContext, lash::TurnId) -> lash::tracing::TraceContext =
        lash::tracing::TraceContext::for_turn;
    // W0065: lash::tracing::TraceContext::for_turn_index [function]
    let _ = lash::tracing::TraceContext::for_turn_index;
    // W0066: lash::tracing::TraceContext::graph_node_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.graph_node_id;
    });
    // W0067: lash::tracing::TraceContext::llm_call_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.llm_call_id;
    });
    // W0068: lash::tracing::TraceContext::metadata [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.metadata;
    });
    // W0069: lash::tracing::TraceContext::parent_graph_node_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.parent_graph_node_id;
    });
    // W0070: lash::tracing::TraceContext::protocol_iteration [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.protocol_iteration;
    });
    // W0071: lash::tracing::TraceContext::run_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.run_id;
    });
    // W0072: lash::tracing::TraceContext::session_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.session_id;
    });
    // W0073: lash::tracing::TraceContext::split [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.split;
    });
    // W0074: lash::tracing::TraceContext::turn_id [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.turn_id;
    });
    // W0075: lash::tracing::TraceContext::turn_index [field]
    field_witness(|value: &lash::tracing::TraceContext| {
        let _ = &value.turn_index;
    });
    // W0076: lash::tracing::TraceEffectEnvelopeDiffEntry [struct]
    type_witness::<lash::tracing::TraceEffectEnvelopeDiffEntry>();
    // W0077: lash::tracing::TraceEffectEnvelopeDiffEntry::path [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEntry| {
        let _ = &value.path;
    });
    // W0078: lash::tracing::TraceEffectEnvelopeDiffEntry::reconstructed [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEntry| {
        let _ = &value.reconstructed;
    });
    // W0079: lash::tracing::TraceEffectEnvelopeDiffEntry::recorded [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEntry| {
        let _ = &value.recorded;
    });
    // W0080: lash::tracing::TraceEffectEnvelopeDiffEvent [struct]
    type_witness::<lash::tracing::TraceEffectEnvelopeDiffEvent>();
    // W0081: lash::tracing::TraceEffectEnvelopeDiffEvent::divergent_paths [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEvent| {
        let _ = &value.divergent_paths;
    });
    // W0082: lash::tracing::TraceEffectEnvelopeDiffEvent::reconstructed_envelope_hash [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEvent| {
        let _ = &value.reconstructed_envelope_hash;
    });
    // W0083: lash::tracing::TraceEffectEnvelopeDiffEvent::recorded_envelope_hash [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffEvent| {
        let _ = &value.recorded_envelope_hash;
    });
    // W0084: lash::tracing::TraceEffectEnvelopeDiffValue [enum]
    type_witness::<lash::tracing::TraceEffectEnvelopeDiffValue>();
    // W0085: lash::tracing::TraceEffectEnvelopeDiffValue::Missing [variant]
    variant_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        matches!(value, lash::tracing::TraceEffectEnvelopeDiffValue::Missing)
    });
    // W0086: lash::tracing::TraceEffectEnvelopeDiffValue::Present [variant]
    variant_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        matches!(
            value,
            lash::tracing::TraceEffectEnvelopeDiffValue::Present { .. }
        )
    });
    // W0087: lash::tracing::TraceEffectEnvelopeDiffValue::Present::json_len [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        if let lash::tracing::TraceEffectEnvelopeDiffValue::Present { json_len, .. } = value {
            let _ = json_len;
        }
    });
    // W0088: lash::tracing::TraceEffectEnvelopeDiffValue::Present::json_sha256 [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        if let lash::tracing::TraceEffectEnvelopeDiffValue::Present { json_sha256, .. } = value {
            let _ = json_sha256;
        }
    });
    // W0089: lash::tracing::TraceEffectEnvelopeDiffValue::Present::value_json [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        if let lash::tracing::TraceEffectEnvelopeDiffValue::Present { value_json, .. } = value {
            let _ = value_json;
        }
    });
    // W0090: lash::tracing::TraceEffectEnvelopeDiffValue::Present::value_json_omitted_reason [field]
    field_witness(|value: &lash::tracing::TraceEffectEnvelopeDiffValue| {
        if let lash::tracing::TraceEffectEnvelopeDiffValue::Present {
            value_json_omitted_reason,
            ..
        } = value
        {
            let _ = value_json_omitted_reason;
        }
    });
    // W0091: lash::tracing::TraceError [struct]
    type_witness::<lash::tracing::TraceError>();
    // W0092: lash::tracing::TraceError::code [field]
    field_witness(|value: &lash::tracing::TraceError| {
        let _ = &value.code;
    });
    // W0093: lash::tracing::TraceError::message [field]
    field_witness(|value: &lash::tracing::TraceError| {
        let _ = &value.message;
    });
    // W0094: lash::tracing::TraceError::raw [field]
    field_witness(|value: &lash::tracing::TraceError| {
        let _ = &value.raw;
    });
    // W0095: lash::tracing::TraceError::retryable [field]
    field_witness(|value: &lash::tracing::TraceError| {
        let _ = &value.retryable;
    });
    // W0096: lash::tracing::TraceError::terminal_reason [field]
    field_witness(|value: &lash::tracing::TraceError| {
        let _ = &value.terminal_reason;
    });
    // W0097: lash::tracing::TraceEvent::EffectEnvelopeDiff [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::EffectEnvelopeDiff { .. })
    });
    // W0098: lash::tracing::TraceEvent::EffectEnvelopeDiff::event [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::EffectEnvelopeDiff { event, .. } = value {
            let _ = event;
        }
    });
    // W0099: lash::tracing::TraceEvent::ExecCodeCompleted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::ExecCodeCompleted { .. })
    });
    // W0100: lash::tracing::TraceEvent::ExecCodeCompleted::duration_ms [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted { duration_ms, .. } = value {
            let _ = duration_ms;
        }
    });
    // W0101: lash::tracing::TraceEvent::ExecCodeCompleted::error [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted { error, .. } = value {
            let _ = error;
        }
    });
    // W0102: lash::tracing::TraceEvent::ExecCodeCompleted::observation_count [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted {
            observation_count, ..
        } = value
        {
            let _ = observation_count;
        }
    });
    // W0103: lash::tracing::TraceEvent::ExecCodeCompleted::observation_projections [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted {
            observation_projections,
            ..
        } = value
        {
            let _ = observation_projections;
        }
    });
    // W0104: lash::tracing::TraceEvent::ExecCodeCompleted::output [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted { output, .. } = value {
            let _ = output;
        }
    });
    // W0105: lash::tracing::TraceEvent::ExecCodeCompleted::output_chars [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted { output_chars, .. } = value {
            let _ = output_chars;
        }
    });
    // W0106: lash::tracing::TraceEvent::ExecCodeCompleted::terminal_finish [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted {
            terminal_finish, ..
        } = value
        {
            let _ = terminal_finish;
        }
    });
    // W0107: lash::tracing::TraceEvent::ExecCodeCompleted::tool_calls [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeCompleted { tool_calls, .. } = value {
            let _ = tool_calls;
        }
    });
    // W0108: lash::tracing::TraceEvent::ExecCodeFailed [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::ExecCodeFailed { .. })
    });
    // W0109: lash::tracing::TraceEvent::ExecCodeFailed::error [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeFailed { error, .. } = value {
            let _ = error;
        }
    });
    // W0110: lash::tracing::TraceEvent::ExecCodeStarted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::ExecCodeStarted { .. })
    });
    // W0111: lash::tracing::TraceEvent::ExecCodeStarted::code [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeStarted { code, .. } = value {
            let _ = code;
        }
    });
    // W0112: lash::tracing::TraceEvent::ExecCodeStarted::code_chars [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ExecCodeStarted { code_chars, .. } = value {
            let _ = code_chars;
        }
    });
    // W0113: lash::tracing::TraceEvent::ObservationProjection [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(
            value,
            lash::tracing::TraceEvent::ObservationProjection { .. }
        )
    });
    // W0114: lash::tracing::TraceEvent::ObservationProjection::projections [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ObservationProjection { projections, .. } = value {
            let _ = projections;
        }
    });
    // W0115: lash::tracing::TraceEvent::LlmCallCompleted::response [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallCompleted { response, .. } = value {
            let _ = response;
        }
    });
    // W0116: lash::tracing::TraceEvent::LlmCallCompleted::stream_summary [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallCompleted { stream_summary, .. } = value {
            let _ = stream_summary;
        }
    });
    // W0117: lash::tracing::TraceEvent::LlmCallFailed [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::LlmCallFailed { .. })
    });
    // W0118: lash::tracing::TraceEvent::LlmCallFailed::error [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallFailed { error, .. } = value {
            let _ = error;
        }
    });
    // W0119: lash::tracing::TraceEvent::LlmCallFailed::stream_summary [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallFailed { stream_summary, .. } = value {
            let _ = stream_summary;
        }
    });
    // W0120: lash::tracing::TraceEvent::LlmCallStarted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::LlmCallStarted { .. })
    });
    // W0121: lash::tracing::TraceEvent::LlmCallStarted::request [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallStarted { request, .. } = value {
            let _ = request;
        }
    });
    // W0122: lash::tracing::TraceEvent::ProtocolStep [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::ProtocolStep { .. })
    });
    // W0123: lash::tracing::TraceEvent::ProtocolStep::payload [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ProtocolStep { payload, .. } = value {
            let _ = payload;
        }
    });
    // W0124: lash::tracing::TraceEvent::RuntimeStreamEvent [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::RuntimeStreamEvent { .. })
    });
    // W0125: lash::tracing::TraceEvent::RuntimeStreamEvent::event [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::RuntimeStreamEvent { event, .. } = value {
            let _ = event;
        }
    });
    // W0131: lash::tracing::TraceEvent::TurnCompleted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::TurnCompleted { .. })
    });
    // W0132: lash::tracing::TraceEvent::TurnCompleted::outcome [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::TurnCompleted { outcome, .. } = value {
            let _ = outcome;
        }
    });
    // W0133: lash::tracing::TraceEvent::TurnStarted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::TurnStarted { .. })
    });
    // W0134: lash::tracing::TraceEvent::TurnStarted::metadata [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::TurnStarted { metadata, .. } = value {
            let _ = metadata;
        }
    });
    // W0135: lash::tracing::TraceEvent::kind [function]
    let _ = lash::tracing::TraceEvent::kind;
    // W0136: lash::tracing::TraceEvent::is_failed [function]
    let _ = lash::tracing::TraceEvent::is_failed;
    // W0137: lash::tracing::TraceLlmMessage [struct]
    type_witness::<lash::tracing::TraceLlmMessage>();
    // W0138: lash::tracing::TraceLlmMessage::blocks [field]
    field_witness(|value: &lash::tracing::TraceLlmMessage| {
        let _ = &value.blocks;
    });
    // W0139: lash::tracing::TraceLlmMessage::role [field]
    field_witness(|value: &lash::tracing::TraceLlmMessage| {
        let _ = &value.role;
    });
    // W0140: lash::tracing::TraceLlmRequest [struct]
    type_witness::<lash::tracing::TraceLlmRequest>();
    // W0141: lash::tracing::TraceLlmRequest::messages [field]
    field_witness(|value: &lash::tracing::TraceLlmRequest| {
        let _ = &value.messages;
    });
    // W0142: lash::tracing::TraceLlmRequest::model [field]
    field_witness(|value: &lash::tracing::TraceLlmRequest| {
        let _ = &value.model;
    });
    // W0143: lash::tracing::TraceLlmRequest::model_variant [field]
    field_witness(|value: &lash::tracing::TraceLlmRequest| {
        let _ = &value.model_variant;
    });
    // W0144: lash::tracing::TraceLlmRequest::output_spec [field]
    field_witness(|value: &lash::tracing::TraceLlmRequest| {
        let _ = &value.output_spec;
    });
    // W0145: lash::tracing::TraceLlmRequest::stream [field]
    field_witness(|value: &lash::tracing::TraceLlmRequest| {
        let _ = &value.stream;
    });
    // W0146: lash::tracing::TraceLlmResponse [struct]
    type_witness::<lash::tracing::TraceLlmResponse>();
    // W0147: lash::tracing::TraceLlmResponse::duration_ms [field]
    field_witness(|value: &lash::tracing::TraceLlmResponse| {
        let _ = &value.duration_ms;
    });
    // W0148: lash::tracing::TraceLlmResponse::generation_disposition [field]
    field_witness(|value: &lash::tracing::TraceLlmResponse| {
        let _ = &value.generation_disposition;
    });
    // W0149: lash::tracing::TraceLlmResponse::parts [field]
    field_witness(|value: &lash::tracing::TraceLlmResponse| {
        let _ = &value.parts;
    });
    // W0150: lash::tracing::TraceLlmResponse::terminal_reason [field]
    field_witness(|value: &lash::tracing::TraceLlmResponse| {
        let _ = &value.terminal_reason;
    });
    // W0151: lash::tracing::TraceLlmResponse::text [field]
    field_witness(|value: &lash::tracing::TraceLlmResponse| {
        let _ = &value.text;
    });
    // W0152: lash::tracing::TraceRuntimeStreamEvent [struct]
    type_witness::<lash::tracing::TraceRuntimeStreamEvent>();
    // W0153: lash::tracing::TraceRuntimeStreamEvent::call_id [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.call_id;
    });
    // W0154: lash::tracing::TraceRuntimeStreamEvent::elapsed_ms [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.elapsed_ms;
    });
    // W0155: lash::tracing::TraceRuntimeStreamEvent::event_name [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.event_name;
    });
    // W0156: lash::tracing::TraceRuntimeStreamEvent::input_json [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.input_json;
    });
    // W0157: lash::tracing::TraceRuntimeStreamEvent::item_id [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.item_id;
    });
    // W0158: lash::tracing::TraceRuntimeStreamEvent::output_index [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.output_index;
    });
    // W0159: lash::tracing::TraceRuntimeStreamEvent::raw_text [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.raw_text;
    });
    // W0160: lash::tracing::TraceRuntimeStreamEvent::sequence [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.sequence;
    });
    // W0161: lash::tracing::TraceRuntimeStreamEvent::usage [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.usage;
    });
    // W0162: lash::tracing::TraceRuntimeStreamEvent::visible_text [field]
    field_witness(|value: &lash::tracing::TraceRuntimeStreamEvent| {
        let _ = &value.visible_text;
    });
    // W0163: lash::tracing::TraceTokenUsage [struct]
    type_witness::<lash::tracing::TraceTokenUsage>();
    // W0164: lash::tracing::TraceTokenUsage::cache_read_input_tokens [field]
    field_witness(|value: &lash::tracing::TraceTokenUsage| {
        let _ = &value.cache_read_input_tokens;
    });
    // W0165: lash::tracing::TraceTokenUsage::cache_write_input_tokens [field]
    field_witness(|value: &lash::tracing::TraceTokenUsage| {
        let _ = &value.cache_write_input_tokens;
    });
    // W0166: lash::tracing::TraceTokenUsage::input_tokens [field]
    field_witness(|value: &lash::tracing::TraceTokenUsage| {
        let _ = &value.input_tokens;
    });
    // W0167: lash::tracing::TraceTokenUsage::output_tokens [field]
    field_witness(|value: &lash::tracing::TraceTokenUsage| {
        let _ = &value.output_tokens;
    });
    // W0168: lash::tracing::TraceTokenUsage::reasoning_output_tokens [field]
    field_witness(|value: &lash::tracing::TraceTokenUsage| {
        let _ = &value.reasoning_output_tokens;
    });
    // W0169: lash::usage::SessionUsageReport [struct]
    type_witness::<lash::usage::SessionUsageReport>();
    // W0170: lash::usage::SessionUsageReport::by_model [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.by_model;
    });
    // W0171: lash::usage::SessionUsageReport::by_source [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.by_source;
    });
    // W0172: lash::usage::SessionUsageReport::by_source_model [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.by_source_model;
    });
    // W0173: lash::usage::SessionUsageReport::entry_count [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.entry_count;
    });
    // W0174: lash::usage::SessionUsageReport::from_entries [function]
    let _ = lash::usage::SessionUsageReport::from_entries;
    // W0175: lash::usage::SessionUsageReport::saturated [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.saturated;
    });
    // W0176: lash::usage::SessionUsageReport::usage [field]
    field_witness(|value: &lash::usage::SessionUsageReport| {
        let _ = &value.usage;
    });
    // W0177: lash::usage::TokenLedgerEntry [struct]
    type_witness::<lash::usage::TokenLedgerEntry>();
    // W0178: lash::usage::TokenLedgerEntry::model [field]
    field_witness(|value: &lash::usage::TokenLedgerEntry| {
        let _ = &value.model;
    });
    // W0179: lash::usage::TokenUsage::checked_add [function]
    let _ = lash::usage::TokenUsage::checked_add;
    // W0180: lash::usage::TokenUsage::checked_input_total [function]
    let _ = lash::usage::TokenUsage::checked_input_total;
    // W0181: lash::usage::TokenUsage::checked_total [function]
    let _ = lash::usage::TokenUsage::checked_total;
    // W0182: lash::usage::TokenUsage::is_zero [function]
    let _ = lash::usage::TokenUsage::is_zero;
    // W0183: lash::usage::TokenUsageOverflow [struct]
    type_witness::<lash::usage::TokenUsageOverflow>();
    // W0184: lash::usage::TokenUsageOverflow::counter [function]
    let _ = lash::usage::TokenUsageOverflow::counter;
    // W0185: lash::usage::UsageReportRow [struct]
    type_witness::<lash::usage::UsageReportRow>();
    // W0186: lash::usage::UsageReportRow::model [field]
    field_witness(|value: &lash::usage::UsageReportRow| {
        let _ = &value.model;
    });
    // W0187: lash::usage::UsageReportRow::source [field]
    field_witness(|value: &lash::usage::UsageReportRow| {
        let _ = &value.source;
    });
    // W0188: lash::usage::UsageReportRow::usage [field]
    field_witness(|value: &lash::usage::UsageReportRow| {
        let _ = &value.usage;
    });
    // W0189: lash::usage::UsageTotals [struct]
    type_witness::<lash::usage::UsageTotals>();
    // W0190: lash::usage::UsageTotals::total_tokens [field]
    field_witness(|value: &lash::usage::UsageTotals| {
        let _ = &value.total_tokens;
    });
    // W0191: lash::usage::UsageTotals::usage [field]
    field_witness(|value: &lash::usage::UsageTotals| {
        let _ = &value.usage;
    });
    // W0198: lash::persistence::SessionCursorError [enum]
    type_witness::<lash::persistence::SessionCursorError>();
    // W0199: lash::persistence::SessionCursorError::Malformed [variant]
    variant_witness(|value: &lash::persistence::SessionCursorError| {
        matches!(
            value,
            lash::persistence::SessionCursorError::Malformed { .. }
        )
    });
    // W0200: lash::persistence::SessionCursorError::Malformed::message [field]
    field_witness(|value: &lash::persistence::SessionCursorError| {
        if let lash::persistence::SessionCursorError::Malformed { message, .. } = value {
            let _ = message;
        }
    });
    // W0201: lash::persistence::SessionCursorError::WrongSession [variant]
    variant_witness(|value: &lash::persistence::SessionCursorError| {
        matches!(
            value,
            lash::persistence::SessionCursorError::WrongSession { .. }
        )
    });
    // W0202: lash::persistence::SessionCursorError::WrongSession::actual_session_id [field]
    field_witness(|value: &lash::persistence::SessionCursorError| {
        if let lash::persistence::SessionCursorError::WrongSession {
            actual_session_id, ..
        } = value
        {
            let _ = actual_session_id;
        }
    });
    // W0203: lash::persistence::SessionCursorError::WrongSession::expected_session_id [field]
    field_witness(|value: &lash::persistence::SessionCursorError| {
        if let lash::persistence::SessionCursorError::WrongSession {
            expected_session_id,
            ..
        } = value
        {
            let _ = expected_session_id;
        }
    });
    // W0204: lash::tracing::TraceEvent::DurableSegmentBoundary [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(
            value,
            lash::tracing::TraceEvent::DurableSegmentBoundary { .. }
        )
    });
    // W0205: lash::tracing::TraceEvent::DurableSegmentBoundary::effects_executed [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableSegmentBoundary {
            effects_executed, ..
        } = value
        {
            let _ = effects_executed;
        }
    });
    // W0206: lash::tracing::TraceEvent::DurableSegmentBoundary::journaled_bytes_estimate [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableSegmentBoundary {
            journaled_bytes_estimate,
            ..
        } = value
        {
            let _ = journaled_bytes_estimate;
        }
    });
    // W0207: lash::tracing::TraceEvent::DurableSegmentBoundary::reason [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableSegmentBoundary { reason, .. } = value {
            let _ = reason;
        }
    });
    // W0208: lash::tracing::TraceEvent::DurableTimerResolved [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(
            value,
            lash::tracing::TraceEvent::DurableTimerResolved { .. }
        )
    });
    // W0209: lash::tracing::TraceEvent::DurableTimerResolved::duration_ms [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableTimerResolved { duration_ms, .. } = value {
            let _ = duration_ms;
        }
    });
    // W0210: lash::tracing::TraceEvent::DurableTimerResolved::status [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableTimerResolved { status, .. } = value {
            let _ = status;
        }
    });
    // W0211: lash::tracing::TraceEvent::DurableTimerStarted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::DurableTimerStarted { .. })
    });
    // W0212: lash::tracing::TraceEvent::DurableTimerStarted::duration_ms [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableTimerStarted { duration_ms, .. } = value {
            let _ = duration_ms;
        }
    });
    // W0213: lash::tracing::TraceEvent::DurableWaitParked [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::DurableWaitParked { .. })
    });
    // W0214: lash::tracing::TraceEvent::DurableWaitParked::wait_kind [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableWaitParked { wait_kind, .. } = value {
            let _ = wait_kind;
        }
    });
    // W0215: lash::tracing::TraceEvent::DurableWaitResolved [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::DurableWaitResolved { .. })
    });
    // W0216: lash::tracing::TraceEvent::DurableWaitResolved::resolution [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableWaitResolved { resolution, .. } = value {
            let _ = resolution;
        }
    });
    // W0217: lash::tracing::TraceEvent::DurableWaitResolved::wait_kind [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::DurableWaitResolved { wait_kind, .. } = value {
            let _ = wait_kind;
        }
    });
    // W0218: lash::tracing::TraceEvent::JournaledEffectSettled [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(
            value,
            lash::tracing::TraceEvent::JournaledEffectSettled { .. }
        )
    });
    // W0219: lash::tracing::TraceEvent::JournaledEffectSettled::effect_kind [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::JournaledEffectSettled { effect_kind, .. } = value {
            let _ = effect_kind;
        }
    });
    // W0220: lash::tracing::TraceEvent::JournaledEffectSettled::effect_name [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::JournaledEffectSettled { effect_name, .. } = value {
            let _ = effect_name;
        }
    });
    // W0221: lash::tracing::TraceEvent::JournaledEffectSettled::status [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::JournaledEffectSettled { status, .. } = value {
            let _ = status;
        }
    });
    // W0222: lash::tracing::TraceEvent::JournaledEffectStarted [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(
            value,
            lash::tracing::TraceEvent::JournaledEffectStarted { .. }
        )
    });
    // W0223: lash::tracing::TraceEvent::JournaledEffectStarted::effect_kind [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::JournaledEffectStarted { effect_kind, .. } = value {
            let _ = effect_kind;
        }
    });
    // W0224: lash::tracing::TraceEvent::JournaledEffectStarted::effect_name [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::JournaledEffectStarted { effect_name, .. } = value {
            let _ = effect_name;
        }
    });
    // W0225: lash::tracing::TraceEvent::StoreErrorObserved [variant]
    variant_witness(|value: &lash::tracing::TraceEvent| {
        matches!(value, lash::tracing::TraceEvent::StoreErrorObserved { .. })
    });
    // W0226: lash::tracing::TraceEvent::StoreErrorObserved::error_class [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::StoreErrorObserved { error_class, .. } = value {
            let _ = error_class;
        }
    });
    // W0227: lash::tracing::TraceEvent::StoreErrorObserved::message [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::StoreErrorObserved { message, .. } = value {
            let _ = message;
        }
    });
    // W0228: lash::tracing::TraceEvent::StoreErrorObserved::operation [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::StoreErrorObserved { operation, .. } = value {
            let _ = operation;
        }
    });
    // W0229: lash::tracing::TraceEvent::LlmCallCompleted::attempts [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallCompleted { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // W0230: lash::tracing::TraceEvent::LlmCallFailed::attempts [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::LlmCallFailed { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // W0231: lash::tracing::TraceEvent::ToolCallCompleted::attempts [field]
    field_witness(|value: &lash::tracing::TraceEvent| {
        if let lash::tracing::TraceEvent::ToolCallCompleted { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // W0232: lash::TurnEvent::ToolIntentOutcome [variant]
    variant_witness(|value: &lash::TurnEvent| {
        matches!(value, lash::TurnEvent::ToolIntentOutcome { .. })
    });
    // W0233: lash::TurnEvent::ToolIntentOutcome::call_id [field]
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolIntentOutcome { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0234: lash::TurnEvent::ToolIntentOutcome::outcome [field]
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolIntentOutcome { outcome, .. } = value {
            let _ = outcome;
        }
    });
    // W0235: lash::LlmCallRecord::replay_drops [field]
    field_witness(|value: &lash::LlmCallRecord| {
        let _ = &value.replay_drops;
    });
}
