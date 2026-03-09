use std::path::Path;

use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::{build_path_entry, parse_optional_bool, path_entries_value, require_str};

/// Find files by glob pattern.
#[derive(Default)]
pub struct Glob;

const MAX_RESULTS: usize = 100;

#[async_trait::async_trait]
impl ToolProvider for Glob {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "glob".into(),
            description: vec![crate::ToolText::new(
                format!(
                    "Find filesystem entries by glob pattern. Returns `PathEntry` objects sorted by `modified_at` (newest first). Each item has: `path`, `kind`, `size_bytes`, `lines` (or null), `modified_at` (RFC3339 UTC). Defaults: limit={}, with_lines=false.",
                    MAX_RESULTS
                ),
                [crate::ExecutionMode::Repl, crate::ExecutionMode::NativeTools],
            )],
            params: vec![
                ToolParam::typed("pattern", "str"),
                ToolParam {
                    name: "path".into(),
                    r#type: "str".into(),
                    description: "Base directory to search in (default: current directory)".into(),
                    required: false,
                },
                ToolParam {
                    name: "limit".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Maximum results to return (default: {}). Use null or \"none\" for no cap.",
                        MAX_RESULTS
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
        let pattern = match require_str(args, "pattern") {
            Ok(s) => s,
            Err(e) => return e,
        };

        let base_dir = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let limit = match parse_limit(args) {
            Ok(l) => l,
            Err(e) => return e,
        };
        let with_lines = match parse_optional_bool(args, "with_lines", false) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let base = Path::new(base_dir);
        if !base.exists() {
            return ToolResult::err_fmt(format_args!("Path does not exist: {base_dir}"));
        }
        if !base.is_dir() {
            return ToolResult::err_fmt(format_args!(
                "{base_dir} is a file, not a directory. Pass the parent directory as path and use the pattern to match files."
            ));
        }

        // Build the glob matcher
        let glob = match globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
        {
            Ok(g) => g,
            Err(e) => {
                return ToolResult::err_fmt(format_args!("Invalid glob pattern: {e}"));
            }
        };

        let matcher = match globset::GlobSetBuilder::new().add(glob).build() {
            Ok(m) => m,
            Err(e) => {
                return ToolResult::err_fmt(format_args!("Failed to build glob matcher: {e}"));
            }
        };

        // Walk the directory tree, respecting .gitignore
        let walker = ignore::WalkBuilder::new(base)
            .hidden(false)
            .git_ignore(true)
            .build();

        let mut matches: Vec<(super::PathEntry, std::time::SystemTime)> = Vec::new();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            // Match against relative path from base
            let rel_path = match path.strip_prefix(base) {
                Ok(r) => r,
                Err(_) => continue,
            };

            if matcher.is_match(rel_path) {
                matches.push(build_path_entry(path, with_lines));
            }
        }

        // Sort by mtime, newest first
        matches.sort_by(|a, b| b.1.cmp(&a.1));
        let total_matches = matches.len();
        if let Some(limit) = limit {
            matches.truncate(limit);
        }

        let items: Vec<super::PathEntry> = matches.into_iter().map(|(entry, _)| entry).collect();
        ToolResult::ok(path_entries_value(items, total_matches))
    }
}

fn parse_limit(args: &serde_json::Value) -> Result<Option<usize>, ToolResult> {
    match args.get("limit") {
        None => Ok(Some(MAX_RESULTS)),
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
    async fn test_glob_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        let paths: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("path").and_then(|x| x.as_str()))
            .collect();
        assert!(paths.iter().any(|p| p.contains("a.rs")));
        assert!(paths.iter().any(|p| p.contains("b.rs")));
        assert!(!paths.iter().any(|p| p.contains("c.txt")));
        assert!(
            arr.iter()
                .all(|v| v.get("size_bytes").and_then(|x| x.as_u64()).is_some())
        );
        assert!(
            arr.iter()
                .all(|v| v.get("modified_at").and_then(|x| x.as_str()).is_some())
        );
        assert!(
            arr.iter()
                .all(|v| v.get("lines").map(|x| x.is_null()).unwrap_or(false))
        );
    }

    #[tokio::test]
    async fn test_glob_no_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        assert!(items(&result).is_empty());
    }

    #[tokio::test]
    async fn test_glob_nested() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/file.rs"), "").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "**/*.rs", "path": dir.path().to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        let paths: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("path").and_then(|x| x.as_str()))
            .collect();
        assert!(paths.iter().any(|p| p.contains("file.rs")));
    }

    #[tokio::test]
    async fn test_glob_truncation_marker() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "limit": 2}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        assert_eq!(arr.len(), 2);
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
    async fn test_glob_limit_none() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "limit": null}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        assert_eq!(arr.len(), 3);
        assert!(
            result
                .result
                .get("truncated")
                .map(|v| v.is_null())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn test_glob_with_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "line1\nline2\nline3\n").unwrap();
        let tool = Glob;
        let result = tool
            .execute(
                "glob",
                &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "with_lines": true}),
            )
            .await;
        assert!(result.success);
        let arr = items(&result);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("lines").and_then(|v| v.as_u64()), Some(3));
    }
}
