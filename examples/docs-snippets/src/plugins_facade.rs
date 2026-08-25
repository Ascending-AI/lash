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
    // FIG-2104-WITNESS-0001: lash::AwaitEventWaitIdentity::ToolCompletion
    variant_witness(|value: &lash::AwaitEventWaitIdentity| {
        matches!(value, lash::AwaitEventWaitIdentity::ToolCompletion { .. })
    });
    // FIG-2104-WITNESS-0002: lash::AwaitEventWaitIdentity::ToolCompletion::tool_call_id
    field_witness(|value: &lash::AwaitEventWaitIdentity| {
        if let lash::AwaitEventWaitIdentity::ToolCompletion { tool_call_id, .. } = value {
            let _ = tool_call_id;
        }
    });
    // FIG-2104-WITNESS-0003: lash::AwaitEventWaitIdentity::tool_completion
    member_witness(|tool_call_id: String| {
        lash::AwaitEventWaitIdentity::tool_completion(tool_call_id)
    });
    // FIG-2104-WITNESS-0004: lash::tools::ToolContext::named_phase
    member_witness(lash::tools::ToolContext::named_phase);
    // FIG-2104-WITNESS-0007: lash::TurnExecutionMetrics::had_tool_calls
    field_witness(|value: &lash::TurnExecutionMetrics| {
        let _ = &value.had_tool_calls;
    });
    // FIG-2104-WITNESS-0008: lash::PluginStack
    type_witness::<lash::PluginStack>();
    // FIG-2104-WITNESS-0009: lash::PluginStack::configure
    member_witness(|stack: lash::PluginStack| stack.configure(|_| {}));
    // FIG-2104-WITNESS-0010: lash::PluginStack::extend
    member_witness(
        |stack: &mut lash::PluginStack,
         plugins: Vec<std::sync::Arc<dyn lash::plugins::PluginFactory>>| {
            stack.extend(plugins);
        },
    );
    // FIG-2104-WITNESS-0011: lash::PluginStack::factories
    member_witness(lash::PluginStack::factories);
    // FIG-2104-WITNESS-0012: lash::PluginStack::from_factories
    member_witness(
        |plugins: Vec<std::sync::Arc<dyn lash::plugins::PluginFactory>>| {
            lash::PluginStack::from_factories(plugins)
        },
    );
    // FIG-2104-WITNESS-0013: lash::PluginStack::into_factories
    member_witness(lash::PluginStack::into_factories);
    // FIG-2104-WITNESS-0014: lash::PluginStack::new
    member_witness(lash::PluginStack::new);
    // FIG-2104-WITNESS-0015: lash::PluginStack::push
    member_witness(lash::PluginStack::push);
    // FIG-2104-WITNESS-0016: lash::PluginStack::remove
    member_witness(lash::PluginStack::remove);
    // FIG-2104-WITNESS-0017: lash::PluginStack::replace
    member_witness(lash::PluginStack::replace);
    // FIG-2104-WITNESS-0018: lash::PluginStack::retain
    member_witness(|stack: &mut lash::PluginStack| {
        let _ = stack.retain(|_| true);
    });
    // FIG-2104-WITNESS-0019: lash::SessionCommand::RefreshToolCatalog
    variant_witness(|value: &lash::SessionCommand| {
        matches!(value, lash::SessionCommand::RefreshToolCatalog { .. })
    });
    // FIG-2104-WITNESS-0020: lash::SessionCommand::RefreshToolCatalog::reason
    field_witness(|value: &lash::SessionCommand| {
        if let lash::SessionCommand::RefreshToolCatalog { reason, .. } = value {
            let _ = reason;
        }
    });
    // FIG-2104-WITNESS-0021: lash::SessionCreateRequest::plugin_options
    field_witness(|value: &lash::SessionCreateRequest| {
        let _ = &value.plugin_options;
    });
    // FIG-2104-WITNESS-0022: lash::SessionCreateRequest::plugin_source
    field_witness(|value: &lash::SessionCreateRequest| {
        let _ = &value.plugin_source;
    });
    // FIG-2104-WITNESS-0023: lash::SessionCreateRequest::tool_access
    field_witness(|value: &lash::SessionCreateRequest| {
        let _ = &value.tool_access;
    });
    // FIG-2104-WITNESS-0024: lash::SessionCreateRequest::with_plugin_source
    member_witness(lash::SessionCreateRequest::with_plugin_source);
    // FIG-2104-WITNESS-0025: lash::SessionCreateRequest::with_tool_access
    member_witness(lash::SessionCreateRequest::with_tool_access);
    // FIG-2104-WITNESS-0026: lash::SessionError::ProviderMismatch
    variant_witness(|value: &lash::SessionError| {
        matches!(value, lash::SessionError::ProviderMismatch { .. })
    });
    // FIG-2104-WITNESS-0027: lash::SessionError::ProviderMismatch::actual
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderMismatch { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2104-WITNESS-0028: lash::SessionError::ProviderMismatch::expected
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderMismatch { expected, .. } = value {
            let _ = expected;
        }
    });
    // FIG-2104-WITNESS-0029: lash::SessionError::ProviderMismatch::session_id
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderMismatch { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0030: lash::SessionError::ProviderUnavailable
    variant_witness(|value: &lash::SessionError| {
        matches!(value, lash::SessionError::ProviderUnavailable { .. })
    });
    // FIG-2104-WITNESS-0031: lash::SessionError::ProviderUnavailable::provider_id
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderUnavailable { provider_id, .. } = value {
            let _ = provider_id;
        }
    });
    // FIG-2104-WITNESS-0032: lash::SessionError::ProviderUnavailable::session_id
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderUnavailable { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0033: lash::SessionError::ProviderUnconfigured
    variant_witness(|value: &lash::SessionError| {
        matches!(value, lash::SessionError::ProviderUnconfigured { .. })
    });
    // FIG-2104-WITNESS-0034: lash::SessionError::ProviderUnconfigured::session_id
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::ProviderUnconfigured { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0035: lash::TurnEvent::PluginRuntime
    variant_witness(|value: &lash::TurnEvent| {
        matches!(value, lash::TurnEvent::PluginRuntime { .. })
    });
    // FIG-2104-WITNESS-0036: lash::TurnEvent::PluginRuntime::event
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::PluginRuntime { event, .. } = value {
            let _ = event;
        }
    });
    // FIG-2104-WITNESS-0037: lash::TurnEvent::PluginRuntime::plugin_id
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::PluginRuntime { plugin_id, .. } = value {
            let _ = plugin_id;
        }
    });
    // FIG-2104-WITNESS-0038: lash::TurnEvent::ToolCallCompleted::graph_key
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolCallCompleted { graph_key, .. } = value {
            let _ = graph_key;
        }
    });
    // FIG-2104-WITNESS-0039: lash::TurnEvent::ToolCallCompleted::parent_call_id
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolCallCompleted { parent_call_id, .. } = value {
            let _ = parent_call_id;
        }
    });
    // FIG-2104-WITNESS-0040: lash::TurnEvent::ToolCallStarted::graph_key
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolCallStarted { graph_key, .. } = value {
            let _ = graph_key;
        }
    });
    // FIG-2104-WITNESS-0041: lash::TurnEvent::ToolCallStarted::parent_call_id
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolCallStarted { parent_call_id, .. } = value {
            let _ = parent_call_id;
        }
    });
    // FIG-2104-WITNESS-0042: lash::TurnEvent::ToolValue::value
    field_witness(|value: &lash::TurnEvent| {
        if let lash::TurnEvent::ToolValue { value, .. } = value {
            let _ = value;
        }
    });
    // FIG-2104-WITNESS-0043: lash::direct::LlmOutputPart::ToolCall
    variant_witness(|value: &lash::direct::LlmOutputPart| {
        matches!(value, lash::direct::LlmOutputPart::ToolCall { .. })
    });
    // FIG-2104-WITNESS-0044: lash::direct::LlmOutputPart::ToolCall::call_id
    field_witness(|value: &lash::direct::LlmOutputPart| {
        if let lash::direct::LlmOutputPart::ToolCall { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // FIG-2104-WITNESS-0045: lash::direct::LlmOutputPart::ToolCall::input_json
    field_witness(|value: &lash::direct::LlmOutputPart| {
        if let lash::direct::LlmOutputPart::ToolCall { input_json, .. } = value {
            let _ = input_json;
        }
    });
    // FIG-2104-WITNESS-0046: lash::direct::LlmOutputPart::ToolCall::replay
    field_witness(|value: &lash::direct::LlmOutputPart| {
        if let lash::direct::LlmOutputPart::ToolCall { replay, .. } = value {
            let _ = replay;
        }
    });
    // FIG-2104-WITNESS-0047: lash::direct::LlmOutputPart::ToolCall::tool_name
    field_witness(|value: &lash::direct::LlmOutputPart| {
        if let lash::direct::LlmOutputPart::ToolCall { tool_name, .. } = value {
            let _ = tool_name;
        }
    });
    // FIG-2104-WITNESS-0048: lash::direct::LlmStreamEvent::Evidence
    variant_witness(|value: &lash::direct::LlmStreamEvent| {
        matches!(value, lash::direct::LlmStreamEvent::Evidence(..))
    });
    // FIG-2104-WITNESS-0049: lash::direct::LlmStreamEvent::Evidence::0
    field_witness(|value: &lash::direct::LlmStreamEvent| {
        if let lash::direct::LlmStreamEvent::Evidence(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0050: lash::direct::LlmTerminalReason::ProviderError
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::ProviderError)
    });
    // FIG-2104-WITNESS-0051: lash::direct::LlmTerminalReason::ToolUse
    variant_witness(|value: &lash::direct::LlmTerminalReason| {
        matches!(value, lash::direct::LlmTerminalReason::ToolUse)
    });
    // FIG-2104-WITNESS-0052: lash::direct::ProviderFileScope
    type_witness::<lash::direct::ProviderFileScope>();
    // FIG-2104-WITNESS-0053: lash::direct::ProviderFileScope::credential_scope
    field_witness(|value: &lash::direct::ProviderFileScope| {
        let _ = &value.credential_scope;
    });
    // FIG-2104-WITNESS-0054: lash::direct::ProviderFileScope::new
    member_witness(|provider: String, credential_scope: String| {
        lash::direct::ProviderFileScope::new(provider, credential_scope)
    });
    // FIG-2104-WITNESS-0055: lash::direct::ProviderFileScope::provider
    field_witness(|value: &lash::direct::ProviderFileScope| {
        let _ = &value.provider;
    });
    // FIG-2104-WITNESS-0056: lash::durability::RuntimeHostConfig::prompt
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.prompt;
    });
    // FIG-2104-WITNESS-0057: lash::durability::RuntimeHostConfig::providers
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.providers;
    });
    // FIG-2104-WITNESS-0058: lash::messages::MessageOrigin::Plugin
    variant_witness(|value: &lash::messages::MessageOrigin| {
        matches!(value, lash::messages::MessageOrigin::Plugin { .. })
    });
    // FIG-2104-WITNESS-0059: lash::messages::MessageOrigin::Plugin::plugin_id
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Plugin { plugin_id, .. } = value {
            let _ = plugin_id;
        }
    });
    // FIG-2104-WITNESS-0060: lash::messages::MessageOrigin::Plugin::transient
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Plugin { transient, .. } = value {
            let _ = transient;
        }
    });
    // FIG-2104-WITNESS-0061: lash::persistence::HydratedSessionCheckpoint::plugin_snapshot_revision
    field_witness(|value: &lash::persistence::HydratedSessionCheckpoint| {
        let _ = &value.plugin_snapshot_revision;
    });
    // FIG-2104-WITNESS-0062: lash::persistence::PersistedSessionConfig::provider_id
    field_witness(|value: &lash::persistence::PersistedSessionConfig| {
        let _ = &value.provider_id;
    });
    // FIG-2104-WITNESS-0063: lash::persistence::PersistedTurnState::last_prompt_usage
    field_witness(|value: &lash::persistence::PersistedTurnState| {
        let _ = &value.last_prompt_usage;
    });
    // FIG-2104-WITNESS-0064: lash::persistence::ProtocolEvent::plugin_id
    field_witness(|value: &lash::persistence::ProtocolEvent| {
        let _ = &value.plugin_id;
    });
    // FIG-2104-WITNESS-0065: lash::persistence::RuntimeSessionState::last_prompt_usage
    field_witness(|value: &lash::persistence::RuntimeSessionState| {
        let _ = &value.last_prompt_usage;
    });
    // FIG-2104-WITNESS-0066: lash::persistence::RuntimeSessionState::plugin_snapshot
    member_witness(lash::persistence::RuntimeSessionState::plugin_snapshot);
    // FIG-2104-WITNESS-0067: lash::persistence::RuntimeSessionState::plugin_snapshot_ref
    member_witness(lash::persistence::RuntimeSessionState::plugin_snapshot_ref);
    // FIG-2104-WITNESS-0068: lash::persistence::RuntimeSessionState::set_plugin_snapshot
    member_witness(lash::persistence::RuntimeSessionState::set_plugin_snapshot);
    // FIG-2104-WITNESS-0069: lash::persistence::RuntimeSessionState::set_tool_state_snapshot
    member_witness(lash::persistence::RuntimeSessionState::set_tool_state_snapshot);
    // FIG-2104-WITNESS-0070: lash::persistence::RuntimeSessionState::plugin_snapshot_revision
    field_witness(|value: &lash::persistence::RuntimeSessionState| {
        let _ = &value.plugin_snapshot_revision;
    });
    // FIG-2104-WITNESS-0071: lash::persistence::RuntimeSessionState::refresh_plugin_snapshots
    member_witness(lash::persistence::RuntimeSessionState::refresh_plugin_snapshots);
    // FIG-2104-WITNESS-0072: lash::persistence::RuntimeSessionState::tool_state_generation
    member_witness(lash::persistence::RuntimeSessionState::tool_state_generation);
    // FIG-2104-WITNESS-0073: lash::persistence::RuntimeSessionState::tool_state_ref
    member_witness(lash::persistence::RuntimeSessionState::tool_state_ref);
    // FIG-2104-WITNESS-0074: lash::persistence::RuntimeSessionState::tool_state_snapshot
    member_witness(lash::persistence::RuntimeSessionState::tool_state_snapshot);
    // FIG-2104-WITNESS-0075: lash::persistence::SessionCheckpoint::plugin_snapshot_revision
    field_witness(|value: &lash::persistence::SessionCheckpoint| {
        let _ = &value.plugin_snapshot_revision;
    });
    // FIG-2104-WITNESS-0076: lash::persistence::SessionGraph::append_plugin
    member_witness(
        |graph: &mut lash::persistence::SessionGraph,
         plugin_type: String,
         body: serde_json::Value| { graph.append_plugin(plugin_type, body) },
    );
    // FIG-2104-WITNESS-0077: lash::persistence::SessionNodeRecord::plugin_body
    member_witness(|node: &lash::persistence::SessionNodeRecord| {
        node.plugin_body::<serde_json::Value>()
    });
    // FIG-2104-WITNESS-0078: lash::persistence::SessionReadView::last_prompt_usage
    member_witness(lash::persistence::SessionReadView::last_prompt_usage);
    // FIG-2104-WITNESS-0079: lash::plugins::AssistantResponseTransform
    type_witness::<lash::plugins::AssistantResponseTransform>();
    // FIG-2104-WITNESS-0080: lash::plugins::AssistantResponseTransform::events
    field_witness(|value: &lash::plugins::AssistantResponseTransform| {
        let _ = &value.events;
    });
    // FIG-2104-WITNESS-0081: lash::plugins::AssistantResponseTransform::response
    field_witness(|value: &lash::plugins::AssistantResponseTransform| {
        let _ = &value.response;
    });
    // FIG-2104-WITNESS-0082: lash::plugins::AssistantStreamTransform
    type_witness::<lash::plugins::AssistantStreamTransform>();
    // FIG-2104-WITNESS-0083: lash::plugins::AssistantStreamTransform::abort_stream
    field_witness(|value: &lash::plugins::AssistantStreamTransform| {
        let _ = &value.abort_stream;
    });
    // FIG-2104-WITNESS-0084: lash::plugins::AssistantStreamTransform::chunk
    field_witness(|value: &lash::plugins::AssistantStreamTransform| {
        let _ = &value.chunk;
    });
    // FIG-2104-WITNESS-0085: lash::plugins::AssistantStreamTransform::events
    field_witness(|value: &lash::plugins::AssistantStreamTransform| {
        let _ = &value.events;
    });
    // FIG-2104-WITNESS-0086: lash::plugins::AssistantStreamTransform::reasoning_deltas
    field_witness(|value: &lash::plugins::AssistantStreamTransform| {
        let _ = &value.reasoning_deltas;
    });
    // FIG-2104-WITNESS-0087: lash::plugins::CheckpointHookContext
    type_witness::<lash::plugins::CheckpointHookContext>();
    // FIG-2104-WITNESS-0088: lash::plugins::CheckpointHookContext::session_graph
    field_witness(|value: &lash::plugins::CheckpointHookContext| {
        let _ = &value.session_graph;
    });
    // FIG-2104-WITNESS-0089: lash::plugins::CheckpointHookContext::session_id
    field_witness(|value: &lash::plugins::CheckpointHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0090: lash::plugins::CheckpointHookContext::session_lifecycle
    field_witness(|value: &lash::plugins::CheckpointHookContext| {
        let _ = &value.session_lifecycle;
    });
    // FIG-2104-WITNESS-0091: lash::plugins::CheckpointHookContext::state
    field_witness(|value: &lash::plugins::CheckpointHookContext| {
        let _ = &value.state;
    });
    // FIG-2104-WITNESS-0092: lash::plugins::CompactionContext
    type_witness::<lash::plugins::CompactionContext>();
    // FIG-2104-WITNESS-0093: lash::plugins::CompactionContext::instructions
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.instructions;
    });
    // FIG-2104-WITNESS-0094: lash::plugins::CompactionContext::scoped_effect_controller
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.scoped_effect_controller;
    });
    // FIG-2104-WITNESS-0095: lash::plugins::CompactionContext::session_graph
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.session_graph;
    });
    // FIG-2104-WITNESS-0096: lash::plugins::CompactionContext::session_id
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0097: lash::plugins::CompactionContext::session_lifecycle
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.session_lifecycle;
    });
    // FIG-2104-WITNESS-0098: lash::plugins::CompactionContext::sessions
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.sessions;
    });
    // FIG-2104-WITNESS-0099: lash::plugins::CompactionContext::state
    field_witness(|value: &lash::plugins::CompactionContext| {
        let _ = &value.state;
    });
    // FIG-2104-WITNESS-0100: lash::plugins::ContextCompaction
    type_witness::<lash::plugins::ContextCompaction>();
    // FIG-2104-WITNESS-0101: lash::plugins::ContextCompaction::initial_nodes
    field_witness(|value: &lash::plugins::ContextCompaction| {
        let _ = &value.initial_nodes;
    });
    // FIG-2104-WITNESS-0102: lash::plugins::ContextCompaction::is_empty
    member_witness(lash::plugins::ContextCompaction::is_empty);
    // FIG-2104-WITNESS-0103: lash::plugins::ContextCompaction::new
    member_witness(lash::plugins::ContextCompaction::new);
    // FIG-2104-WITNESS-0104: lash::plugins::ContextError
    type_witness::<lash::plugins::ContextError>();
    // FIG-2104-WITNESS-0105: lash::plugins::ContextError::Pipeline
    variant_witness(|value: &lash::plugins::ContextError| {
        matches!(value, lash::plugins::ContextError::Pipeline(..))
    });
    // FIG-2104-WITNESS-0106: lash::plugins::ContextError::Pipeline::0
    field_witness(|value: &lash::plugins::ContextError| {
        if let lash::plugins::ContextError::Pipeline(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0107: lash::plugins::ContextError::Session
    variant_witness(|value: &lash::plugins::ContextError| {
        matches!(value, lash::plugins::ContextError::Session(..))
    });
    // FIG-2104-WITNESS-0108: lash::plugins::ContextError::Session::0
    field_witness(|value: &lash::plugins::ContextError| {
        if let lash::plugins::ContextError::Session(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0109: lash::plugins::PluginError::AppendOperationIdentityConflict
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::AppendOperationIdentityConflict { .. }
        )
    });
    // FIG-2104-WITNESS-0110: lash::plugins::PluginError::AppendOperationIdentityConflict::operation_key
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendOperationIdentityConflict {
            operation_key, ..
        } = value
        {
            let _ = operation_key;
        }
    });
    // FIG-2104-WITNESS-0111: lash::plugins::PluginError::AppendOperationIdentityConflict::session_id
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendOperationIdentityConflict { session_id, .. } =
            value
        {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0112: lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt { .. }
        )
    });
    // FIG-2104-WITNESS-0113: lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt::attempted
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt {
            attempted,
            ..
        } = value
        {
            let _ = attempted;
        }
    });
    // FIG-2104-WITNESS-0114: lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt::operation_key
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt {
            operation_key,
            ..
        } = value
        {
            let _ = operation_key;
        }
    });
    // FIG-2104-WITNESS-0115: lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt::session_id
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0116: lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt::stored
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::AppendReceiptRequestedNodeCountCorrupt {
            stored, ..
        } = value
        {
            let _ = stored;
        }
    });
    // FIG-2104-WITNESS-0117: lash::plugins::PluginError::ClockBeforeUnixEpoch
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ClockBeforeUnixEpoch { .. }
        )
    });
    // FIG-2104-WITNESS-0118: lash::plugins::PluginError::ClockBeforeUnixEpoch::clock
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ClockBeforeUnixEpoch { clock, .. } = value {
            let _ = clock;
        }
    });
    // FIG-2104-WITNESS-0119: lash::plugins::PluginError::ClockBeforeUnixEpoch::epoch_ms
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ClockBeforeUnixEpoch { epoch_ms, .. } = value {
            let _ = epoch_ms;
        }
    });
    // FIG-2104-WITNESS-0120: lash::plugins::PluginError::InvalidProcessWakeIdentity
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::InvalidProcessWakeIdentity { .. }
        )
    });
    // FIG-2104-WITNESS-0121: lash::plugins::PluginError::InvalidProcessWakeIdentity::wake_id
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::InvalidProcessWakeIdentity { wake_id, .. } = value {
            let _ = wake_id;
        }
    });
    // FIG-2104-WITNESS-0122: lash::plugins::PluginError::Invoke
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::Invoke(..))
    });
    // FIG-2104-WITNESS-0123: lash::plugins::PluginError::Invoke::0
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::Invoke(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0124: lash::plugins::PluginError::MonotonicCounterOverflow
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::MonotonicCounterOverflow { .. }
        )
    });
    // FIG-2104-WITNESS-0125: lash::plugins::PluginError::MonotonicCounterOverflow::counter
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::MonotonicCounterOverflow { counter, .. } = value {
            let _ = counter;
        }
    });
    // FIG-2104-WITNESS-0126: lash::plugins::PluginError::MonotonicCounterOverflow::current
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::MonotonicCounterOverflow { current, .. } = value {
            let _ = current;
        }
    });
    // FIG-2104-WITNESS-0127: lash::plugins::PluginError::Registration
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::Registration(..))
    });
    // FIG-2104-WITNESS-0128: lash::plugins::PluginError::Registration::0
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::Registration(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0129: lash::plugins::PluginError::RuntimeEffectController::0
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::RuntimeEffectController(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0130: lash::plugins::PluginError::Session::0
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::Session(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0131: lash::plugins::PluginError::SessionExecutionLeaseLost
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::SessionExecutionLeaseLost { .. }
        )
    });
    // FIG-2104-WITNESS-0132: lash::plugins::PluginError::SessionExecutionLeaseLost::session_id
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::SessionExecutionLeaseLost { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2104-WITNESS-0133: lash::plugins::PluginError::Snapshot
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::Snapshot(..))
    });
    // FIG-2104-WITNESS-0134: lash::plugins::PluginError::Snapshot::0
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::Snapshot(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0135: lash::plugins::PluginError::StoredDataCorrupt
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::StoredDataCorrupt { .. })
    });
    // FIG-2104-WITNESS-0136: lash::plugins::PluginError::StoredDataCorrupt::message
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::StoredDataCorrupt { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2104-WITNESS-0137: lash::plugins::PluginError::StoredDataCorrupt::record_kind
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::StoredDataCorrupt { record_kind, .. } = value {
            let _ = record_kind;
        }
    });
    // FIG-2104-WITNESS-0138: lash::plugins::PluginError::UnstagedUsageConfirmation
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::UnstagedUsageConfirmation { .. }
        )
    });
    // FIG-2104-WITNESS-0139: lash::plugins::PluginError::UnstagedUsageConfirmation::confirmed_count
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::UnstagedUsageConfirmation {
            confirmed_count, ..
        } = value
        {
            let _ = confirmed_count;
        }
    });
    // FIG-2104-WITNESS-0140: lash::plugins::PluginError::UnstagedUsageConfirmation::staged_count
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::UnstagedUsageConfirmation { staged_count, .. } = value {
            let _ = staged_count;
        }
    });
    // FIG-2104-WITNESS-0141: lash::plugins::PluginExtensionContribution
    type_witness::<lash::plugins::PluginExtensionContribution>();
    // FIG-2104-WITNESS-0142: lash::plugins::PluginExtensionContribution::extension_id
    field_witness(|value: &lash::plugins::PluginExtensionContribution| {
        let _ = &value.extension_id;
    });
    // FIG-2104-WITNESS-0143: lash::plugins::PluginExtensionContribution::from_value
    member_witness(|extension_id: String, payload: serde_json::Value| {
        lash::plugins::PluginExtensionContribution::from_value(extension_id, payload)
    });
    // FIG-2104-WITNESS-0144: lash::plugins::PluginExtensionContribution::new
    member_witness(|extension_id: String, payload: serde_json::Value| {
        lash::plugins::PluginExtensionContribution::new(extension_id, payload)
    });
    // FIG-2104-WITNESS-0145: lash::plugins::PluginExtensionContribution::payload
    field_witness(|value: &lash::plugins::PluginExtensionContribution| {
        let _ = &value.payload;
    });
    // FIG-2104-WITNESS-0146: lash::plugins::PluginHost
    type_witness::<lash::plugins::PluginHost>();
    // FIG-2104-WITNESS-0147: lash::plugins::PluginHost::build_session
    member_witness(|host: &lash::plugins::PluginHost, session_id: String| {
        host.build_session(session_id)
    });
    // FIG-2104-WITNESS-0148: lash::plugins::PluginHost::build_session_with_overlay
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         overlay: lash::plugins::ToolCatalogContribution,
         tool_snapshot: Option<lash::tools::ToolState>| {
            host.build_session_with_overlay(
                session_id,
                overlay,
                tool_snapshot,
                lash::plugins::SessionCreationConfig::default(),
            )
        },
    );
    // FIG-2104-WITNESS-0149: lash::plugins::PluginHost::build_session_with_parent
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         parent_session_id: Option<String>,
         authority: lash::plugins::SessionAuthorityContext| {
            host.build_session_with_parent(
                session_id,
                parent_session_id,
                lash::plugins::SessionCreationConfig {
                    authority,
                    ..Default::default()
                },
            )
        },
    );
    // FIG-2104-WITNESS-0150: lash::plugins::PluginHost::build_session_with_parent_and_overlay
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         parent_session_id: Option<String>,
         overlay: lash::plugins::ToolCatalogContribution,
         tool_snapshot: Option<lash::tools::ToolState>,
         authority: lash::plugins::SessionAuthorityContext| {
            host.build_session_with_parent_and_overlay(
                session_id,
                parent_session_id,
                overlay,
                tool_snapshot,
                lash::plugins::SessionCreationConfig {
                    authority,
                    ..Default::default()
                },
            )
        },
    );
    // FIG-2104-WITNESS-0151: lash::plugins::PluginHost::empty
    member_witness(lash::plugins::PluginHost::empty);
    // FIG-2104-WITNESS-0152: lash::plugins::PluginHost::extensions
    member_witness(lash::plugins::PluginHost::extensions);
    // FIG-2104-WITNESS-0153: lash::plugins::PluginHost::factories
    member_witness(lash::plugins::PluginHost::factories);
    // FIG-2104-WITNESS-0154: lash::plugins::PluginHost::isolated_registry
    member_witness(lash::plugins::PluginHost::isolated_registry);
    // FIG-2104-WITNESS-0155: lash::plugins::PluginHost::new
    member_witness(lash::plugins::PluginHost::new);
    // FIG-2104-WITNESS-0156: lash::plugins::PluginHost::session
    member_witness(lash::plugins::PluginHost::session);
    // FIG-2104-WITNESS-0157: lash::plugins::PluginHost::unregister_session
    member_witness(lash::plugins::PluginHost::unregister_session);
    // FIG-2104-WITNESS-0158: lash::plugins::PluginHost::with_extensions
    member_witness(lash::plugins::PluginHost::with_extensions);
    // FIG-2104-WITNESS-0159: lash::plugins::PluginLifecycleEvent::SessionRestored
    variant_witness(|value: &lash::plugins::PluginLifecycleEvent| {
        matches!(
            value,
            lash::plugins::PluginLifecycleEvent::SessionRestored(..)
        )
    });
    // FIG-2104-WITNESS-0160: lash::plugins::PluginLifecycleEvent::SessionRestored::0
    field_witness(|value: &lash::plugins::PluginLifecycleEvent| {
        if let lash::plugins::PluginLifecycleEvent::SessionRestored(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0161: lash::plugins::PluginLifecycleEvent::TurnFinalized
    variant_witness(|value: &lash::plugins::PluginLifecycleEvent| {
        matches!(
            value,
            lash::plugins::PluginLifecycleEvent::TurnFinalized(..)
        )
    });
    // FIG-2104-WITNESS-0162: lash::plugins::PluginLifecycleEvent::TurnFinalized::0
    field_witness(|value: &lash::plugins::PluginLifecycleEvent| {
        if let lash::plugins::PluginLifecycleEvent::TurnFinalized(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0163: lash::plugins::PluginMessage::content
    field_witness(|value: &lash::plugins::PluginMessage| {
        let _ = &value.content;
    });
    // FIG-2104-WITNESS-0164: lash::plugins::PluginMessage::first_text
    member_witness(lash::plugins::PluginMessage::first_text);
    // FIG-2104-WITNESS-0165: lash::plugins::PluginMessage::id
    field_witness(|value: &lash::plugins::PluginMessage| {
        let _ = &value.id;
    });
    // FIG-2104-WITNESS-0166: lash::plugins::PluginMessage::origin
    field_witness(|value: &lash::plugins::PluginMessage| {
        let _ = &value.origin;
    });
    // FIG-2104-WITNESS-0167: lash::plugins::PluginMessage::parts
    field_witness(|value: &lash::plugins::PluginMessage| {
        let _ = &value.parts;
    });
    // FIG-2104-WITNESS-0168: lash::plugins::PluginMessage::role
    field_witness(|value: &lash::plugins::PluginMessage| {
        let _ = &value.role;
    });
    // FIG-2104-WITNESS-0169: lash::plugins::PluginMessage::with_origin
    member_witness(lash::plugins::PluginMessage::with_origin);
    // FIG-2104-WITNESS-0170: lash::plugins::PluginOptions
    type_witness::<lash::plugins::PluginOptions>();
    // FIG-2104-WITNESS-0171: lash::plugins::PluginOptions::decode
    member_witness(|options: &lash::plugins::PluginOptions, plugin_id: &str| {
        options.decode::<serde_json::Value>(plugin_id)
    });
    // FIG-2104-WITNESS-0172: lash::plugins::PluginOptions::empty
    member_witness(lash::plugins::PluginOptions::empty);
    // FIG-2104-WITNESS-0173: lash::plugins::PluginOptions::insert_typed
    member_witness(
        |options: &mut lash::plugins::PluginOptions,
         plugin_id: String,
         value: serde_json::Value| { options.insert_typed(plugin_id, value) },
    );
    // FIG-2104-WITNESS-0174: lash::plugins::PluginOptions::plugins
    field_witness(|value: &lash::plugins::PluginOptions| {
        let _ = &value.plugins;
    });
    // FIG-2104-WITNESS-0175: lash::plugins::PluginOptions::typed
    member_witness(|plugin_id: String, value: serde_json::Value| {
        lash::plugins::PluginOptions::typed(plugin_id, value)
    });
    // FIG-2104-WITNESS-0176: lash::plugins::PluginRuntimeEvent
    type_witness::<lash::plugins::PluginRuntimeEvent>();
    // FIG-2104-WITNESS-0177: lash::plugins::PluginRuntimeEvent::Custom
    variant_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        matches!(value, lash::plugins::PluginRuntimeEvent::Custom { .. })
    });
    // FIG-2104-WITNESS-0178: lash::plugins::PluginRuntimeEvent::Custom::name
    field_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        if let lash::plugins::PluginRuntimeEvent::Custom { name, .. } = value {
            let _ = name;
        }
    });
    // FIG-2104-WITNESS-0179: lash::plugins::PluginRuntimeEvent::Custom::payload
    field_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        if let lash::plugins::PluginRuntimeEvent::Custom { payload, .. } = value {
            let _ = payload;
        }
    });
    // FIG-2104-WITNESS-0180: lash::plugins::PluginRuntimeEvent::Status
    variant_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        matches!(value, lash::plugins::PluginRuntimeEvent::Status { .. })
    });
    // FIG-2104-WITNESS-0181: lash::plugins::PluginRuntimeEvent::Status::detail
    field_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        if let lash::plugins::PluginRuntimeEvent::Status { detail, .. } = value {
            let _ = detail;
        }
    });
    // FIG-2104-WITNESS-0182: lash::plugins::PluginRuntimeEvent::Status::key
    field_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        if let lash::plugins::PluginRuntimeEvent::Status { key, .. } = value {
            let _ = key;
        }
    });
    // FIG-2104-WITNESS-0183: lash::plugins::PluginRuntimeEvent::Status::label
    field_witness(|value: &lash::plugins::PluginRuntimeEvent| {
        if let lash::plugins::PluginRuntimeEvent::Status { label, .. } = value {
            let _ = label;
        }
    });
    // FIG-2104-WITNESS-0184: lash::plugins::PluginSession
    type_witness::<lash::plugins::PluginSession>();
    // FIG-2104-WITNESS-0185: lash::plugins::PluginSession::apply_checkpoint
    member_witness(lash::plugins::PluginSession::apply_checkpoint);
    // FIG-2104-WITNESS-0186: lash::plugins::PluginSession::at_checkpoint
    member_witness(lash::plugins::PluginSession::at_checkpoint);
    // FIG-2104-WITNESS-0187: lash::plugins::PluginSession::collect_prompt_contributions
    member_witness(lash::plugins::PluginSession::collect_prompt_contributions);
    // FIG-2104-WITNESS-0188: lash::plugins::PluginSession::compact_context
    member_witness(lash::plugins::PluginSession::compact_context);
    // FIG-2104-WITNESS-0189: lash::plugins::PluginSession::emit_runtime_event
    member_witness(lash::plugins::PluginSession::emit_runtime_event);
    // FIG-2104-WITNESS-0190: lash::plugins::PluginSession::emit_runtime_event_with_phase_probe
    member_witness(lash::plugins::PluginSession::emit_runtime_event_with_phase_probe);
    // FIG-2104-WITNESS-0191: lash::plugins::PluginSession::extensions
    member_witness(lash::plugins::PluginSession::extensions);
    // FIG-2104-WITNESS-0192: lash::plugins::PluginSession::finalize_turn_with_phase_probe
    member_witness(lash::plugins::PluginSession::finalize_turn_with_phase_probe);
    // FIG-2104-WITNESS-0193: lash::plugins::PluginSession::finish_assistant_stream
    member_witness(lash::plugins::PluginSession::finish_assistant_stream);
    // FIG-2104-WITNESS-0194: lash::plugins::PluginSession::fork_for_child_session
    member_witness(
        |session: &lash::plugins::PluginSession,
         session_id: String,
         parent_session_id: Option<String>,
         authority: lash::plugins::SessionAuthorityContext| {
            session.fork_for_child_session(
                session_id,
                parent_session_id,
                lash::plugins::SessionCreationConfig {
                    authority,
                    ..Default::default()
                },
            )
        },
    );
    // FIG-2104-WITNESS-0195: lash::plugins::PluginSession::fork_for_session
    member_witness(
        |session: &lash::plugins::PluginSession, session_id: String| {
            session.fork_for_session(session_id, lash::plugins::SessionCreationConfig::default())
        },
    );
    // FIG-2104-WITNESS-0196: lash::plugins::PluginSession::fork_for_session_with_tool_catalog
    member_witness(
        |session: &lash::plugins::PluginSession,
         session_id: String,
         overlay: lash::plugins::ToolCatalogContribution| {
            session.fork_for_session_with_tool_catalog(
                session_id,
                overlay,
                lash::plugins::SessionCreationConfig::default(),
            )
        },
    );
    // FIG-2104-WITNESS-0197: lash::plugins::PluginSession::has_assistant_stream_finished_hooks
    member_witness(lash::plugins::PluginSession::has_assistant_stream_finished_hooks);
    // FIG-2104-WITNESS-0198: lash::plugins::PluginSession::has_assistant_stream_hooks
    member_witness(lash::plugins::PluginSession::has_assistant_stream_hooks);
    // FIG-2104-WITNESS-0199: lash::plugins::PluginSession::has_runtime_event_hooks
    member_witness(lash::plugins::PluginSession::has_runtime_event_hooks);
    // FIG-2104-WITNESS-0200: lash::plugins::PluginSession::host
    member_witness(lash::plugins::PluginSession::host);
    // FIG-2104-WITNESS-0201: lash::plugins::PluginSession::mutate_session_config
    member_witness(lash::plugins::PluginSession::mutate_session_config);
    // FIG-2104-WITNESS-0202: lash::plugins::PluginSession::plugin_operations
    member_witness(lash::plugins::PluginSession::plugin_operations);
    // FIG-2104-WITNESS-0203: lash::plugins::PluginSession::prepare_turn_context
    member_witness(lash::plugins::PluginSession::prepare_turn_context);
    // FIG-2104-WITNESS-0204: lash::plugins::PluginSession::prepare_turn_with_phase_probe
    member_witness(lash::plugins::PluginSession::prepare_turn_with_phase_probe);
    // FIG-2104-WITNESS-0205: lash::plugins::PluginSession::project_tool_result
    member_witness(lash::plugins::PluginSession::project_tool_result);
    // FIG-2104-WITNESS-0206: lash::plugins::PluginSession::protocol_driver
    member_witness(lash::plugins::PluginSession::protocol_driver);
    // FIG-2104-WITNESS-0207: lash::plugins::PluginSession::resolve_tool_catalog
    member_witness(lash::plugins::PluginSession::resolve_tool_catalog);
    // FIG-2104-WITNESS-0208: lash::plugins::PluginSession::resolved_tool_catalog
    member_witness(lash::plugins::PluginSession::resolved_tool_catalog);
    // FIG-2104-WITNESS-0209: lash::plugins::PluginSession::restore
    member_witness(lash::plugins::PluginSession::restore);
    // FIG-2104-WITNESS-0210: lash::plugins::PluginSession::session_id
    member_witness(lash::plugins::PluginSession::session_id);
    // FIG-2104-WITNESS-0211: lash::plugins::PluginSession::snapshot
    member_witness(lash::plugins::PluginSession::snapshot);
    // FIG-2104-WITNESS-0212: lash::plugins::PluginSession::snapshot_revision_fingerprint
    member_witness(lash::plugins::PluginSession::snapshot_revision_fingerprint);
    // FIG-2104-WITNESS-0213: lash::plugins::PluginSession::subagent_context
    member_witness(lash::plugins::PluginSession::subagent_context);
    // FIG-2104-WITNESS-0214: lash::plugins::PluginSession::tool_access
    member_witness(lash::plugins::PluginSession::tool_access);
    // FIG-2104-WITNESS-0215: lash::plugins::PluginSession::tool_registry
    member_witness(lash::plugins::PluginSession::tool_registry);
    // FIG-2104-WITNESS-0216: lash::plugins::PreparedContext::prompt_contributions
    field_witness(|value: &lash::plugins::PreparedContext| {
        let _ = &value.prompt_contributions;
    });
    // FIG-2104-WITNESS-0217: lash::plugins::PromptHookContext::session_id
    field_witness(|value: &lash::plugins::PromptHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0218: lash::plugins::SessionAppendNode::Plugin
    variant_witness(|value: &lash::plugins::SessionAppendNode| {
        matches!(value, lash::plugins::SessionAppendNode::Plugin { .. })
    });
    // FIG-2104-WITNESS-0219: lash::plugins::SessionAppendNode::Plugin::body
    field_witness(|value: &lash::plugins::SessionAppendNode| {
        if let lash::plugins::SessionAppendNode::Plugin { body, .. } = value {
            let _ = body;
        }
    });
    // FIG-2104-WITNESS-0220: lash::plugins::SessionAppendNode::Plugin::plugin_type
    field_witness(|value: &lash::plugins::SessionAppendNode| {
        if let lash::plugins::SessionAppendNode::Plugin { plugin_type, .. } = value {
            let _ = plugin_type;
        }
    });
    // FIG-2104-WITNESS-0221: lash::plugins::SessionStateService::apply_tool_state
    fn method_witness_0221<T: lash::plugins::SessionStateService>() {
        member_witness(T::apply_tool_state);
    }
    // FIG-2104-WITNESS-0222: lash::plugins::SessionStateService::set_tool_membership
    fn method_witness_0222<T: lash::plugins::SessionStateService>() {
        member_witness(T::set_tool_membership);
    }
    // FIG-2104-WITNESS-0223: lash::plugins::SessionStateService::shared_tool_catalog
    fn method_witness_0223<T: lash::plugins::SessionStateService>() {
        member_witness(T::shared_tool_catalog);
    }
    // FIG-2104-WITNESS-0224: lash::plugins::SessionStateService::tool_catalog
    fn method_witness_0224<T: lash::plugins::SessionStateService>() {
        member_witness(T::tool_catalog);
    }
    // FIG-2104-WITNESS-0225: lash::plugins::SessionStateService::tool_state
    fn method_witness_0225<T: lash::plugins::SessionStateService>() {
        member_witness(T::tool_state);
    }
    // FIG-2104-WITNESS-0226: lash::plugins::ToolCallHookContext
    type_witness::<lash::plugins::ToolCallHookContext>();
    // FIG-2104-WITNESS-0227: lash::plugins::ToolCallHookContext::args
    field_witness(|value: &lash::plugins::ToolCallHookContext| {
        let _ = &value.args;
    });
    // FIG-2104-WITNESS-0228: lash::plugins::ToolCallHookContext::argument_projection
    field_witness(|value: &lash::plugins::ToolCallHookContext| {
        let _ = &value.argument_projection;
    });
    // FIG-2104-WITNESS-0229: lash::plugins::ToolCallHookContext::new
    member_witness(lash::plugins::ToolCallHookContext::new);
    // FIG-2104-WITNESS-0230: lash::plugins::ToolCallHookContext::session_id
    field_witness(|value: &lash::plugins::ToolCallHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0231: lash::plugins::ToolCallHookContext::session_snapshot
    member_witness(lash::plugins::ToolCallHookContext::session_snapshot);
    // FIG-2104-WITNESS-0232: lash::plugins::ToolCallHookContext::set_tool_membership
    member_witness(lash::plugins::ToolCallHookContext::set_tool_membership);
    // FIG-2104-WITNESS-0233: lash::plugins::ToolCallHookContext::tool_name
    field_witness(|value: &lash::plugins::ToolCallHookContext| {
        let _ = &value.tool_name;
    });
    // FIG-2104-WITNESS-0234: lash::plugins::ToolCatalog::callable_tools
    member_witness(lash::plugins::ToolCatalog::callable_tools);
    // FIG-2104-WITNESS-0235: lash::plugins::ToolCatalog::from_tool_definitions
    member_witness(lash::plugins::ToolCatalog::from_tool_definitions);
    // FIG-2104-WITNESS-0236: lash::plugins::ToolCatalog::from_tools
    member_witness(lash::plugins::ToolCatalog::from_tools);
    // FIG-2104-WITNESS-0237: lash::plugins::ToolCatalog::has_callable_tool
    member_witness(lash::plugins::ToolCatalog::has_callable_tool);
    // FIG-2104-WITNESS-0238: lash::plugins::ToolCatalog::model_tool_specs
    member_witness(lash::plugins::ToolCatalog::model_tool_specs);
    // FIG-2104-WITNESS-0239: lash::plugins::ToolCatalog::resolve_contract
    member_witness(lash::plugins::ToolCatalog::resolve_contract);
    // FIG-2104-WITNESS-0240: lash::plugins::ToolCatalog::tool_names
    member_witness(lash::plugins::ToolCatalog::tool_names);
    // FIG-2104-WITNESS-0241: lash::plugins::ToolCatalog::tool_names_fingerprint
    member_witness(lash::plugins::ToolCatalog::tool_names_fingerprint);
    // FIG-2104-WITNESS-0242: lash::plugins::ToolResultHookContext
    type_witness::<lash::plugins::ToolResultHookContext>();
    // FIG-2104-WITNESS-0243: lash::plugins::ToolResultHookContext::args
    field_witness(|value: &lash::plugins::ToolResultHookContext| {
        let _ = &value.args;
    });
    // FIG-2104-WITNESS-0244: lash::plugins::ToolResultHookContext::duration_ms
    field_witness(|value: &lash::plugins::ToolResultHookContext| {
        let _ = &value.duration_ms;
    });
    // FIG-2104-WITNESS-0245: lash::plugins::ToolResultHookContext::new
    member_witness(lash::plugins::ToolResultHookContext::new);
    // FIG-2104-WITNESS-0246: lash::plugins::ToolResultHookContext::session_id
    field_witness(|value: &lash::plugins::ToolResultHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0247: lash::plugins::ToolResultHookContext::session_snapshot
    member_witness(lash::plugins::ToolResultHookContext::session_snapshot);
    // FIG-2104-WITNESS-0248: lash::plugins::ToolResultHookContext::set_tool_membership
    member_witness(lash::plugins::ToolResultHookContext::set_tool_membership);
    // FIG-2104-WITNESS-0249: lash::plugins::ToolResultHookContext::tool_name
    field_witness(|value: &lash::plugins::ToolResultHookContext| {
        let _ = &value.tool_name;
    });
    // FIG-2104-WITNESS-0250: lash::plugins::TurnHookContext::session_id
    field_witness(|value: &lash::plugins::TurnHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0251: lash::plugins::TurnResultHookContext
    type_witness::<lash::plugins::TurnResultHookContext>();
    // FIG-2104-WITNESS-0252: lash::plugins::TurnResultHookContext::session_id
    field_witness(|value: &lash::plugins::TurnResultHookContext| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0253: lash::plugins::TurnResultHookContext::turn
    field_witness(|value: &lash::plugins::TurnResultHookContext| {
        let _ = &value.turn;
    });
    // FIG-2104-WITNESS-0254: lash::plugins::TurnTransformContext::prompt_usage
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.prompt_usage;
    });
    // FIG-2104-WITNESS-0255: lash::prompt::PromptTemplate::render
    member_witness(lash::prompt::PromptTemplate::render);
    // FIG-2104-WITNESS-0256: lash::provider::ExecutionEvidence
    type_witness::<lash::provider::ExecutionEvidence>();
    // FIG-2104-WITNESS-0257: lash::provider::ExecutionEvidence::collection_interruption
    field_witness(|value: &lash::provider::ExecutionEvidence| {
        let _ = &value.collection_interruption;
    });
    // FIG-2104-WITNESS-0258: lash::provider::ExecutionEvidence::merge
    member_witness(lash::provider::ExecutionEvidence::merge);
    // FIG-2104-WITNESS-0259: lash::provider::ExecutionEvidence::merge_optional
    member_witness(lash::provider::ExecutionEvidence::merge_optional);
    // FIG-2104-WITNESS-0260: lash::provider::ExecutionEvidence::provider_finish_reason
    field_witness(|value: &lash::provider::ExecutionEvidence| {
        let _ = &value.provider_finish_reason;
    });
    // FIG-2104-WITNESS-0261: lash::provider::ExecutionEvidence::provider_request_id
    field_witness(|value: &lash::provider::ExecutionEvidence| {
        let _ = &value.provider_request_id;
    });
    // FIG-2104-WITNESS-0262: lash::provider::ExecutionEvidence::provider_response_id
    field_witness(|value: &lash::provider::ExecutionEvidence| {
        let _ = &value.provider_response_id;
    });
    // FIG-2104-WITNESS-0263: lash::provider::ExecutionEvidence::served_model
    field_witness(|value: &lash::provider::ExecutionEvidence| {
        let _ = &value.served_model;
    });
    // FIG-2104-WITNESS-0264: lash::provider::ExecutionEvidenceCollectionInterruption
    type_witness::<lash::provider::ExecutionEvidenceCollectionInterruption>();
    // FIG-2104-WITNESS-0265: lash::provider::ExecutionEvidenceCollectionInterruption::ProtocolAbort
    variant_witness(
        |value: &lash::provider::ExecutionEvidenceCollectionInterruption| {
            matches!(
                value,
                lash::provider::ExecutionEvidenceCollectionInterruption::ProtocolAbort
            )
        },
    );
    // FIG-2104-WITNESS-0266: lash::provider::ExecutionEvidenceMergeError
    type_witness::<lash::provider::ExecutionEvidenceMergeError>();
    // FIG-2104-WITNESS-0267: lash::provider::ExecutionEvidenceMergeError::BeforeResponseStart
    variant_witness(|value: &lash::provider::ExecutionEvidenceMergeError| {
        matches!(
            value,
            lash::provider::ExecutionEvidenceMergeError::BeforeResponseStart
        )
    });
    // FIG-2104-WITNESS-0268: lash::provider::ExecutionEvidenceMergeError::IdentityConflict
    variant_witness(|value: &lash::provider::ExecutionEvidenceMergeError| {
        matches!(
            value,
            lash::provider::ExecutionEvidenceMergeError::IdentityConflict { .. }
        )
    });
    // FIG-2104-WITNESS-0269: lash::provider::ExecutionEvidenceMergeError::IdentityConflict::current
    field_witness(|value: &lash::provider::ExecutionEvidenceMergeError| {
        if let lash::provider::ExecutionEvidenceMergeError::IdentityConflict { current, .. } = value
        {
            let _ = current;
        }
    });
    // FIG-2104-WITNESS-0270: lash::provider::ExecutionEvidenceMergeError::IdentityConflict::field
    field_witness(|value: &lash::provider::ExecutionEvidenceMergeError| {
        if let lash::provider::ExecutionEvidenceMergeError::IdentityConflict { field, .. } = value {
            let _ = field;
        }
    });
    // FIG-2104-WITNESS-0271: lash::provider::ExecutionEvidenceMergeError::IdentityConflict::next
    field_witness(|value: &lash::provider::ExecutionEvidenceMergeError| {
        if let lash::provider::ExecutionEvidenceMergeError::IdentityConflict { next, .. } = value {
            let _ = next;
        }
    });
    // FIG-2104-WITNESS-0272: lash::provider::ExecutionEvidenceMergeError::code
    member_witness(lash::provider::ExecutionEvidenceMergeError::code);
    // FIG-2104-WITNESS-0273: lash::provider::LlmRequest::agent_frame_id
    member_witness(lash::provider::LlmRequest::agent_frame_id);
    // FIG-2104-WITNESS-0274: lash::provider::LlmRequest::continuation_key
    member_witness(lash::provider::LlmRequest::continuation_key);
    // FIG-2104-WITNESS-0275: lash::provider::LlmRequest::generation
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.generation;
    });
    // FIG-2104-WITNESS-0276: lash::provider::LlmRequest::messages
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.messages;
    });
    // FIG-2104-WITNESS-0277: lash::provider::LlmRequest::model
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.model;
    });
    // FIG-2104-WITNESS-0278: lash::provider::LlmRequest::model_capability
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.model_capability;
    });
    // FIG-2104-WITNESS-0279: lash::provider::LlmRequest::model_variant
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.model_variant;
    });
    // FIG-2104-WITNESS-0280: lash::provider::LlmRequest::output_spec
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.output_spec;
    });
    // FIG-2104-WITNESS-0281: lash::provider::LlmRequest::provider_trace
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.provider_trace;
    });
    // FIG-2104-WITNESS-0282: lash::provider::LlmRequest::request_id
    member_witness(lash::provider::LlmRequest::request_id);
    // FIG-2104-WITNESS-0283: lash::provider::LlmRequest::resolved_stored
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.resolved_stored;
    });
    // FIG-2104-WITNESS-0284: lash::provider::LlmRequest::scope
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.scope;
    });
    // FIG-2104-WITNESS-0285: lash::provider::LlmRequest::session_id
    member_witness(lash::provider::LlmRequest::session_id);
    // FIG-2104-WITNESS-0286: lash::provider::LlmRequest::stream_events
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.stream_events;
    });
    // FIG-2104-WITNESS-0287: lash::provider::LlmRequest::tool_choice
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.tool_choice;
    });
    // FIG-2104-WITNESS-0288: lash::provider::LlmRequest::tools
    field_witness(|value: &lash::provider::LlmRequest| {
        let _ = &value.tools;
    });
    // FIG-2104-WITNESS-0289: lash::provider::LlmRequestScope
    type_witness::<lash::provider::LlmRequestScope>();
    // FIG-2104-WITNESS-0290: lash::provider::LlmRequestScope::agent_frame_id
    field_witness(|value: &lash::provider::LlmRequestScope| {
        let _ = &value.agent_frame_id;
    });
    // FIG-2104-WITNESS-0291: lash::provider::LlmRequestScope::continuation_key
    member_witness(lash::provider::LlmRequestScope::continuation_key);
    // FIG-2104-WITNESS-0292: lash::provider::LlmRequestScope::new
    member_witness(
        |session_id: String, agent_frame_id: String, request_id: String| {
            lash::provider::LlmRequestScope::new(session_id, agent_frame_id, request_id)
        },
    );
    // FIG-2104-WITNESS-0293: lash::provider::LlmRequestScope::request_id
    field_witness(|value: &lash::provider::LlmRequestScope| {
        let _ = &value.request_id;
    });
    // FIG-2104-WITNESS-0294: lash::provider::LlmRequestScope::session_id
    field_witness(|value: &lash::provider::LlmRequestScope| {
        let _ = &value.session_id;
    });
    // FIG-2104-WITNESS-0295: lash::provider::LlmResponse::execution_evidence
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.execution_evidence;
    });
    // FIG-2104-WITNESS-0296: lash::provider::LlmResponse::generation_disposition
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.generation_disposition;
    });
    // FIG-2104-WITNESS-0297: lash::provider::LlmResponse::http_summary
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.http_summary;
    });
    // FIG-2104-WITNESS-0298: lash::provider::LlmResponse::provider_usage
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.provider_usage;
    });
    // FIG-2104-WITNESS-0299: lash::provider::LlmResponse::request_body
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.request_body;
    });
    // FIG-2104-WITNESS-0300: lash::provider::LlmResponse::terminal_diagnostic
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.terminal_diagnostic;
    });
    // FIG-2104-WITNESS-0301: lash::provider::LlmResponse::terminal_reason
    field_witness(|value: &lash::provider::LlmResponse| {
        let _ = &value.terminal_reason;
    });
    // FIG-2104-WITNESS-0302: lash::provider::LlmStreamEvidence
    type_witness::<lash::provider::LlmStreamEvidence>();
    // FIG-2104-WITNESS-0303: lash::provider::LlmStreamEvidence::execution_evidence
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.execution_evidence;
    });
    // FIG-2104-WITNESS-0304: lash::provider::LlmStreamEvidence::generation_disposition
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.generation_disposition;
    });
    // FIG-2104-WITNESS-0305: lash::provider::LlmStreamEvidence::http_summary
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.http_summary;
    });
    // FIG-2104-WITNESS-0306: lash::provider::LlmStreamEvidence::merge
    member_witness(lash::provider::LlmStreamEvidence::merge);
    // FIG-2104-WITNESS-0307: lash::provider::LlmStreamEvidence::provider_usage
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.provider_usage;
    });
    // FIG-2104-WITNESS-0308: lash::provider::LlmStreamEvidence::request_body
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.request_body;
    });
    // FIG-2104-WITNESS-0309: lash::provider::LlmStreamEvidence::response_metadata
    field_witness(|value: &lash::provider::LlmStreamEvidence| {
        let _ = &value.response_metadata;
    });
    // FIG-2104-WITNESS-0310: lash::provider::LlmTransportError
    type_witness::<lash::provider::LlmTransportError>();
    // FIG-2104-WITNESS-0311: lash::provider::ProviderComponents
    type_witness::<lash::provider::ProviderComponents>();
    // FIG-2104-WITNESS-0312: lash::provider::ProviderComponents::failure_classifier
    field_witness(|value: &lash::provider::ProviderComponents| {
        let _ = &value.failure_classifier;
    });
    // FIG-2104-WITNESS-0313: lash::provider::ProviderComponents::map_provider
    member_witness(|components: lash::provider::ProviderComponents| {
        components.map_provider(|provider| provider)
    });
    // FIG-2104-WITNESS-0314: lash::provider::ProviderComponents::provider
    field_witness(|value: &lash::provider::ProviderComponents| {
        let _ = &value.provider;
    });
    // FIG-2104-WITNESS-0315: lash::provider::ProviderComponents::rate_limiter
    field_witness(|value: &lash::provider::ProviderComponents| {
        let _ = &value.rate_limiter;
    });
    // FIG-2104-WITNESS-0316: lash::provider::ProviderComponents::with_clock
    member_witness(lash::provider::ProviderComponents::with_clock);
    // FIG-2104-WITNESS-0317: lash::provider::ProviderComponents::with_failure_classifier
    member_witness(lash::provider::ProviderComponents::with_failure_classifier);
    // FIG-2104-WITNESS-0318: lash::provider::ProviderHandle
    type_witness::<lash::provider::ProviderHandle>();
    // FIG-2104-WITNESS-0319: lash::provider::ProviderHandle::complete
    member_witness(lash::provider::ProviderHandle::complete);
    // FIG-2104-WITNESS-0320: lash::provider::ProviderHandle::kind
    member_witness(lash::provider::ProviderHandle::kind);
    // FIG-2104-WITNESS-0321: lash::provider::ProviderHandle::options
    member_witness(lash::provider::ProviderHandle::options);
    // FIG-2104-WITNESS-0322: lash::provider::ProviderHandle::requires_streaming
    member_witness(lash::provider::ProviderHandle::requires_streaming);
    // FIG-2104-WITNESS-0323: lash::provider::ProviderHandle::set_options
    member_witness(lash::provider::ProviderHandle::set_options);
    // FIG-2104-WITNESS-0324: lash::provider::ProviderHandle::unconfigured
    member_witness(lash::provider::ProviderHandle::unconfigured);
    // FIG-2104-WITNESS-0325: lash::provider::ProviderHandle::with_clock
    member_witness(lash::provider::ProviderHandle::with_clock);
    // FIG-2104-WITNESS-0326: lash::provider::ProviderOptions::sse_event_bytes
    field_witness(|value: &lash::provider::ProviderOptions| {
        let _ = &value.sse_event_bytes;
    });
    // FIG-2104-WITNESS-0327: lash::provider::ProviderOptions::sse_total_bytes
    field_witness(|value: &lash::provider::ProviderOptions| {
        let _ = &value.sse_total_bytes;
    });
    // FIG-2104-WITNESS-0328: lash::runtime::AssembledTurn::tool_calls
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.tool_calls;
    });
    // FIG-2104-WITNESS-0329: lash::runtime::LlmRequestSpec::tool_choice
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.tool_choice;
    });
    // FIG-2104-WITNESS-0330: lash::runtime::LlmRequestSpec::tools
    field_witness(|value: &lash::runtime::LlmRequestSpec| {
        let _ = &value.tools;
    });
    // FIG-2104-WITNESS-0331: lash::runtime::PromptUsage
    type_witness::<lash::runtime::PromptUsage>();
    // FIG-2104-WITNESS-0332: lash::runtime::PromptUsage::cache_read_input_tokens
    field_witness(|value: &lash::runtime::PromptUsage| {
        let _ = &value.cache_read_input_tokens;
    });
    // FIG-2104-WITNESS-0333: lash::runtime::PromptUsage::cache_write_input_tokens
    field_witness(|value: &lash::runtime::PromptUsage| {
        let _ = &value.cache_write_input_tokens;
    });
    // FIG-2104-WITNESS-0334: lash::runtime::PromptUsage::context_budget_tokens
    field_witness(|value: &lash::runtime::PromptUsage| {
        let _ = &value.context_budget_tokens;
    });
    // FIG-2104-WITNESS-0335: lash::runtime::PromptUsage::input_tokens
    field_witness(|value: &lash::runtime::PromptUsage| {
        let _ = &value.input_tokens;
    });
    // FIG-2104-WITNESS-0336: lash::runtime::PromptUsage::prompt_context_tokens
    field_witness(|value: &lash::runtime::PromptUsage| {
        let _ = &value.prompt_context_tokens;
    });
    // FIG-2104-WITNESS-0337: lash::runtime::RuntimeEffectCommand::ToolAttempt
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(
            value,
            lash::runtime::RuntimeEffectCommand::ToolAttempt { .. }
        )
    });
    // FIG-2104-WITNESS-0338: lash::runtime::RuntimeEffectCommand::ToolAttempt::attempt
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::ToolAttempt { attempt, .. } = value {
            let _ = attempt;
        }
    });
    // FIG-2104-WITNESS-0339: lash::runtime::RuntimeEffectCommand::ToolAttempt::call
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::ToolAttempt { call, .. } = value {
            let _ = call;
        }
    });
    // FIG-2104-WITNESS-0340: lash::runtime::RuntimeEffectCommand::ToolAttempt::execution_grant
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::ToolAttempt {
            execution_grant, ..
        } = value
        {
            let _ = execution_grant;
        }
    });
    // FIG-2104-WITNESS-0341: lash::runtime::RuntimeEffectCommand::ToolAttempt::max_attempts
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::ToolAttempt { max_attempts, .. } = value {
            let _ = max_attempts;
        }
    });
    // FIG-2104-WITNESS-0342: lash::runtime::RuntimeEffectCommand::ToolBatch
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(value, lash::runtime::RuntimeEffectCommand::ToolBatch { .. })
    });
    // FIG-2104-WITNESS-0343: lash::runtime::RuntimeEffectCommand::ToolBatch::batch
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::ToolBatch { batch, .. } = value {
            let _ = batch;
        }
    });
    // FIG-2104-WITNESS-0344: lash::runtime::RuntimeEffectKind::ToolAttempt
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::ToolAttempt)
    });
    // FIG-2104-WITNESS-0345: lash::runtime::RuntimeEffectKind::ToolBatch
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::ToolBatch)
    });
    // FIG-2104-WITNESS-0346: lash::runtime::RuntimeEffectOutcome::ToolAttempt
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(
            value,
            lash::runtime::RuntimeEffectOutcome::ToolAttempt { .. }
        )
    });
    // FIG-2104-WITNESS-0347: lash::runtime::RuntimeEffectOutcome::ToolAttempt::launch
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::ToolAttempt { launch, .. } = value {
            let _ = launch;
        }
    });
    // FIG-2104-WITNESS-0348: lash::runtime::RuntimeEffectOutcome::ToolBatch
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(value, lash::runtime::RuntimeEffectOutcome::ToolBatch { .. })
    });
    // FIG-2104-WITNESS-0349: lash::runtime::RuntimeEffectOutcome::ToolBatch::launches
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::ToolBatch { launches, .. } = value {
            let _ = launches;
        }
    });
    // FIG-2104-WITNESS-0350: lash::runtime::RuntimeErrorCode::DurableEffectLivePluginInput
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::DurableEffectLivePluginInput
        )
    });
    // FIG-2104-WITNESS-0351: lash::runtime::RuntimeErrorCode::PluginCheckpoint
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::PluginCheckpoint)
    });
    // FIG-2104-WITNESS-0352: lash::runtime::RuntimeErrorCode::PluginFinalizeTurn
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::PluginFinalizeTurn)
    });
    // FIG-2104-WITNESS-0353: lash::runtime::RuntimeErrorCode::PluginPrepareTurn
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::PluginPrepareTurn)
    });
    // FIG-2104-WITNESS-0354: lash::runtime::RuntimeErrorCode::PluginSessionManager
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::PluginSessionManager)
    });
    // FIG-2104-WITNESS-0355: lash::runtime::SessionPolicy::prompt
    field_witness(|value: &lash::runtime::SessionPolicy| {
        let _ = &value.prompt;
    });
    // FIG-2104-WITNESS-0356: lash::runtime::SessionPolicy::provider_id
    field_witness(|value: &lash::runtime::SessionPolicy| {
        let _ = &value.provider_id;
    });
    // FIG-2104-WITNESS-0357: lash::runtime::SessionPolicy::recorded_provider_id
    member_witness(lash::runtime::SessionPolicy::recorded_provider_id);
    // FIG-2104-WITNESS-0358: lash::runtime::SessionSnapshot::last_prompt_usage
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.last_prompt_usage;
    });
    // FIG-2104-WITNESS-0359: lash::runtime::SessionSnapshot::plugin_snapshot_ref
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.plugin_snapshot_ref;
    });
    // FIG-2104-WITNESS-0360: lash::runtime::SessionSnapshot::plugin_snapshot_revision
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.plugin_snapshot_revision;
    });
    // FIG-2104-WITNESS-0361: lash::runtime::SessionSnapshot::tool_state_generation
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.tool_state_generation;
    });
    // FIG-2104-WITNESS-0362: lash::runtime::SessionSnapshot::tool_state_ref
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.tool_state_ref;
    });
    // FIG-2104-WITNESS-0363: lash::runtime::TurnContext::has_live_plugin_inputs
    member_witness(lash::runtime::TurnContext::has_live_plugin_inputs);
    // FIG-2104-WITNESS-0364: lash::runtime::TurnContext::insert_plugin_input
    member_witness(
        |context: &mut lash::runtime::TurnContext,
         plugin_id: &'static str,
         input: serde_json::Value| { context.insert_plugin_input(plugin_id, input) },
    );
    // FIG-2104-WITNESS-0365: lash::runtime::TurnContext::live_plugin_input_ids
    member_witness(lash::runtime::TurnContext::live_plugin_input_ids);
    // FIG-2104-WITNESS-0366: lash::runtime::TurnContext::plugin_input
    member_witness(
        |context: &lash::runtime::TurnContext, plugin_id: &'static str| {
            let _ = context.plugin_input::<serde_json::Value>(plugin_id);
        },
    );
    // FIG-2104-WITNESS-0367: lash::runtime::TurnContext::prompt_layer
    member_witness(lash::runtime::TurnContext::prompt_layer);
    // FIG-2104-WITNESS-0368: lash::runtime::TurnContext::provider
    member_witness(lash::runtime::TurnContext::provider);
    // FIG-2104-WITNESS-0369: lash::runtime::TurnContext::set_prompt_layer
    member_witness(lash::runtime::TurnContext::set_prompt_layer);
    // FIG-2104-WITNESS-0370: lash::runtime::TurnContext::set_provider
    member_witness(lash::runtime::TurnContext::set_provider);
    // FIG-2104-WITNESS-0372: lash::tools::PreparedToolCall::args
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.args;
    });
    // FIG-2104-WITNESS-0373: lash::tools::PreparedToolCall::call_id
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.call_id;
    });
    // FIG-2104-WITNESS-0374: lash::tools::PreparedToolCall::identity
    member_witness(lash::tools::PreparedToolCall::identity);
    // FIG-2104-WITNESS-0375: lash::tools::PreparedToolCall::prepared_payload
    field_witness(|value: &lash::tools::PreparedToolCall| {
        let _ = &value.prepared_payload;
    });
}
