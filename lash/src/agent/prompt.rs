use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use crate::ExecutionMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSectionName {
    Identity,
    Environment,
    Personality,
    ExecutionContract,
    TerminationContract,
    ToolAccess,
    ToolGuides,
    AvailableTools,
    ErrorRecovery,
    Builtins,
    Memory,
    MemoryApi,
    ProjectInstructions,
    Guidelines,
}

impl PromptSectionName {
    pub const ALL: [Self; 14] = [
        Self::Identity,
        Self::Personality,
        Self::ExecutionContract,
        Self::TerminationContract,
        Self::ErrorRecovery,
        Self::Guidelines,
        Self::ProjectInstructions,
        Self::Environment,
        Self::Memory,
        Self::MemoryApi,
        Self::ToolAccess,
        Self::Builtins,
        Self::ToolGuides,
        Self::AvailableTools,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Environment => "environment",
            Self::Personality => "personality",
            Self::ExecutionContract => "execution_contract",
            Self::TerminationContract => "termination_contract",
            Self::ToolAccess => "tool_access",
            Self::ToolGuides => "tool_guides",
            Self::AvailableTools => "available_tools",
            Self::ErrorRecovery => "error_recovery",
            Self::Builtins => "builtins",
            Self::Memory => "memory",
            Self::MemoryApi => "memory_api",
            Self::ProjectInstructions => "project_instructions",
            Self::Guidelines => "guidelines",
        }
    }

    pub fn names_csv() -> String {
        Self::ALL
            .iter()
            .map(Self::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl FromStr for PromptSectionName {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = raw.trim().to_ascii_lowercase();
        match value.as_str() {
            "identity" => Ok(Self::Identity),
            "environment" => Ok(Self::Environment),
            "personality" => Ok(Self::Personality),
            "execution_contract" => Ok(Self::ExecutionContract),
            "termination_contract" => Ok(Self::TerminationContract),
            "tool_access" => Ok(Self::ToolAccess),
            "tool_guides" => Ok(Self::ToolGuides),
            "available_tools" => Ok(Self::AvailableTools),
            "error_recovery" => Ok(Self::ErrorRecovery),
            "builtins" => Ok(Self::Builtins),
            "memory" => Ok(Self::Memory),
            "memory_api" => Ok(Self::MemoryApi),
            "project_instructions" => Ok(Self::ProjectInstructions),
            "guidelines" => Ok(Self::Guidelines),
            _ => Err(format!(
                "unknown prompt section `{raw}` (expected one of: {})",
                Self::names_csv()
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOverrideMode {
    Replace,
    Prepend,
    Append,
    Disable,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PromptSectionOverride {
    pub section: PromptSectionName,
    pub mode: PromptOverrideMode,
    #[serde(default)]
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptProfile {
    RootInteractive,
    RootHeadless,
    SubAgentInteractive,
    SubAgentHeadless,
}

impl PromptProfile {
    pub fn from_flags(headless: bool, sub_agent: bool) -> Self {
        match (headless, sub_agent) {
            (false, false) => Self::RootInteractive,
            (true, false) => Self::RootHeadless,
            (false, true) => Self::SubAgentInteractive,
            (true, true) => Self::SubAgentHeadless,
        }
    }

    pub fn is_headless(self) -> bool {
        matches!(self, Self::RootHeadless | Self::SubAgentHeadless)
    }

    pub fn is_subagent(self) -> bool {
        matches!(self, Self::SubAgentInteractive | Self::SubAgentHeadless)
    }
}

pub struct PromptComposeInput<'a> {
    pub profile: PromptProfile,
    pub execution_mode: ExecutionMode,
    pub context: &'a str,
    pub tool_list: &'a str,
    pub tool_names: &'a [String],
    pub has_history: bool,
    pub enabled_capability_ids: &'a BTreeSet<String>,
    pub helper_bindings: &'a BTreeSet<String>,
    pub capability_prompt_sections: &'a [String],
    pub can_write: bool,
    pub include_soul: bool,
    pub project_instructions: &'a str,
    pub overrides: &'a [PromptSectionOverride],
}

fn has_capability(input: &PromptComposeInput<'_>, id: &str) -> bool {
    input.enabled_capability_ids.contains(id)
}

fn has_helper(input: &PromptComposeInput<'_>, helper: &str) -> bool {
    input.helper_bindings.contains(helper)
}

pub fn compose_system_prompt(input: PromptComposeInput<'_>) -> String {
    let mut sections: HashMap<PromptSectionName, Option<String>> = HashMap::new();
    for section in PromptSectionName::ALL {
        sections.insert(section, default_section(section, &input));
    }
    apply_overrides(&mut sections, input.overrides);

    PromptSectionName::ALL
        .iter()
        .filter_map(|section| sections.get(section).cloned().flatten())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn apply_overrides(
    sections: &mut HashMap<PromptSectionName, Option<String>>,
    overrides: &[PromptSectionOverride],
) {
    for ov in overrides {
        let entry = sections.entry(ov.section).or_insert(None);
        match ov.mode {
            PromptOverrideMode::Disable => {
                *entry = None;
            }
            PromptOverrideMode::Replace => {
                *entry = Some(ov.content.clone());
            }
            PromptOverrideMode::Prepend => {
                let existing = entry.take().unwrap_or_default();
                *entry = Some(if existing.trim().is_empty() {
                    ov.content.clone()
                } else {
                    format!("{}\n\n{}", ov.content, existing)
                });
            }
            PromptOverrideMode::Append => {
                let existing = entry.take().unwrap_or_default();
                *entry = Some(if existing.trim().is_empty() {
                    ov.content.clone()
                } else {
                    format!("{}\n\n{}", existing, ov.content)
                });
            }
        }
    }
}

fn default_section(section: PromptSectionName, input: &PromptComposeInput<'_>) -> Option<String> {
    let profile = input.profile;
    match section {
        PromptSectionName::Identity => Some(match profile {
            PromptProfile::RootInteractive => match input.execution_mode {
                ExecutionMode::Repl => "You are an AI coding assistant operating in a persistent Python REPL with tool access.\nYou power lash, a terminal-based coding agent. Understand the codebase, make changes, run commands, and report outcomes clearly.".to_string(),
                ExecutionMode::NativeTools => "You are an AI coding assistant with native tool-calling access.\nYou power lash, a terminal-based coding agent. Understand the codebase, make changes, run commands, and report outcomes clearly.".to_string(),
            },
            PromptProfile::RootHeadless => "You are an autonomous AI coding agent running in non-interactive mode.\nComplete the task end-to-end without asking for user input.".to_string(),
            PromptProfile::SubAgentInteractive => {
                if input.can_write {
                    "You are a sub-agent inside lash working on a delegated task.\nUse tools decisively and return results to the caller via done() when complete.".to_string()
                } else {
                    "You are a read-only sub-agent inside lash working on a delegated task.\nFocus on lookup/summarization tasks and return results to the caller via done() when complete.".to_string()
                }
            }
            PromptProfile::SubAgentHeadless => {
                if input.can_write {
                    "You are a headless sub-agent inside lash working on a delegated task.\nOperate autonomously and return final results via done() only when complete.".to_string()
                } else {
                    "You are a headless read-only sub-agent inside lash working on a delegated task.\nOperate autonomously on lookup/summarization tasks and return final results via done() only when complete.".to_string()
                }
            }
        }),
        PromptSectionName::Environment => Some(format!("## Environment\n\n{}", input.context)),
        PromptSectionName::Personality => {
            if !input.include_soul {
                None
            } else {
                Some(
                    "## Core Principles\n\n- First-principles thinker\n- Allergic to accidental complexity\n- Direct over diplomatic\n- Skeptical of abstraction\n- Show, don't lecture\n- High standards by default"
                        .to_string(),
                )
            }
        }
        PromptSectionName::ExecutionContract => Some(format!(
            "{}\n{}",
            if matches!(input.execution_mode, ExecutionMode::Repl) {
                "## Execution Contract\n\nYour output can include prose and `<repl>` blocks.\n- `<repl>` blocks execute immediately when `</repl>` is reached\n- Maximum one `<repl>` block per turn\n- After a `<repl>` block executes, lash continues in a new internal turn with the execution output\n- Do not emit additional `<repl>` blocks in the same response after one has closed\n- Use `<repl>` only when execution is needed; prose-only responses are valid when no execution is required\n- For direct conversational requests (greetings, small talk, time/date, simple Q&A), respond in prose and do not open `<repl>`\n- Variables persist across turns\n- `print(...)` output is model-visible only"
            } else {
                "## Execution Contract\n\nUse native tool calls when execution is needed.\n- Do not emit `<repl>` blocks or Python code\n- Call tools directly with valid arguments\n- When several tool calls are independent, emit them together in the same response instead of serializing them across turns\n- Use serial tool calls only when later arguments depend on earlier tool results or when ordering is required for correctness\n- Avoid unnecessary prose between independent tool calls; optimize for completing the task with as few tool rounds as possible\n- After tool results are returned, continue with the next tool calls or final answer\n- For direct conversational requests that need no tools, respond in prose only"
            },
            if matches!(input.execution_mode, ExecutionMode::Repl) {
                if profile.is_headless() {
                    "- In headless mode, prose-only turns are invalid; execute via `<repl>`"
                } else {
                    "- In interactive mode, call `done(...)` only when your final user-facing answer is ready"
                }
            } else if profile.is_headless() {
                "- In headless mode, keep calling tools until the task is complete, then return a final answer without extra commentary."
            } else {
                "- In interactive mode, return a final user-facing answer once the task is complete."
            }
        )),
        PromptSectionName::TerminationContract => Some(format!(
            "{}\n{}",
            if matches!(input.execution_mode, ExecutionMode::Repl) {
                "## Termination Contract\n\n`done(value)` ends the turn and returns control.\n- `done(...)` may only be called inside `<repl>`\n- Do not use `done(...)` for status updates"
            } else {
                "## Termination Contract\n\nFinish by returning a final assistant answer.\n- Do not emit fake tool calls or placeholder arguments\n- Do not stop after tool execution unless the task is actually complete"
            },
            if matches!(input.execution_mode, ExecutionMode::Repl) {
                if profile.is_headless() {
                    "- Headless: call `done(...)` only after the task is fully completed"
                } else {
                    "- Interactive: call `done(...)` only when your final user-facing answer is ready"
                }
            } else if profile.is_headless() {
                "- Headless: do not stop on prose-only intermediate steps."
            } else {
                "- Interactive: provide a concise final answer when complete."
            }
        )),
        PromptSectionName::ToolAccess => Some(
            if matches!(input.execution_mode, ExecutionMode::Repl) {
                "## Tool Access\n\n- All visible tools are available in the `tools` module (e.g. `tools.read_file(...)`)\n- `from tools import *` is applied automatically, so visible tools are also callable as globals\n- If a tool name is overwritten, restore it with `import tools` or `from tools import <tool>` / `from tools import *`\n- Use `await` only for long-running calls (`delegate_*()` and shell handle methods like `proc.wait()`)\n- Use `asyncio.gather(...)` only for `delegate_*()` and shell handle methods"
                    .to_string()
            } else {
                format!(
                    "## Tool Access\n\n- The runtime will expose the visible tools as native tool calls\n- Use only the tools shown in Available Tools\n- Prefer parallel independent tool calls in one assistant turn when the model/provider supports multiple calls\n- Good candidates for one-turn parallel calls: reading several files, multiple searches, independent inspections, and unrelated diagnostics\n- Do not parallelize dependent steps, ordered stateful edits, or operations whose arguments depend on prior output\n- Never invent tool names or arguments that are not described{}",
                    if input.tool_names.iter().any(|name| name == "batch") {
                        "\n- If `batch` is available and you need 2 or more independent tool calls, prefer `batch` over spacing those calls across multiple turns"
                    } else {
                        ""
                    }
                )
            },
        ),
        PromptSectionName::ToolGuides => {
            let guide = tool_guides(
                input.tool_names,
                has_helper(input, "search_history") || has_capability(input, "history"),
                has_helper(input, "search_mem") || has_capability(input, "memory"),
                has_helper(input, "search_skills") || has_capability(input, "skills"),
                input.capability_prompt_sections,
            );
            if guide.is_empty() {
                None
            } else {
                Some(format!("## Tool Guide\n\n{}", guide))
            }
        }
        PromptSectionName::AvailableTools => Some(format!(
            "## Available Tools\n\n{}\n\nUse `help(tool_name)` or `help(tools.tool_name)` for signatures and docs.",
            input.tool_list
        )),
        PromptSectionName::ErrorRecovery => Some(
            "## Error Recovery\n\nTool failures raise `ToolError`; execution stops at the failing line.\n- Read the traceback and fix the cause before retrying\n- Do not repeat failing calls unchanged\n- If REPL state is corrupted, call `reset_repl()`"
                .to_string(),
        ),
        PromptSectionName::Builtins => Some(builtins_section(
            profile,
            input.tool_names,
            has_helper(input, "search_history") || has_capability(input, "history"),
            has_helper(input, "search_mem") || has_capability(input, "memory"),
            has_helper(input, "search_skills") || has_capability(input, "skills"),
        )),
        PromptSectionName::Memory => {
            let history_enabled = has_helper(input, "search_history") || has_capability(input, "history");
            let memory_enabled = has_helper(input, "search_mem") || has_capability(input, "memory");
            if history_enabled || memory_enabled {
                Some(memory_section(
                    input.has_history,
                    profile.is_subagent(),
                    history_enabled,
                    memory_enabled,
                ))
            } else {
                None
            }
        }
        PromptSectionName::MemoryApi => {
            if has_helper(input, "search_mem") || has_capability(input, "memory") {
                Some(memory_api_section())
            } else {
                None
            }
        }
        PromptSectionName::ProjectInstructions => {
            if input.project_instructions.trim().is_empty() {
                None
            } else {
                Some(format!(
                    "## Project Instructions\n\n{}",
                    input.project_instructions
                ))
            }
        }
        PromptSectionName::Guidelines => Some(format!(
            "## Guidelines\n\n- Bias toward concrete execution over planning chatter\n- For substantial scripts/workflows, create files and run them with host tooling\n- Use isolated environments only when required dependencies are missing\n- Avoid redundant file reads when values already exist in variables\n- Never speculate about files you have not read\n- Be concise and action-oriented\n{}\n{}\n{}",
            if profile == PromptProfile::RootInteractive {
                "- In interactive mode, when a concrete deliverable is requested, prefer completing it in the current response over reconnaissance-only steps"
            } else {
                ""
            },
            if !input.can_write {
                "- This agent is read-only: do not modify files; focus on inspection, lookup, and summarization"
            } else {
                ""
            },
            if !(has_helper(input, "search_history") || has_capability(input, "history")) {
                ""
            } else {
                "- Use `_history` and `_mem` only when prior-turn recall is actually needed"
            }
        )),
    }
}

fn builtins_section(
    profile: PromptProfile,
    tool_names: &[String],
    history_enabled: bool,
    memory_enabled: bool,
    skills_enabled: bool,
) -> String {
    let has_skills_tool = tool_names
        .iter()
        .any(|name| name == "skills" || name == "load_skill");
    let mut lines = vec![
        "## Built-ins".to_string(),
        "".to_string(),
        "- `done(value)`".to_string(),
    ];
    if profile.is_headless() {
        lines.push("- `ask(...)` is unavailable in headless mode".to_string());
    } else {
        lines.push("- `ask(question, options=None)`".to_string());
    }
    lines.push("- `list_tools(query=None)`".to_string());
    lines.push("- `search_tools(query, mode=\"hybrid\", regex=None, limit=10)`".to_string());
    if skills_enabled && has_skills_tool {
        lines.push("- `search_skills(query, mode=\"hybrid\", regex=None, limit=10)`".to_string());
    }
    if history_enabled {
        lines.push(
            "- `search_history(query, mode=\"hybrid\", regex=None, limit=10, fields=None, since_turn=None)`"
                .to_string(),
        );
    }
    if memory_enabled {
        lines.push(
            "- `search_mem(query, mode=\"hybrid\", regex=None, limit=10, keys=None)`".to_string(),
        );
    }
    lines.push("- `help(tool_name)`".to_string());
    lines.push("- `reset_repl()`".to_string());
    lines.join("\n")
}

fn memory_section(
    has_history: bool,
    is_subagent: bool,
    history_enabled: bool,
    memory_enabled: bool,
) -> String {
    let title = if history_enabled && memory_enabled {
        "## Memory & History"
    } else if memory_enabled {
        "## Memory"
    } else {
        "## History"
    };
    let mut lines = vec![title.to_string(), "".to_string()];
    if memory_enabled {
        lines.push(
            "- `_mem` is persistent key-value memory across context pruning; use it to store durable decisions"
                .to_string(),
        );
        lines.push("- Retrieve memory with `search_mem(...)` / `_mem.search(...)`".to_string());
    }
    if history_enabled && has_history {
        lines.push(
            "- `_history` stores pruned prior turns; use `_history.user_messages()` and `_history.search(...)`"
                .to_string(),
        );
    }
    if is_subagent {
        if history_enabled && memory_enabled {
            lines.push(
                "- `_history` and `_mem` can include parent-agent context inherited at spawn time"
                    .to_string(),
            );
        } else if history_enabled {
            lines.push(
                "- `_history` can include parent-agent context inherited at spawn time".to_string(),
            );
        } else if memory_enabled {
            lines.push(
                "- `_mem` can include parent-agent context inherited at spawn time".to_string(),
            );
        }
    }
    lines.join("\n")
}

fn memory_api_section() -> String {
    [
        "## Memory API",
        "",
        "- `_mem.set(key, description, value=None)` stores/updates memory",
        "- `_mem.get(key)` returns a stored value or `None`",
        "- `_mem.delete(key)` removes a key",
        "- `_mem.all()` lists keys with descriptions",
        "- `_mem.search(query, mode=\"hybrid\", regex=None, limit=10, keys=None)` searches memory",
        "- `search_mem(query, mode=\"hybrid\", regex=None, limit=10, keys=None)` is the built-in helper",
        "- Example: `_mem.set(\"repo_convention\", \"Use snake_case for tool names\")`",
    ]
    .join("\n")
}

fn tool_guides(
    tool_names: &[String],
    history_enabled: bool,
    memory_enabled: bool,
    skills_enabled: bool,
    dynamic_prompt_sections: &[String],
) -> String {
    let tools: HashSet<&str> = tool_names.iter().map(String::as_str).collect();
    let mut chunks = Vec::new();

    if tools.contains("ls") || tools.contains("read_file") || tools.contains("glob") {
        chunks.push(
            "**Orient -> Read -> Act**\n1. `ls()` / `glob()` to inspect `PathEntry` objects (`path`, `kind`, `size_bytes`, `modified_at`, optional `lines`)\n2. `read_file`/`grep` for content-level context before editing\n3. `edit_file` for changes, `write_file` for new files"
                .to_string(),
        );
    }
    if tools.contains("read_file") {
        chunks.push(
            "**Image reads**\nIf `read_file(...)` on an image returns an `[Image: ...]` marker, that marker is metadata only. Use the attached image context to describe what is visibly present; do not just repeat the marker text."
                .to_string(),
        );
    }
    if tools.contains("edit_file") {
        chunks.push(
            "**Hashline edits**\n`read_file` returns `LINE:HASH|text` where HASH is an 8-character hex value (example: `42:a5c1d2e3|...`). Always read first, then edit using those anchors."
                .to_string(),
        );
    }
    if tools.contains("find_replace") {
        chunks.push("**find_replace** performs exact text substitution and is best for straightforward renames/typo fixes.".to_string());
    }
    if tools.contains("glob") {
        chunks.push(
            "**glob/ls output**\n`glob(...)` returns `PathEntry` items sorted by modification time (newest first). `ls(...)` returns `PathEntry` items for directory traversal. If truncated, inspect returned truncation metadata and rerun with `limit=None` when needed."
                .to_string(),
        );
    }
    if tools.contains("shell") {
        chunks.push(
            "**Shell handles**\n`proc = shell(cmd)` then use `await proc.wait()`, `await proc.read()`, `await proc.write(...)`, `await proc.kill()`."
                .to_string(),
        );
        chunks.push(
            "**Git safety**\nDo not revert user changes you did not make. Avoid destructive git commands unless explicitly requested."
                .to_string(),
        );
    }
    if tools.contains("delegate_task")
        || tools.contains("delegate_deep")
        || tools.contains("delegate_search")
    {
        chunks.push(
            "**Delegation tiers**\nUse low-cost delegates for read-only lookup/summarization tasks, and stronger delegates for edits/refactors. Avoid concurrent delegates editing the same file."
                .to_string(),
        );
    }
    if tools.contains("agent_call") {
        let heading = if history_enabled || memory_enabled {
            "History recall pattern"
        } else {
            "Context recall pattern"
        };
        let recall_line = if history_enabled && memory_enabled {
            "When prior context likely matters, use a low-intelligence (read-only) `agent_call` to summarize relevant `_history`/`_mem` quickly, then continue execution."
        } else if history_enabled {
            "When prior context likely matters, use a low-intelligence (read-only) `agent_call` to summarize relevant `_history` quickly, then continue execution."
        } else if memory_enabled {
            "When prior context likely matters, use a low-intelligence (read-only) `agent_call` to summarize relevant `_mem` quickly, then continue execution."
        } else {
            "When prior context likely matters, use a low-intelligence (read-only) `agent_call` to summarize relevant context quickly, then continue execution."
        };
        chunks.push(
            format!(
                "**{}**\n{}\nDo not delegate straightforward local file/image inspection that you can do directly with available tools.",
                heading,
                recall_line
            ),
        );
    }
    if tools.contains("create_task") {
        chunks.push(
            "**Task management**\nFor multi-step work: create tasks, keep one in progress, and mark completion immediately. Use `create_task(subject=...)` (not `title`). For `update_task`, valid statuses are `pending`, `in_progress`, `completed`, and `cancelled`."
                .to_string(),
        );
    }
    if tools.contains("batch") {
        chunks.push(
            "**Batching**\nUse `batch(tool_calls=[...])` for 2-25 independent tool calls when you already know the arguments up front. Good fits: reading several files, multiple searches, or unrelated diagnostics. Do not batch dependent steps and do not nest `batch` inside `batch`."
                .to_string(),
        );
    }
    if tools.contains("agent_call") || skills_enabled {
        for section in dynamic_prompt_sections {
            if !section.trim().is_empty() {
                chunks.push(section.clone());
            }
        }
    }
    chunks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{
        AgentCapabilities, CapabilityId, capability_def, helper_bindings_for_capability,
    };

    fn ids_for(caps: &AgentCapabilities) -> BTreeSet<String> {
        caps.enabled_capabilities
            .iter()
            .map(|id| id.as_str().to_string())
            .collect()
    }

    fn helpers_for(caps: &AgentCapabilities) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for id in &caps.enabled_capabilities {
            for helper in helper_bindings_for_capability(*id) {
                out.insert((*helper).to_string());
            }
        }
        out
    }

    fn prompt_sections_for(caps: &AgentCapabilities) -> Vec<String> {
        caps.enabled_capabilities
            .iter()
            .filter_map(|id| capability_def(*id).and_then(|d| d.prompt_section.map(str::to_string)))
            .collect()
    }

    fn can_write(caps: &AgentCapabilities) -> bool {
        caps.enabled(CapabilityId::CoreWrite)
    }

    #[test]
    fn parses_prompt_section_names() {
        assert_eq!(
            PromptSectionName::from_str("tool_guides").unwrap(),
            PromptSectionName::ToolGuides
        );
        assert_eq!(
            PromptSectionName::from_str("memory_api").unwrap(),
            PromptSectionName::MemoryApi
        );
        assert!(PromptSectionName::from_str("nope").is_err());
    }

    #[test]
    fn overrides_apply_in_order() {
        let overrides = vec![
            PromptSectionOverride {
                section: PromptSectionName::Identity,
                mode: PromptOverrideMode::Replace,
                content: "A".into(),
            },
            PromptSectionOverride {
                section: PromptSectionName::Identity,
                mode: PromptOverrideMode::Append,
                content: "B".into(),
            },
        ];
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootHeadless,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: false,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &overrides,
        });
        assert!(text.starts_with("A\n\nB"));
    }

    #[test]
    fn memory_api_section_is_included_by_default() {
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: false,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(text.contains("## Memory API"));
        assert!(text.contains("_mem.set(key, description, value=None)"));
    }

    #[test]
    fn memory_api_section_can_be_disabled() {
        let overrides = vec![PromptSectionOverride {
            section: PromptSectionName::MemoryApi,
            mode: PromptOverrideMode::Disable,
            content: String::new(),
        }];
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: false,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &overrides,
        });
        assert!(!text.contains("## Memory API"));
    }

    #[test]
    fn skills_not_mentioned_when_skills_capability_disabled() {
        let caps = AgentCapabilities::default().disable(CapabilityId::Skills);
        let tools = vec!["load_skill".to_string(), "skills".to_string()];
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &tools,
            has_history: false,
            enabled_capability_ids: &ids_for(&caps),
            helper_bindings: &helpers_for(&caps),
            capability_prompt_sections: &prompt_sections_for(&caps),
            can_write: can_write(&caps),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(!text.contains("search_skills("));
        assert!(!text.contains("## Skills"));
    }

    #[test]
    fn subagent_prompt_mentions_read_only_when_core_write_disabled() {
        let caps = AgentCapabilities::default().disable(CapabilityId::CoreWrite);
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::SubAgentInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: false,
            enabled_capability_ids: &ids_for(&caps),
            helper_bindings: &helpers_for(&caps),
            capability_prompt_sections: &prompt_sections_for(&caps),
            can_write: can_write(&caps),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(text.contains("read-only sub-agent"));
        assert!(text.contains("This agent is read-only"));
    }

    #[test]
    fn dynamic_capability_prompt_sections_are_included() {
        let caps = AgentCapabilities::default();
        let mut cap_ids = ids_for(&caps);
        cap_ids.insert("custom_cap".to_string());
        let sections = vec!["## Custom Capability\n\nCustom guidance.".to_string()];
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &["agent_call".to_string()],
            has_history: false,
            enabled_capability_ids: &cap_ids,
            helper_bindings: &helpers_for(&caps),
            capability_prompt_sections: &sections,
            can_write: can_write(&caps),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(text.contains("## Custom Capability"));
        assert!(text.contains("Custom guidance."));
    }

    #[test]
    fn native_tools_prompt_emphasizes_parallel_independent_calls() {
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::NativeTools,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: false,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(text.contains("emit them together in the same response"));
        assert!(text.contains("Good candidates for one-turn parallel calls"));
        assert!(text.contains("Do not parallelize dependent steps"));
    }

    #[test]
    fn native_tools_prompt_prefers_batch_when_available() {
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::NativeTools,
            context: "ctx",
            tool_list: "tools",
            tool_names: &["batch".to_string()],
            has_history: false,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(text.contains("prefer `batch`"));
        assert!(text.contains("Use `batch(tool_calls=[...])`"));
    }

    #[test]
    fn prompt_orders_dynamic_sections_late() {
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &["read_file".to_string()],
            has_history: true,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: true,
            project_instructions: "project rules",
            overrides: &[],
        });
        let guidelines_idx = text.find("## Guidelines").unwrap();
        let project_idx = text.find("## Project Instructions").unwrap();
        let env_idx = text.find("## Environment").unwrap();
        let tools_idx = text.find("## Available Tools").unwrap();
        assert!(guidelines_idx < project_idx);
        assert!(project_idx < env_idx);
        assert!(env_idx < tools_idx);
    }

    #[test]
    fn prompt_does_not_emit_runtime_prune_status() {
        let text = compose_system_prompt(PromptComposeInput {
            profile: PromptProfile::RootInteractive,
            execution_mode: crate::ExecutionMode::Repl,
            context: "ctx",
            tool_list: "tools",
            tool_names: &[],
            has_history: true,
            enabled_capability_ids: &ids_for(&AgentCapabilities::default()),
            helper_bindings: &helpers_for(&AgentCapabilities::default()),
            capability_prompt_sections: &prompt_sections_for(&AgentCapabilities::default()),
            can_write: can_write(&AgentCapabilities::default()),
            include_soul: false,
            project_instructions: "",
            overrides: &[],
        });
        assert!(!text.contains("Context-pruned turns this run"));
        assert!(!text.contains("Skip history-mining detours"));
    }
}
