use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lash_core::{ToolCall, ToolDefinition, ToolOutcome, ToolRetryPolicy};

use lash_tool_support::{
    FS_DEFAULTS_PREAMBLE, OptionalUsizeArg, StaticToolExecute, StaticToolProvider,
    ToolDefinitionLashlangExt, TruncationMeta, default_glob_limit, default_path_dot,
    execute_typed_tool, execution_failure, invalid_request_failure, io_failure, non_empty_string,
    resolve_under, rg_file_list, run_blocking_value,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Find files by glob pattern.
#[derive(Default)]
pub struct Glob;

/// Build the cached `glob` tool provider.
pub fn glob_provider() -> StaticToolProvider<Glob> {
    StaticToolProvider::new(vec![glob_tool_definition()], Glob)
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    /// Glob pattern to match.
    pattern: String,
    /// Base directory to search in.
    #[serde(default = "default_path_dot")]
    path: String,
    /// Maximum results to return. Use null or "none" for no cap.
    #[serde(default = "default_glob_limit")]
    limit: OptionalUsizeArg,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobOutput {
    paths: Vec<String>,
    truncated: Option<TruncationMeta>,
}

#[async_trait::async_trait]
impl StaticToolExecute for Glob {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        execute_typed_tool::<GlobArgs, GlobOutput, _, _>(call.args, |args| async move {
            match run_blocking_value(move || execute_glob_sync(args)).await {
                Ok(result) => result,
                Err(err) => Err(err),
            }
        })
        .await
    }
}

fn execute_glob_sync(args: GlobArgs) -> Result<GlobOutput, ToolOutcome> {
    non_empty_string(&args.pattern, "pattern")?;
    let limit = args.limit.into_option("limit", 1)?;
    let cwd = std::env::current_dir().map_err(|err| {
        io_failure(
            "current_dir_failed",
            format!("Failed to determine cwd: {err}"),
        )
    })?;
    let base = resolve_under(&cwd, Path::new(&args.path));
    if !base.exists() {
        return Err(io_failure(
            "path_not_found",
            format!("Path does not exist: {}", base.display()),
        ));
    }
    if !base.is_dir() {
        return Err(invalid_request_failure(
            "path_not_directory",
            format!(
                "{} is a file, not a directory. Pass the parent directory as path and use the pattern to match files.",
                base.display()
            ),
        ));
    }

    let glob = globset::GlobBuilder::new(&args.pattern)
        .literal_separator(false)
        .build()
        .map_err(|err| {
            invalid_request_failure(
                "invalid_glob_pattern",
                format!("Invalid glob pattern: {err}"),
            )
        })?;
    let matcher = globset::GlobSetBuilder::new()
        .add(glob)
        .build()
        .map_err(|err| {
            execution_failure(
                "glob_matcher_build_failed",
                format!("Failed to build glob matcher: {err}"),
            )
        })?;

    let files = rg_file_list(&base, false, true, None, &[])?;

    let mut matched_paths = BTreeSet::new();
    for file in files {
        let Ok(rel_path) = file.strip_prefix(&base) else {
            continue;
        };
        if matcher.is_match(rel_path) {
            matched_paths.insert(file.clone());
        }
        let components = rel_path.components().collect::<Vec<_>>();
        if components.len() <= 1 {
            continue;
        }
        let mut current = PathBuf::new();
        for component in components.iter().take(components.len() - 1) {
            current.push(component.as_os_str());
            if matcher.is_match(&current) {
                matched_paths.insert(base.join(&current));
            }
        }
    }

    let total_matches = matched_paths.len();
    let mut paths = matched_paths
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    if let Some(limit) = limit {
        paths.truncate(limit);
    }

    let shown = paths.len();
    let truncated = (total_matches > shown).then_some(TruncationMeta {
        shown,
        total: total_matches,
        omitted: total_matches - shown,
    });
    Ok(GlobOutput { paths, truncated })
}

fn glob_tool_definition() -> ToolDefinition {
    ToolDefinition::typed::<GlobArgs, GlobOutput>(
                "tool:glob",
                "glob",
                [
                    "Find filesystem paths by glob. ",
                    FS_DEFAULTS_PREAMBLE,
                    " Returns `paths` sorted lexicographically with truncation metadata. Defaults: path=\".\", limit=100.",
                ]
                .concat(),
            )
            .with_examples(vec![
                r#"await files.glob({ pattern: "**/*.rs", path: "crates/lash/src", limit: 50 })?"#.into(),
                r#"await files.glob({ pattern: "**/Cargo.toml", path: "." })?"#.into(),
            ])
            .with_lashlang_binding(lash_tool_support::lashlang_binding(
                ["files"],
                "glob",
                &["find_files"],
            ))
            .with_retry_policy(ToolRetryPolicy::safe(2, 25, 100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn paths(result: &ToolOutcome) -> Vec<String> {
        let value = result.value_for_projection();
        value
            .get("paths")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn glob_contract_documents_result_shape() {
        let definition = glob_tool_definition();
        assert_eq!(
            definition.contract.output_schema.canonical["type"],
            json!("object")
        );
        assert!(definition.contract.output_schema.canonical["properties"]["paths"].is_object());
        assert!(
            definition
                .compact_contract()
                .render_signature()
                .contains("paths")
        );
    }

    #[tokio::test]
    async fn test_glob_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let paths = paths(&result);
        assert!(paths.iter().any(|p| p.contains("a.rs")));
        assert!(paths.iter().any(|p| p.contains("b.rs")));
        assert!(!paths.iter().any(|p| p.contains("c.txt")));
    }

    #[tokio::test]
    async fn test_glob_no_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        assert!(paths(&result).is_empty());
    }

    #[tokio::test]
    async fn invalid_glob_pattern_is_a_structured_invalid_request() {
        let dir = TempDir::new().unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "[", "path": dir.path()}),
        )
        .await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("invalid glob must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::InvalidRequest);
        assert_eq!(failure.code, "invalid_glob_pattern");
        assert!(failure.message.contains("Invalid glob pattern"));
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }

    #[tokio::test]
    async fn test_glob_nested() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/file.rs"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "**/*.rs", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let paths = paths(&result);
        assert!(paths.iter().any(|p| p.contains("file.rs")));
    }

    #[tokio::test]
    async fn test_glob_truncation_marker() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "limit": 2}),
        )
        .await;
        assert!(result.is_success());
        assert_eq!(paths(&result).len(), 2);
        let value = result.value_for_projection();
        let truncated = value.get("truncated").and_then(|v| v.as_object()).unwrap();
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
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "limit": null}),
        )
        .await;
        assert!(result.is_success());
        assert_eq!(paths(&result).len(), 3);
        assert!(
            result
                .value_for_projection()
                .get("truncated")
                .map(|v| v.is_null())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn test_glob_rejects_removed_list_like_options() {
        let dir = TempDir::new().unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "with_lines": true}),
        )
        .await;
        assert!(!result.is_success());
    }

    #[tokio::test]
    async fn test_glob_excludes_hidden_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".hidden.rs"), "").unwrap();
        std::fs::write(dir.path().join("shown.rs"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let paths = paths(&result);
        assert!(paths.iter().any(|p| p.ends_with("/shown.rs")));
        assert!(!paths.iter().any(|p| p.ends_with("/.hidden.rs")));
    }

    #[tokio::test]
    async fn test_glob_respects_repo_gitignore_by_default() {
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({
                "pattern": "*.rs",
                "path": dir.path().to_str().unwrap()
            }),
        )
        .await;
        assert!(result.is_success());
        let paths = paths(&result);
        assert!(!paths.iter().any(|p| p.ends_with("/ignored.rs")));
    }

    #[tokio::test]
    async fn test_glob_excludes_dot_git_even_when_pattern_matches_it() {
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": ".git/**", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        assert!(paths(&result).is_empty());
    }

    #[tokio::test]
    async fn test_glob_excludes_node_modules_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(dir.path().join("app.js"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &glob_provider(),
            "glob",
            &json!({"pattern": "**/*.js", "path": dir.path().to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let paths = paths(&result);
        assert!(paths.iter().any(|p| p.ends_with("/app.js")));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));
    }
}
