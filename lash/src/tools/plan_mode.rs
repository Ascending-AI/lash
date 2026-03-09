use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

use crate::{ToolDefinition, ToolProvider, ToolResult};

use super::read_to_string;

/// Shared state for plan mode across tool calls.
pub struct PlanState {
    pub plan_file: Option<PathBuf>,
    pub active: bool,
}

/// Plan mode tool provider: enter_plan_mode and exit_plan_mode.
///
/// The Rust side handles file path generation and plan file reading.
/// The runtime orchestrates the user-facing approval flow.
pub struct PlanMode {
    state: Mutex<PlanState>,
}

impl PlanMode {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PlanState {
                plan_file: None,
                active: false,
            }),
        }
    }
}

impl Default for PlanMode {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolProvider for PlanMode {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "enter_plan_mode".into(),
                description: vec![crate::ToolText::new(
                    "Enter plan mode. Returns the plan file path to write your plan to.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "exit_plan_mode".into(),
                description: vec![crate::ToolText::new(
                    "Exit plan mode. Returns plan content for user approval.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
        ]
    }

    async fn execute(&self, name: &str, _args: &serde_json::Value) -> ToolResult {
        match name {
            "enter_plan_mode" => {
                let mut state = self.state.lock().unwrap();

                // Generate plan file path if not already set
                if state.plan_file.is_none() {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let base = find_git_root(&cwd)
                        .map(|root| root.join(".lash"))
                        .unwrap_or_else(crate::lash_home);
                    let plans_dir = base.join("plans");
                    let _ = std::fs::create_dir_all(&plans_dir);

                    let now = chrono::Local::now();
                    let timestamp = now.format("%Y%m%d_%H%M%S");
                    let slug = generate_slug();
                    let filename = format!("{}-{}.md", timestamp, slug);
                    state.plan_file = Some(plans_dir.join(filename));
                }

                state.active = true;
                let path = state.plan_file.as_ref().unwrap().display().to_string();
                ToolResult::ok(json!({"plan_file": path}))
            }
            "exit_plan_mode" => {
                let state = self.state.lock().unwrap();
                let Some(ref path) = state.plan_file else {
                    return ToolResult::err_fmt("No plan file set — call enter_plan_mode first");
                };

                let plan_content = read_to_string(path).unwrap_or_default();

                let path_str = path.display().to_string();
                ToolResult::ok(json!({
                    "plan_content": plan_content,
                    "plan_file": path_str,
                }))
            }
            _ => ToolResult::err_fmt(format_args!("Unknown plan_mode tool: {name}")),
        }
    }
}

/// Walk up from a directory looking for a `.git/` directory.
fn find_git_root(start: &std::path::Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Generate a readable slug like "swift-falcon".
fn generate_slug() -> String {
    const ADJECTIVES: &[&str] = &[
        "swift", "bold", "calm", "dark", "keen", "warm", "cool", "fair", "deep", "wild", "soft",
        "pure", "vast", "slim", "rare", "firm", "lean", "rich", "true", "wise", "fast", "safe",
        "full", "neat", "open", "flat", "pale", "dry", "raw", "new",
    ];
    const NOUNS: &[&str] = &[
        "falcon", "ember", "coral", "prism", "ridge", "cedar", "bloom", "frost", "stone", "grain",
        "drift", "spark", "forge", "shade", "crest", "brook", "flint", "moss", "peak", "dust",
        "glow", "wave", "pine", "iron", "salt", "bone", "mist", "clay", "sage", "arch",
    ];

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let h = hasher.finish();

    let adj = ADJECTIVES[(h as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((h >> 16) as usize) % NOUNS.len()];
    format!("{}-{}", adj, noun)
}
