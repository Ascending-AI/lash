//! The benchmark harness's own plugin stack.
//!
//! It replaces the deleted `lash-standard-plugins` bundle: the harness composes
//! only the plugins its scenarios exercise, so it does not inherit the shell or
//! web tool families the bundle used to install.

use std::sync::Arc;

use lash_plugin_process_controls::SessionProcessAdminPluginFactory;
use lash_plugin_rolling_history::RollingHistoryPluginFactory;
use lash_plugin_tool_output_budget::ToolOutputBudgetPluginFactory;

pub(crate) fn runtime_perf_plugin_stack(
    uses_rolling_history: bool,
    include_cancel_process: bool,
) -> lash::PluginStack {
    let mut stack = lash::PluginStack::new();
    stack.push(Arc::new(ToolOutputBudgetPluginFactory::default()));
    if uses_rolling_history {
        stack.push(Arc::new(RollingHistoryPluginFactory::default()));
    }
    let processes = if include_cancel_process {
        SessionProcessAdminPluginFactory::new()
    } else {
        SessionProcessAdminPluginFactory::without_cancel_process()
    };
    stack.push(Arc::new(processes));
    stack
}
