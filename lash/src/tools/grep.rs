use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::require_str;

/// Search file contents using regex patterns.
#[derive(Default)]
pub struct Grep;

const MAX_RESULTS: usize = 100;
const DEFAULT_CONTEXT: usize = 3;
const MAX_LINE_LEN: usize = 500;
const TRUNCATION_HINT: &str = "Pass `limit=null` for all.";

#[async_trait::async_trait]
impl ToolProvider for Grep {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "grep".into(),
            description: vec![crate::ToolText::new(
                format!(
                    "Search file contents for a regex pattern (ripgrep-compatible regex). Returns matching lines with file paths and line numbers, plus context lines (ripgrep `-C`) around each match. Each item is a string in ripgrep format: `file:line:text` for matches, `file-line-text` for context lines. Defaults: context={}, limit={}.",
                    DEFAULT_CONTEXT, MAX_RESULTS
                ),
                [
                    crate::ExecutionMode::Repl,
                    crate::ExecutionMode::NativeTools,
                ],
            )],
            params: vec![
                ToolParam::typed("pattern", "str"),
                ToolParam {
                    name: "path".into(),
                    r#type: "str".into(),
                    description: "Directory or file to search in (default: current directory)"
                        .into(),
                    required: false,
                },
                ToolParam {
                    name: "include".into(),
                    r#type: "str".into(),
                    description: "Glob pattern to filter files (e.g. \"*.rs\", \"*.py\")".into(),
                    required: false,
                },
                ToolParam {
                    name: "context".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Number of context lines before/after each match (ripgrep `-C`, default: {}). Use 0 for match-only output.",
                        DEFAULT_CONTEXT
                    ),
                    required: false,
                },
                ToolParam {
                    name: "limit".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Maximum matches to return (default: {}). Use null or \"none\" for no cap.",
                        MAX_RESULTS
                    ),
                    required: false,
                },
            ],
            returns: "list[str]".into(),
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

        let include = args.get("include").and_then(|v| v.as_str());
        let context = match parse_context(args) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let limit = match parse_limit(args) {
            Ok(l) => l,
            Err(e) => return e,
        };

        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::err_fmt(format_args!("Invalid regex pattern: {e}"));
            }
        };

        let include_glob = if let Some(inc) = include {
            match globset::GlobBuilder::new(inc).build() {
                Ok(g) => globset::GlobSetBuilder::new().add(g).build().ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        let base = Path::new(base_dir);

        // If it's a single file, search just that file
        if base.is_file() {
            let searched = search_file(base, &re, context, limit);
            let mut output = format_matches(&searched.entries);
            maybe_append_truncation_line(&mut output, limit, searched.match_count);
            return ToolResult::ok(json!(output));
        }

        if !base.is_dir() {
            return ToolResult::err_fmt(format_args!("Not a file or directory: {base_dir}"));
        }

        let walker = ignore::WalkBuilder::new(base)
            .hidden(false)
            .git_ignore(true)
            .build();

        let mut all_entries: Vec<GrepHit> = Vec::new();
        let mut total_matches: usize = 0;
        let mut rendered_matches: usize = 0;

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            // Apply include filter
            if let Some(ref glob_set) = include_glob {
                let filename = path.file_name().map(Path::new).unwrap_or(path);
                if !glob_set.is_match(filename) {
                    continue;
                }
            }

            let remaining = limit.map(|l| l.saturating_sub(rendered_matches));
            let searched = search_file(path, &re, context, remaining);
            total_matches += searched.match_count;
            rendered_matches += searched.rendered_matches;
            all_entries.extend(searched.entries);
        }

        let mut output = format_matches(&all_entries);
        maybe_append_truncation_line(&mut output, limit, total_matches);
        ToolResult::ok(json!(output))
    }
}

fn parse_context(args: &serde_json::Value) -> Result<usize, ToolResult> {
    match args.get("context") {
        None => Ok(DEFAULT_CONTEXT),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(n as usize),
            None => Err(ToolResult::err_fmt(format_args!(
                "Invalid context: expected int >= 0"
            ))),
        },
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

#[derive(Clone)]
struct GrepHit {
    file: String,
    line: usize,
    content: String,
    is_match: bool,
}

struct SearchResult {
    entries: Vec<GrepHit>,
    match_count: usize,
    rendered_matches: usize,
}

fn maybe_append_truncation_line(
    output: &mut Vec<String>,
    limit: Option<usize>,
    total_matches: usize,
) {
    if let Some(limit) = limit
        && total_matches > limit
    {
        let omitted = total_matches - limit;
        output.push(format!(
            "[results truncated: showing {} of {} matches ({} omitted). {}]",
            limit, total_matches, omitted, TRUNCATION_HINT
        ));
    }
}

fn format_matches(matches: &[GrepHit]) -> Vec<String> {
    matches
        .iter()
        .map(|m| {
            if m.is_match {
                format!("{}:{}:{}", m.file, m.line, m.content)
            } else {
                format!("{}-{}-{}", m.file, m.line, m.content)
            }
        })
        .collect()
}

fn search_file(
    path: &Path,
    re: &regex::Regex,
    context: usize,
    max_matches: Option<usize>,
) -> SearchResult {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            return SearchResult {
                entries: vec![],
                match_count: 0,
                rendered_matches: 0,
            };
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut matched_lines: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            matched_lines.push(i);
        }
    }
    let match_count = matched_lines.len();

    if match_count == 0 {
        return SearchResult {
            entries: vec![],
            match_count: 0,
            rendered_matches: 0,
        };
    }

    let rendered_matches = max_matches
        .map(|m| m.min(match_count))
        .unwrap_or(match_count);
    if rendered_matches == 0 {
        return SearchResult {
            entries: vec![],
            match_count,
            rendered_matches: 0,
        };
    }

    let mut included: BTreeMap<usize, bool> = BTreeMap::new();
    for &match_idx in matched_lines.iter().take(rendered_matches) {
        let start = match_idx.saturating_sub(context);
        let end = (match_idx + context).min(lines.len().saturating_sub(1));
        for i in start..=end {
            let is_match = i == match_idx;
            included
                .entry(i)
                .and_modify(|existing| *existing = *existing || is_match)
                .or_insert(is_match);
        }
    }

    let file = path.to_string_lossy().to_string();
    let entries = included
        .into_iter()
        .map(|(idx, is_match)| {
            let line = lines[idx];
            let content = if line.len() > MAX_LINE_LEN {
                format!("{}...", &line[..MAX_LINE_LEN])
            } else {
                line.to_string()
            };
            GrepHit {
                file: file.clone(),
                line: idx + 1,
                content,
                is_match,
            }
        })
        .collect();

    SearchResult {
        entries,
        match_count,
        rendered_matches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_grep_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "hello world\nfoo bar\nhello again",
        )
        .unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "hello", "path": dir.path().to_str().unwrap(), "context": 0}),
            )
            .await;
        assert!(result.success);
        let arr = result.result.as_array().unwrap();
        let text: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(text.iter().any(|l| l.contains("hello world")));
        assert!(text.iter().any(|l| l.contains("hello again")));
        assert!(!text.iter().any(|l| l.contains("foo bar")));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "xyz", "path": dir.path().to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        assert!(result.result.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_grep_single_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}").unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "println", "path": path.to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        let arr = result.result.as_array().unwrap();
        assert!(arr.iter().any(|v| v.as_str().unwrap().contains("println")));
    }

    #[tokio::test]
    async fn test_grep_truncation_marker() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "hello one\nhello two\nhello three\n",
        )
        .unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "hello", "path": dir.path().to_str().unwrap(), "context": 0, "limit": 2}),
            )
            .await;
        assert!(result.success);
        let arr = result.result.as_array().unwrap();
        let text: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap_or("")).collect();
        assert_eq!(text.len(), 3);
        assert!(text[2].contains("showing 2 of 3"));
        assert!(text[2].contains("1 omitted"));
    }

    #[tokio::test]
    async fn test_grep_limit_none() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "hello one\nhello two\nhello three\n",
        )
        .unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "hello", "path": dir.path().to_str().unwrap(), "context": 0, "limit": null}),
            )
            .await;
        assert!(result.success);
        let arr = result.result.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[tokio::test]
    async fn test_grep_default_context_includes_neighbor_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "hello world\nfoo bar\nhello again",
        )
        .unwrap();
        let tool = Grep;
        let result = tool
            .execute(
                "grep",
                &json!({"pattern": "hello", "path": dir.path().to_str().unwrap()}),
            )
            .await;
        assert!(result.success);
        let arr = result.result.as_array().unwrap();
        let text: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap_or("")).collect();
        assert!(text.iter().any(|l| l.contains(":1:hello world")));
        assert!(text.iter().any(|l| l.contains("-2-foo bar")));
        assert!(text.iter().any(|l| l.contains(":3:hello again")));
    }
}
