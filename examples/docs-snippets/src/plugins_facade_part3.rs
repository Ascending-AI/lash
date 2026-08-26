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
    // FIG-2104-WITNESS-0751: lash::tools::ToolIntentParentEndOutcome::Abandoned
    variant_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        matches!(
            value,
            lash::tools::ToolIntentParentEndOutcome::Abandoned { .. }
        )
    });
    // FIG-2104-WITNESS-0752: lash::tools::ToolIntentParentEndOutcome::Abandoned::identity
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Abandoned { identity, .. } = value {
            let _ = identity;
        }
    });
    // FIG-2104-WITNESS-0753: lash::tools::ToolIntentParentEndOutcome::Abandoned::process_id
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Abandoned { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // FIG-2104-WITNESS-0754: lash::tools::ToolIntentParentEndOutcome::Cancelled
    variant_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        matches!(
            value,
            lash::tools::ToolIntentParentEndOutcome::Cancelled { .. }
        )
    });
    // FIG-2104-WITNESS-0755: lash::tools::ToolIntentParentEndOutcome::Cancelled::identity
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Cancelled { identity, .. } = value {
            let _ = identity;
        }
    });
    // FIG-2104-WITNESS-0756: lash::tools::ToolIntentParentEndOutcome::Cancelled::process_id
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Cancelled { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // FIG-2104-WITNESS-0757: lash::tools::ToolIntentParentEndOutcome::Refused
    variant_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        matches!(
            value,
            lash::tools::ToolIntentParentEndOutcome::Refused { .. }
        )
    });
    // FIG-2104-WITNESS-0758: lash::tools::ToolIntentParentEndOutcome::Refused::code
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Refused { code, .. } = value {
            let _ = code;
        }
    });
    // FIG-2104-WITNESS-0759: lash::tools::ToolIntentParentEndOutcome::Refused::identity
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Refused { identity, .. } = value {
            let _ = identity;
        }
    });
    // FIG-2104-WITNESS-0760: lash::tools::ToolIntentParentEndOutcome::Refused::message
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Refused { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2104-WITNESS-0761: lash::tools::ToolIntentParentEndOutcome::Refused::process_id
    field_witness(|value: &lash::tools::ToolIntentParentEndOutcome| {
        if let lash::tools::ToolIntentParentEndOutcome::Refused { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // FIG-2104-WITNESS-0762: lash::tools::ToolIntentParentEndAction
    type_witness::<lash::tools::ToolIntentParentEndAction>();
    // FIG-2104-WITNESS-0763: lash::tools::ToolIntentParentEndAction::identity
    field_witness(|value: &lash::tools::ToolIntentParentEndAction| {
        let _ = &value.identity;
    });
    // FIG-2104-WITNESS-0764: lash::tools::ToolIntentParentEndAction::parent_end
    field_witness(|value: &lash::tools::ToolIntentParentEndAction| {
        let _ = &value.parent_end;
    });
    // FIG-2104-WITNESS-0765: lash::tools::StaticToolExecute::attempt_may_defer
    fn method_witness_0765<T: lash::tools::StaticToolExecute>() {
        member_witness(T::attempt_may_defer);
    }
    // FIG-2104-WITNESS-0766: lash::tools::StaticToolExecute::execute_attempt
    fn method_witness_0766<T: lash::tools::StaticToolExecute>() {
        member_witness(T::execute_attempt);
    }
    // FIG-2104-WITNESS-0767: lash::plugins::PluginSessionContext::protocol_turn_options
    field_witness(|value: &lash::plugins::PluginSessionContext| {
        let _ = &value.protocol_turn_options;
    });
    // FIG-2104-WITNESS-0768: lash::plugins::SessionCreationConfig::protocol_turn_options
    field_witness(|value: &lash::plugins::SessionCreationConfig| {
        let _ = &value.protocol_turn_options;
    });
    // FIG-2104-WITNESS-0769: lash::provider::LlmContentBlock::Attachment
    variant_witness(|value: &lash::provider::LlmContentBlock| {
        matches!(value, lash::provider::LlmContentBlock::Attachment { .. })
    });
    // FIG-2104-WITNESS-0770: lash::provider::LlmContentBlock::Attachment::attachment_idx
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::Attachment { attachment_idx, .. } = value {
            let _ = attachment_idx;
        }
    });
    // FIG-2104-WITNESS-0771: lash::provider::LlmContentBlock::Reasoning
    variant_witness(|value: &lash::provider::LlmContentBlock| {
        matches!(value, lash::provider::LlmContentBlock::Reasoning { .. })
    });
    // FIG-2104-WITNESS-0772: lash::provider::LlmContentBlock::Reasoning::replay
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::Reasoning { replay, .. } = value {
            let _ = replay;
        }
    });
    // FIG-2104-WITNESS-0773: lash::provider::LlmContentBlock::Reasoning::text
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::Reasoning { text, .. } = value {
            let _ = text;
        }
    });
    // FIG-2104-WITNESS-0774: lash::provider::LlmContentBlock::Text::cache_breakpoint
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::Text {
            cache_breakpoint, ..
        } = value
        {
            let _ = cache_breakpoint;
        }
    });
    // FIG-2104-WITNESS-0775: lash::provider::LlmContentBlock::Text::response_meta
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::Text { response_meta, .. } = value {
            let _ = response_meta;
        }
    });
    // FIG-2104-WITNESS-0776: lash::provider::LlmContentBlock::ToolCall
    variant_witness(|value: &lash::provider::LlmContentBlock| {
        matches!(value, lash::provider::LlmContentBlock::ToolCall { .. })
    });
    // FIG-2104-WITNESS-0777: lash::provider::LlmContentBlock::ToolCall::call_id
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolCall { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // FIG-2104-WITNESS-0778: lash::provider::LlmContentBlock::ToolCall::input_json
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolCall { input_json, .. } = value {
            let _ = input_json;
        }
    });
    // FIG-2104-WITNESS-0779: lash::provider::LlmContentBlock::ToolCall::replay
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolCall { replay, .. } = value {
            let _ = replay;
        }
    });
    // FIG-2104-WITNESS-0780: lash::provider::LlmContentBlock::ToolCall::tool_name
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolCall { tool_name, .. } = value {
            let _ = tool_name;
        }
    });
    // FIG-2104-WITNESS-0781: lash::provider::LlmContentBlock::ToolResult
    variant_witness(|value: &lash::provider::LlmContentBlock| {
        matches!(value, lash::provider::LlmContentBlock::ToolResult { .. })
    });
    // FIG-2104-WITNESS-0782: lash::provider::LlmContentBlock::ToolResult::call_id
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolResult { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // FIG-2104-WITNESS-0783: lash::provider::LlmContentBlock::ToolResult::content
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolResult { content, .. } = value {
            let _ = content;
        }
    });
    // FIG-2104-WITNESS-0784: lash::provider::LlmContentBlock::ToolResult::tool_name
    field_witness(|value: &lash::provider::LlmContentBlock| {
        if let lash::provider::LlmContentBlock::ToolResult { tool_name, .. } = value {
            let _ = tool_name;
        }
    });
    // FIG-2104-WITNESS-0785: lash::provider::LlmMessage::is_blank
    member_witness(lash::provider::LlmMessage::is_blank);
    // FIG-2104-WITNESS-0786: lash::provider::LlmMessage::new
    member_witness(lash::provider::LlmMessage::new);
    // FIG-2104-WITNESS-0787: lash::provider::LlmMessage::text
    member_witness(|role: lash::provider::LlmRole, text: String| {
        lash::provider::LlmMessage::text(role, text)
    });
    // FIG-2104-WITNESS-0788: lash::provider::LlmRole::System
    variant_witness(|value: &lash::provider::LlmRole| {
        matches!(value, lash::provider::LlmRole::System)
    });
    // FIG-2104-WITNESS-0789: lash::provider::LlmRole::User
    variant_witness(|value: &lash::provider::LlmRole| {
        matches!(value, lash::provider::LlmRole::User)
    });
    // FIG-2104-WITNESS-0790: lash::tools::ToolBatchReplies::settled_in_input_order
    member_witness(lash::tools::ToolBatchReplies::settled_in_input_order);
    // FIG-2138-WITNESS-0791: lash::plugins::PluginSessionContext::materialization
    field_witness(|value: &lash::plugins::PluginSessionContext| {
        let _ = value.materialization;
    });
    // FIG-2138-WITNESS-0792: lash::plugins::PluginSessionMaterialization
    type_witness::<lash::plugins::PluginSessionMaterialization>();
    // FIG-2138-WITNESS-0793: lash::plugins::PluginSessionMaterialization::Creation
    variant_witness(|value: &lash::plugins::PluginSessionMaterialization| {
        matches!(value, lash::plugins::PluginSessionMaterialization::Creation)
    });
    // FIG-2138-WITNESS-0794: lash::plugins::PluginSessionMaterialization::Rematerialization
    variant_witness(|value: &lash::plugins::PluginSessionMaterialization| {
        matches!(
            value,
            lash::plugins::PluginSessionMaterialization::Rematerialization
        )
    });
    // FIG-2138-WITNESS-0795: lash::plugins::SessionCreationConfig
    type_witness::<lash::plugins::SessionCreationConfig>();
    // FIG-2138-WITNESS-0796: lash::plugins::SessionCreationConfig::authority
    field_witness(|value: &lash::plugins::SessionCreationConfig| {
        let _ = &value.authority;
    });
    // FIG-2138-WITNESS-0797: lash::plugins::RecordedSessionConfig
    type_witness::<lash::plugins::RecordedSessionConfig>();
    // FIG-2138-WITNESS-0798: lash::plugins::RecordedSessionConfig::authority
    field_witness(|value: &lash::plugins::RecordedSessionConfig| {
        let _ = &value.authority;
    });
    // FIG-2138-WITNESS-0799: lash::plugins::RecordedSessionConfig::protocol_turn_options
    field_witness(|value: &lash::plugins::RecordedSessionConfig| {
        let _ = &value.protocol_turn_options;
    });
    // FIG-2138-WITNESS-0800: lash::plugins::RecordedSessionConfig::new
    member_witness(lash::plugins::RecordedSessionConfig::new);
    // FIG-2138-WITNESS-0801: lash::plugins::PluginHost::rematerialize_session
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         snapshot: &lash::plugins::PluginSessionSnapshot,
         config: lash::plugins::RecordedSessionConfig| {
            host.rematerialize_session(session_id, snapshot, config)
        },
    );
    // FIG-2138-WITNESS-0802: lash::plugins::PluginHost::rematerialize_session_with_overlay
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         snapshot: &lash::plugins::PluginSessionSnapshot,
         overlay: lash::plugins::ToolCatalogContribution,
         tool_snapshot: Option<lash::tools::ToolState>,
         config: lash::plugins::RecordedSessionConfig| {
            host.rematerialize_session_with_overlay(
                session_id,
                snapshot,
                overlay,
                tool_snapshot,
                config,
            )
        },
    );
    // FIG-2138-WITNESS-0803: lash::plugins::PluginHost::rematerialize_session_with_parent
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         parent_session_id: Option<String>,
         snapshot: &lash::plugins::PluginSessionSnapshot,
         config: lash::plugins::RecordedSessionConfig| {
            host.rematerialize_session_with_parent(session_id, parent_session_id, snapshot, config)
        },
    );
    // FIG-2138-WITNESS-0804: lash::plugins::PluginHost::rematerialize_session_with_parent_and_overlay
    member_witness(
        |host: &lash::plugins::PluginHost,
         session_id: String,
         parent_session_id: Option<String>,
         snapshot: &lash::plugins::PluginSessionSnapshot,
         overlay: lash::plugins::ToolCatalogContribution,
         tool_snapshot: Option<lash::tools::ToolState>,
         config: lash::plugins::RecordedSessionConfig| {
            host.rematerialize_session_with_parent_and_overlay(
                session_id,
                parent_session_id,
                snapshot,
                overlay,
                tool_snapshot,
                config,
            )
        },
    );
    // FIG-2138-WITNESS-0805: lash::plugins::PluginError::MissingRecordedSessionConfig
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::MissingRecordedSessionConfig { .. }
        )
    });
    // FIG-2138-WITNESS-0806..0807: lash::plugins::PluginError::MissingRecordedSessionConfig fields
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::MissingRecordedSessionConfig { plugin_id, field } = value
        {
            let _ = (plugin_id, field);
        }
    });
    // FIG-2138-WITNESS-0808: lash::SessionError::Plugin
    variant_witness(|value: &lash::SessionError| matches!(value, lash::SessionError::Plugin(..)));
    // FIG-2104-CLOSURE-0001: lash::tools::ToolAttachmentClient::put
    member_witness(lash::tools::ToolAttachmentClient::put);
    // FIG-2104-CLOSURE-0002: lash::tools::ToolDirectCompletionClient::complete
    member_witness(lash::tools::ToolDirectCompletionClient::complete);
    // FIG-2104-CLOSURE-0003: lash::tools::ToolDispatchClient::batch
    member_witness(lash::tools::ToolDispatchClient::batch);
    // FIG-2104-CLOSURE-0004: lash::tools::ToolDispatchClient::callable_tool_manifest
    member_witness(lash::tools::ToolDispatchClient::callable_tool_manifest);
    // FIG-2104-CLOSURE-0005: lash::tools::ToolProcessEventClient::emit
    member_witness(
        |client: &lash::tools::ToolProcessEventClient,
         event_type: String,
         payload: serde_json::Value| {
            std::mem::drop(client.emit(event_type, payload));
        },
    );
    // FIG-2104-CLOSURE-0006: lash::tools::ToolProcessEventClient::emit_request
    member_witness(lash::tools::ToolProcessEventClient::emit_request);
    // FIG-2104-CLOSURE-0007: lash::tools::ToolProcessEventClient::wait_event_after
    member_witness(lash::tools::ToolProcessEventClient::wait_event_after);
    // FIG-2104-CLOSURE-0008: lash::tools::ToolSessionAdmin::close_session
    member_witness(lash::tools::ToolSessionAdmin::close_session);
    // FIG-2104-CLOSURE-0009: lash::tools::ToolSessionAdmin::create_session
    member_witness(lash::tools::ToolSessionAdmin::create_session);
    // FIG-2104-CLOSURE-0010: lash::tools::ToolSessionAdmin::model
    member_witness(lash::tools::ToolSessionAdmin::model);
    // FIG-2104-CLOSURE-0011: lash::tools::ToolSessionAdmin::set_tool_membership
    member_witness(lash::tools::ToolSessionAdmin::set_tool_membership);
    // FIG-2104-CLOSURE-0012: lash::tools::ToolSessionAdmin::shared_tool_catalog
    member_witness(lash::tools::ToolSessionAdmin::shared_tool_catalog);
    // FIG-2104-CLOSURE-0013: lash::tools::ToolSessionAdmin::snapshot
    member_witness(
        |sessions: &lash::tools::ToolSessionAdmin<'_>, session_id: String| {
            std::mem::drop(sessions.snapshot(session_id));
        },
    );
    // FIG-2104-CLOSURE-0014: lash::tools::ToolSessionAdmin::snapshot_current
    member_witness(lash::tools::ToolSessionAdmin::snapshot_current);
    // FIG-2104-CLOSURE-0015: lash::tools::ToolSessionAdmin::start_turn
    member_witness(lash::tools::ToolSessionAdmin::start_turn);
    // FIG-2104-CLOSURE-0016: lash::tools::ToolSessionAdmin::tool_catalog
    member_witness(lash::tools::ToolSessionAdmin::tool_catalog);
    // FIG-2104-CLOSURE-0017: lash::tools::OrchestrationContext::cancel_process
    member_witness(lash::tools::OrchestrationContext::cancel_process);
    // FIG-2104-CLOSURE-0018: lash::tools::OrchestrationContext::prepared_payload
    member_witness(lash::tools::OrchestrationContext::prepared_payload);
    // FIG-2104-CLOSURE-0019: lash::tools::OrchestrationContext::signal_process
    member_witness(
        |context: &lash::tools::OrchestrationContext<'_>,
         process_id: &str,
         signal_name: &str,
         signal_id: String,
         payload: serde_json::Value| {
            std::mem::drop(context.signal_process(process_id, signal_name, signal_id, payload));
        },
    );
    // FIG-2104-CLOSURE-0020..0022: ToolStateFacadeOps read surface
    fn tool_state_read_witnesses<T: lash::tools::ToolStateFacadeOps>() {
        member_witness(T::contains);
        member_witness(T::generation);
        member_witness(T::tool_manifests);
    }
}
