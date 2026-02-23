use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use crate::ToolDefinition;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityId {
    Core,
    Shell,
    Tasks,
    Planning,
    Delegation,
    Memory,
    History,
    Skills,
    Web,
    Context,
}

impl CapabilityId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Shell => "shell",
            Self::Tasks => "tasks",
            Self::Planning => "planning",
            Self::Delegation => "delegation",
            Self::Memory => "memory",
            Self::History => "history",
            Self::Skills => "skills",
            Self::Web => "web",
            Self::Context => "context",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub enabled_capabilities: BTreeSet<CapabilityId>,
    #[serde(default)]
    pub enabled_tools: BTreeSet<String>,
}

impl AgentCapabilities {
    pub fn enabled(&self, capability: CapabilityId) -> bool {
        self.enabled_capabilities.contains(&capability)
    }

    pub fn enable(mut self, capability: CapabilityId) -> Self {
        self.enabled_capabilities.insert(capability);
        self
    }

    pub fn disable(mut self, capability: CapabilityId) -> Self {
        self.enabled_capabilities.remove(&capability);
        self
    }

    pub fn with_tool(mut self, name: impl Into<String>) -> Self {
        self.enabled_tools.insert(name.into());
        self
    }
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            enabled_capabilities: default_enabled_capabilities(),
            enabled_tools: BTreeSet::new(),
        }
    }
}

pub struct CapabilityDefinition {
    pub id: CapabilityId,
    pub name: &'static str,
    pub description: &'static str,
    pub prompt_section: Option<&'static str>,
    pub helper_bindings: &'static [&'static str],
    pub tools: &'static [&'static str],
    pub enabled_by_default: bool,
}

pub const CAPABILITY_DEFINITIONS: &[CapabilityDefinition] = &[
    CapabilityDefinition {
        id: CapabilityId::Core,
        name: "Core",
        description: "Core file navigation/editing and tool discovery.",
        prompt_section: None,
        helper_bindings: &[],
        tools: &[
            "read_file",
            "write_file",
            "edit_file",
            "find_replace",
            "glob",
            "grep",
            "ls",
            "search_tools",
        ],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Shell,
        name: "Shell",
        description: "Shell execution and process handles.",
        prompt_section: None,
        helper_bindings: &[],
        tools: &[
            "shell",
            "shell_result",
            "shell_output",
            "shell_write",
            "shell_kill",
        ],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Tasks,
        name: "Tasks",
        description: "Task planning and dependency graph tooling.",
        prompt_section: None,
        helper_bindings: &[],
        tools: &[
            "tasks",
            "tasks_summary",
            "get_task",
            "create_task",
            "update_task",
            "delete_task",
            "claim_task",
            "add_blocks",
            "remove_blocks",
            "add_blocked_by",
            "remove_blocked_by",
        ],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Planning,
        name: "Planning",
        description: "Interactive plan mode lifecycle tools.",
        prompt_section: None,
        helper_bindings: &["enter_plan_mode", "exit_plan_mode"],
        tools: &["enter_plan_mode", "exit_plan_mode"],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Delegation,
        name: "Delegation",
        description: "Sub-agent orchestration via agent_call.",
        prompt_section: Some(
            "## Delegation\n\nUse `agent_call` for scoped sub-tasks. Prefer low-intelligence delegates for lookup work, and avoid overlapping file edits across concurrent delegates.",
        ),
        helper_bindings: &[],
        tools: &["agent_call", "agent_result", "agent_output", "agent_kill"],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::History,
        name: "History",
        description: "Persistent turn history and retrieval.",
        prompt_section: Some(
            "## History\n\n`_history` tracks prior turns across pruning. Use `_history.search(...)` or `search_history(...)` only when previous context is needed.",
        ),
        helper_bindings: &["search_history", "TurnHistory", "HistoryMatch", "_history"],
        tools: &[
            "search_history",
            "history_add_turn",
            "history_export",
            "history_load",
        ],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Memory,
        name: "Memory",
        description: "Persistent key-value memory and retrieval.",
        prompt_section: Some(
            "## Memory\n\n`_mem` is persistent key-value memory. Store durable decisions and retrieve with `_mem.search(...)` / `search_mem(...)`.",
        ),
        helper_bindings: &["search_mem", "Mem", "MemEntry", "MemMatch", "_mem"],
        tools: &[
            "search_mem",
            "mem_set_turn",
            "mem_set",
            "mem_get",
            "mem_delete",
            "mem_export",
            "mem_load",
        ],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Skills,
        name: "Skills",
        description: "Skill discovery and loading.",
        prompt_section: Some(
            "## Skills\n\nWhen the user requests a skill-based workflow, use `tools.load_skill(name)` and follow the skill instructions.",
        ),
        helper_bindings: &["search_skills"],
        tools: &["skills", "load_skill", "read_skill_file", "search_skills"],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Web,
        name: "Web",
        description: "Web search and page fetch tools.",
        prompt_section: None,
        helper_bindings: &[],
        tools: &["search_web", "fetch_url"],
        enabled_by_default: true,
    },
    CapabilityDefinition {
        id: CapabilityId::Context,
        name: "Context",
        description: "Archived context lookups.",
        prompt_section: None,
        helper_bindings: &[],
        tools: &["view_message"],
        enabled_by_default: true,
    },
];

pub fn default_enabled_capabilities() -> BTreeSet<CapabilityId> {
    CAPABILITY_DEFINITIONS
        .iter()
        .filter(|d| d.enabled_by_default)
        .map(|d| d.id)
        .collect()
}

pub fn capability_def(id: CapabilityId) -> Option<&'static CapabilityDefinition> {
    CAPABILITY_DEFINITIONS.iter().find(|d| d.id == id)
}

pub fn tools_for_capability(id: CapabilityId) -> &'static [&'static str] {
    capability_def(id).map(|d| d.tools).unwrap_or(&[])
}

pub fn helper_bindings_for_capability(id: CapabilityId) -> &'static [&'static str] {
    capability_def(id).map(|d| d.helper_bindings).unwrap_or(&[])
}

pub fn capability_for_tool(name: &str) -> Option<CapabilityId> {
    static TOOL_TO_CAP: LazyLock<HashMap<&'static str, CapabilityId>> = LazyLock::new(|| {
        let mut out = HashMap::new();
        for def in CAPABILITY_DEFINITIONS {
            for tool in def.tools {
                out.insert(*tool, def.id);
            }
        }
        out
    });
    TOOL_TO_CAP.get(name).copied()
}

pub struct ResolvedFeatures {
    pub enabled_capabilities: BTreeSet<CapabilityId>,
    pub effective_tools: BTreeSet<String>,
    pub helper_bindings: BTreeSet<String>,
}

pub fn resolve_features(
    caps: &AgentCapabilities,
    available_defs: &[ToolDefinition],
) -> ResolvedFeatures {
    let available: HashSet<&str> = available_defs.iter().map(|d| d.name.as_str()).collect();
    let mut effective_tools = BTreeSet::new();
    let mut helper_bindings = BTreeSet::new();
    for id in &caps.enabled_capabilities {
        for tool in tools_for_capability(*id) {
            if available.contains(*tool) {
                effective_tools.insert((*tool).to_string());
            }
        }
        for binding in helper_bindings_for_capability(*id) {
            helper_bindings.insert((*binding).to_string());
        }
    }
    for tool in &caps.enabled_tools {
        if available.contains(tool.as_str()) {
            effective_tools.insert(tool.clone());
        }
    }
    ResolvedFeatures {
        enabled_capabilities: caps.enabled_capabilities.clone(),
        effective_tools,
        helper_bindings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(names: &[&str]) -> Vec<ToolDefinition> {
        names
            .iter()
            .map(|n| ToolDefinition {
                name: (*n).to_string(),
                description: String::new(),
                params: vec![],
                returns: "any".to_string(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: false,
            })
            .collect()
    }

    #[test]
    fn explicit_tool_is_included_without_capability_helpers() {
        let mut caps = AgentCapabilities::default().disable(CapabilityId::Skills);
        caps = caps.with_tool("load_skill");
        let resolved = resolve_features(&caps, &defs(&["load_skill", "search_skills"]));
        assert!(resolved.effective_tools.contains("load_skill"));
        assert!(!resolved.helper_bindings.contains("search_skills"));
    }

    #[test]
    fn enabled_capability_adds_bundled_tools_and_helpers() {
        let caps = AgentCapabilities::default().enable(CapabilityId::History);
        let resolved = resolve_features(
            &caps,
            &defs(&[
                "search_history",
                "history_add_turn",
                "history_export",
                "history_load",
            ]),
        );
        assert!(resolved.effective_tools.contains("search_history"));
        assert!(resolved.helper_bindings.contains("search_history"));
    }
}
