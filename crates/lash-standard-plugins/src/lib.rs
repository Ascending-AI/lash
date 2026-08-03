pub mod rolling_history;

use std::sync::Arc;

use lash_core::plugin::{PluginSpec, StaticPluginFactory};
use lash_core::{ToolProvider, facade_support::PluginStack};
use lash_plugin_process_controls::SessionProcessAdminPluginFactory;
use lash_plugin_tool_output_budget::{ToolOutputBudgetPluginFactory, tool_output_budget_stack};
use lash_tools::files::{edit_provider, glob_provider, read_file_provider, write_provider};
use lash_tools::shell::StandardShellPluginFactory;
use lash_tools::web::{fetch_url_provider, web_search_provider};
pub use rolling_history::RollingHistoryConfig;
use rolling_history::RollingHistoryPluginFactory;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StandardContextApproachKind {
    RollingHistory,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StandardContextApproach {
    RollingHistory(RollingHistoryConfig),
}

impl Default for StandardContextApproach {
    fn default() -> Self {
        Self::RollingHistory(RollingHistoryConfig)
    }
}

impl StandardContextApproach {
    pub fn kind(&self) -> StandardContextApproachKind {
        match self {
            Self::RollingHistory(_) => StandardContextApproachKind::RollingHistory,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StandardToolStackOptions {
    pub standard_context_approach: Option<StandardContextApproach>,
    pub tavily_api_key: Option<String>,
    pub include_cancel_process: bool,
}

impl Default for StandardToolStackOptions {
    fn default() -> Self {
        Self {
            standard_context_approach: None,
            tavily_api_key: None,
            include_cancel_process: true,
        }
    }
}

pub fn standard_tool_stack(options: StandardToolStackOptions) -> PluginStack {
    let mut stack = PluginStack::new();
    push_core_runtime_tools(&mut stack);
    push_standard_context_tools(&mut stack, options.standard_context_approach.as_ref());
    push_local_runtime_tools(&mut stack, options.include_cancel_process);
    if let Some(key) = options.tavily_api_key {
        push_web_tools(&mut stack, key);
    }
    stack
}

pub fn locked_down_rlm_plugin_stack() -> PluginStack {
    tool_output_budget_stack()
}

fn push_core_runtime_tools(stack: &mut PluginStack) {
    stack.push(Arc::new(ToolOutputBudgetPluginFactory::default()));
}

fn push_standard_context_tools(
    stack: &mut PluginStack,
    standard_context_approach: Option<&StandardContextApproach>,
) {
    match standard_context_approach {
        Some(StandardContextApproach::RollingHistory(config)) => {
            stack.push(Arc::new(RollingHistoryPluginFactory::new(config.clone())));
        }
        None => {}
    }
}

fn push_local_runtime_tools(stack: &mut PluginStack, include_cancel_process: bool) {
    let processes = if include_cancel_process {
        SessionProcessAdminPluginFactory::new()
    } else {
        SessionProcessAdminPluginFactory::without_cancel_process()
    };
    stack.push(Arc::new(processes));
    stack.push(Arc::new(StandardShellPluginFactory::new()));
    stack.push(Arc::new(StaticPluginFactory::new(
        "edit",
        PluginSpec::new().with_tool_provider(Arc::new(edit_provider()) as Arc<dyn ToolProvider>),
    )));
    stack.push(Arc::new(StaticPluginFactory::new(
        "write",
        PluginSpec::new().with_tool_provider(Arc::new(write_provider()) as Arc<dyn ToolProvider>),
    )));
    stack.push(Arc::new(StaticPluginFactory::new(
        "read_file",
        PluginSpec::new()
            .with_tool_provider(Arc::new(read_file_provider()) as Arc<dyn ToolProvider>),
    )));
    stack.push(Arc::new(StaticPluginFactory::new(
        "glob",
        PluginSpec::new().with_tool_provider(Arc::new(glob_provider()) as Arc<dyn ToolProvider>),
    )));
}

fn push_web_tools(stack: &mut PluginStack, tavily_api_key: String) {
    let search_key = tavily_api_key.clone();
    stack.push(Arc::new(StaticPluginFactory::new(
        "search_web",
        PluginSpec::new()
            .with_tool_provider(Arc::new(web_search_provider(search_key)) as Arc<dyn ToolProvider>),
    )));
    stack.push(Arc::new(StaticPluginFactory::new(
        "fetch_url",
        PluginSpec::new().with_tool_provider(
            Arc::new(fetch_url_provider(tavily_api_key)) as Arc<dyn ToolProvider>
        ),
    )));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack_ids(stack: &PluginStack) -> Vec<&'static str> {
        stack
            .factories()
            .iter()
            .map(|factory| factory.id())
            .collect()
    }

    fn tool_names_for_stack(
        protocol_factories: Vec<Arc<dyn lash_core::plugin::PluginFactory>>,
        standard_context_approach: Option<StandardContextApproach>,
        include_cancel_process: bool,
    ) -> Vec<String> {
        let mut factories = standard_tool_stack(StandardToolStackOptions {
            standard_context_approach: standard_context_approach.clone(),
            include_cancel_process,
            ..Default::default()
        })
        .into_factories();
        factories.extend(protocol_factories);
        let host = lash_core::facade_support::PluginHost::new(factories);
        let session_id = "test".to_string();
        let session = host
            .build_session(session_id.clone(), None)
            .expect("session");
        session
            .resolved_tool_catalog(&session_id)
            .expect("tool catalog")
            .tool_names()
            .as_ref()
            .clone()
    }

    #[test]
    fn rolling_history_context_installs_rolling_history_only() {
        let stack = standard_tool_stack(StandardToolStackOptions {
            standard_context_approach: Some(StandardContextApproach::RollingHistory(
                Default::default(),
            )),
            tavily_api_key: None,
            include_cancel_process: true,
        });
        let ids = stack_ids(&stack);

        assert!(ids.contains(&"rolling_history"));
    }

    #[test]
    fn web_tools_are_explicitly_keyed() {
        let without_web = stack_ids(&standard_tool_stack(StandardToolStackOptions::default()));
        let with_web = stack_ids(&standard_tool_stack(StandardToolStackOptions {
            tavily_api_key: Some("key".to_string()),
            ..Default::default()
        }));

        assert!(!without_web.contains(&"search_web"));
        assert!(with_web.contains(&"search_web"));
        assert!(with_web.contains(&"fetch_url"));
    }

    #[test]
    fn standard_stack_does_not_install_cli_local_grep() {
        let ids = stack_ids(&standard_tool_stack(StandardToolStackOptions::default()));

        assert!(!ids.contains(&"grep"));
    }

    #[test]
    fn standard_stack_exposes_glob_and_read_without_ls() {
        let names = tool_names_for_stack(
            lash_core::testing::test_standard_protocol_factories(),
            None,
            true,
        );

        assert!(names.contains(&"glob".to_string()));
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"edit".to_string()));
        assert!(names.contains(&"write".to_string()));
        assert!(!names.contains(&"ls".to_string()));
    }

    #[test]
    fn shared_stack_exposes_process_list_in_rlm_without_cancel_tool() {
        let standard_names = tool_names_for_stack(
            lash_core::testing::test_standard_protocol_factories(),
            Some(StandardContextApproach::default()),
            true,
        );
        let rlm_names = tool_names_for_stack(
            lash_core::testing::test_code_protocol_factories(),
            None,
            false,
        );

        assert!(standard_names.contains(&"list_process_handles".to_string()));
        assert!(standard_names.contains(&"cancel_process".to_string()));
        assert!(rlm_names.contains(&"list_process_handles".to_string()));
        assert!(!rlm_names.contains(&"cancel_process".to_string()));
    }
}
