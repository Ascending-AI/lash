use std::path::PathBuf;

use serde_json::json;

use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::read_to_string;
use super::run_blocking;

#[derive(Clone)]
pub struct SkillStore {
    skill_dirs: Vec<PathBuf>, // [~/.lash/skills, .lash/skills] — later overrides earlier
}

struct SkillEntry {
    name: String,
    description: String,
    instructions: String,
    files: Vec<String>,
    base_path: PathBuf,
}

/// Parse YAML frontmatter from a markdown file.
/// Returns `(name, description, body)` where name may be empty (caller fills from dir name).
fn parse_frontmatter(text: &str) -> Option<(String, String, String)> {
    let text = text.trim_start();
    if !text.starts_with("---") {
        return None;
    }

    let after_open = &text[3..];
    let close_idx = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_idx];
    let body_start = 3 + close_idx + 4; // skip "---" + "\n---"
    let body = text[body_start..].trim().to_string();

    let mut name = String::new();
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().to_string();
        }
    }

    Some((name, description, body))
}

impl SkillStore {
    pub fn new(skill_dirs: Vec<PathBuf>) -> Self {
        Self { skill_dirs }
    }

    fn discover_skills(&self) -> Vec<SkillEntry> {
        let mut skills: Vec<SkillEntry> = Vec::new();

        for dir in &self.skill_dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                let text = match std::fs::read_to_string(&skill_md) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let (mut name, description, instructions) = match parse_frontmatter(&text) {
                    Some(t) => t,
                    None => continue,
                };

                // Default name to directory name if not set in frontmatter
                if name.is_empty() {
                    if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                        name = dir_name.to_string();
                    } else {
                        continue;
                    }
                }

                // Validate name: only lowercase alphanumeric and hyphens
                if !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    continue;
                }

                // List supporting files (non-SKILL.md, recursive)
                let files = list_skill_files(&path);

                // Later dirs override earlier by name
                skills.retain(|s| s.name != name);
                skills.push(SkillEntry {
                    name,
                    description,
                    instructions,
                    files,
                    base_path: path,
                });
            }
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    fn find_skill(&self, name: &str) -> Option<SkillEntry> {
        self.discover_skills().into_iter().find(|s| s.name == name)
    }

    fn execute_skills(&self) -> ToolResult {
        let skills = self.discover_skills();
        let items: Vec<serde_json::Value> = skills
            .iter()
            .map(|s| {
                json!({
                    "__type__": "skill_summary",
                    "name": s.name,
                    "description": s.description,
                    "file_count": s.files.len(),
                })
            })
            .collect();
        ToolResult::ok(json!({ "__type__": "skill_list", "items": items }))
    }

    fn execute_load_skill(&self, args: &serde_json::Value) -> ToolResult {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return ToolResult::err(json!("Missing required parameter: name")),
        };

        match self.find_skill(name) {
            Some(skill) => ToolResult::ok(json!({
                "__type__": "skill",
                "name": skill.name,
                "description": skill.description,
                "instructions": skill.instructions,
                "files": skill.files,
                "file_count": skill.files.len(),
            })),
            None => ToolResult::err_fmt(format_args!("Skill not found: {name}")),
        }
    }

    fn execute_read_skill_file(&self, args: &serde_json::Value) -> ToolResult {
        let skill_name = match args.get("skill_name").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return ToolResult::err(json!("Missing required parameter: skill_name")),
        };
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return ToolResult::err(json!("Missing required parameter: path")),
        };

        let skill = match self.find_skill(skill_name) {
            Some(s) => s,
            None => return ToolResult::err_fmt(format_args!("Skill not found: {skill_name}")),
        };

        // Resolve and validate path doesn't escape skill directory
        let target = skill.base_path.join(path);
        let canonical_base = match skill.base_path.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::err_fmt(format_args!("Skill directory error: {e}")),
        };
        let canonical_target = match target.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                return ToolResult::err_fmt(format_args!("File not found: {path}"));
            }
        };

        if !canonical_target.starts_with(&canonical_base) {
            return ToolResult::err(json!("Path escapes skill directory"));
        }

        match read_to_string(&canonical_target) {
            Ok(content) => ToolResult::ok(json!(content)),
            Err(e) => e,
        }
    }
}

/// Recursively list files in a skill directory, excluding SKILL.md.
/// Returns relative paths from the skill directory root.
fn list_skill_files(dir: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort();
    files
}

fn collect_files(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out);
        } else if path.is_file() {
            if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md")
                && path.parent() == Some(base)
            {
                continue; // skip the main SKILL.md at root level
            }
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for SkillStore {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "skills".into(),
                description: vec![crate::ToolText::new(
                    "List available skills. Returns a dict with `items` (list of `{name, description, file_count}` dicts).",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "load_skill".into(),
                description: vec![crate::ToolText::new(
                    "Load a skill by name. Returns a dict with `name`, `description`, `instructions`, `files`, and `file_count`.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("name", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "read_skill_file".into(),
                description: vec![crate::ToolText::new(
                    "Read a supporting file from a skill directory.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("skill_name", "str"),
                    ToolParam {
                        name: "path".into(),
                        r#type: "str".into(),
                        description: "Relative path within the skill directory".into(),
                        required: true,
                    },
                ],
                returns: "str".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: false,
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        let store = self.clone();
        let name = name.to_string();
        let args = args.clone();
        run_blocking(move || match name.as_str() {
            "skills" => store.execute_skills(),
            "load_skill" => store.execute_load_skill(&args),
            "read_skill_file" => store.execute_read_skill_file(&args),
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {}", name)),
        })
        .await
    }
}
