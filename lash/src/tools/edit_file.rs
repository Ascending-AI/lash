use serde_json::json;
use std::path::Path;

use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::hashline::{self, HashlineEdit};
use super::{compact_diff, read_to_string, require_str, run_blocking};

/// Hashline-aware file editing tool.
#[derive(Default)]
pub struct EditFile;

#[async_trait::async_trait]
impl ToolProvider for EditFile {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "edit_file".into(),
            description: concat!(
                "Anchor-based file editor for structural or multi-line changes. ",
                "Use this when you need precise placement (insert/delete/replace ranges) and have anchors from `read_file`.\n\n",
                "If you already know the exact text to change and just want a quick substitution, prefer `find_replace`.\n\n",
                "Each edit is one of: ",
                "{\"set_line\": {\"anchor\": \"LINE:8HEX\", \"new_text\": \"...\"}}, ",
                "{\"replace_lines\": {\"start_anchor\": \"LINE:8HEX\", \"end_anchor\": \"LINE:8HEX\", \"new_text\": \"...\"}}, ",
                "{\"insert_after\": {\"anchor\": \"LINE:8HEX\", \"text\": \"...\"}}, ",
                "{\"replace\": {\"old_text\": \"...\", \"new_text\": \"...\", \"all\": false}}",
            ).into(),
            params: vec![
                ToolParam::typed("path", "str"),
                ToolParam {
                    name: "edits".into(),
                    r#type: "list".into(),
                    description: "List of edit operations".into(),
                    required: true,
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
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };

        let edits_json = match args.get("edits").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => {
                return ToolResult::err(json!(
                    "Missing or invalid 'edits' parameter: expected a list"
                ));
            }
        };
        let edits_count = edits_json.len();

        let edits = match parse_edits(edits_json) {
            Ok(e) => e,
            Err(msg) => {
                return ToolResult::err(json!(msg));
            }
        };

        run_blocking(move || {
            let path = Path::new(&path_str);
            if !path.exists() {
                return ToolResult::err_fmt(format_args!("File does not exist: {path_str}"));
            }

            let content = match read_to_string(path) {
                Ok(c) => c,
                Err(e) => return e,
            };

            match hashline::apply_hashline_edits(&content, edits) {
                Ok(new_content) => {
                    if let Err(e) = std::fs::write(path, &new_content) {
                        return ToolResult::err_fmt(format_args!("Failed to write file: {e}"));
                    }
                    let new_lines = new_content.lines().count();
                    let summary = format!(
                        "Applied {} edit(s) to {} ({} lines)",
                        edits_count, path_str, new_lines,
                    );
                    let diff = compact_diff(&content, &new_content, &path_str, 50);
                    ToolResult::ok(json!({
                        "__type__": "edit_result",
                        "summary": summary,
                        "diff": diff,
                    }))
                }
                Err(msg) => ToolResult::err(json!(msg)),
            }
        })
        .await
    }
}

fn parse_edits(edits: &[serde_json::Value]) -> Result<Vec<HashlineEdit>, String> {
    let mut result = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let edit = parse_single_edit(edit).map_err(|e| format!("Edit #{}: {}", i + 1, e))?;
        result.push(edit);
    }
    Ok(result)
}

fn parse_single_edit(edit: &serde_json::Value) -> Result<HashlineEdit, String> {
    if let Some(obj) = edit.get("set_line") {
        let anchor = obj
            .get("anchor")
            .and_then(|v| v.as_str())
            .ok_or("set_line: missing 'anchor'")?
            .to_string();
        let new_text = obj
            .get("new_text")
            .and_then(|v| v.as_str())
            .ok_or("set_line: missing 'new_text'")?
            .to_string();
        return Ok(HashlineEdit::SetLine { anchor, new_text });
    }

    if let Some(obj) = edit.get("replace_lines") {
        let start_anchor = obj
            .get("start_anchor")
            .and_then(|v| v.as_str())
            .ok_or("replace_lines: missing 'start_anchor'")?
            .to_string();
        let end_anchor = obj
            .get("end_anchor")
            .and_then(|v| v.as_str())
            .ok_or("replace_lines: missing 'end_anchor'")?
            .to_string();
        let new_text = obj
            .get("new_text")
            .and_then(|v| v.as_str())
            .ok_or("replace_lines: missing 'new_text'")?
            .to_string();
        return Ok(HashlineEdit::ReplaceLines {
            start_anchor,
            end_anchor,
            new_text,
        });
    }

    if let Some(obj) = edit.get("insert_after") {
        let anchor = obj
            .get("anchor")
            .and_then(|v| v.as_str())
            .ok_or("insert_after: missing 'anchor'")?
            .to_string();
        let text = obj
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or("insert_after: missing 'text'")?
            .to_string();
        return Ok(HashlineEdit::InsertAfter { anchor, text });
    }

    if let Some(obj) = edit.get("replace") {
        let old_text = obj
            .get("old_text")
            .and_then(|v| v.as_str())
            .ok_or("replace: missing 'old_text'")?
            .to_string();
        let new_text = obj
            .get("new_text")
            .and_then(|v| v.as_str())
            .ok_or("replace: missing 'new_text'")?
            .to_string();
        let all = obj.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        return Ok(HashlineEdit::Replace {
            old_text,
            new_text,
            all,
        });
    }

    Err(format!(
        "Unknown edit type. Expected one of: set_line, replace_lines, insert_after, replace. Got: {}",
        edit
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_set_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let hash = hashline::compute_line_hash("line2");
        let tool = EditFile;
        let result = tool
            .execute("edit_file", &json!({
                "path": path.to_str().unwrap(),
                "edits": [{"set_line": {"anchor": format!("2:{}", hash), "new_text": "replaced"}}]
            }))
            .await;
        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nreplaced\nline3\n"
        );
    }

    #[tokio::test]
    async fn test_insert_after() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let hash = hashline::compute_line_hash("line1");
        let tool = EditFile;
        let result = tool
            .execute("edit_file", &json!({
                "path": path.to_str().unwrap(),
                "edits": [{"insert_after": {"anchor": format!("1:{}", hash), "text": "inserted"}}]
            }))
            .await;
        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\ninserted\nline2\nline3"
        );
    }

    #[tokio::test]
    async fn test_replace_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne").unwrap();
        let hash_b = hashline::compute_line_hash("b");
        let hash_d = hashline::compute_line_hash("d");
        let tool = EditFile;
        let result = tool
            .execute(
                "edit_file",
                &json!({
                    "path": path.to_str().unwrap(),
                    "edits": [{"replace_lines": {
                        "start_anchor": format!("2:{}", hash_b),
                        "end_anchor": format!("4:{}", hash_d),
                        "new_text": "X\nY"
                    }}]
                }),
            )
            .await;
        assert!(result.success);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nX\nY\ne");
    }

    #[tokio::test]
    async fn test_stale_anchor() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let tool = EditFile;
        let result = tool
            .execute(
                "edit_file",
                &json!({
                    "path": path.to_str().unwrap(),
                    "edits": [{"set_line": {"anchor": "2:ffffffff", "new_text": "replaced"}}]
                }),
            )
            .await;
        assert!(!result.success);
    }
}
