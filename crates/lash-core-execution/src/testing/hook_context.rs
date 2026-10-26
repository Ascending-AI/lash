//! Contexts for directly exercising a plugin tool hook.
use super::{MockSessionManager, standard_test_policy};
use crate::SessionPolicy;

/// Build a `SessionPolicy` populated with the canonical test model used by
/// lash's in-tree tests.
pub fn mock_session_policy() -> SessionPolicy {
    standard_test_policy()
}

/// The context a tool hook of a session's call to `tool_name` reads, built
/// for a test that calls the hook directly.
pub fn tool_hook_context(
    tool_name: &str,
    argument_projection: lash_vocabulary::tool_contract::ToolArgumentProjectionPolicy,
) -> crate::plugin::ToolHookContext {
    crate::plugin::ToolHookContext {
        owner: lash_vocabulary::identity::RuntimeOwner::Session(
            lash_kernel_value::identity::SessionId::from("session"),
        ),
        call_id: crate::ToolCallId::fixture("hook"),
        tool_id: lash_vocabulary::tool_contract::ToolId::from(format!("tool:{tool_name}")),
        tool_name: tool_name.to_string(),
        config: crate::OwnerConfigView::session(Default::default()),
        argument_projection,
        turn_context: crate::TurnContext::default(),
        check_phase: crate::plugin::ControlPhaseDeadline::new(
            &crate::SystemClock,
            "tool_args",
            crate::ExecutionBudgets::recommended().tool_check_phase(),
        ),
        clock: std::sync::Arc::new(crate::SystemClock),
        sessions: std::sync::Arc::new(MockSessionManager::default()),
        trace: Default::default(),
    }
}
