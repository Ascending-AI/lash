//! Compile-time witnesses for protocol-area facade and integrator contracts.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices: at the
//! dispatch-time recount this area held 412 rows. The 406 rows whose item
//! still exists are type-checked here through the path a host or integrator
//! would name — `lash::` for facade surface, `lash_core::` for internal seams
//! the integrator classes consume directly. The 6 rows whose item no longer
//! exists anywhere in this workspace are listed in the pull request rather
//! than witnessed here.

#![cfg(feature = "testing")]
#![allow(deprecated)]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0001: lash::FrameKey [struct]
    type_witness::<lash::FrameKey>();
    // W0002: lash::FrameKey::as_str [function]
    let _ = lash::FrameKey::as_str;
    // W0003: lash::FrameKey::from_call_site [function]
    let _ = lash::FrameKey::from_call_site;
    // W0004: lash::FrameKey::from_caller_material [function]
    let _ = lash::FrameKey::from_caller_material;
    // W0005: lash_core::FrameKeyError [enum]
    type_witness::<lash_core::FrameKeyError>();
    // W0006: lash_core::FrameKeyError::EmptyCallerMaterial [variant]
    variant_witness(|value: &lash_core::FrameKeyError| {
        matches!(value, lash_core::FrameKeyError::EmptyCallerMaterial)
    });
    // W0007: lash::LlmCallRecord [struct]
    type_witness::<lash::LlmCallRecord>();
    // W0008: lash::LlmCallRecord::attempts [field]
    field_witness(|value: &lash::LlmCallRecord| {
        let _ = &value.attempts;
    });
    // W0009: lash::LlmCallRecord::call_id [field]
    field_witness(|value: &lash::LlmCallRecord| {
        let _ = &value.call_id;
    });
    // W0010: lash::LlmCallRecord::label [field]
    field_witness(|value: &lash::LlmCallRecord| {
        let _ = &value.label;
    });
    // W0011: lash::ModelLimits [struct]
    type_witness::<lash::ModelLimits>();
    // W0012: lash::ModelLimits::context_window_tokens [field]
    field_witness(|value: &lash::ModelLimits| {
        let _ = &value.context_window_tokens;
    });
    // W0013: lash::ModelLimits::from_token_limits [function]
    let _: fn(usize, Option<usize>) -> Result<lash::ModelLimits, lash::ModelLimitsError> =
        lash::ModelLimits::from_token_limits;
    // W0014: lash::ModelLimits::output_token_capacity [field]
    field_witness(|value: &lash::ModelLimits| {
        let _ = &value.output_token_capacity;
    });
    // W0015: lash::ModelLimitsError [enum]
    type_witness::<lash::ModelLimitsError>();
    // W0016: lash::ModelLimitsError::MissingContextWindowTokens [variant]
    variant_witness(|value: &lash::ModelLimitsError| {
        matches!(value, lash::ModelLimitsError::MissingContextWindowTokens)
    });
    // W0017: lash::ModelLimitsError::ZeroContextWindowTokens [variant]
    variant_witness(|value: &lash::ModelLimitsError| {
        matches!(value, lash::ModelLimitsError::ZeroContextWindowTokens)
    });
    // W0018: lash::ModelLimitsError::ZeroOutputTokenCapacity [variant]
    variant_witness(|value: &lash::ModelLimitsError| {
        matches!(value, lash::ModelLimitsError::ZeroOutputTokenCapacity)
    });
    // W0019: lash::ModelSpec::capability [field]
    field_witness(|value: &lash::ModelSpec| {
        let _ = &value.capability;
    });
    // W0020: lash::ModelSpec::context_window_tokens [function]
    let _ = lash::ModelSpec::context_window_tokens;
    // W0021: lash::ModelSpec::from_token_limits [function]
    let _: fn(
        String,
        lash::provider::ReasoningSelection,
        usize,
        Option<usize>,
    ) -> Result<lash::ModelSpec, lash::ModelLimitsError> = lash::ModelSpec::from_token_limits;
    // W0022: lash::ModelSpec::id [field]
    field_witness(|value: &lash::ModelSpec| {
        let _ = &value.id;
    });
    // W0023: lash::ModelSpec::limits [field]
    field_witness(|value: &lash::ModelSpec| {
        let _ = &value.limits;
    });
    // W0024: lash::ModelSpec::new [function]
    let _: fn(String, std::num::NonZeroUsize) -> lash::ModelSpec = lash::ModelSpec::new;
    // W0025: lash::ModelSpec::variant [field]
    field_witness(|value: &lash::ModelSpec| {
        let _ = &value.variant;
    });
    // W0026: lash::ModelSpec::with_limits [function]
    let _: fn(String, lash::provider::ReasoningSelection, lash::ModelLimits) -> lash::ModelSpec =
        lash::ModelSpec::with_limits;
    // W0027: lash::ModelSpec::with_variant [function]
    let _ = lash::ModelSpec::with_variant;
    // W0028: lash::ModelSpecBuilder::output_token_capacity [function]
    let _ = lash::ModelSpecBuilder::output_token_capacity;
    // W0029: lash::ModelSpecBuilder::variant [function]
    let _ = lash::ModelSpecBuilder::variant;
    // W0030: lash::SessionError::Protocol [variant]
    variant_witness(|value: &lash::SessionError| matches!(value, lash::SessionError::Protocol(..)));
    // W0031: lash::SessionError::Protocol::0 [field]
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::Protocol(f0) = value {
            let _ = f0;
        }
    });
    // W0032: lash::TurnEvent::ModelCallRecorded [variant]
    variant_witness(|value: &lash::TurnEvent| {
        matches!(value, lash::TurnEvent::ModelCallRecorded { .. })
    });
    // W0033: lash::TurnEvent::ModelCallRecorded::record [field]
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ModelCallRecorded { record, .. } = value {
            let _ = record;
        }
    });
    // W0034: lash::TurnEvent::ModelRequestStarted [variant]
    variant_witness(|value: &lash::TurnEvent| {
        matches!(value, lash::TurnEvent::ModelRequestStarted { .. })
    });
    // W0035: lash::TurnEvent::ModelRequestStarted::protocol_iteration [field]
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ModelRequestStarted {
            protocol_iteration, ..
        } = value
        {
            let _ = protocol_iteration;
        }
    });
    // W0036: lash::TurnInput::protocol_extension [field]
    field_witness(|value: &lash::TurnInput| {
        let _ = &value.protocol_extension;
    });
    // W0037: lash::TurnInput::protocol_turn_options [field]
    field_witness(|value: &lash::TurnInput| {
        let _ = &value.protocol_turn_options;
    });
    // W0038: lash::TurnInput::with_protocol_turn_options [function]
    let _ = lash::TurnInput::with_protocol_turn_options;
    // W0039: lash::direct::DirectCompletion [struct]
    type_witness::<lash::direct::DirectCompletion>();
    // W0040: lash::direct::DirectCompletion::llm_call [field]
    field_witness(|value: &lash::direct::DirectCompletion| {
        let _ = &value.llm_call;
    });
    // W0041: lash::direct::DirectCompletion::text [field]
    field_witness(|value: &lash::direct::DirectCompletion| {
        let _ = &value.text;
    });
    // W0042: lash::direct::DirectJsonSchema [struct]
    type_witness::<lash::direct::DirectJsonSchema>();
    // W0043: lash::direct::DirectJsonSchema::name [field]
    field_witness(|value: &lash::direct::DirectJsonSchema| {
        let _ = &value.name;
    });
    // W0044: lash::direct::DirectJsonSchema::schema [field]
    field_witness(|value: &lash::direct::DirectJsonSchema| {
        let _ = &value.schema;
    });
    // W0045: lash::direct::DirectJsonSchema::strict [field]
    field_witness(|value: &lash::direct::DirectJsonSchema| {
        let _ = &value.strict;
    });
    // W0046: lash::direct::DirectLlmCompletion [struct]
    type_witness::<lash::direct::DirectLlmCompletion>();
    // W0047: lash::direct::DirectLlmCompletion::llm_call [field]
    field_witness(|value: &lash::direct::DirectLlmCompletion| {
        let _ = &value.llm_call;
    });
    // W0048: lash::direct::DirectLlmCompletion::response [field]
    field_witness(|value: &lash::direct::DirectLlmCompletion| {
        let _ = &value.response;
    });
    // W0049: lash::direct::DirectMessage [struct]
    type_witness::<lash::direct::DirectMessage>();
    // W0050: lash::direct::DirectMessage::parts [field]
    field_witness(|value: &lash::direct::DirectMessage| {
        let _ = &value.parts;
    });
    // W0051: lash::direct::DirectMessage::role [field]
    field_witness(|value: &lash::direct::DirectMessage| {
        let _ = &value.role;
    });
    // W0052: lash::direct::DirectOutputSpec [enum]
    type_witness::<lash::direct::DirectOutputSpec>();
    // W0053: lash::direct::DirectOutputSpec::JsonObject [variant]
    variant_witness(|value: &lash::direct::DirectOutputSpec| {
        matches!(value, lash::direct::DirectOutputSpec::JsonObject)
    });
    // W0054: lash::direct::DirectOutputSpec::JsonSchema [variant]
    variant_witness(|value: &lash::direct::DirectOutputSpec| {
        matches!(value, lash::direct::DirectOutputSpec::JsonSchema(..))
    });
    // W0055: lash::direct::DirectOutputSpec::JsonSchema::0 [field]
    field_witness(|value: &lash::direct::DirectOutputSpec| {
        if let lash::direct::DirectOutputSpec::JsonSchema(f0) = value {
            let _ = f0;
        }
    });
    // W0056: lash::direct::DirectOutputSpec::Text [variant]
    variant_witness(|value: &lash::direct::DirectOutputSpec| {
        matches!(value, lash::direct::DirectOutputSpec::Text)
    });
    // W0057: lash::direct::DirectPart [enum]
    type_witness::<lash::direct::DirectPart>();
    // W0058: lash::direct::DirectPart::Text [variant]
    variant_witness(|value: &lash::direct::DirectPart| {
        matches!(value, lash::direct::DirectPart::Text(..))
    });
    // W0059: lash::direct::DirectPart::Text::0 [field]
    field_witness(|value: &lash::direct::DirectPart| {
        if let lash::direct::DirectPart::Text(f0) = value {
            let _ = f0;
        }
    });
    // W0060: lash::direct::DirectRequest [struct]
    type_witness::<lash::direct::DirectRequest>();
    // W0061: lash::direct::DirectRequest::caused_by [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.caused_by;
    });
    // W0062: lash::direct::DirectRequest::generation [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.generation;
    });
    // W0063: lash::direct::DirectRequest::json [function]
    let _: fn(String, String) -> lash::direct::DirectRequest = lash::direct::DirectRequest::json;
    // W0064: lash::direct::DirectRequest::json_schema [function]
    let _: fn(String, String, lash::direct::DirectJsonSchema) -> lash::direct::DirectRequest =
        lash::direct::DirectRequest::json_schema;
    // W0065: lash::direct::DirectRequest::model [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.model;
    });
    // W0066: lash::direct::DirectRequest::model_capability [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.model_capability;
    });
    // W0067: lash::direct::DirectRequest::output [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.output;
    });
    // W0068: lash::direct::DirectRequest::replay [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.replay;
    });
    // W0069: lash::direct::DirectRequest::session_id [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.session_id;
    });
    // W0070: lash::direct::DirectRequest::stream_events [field]
    field_witness(|value: &lash::direct::DirectRequest| {
        let _ = &value.stream_events;
    });
    // W0071: lash::direct::DirectRequest::with_caused_by [function]
    let _ = lash::direct::DirectRequest::with_caused_by;
    // W0072: lash::direct::DirectRequest::with_replay_key [function]
    let _: fn(lash::direct::DirectRequest, String) -> lash::direct::DirectRequest =
        lash::direct::DirectRequest::with_replay_key;
    // W0073: lash::direct::DirectRole [enum]
    type_witness::<lash::direct::DirectRole>();
    // W0074: lash::direct::DirectRole::Assistant [variant]
    variant_witness(|value: &lash::direct::DirectRole| {
        matches!(value, lash::direct::DirectRole::Assistant)
    });
    // W0075: lash::direct::DirectRole::System [variant]
    variant_witness(|value: &lash::direct::DirectRole| {
        matches!(value, lash::direct::DirectRole::System)
    });
    // W0076: lash::direct::DirectRole::User [variant]
    variant_witness(|value: &lash::direct::DirectRole| {
        matches!(value, lash::direct::DirectRole::User)
    });
    // W0077: lash::direct::GenerationReceipt [struct]
    type_witness::<lash::direct::GenerationReceipt>();
    // W0078: lash::direct::GenerationReceipt::cache [field]
    field_witness(|value: &lash::direct::GenerationReceipt| {
        let _ = &value.cache;
    });
    // W0079: lash::direct::GenerationReceipt::fully_honored [function]
    let _ = lash::direct::GenerationReceipt::fully_honored;
    // W0080: lash::direct::GenerationReceipt::nothing_omitted [function]
    let _ = lash::direct::GenerationReceipt::nothing_omitted;
    // W0081: lash::direct::GenerationReceipt::output_token_cap [field]
    field_witness(|value: &lash::direct::GenerationReceipt| {
        let _ = &value.output_token_cap;
    });
    // W0082: lash::direct::GenerationReceipt::seed [field]
    field_witness(|value: &lash::direct::GenerationReceipt| {
        let _ = &value.seed;
    });
    // W0083: lash::direct::GenerationReceipt::stop_sequences [field]
    field_witness(|value: &lash::direct::GenerationReceipt| {
        let _ = &value.stop_sequences;
    });
    // W0084: lash::direct::GenerationReceipt::temperature [field]
    field_witness(|value: &lash::direct::GenerationReceipt| {
        let _ = &value.temperature;
    });
    // W0085: lash::direct::GenerationOptionOutcome [enum]
    type_witness::<lash::direct::GenerationOptionOutcome>();
    // W0086: lash::direct::GenerationOptionOutcome::Applied [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(value, lash::direct::GenerationOptionOutcome::Applied)
    });
    // W0087: lash::direct::GenerationOptionOutcome::ClampedToCapacity [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(
            value,
            lash::direct::GenerationOptionOutcome::ClampedToCapacity
        )
    });
    // W0088: lash::direct::GenerationOptionOutcome::NotRequested [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(value, lash::direct::GenerationOptionOutcome::NotRequested)
    });
    // W0089: lash::direct::GenerationOptionOutcome::OmittedSamplingPinned [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(
            value,
            lash::direct::GenerationOptionOutcome::OmittedSamplingPinned
        )
    });
    // W0090: lash::direct::GenerationOptionOutcome::OmittedUnsupported [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(
            value,
            lash::direct::GenerationOptionOutcome::OmittedUnsupported
        )
    });
    // W0091: lash::direct::GenerationOptionOutcome::SuppressedProtocolOwned [variant]
    variant_witness(|value: &lash::direct::GenerationOptionOutcome| {
        matches!(
            value,
            lash::direct::GenerationOptionOutcome::SuppressedProtocolOwned
        )
    });
    // W0092: lash::direct::GenerationOptionOutcome::applied [function]
    let _ = lash::direct::GenerationOptionOutcome::applied;
    // W0093: lash::direct::GenerationOptionOutcome::is_honored [function]
    let _ = lash::direct::GenerationOptionOutcome::is_honored;
    // W0094: lash::direct::GenerationOptionOutcome::is_omitted [function]
    let _ = lash::direct::GenerationOptionOutcome::is_omitted;
    // W0095: lash::direct::GenerationOptionOutcome::sampling_pinned [function]
    let _ = lash::direct::GenerationOptionOutcome::sampling_pinned;
    // W0096: lash::direct::GenerationOptionOutcome::unsupported [function]
    let _ = lash::direct::GenerationOptionOutcome::unsupported;
    // W0097: lash::direct::GenerationOptions [struct]
    type_witness::<lash::direct::GenerationOptions>();
    // W0098: lash::direct::GenerationOptions::merged_over [function]
    let _ = lash::direct::GenerationOptions::merged_over;
    // W0099: lash::direct::GenerationOptions::output_token_cap [field]
    field_witness(|value: &lash::direct::GenerationOptions| {
        let _ = &value.output_token_cap;
    });
    // W0100: lash::direct::GenerationOptions::output_token_cap_u64 [function]
    let _ = lash::direct::GenerationOptions::output_token_cap_u64;
    // W0101: lash::direct::GenerationOptions::projection_provenance [field]
    field_witness(|value: &lash::direct::GenerationOptions| {
        let _ = &value.projection_provenance;
    });
    // W0102: lash::direct::GenerationOptions::suppress_stop_sequences_for_protocol [function]
    let _ = lash::direct::GenerationOptions::suppress_stop_sequences_for_protocol;
    // W0103: lash::direct::GenerationOptions::seed [field]
    field_witness(|value: &lash::direct::GenerationOptions| {
        let _ = &value.seed;
    });
    // W0104: lash::direct::GenerationOptions::stop_sequences [field]
    field_witness(|value: &lash::direct::GenerationOptions| {
        let _ = &value.stop_sequences;
    });
    // W0105: lash::direct::GenerationOptions::stop_sequences_suppressed_by_protocol [function]
    let _ = lash::direct::GenerationOptions::stop_sequences_suppressed_by_protocol;
    // W0106: lash::direct::GenerationOptions::temperature [field]
    field_witness(|value: &lash::direct::GenerationOptions| {
        let _ = &value.temperature;
    });
    // W0107: lash::direct::LlmTerminalReason [enum]
    type_witness::<lash::direct::LlmTerminalReason>();
    // W0108: lash::direct::LlmTerminalReason::Cancelled [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::Cancelled)
    });
    // W0109: lash::direct::LlmTerminalReason::ContentFilter [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::ContentFilter)
    });
    // W0110: lash::direct::LlmTerminalReason::ContextOverflow [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::ContextOverflow)
    });
    // W0111: lash::direct::LlmTerminalReason::OutputLimit [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::OutputLimit)
    });
    // W0112: lash::direct::LlmTerminalReason::Stop [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::Stop)
    });
    // W0113: lash::direct::LlmTerminalReason::Unknown [variant]
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::Unknown)
    });
    // W0114: lash::direct::LlmTerminalReason::code [function]
    let _ = lash::direct::LlmTerminalReason::code;
    // W0115: lash::direct::NonNegativeFiniteF64 [struct]
    type_witness::<lash::direct::NonNegativeFiniteF64>();
    // W0116: lash::persistence::PendingTurnInputClaimDiagnostics::claim_session_lease_generation [field]
    field_witness(
        |value: &lash::persistence::PendingTurnInputClaimDiagnostics| {
            let _ = &value.claim_session_lease_generation;
        },
    );
    // W0117: lash::persistence::PersistedSessionConfig::generation [field]
    field_witness(|value: &lash::persistence::PersistedSessionConfig| {
        let _ = &value.generation;
    });
    // W0118: lash::persistence::PersistedSessionConfig::model [field]
    field_witness(|value: &lash::persistence::PersistedSessionConfig| {
        let _ = &value.model;
    });
    // W0119: lash::persistence::PersistedTurnState::protocol_turn_options [field]
    field_witness(|value: &lash::persistence::PersistedTurnState| {
        let _ = &value.protocol_turn_options;
    });
    // W0120: lash::persistence::ProtocolEvent [struct]
    type_witness::<lash::persistence::ProtocolEvent>();
    // W0121: lash::persistence::ProtocolEvent::decode [function]
    let _ = lash::persistence::ProtocolEvent::decode::<()>(todo!(), "");
    // W0122: lash::persistence::ProtocolEvent::payload [field]
    field_witness(|value: &lash::persistence::ProtocolEvent| {
        let _ = &value.payload;
    });
    // W0123: lash::persistence::ProtocolEvent::typed [function]
    let _ = lash::persistence::ProtocolEvent::typed::<()>(String::new(), ());
    // W0125: lash::persistence::RuntimeSessionState::effective_protocol_turn_options [function]
    let _ = lash::persistence::RuntimeSessionState::effective_protocol_turn_options;
    // W0126: lash::persistence::RuntimeSessionState::protocol_turn_options [field]
    field_witness(|value: &lash::persistence::RuntimeSessionState| {
        let _ = &value.protocol_turn_options;
    });
    // W0127: lash::persistence::SessionCheckpoint::schema_version [field]
    field_witness(|value: &lash::persistence::SessionCheckpoint| {
        let _ = &value.schema_version;
    });
    // W0128: lash::persistence::SessionGraph::append_protocol_event [function]
    let _ = lash::persistence::SessionGraph::append_protocol_event;
    // W0129: lash::persistence::SessionReadView::protocol_turn_options [function]
    let _ = lash::persistence::SessionReadView::protocol_turn_options;
    // W0130: lash::persistence::StoreError::InvalidRecordSchemaVersion [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::InvalidRecordSchemaVersion { .. }
        )
    });
    // W0131: lash::persistence::StoreError::InvalidRecordSchemaVersion::actual [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidRecordSchemaVersion { actual, .. } = value {
            let _ = actual;
        }
    });
    // W0132: lash::persistence::StoreError::InvalidRecordSchemaVersion::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidRecordSchemaVersion { expected, .. } = value {
            let _ = expected;
        }
    });
    // W0133: lash::persistence::StoreError::InvalidRecordSchemaVersion::record_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidRecordSchemaVersion { record_kind, .. } = value
        {
            let _ = record_kind;
        }
    });
    // W0134: lash::persistence::StoreError::MissingRecordSchemaVersion [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::MissingRecordSchemaVersion { .. }
        )
    });
    // W0135: lash::persistence::StoreError::MissingRecordSchemaVersion::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::MissingRecordSchemaVersion { expected, .. } = value {
            let _ = expected;
        }
    });
    // W0136: lash::persistence::StoreError::MissingRecordSchemaVersion::record_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::MissingRecordSchemaVersion { record_kind, .. } = value
        {
            let _ = record_kind;
        }
    });
    // W0137: lash::persistence::StoreError::UnsupportedRecordSchemaVersion [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnsupportedRecordSchemaVersion { .. }
        )
    });
    // W0138: lash::persistence::StoreError::UnsupportedRecordSchemaVersion::actual [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsupportedRecordSchemaVersion { actual, .. } = value
        {
            let _ = actual;
        }
    });
    // W0139: lash::persistence::StoreError::UnsupportedRecordSchemaVersion::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsupportedRecordSchemaVersion { expected, .. } =
            value
        {
            let _ = expected;
        }
    });
    // W0140: lash::persistence::StoreError::UnsupportedRecordSchemaVersion::record_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsupportedRecordSchemaVersion {
            record_kind, ..
        } = value
        {
            let _ = record_kind;
        }
    });
    // W0141: lash::persistence::WorkClaim::session_lease_generation [field]
    field_witness(|value: &lash::persistence::WorkClaim<()>| {
        let _ = &value.session_lease_generation;
    });
    // W0142: lash::plugins::SessionAppendNode::ProtocolEvent [variant]
    variant_witness(|value: &lash::plugins::SessionAppendNode| {
        matches!(
            value,
            lash::plugins::SessionAppendNode::ProtocolEvent { .. }
        )
    });
    // W0143: lash::plugins::SessionAppendNode::ProtocolEvent::event [field]
    field_witness(|value: &lash::plugins::SessionAppendNode| {
        if let lash::plugins::SessionAppendNode::ProtocolEvent { event, .. } = value {
            let _ = event;
        }
    });
    // W0144: lash::plugins::SessionAppendNode::protocol_event [function]
    let _ = lash::plugins::SessionAppendNode::protocol_event;
    // W0145: lash::plugins::SessionStateChangedContext::direct_completions [field]
    field_witness(|value: &lash::plugins::SessionStateChangedContext| {
        let _ = &value.direct_completions;
    });
    // W0146: lash::plugins::TurnTransformContext::direct_completions [field]
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.direct_completions;
    });
    // W0147: lash::remote::Envelope::into_body [function]
    let _ = lash::remote::Envelope::<serde_json::Value>::into_body;
    // W0148: lash::remote::Envelope::new [function]
    let _ = lash::remote::Envelope::<serde_json::Value>::new;
    // W0149: lash::remote::Envelope::protocol_version [function]
    let _ = lash::remote::Envelope::<serde_json::Value>::protocol_version;
    // W0150: lash::remote::RemoteProtocolError::ConflictingLlmCallRecord [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(
            value,
            lash::remote::RemoteProtocolError::ConflictingLlmCallRecord { .. }
        )
    });
    // W0151: lash::remote::RemoteProtocolError::ConflictingLlmCallRecord::call_id [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::ConflictingLlmCallRecord { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0152: lash::remote::RemoteProtocolError::DuplicateLlmCallActivity [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(
            value,
            lash::remote::RemoteProtocolError::DuplicateLlmCallActivity { .. }
        )
    });
    // W0153: lash::remote::RemoteProtocolError::DuplicateLlmCallActivity::call_id [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::DuplicateLlmCallActivity { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0154: lash::remote::RemoteProtocolError::DuplicateLlmCallSummary [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(
            value,
            lash::remote::RemoteProtocolError::DuplicateLlmCallSummary { .. }
        )
    });
    // W0155: lash::remote::RemoteProtocolError::DuplicateLlmCallSummary::call_id [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::DuplicateLlmCallSummary { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0156: lash::remote::RemoteProtocolError::MessageDecode [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(value, lash::remote::RemoteProtocolError::MessageDecode(..))
    });
    // W0157: lash::remote::RemoteProtocolError::MessageDecode::0 [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::MessageDecode(f0) = value {
            let _ = f0;
        }
    });
    // W0158: lash::remote::RemoteProtocolError::MissingLlmCallActivity [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(
            value,
            lash::remote::RemoteProtocolError::MissingLlmCallActivity { .. }
        )
    });
    // W0159: lash::remote::RemoteProtocolError::MissingLlmCallActivity::call_id [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::MissingLlmCallActivity { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0160: lash::remote::RemoteProtocolError::MissingLlmCallSummary [variant]
    variant_witness(|value: &lash::remote::RemoteProtocolError| {
        matches!(
            value,
            lash::remote::RemoteProtocolError::MissingLlmCallSummary { .. }
        )
    });
    // W0161: lash::remote::RemoteProtocolError::MissingLlmCallSummary::call_id [field]
    field_witness(|value: &lash::remote::RemoteProtocolError| {
        if let lash::remote::RemoteProtocolError::MissingLlmCallSummary { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0162: lash::remote::llm::RemoteLlmRequest::encode_json [function]
    let _ = lash::remote::llm::RemoteLlmRequest::encode_json;
    // W0165: lash::remote::turn_input::RemoteTurnInput::decode_json [function]
    let _ = lash::remote::turn_input::RemoteTurnInput::decode_json;
    // W0166: lash::remote::turn_input::RemoteTurnInput::encode_json [function]
    let _ = lash::remote::turn_input::RemoteTurnInput::encode_json;
    // W0167: lash::remote::turn_result::RemoteTurnReport::decode_json [function]
    let _ = lash::remote::turn_result::RemoteTurnReport::decode_json;
    // W0168: lash::remote::turn_result::RemoteTurnReport::llm_calls [field]
    field_witness(|value: &lash::remote::turn_result::RemoteTurnReport| {
        let _ = &value.llm_calls;
    });
    // W0169: lash::runtime::AssembledTurn::llm_calls [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.llm_calls;
    });
    // W0170: lash::runtime::AssistantResponseHookEvents [struct]
    type_witness::<lash::runtime::AssistantResponseHookEvents>();
    // W0171: lash::runtime::AssistantResponseHookEvents::events [field]
    field_witness(|value: &lash::runtime::AssistantResponseHookEvents| {
        let _ = &value.events;
    });
    // W0172: lash::runtime::AssistantResponseHookEvents::plugin_id [field]
    field_witness(|value: &lash::runtime::AssistantResponseHookEvents| {
        let _ = &value.plugin_id;
    });
    // W0173: lash::runtime::DirectCompletionClient [struct]
    type_witness::<lash::runtime::DirectCompletionClient>();
    // W0174: lash::runtime::DirectCompletionClient::direct_completion [function]
    let _ = lash::runtime::DirectCompletionClient::direct_completion;
    // W0175: lash::runtime::DirectCompletionClient::direct_llm_completion [function]
    let _ = lash::runtime::DirectCompletionClient::direct_llm_completion;
    // W0176: lash::runtime::DirectCompletionClient::from_fn [function]
    let _ = lash::runtime::DirectCompletionClient::from_fn(
        |_: lash::direct::DirectRequest,
         _: String|
         -> Result<lash::direct::DirectCompletion, lash::plugins::PluginError> { todo!() },
    );
    // W0177: lash::runtime::LlmRequestSpec [struct]
    type_witness::<lash::runtime::LlmRequestSpec>();
    // W0178: lash::runtime::LlmRequestSpec::generation [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.generation;
    });
    // W0179: lash::runtime::LlmRequestSpec::messages [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.messages;
    });
    // W0180: lash::runtime::LlmRequestSpec::model [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.model;
    });
    // W0181: lash::runtime::LlmRequestSpec::model_capability [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.model_capability;
    });
    // W0182: lash::runtime::LlmRequestSpec::model_variant [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.model_variant;
    });
    // W0183: lash::runtime::LlmRequestSpec::output_spec [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.output_spec;
    });
    // W0184: lash::runtime::LlmRequestSpec::scope [field]
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.scope;
    });
    // W0185: lash::runtime::ProtocolSessionExtensionHandle [struct]
    type_witness::<lash::runtime::ProtocolSessionExtensionHandle>();
    // W0186: lash::runtime::ProtocolSessionExtensionHandle::as_any [function]
    let _ = lash::runtime::ProtocolSessionExtensionHandle::as_any;
    // W0187: lash::runtime::ProtocolSessionExtensionHandle::new [function]
    let _ = lash::runtime::ProtocolSessionExtensionHandle::new(NoopSessionExt);
    // W0188: lash::runtime::ProtocolTurnOptions [struct]
    type_witness::<lash::runtime::ProtocolTurnOptions>();
    // W0189: lash::runtime::ProtocolTurnOptions::decode [function]
    let _ = lash::runtime::ProtocolTurnOptions::decode::<()>(todo!());
    // W0190: lash::runtime::ProtocolTurnOptions::empty [function]
    let _ = lash::runtime::ProtocolTurnOptions::empty;
    // W0191: lash::runtime::ProtocolTurnOptions::from_payload [function]
    let _ = lash::runtime::ProtocolTurnOptions::from_payload;
    // W0192: lash::runtime::ProtocolTurnOptions::is_empty [function]
    let _ = lash::runtime::ProtocolTurnOptions::is_empty;
    // W0193: lash::runtime::ProtocolTurnOptions::payload [field]
    field_witness(|value: &lash::runtime::ProtocolTurnOptions| {
        let _ = &value.payload;
    });
    // W0194: lash::runtime::ProtocolTurnOptions::typed [function]
    let _ = lash::runtime::ProtocolTurnOptions::typed::<()>(());
    // W0195: lash::runtime::RuntimeEffectCommand::Direct [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(value, lash::runtime::RuntimeEffectCommand::Direct { .. })
    });
    // W0196: lash::runtime::RuntimeEffectCommand::Direct::request [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::Direct { request, .. } = value {
            let _ = request;
        }
    });
    // W0197: lash::runtime::RuntimeEffectCommand::LlmCall [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(value, lash::runtime::RuntimeEffectCommand::LlmCall { .. })
    });
    // W0198: lash::runtime::RuntimeEffectCommand::AssistantResponseHooks [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(
            value,
            lash::runtime::RuntimeEffectCommand::AssistantResponseHooks { .. }
        )
    });
    // W0199: lash::runtime::RuntimeEffectCommand::AssistantResponseHooks::response [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::AssistantResponseHooks { response, .. } = value
        {
            let _ = response;
        }
    });
    // W0200: lash::runtime::RuntimeEffectCommand::LanguageRuntimeValue [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(
            value,
            lash::runtime::RuntimeEffectCommand::LanguageRuntimeValue { .. }
        )
    });
    // W0201: lash::runtime::RuntimeEffectCommand::LanguageRuntimeValue::operation [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::LanguageRuntimeValue { operation, .. } = value {
            let _ = operation;
        }
    });
    // W0202: lash::runtime::RuntimeEffectCommand::LlmCall::request [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::LlmCall { request, .. } = value {
            let _ = request;
        }
    });
    // W0203: lash::runtime::RuntimeEffectKind::Direct [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::Direct)
    });
    // W0204: lash::runtime::RuntimeEffectKind::LlmCall [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::LlmCall)
    });
    // W0205: lash::runtime::RuntimeEffectKind::AssistantResponseHooks [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(
            value,
            lash::runtime::RuntimeEffectKind::AssistantResponseHooks
        )
    });
    // W0206: lash::runtime::RuntimeEffectKind::LanguageRuntimeValue [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(
            value,
            lash::runtime::RuntimeEffectKind::LanguageRuntimeValue
        )
    });
    // W0207: lash::runtime::RuntimeEffectLocalExecutor::language_runtime_value [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::language_runtime_value;
    // W0208: lash::runtime::RuntimeEffectOutcome::Direct [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(value, lash::runtime::RuntimeEffectOutcome::Direct { .. })
    });
    // W0209: lash::runtime::RuntimeEffectOutcome::Direct::call_record [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::Direct { call_record, .. } = value {
            let _ = call_record;
        }
    });
    // W0210: lash::runtime::RuntimeEffectOutcome::Direct::result [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::Direct { result, .. } = value {
            let _ = result;
        }
    });
    // W0211: lash::runtime::RuntimeEffectOutcome::LlmCall [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(value, lash::runtime::RuntimeEffectOutcome::LlmCall { .. })
    });
    // W0212: lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(
            value,
            lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks { .. }
        )
    });
    // W0213: lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks::events [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks { events, .. } = value {
            let _ = events;
        }
    });
    // W0214: lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks::response [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::AssistantResponseHooks { response, .. } = value
        {
            let _ = response;
        }
    });
    // W0215: lash::runtime::RuntimeEffectOutcome::LanguageRuntimeValue [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(
            value,
            lash::runtime::RuntimeEffectOutcome::LanguageRuntimeValue { .. }
        )
    });
    // W0216: lash::runtime::RuntimeEffectOutcome::LanguageRuntimeValue::value [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::LanguageRuntimeValue { value, .. } = value {
            let _ = value;
        }
    });
    // W0217: lash::runtime::RuntimeEffectOutcome::LlmCall::call_record [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::LlmCall { call_record, .. } = value {
            let _ = call_record;
        }
    });
    // W0218: lash::runtime::RuntimeEffectOutcome::LlmCall::result [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::LlmCall { result, .. } = value {
            let _ = result;
        }
    });
    // W0219: lash::runtime::RuntimeEffectOutcome::LlmCall::text_streamed [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::LlmCall { text_streamed, .. } = value {
            let _ = text_streamed;
        }
    });
    // W0220: lash::runtime::RuntimeEffectOutcome::into_language_runtime_value [function]
    let _ = lash::runtime::RuntimeEffectOutcome::into_language_runtime_value;
    // W0221: lash::runtime::RuntimeErrorCode::DurableEffectLiveProtocolExtension [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::DurableEffectLiveProtocolExtension
        )
    });
    // W0222: lash::runtime::RuntimeErrorCode::ProtocolBeforeLlmCall [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ProtocolBeforeLlmCall
        )
    });
    // W0223: lash::runtime::RuntimeErrorCode::ProtocolTurnExtension [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ProtocolTurnExtension
        )
    });
    // W0225: lash::runtime::SessionPolicy::generation [field]
    field_witness(|value: &lash::runtime::SessionPolicy| {
        let _ = &value.generation;
    });
    // W0226: lash::runtime::SessionPolicy::model [field]
    field_witness(|value: &lash::runtime::SessionPolicy| {
        let _ = &value.model;
    });
    // W0227: lash::runtime::SessionPolicy::model_id [function]
    let _ = lash::runtime::SessionPolicy::model_id;
    // W0228: lash::runtime::SessionPolicy::model_variant [function]
    let _ = lash::runtime::SessionPolicy::model_variant;
    // W0229: lash::runtime::SessionSnapshot::protocol_turn_options [field]
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.protocol_turn_options;
    });
    // W0230: lash::plugins::AgentFrameRecord::protocol_turn_options [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.protocol_turn_options;
    });
    // W0231: lash_core::AttemptRecord::generation_disposition [field]
    field_witness(|value: &lash_core::AttemptRecord| {
        let _ = &value.generation_disposition;
    });
    // W0232: lash_core::AttemptRecord::protocol_position [field]
    field_witness(|value: &lash_core::AttemptRecord| {
        let _ = &value.protocol_position;
    });
    // W0234: lash::persistence::ExecutedCallOutcome [enum]
    type_witness::<lash::persistence::ExecutedCallOutcome>();
    // W0235: lash::persistence::ExecutedCallOutcome::Err [variant]
    variant_witness(|value: &lash::persistence::ExecutedCallOutcome| {
        matches!(value, lash::persistence::ExecutedCallOutcome::Err)
    });
    // W0236: lash::persistence::ExecutedCallOutcome::Ok [variant]
    variant_witness(|value: &lash::persistence::ExecutedCallOutcome| {
        matches!(value, lash::persistence::ExecutedCallOutcome::Ok)
    });
    // W0237: lash::persistence::ExecutedCallOutcome::as_str [function]
    let _ = lash::persistence::ExecutedCallOutcome::as_str;
    // W0238: lash::persistence::ExecutedCallRecord [struct]
    type_witness::<lash::persistence::ExecutedCallRecord>();
    // W0239: lash::persistence::ExecutedCallRecord::operation [field]
    field_witness(|value: &lash::persistence::ExecutedCallRecord| {
        let _ = &value.operation;
    });
    // W0240: lash::persistence::ExecutedCallRecord::outcome [field]
    field_witness(|value: &lash::persistence::ExecutedCallRecord| {
        let _ = &value.outcome;
    });
    // W0241: lash::plugins::HostTurnProtocol [struct]
    type_witness::<lash::plugins::HostTurnProtocol>();
    // W0242: lash_core::LlmCallError [struct]
    type_witness::<lash_core::LlmCallError>();
    // W0243: lash_core::LlmCallError::code [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.code;
    });
    // W0244: lash_core::LlmCallError::kind [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.kind;
    });
    // W0245: lash_core::LlmCallError::message [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.message;
    });
    // W0246: lash_core::LlmCallError::partial_response [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.partial_response;
    });
    // W0247: lash_core::LlmCallError::raw [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.raw;
    });
    // W0248: lash_core::LlmCallError::request_body [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.request_body;
    });
    // W0249: lash_core::LlmCallError::retryable [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.retryable;
    });
    // W0250: lash_core::LlmCallError::terminal_reason [field]
    field_witness(|value: &lash_core::LlmCallError| {
        let _ = &value.terminal_reason;
    });
    // W0251: lash_core::LlmCallId [struct]
    type_witness::<lash_core::LlmCallId>();
    // W0252: lash_core::LlmCallId::0 [field]
    field_witness(|value: &lash_core::LlmCallId| {
        let _ = &value.0;
    });
    // W0253: lash::plugins::ProtocolBeforeLlmCallContext [struct]
    type_witness::<lash::plugins::ProtocolBeforeLlmCallContext>();
    // W0254: lash::plugins::ProtocolBeforeLlmCallContext::session_graph [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.session_graph;
    });
    // W0255: lash::plugins::ProtocolBeforeLlmCallContext::session_id [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.session_id;
    });
    // W0256: lash::plugins::ProtocolBeforeLlmCallContext::sessions [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.sessions;
    });
    // W0257: lash::plugins::ProtocolBeforeLlmCallContext::state [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.state;
    });
    // W0258: lash::plugins::ProtocolBuildInput [struct]
    type_witness::<lash::plugins::ProtocolBuildInput>();
    // W0259: lash::plugins::ProtocolDriverState [struct]
    type_witness::<lash::plugins::ProtocolDriverState>();
    // W0260: lash::plugins::ProtocolDriverState::new [function]
    let _ = lash::plugins::ProtocolDriverState::new(String::new(), todo!());
    // W0261: lash::plugins::ProtocolDriverState::payload [field]
    field_witness(|value: &lash::plugins::ProtocolDriverState| {
        let _ = &value.payload;
    });
    // W0262: lash::plugins::ProtocolLlmCallAction [enum]
    type_witness::<lash::plugins::ProtocolLlmCallAction>();
    // W0263: lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame [variant]
    variant_witness(|value: &lash::plugins::ProtocolLlmCallAction| {
        matches!(
            value,
            lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame { .. }
        )
    });
    // W0264: lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame::frame_key [field]
    field_witness(|value: &lash::plugins::ProtocolLlmCallAction| {
        let lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame { frame_key, .. } = value;
        let _ = frame_key;
    });
    // W0265: lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame::task [field]
    field_witness(|value: &lash::plugins::ProtocolLlmCallAction| {
        let lash::plugins::ProtocolLlmCallAction::SwitchAgentFrame { task, .. } = value;
        let _ = task;
    });
    // W0266: lash::provider::ProtocolPosition [enum]
    type_witness::<lash::provider::ProtocolPosition>();
    // W0267: lash::provider::ProtocolPosition::NoResponse [variant]
    variant_witness(|value: &lash::provider::ProtocolPosition| {
        matches!(value, lash::provider::ProtocolPosition::NoResponse)
    });
    // W0268: lash::provider::ProtocolPosition::OutputStarted [variant]
    variant_witness(|value: &lash::provider::ProtocolPosition| {
        matches!(value, lash::provider::ProtocolPosition::OutputStarted)
    });
    // W0269: lash_core::ProtocolSessionExtension [trait]
    fn trait_witness_0269<T: lash_core::ProtocolSessionExtension>() {}
    // W0270: lash_core::ProtocolSessionExtension::as_any [function]
    fn meth_0270<T: lash_core::ProtocolSessionExtension>(_: &T) {
        let _ = T::as_any;
    }
    // W0271: lash::plugins::ProtocolTurnExtension [trait]
    fn trait_witness_0271<T: lash::plugins::ProtocolTurnExtension>() {}
    // W0272: lash::plugins::ProtocolTurnExtension::as_any [function]
    fn meth_0272<T: lash::plugins::ProtocolTurnExtension>(_: &T) {
        let _ = T::as_any;
    }
    // W0273: lash::runtime::ProtocolTurnExtensionHandle [struct]
    type_witness::<lash::runtime::ProtocolTurnExtensionHandle>();
    // W0274: lash::runtime::ProtocolTurnExtensionHandle::as_any [function]
    let _ = lash::runtime::ProtocolTurnExtensionHandle::as_any;
    // W0275: lash::runtime::ProtocolTurnExtensionHandle::new [function]
    let _ = lash::runtime::ProtocolTurnExtensionHandle::new(NoopTurnExt);
    // W0276: lash::plugins::ProtocolTurnOptionsError [enum]
    type_witness::<lash::plugins::ProtocolTurnOptionsError>();
    // W0277: lash::plugins::ProtocolTurnOptionsError::Decode [variant]
    variant_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        matches!(value, lash::plugins::ProtocolTurnOptionsError::Decode(..))
    });
    // W0278: lash::plugins::ProtocolTurnOptionsError::Decode::0 [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::Decode(f0) = value {
            let _ = f0;
        }
    });
    // W0279: lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion [variant]
    variant_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        matches!(
            value,
            lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion { .. }
        )
    });
    // W0280: lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion::actual [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion { actual, .. } = value
        {
            let _ = actual;
        }
    });
    // W0281: lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion::expected [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::InvalidSchemaVersion { expected, .. } =
            value
        {
            let _ = expected;
        }
    });
    // W0282: lash::plugins::ProtocolTurnOptionsError::MissingSchemaVersion [variant]
    variant_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        matches!(
            value,
            lash::plugins::ProtocolTurnOptionsError::MissingSchemaVersion { .. }
        )
    });
    // W0283: lash::plugins::ProtocolTurnOptionsError::MissingSchemaVersion::expected [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::MissingSchemaVersion { expected, .. } =
            value
        {
            let _ = expected;
        }
    });
    // W0284: lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion [variant]
    variant_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        matches!(
            value,
            lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion { .. }
        )
    });
    // W0285: lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion::actual [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion {
            actual, ..
        } = value
        {
            let _ = actual;
        }
    });
    // W0286: lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion::expected [field]
    field_witness(|value: &lash::plugins::ProtocolTurnOptionsError| {
        if let lash::plugins::ProtocolTurnOptionsError::UnsupportedSchemaVersion {
            expected, ..
        } = value
        {
            let _ = expected;
        }
    });
    // W0287: lash::plugins::RuntimeExecutionContext::journaled_language_runtime_value [function]
    let _ = lash::plugins::RuntimeExecutionContext::journaled_language_runtime_value;
    // W0288: lash_core::SchemaContract [struct]
    type_witness::<lash_core::SchemaContract>();
    // W0289: lash_core::SchemaContract::canonical [field]
    field_witness(|value: &lash_core::SchemaContract| {
        let _ = &value.canonical;
    });
    // W0290: lash_core::SchemaContract::new [function]
    let _ = lash_core::SchemaContract::new;
    // W0291: lash_core::SchemaContract::projection [field]
    field_witness(|value: &lash_core::SchemaContract| {
        let _ = &value.projection;
    });
    // W0292: lash_core::SchemaContract::with_override [function]
    let _: fn(lash_core::SchemaContract, String, serde_json::Value) -> lash_core::SchemaContract =
        lash_core::SchemaContract::with_override;
    // W0293: lash_core::SchemaProjectionOverride [struct]
    type_witness::<lash_core::SchemaProjectionOverride>();
    // W0294: lash_core::SchemaProjectionOverride::dialect [field]
    field_witness(|value: &lash_core::SchemaProjectionOverride| {
        let _ = &value.dialect;
    });
    // W0295: lash_core::SchemaProjectionOverride::schema [field]
    field_witness(|value: &lash_core::SchemaProjectionOverride| {
        let _ = &value.schema;
    });
    // W0296: lash_core::SchemaProjectionPolicy [struct]
    type_witness::<lash_core::SchemaProjectionPolicy>();
    // W0297: lash_core::SchemaProjectionPolicy::mode [field]
    field_witness(|value: &lash_core::SchemaProjectionPolicy| {
        let _ = &value.mode;
    });
    // W0298: lash_core::SchemaProjectionPolicy::overrides [field]
    field_witness(|value: &lash_core::SchemaProjectionPolicy| {
        let _ = &value.overrides;
    });
    // W0299: lash::persistence::SessionNodePayload::FrameOpen::protocol_turn_options [field]
    field_witness(|value: &lash::persistence::SessionNodePayload| {
        if let lash::persistence::SessionNodePayload::FrameOpen {
            protocol_turn_options,
            ..
        } = value
        {
            let _ = protocol_turn_options;
        }
    });
    // W0300: lash::plugins::ChatContextProjector [struct]
    type_witness::<lash::plugins::ChatContextProjector>();
    // W0301: lash::plugins::ContextProjector [trait]
    fn trait_witness_0301<T: lash::plugins::ContextProjector>() {}
    // W0302: lash::plugins::ContextProjector::project [function]
    fn meth_0302<T: lash::plugins::ContextProjector>(_: &T) {
        let _ = T::project;
    }
    // W0303: lash::plugins::LlmToolSpec [struct]
    type_witness::<lash::plugins::LlmToolSpec>();
    // W0304: lash::plugins::LlmToolSpec::description [field]
    field_witness(|value: &lash::plugins::LlmToolSpec| {
        let _ = &value.description;
    });
    // W0305: lash::plugins::LlmToolSpec::input_schema [field]
    field_witness(|value: &lash::plugins::LlmToolSpec| {
        let _ = &value.input_schema;
    });
    // W0306: lash::plugins::LlmToolSpec::name [field]
    field_witness(|value: &lash::plugins::LlmToolSpec| {
        let _ = &value.name;
    });
    // W0307: lash::plugins::LlmToolSpec::output_schema [field]
    field_witness(|value: &lash::plugins::LlmToolSpec| {
        let _ = &value.output_schema;
    });
    // W0308: lash::plugins::PromptFingerprint [struct]
    type_witness::<lash::plugins::PromptFingerprint>();
    // W0309: lash::plugins::ProtocolDriverHandle [trait]
    fn trait_witness_0309<T: lash::plugins::ProtocolDriverHandle>() {}
    // W0310: lash::plugins::ProtocolDriverHandle::handle_exec_result [function]
    fn meth_0310<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::handle_exec_result;
    }
    // W0311: lash::plugins::ProtocolDriverHandle::handle_llm_success [function]
    fn meth_0311<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::handle_llm_success;
    }
    // W0312: lash::plugins::ProtocolDriverHandle::handle_tool_results [function]
    fn meth_0312<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::handle_tool_results;
    }
    // W0313: lash::plugins::ProtocolDriverHandle::handles_output_limit_response [function]
    fn meth_0313<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::handles_output_limit_response;
    }
    // W0314: lash::plugins::ProtocolDriverHandle::prepare_protocol_iteration [function]
    fn meth_0314<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::prepare_protocol_iteration;
    }
    // W0315: lash::plugins::ProtocolDriverHandle::project_visible_assistant_prose [function]
    fn meth_0315<T: lash::plugins::ProtocolDriverHandle>(_: &T) {
        let _ = T::project_visible_assistant_prose;
    }
    // W0316: lash::plugins::TurnDriverConfig [type_alias]
    type_witness::<lash::plugins::TurnDriverConfig>();
    // W0318: lash_core::facade_support::EffectId::0 [field]
    field_witness(|value: &lash_core::facade_support::EffectId| {
        let _ = &value.0;
    });
    // W0319: lash_core::test_support::SchemaDialect::as_str [function]
    let _ = lash_core::test_support::SchemaDialect::as_str;
    // W0320: lash_core::test_support::SchemaDialect::new [function]
    let _: fn(String) -> lash_core::test_support::SchemaDialect =
        lash_core::test_support::SchemaDialect::new;
    // W0321: lash_core::facade_support::SchemaResolutionError [struct]
    type_witness::<lash_core::facade_support::SchemaResolutionError>();
    // W0322: lash_core::facade_support::SchemaResolutionError::diagnostics [field]
    field_witness(|value: &lash_core::facade_support::SchemaResolutionError| {
        let _ = &value.diagnostics;
    });
    // W0323: lash_core::facade_support::SchemaResolutionError::dialect [field]
    field_witness(|value: &lash_core::facade_support::SchemaResolutionError| {
        let _ = &value.dialect;
    });
    // W0324: lash_core::facade_support::SchemaResolutionError::provider [field]
    field_witness(|value: &lash_core::facade_support::SchemaResolutionError| {
        let _ = &value.provider;
    });
    // W0325: lash_core::facade_support::SchemaResolutionError::purpose [field]
    field_witness(|value: &lash_core::facade_support::SchemaResolutionError| {
        let _ = &value.purpose;
    });
    // W0326: lash_core::facade_support::resolve_schema [function]
    let _ = lash_core::facade_support::resolve_schema;
    // W0327: lash::provider::ProviderCompletion [struct]
    type_witness::<lash::provider::ProviderCompletion>();
    // W0328: lash::provider::ProviderCompletion::call_record [field]
    field_witness(|value: &lash::provider::ProviderCompletion| {
        let _ = &value.call_record;
    });
    // W0329: lash::provider::ProviderCompletion::response [field]
    field_witness(|value: &lash::provider::ProviderCompletion| {
        let _ = &value.response;
    });
    // W0330: lash::provider::ProviderCompletionError [struct]
    type_witness::<lash::provider::ProviderCompletionError>();
    // W0331: lash::provider::ProviderCompletionError::call_record [field]
    field_witness(|value: &lash::provider::ProviderCompletionError| {
        let _ = &value.call_record;
    });
    // W0332: lash::provider::ProviderCompletionError::error [field]
    field_witness(|value: &lash::provider::ProviderCompletionError| {
        let _ = &value.error;
    });
    // W0333: lash::provider::CacheRetention [enum]
    type_witness::<lash::provider::CacheRetention>();
    // W0334: lash::provider::CacheRetention::Long [variant]
    variant_witness(|value: &lash::provider::CacheRetention| {
        matches!(value, lash::provider::CacheRetention::Long)
    });
    // W0335: lash::provider::CacheRetention::None [variant]
    variant_witness(|value: &lash::provider::CacheRetention| {
        matches!(value, lash::provider::CacheRetention::None)
    });
    // W0336: lash::provider::CacheRetention::Short [variant]
    variant_witness(|value: &lash::provider::CacheRetention| {
        matches!(value, lash::provider::CacheRetention::Short)
    });
    // W0337: lash::provider::ProviderRateLimitPermit [struct]
    type_witness::<lash::provider::ProviderRateLimitPermit>();
    // W0338: lash::provider::ProviderRateLimiter [struct]
    type_witness::<lash::provider::ProviderRateLimiter>();
    // W0339: lash::provider::ProviderRateLimiter::admit [function]
    let _ = lash::provider::ProviderRateLimiter::admit;
    // W0340: lash::provider::ProviderRateLimiter::clock [function]
    let _ = lash::provider::ProviderRateLimiter::clock;
    // W0342: lash::provider::ProviderRateLimiter::new [function]
    let _ = lash::provider::ProviderRateLimiter::new;
    // W0343: lash::provider::ProviderRateLimiter::with_clock [function]
    let _ = lash::provider::ProviderRateLimiter::with_clock;
    // W0344: lash_core::SchemaContract::canonical [function]
    let _ = lash_core::SchemaContract::canonical;
    // W0345: lash::plugins::HydratedExecutionState [struct]
    type_witness::<lash::plugins::HydratedExecutionState>();
    // W0346: lash::plugins::HydratedExecutionState::components [field]
    field_witness(|value: &lash::plugins::HydratedExecutionState| {
        let _ = &value.components;
    });
    // W0347: lash::plugins::HydratedExecutionState::root [field]
    field_witness(|value: &lash::plugins::HydratedExecutionState| {
        let _ = &value.root;
    });
    // W0348: lash::runtime::ApplyConfigPatch::generation [field]
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.generation;
    });
    // W0349: lash::runtime::ApplyConfigPatch::model [field]
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.model;
    });
    // W0350: lash::runtime::ApplyConfigPatch::prompt [field]
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.prompt;
    });
    // W0351: lash::runtime::ApplyConfigPatch::schema_version [field]
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.schema_version;
    });
    // W0352: lash::runtime::ApplyConfigPatch::turn_budget [field]
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.turn_budget;
    });
    // W0353: lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome [variant]
    variant_witness(|value: &lash::remote::usage::RemoteTurnEvent| {
        matches!(
            value,
            lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome { .. }
        )
    });
    // W0354: lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome::call_id [field]
    field_witness(|value: &lash::remote::usage::RemoteTurnEvent| {
        if let lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0355: lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome::outcome [field]
    field_witness(|value: &lash::remote::usage::RemoteTurnEvent| {
        if let lash::remote::usage::RemoteTurnEvent::ToolIntentOutcome { outcome, .. } = value {
            let _ = outcome;
        }
    });
    // W0356: lash::remote::usage::RemoteTurnActivity::decode_json [function]
    let _ = lash::remote::usage::RemoteTurnActivity::decode_json;
    #[cfg(feature = "rlm")]
    {
        // W0357: lash::rlm::RlmCreateExtras::termination [field]
        field_witness(|value: &lash::rlm::RlmCreateExtras| {
            let _ = &value.termination;
        });
        // W0358: lash::rlm::RlmSessionConfig::final_answer_format [field]
        field_witness(|value: &lash::rlm::RlmSessionConfig| {
            let _ = &value.final_answer_format;
        });
        // W0359: lash::rlm::RlmSessionConfig::final_answer_format [function]
        let _ = lash::rlm::RlmSessionConfig::final_answer_format;
        // W0360: lash::rlm::RlmSessionConfigError::Session [variant]
        variant_witness(|value: &lash::rlm::RlmSessionConfigError| {
            matches!(value, lash::rlm::RlmSessionConfigError::Session(..))
        });
        // W0361: lash::rlm::RlmSessionConfigError::Session::0 [field]
        field_witness(|value: &lash::rlm::RlmSessionConfigError| {
            if let lash::rlm::RlmSessionConfigError::Session(f0) = value {
                let _ = f0;
            }
        });
        // W0362: lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat [variant]
        variant_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            matches!(
                value,
                lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat { .. }
            )
        });
        // W0363: lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat::recorded [field]
        field_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            if let lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat { recorded, .. } = value {
                let _ = recorded;
            }
        });
        // W0364: lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat::requested [field]
        field_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            if let lash::rlm::RlmSessionConfigConflict::FinalAnswerFormat { requested, .. } = value
            {
                let _ = requested;
            }
        });
        // W0365: lash::rlm::RlmSessionConfigConflict::Termination [variant]
        variant_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            matches!(
                value,
                lash::rlm::RlmSessionConfigConflict::Termination { .. }
            )
        });
        // W0366: lash::rlm::RlmSessionConfigConflict::Termination::recorded [field]
        field_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            if let lash::rlm::RlmSessionConfigConflict::Termination { recorded, .. } = value {
                let _ = recorded;
            }
        });
        // W0367: lash::rlm::RlmSessionConfigConflict::Termination::requested [field]
        field_witness(|value: &lash::rlm::RlmSessionConfigConflict| {
            if let lash::rlm::RlmSessionConfigConflict::Termination { requested, .. } = value {
                let _ = requested;
            }
        });
        // W0368: lash::rlm::RlmTermination::Natural [variant]
        variant_witness(|value: &lash::rlm::RlmTermination| {
            matches!(value, lash::rlm::RlmTermination::Natural)
        });
    }
    // W0369: lash::direct::ProviderReasoningReplay::encrypted_content [field]
    field_witness(|value: &lash::direct::ProviderReasoningReplay| {
        let _ = &value.encrypted_content;
    });
    // W0370: lash::direct::ProviderReasoningReplay::is_empty [function]
    let _ = lash::direct::ProviderReasoningReplay::is_empty;
    // W0371: lash::direct::ProviderReasoningReplay::item_id [field]
    field_witness(|value: &lash::direct::ProviderReasoningReplay| {
        let _ = &value.item_id;
    });
    // W0372: lash::direct::ProviderReasoningReplay::redacted [field]
    field_witness(|value: &lash::direct::ProviderReasoningReplay| {
        let _ = &value.redacted;
    });
    // W0373: lash::direct::ProviderReasoningReplay::summary [field]
    field_witness(|value: &lash::direct::ProviderReasoningReplay| {
        let _ = &value.summary;
    });
    // W0374: lash::plugins::CodeExecutorPlugin [trait]
    fn trait_witness_0374<T: lash::plugins::CodeExecutorPlugin>() {}
    // W0375: lash::plugins::CodeExecutorPlugin::execute_code [function]
    fn meth_0375<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::execute_code;
    }
    // W0376: lash::plugins::CodeExecutorPlugin::execution_state_dirty [function]
    fn meth_0376<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::execution_state_dirty;
    }
    // W0377: lash::plugins::CodeExecutorPlugin::snapshot_execution_state [function]
    fn meth_0377<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::snapshot_execution_state;
    }
    // W0378: lash::plugins::CodeExecutorPlugin::probe_execution_state_capture [function]
    fn meth_0378<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::probe_execution_state_capture;
    }
    // W0379: lash::plugins::CodeExecutorPlugin::hydrated_execution_state [function]
    fn meth_0379<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::hydrated_execution_state;
    }
    // W0380: lash::plugins::CodeExecutorPlugin::acknowledge_execution_state_capture [function]
    fn meth_0380<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::acknowledge_execution_state_capture;
    }
    // W0381: lash::plugins::CodeExecutorPlugin::abort_execution_state_capture [function]
    fn meth_0381<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::abort_execution_state_capture;
    }
    // W0382: lash::plugins::CodeExecutorPlugin::restore_execution_state [function]
    fn meth_0382<T: lash::plugins::CodeExecutorPlugin>(_: &T) {
        let _ = T::restore_execution_state;
    }
    // W0383: lash::plugins::ExecutionStateComponentSnapshot [enum]
    type_witness::<lash::plugins::ExecutionStateComponentSnapshot>();
    // W0384: lash::plugins::ExecutionStateComponentSnapshot::Changed [variant]
    variant_witness(|value: &lash::plugins::ExecutionStateComponentSnapshot| {
        matches!(
            value,
            lash::plugins::ExecutionStateComponentSnapshot::Changed(..)
        )
    });
    // W0385: lash::plugins::ExecutionStateComponentSnapshot::Changed::0 [field]
    field_witness(|value: &lash::plugins::ExecutionStateComponentSnapshot| {
        if let lash::plugins::ExecutionStateComponentSnapshot::Changed(f0) = value {
            let _ = f0;
        }
    });
    // W0386: lash::plugins::ExecutionStateComponentSnapshot::Unchanged [variant]
    variant_witness(|value: &lash::plugins::ExecutionStateComponentSnapshot| {
        matches!(
            value,
            lash::plugins::ExecutionStateComponentSnapshot::Unchanged
        )
    });
    // W0387: lash::plugins::ExecutionStateSnapshot [struct]
    type_witness::<lash::plugins::ExecutionStateSnapshot>();
    // W0388: lash::plugins::ExecutionStateSnapshot::root [field]
    field_witness(|value: &lash::plugins::ExecutionStateSnapshot| {
        let _ = &value.root;
    });
    // W0389: lash::plugins::ExecutionStateSnapshot::components [field]
    field_witness(|value: &lash::plugins::ExecutionStateSnapshot| {
        let _ = &value.components;
    });
    // W0390: lash::plugins::ExecutionStateSnapshot::from_root [function]
    let _ = lash::plugins::ExecutionStateSnapshot::from_root;
    // W0391: lash::plugins::ExecutionStateSnapshot::changed_component [function]
    let _: fn(&mut lash::plugins::ExecutionStateSnapshot, String, std::sync::Arc<[u8]>) =
        lash::plugins::ExecutionStateSnapshot::changed_component;
    // W0392: lash::plugins::ExecutionStateSnapshot::unchanged_component [function]
    let _: fn(&mut lash::plugins::ExecutionStateSnapshot, String) =
        lash::plugins::ExecutionStateSnapshot::unchanged_component;
    // W0393: lash::plugins::ExecutionStateSnapshot::from_hydrated [function]
    let _ = lash::plugins::ExecutionStateSnapshot::from_hydrated;
    // W0394: lash::plugins::ProtocolDriverPlugin [trait]
    fn trait_witness_0394<T: lash::plugins::ProtocolDriverPlugin>() {}
    // W0395: lash::plugins::ProtocolDriverPlugin::build_preamble [function]
    fn meth_0395<T: lash::plugins::ProtocolDriverPlugin>(_: &T) {
        let _ = T::build_preamble;
    }
    // W0396: lash::plugins::ProtocolRuntimeContext [struct]
    type_witness::<lash::plugins::ProtocolRuntimeContext>();
    // W0397: lash::plugins::ProtocolRuntimeContext::protocol_turn_options [function]
    let _ = lash::plugins::ProtocolRuntimeContext::protocol_turn_options;
    // W0398: lash::plugins::ProtocolRuntimeContext::set_protocol_turn_options [function]
    let _ = lash::plugins::ProtocolRuntimeContext::set_protocol_turn_options;
    // W0399: lash::plugins::ProtocolRuntimeContext::set_protocol_turn_options_all_frames [function]
    let _ = lash::plugins::ProtocolRuntimeContext::set_protocol_turn_options_all_frames;
    // W0400: lash::plugins::ProtocolSessionContext [struct]
    type_witness::<lash::plugins::ProtocolSessionContext>();
    // W0401: lash::plugins::ProtocolSessionContext::session_id [function]
    let _ = lash::plugins::ProtocolSessionContext::session_id;
    // W0402: lash::plugins::ProtocolSessionMaterialization [struct]
    type_witness::<lash::plugins::ProtocolSessionMaterialization>();
    // W0403: lash::plugins::ProtocolSessionMaterialization::plugin_options [field]
    field_witness(|value: &lash::plugins::ProtocolSessionMaterialization| {
        let _ = &value.plugin_options;
    });
    // W0404: lash::plugins::ProtocolSessionMaterialization::is_root_session [field]
    field_witness(|value: &lash::plugins::ProtocolSessionMaterialization| {
        let _ = &value.is_root_session;
    });
    // W0405: lash::plugins::ProtocolSessionPlugin [trait]
    fn trait_witness_0405<T: lash::plugins::ProtocolSessionPlugin>() {}
    // W0406: lash::plugins::ProtocolSessionPlugin::initialize_session [function]
    fn meth_0406<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::initialize_session;
    }
    // W0407: lash::plugins::ProtocolSessionPlugin::restore_session [function]
    fn meth_0407<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::restore_session;
    }
    // W0408: lash::plugins::ProtocolSessionPlugin::append_session_nodes [function]
    fn meth_0408<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::append_session_nodes;
    }
    // W0409: lash::plugins::ProtocolSessionPlugin::apply_session_extension [function]
    fn meth_0409<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::apply_session_extension;
    }
    // W0410: lash::plugins::ProtocolSessionPlugin::validate_turn_extension [function]
    fn meth_0410<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::validate_turn_extension;
    }
    // W0411: lash::plugins::ProtocolSessionPlugin::configure_runtime_on_materialize [function]
    fn meth_0411<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::configure_runtime_on_materialize;
    }
    // W0412: lash::plugins::ProtocolSessionPlugin::before_llm_call [function]
    fn meth_0412<T: lash::plugins::ProtocolSessionPlugin>(_: &T) {
        let _ = T::before_llm_call;
    }
}
struct NoopSessionExt;
impl lash_core::ProtocolSessionExtension for NoopSessionExt {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct NoopTurnExt;
impl lash::plugins::ProtocolTurnExtension for NoopTurnExt {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
