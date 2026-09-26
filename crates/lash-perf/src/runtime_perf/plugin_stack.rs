//! The benchmark harness's own plugin stack.
//!
//! The harness composes only the plugins its scenarios exercise, so it does not
//! inherit tool families it never drives.

use std::sync::Arc;

use lash_plugin_process_controls::SessionProcessAdminPluginFactory;
use lash_plugin_standard_compaction::StandardCompactionPluginFactory;
use lash_plugin_tool_output_budget::ToolOutputBudgetPluginFactory;

pub(crate) fn runtime_perf_plugin_stack(
    uses_standard_compaction: bool,
    include_cancel_process: bool,
) -> lash::PluginStack {
    let mut stack = lash::PluginStack::new();
    stack.push(Arc::new(ToolOutputBudgetPluginFactory::default()));
    if uses_standard_compaction {
        stack.push(Arc::new(StandardCompactionPluginFactory::default()));
    }
    let processes = if include_cancel_process {
        SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter)
    } else {
        SessionProcessAdminPluginFactory::without_cancel_process(
            lash_core::lifetime::session_or_starter,
        )
    };
    stack.push(Arc::new(processes));
    stack
}
