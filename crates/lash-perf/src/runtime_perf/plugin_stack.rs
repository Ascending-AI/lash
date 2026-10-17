//! The benchmark harness's own plugin stack.
//!
//! The harness composes only the plugins its scenarios exercise, so it does not
//! inherit tool families it never executes.

use std::sync::Arc;

use lash_plugin_process_controls::SessionProcessAdminPluginFactory;
use lash_plugin_standard_compaction::StandardCompactionPluginFactory;

/// The stack, its process controls starting a process under the turn's run
/// with `components` (FIG-6034).
pub(crate) fn runtime_perf_plugin_stack(
    uses_standard_compaction: bool,
    include_cancel_process: bool,
    components: &[lash_core::ProcessConfigKey<()>],
) -> lash::PluginStack {
    let mut stack = lash::PluginStack::new();
    if uses_standard_compaction {
        stack.push(Arc::new(StandardCompactionPluginFactory::default()));
    }
    let processes = if include_cancel_process {
        SessionProcessAdminPluginFactory::new(
            lash_execution_contracts::process::lifetime::session_or_starter,
            lash_core::testing::run_policy_naming(components),
        )
    } else {
        SessionProcessAdminPluginFactory::without_cancel_process(
            lash_execution_contracts::process::lifetime::session_or_starter,
            lash_core::testing::run_policy_naming(components),
        )
    };
    stack.push(Arc::new(processes));
    stack
}
