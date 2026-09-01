//! Facade-only compile-time witnesses for plugin-area extension contracts.
//!
//! These probes deliberately live in normal example source rather than a test
//! module: an integrator should be able to name and compose every contract through
//! `lash` without reaching into implementation crates.

#![allow(dead_code, unreachable_code, unused_variables)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

pub(crate) fn plugin_area_facade_witnesses() {
    // FIG-2104-WITNESS-0376: lash::tools::PreparedToolCall::replay
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.replay;
    });
    // FIG-2104-WITNESS-0377: lash::tools::PreparedToolCall::tool_id
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.tool_id;
    });
    // FIG-2104-WITNESS-0378: lash::tools::PreparedToolCall::tool_name
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.tool_name;
    });
    // FIG-2104-WITNESS-0379: lash::tools::ToolCall::args
    field_witness(|value: &lash::tools::ToolCall| {
        let _ = &value.args;
    });
    // FIG-2104-WITNESS-0380: lash::tools::ToolCall::context
    field_witness(|value: &lash::tools::ToolCall| {
        let _ = &value.context;
    });
    // FIG-2104-WITNESS-0381: lash::tools::ToolCall::name
    field_witness(|value: &lash::tools::ToolCall| {
        let _ = &value.name;
    });
    // FIG-2104-WITNESS-0382: lash::tools::ToolCallRecord
    type_witness::<lash::tools::ToolCallRecord>();
    // FIG-2104-WITNESS-0383: lash::tools::ToolCallRecord::args
    field_witness(|value: &lash::tools::ToolCallRecord| {
        let _ = &value.args;
    });
    // FIG-2104-WITNESS-0384: lash::tools::ToolCallRecord::call_id
    field_witness(|value: &lash::tools::ToolCallRecord| {
        let _ = &value.call_id;
    });
    // FIG-2104-WITNESS-0385: lash::tools::ToolCallRecord::duration_ms
    field_witness(|value: &lash::tools::ToolCallRecord| {
        let _ = &value.duration_ms;
    });
    // FIG-2104-WITNESS-0386: lash::tools::ToolCallRecord::output
    field_witness(|value: &lash::tools::ToolCallRecord| {
        let _ = &value.output;
    });
    // FIG-2104-WITNESS-0387: lash::tools::ToolCallRecord::tool
    field_witness(|value: &lash::tools::ToolCallRecord| {
        let _ = &value.tool;
    });
    // FIG-2104-WITNESS-0388: lash::tools::ToolCancellation::message
    field_witness(|value: &lash::tools::ToolCancellation| {
        let _ = &value.message;
    });
    // FIG-2104-WITNESS-0389: lash::tools::ToolCancellation::raw
    field_witness(|value: &lash::tools::ToolCancellation| {
        let _ = &value.raw;
    });
    // FIG-2104-WITNESS-0390: lash::tools::ToolCancellation::source
    field_witness(|value: &lash::tools::ToolCancellation| {
        let _ = &value.source;
    });
    // FIG-2104-WITNESS-0391: lash::tools::ToolCancellation::to_json_value
    member_witness(lash::tools::ToolCancellation::to_json_value);
    // FIG-2104-WITNESS-0392: lash::tools::ToolContext::agent_frame_id
    member_witness(lash::tools::ToolContext::agent_frame_id);
    // FIG-2104-WITNESS-0393: lash::tools::ToolContext::attempt_number
    member_witness(lash::tools::ToolContext::attempt_number);
    // FIG-2104-WITNESS-0394: lash::tools::ToolContext::cancellation_token
    member_witness(lash::tools::ToolContext::cancellation_token);
    // FIG-2104-WITNESS-0395: lash::tools::ToolContext::decode_prepared_payload
    member_witness(|context: &lash::tools::ToolContext<'_>| {
        context.decode_prepared_payload::<serde_json::Value>()
    });
    // FIG-2104-WITNESS-0396: lash::tools::ToolContext::direct_completions
    member_witness(lash::tools::ToolContext::direct_completions);
    // FIG-2104-WITNESS-0397: lash::tools::ToolContext::dispatch
    member_witness(lash::tools::ToolContext::dispatch);
    // FIG-2104-WITNESS-0398: lash::tools::ToolContext::max_attempts
    member_witness(lash::tools::ToolContext::max_attempts);
    // FIG-2104-WITNESS-0399: lash::tools::ToolContext::prepared_payload
    member_witness(lash::tools::ToolContext::prepared_payload);
    // FIG-2104-WITNESS-0400: lash::tools::ToolContext::replay_key
    member_witness(lash::tools::ToolContext::replay_key);
    // FIG-2104-WITNESS-0401: lash::tools::ToolContext::session_id
    member_witness(lash::tools::ToolContext::session_id);
    // FIG-2104-WITNESS-0402: lash::tools::ToolContext::sessions
    member_witness(lash::tools::ToolContext::sessions);
    // FIG-2104-WITNESS-0403: lash::tools::ToolContext::tool_call_id
    member_witness(lash::tools::ToolContext::tool_call_id);
    // FIG-2104-WITNESS-0404: lash::tools::ToolContext::tool_execution_binding
    member_witness(lash::tools::ToolContext::tool_execution_binding);
    // FIG-2104-WITNESS-0405: lash::tools::ToolControl::SwitchAgentFrame
    variant_witness(|value: &lash::tools::ToolControl| {
        matches!(value, lash::tools::ToolControl::SwitchAgentFrame { .. })
    });
    // FIG-2104-WITNESS-0406: lash::tools::ToolControl::SwitchAgentFrame::frame_key
    field_witness(|value: &lash::tools::ToolControl| {
        if let lash::tools::ToolControl::SwitchAgentFrame { frame_key, .. } = value {
            let _ = frame_key;
        }
    });
    // FIG-2104-WITNESS-0407: lash::tools::ToolControl::SwitchAgentFrame::initial_nodes
    field_witness(|value: &lash::tools::ToolControl| {
        if let lash::tools::ToolControl::SwitchAgentFrame { initial_nodes, .. } = value {
            let _ = initial_nodes;
        }
    });
    // FIG-2104-WITNESS-0408: lash::tools::ToolControl::SwitchAgentFrame::task
    field_witness(|value: &lash::tools::ToolControl| {
        if let lash::tools::ToolControl::SwitchAgentFrame { task, .. } = value {
            let _ = task;
        }
    });
    // FIG-2104-WITNESS-0409: lash::tools::ToolPrepareCall
    type_witness::<lash::tools::ToolPrepareCall>();
    // FIG-2104-WITNESS-0410: lash::tools::ToolPrepareCall::context
    field_witness(|value: &lash::tools::ToolPrepareCall| {
        let _ = &value.context;
    });
    // FIG-2104-WITNESS-0411: lash::tools::ToolPrepareCall::pending
    field_witness(|value: &lash::tools::ToolPrepareCall| {
        let _ = &value.pending;
    });
    // FIG-2104-WITNESS-0412: lash::tools::ToolPrepareCall::tool_id
    field_witness(|value: &lash::tools::ToolPrepareCall| {
        let _ = &value.tool_id;
    });
    // FIG-2104-WITNESS-0413: lash::tools::ToolPrepareContext
    type_witness::<lash::tools::ToolPrepareContext>();
    // FIG-2104-WITNESS-0414: lash::tools::ToolPrepareContext::plugin_input
    member_witness(
        |context: &lash::tools::ToolPrepareContext, plugin_id: &'static str| {
            let _ = context.plugin_input::<serde_json::Value>(plugin_id);
        },
    );
    // FIG-2104-WITNESS-0415: lash::tools::ToolPrepareContext::session_id
    member_witness(lash::tools::ToolPrepareContext::session_id);
    // FIG-2104-WITNESS-0416: lash::tools::ToolPrepareContext::session_snapshot
    member_witness(lash::tools::ToolPrepareContext::session_snapshot);
    // FIG-2104-WITNESS-0417: lash::tools::ToolPrepareContext::shared_tool_catalog
    member_witness(lash::tools::ToolPrepareContext::shared_tool_catalog);
    // FIG-2104-WITNESS-0418: lash::tools::ToolPrepareContext::tool_call_id
    member_witness(lash::tools::ToolPrepareContext::tool_call_id);
    // FIG-2104-WITNESS-0419: lash::tools::ToolPrepareContext::tool_catalog
    member_witness(lash::tools::ToolPrepareContext::tool_catalog);
    // FIG-2104-WITNESS-0420: lash::tools::ToolPrepareContext::tool_execution_binding
    member_witness(lash::tools::ToolPrepareContext::tool_execution_binding);
    // FIG-2104-WITNESS-0421: lash::tools::ToolPrepareContext::turn_context
    member_witness(lash::tools::ToolPrepareContext::turn_context);
    // FIG-2104-WITNESS-0422: lash::tools::ToolProvider::execute
    fn method_witness_0422<T: lash::tools::ToolProvider>() {
        member_witness(T::execute);
    }
    // FIG-2104-WITNESS-0423: lash::tools::ToolProvider::execute_by_id
    fn method_witness_0423<T: lash::tools::ToolProvider>() {
        member_witness(T::execute_by_id);
    }
    // FIG-2104-WITNESS-0424: lash::tools::ToolProvider::execute_granted
    fn method_witness_0424<T: lash::tools::ToolProvider>() {
        member_witness(T::execute_granted);
    }
    // FIG-2104-WITNESS-0425: lash::tools::ToolProvider::prepare_granted_tool_call
    fn method_witness_0425<T: lash::tools::ToolProvider>() {
        member_witness(T::prepare_granted_tool_call);
    }
    // FIG-2104-WITNESS-0426: lash::tools::ToolProvider::prepare_tool_call
    fn method_witness_0426<T: lash::tools::ToolProvider>() {
        member_witness(T::prepare_tool_call);
    }
    // FIG-2104-WITNESS-0427: lash::tools::ToolProvider::resolve_contract
    fn method_witness_0427<T: lash::tools::ToolProvider>() {
        member_witness(T::resolve_contract);
    }
    // FIG-2104-WITNESS-0428: lash::tools::ToolProvider::resolve_contract_by_id
    fn method_witness_0428<T: lash::tools::ToolProvider>() {
        member_witness(T::resolve_contract_by_id);
    }
    // FIG-2104-WITNESS-0429: lash::tools::ToolProvider::resolve_manifest
    fn method_witness_0429<T: lash::tools::ToolProvider>() {
        member_witness(T::resolve_manifest);
    }
    // FIG-2104-WITNESS-0430: lash::tools::ToolProvider::resolve_manifest_by_id
    fn method_witness_0430<T: lash::tools::ToolProvider>() {
        member_witness(T::resolve_manifest_by_id);
    }
    // FIG-2104-WITNESS-0431: lash::tools::ToolProvider::tool_manifests
    fn method_witness_0431<T: lash::tools::ToolProvider>() {
        member_witness(T::tool_manifests);
    }
    // FIG-2104-WITNESS-0432: lash::tools::ToolRetryStatus::Exhausted
    variant_witness(|value: &lash::tools::ToolRetryStatus| {
        matches!(value, lash::tools::ToolRetryStatus::Exhausted { .. })
    });
    // FIG-2104-WITNESS-0433: lash::tools::ToolRetryStatus::Exhausted::attempts
    field_witness(|value: &lash::tools::ToolRetryStatus| {
        if let lash::tools::ToolRetryStatus::Exhausted { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // FIG-2104-WITNESS-0434: lash::tools::ToolRetryStatus::Never
    variant_witness(|value: &lash::tools::ToolRetryStatus| {
        matches!(value, lash::tools::ToolRetryStatus::Never)
    });
    // FIG-2104-WITNESS-0435: lash::tools::ToolState::is_empty
    member_witness(lash::tools::ToolState::is_empty);
    // FIG-2104-WITNESS-0436: lash::tools::ToolState::iter
    member_witness(lash::tools::ToolState::iter);
    // FIG-2104-WITNESS-0437: lash::tools::ToolState::len
    member_witness(lash::tools::ToolState::len);
    // FIG-2104-WITNESS-0438: lash::tools::ToolState::manifest_mut
    member_witness(lash::tools::ToolState::manifest_mut);
    // FIG-2104-WITNESS-0439: lash::tools::ToolState::remove
    member_witness(lash::tools::ToolState::remove);
    // FIG-2104-WITNESS-0440: lash::tools::ToolStateEntry::is_member
    member_witness(lash::tools::ToolStateEntry::is_member);
    // FIG-2104-WITNESS-0441: lash::tools::ToolStateEntry::manifest
    member_witness(lash::tools::ToolStateEntry::manifest);
    // FIG-2104-WITNESS-0442: lash::tools::ToolValue::Array
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::Array(..))
    });
    // FIG-2104-WITNESS-0443: lash::tools::ToolValue::Array::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::Array(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0444: lash::tools::ToolValue::Attachment
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::Attachment(..))
    });
    // FIG-2104-WITNESS-0445: lash::tools::ToolValue::Attachment::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::Attachment(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0446: lash::tools::ToolValue::Bool
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::Bool(..))
    });
    // FIG-2104-WITNESS-0447: lash::tools::ToolValue::Bool::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::Bool(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0448: lash::tools::ToolValue::Null
    variant_witness(|value: &lash::tools::ToolValue| matches!(value, lash::tools::ToolValue::Null));
    // FIG-2104-WITNESS-0449: lash::tools::ToolValue::Number
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::Number(..))
    });
    // FIG-2104-WITNESS-0450: lash::tools::ToolValue::Number::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::Number(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0451: lash::tools::ToolValue::Object
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::Object(..))
    });
    // FIG-2104-WITNESS-0452: lash::tools::ToolValue::Object::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::Object(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0453: lash::tools::ToolValue::UntrustedJson
    variant_witness(|value: &lash::tools::ToolValue| {
        matches!(value, lash::tools::ToolValue::UntrustedJson(..))
    });
    // FIG-2104-WITNESS-0454: lash::tools::ToolValue::UntrustedJson::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::UntrustedJson(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0455: lash::tools::ToolValue::String::0
    field_witness(|value: &lash::tools::ToolValue| {
        if let lash::tools::ToolValue::String(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0456: lash::tools::ToolValue::attachments
    member_witness(lash::tools::ToolValue::attachments);
    // FIG-2104-WITNESS-0457: lash::turn::TurnIssue::provider_failure_kind
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.provider_failure_kind;
    });
    // FIG-2104-WITNESS-0458: lash::plugins::AgentFrameAssignment::plugin_options
    field_witness(|value: &lash::plugins::AgentFrameAssignment| {
        let _ = &value.plugin_options;
    });
    // FIG-2104-WITNESS-0459: lash::tools::CompactToolContract
    type_witness::<lash::tools::CompactToolContract>();
    // FIG-2104-WITNESS-0460: lash::tools::CompactToolContract::description
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.description;
    });
    // FIG-2104-WITNESS-0461: lash::tools::CompactToolContract::examples
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.examples;
    });
    // FIG-2104-WITNESS-0462: lash::tools::CompactToolContract::name
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.name;
    });
    // FIG-2104-WITNESS-0463: lash::tools::CompactToolContract::parameters
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.parameters;
    });
    // FIG-2104-WITNESS-0464: lash::tools::CompactToolContract::render_markdown
    member_witness(lash::tools::CompactToolContract::render_markdown);
    // FIG-2104-WITNESS-0465: lash::tools::CompactToolContract::render_signature
    member_witness(lash::tools::CompactToolContract::render_signature);
    // FIG-2104-WITNESS-0466: lash::tools::CompactToolContract::return_fields
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.return_fields;
    });
    // FIG-2104-WITNESS-0467: lash::tools::CompactToolContract::returns
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.returns;
    });
    // FIG-2104-WITNESS-0468: lash::tools::CompactToolContract::signature
    field_witness(|value: &lash::tools::CompactToolContract| {
        let _ = &value.signature;
    });
    // FIG-2104-WITNESS-0469: lash::plugins::ExecResponse::tool_calls
    field_witness(|value: &lash::plugins::ExecResponse| {
        let _ = &value.calls;
    });
    // FIG-2104-WITNESS-0470: lash::provider::NormalizedError::provider_code
    field_witness(|value: &lash::provider::NormalizedError| {
        let _ = &value.provider_code;
    });
    // FIG-2104-WITNESS-0471: lash::provider::NormalizedError::provider_request_id
    field_witness(|value: &lash::provider::NormalizedError| {
        let _ = &value.provider_request_id;
    });
    // FIG-2104-WITNESS-0472: lash::messages::Part::tool_call_id
    field_witness(|value: &lash::messages::Part| {
        let _ = &value.tool_call_id;
    });
    // FIG-2104-WITNESS-0473: lash::messages::Part::tool_name
    field_witness(|value: &lash::messages::Part| {
        let _ = &value.tool_name;
    });
    // FIG-2104-WITNESS-0474: lash::messages::Part::tool_replay
    field_witness(|value: &lash::messages::Part| {
        let _ = &value.tool_replay;
    });
    // FIG-2104-WITNESS-0475: lash::messages::Part::tool_call
    member_witness(lash::messages::Part::tool_call);
    // FIG-2104-WITNESS-0476: lash::messages::Part::tool_result
    member_witness(lash::messages::Part::tool_result);
    // FIG-2104-WITNESS-0477: lash::messages::PartKind::ToolCall
    variant_witness(|value: &lash::messages::PartKind| {
        matches!(value, lash::messages::PartKind::ToolCall)
    });
    // FIG-2104-WITNESS-0478: lash::messages::PartKind::ToolResult
    variant_witness(|value: &lash::messages::PartKind| {
        matches!(value, lash::messages::PartKind::ToolResult)
    });
    // FIG-2104-WITNESS-0479: lash::plugins::PluginSessionSnapshot
    type_witness::<lash::plugins::PluginSessionSnapshot>();
    // FIG-2104-WITNESS-0480: lash::plugins::PluginSessionSnapshot::plugins
    field_witness(|value: &lash::plugins::PluginSessionSnapshot| {
        let _ = &value.plugins;
    });
    // FIG-2104-WITNESS-0481: lash::plugins::PluginSnapshotArtifact
    type_witness::<lash::plugins::PluginSnapshotArtifact>();
    // FIG-2104-WITNESS-0482: lash::plugins::PluginSnapshotArtifact::data
    field_witness(|value: &lash::plugins::PluginSnapshotArtifact| {
        let _ = &value.data;
    });
    // FIG-2104-WITNESS-0483: lash::plugins::PluginSnapshotArtifact::name
    field_witness(|value: &lash::plugins::PluginSnapshotArtifact| {
        let _ = &value.name;
    });
    // FIG-2104-WITNESS-0484: lash::plugins::PluginSnapshotEntry
    type_witness::<lash::plugins::PluginSnapshotEntry>();
    // FIG-2104-WITNESS-0485: lash::plugins::PluginSnapshotEntry::artifacts
    field_witness(|value: &lash::plugins::PluginSnapshotEntry| {
        let _ = &value.artifacts;
    });
    // FIG-2104-WITNESS-0486: lash::plugins::PluginSnapshotEntry::meta
    field_witness(|value: &lash::plugins::PluginSnapshotEntry| {
        let _ = &value.meta;
    });
    // FIG-2104-WITNESS-0487: lash::tools::PreparedToolBatch
    type_witness::<lash::tools::PreparedToolBatch>();
    // FIG-2104-WITNESS-0488: lash::tools::PreparedToolBatch::batch_id
    field_witness(|value: &lash::tools::PreparedToolBatch| {
        let _ = &value.batch_id;
    });
    // FIG-2104-WITNESS-0489: lash::tools::PreparedToolBatch::calls
    field_witness(|value: &lash::tools::PreparedToolBatch| {
        let _ = &value.calls;
    });
    // FIG-2104-WITNESS-0490: lash::tools::PreparedToolBatch::new
    member_witness(
        |batch_id: String, calls: Vec<lash::tools::PreparedToolCall>| {
            lash::tools::PreparedToolBatch::new(batch_id, calls)
        },
    );
    // FIG-2104-WITNESS-0491: lash::tools::PreparedToolBatchCall
    type_witness::<lash::tools::PreparedToolBatchCall>();
    // FIG-2104-WITNESS-0492: lash::tools::PreparedToolBatchCall::call
    field_witness(|value: &lash::tools::PreparedToolBatchCall| {
        let _ = &value.call;
    });
    // FIG-2104-WITNESS-0493: lash::tools::PreparedToolBatchCall::execution_grant
    field_witness(|value: &lash::tools::PreparedToolBatchCall| {
        let _ = &value.execution_grant;
    });
    // FIG-2104-WITNESS-0494: lash::tools::PreparedToolBatchCall::replay_suffix
    field_witness(|value: &lash::tools::PreparedToolBatchCall| {
        let _ = &value.replay_suffix;
    });
    // FIG-2104-WITNESS-0495: lash::prompt::PromptSlotLayer
    type_witness::<lash::prompt::PromptSlotLayer>();
    // FIG-2104-WITNESS-0496: lash::prompt::PromptSlotLayer::contributions
    field_witness(|value: &lash::prompt::PromptSlotLayer| {
        let _ = &value.contributions;
    });
    // FIG-2104-WITNESS-0497: lash::prompt::PromptSlotLayer::reset
    field_witness(|value: &lash::prompt::PromptSlotLayer| {
        let _ = &value.reset;
    });
    // FIG-2104-WITNESS-0498: lash::plugins::ProtocolBeforeLlmCallContext::latest_prompt_usage
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.latest_prompt_usage;
    });
    // FIG-2104-WITNESS-0499: lash::plugins::ProtocolBuildInput::extra_prompt_contributions
    field_witness(|value: &lash::plugins::ProtocolBuildInput| {
        let _ = &value.extra_prompt_contributions;
    });
    // FIG-2104-WITNESS-0500: lash::plugins::ProtocolBuildInput::plugin_extensions
    field_witness(|value: &lash::plugins::ProtocolBuildInput| {
        let _ = &value.plugin_extensions;
    });
    // FIG-2104-WITNESS-0501: lash::plugins::ProtocolBuildInput::tool_catalog
    field_witness(|value: &lash::plugins::ProtocolBuildInput| {
        let _ = &value.tool_catalog;
    });
    // FIG-2104-WITNESS-0502: lash::plugins::ProtocolDriverState::plugin_id
    field_witness(|value: &lash::plugins::ProtocolDriverState| {
        let _ = &value.plugin_id;
    });
    // FIG-2104-WITNESS-0503: lash::plugins::ProtocolTurnExtension::prompt_contributions
    fn method_witness_0503<T: lash::plugins::ProtocolTurnExtension>() {
        member_witness(T::prompt_contributions);
    }
    // FIG-2104-WITNESS-0504: lash::runtime::ProtocolTurnExtensionHandle::prompt_contributions
    member_witness(lash::runtime::ProtocolTurnExtensionHandle::prompt_contributions);
    // FIG-2104-WITNESS-0505: lash::plugins::RuntimeExecutionContext::await_tool_handle
    member_witness(lash::plugins::RuntimeExecutionContext::await_tool_handle);
    // FIG-2104-WITNESS-0506: lash::plugins::RuntimeExecutionContext::call_tool_batch
    member_witness(lash::plugins::RuntimeExecutionContext::call_tool_batch);
    // FIG-2104-WITNESS-0507: lash::plugins::RuntimeExecutionContext::call_tool_by_id
    member_witness(lash::plugins::RuntimeExecutionContext::call_tool_by_id);
    // FIG-2104-WITNESS-0508: lash::plugins::RuntimeExecutionContext::call_tool_by_id_with_child_execution_trace_hook
    member_witness(
        lash::plugins::RuntimeExecutionContext::call_tool_by_id_with_child_execution_trace_hook,
    );
    // FIG-2104-WITNESS-0509: lash::plugins::RuntimeExecutionContext::call_tool_with_execution_grant
    member_witness(lash::plugins::RuntimeExecutionContext::call_tool_with_execution_grant);
    // FIG-2104-WITNESS-0510: lash::plugins::RuntimeExecutionContext::call_tool_with_execution_grant_and_child_execution_trace_hook
    member_witness(lash::plugins::RuntimeExecutionContext::call_tool_with_execution_grant_and_child_execution_trace_hook);
    // FIG-2104-WITNESS-0511: lash::plugins::RuntimeExecutionContext::callable_tool_manifest_by_id
    member_witness(lash::plugins::RuntimeExecutionContext::callable_tool_manifest_by_id);
    // FIG-2104-WITNESS-0512: lash::plugins::RuntimeExecutionContext::cancel_tool_handle
    member_witness(lash::plugins::RuntimeExecutionContext::cancel_tool_handle);
    // FIG-2104-WITNESS-0513: lash::plugins::RuntimeExecutionContext::named_phase
    member_witness(lash::plugins::RuntimeExecutionContext::named_phase);
    // FIG-2104-WITNESS-0514: lash::plugins::RuntimeExecutionContext::signal_tool_handle
    member_witness(lash::plugins::RuntimeExecutionContext::signal_tool_handle);
    // FIG-2104-WITNESS-0515: lash::plugins::RuntimeExecutionContext::tool_catalog
    member_witness(lash::plugins::RuntimeExecutionContext::tool_catalog);
    // FIG-2104-WITNESS-0516: lash::plugins::SessionContextOverlay::include_base_tools
    field_witness(|value: &lash::plugins::SessionContextOverlay| {
        let _ = &value.include_base_tools;
    });
    // FIG-2104-WITNESS-0517: lash::plugins::SessionContextOverlay::prompt_contributions
    field_witness(|value: &lash::plugins::SessionContextOverlay| {
        let _ = &value.prompt_contributions;
    });
    // FIG-2104-WITNESS-0518: lash::plugins::SessionContextOverlay::tool_providers
    field_witness(|value: &lash::plugins::SessionContextOverlay| {
        let _ = &value.tool_providers;
    });
    // FIG-2104-WITNESS-0519: lash::persistence::SessionNodePayload::Plugin
    variant_witness(|value: &lash::persistence::SessionNodePayload| {
        matches!(value, lash::persistence::SessionNodePayload::Plugin { .. })
    });
    // FIG-2104-WITNESS-0520: lash::persistence::SessionNodePayload::Plugin::body
    field_witness(|value: &lash::persistence::SessionNodePayload| {
        if let lash::persistence::SessionNodePayload::Plugin { body, .. } = value {
            let _ = body;
        }
    });
    // FIG-2104-WITNESS-0521: lash::persistence::SessionNodePayload::Plugin::plugin_type
    field_witness(|value: &lash::persistence::SessionNodePayload| {
        if let lash::persistence::SessionNodePayload::Plugin { plugin_type, .. } = value {
            let _ = plugin_type;
        }
    });
    // FIG-2104-WITNESS-0522: lash::plugins::SessionPluginSource
    type_witness::<lash::plugins::SessionPluginSource>();
    // FIG-2104-WITNESS-0523: lash::plugins::SessionPluginSource::CurrentHostFresh
    variant_witness(|value: &lash::plugins::SessionPluginSource| {
        matches!(value, lash::plugins::SessionPluginSource::CurrentHostFresh)
    });
    // FIG-2104-WITNESS-0524: lash::plugins::SessionPluginSource::CurrentSessionFork
    variant_witness(|value: &lash::plugins::SessionPluginSource| {
        matches!(
            value,
            lash::plugins::SessionPluginSource::CurrentSessionFork
        )
    });
    // FIG-2104-WITNESS-0525: lash::durability::ToolAttemptLaunch
    type_witness::<lash::durability::ToolAttemptLaunch>();
    // FIG-2104-WITNESS-0526: lash::durability::ToolAttemptLaunch::Done
    variant_witness(|value: &lash::durability::ToolAttemptLaunch| {
        matches!(value, lash::durability::ToolAttemptLaunch::Done { .. })
    });
    // FIG-2104-WITNESS-0527: lash::durability::ToolAttemptLaunch::Done::record
    field_witness(|value: &lash::durability::ToolAttemptLaunch| {
        if let lash::durability::ToolAttemptLaunch::Done { record, .. } = value {
            let _ = record;
        }
    });
    // FIG-2104-WITNESS-0528: lash::durability::ToolAttemptLaunch::Pending
    variant_witness(|value: &lash::durability::ToolAttemptLaunch| {
        matches!(value, lash::durability::ToolAttemptLaunch::Pending { .. })
    });
    // FIG-2104-WITNESS-0529: lash::durability::ToolAttemptLaunch::Pending::duration_ms
    field_witness(|value: &lash::durability::ToolAttemptLaunch| {
        if let lash::durability::ToolAttemptLaunch::Pending { duration_ms, .. } = value {
            let _ = duration_ms;
        }
    });
    // FIG-2104-WITNESS-0530: lash::durability::ToolAttemptLaunch::Pending::key
    field_witness(|value: &lash::durability::ToolAttemptLaunch| {
        if let lash::durability::ToolAttemptLaunch::Pending { key, .. } = value {
            let _ = key;
        }
    });
    // FIG-2104-WITNESS-0531: lash::durability::ToolAttemptLaunch::Pending::pending
    field_witness(|value: &lash::durability::ToolAttemptLaunch| {
        if let lash::durability::ToolAttemptLaunch::Pending { pending, .. } = value {
            let _ = pending;
        }
    });
    // FIG-2104-WITNESS-0532: lash::durability::ToolCallLaunch
    type_witness::<lash::durability::ToolCallLaunch>();
    // FIG-2104-WITNESS-0533: lash::durability::ToolCallLaunch::Done
    variant_witness(|value: &lash::durability::ToolCallLaunch| {
        matches!(value, lash::durability::ToolCallLaunch::Done { .. })
    });
    // FIG-2104-WITNESS-0534: lash::durability::ToolCallLaunch::Done::result
    field_witness(|value: &lash::durability::ToolCallLaunch| {
        if let lash::durability::ToolCallLaunch::Done { result, .. } = value {
            let _ = result;
        }
    });
    // FIG-2104-WITNESS-0535: lash::durability::ToolCallLaunch::Pending
    variant_witness(|value: &lash::durability::ToolCallLaunch| {
        matches!(value, lash::durability::ToolCallLaunch::Pending { .. })
    });
    // FIG-2104-WITNESS-0536: lash::durability::ToolCallLaunch::Pending::duration_ms
    field_witness(|value: &lash::durability::ToolCallLaunch| {
        if let lash::durability::ToolCallLaunch::Pending { duration_ms, .. } = value {
            let _ = duration_ms;
        }
    });
    // FIG-2104-WITNESS-0537: lash::durability::ToolCallLaunch::Pending::key
    field_witness(|value: &lash::durability::ToolCallLaunch| {
        if let lash::durability::ToolCallLaunch::Pending { key, .. } = value {
            let _ = key;
        }
    });
    // FIG-2104-WITNESS-0538: lash::durability::ToolCallLaunch::Pending::pending
    field_witness(|value: &lash::durability::ToolCallLaunch| {
        if let lash::durability::ToolCallLaunch::Pending { pending, .. } = value {
            let _ = pending;
        }
    });
    // FIG-2104-WITNESS-0539: lash::tools::ToolCallOutcome
    type_witness::<lash::tools::ToolCallOutcome>();
    // FIG-2104-WITNESS-0540: lash::tools::ToolCallOutcome::Cancelled
    variant_witness(|value: &lash::tools::ToolCallOutcome| {
        matches!(value, lash::tools::ToolCallOutcome::Cancelled(..))
    });
    // FIG-2104-WITNESS-0541: lash::tools::ToolCallOutcome::Cancelled::0
    field_witness(|value: &lash::tools::ToolCallOutcome| {
        if let lash::tools::ToolCallOutcome::Cancelled(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0542: lash::tools::ToolCallOutcome::Failure
    variant_witness(|value: &lash::tools::ToolCallOutcome| {
        matches!(value, lash::tools::ToolCallOutcome::Failure(..))
    });
    // FIG-2104-WITNESS-0543: lash::tools::ToolCallOutcome::Failure::0
    field_witness(|value: &lash::tools::ToolCallOutcome| {
        if let lash::tools::ToolCallOutcome::Failure(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0544: lash::tools::ToolCallOutcome::Success
    variant_witness(|value: &lash::tools::ToolCallOutcome| {
        matches!(value, lash::tools::ToolCallOutcome::Success(..))
    });
    // FIG-2104-WITNESS-0545: lash::tools::ToolCallOutcome::Success::0
    field_witness(|value: &lash::tools::ToolCallOutcome| {
        if let lash::tools::ToolCallOutcome::Success(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0546: lash::tools::ToolCatalogEntry
    type_witness::<lash::tools::ToolCatalogEntry>();
    // FIG-2104-WITNESS-0547: lash::tools::ToolCatalogEntry::manifest
    field_witness(|value: &lash::tools::ToolCatalogEntry| {
        let _ = &value.manifest;
    });
    // FIG-2104-WITNESS-0548: lash::tools::ToolRegistry
    type_witness::<lash::tools::ToolRegistry>();
    // FIG-2104-WITNESS-0549: lash::tools::ToolRegistry::from_tool_provider
    member_witness(lash::tools::ToolRegistry::from_tool_provider);
    // FIG-2104-WITNESS-0550: lash::tools::OrchestratingToolDef
    type_witness::<lash::tools::OrchestratingToolDef>();
    // FIG-2104-WITNESS-0551: lash::tools::OrchestratingToolDef::from_first_party
    member_witness(lash::tools::OrchestratingToolDef::from_first_party);
    // FIG-2104-WITNESS-0552: lash::tools::OrchestrationContext::sessions
    member_witness(lash::tools::OrchestrationContext::sessions);
    // FIG-2104-WITNESS-0553: lash::tools::OrchestrationContext::triggers
    member_witness(lash::tools::OrchestrationContext::triggers);
    // FIG-2104-WITNESS-0554: lash::tools::OrchestrationContext::session_id
    member_witness(lash::tools::OrchestrationContext::session_id);
    // FIG-2104-WITNESS-0555: lash::tools::ToolRegistryFacadeOps
    fn trait_witness_0555<T: lash::tools::ToolRegistryFacadeOps>() {}
    // FIG-2104-WITNESS-0556: lash::tools::ToolRegistryFacadeOps::add_tool_provider
    fn method_witness_0556<T: lash::tools::ToolRegistryFacadeOps>() {
        member_witness(T::add_tool_provider);
    }
    // FIG-2104-WITNESS-0557: lash::tools::ToolRegistryFacadeOps::remove_source
    fn method_witness_0557<T: lash::tools::ToolRegistryFacadeOps>() {
        member_witness(T::remove_source);
    }
    // FIG-2104-WITNESS-0558: lash::tools::ToolStateFacadeOps
    fn trait_witness_0558<T: lash::tools::ToolStateFacadeOps>() {}
    // FIG-2104-WITNESS-0559: lash::tools::ToolStateFacadeOps::get
    fn method_witness_0559<T: lash::tools::ToolStateFacadeOps>() {
        member_witness(T::get);
    }
    // FIG-2104-WITNESS-0560: lash::tools::ToolStateFacadeOps::set_membership
    fn method_witness_0560<T: lash::tools::ToolStateFacadeOps>() {
        member_witness(T::set_membership);
    }
    // FIG-2104-WITNESS-0564: lash::plugins::PluginOperationOutcome::events
    field_witness(
        |value: &lash::plugins::PluginOperationOutcome<serde_json::Value>| {
            let _ = &value.events;
        },
    );
    // FIG-2104-WITNESS-0565: lash::plugins::PluginOperationOutcome::output
    field_witness(
        |value: &lash::plugins::PluginOperationOutcome<serde_json::Value>| {
            let _ = &value.output;
        },
    );
    // FIG-2104-WITNESS-0566: lash::plugins::SessionParam::Forbidden
    variant_witness(|value: &lash::plugins::SessionParam| {
        matches!(value, lash::plugins::SessionParam::Forbidden)
    });
    // FIG-2104-WITNESS-0567: lash::plugins::TurnHookReport::assistant_output
    field_witness(|value: &lash::plugins::TurnHookReport| {
        let _ = &value.assistant_output;
    });
    // FIG-2104-WITNESS-0568: lash::plugins::TurnHookReport::errors
    field_witness(|value: &lash::plugins::TurnHookReport| {
        let _ = &value.errors;
    });
    // FIG-2104-WITNESS-0569: lash::plugins::TurnHookReport::execution
    field_witness(|value: &lash::plugins::TurnHookReport| {
        let _ = &value.execution;
    });
    // FIG-2104-WITNESS-0570: lash::plugins::TurnHookReport::token_usage
    field_witness(|value: &lash::plugins::TurnHookReport| {
        let _ = &value.token_usage;
    });
    // FIG-2104-WITNESS-0571: lash::plugins::TurnHookReport::tool_calls
    field_witness(|value: &lash::plugins::TurnHookReport| {
        let _ = &value.tool_calls;
    });
    // FIG-2104-WITNESS-0572: lash::plugins::SessionAuthorityContext
    type_witness::<lash::plugins::SessionAuthorityContext>();
    // FIG-2104-WITNESS-0573: lash::plugins::PluginOperationInvokeError::Failed
    variant_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        matches!(value, lash::plugins::PluginOperationInvokeError::Failed(..))
    });
    // FIG-2104-WITNESS-0574: lash::plugins::PluginOperationInvokeError::Failed::0
    field_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        if let lash::plugins::PluginOperationInvokeError::Failed(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0575: lash::plugins::PluginOperationInvokeError::MissingSession
    variant_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        matches!(
            value,
            lash::plugins::PluginOperationInvokeError::MissingSession(..)
        )
    });
    // FIG-2104-WITNESS-0576: lash::plugins::PluginOperationInvokeError::MissingSession::0
    field_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        if let lash::plugins::PluginOperationInvokeError::MissingSession(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0577: lash::plugins::PluginOperationInvokeError::UnexpectedSession
    variant_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        matches!(
            value,
            lash::plugins::PluginOperationInvokeError::UnexpectedSession(..)
        )
    });
    // FIG-2104-WITNESS-0578: lash::plugins::PluginOperationInvokeError::UnexpectedSession::0
    field_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        if let lash::plugins::PluginOperationInvokeError::UnexpectedSession(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0579: lash::plugins::PluginOperationInvokeError::UnknownSession
    variant_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        matches!(
            value,
            lash::plugins::PluginOperationInvokeError::UnknownSession(..)
        )
    });
    // FIG-2104-WITNESS-0580: lash::plugins::PluginOperationInvokeError::UnknownSession::0
    field_witness(|value: &lash::plugins::PluginOperationInvokeError| {
        if let lash::plugins::PluginOperationInvokeError::UnknownSession(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0581: lash::plugins::CheckpointApplication
    type_witness::<lash::plugins::CheckpointApplication>();
    // FIG-2104-WITNESS-0582: lash::plugins::CheckpointApplication::abort
    field_witness(|value: &lash::plugins::CheckpointApplication| {
        let _ = &value.abort;
    });
    // FIG-2104-WITNESS-0583: lash::plugins::CheckpointApplication::events
    field_witness(|value: &lash::plugins::CheckpointApplication| {
        let _ = &value.events;
    });
    // FIG-2104-WITNESS-0584: lash::plugins::CheckpointApplication::messages
    field_witness(|value: &lash::plugins::CheckpointApplication| {
        let _ = &value.messages;
    });
    // FIG-2104-WITNESS-0585: lash::plugins::PluginAbort
    type_witness::<lash::plugins::PluginAbort>();
    // FIG-2104-WITNESS-0586: lash::plugins::PluginAbort::code
    field_witness(|value: &lash::plugins::PluginAbort| {
        let _ = &value.code;
    });
    // FIG-2104-WITNESS-0587: lash::plugins::PluginAbort::message
    field_witness(|value: &lash::plugins::PluginAbort| {
        let _ = &value.message;
    });
    // FIG-2104-WITNESS-0588: lash::plugins::PrepareTurnRequest
    type_witness::<lash::plugins::PrepareTurnRequest>();
    // FIG-2104-WITNESS-0589: lash::plugins::PrepareTurnRequest::messages
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.messages;
    });
    // FIG-2104-WITNESS-0590: lash::plugins::PrepareTurnRequest::session_graph
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.session_graph;
    });
    // FIG-2104-WITNESS-0591: lash::plugins::PrepareTurnRequest::session_id
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0592: lash::plugins::PrepareTurnRequest::session_lifecycle
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.session_lifecycle;
    });
    // FIG-2104-WITNESS-0593: lash::plugins::PrepareTurnRequest::sessions
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.sessions;
    });
    // FIG-2104-WITNESS-0594: lash::plugins::PrepareTurnRequest::state
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.state;
    });
    // FIG-2104-WITNESS-0595: lash::plugins::PrepareTurnRequest::turn_context
    field_witness(|value: &lash::plugins::PrepareTurnRequest| {
        let _ = &value.turn_context;
    });
    // FIG-2104-WITNESS-0596: lash::plugins::TurnFinalization
    type_witness::<lash::plugins::TurnFinalization>();
    // FIG-2104-WITNESS-0597: lash::plugins::TurnFinalization::events
    field_witness(|value: &lash::plugins::TurnFinalization| {
        let _ = &value.events;
    });
    // FIG-2104-WITNESS-0598: lash::plugins::TurnFinalization::turn
    field_witness(|value: &lash::plugins::TurnFinalization| {
        let _ = &value.turn;
    });
    // FIG-2104-WITNESS-0599: lash::plugins::TurnPreparation
    type_witness::<lash::plugins::TurnPreparation>();
    // FIG-2104-WITNESS-0600: lash::plugins::TurnPreparation::abort
    field_witness(|value: &lash::plugins::TurnPreparation| {
        let _ = &value.abort;
    });
    // FIG-2104-WITNESS-0601: lash::plugins::TurnPreparation::events
    field_witness(|value: &lash::plugins::TurnPreparation| {
        let _ = &value.events;
    });
    // FIG-2104-WITNESS-0602: lash::plugins::TurnPreparation::messages
    field_witness(|value: &lash::plugins::TurnPreparation| {
        let _ = &value.messages;
    });
    // FIG-2104-WITNESS-0603: lash::tools::ToolTriggerEffectOutcome
    type_witness::<lash::tools::ToolTriggerEffectOutcome>();
    // FIG-2104-WITNESS-0604: lash::tools::ToolTriggerEffectOutcome::deliveries
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.deliveries;
    });
    // FIG-2104-WITNESS-0605: lash::tools::ToolTriggerEffectOutcome::idempotency_key
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.idempotency_key;
    });
    // FIG-2104-WITNESS-0606: lash::tools::ToolTriggerEffectOutcome::occurrence_id
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.occurrence_id;
    });
    // FIG-2104-WITNESS-0607: lash::tools::ToolTriggerEffectOutcome::payload
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.payload;
    });
    // FIG-2104-WITNESS-0608: lash::tools::ToolTriggerEffectOutcome::source
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.source;
    });
    // FIG-2104-WITNESS-0609: lash::tools::ToolTriggerEffectOutcome::source_key
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.source_key;
    });
    // FIG-2104-WITNESS-0610: lash::tools::ToolTriggerEffectOutcome::source_type
    field_witness(|value: &lash::tools::ToolTriggerEffectOutcome| {
        let _ = &value.source_type;
    });
    // FIG-2104-WITNESS-0611: lash::tools::ToolChildExecutionTraceHook
    type_witness::<lash::tools::ToolChildExecutionTraceHook>();
    // FIG-2104-WITNESS-0612: lash::tools::ToolChildExecutionTraceHook::child_process_started
    member_witness(lash::tools::ToolChildExecutionTraceHook::child_process_started);
    // FIG-2104-WITNESS-0613: lash::tools::ToolChildExecutionTraceHook::new
    member_witness(|| lash::tools::ToolChildExecutionTraceHook::new(|_| {}));
    // FIG-2104-WITNESS-0614: lash::tools::ToolChildProcessStarted
    type_witness::<lash::tools::ToolChildProcessStarted>();
    // FIG-2104-WITNESS-0615: lash::tools::ToolChildProcessStarted::child_entry_name
    field_witness(|value: &lash::tools::ToolChildProcessStarted| {
        let _ = &value.child_entry_name;
    });
    // FIG-2104-WITNESS-0616: lash::tools::ToolChildProcessStarted::process_id
    field_witness(|value: &lash::tools::ToolChildProcessStarted| {
        let _ = &value.process_id;
    });
    // FIG-2104-WITNESS-0617: lash::tools::ToolAttachmentClient
    type_witness::<lash::tools::ToolAttachmentClient>();
    // FIG-2104-WITNESS-0618: lash::tools::ToolDirectCompletionClient
    type_witness::<lash::tools::ToolDirectCompletionClient>();
    // FIG-2104-WITNESS-0619: lash::tools::ToolDispatchClient
    type_witness::<lash::tools::ToolDispatchClient>();
    // FIG-2104-WITNESS-0620: lash::tools::ToolProcessEventClient
    type_witness::<lash::tools::ToolProcessEventClient>();
    // FIG-2104-WITNESS-0621: lash::tools::ToolSessionAdmin
    type_witness::<lash::tools::ToolSessionAdmin>();
    // FIG-2104-WITNESS-0622: lash::tools::ReconfigureError
    type_witness::<lash::tools::ReconfigureError>();
    // FIG-2104-WITNESS-0623: lash::tools::ReconfigureError::GenerationMismatch
    variant_witness(|value: &lash::tools::ReconfigureError| {
        matches!(
            value,
            lash::tools::ReconfigureError::GenerationMismatch { .. }
        )
    });
    // FIG-2104-WITNESS-0624: lash::tools::ReconfigureError::GenerationMismatch::actual
    field_witness(|value: &lash::tools::ReconfigureError| {
        if let lash::tools::ReconfigureError::GenerationMismatch { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2104-WITNESS-0625: lash::tools::ReconfigureError::GenerationMismatch::expected
    field_witness(|value: &lash::tools::ReconfigureError| {
        if let lash::tools::ReconfigureError::GenerationMismatch { expected, .. } = value {
            let _ = expected;
        }
    });
    // FIG-2104-WITNESS-0626: lash::tools::ReconfigureError::UnknownSource
    variant_witness(|value: &lash::tools::ReconfigureError| {
        matches!(value, lash::tools::ReconfigureError::UnknownSource(..))
    });
    // FIG-2104-WITNESS-0627: lash::tools::ReconfigureError::UnknownSource::0
    field_witness(|value: &lash::tools::ReconfigureError| {
        if let lash::tools::ReconfigureError::UnknownSource(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0628: lash::tools::ReconfigureError::Validation
    variant_witness(|value: &lash::tools::ReconfigureError| {
        matches!(value, lash::tools::ReconfigureError::Validation(..))
    });
    // FIG-2104-WITNESS-0629: lash::tools::ReconfigureError::Validation::0
    field_witness(|value: &lash::tools::ReconfigureError| {
        if let lash::tools::ReconfigureError::Validation(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0630: lash::tools::turn_outcome_from_tool_control
    member_witness(lash::tools::turn_outcome_from_tool_control);
    // FIG-2104-WITNESS-0631: lash::runtime::ApplyConfigPatch::provider_id
    field_witness(|value: &lash::runtime::ApplyConfigPatch| {
        let _ = &value.provider_id;
    });
    // FIG-2104-WITNESS-0632: lash::tools::AttemptContext::agent_frame_id
    member_witness(lash::tools::AttemptContext::agent_frame_id);
    // FIG-2104-WITNESS-0633: lash::tools::AttemptContext::async_process_id
    member_witness(lash::tools::AttemptContext::async_process_id);
    // FIG-2104-WITNESS-0634: lash::tools::AttemptContext::attachments
    member_witness(lash::tools::AttemptContext::attachments);
    // FIG-2104-WITNESS-0635: lash::tools::AttemptContext::attempt_number
    member_witness(lash::tools::AttemptContext::attempt_number);
    // FIG-2104-WITNESS-0636: lash::tools::AttemptContext::cancellation_token
    member_witness(lash::tools::AttemptContext::cancellation_token);
    // FIG-2104-WITNESS-0637: lash::tools::AttemptContext::completion_key
    member_witness(lash::tools::AttemptContext::completion_key);
    // FIG-2104-WITNESS-0638: lash::tools::AttemptContext::decode_prepared_payload
    member_witness(|context: &lash::tools::AttemptContext<'_>| {
        context.decode_prepared_payload::<serde_json::Value>()
    });
    // FIG-2104-WITNESS-0639: lash::tools::AttemptContext::direct_completions
    member_witness(lash::tools::AttemptContext::direct_completions);
    // FIG-2104-WITNESS-0640: lash::tools::AttemptContext::intent_identity
    member_witness(lash::tools::AttemptContext::intent_identity);
    // FIG-2104-WITNESS-0641: lash::tools::AttemptContext::max_attempts
    member_witness(lash::tools::AttemptContext::max_attempts);
    // FIG-2104-WITNESS-0642: lash::tools::AttemptContext::named_phase
    member_witness(lash::tools::AttemptContext::named_phase);
    // FIG-2104-WITNESS-0643: lash::tools::AttemptContext::prepared_payload
    member_witness(lash::tools::AttemptContext::prepared_payload);
    // FIG-2104-WITNESS-0644: lash::tools::AttemptContext::processes
    member_witness(lash::tools::AttemptContext::processes);
    // FIG-2104-WITNESS-0645: lash::tools::AttemptContext::provider
    member_witness(lash::tools::AttemptContext::provider);
    // FIG-2104-WITNESS-0646: lash::tools::AttemptContext::replay_key
    member_witness(lash::tools::AttemptContext::replay_key);
    // FIG-2104-WITNESS-0647: lash::tools::AttemptContext::runtime_process_id
    member_witness(lash::tools::AttemptContext::runtime_process_id);
    // FIG-2104-WITNESS-0648: lash::tools::AttemptContext::sessions
    member_witness(lash::tools::AttemptContext::sessions);
    // FIG-2104-WITNESS-0649: lash::tools::AttemptContext::tool_call_id
    member_witness(lash::tools::AttemptContext::tool_call_id);
    // FIG-2104-WITNESS-0650: lash::tools::AttemptContext::tool_execution_binding
    member_witness(lash::tools::AttemptContext::tool_execution_binding);
    // FIG-2104-WITNESS-0651: lash::tools::AttemptContext::execution_scope_id
    member_witness(lash::tools::AttemptContext::execution_scope_id);
    // FIG-2104-WITNESS-0652: lash::tools::AttemptProcessReads
    type_witness::<lash::tools::AttemptProcessReads>();
    // FIG-2104-WITNESS-0653: lash::tools::AttemptProcessReads::list_handles_filtered
    member_witness(lash::tools::AttemptProcessReads::list_handles_filtered);
    // FIG-2104-WITNESS-0654: lash::tools::AttemptSessionReads
    type_witness::<lash::tools::AttemptSessionReads>();
    // FIG-2104-WITNESS-0655: lash::tools::AttemptSessionReads::model
    member_witness(lash::tools::AttemptSessionReads::model);
    // FIG-2104-WITNESS-0656: lash::tools::AttemptSessionReads::shared_tool_catalog
    member_witness(lash::tools::AttemptSessionReads::shared_tool_catalog);
    // FIG-2104-WITNESS-0657: lash::tools::AttemptSessionReads::snapshot
    member_witness(
        |sessions: &lash::tools::AttemptSessionReads, session_id: String| {
            std::mem::drop(sessions.snapshot(session_id));
        },
    );
    // FIG-2104-WITNESS-0658: lash::tools::AttemptSessionReads::snapshot_current
    member_witness(lash::tools::AttemptSessionReads::snapshot_current);
    // FIG-2104-WITNESS-0659: lash::tools::AttemptSessionReads::tool_catalog
    member_witness(lash::tools::AttemptSessionReads::tool_catalog);
    // FIG-2104-WITNESS-0660: lash::tools::CancelProcessIntent
    type_witness::<lash::tools::CancelProcessIntent>();
    // FIG-2104-WITNESS-0661: lash::tools::CancelProcessIntent::process_id
    field_witness(|value: &lash::tools::CancelProcessIntent| {
        let _ = &value.process_id;
    });
    // FIG-2104-WITNESS-0662: lash::tools::CancelProcessIntent::reason
    field_witness(|value: &lash::tools::CancelProcessIntent| {
        let _ = &value.reason;
    });
    // FIG-2104-WITNESS-0663: lash::tools::CancelProcessIntent::session_id
    field_witness(|value: &lash::tools::CancelProcessIntent| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0664: lash::tools::EmitProcessEventIntent
    type_witness::<lash::tools::EmitProcessEventIntent>();
    // FIG-2104-WITNESS-0665: lash::tools::EmitProcessEventIntent::event_type
    field_witness(|value: &lash::tools::EmitProcessEventIntent| {
        let _ = &value.event_type;
    });
    // FIG-2104-WITNESS-0666: lash::tools::EmitProcessEventIntent::payload
    field_witness(|value: &lash::tools::EmitProcessEventIntent| {
        let _ = &value.payload;
    });
    // FIG-2104-WITNESS-0667: lash::tools::EmitProcessEventIntent::process_id
    field_witness(|value: &lash::tools::EmitProcessEventIntent| {
        let _ = &value.process_id;
    });
    // FIG-2104-WITNESS-0668: lash::tools::EmitProcessEventIntent::session_id
    field_witness(|value: &lash::tools::EmitProcessEventIntent| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0669: lash::tools::ProcessParentEndPolicy
    type_witness::<lash::tools::ProcessParentEndPolicy>();
    // FIG-2104-WITNESS-0670: lash::tools::ProcessParentEndPolicy::Abandon
    variant_witness(|value: &lash::tools::ProcessParentEndPolicy| {
        matches!(value, lash::tools::ProcessParentEndPolicy::Abandon)
    });
    // FIG-2104-WITNESS-0671: lash::tools::ProcessParentEndPolicy::Cancel
    variant_witness(|value: &lash::tools::ProcessParentEndPolicy| {
        matches!(value, lash::tools::ProcessParentEndPolicy::Cancel)
    });
    // FIG-2104-WITNESS-0672: lash::tools::SignalProcessIntent
    type_witness::<lash::tools::SignalProcessIntent>();
    // FIG-2104-WITNESS-0673: lash::tools::SignalProcessIntent::payload
    field_witness(|value: &lash::tools::SignalProcessIntent| {
        let _ = &value.payload;
    });
    // FIG-2104-WITNESS-0674: lash::tools::SignalProcessIntent::process_id
    field_witness(|value: &lash::tools::SignalProcessIntent| {
        let _ = &value.process_id;
    });
    // FIG-2104-WITNESS-0675: lash::tools::SignalProcessIntent::session_id
    field_witness(|value: &lash::tools::SignalProcessIntent| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0676: lash::tools::SignalProcessIntent::signal_name
    field_witness(|value: &lash::tools::SignalProcessIntent| {
        let _ = &value.signal_name;
    });
    // FIG-2104-WITNESS-0677: lash::tools::ToolAttemptOutcome::Pending
    variant_witness(|value: &lash::tools::ToolAttemptOutcome| {
        matches!(value, lash::tools::ToolAttemptOutcome::Pending(..))
    });
    // FIG-2104-WITNESS-0678: lash::tools::ToolAttemptOutcome::Pending::0
    field_witness(|value: &lash::tools::ToolAttemptOutcome| {
        if let lash::tools::ToolAttemptOutcome::Pending(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0679: lash::tools::ToolAttemptOutcome::done_without_intents
    member_witness(lash::tools::ToolAttemptOutcome::done_without_intents);
    // FIG-2104-WITNESS-0680: lash::tools::ToolAttemptOutcome::pending
    member_witness(lash::tools::ToolAttemptOutcome::pending);
    // FIG-2104-WITNESS-0681: lash::tools::ToolOutcomeDone::failure
    member_witness(lash::tools::ToolOutcomeDone::failure);
    // FIG-2104-WITNESS-0682: lash::tools::ToolOutcomeDone::from_output
    member_witness(lash::tools::ToolOutcomeDone::from_output);
    // FIG-2104-WITNESS-0683: lash::tools::ToolIntent::CancelProcess
    variant_witness(|value: &lash::tools::ToolIntent| {
        matches!(value, lash::tools::ToolIntent::CancelProcess(..))
    });
    // FIG-2104-WITNESS-0684: lash::tools::ToolIntent::CancelProcess::0
    field_witness(|value: &lash::tools::ToolIntent| {
        if let lash::tools::ToolIntent::CancelProcess(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0685: lash::tools::ToolIntent::SignalProcess
    variant_witness(|value: &lash::tools::ToolIntent| {
        matches!(value, lash::tools::ToolIntent::SignalProcess(..))
    });
    // FIG-2104-WITNESS-0686: lash::tools::ToolIntent::SignalProcess::0
    field_witness(|value: &lash::tools::ToolIntent| {
        if let lash::tools::ToolIntent::SignalProcess(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0687: lash::tools::ToolProvider::execute_attempt_by_id
    fn method_witness_0687<T: lash::tools::ToolProvider>() {
        member_witness(T::execute_attempt_by_id);
    }
    // FIG-2104-WITNESS-0688: lash::tools::ToolProvider::execute_granted_attempt
    fn method_witness_0688<T: lash::tools::ToolProvider>() {
        member_witness(T::execute_granted_attempt);
    }
    // FIG-2104-WITNESS-0689: lash::tools::ToolProvider::attempt_may_defer
    fn method_witness_0689<T: lash::tools::ToolProvider>() {
        member_witness(T::attempt_may_defer);
    }
    // FIG-2104-WITNESS-0690: lash::tools::TOOL_INTENT_MAX_CANONICAL_BYTES
    member_witness(lash::tools::TOOL_INTENT_MAX_CANONICAL_BYTES);
    // FIG-2104-WITNESS-0691: lash::tools::TOOL_INTENT_MAX_COUNT
    member_witness(lash::tools::TOOL_INTENT_MAX_COUNT);
    // FIG-2104-WITNESS-0692: lash::tools::TOOL_INTENT_MAX_PER_KIND
    member_witness(lash::tools::TOOL_INTENT_MAX_PER_KIND);
    // FIG-2104-WITNESS-0693: lash::tools::TOOL_INTENT_PROTOCOL_V1
    member_witness(lash::tools::TOOL_INTENT_PROTOCOL_V1);
    // FIG-2104-WITNESS-0694: lash::durability::ToolAttemptLaunch::Done::intents
    field_witness(|value: &lash::durability::ToolAttemptLaunch| {
        if let lash::durability::ToolAttemptLaunch::Done { intents, .. } = value {
            let _ = intents;
        }
    });
    // FIG-2104-WITNESS-0695: lash::tools::ToolIntentExecutionOutcome::Executed::identity
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Executed { identity, .. } = value {
            let _ = identity;
        }
    });
    // FIG-2104-WITNESS-0696: lash::tools::ToolIntentExecutionOutcome::Executed::result
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Executed { result, .. } = value {
            let _ = result;
        }
    });
    // FIG-2104-WITNESS-0697: lash::tools::ToolIntentExecutionOutcome::Refused
    variant_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        matches!(
            value,
            lash::tools::ToolIntentExecutionOutcome::Refused { .. }
        )
    });
    // FIG-2104-WITNESS-0698: lash::tools::ToolIntentExecutionOutcome::Refused::identity
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Refused { identity, .. } = value {
            let _ = identity;
        }
    });
    // FIG-2104-WITNESS-0699: lash::tools::ToolIntentExecutionOutcome::Refused::intent_index
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Refused { intent_index, .. } = value {
            let _ = intent_index;
        }
    });
    // FIG-2104-WITNESS-0700: lash::tools::ToolIntentExecutionOutcome::Refused::kind
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Refused { kind, .. } = value {
            let _ = kind;
        }
    });
    // FIG-2104-WITNESS-0701: lash::tools::ToolIntentExecutionOutcome::Refused::refusal
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Refused { refusal, .. } = value {
            let _ = refusal;
        }
    });
    // FIG-2104-WITNESS-0702: lash::tools::ToolIntentExecutionOutcome::ProtocolRefused
    variant_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        matches!(
            value,
            lash::tools::ToolIntentExecutionOutcome::ProtocolRefused { .. }
        )
    });
    // FIG-2104-WITNESS-0703: lash::tools::ToolIntentExecutionOutcome::ProtocolRefused::refusal
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::ProtocolRefused { refusal, .. } = value {
            let _ = refusal;
        }
    });
    // FIG-2104-WITNESS-0704: lash::tools::ToolIntentExecutionOutcome::kind
    member_witness(lash::tools::ToolIntentExecutionOutcome::kind);
    // FIG-2104-WITNESS-0705: lash::tools::ToolIntentExecutionOutcome::model_addendum
    member_witness(lash::tools::ToolIntentExecutionOutcome::model_addendum);
    // FIG-2104-WITNESS-0706: lash::tools::ToolIntentIdentity
    type_witness::<lash::tools::ToolIntentIdentity>();
    // FIG-2104-WITNESS-0707: lash::tools::ToolIntentIdentity::intent_index
    field_witness(|value: &lash::tools::ToolIntentIdentity| {
        let _ = &value.intent_index;
    });
    // FIG-2104-WITNESS-0708: lash::tools::ToolIntentIdentity::replay_key
    field_witness(|value: &lash::tools::ToolIntentIdentity| {
        let _ = &value.replay_key;
    });
    // FIG-2104-WITNESS-0709: lash::tools::ToolIntentIdentity::session_id
    field_witness(|value: &lash::tools::ToolIntentIdentity| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0710: lash::tools::ToolIntentIdentity::tool_call_id
    field_witness(|value: &lash::tools::ToolIntentIdentity| {
        let _ = &value.tool_call_id;
    });
    // FIG-2104-WITNESS-0711: lash::tools::ToolIntentIdentity::execution_scope_id
    field_witness(|value: &lash::tools::ToolIntentIdentity| {
        let _ = &value.execution_scope_id;
    });
    // FIG-2104-WITNESS-0712: lash::tools::ToolIntentKind
    type_witness::<lash::tools::ToolIntentKind>();
    // FIG-2104-WITNESS-0713: lash::tools::ToolIntentKind::CancelProcess
    variant_witness(|value: &lash::tools::ToolIntentKind| {
        matches!(value, lash::tools::ToolIntentKind::CancelProcess)
    });
    // FIG-2104-WITNESS-0714: lash::tools::ToolIntentKind::EmitProcessEvent
    variant_witness(|value: &lash::tools::ToolIntentKind| {
        matches!(value, lash::tools::ToolIntentKind::EmitProcessEvent)
    });
    // FIG-2104-WITNESS-0715: lash::tools::ToolIntentKind::SignalProcess
    variant_witness(|value: &lash::tools::ToolIntentKind| {
        matches!(value, lash::tools::ToolIntentKind::SignalProcess)
    });
    // FIG-2104-WITNESS-0716: lash::tools::ToolIntentKind::StartProcess
    variant_witness(|value: &lash::tools::ToolIntentKind| {
        matches!(value, lash::tools::ToolIntentKind::StartProcess)
    });
    // FIG-2104-WITNESS-0717: lash::tools::ToolIntentKind::as_str
    member_witness(lash::tools::ToolIntentKind::as_str);
    // FIG-2104-WITNESS-0718: lash::tools::ToolIntentRefusalReason
    type_witness::<lash::tools::ToolIntentRefusalReason>();
    // FIG-2104-WITNESS-0719: lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded { .. }
        )
    });
    // FIG-2104-WITNESS-0720: lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded::actual
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
            actual, ..
        } = value
        {
            let _ = actual;
        }
    });
    // FIG-2104-WITNESS-0721: lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded::maximum
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
            maximum, ..
        } = value
        {
            let _ = maximum;
        }
    });
    // FIG-2104-WITNESS-0722: lash::tools::ToolIntentRefusalReason::CommandFailed
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::CommandFailed { .. }
        )
    });
    // FIG-2104-WITNESS-0723: lash::tools::ToolIntentRefusalReason::CommandFailed::code
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CommandFailed { code, .. } = value {
            let _ = code;
        }
    });
    // FIG-2104-WITNESS-0724: lash::tools::ToolIntentRefusalReason::CommandFailed::message
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CommandFailed { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2104-WITNESS-0725: lash::tools::ToolIntentRefusalReason::CountBudgetExceeded
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::CountBudgetExceeded { .. }
        )
    });
    // FIG-2104-WITNESS-0726: lash::tools::ToolIntentRefusalReason::CountBudgetExceeded::actual
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CountBudgetExceeded { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2104-WITNESS-0727: lash::tools::ToolIntentRefusalReason::CountBudgetExceeded::maximum
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::CountBudgetExceeded { maximum, .. } = value {
            let _ = maximum;
        }
    });
    // FIG-2104-WITNESS-0728: lash::tools::ToolIntentRefusalReason::IntentIndexOverflow
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::IntentIndexOverflow
        )
    });
    // FIG-2104-WITNESS-0729: lash::tools::ToolIntentRefusalReason::MissingToolCallId
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::MissingToolCallId
        )
    });
    // FIG-2104-WITNESS-0730: lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded { .. }
        )
    });
    // FIG-2104-WITNESS-0731: lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded::actual
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2104-WITNESS-0732: lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded::kind
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded { kind, .. } = value {
            let _ = kind;
        }
    });
    // FIG-2104-WITNESS-0733: lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded::maximum
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::PerKindBudgetExceeded { maximum, .. } = value {
            let _ = maximum;
        }
    });
    // FIG-2104-WITNESS-0734: lash::tools::ToolIntentRefusalReason::SessionMismatch
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::SessionMismatch { .. }
        )
    });
    // FIG-2104-WITNESS-0735: lash::tools::ToolIntentRefusalReason::SessionMismatch::expected
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::SessionMismatch { expected, .. } = value {
            let _ = expected;
        }
    });
    // FIG-2104-WITNESS-0736: lash::tools::ToolIntentRefusalReason::SessionMismatch::recorded
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::SessionMismatch { recorded, .. } = value {
            let _ = recorded;
        }
    });
    // FIG-2104-WITNESS-0737: lash::tools::ToolIntentRefusalReason::UnsupportedProtocolVersion
    variant_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        matches!(
            value,
            lash::tools::ToolIntentRefusalReason::UnsupportedProtocolVersion { .. }
        )
    });
    // FIG-2104-WITNESS-0738: lash::tools::ToolIntentRefusalReason::UnsupportedProtocolVersion::recorded
    field_witness(|value: &lash::tools::ToolIntentRefusalReason| {
        if let lash::tools::ToolIntentRefusalReason::UnsupportedProtocolVersion {
            recorded, ..
        } = value
        {
            let _ = recorded;
        }
    });
    // FIG-2104-WITNESS-0739: lash::tools::ToolIntentRefusalReason::code
    member_witness(lash::tools::ToolIntentRefusalReason::code);
    // FIG-2104-WITNESS-0740: lash::tools::derive_tool_intent_identity
    member_witness(lash::tools::derive_tool_intent_identity);
    // FIG-2104-WITNESS-0741: lash::tools::ToolSessionModel
    type_witness::<lash::tools::ToolSessionModel>();
    // FIG-2104-WITNESS-0742: lash::tools::ToolSessionModel::generation
    field_witness(|value: &lash::tools::ToolSessionModel| {
        let _ = &value.generation;
    });
    // FIG-2104-WITNESS-0743: lash::tools::ToolSessionModel::model
    field_witness(|value: &lash::tools::ToolSessionModel| {
        let _ = &value.model;
    });
    // FIG-2104-WITNESS-0744: lash::tools::ToolSessionModel::model_capability
    field_witness(|value: &lash::tools::ToolSessionModel| {
        let _ = &value.model_capability;
    });
    // FIG-2104-WITNESS-0745: lash::tools::ToolSessionModel::model_variant
    field_witness(|value: &lash::tools::ToolSessionModel| {
        let _ = &value.model_variant;
    });
    // FIG-2104-WITNESS-0746: lash::tools::ToolIntentExecutionOutcome::Executed::parent_end
    field_witness(|value: &lash::tools::ToolIntentExecutionOutcome| {
        if let lash::tools::ToolIntentExecutionOutcome::Executed { parent_end, .. } = value {
            let _ = parent_end;
        }
    });
    // FIG-2104-WITNESS-0747: lash::tools::ToolIntentParentEnd
    type_witness::<lash::tools::ToolIntentParentEnd>();
    // FIG-2104-WITNESS-0748: lash::tools::ToolIntentParentEnd::policy
    field_witness(|value: &lash::tools::ToolIntentParentEnd| {
        let _ = &value.policy;
    });
    // FIG-2104-WITNESS-0749: lash::tools::ToolIntentParentEnd::process_id
    field_witness(|value: &lash::tools::ToolIntentParentEnd| {
        let _ = &value.process_id;
    });
    // FIG-2104-WITNESS-0750: lash::tools::ToolIntentParentEndOutcome
    type_witness::<lash::tools::ToolIntentParentEndOutcome>();
}
