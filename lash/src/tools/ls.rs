use std::path::Path;

use super::{build_path_entry, parse_optional_bool, path_entries_value};
use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

/// List filesystem entries in a directory tree.
#[derive(Default)]
pub struct Ls;

const DEFAULT_DEPTH: usize = 3;
const MAX_ENTRIES: usize = 500;

#[async_trait::async_trait]
impl ToolProvider for Ls {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "ls".into(),
            description: vec![crate::ToolText::new(
                format!(
                    "List filesystem entries as `PathEntry` objects, respecting .gitignore. Sorted alphabetically by path. Each item has: `path`, `kind`, `size_bytes`, `lines` (or null), `modified_at` (RFC3339 UTC). Defaults: depth={}, limit={}, with_lines=false.",
                    DEFAULT_DEPTH, MAX_ENTRIES
                ),
                [crate::ExecutionMode::Repl, crate::ExecutionMode::NativeTools],
            )],
            params: vec![
                ToolParam {
                    name: "path".into(),
                    r#type: "str".into(),
                    description: "Directory to list (default: current directory)".into(),
                    required: false,
                },
                ToolParam {
                    name: "ignore".into(),
                    r#type: "list".into(),
                    description: "Additional patterns to ignore".into(),
                    required: false,
                },
                ToolParam {
                    name: "depth".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Maximum directory depth to traverse (default: {}). Use null or \"none\" for no depth cap.",
                        DEFAULT_DEPTH
                    ),
                    required: false,
                },
                ToolParam {
                    name: "limit".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Maximum entries to return (default: {}). Use null or \"none\" for no cap.",
                        MAX_ENTRIES
                    ),
                    required: false,
                },
                ToolParam {
                    name: "with_lines".into(),
                    r#type: "bool".into(),
                    description:
                        "If true, count text lines for file entries (`lines` field). Non-text files return lines=null. Default: false."
                            .into(),
                    required: false,
                },
            ],
            returns: "dict".into(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        }]
    }

    async fn execute(&self, _name: &str, args: &serde_json::Value) -> ToolResult {
        let base_dir = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let ignore_patterns: Vec<&str> = args
            .get("ignore")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let max_depth = match parse_depth(args) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let limit = match parse_limit(args) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let with_lines = match parse_optional_bool(args, "with_lines", false) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let base = Path::new(base_dir);
        if !base.is_dir() {
            return ToolResult::err_fmt(format_args!("Not a directory: {base_dir}"));
        }

        let mut builder = ignore::WalkBuilder::new(base);
        builder.hidden(true).git_ignore(true).max_depth(max_depth);

        // Add custom ignore patterns
        let mut overrides = ignore::overrides::OverrideBuilder::new(base);
        for pat in &ignore_patterns {
            let _ = overrides.add(&format!("!{}", pat));
        }
        if let Ok(ov) = overrides.build() {
            builder.overrides(ov);
        }

        let walker = builder.build();
        let mut entries: Vec<super::PathEntry> = Vec::new();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            let rel_path = path.strip_prefix(base).unwrap_or(path);
            let depth = rel_path.components().count();

            if depth == 0 {
                continue; // Skip the root itself
            }

            let (entry, _) = build_path_entry(path, with_lines);
            entries.push(entry);
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let total_entries = entries.len();
        if let Some(limit) = limit {
            entries.truncate(limit);
        }
        ToolResult::ok(path_entries_value(entries, total_entries))
    }
}

fn parse_depth(args: &serde_json::Value) -> Result<Option<usize>, ToolResult> {
    match args.get("depth") {
        None => Ok(Some(DEFAULT_DEPTH)),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => {
            if let Some(s) = v.as_str() {
                if s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid depth: expected int, null, or \"none\""
                )));
            }

            let n = match v.as_u64() {
                Some(n) => n,
                None => {
                    return Err(ToolResult::err_fmt(format_args!(
                        "Invalid depth: expected int, null, or \"none\""
                    )));
                }
            };

            if n == 0 {
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid depth: must be >= 1, or use null/\"none\" for no cap"
                )));
            }
            Ok(Some(n as usize))
        }
    }
}

fn parse_limit(args: &serde_json::Value) -> Result<Option<usize>, ToolResult> {
    match args.get("limit") {
        None => Ok(Some(MAX_ENTRIES)),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => {
            if let Some(s) = v.as_str() {
                if s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid limit: expected int, null, or \"none\""
                )));
            }

            let n = match v.as_u64() {
                Some(n) => n,
                None => {
                    return Err(ToolResult::err_fmt(format_args!(
                        "Invalid limit: expected int, null, or \"none\""
                    )));
                }
            };

            if n == 0 {
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid limit: must be >= 1, or use null/\"none\" for no cap"
                )));
            }
            Ok(Some(n as usize))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;
    use serde_json::json;
    use tempfile::TempDir;

    fn items(result: &ToolResult) -> &Vec<serde_json::Value> {
        let obj = result.result.as_object().unwrap();
        assert_eq!(
            obj.get("__type__").and_then(|v| v.as_str()),
            Some("path_entries")
        );
        obj.get("items").and_then(|v| v.as_array()).unwrap()
    }

    #[tokio::test]
    async fn test_ls_files_and_dirs() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir/nested.rs"), "").unwrap();
        let tool = Ls;
        let result = tool
            .execute("ls", &json!({"path": dir.path().to_str().unwrap()}))
            .await;
        assert!(result.success);
        let arr = items(&result);
        let paths: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("path").and_then(|x| x.as_str()))
            .collect();
        assert!(paths.iter().any(|p| p.contains("file.txt")));
        assert!(paths.iter().any(|p| p.contains("subdir")));
        assert!(paths.iter().any(|p| p.contains("nested.rs")));
    }

    #[tokio::test]
    async fn test_ls_empty_dir() {
        let dir = TempDir::new().unwrap();
        let tool = Ls;
        let result = tool
            .execute("ls", &json!({"path": dir.path().to_str().unwrap()}))
            .await;
        assert!(result.success);
        assert!(items(&result).is_empty());
    }

    #[tokio::test]
    async fn test_ls_not_a_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "").unwrap();
        let tool = Ls;
        let result = tool
            .execute("ls", &json!({"path": path.to_str().unwrap()}))
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_ls_depth_limit() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::write(dir.path().join("a/b/c/file.txt"), "").unwrap();
        let tool = Ls;
        let result = tool
            .execute(
                "ls",
                &json!({"path": dir.path().to_str().unwrap(), "depth": 1}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        let paths: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("path").and_then(|x| x.as_str()))
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("/a")));
        assert!(!paths.iter().any(|p| p.ends_with("/b")));
        assert!(!paths.iter().any(|p| p.ends_with("/file.txt")));
    }

    #[tokio::test]
    async fn test_ls_limit_truncation_metadata() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let tool = Ls;
        let result = tool
            .execute(
                "ls",
                &json!({"path": dir.path().to_str().unwrap(), "limit": 2}),
            )
            .await;
        assert!(result.success);
        assert_eq!(items(&result).len(), 2);
        let truncated = result
            .result
            .get("truncated")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(truncated.get("shown").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(truncated.get("total").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(truncated.get("omitted").and_then(|v| v.as_u64()), Some(1));
    }

    #[tokio::test]
    async fn test_ls_with_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "line1\nline2\n").unwrap();
        let tool = Ls;
        let result = tool
            .execute(
                "ls",
                &json!({"path": dir.path().to_str().unwrap(), "with_lines": true}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("lines").and_then(|v| v.as_u64()), Some(2));
    }
}
