use serde_json::json;
use std::path::Path;

use crate::{ToolDefinition, ToolParam, ToolResult};

use super::{compact_diff, read_to_string, require_str};

/// Simple text find-and-replace tool (no anchors needed).
#[derive(Default)]
pub struct FindReplace;

#[async_trait::async_trait]
impl crate::ToolProvider for FindReplace {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "find_replace".into(),
            description: concat!(
                "Fast path for exact-text edits. No anchors needed — works on raw file content.\n\n",
                "Use this when you already know the exact `old_text` to change (single-line fixes, typos, straightforward renames). ",
                "`old_text` must match exactly, including whitespace/newlines. ",
                "By default it replaces only the first occurrence; set `all=true` to replace every match.\n\n",
                "For structural edits (insertions, range rewrites, anchor-sensitive placement), use `edit_file`.",
            ).into(),
            params: vec![
                ToolParam::typed("path", "str"),
                ToolParam {
                    name: "old_text".into(),
                    r#type: "str".into(),
                    description: "Exact text to find".into(),
                    required: true,
                },
                ToolParam {
                    name: "new_text".into(),
                    r#type: "str".into(),
                    description: "Replacement text".into(),
                    required: true,
                },
                ToolParam {
                    name: "all".into(),
                    r#type: "bool".into(),
                    description: "Replace all occurrences (default: false, first only)".into(),
                    required: false,
                },
            ],
            returns: "EditResult".into(),
            examples: vec![],
                hidden: false,
                inject_into_prompt: true,
        }]
    }

    async fn execute(&self, _name: &str, args: &serde_json::Value) -> ToolResult {
        let path_str = match require_str(args, "path") {
            Ok(s) => s,
            Err(e) => return e,
        };

        let old_text = match args.get("old_text").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::err_fmt("Missing required parameter: old_text"),
        };

        let new_text = match args.get("new_text").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::err_fmt("Missing required parameter: new_text"),
        };

        let replace_all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);

        let path = Path::new(path_str);

        if !path.exists() {
            return ToolResult::err_fmt(format_args!("File does not exist: {path_str}"));
        }

        let content = match read_to_string(path) {
            Ok(c) => c,
            Err(e) => return e,
        };

        if !content.contains(old_text) {
            return ToolResult::err_fmt("old_text not found in file");
        }

        let (new_content, count) = if replace_all {
            let count = content.matches(old_text).count();
            (content.replace(old_text, new_text), count)
        } else {
            (content.replacen(old_text, new_text, 1), 1)
        };

        if let Err(e) = std::fs::write(path, &new_content) {
            return ToolResult::err_fmt(format_args!("Failed to write file: {e}"));
        }

        let label = if count == 1 {
            "1 replacement".to_string()
        } else {
            format!("{} replacements", count)
        };

        let diff = compact_diff(&content, &new_content, path_str, 50);
        let summary = format!("{} made in {}", label, path_str);
        ToolResult::ok(json!({
            "__type__": "edit_result",
            "summary": summary,
            "diff": diff,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_replace_first() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "foo bar foo baz").unwrap();
        let tool = FindReplace;
        let result = tool
            .execute(
                "find_replace",
                &json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "foo",
                    "new_text": "qux"
                }),
            )
            .await;
        assert!(result.success);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "qux bar foo baz");
    }

    #[tokio::test]
    async fn test_replace_all() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "foo bar foo baz foo").unwrap();
        let tool = FindReplace;
        let result = tool
            .execute(
                "find_replace",
                &json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "foo",
                    "new_text": "qux",
                    "all": true
                }),
            )
            .await;
        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "qux bar qux baz qux"
        );
        let obj = result.result.as_object().unwrap();
        assert_eq!(
            obj.get("__type__").and_then(|v| v.as_str()),
            Some("edit_result")
        );
        assert!(
            obj.get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("3 replacements")
        );
        assert!(obj.get("diff").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn test_no_match() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let tool = FindReplace;
        let result = tool
            .execute(
                "find_replace",
                &json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "xyz",
                    "new_text": "abc"
                }),
            )
            .await;
        assert!(!result.success);
    }
}
