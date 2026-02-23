use std::collections::BTreeSet;
use std::sync::Arc;

use crate::{ProgressSender, ToolDefinition, ToolProvider, ToolResult};

#[derive(Clone)]
pub struct FilteredTools {
    inner: Arc<dyn ToolProvider>,
    allowed: Arc<BTreeSet<String>>,
    definitions_cache: Vec<ToolDefinition>,
}

impl FilteredTools {
    pub fn new(inner: Arc<dyn ToolProvider>, allowed: BTreeSet<String>) -> Self {
        let definitions_cache = inner
            .definitions()
            .into_iter()
            .filter(|d| allowed.contains(&d.name))
            .collect();
        Self {
            inner,
            allowed: Arc::new(allowed),
            definitions_cache,
        }
    }

    fn allows(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }
}

#[async_trait::async_trait]
impl ToolProvider for FilteredTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions_cache.clone()
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        if !self.allows(name) {
            return ToolResult::err_fmt(format_args!("Unknown tool: {name}"));
        }
        self.inner.execute(name, args).await
    }

    async fn execute_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        if !self.allows(name) {
            return ToolResult::err_fmt(format_args!("Unknown tool: {name}"));
        }
        self.inner.execute_streaming(name, args, progress).await
    }
}
