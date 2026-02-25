use std::collections::BTreeMap;
use std::ops::{Add, Sub};
use std::path::PathBuf;
use std::sync::Arc;

use crate::capabilities::{CapabilityId, tools_for_capability};
use crate::{ProgressSender, ToolDefinition, ToolProvider, ToolResult};

/// Dependencies for constructing the default tool set.
#[derive(Default)]
pub struct ToolSetDeps {
    #[cfg(feature = "sqlite-store")]
    pub store: Option<Arc<crate::store::Store>>,
    pub tavily_api_key: Option<String>,
    pub skill_dirs: Option<Vec<PathBuf>>,
}

/// A composable set of tools supporting set-algebra operators.
///
/// ```rust,ignore
/// let tools = ToolSet::defaults(deps) + my_custom_tool - "shell";
/// ```
pub struct ToolSet {
    tools: BTreeMap<String, (ToolDefinition, Arc<dyn ToolProvider>)>,
}

impl ToolSet {
    /// Create an empty tool set.
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// Core zero-dependency tools.
    pub fn core() -> Self {
        Self::new()
            + super::Shell::new()
            + super::ReadFile::new()
            + super::WriteFile
            + super::EditFile
            + super::FindReplace
            + super::Glob
            + super::Grep
            + super::Ls
            + super::PlanMode::new()
    }

    /// Default tools: core + store/web/skills based on provided deps.
    pub fn defaults(deps: ToolSetDeps) -> Self {
        let mut set = Self::core();

        #[cfg(feature = "sqlite-store")]
        if let Some(ref store) = deps.store {
            set = set + super::TaskStore::new(Arc::clone(store));
            set = set
                + super::StateStore::new(
                    Arc::clone(store),
                    deps.skill_dirs.clone().unwrap_or_default(),
                );
        }

        if let Some(key) = deps.tavily_api_key {
            set = set + super::WebSearch::new(key.clone()) + super::FetchUrl::new(key);
        }

        if let Some(dirs) = deps.skill_dirs {
            set = set + super::SkillStore::new(dirs);
        }

        set
    }

    fn insert_provider(mut self, provider: impl ToolProvider) -> Self {
        let arc: Arc<dyn ToolProvider> = Arc::new(provider);
        for def in arc.definitions() {
            self.tools.insert(def.name.clone(), (def, Arc::clone(&arc)));
        }
        self
    }

    fn insert_arc(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        for def in provider.definitions() {
            self.tools
                .insert(def.name.clone(), (def, Arc::clone(&provider)));
        }
        self
    }
}

impl Default for ToolSet {
    fn default() -> Self {
        Self::new()
    }
}

// ── Operator: ToolSet + impl ToolProvider ──

impl<T: ToolProvider> Add<T> for ToolSet {
    type Output = ToolSet;
    fn add(self, rhs: T) -> ToolSet {
        self.insert_provider(rhs)
    }
}

// ── Operator: ToolSet + Arc<dyn ToolProvider> ──

impl Add<Arc<dyn ToolProvider>> for ToolSet {
    type Output = ToolSet;
    fn add(self, rhs: Arc<dyn ToolProvider>) -> ToolSet {
        self.insert_arc(rhs)
    }
}

// ── Operator: ToolSet - &str (remove by name) ──

impl Sub<&str> for ToolSet {
    type Output = ToolSet;
    fn sub(mut self, rhs: &str) -> ToolSet {
        self.tools.remove(rhs);
        self
    }
}

// ── Operator: ToolSet - CapabilityId (remove by capability) ──

impl Sub<CapabilityId> for ToolSet {
    type Output = ToolSet;
    fn sub(mut self, rhs: CapabilityId) -> ToolSet {
        for name in tools_for_capability(rhs) {
            self.tools.remove(*name);
        }
        self
    }
}

// ── ToolProvider impl ──

#[async_trait::async_trait]
impl ToolProvider for ToolSet {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|(def, _)| def.clone()).collect()
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        match self.tools.get(name) {
            Some((_, provider)) => provider.execute(name, args).await,
            None => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        }
    }

    async fn execute_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some((_, provider)) => provider.execute_streaming(name, args, progress).await,
            None => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolParam;

    struct MockAlpha;

    #[async_trait::async_trait]
    impl ToolProvider for MockAlpha {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "alpha".into(),
                description: "Alpha tool".into(),
                params: vec![],
                returns: "str".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            }]
        }
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolResult {
            ToolResult::ok(serde_json::json!("alpha_result"))
        }
    }

    struct MockBeta;

    #[async_trait::async_trait]
    impl ToolProvider for MockBeta {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "beta".into(),
                description: "Beta tool".into(),
                params: vec![ToolParam::typed("x", "int")],
                returns: "int".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            }]
        }
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolResult {
            ToolResult::ok(serde_json::json!(42))
        }
    }

    /// Provider that exposes two tools under one struct.
    struct MockMulti;

    #[async_trait::async_trait]
    impl ToolProvider for MockMulti {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition {
                    name: "multi_a".into(),
                    description: String::new(),
                    params: vec![],
                    returns: "str".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                },
                ToolDefinition {
                    name: "multi_b".into(),
                    description: String::new(),
                    params: vec![],
                    returns: "str".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                },
            ]
        }
        async fn execute(&self, name: &str, _args: &serde_json::Value) -> ToolResult {
            ToolResult::ok(serde_json::json!(format!("{name}_result")))
        }
    }

    #[test]
    fn new_is_empty() {
        let set = ToolSet::new();
        assert!(set.definitions().is_empty());
    }

    #[test]
    fn add_provider_via_operator() {
        let set = ToolSet::new() + MockAlpha;
        let defs = set.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "alpha");
    }

    #[test]
    fn add_arc_provider() {
        let arc: Arc<dyn ToolProvider> = Arc::new(MockBeta);
        let set = ToolSet::new() + arc;
        let defs = set.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "beta");
    }

    #[test]
    fn sub_str_removes_tool() {
        let set = ToolSet::new() + MockAlpha + MockBeta - "alpha";
        let names: Vec<String> = set.definitions().iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, vec!["beta"]);
    }

    #[test]
    fn sub_str_missing_is_noop() {
        let set = ToolSet::new() + MockAlpha - "nonexistent";
        assert_eq!(set.definitions().len(), 1);
    }

    #[test]
    fn sub_capability_removes_group() {
        // Shell capability tools: shell, shell_wait, shell_read, shell_write, shell_kill
        // MockAlpha's "alpha" tool is NOT in the Shell capability, so it survives.
        let set = ToolSet::new() + MockAlpha - CapabilityId::Shell;
        let names: Vec<String> = set.definitions().iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn operator_chaining() {
        let set = ToolSet::new() + MockAlpha + MockBeta - "alpha" + MockMulti - "multi_b";
        let mut names: Vec<String> = set.definitions().iter().map(|d| d.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["beta", "multi_a"]);
    }

    #[test]
    fn duplicate_name_last_wins() {
        // MockAlpha defines "alpha". Adding another provider that also defines "alpha"
        // should replace the first.
        struct MockAlpha2;

        #[async_trait::async_trait]
        impl ToolProvider for MockAlpha2 {
            fn definitions(&self) -> Vec<ToolDefinition> {
                vec![ToolDefinition {
                    name: "alpha".into(),
                    description: "Alpha v2".into(),
                    params: vec![],
                    returns: "str".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                }]
            }
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolResult {
                ToolResult::ok(serde_json::json!("alpha_v2"))
            }
        }

        let set = ToolSet::new() + MockAlpha + MockAlpha2;
        let defs = set.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "Alpha v2");
    }

    #[test]
    fn multi_tool_provider_shares_arc() {
        let set = ToolSet::new() + MockMulti;
        let defs = set.definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"multi_a"));
        assert!(names.contains(&"multi_b"));
    }

    #[test]
    fn toolset_plus_toolset() {
        let a = ToolSet::new() + MockAlpha;
        let b = ToolSet::new() + MockBeta;
        let combined = a + b;
        let mut names: Vec<String> = combined
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn execute_routes_correctly() {
        let set = ToolSet::new() + MockAlpha + MockBeta;
        let r = set.execute("alpha", &serde_json::json!({})).await;
        assert!(r.success);
        assert_eq!(r.result, serde_json::json!("alpha_result"));

        let r = set.execute("beta", &serde_json::json!({"x": 1})).await;
        assert!(r.success);
        assert_eq!(r.result, serde_json::json!(42));
    }

    #[tokio::test]
    async fn execute_unknown_tool_errors() {
        let set = ToolSet::new() + MockAlpha;
        let r = set.execute("nonexistent", &serde_json::json!({})).await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn execute_multi_tool_provider() {
        let set = ToolSet::new() + MockMulti;
        let r = set.execute("multi_a", &serde_json::json!({})).await;
        assert!(r.success);
        assert_eq!(r.result, serde_json::json!("multi_a_result"));

        let r = set.execute("multi_b", &serde_json::json!({})).await;
        assert!(r.success);
        assert_eq!(r.result, serde_json::json!("multi_b_result"));
    }

    #[tokio::test]
    async fn removed_tool_is_not_executable() {
        let set = ToolSet::new() + MockAlpha + MockBeta - "alpha";
        let r = set.execute("alpha", &serde_json::json!({})).await;
        assert!(!r.success);
    }
}
