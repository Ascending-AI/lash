//! Shared implementation support for Lash tools.
//!
//! Filesystem path resolution in this crate exists to make tool behavior
//! predictable. It is not a security boundary: tools decide which files they
//! should expose, while sandboxing and filesystem isolation belong to the host.

use lash_core::{ToolDefinition, ToolFailure, ToolFailureClass, ToolOutcome};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::future::Future;
use std::path::{Component, Path, PathBuf};

mod static_provider;
#[cfg(feature = "lashlang")]
pub use lash_lashlang_runtime::ToolBinding;
pub use static_provider::{StaticToolExecute, StaticToolProvider};

#[cfg(not(feature = "lashlang"))]
#[derive(Clone, Debug, Default)]
pub struct ToolBinding;

#[cfg(not(feature = "lashlang"))]
impl ToolBinding {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        operation: impl Into<String>,
    ) -> Self {
        let _ = module_path
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>();
        let _ = operation.into();
        Self
    }

    pub fn with_authority_type(self, authority_type: impl Into<String>) -> Self {
        let _ = authority_type.into();
        self
    }

    pub fn with_aliases(self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let _ = aliases.into_iter().map(Into::into).collect::<Vec<String>>();
        self
    }
}

pub trait ToolDefinitionBindingExt {
    fn with_tool_binding(self, lashlang_binding: ToolBinding) -> Self;
}

#[cfg(feature = "lashlang")]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, lashlang_binding: ToolBinding) -> Self {
        lash_lashlang_runtime::ToolDefinitionBindingExt::with_tool_binding(self, lashlang_binding)
    }
}

#[cfg(not(feature = "lashlang"))]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, _lashlang_binding: ToolBinding) -> Self {
        self
    }
}

/// Resolve a possibly-relative `path` against `base`, returning a lexically
/// normalized [`PathBuf`]. File tools pass the process current working
/// directory as `base`, so relative tool paths consistently resolve from that
/// directory.
///
/// Behavior:
/// - Absolute `path` passes through unchanged (only normalized).
/// - Relative `path` is joined onto `base`.
/// - `.` and `..` components are collapsed *lexically* — purely by string
///   manipulation, without touching the filesystem and without requiring the
///   path (or its parents) to exist.
///
/// Lexical resolution is deliberate: missing targets and missing parents are
/// accepted, and symlink components remain exactly as named rather than being
/// rewritten to their real paths. This convention provides predictable path
/// handling only; it does not scope access or enforce a sandbox. A tool remains
/// responsible for deciding whether a file should be accessible, and the host
/// owns any security isolation or sandboxing policy.
pub fn resolve_under(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_lexical(&joined)
}

/// Lexically collapse `.` and `..` components in `path` without touching the
/// filesystem. Leading `..` components (that would escape the root) are
/// preserved verbatim, matching `Path::join` intuitions for relative roots.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::ParentDir) | None if !path.has_root() => {
                    normalized.push(component.as_os_str());
                }
                Some(Component::Prefix(_) | Component::RootDir)
                | Some(Component::ParentDir)
                | None => {}
                Some(Component::CurDir) => unreachable!("current-directory components are removed"),
            },
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Render `path` relative to `base` for display, falling back to the file name
/// (then the full path) when `path` is not under `base`. Backslashes are
/// normalized to forward slashes so output is stable across platforms.
pub fn display_relative(base: &Path, path: &Path) -> String {
    let display = path
        .strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string();
    let display = if display.is_empty() {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".")
            .to_string()
    } else {
        display
    };
    display.replace('\\', "/")
}

/// Shared preamble describing default filesystem discovery behavior.
pub const FS_DEFAULTS_PREAMBLE: &str = "By default this excludes hidden entries, `.git`, and `node_modules`, and respects ignore files.";

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TruncationMeta {
    pub shown: usize,
    pub total: usize,
    pub omitted: usize,
}

pub fn invalid_request_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::invalid_request(code, message))
}

pub fn io_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::io(code, message))
}

pub fn retryable_io_failure(
    code: impl Into<String>,
    message: impl Into<String>,
    after_ms: Option<u64>,
) -> ToolOutcome {
    ToolOutcome::retryable_failure(ToolFailureClass::Io, code, message, after_ms)
}

pub fn execution_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::tool(
        ToolFailureClass::Execution,
        code,
        message,
    ))
}

pub fn invalid_tool_args(message: impl Into<String>) -> ToolOutcome {
    invalid_request_failure("invalid_tool_args", message)
}

pub fn typed_tool_args<Args>(args: &serde_json::Value) -> Result<Args, ToolOutcome>
where
    Args: DeserializeOwned + JsonSchema,
{
    serde_json::from_value(args.clone())
        .map_err(|err| invalid_tool_args(format!("Invalid tool arguments: {err}")))
}

pub fn typed_tool_ok<Output>(output: Output) -> ToolOutcome
where
    Output: Serialize + JsonSchema,
{
    match serde_json::to_value(output) {
        Ok(value) => ToolOutcome::ok(value),
        Err(err) => execution_failure(
            "tool_result_serialization_failed",
            format!("Failed to serialize tool result: {err}"),
        ),
    }
}

pub async fn execute_typed_tool<Args, Output, F, Fut>(
    args: &serde_json::Value,
    execute: F,
) -> ToolOutcome
where
    Args: DeserializeOwned + JsonSchema,
    Output: Serialize + JsonSchema,
    F: FnOnce(Args) -> Fut,
    Fut: Future<Output = Result<Output, ToolOutcome>>,
{
    let args = match typed_tool_args::<Args>(args) {
        Ok(args) => args,
        Err(err) => return err,
    };
    match execute(args).await {
        Ok(output) => typed_tool_ok(output),
        Err(err) => err,
    }
}

pub async fn execute_typed_tool_result<Args, F, Fut>(
    args: &serde_json::Value,
    execute: F,
) -> ToolOutcome
where
    Args: DeserializeOwned + JsonSchema,
    F: FnOnce(Args) -> Fut,
    Fut: Future<Output = ToolOutcome>,
{
    let args = match typed_tool_args::<Args>(args) {
        Ok(args) => args,
        Err(err) => return err,
    };
    execute(args).await
}

pub fn non_empty_string(value: &str, key: &str) -> Result<(), ToolOutcome> {
    if value.is_empty() {
        Err(invalid_tool_args(format!(
            "Missing required parameter: {key}"
        )))
    } else {
        Ok(())
    }
}

pub fn default_path_dot() -> String {
    ".".to_string()
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum OptionalUsizeArg {
    Value(usize),
    NoneString(String),
    Null(()),
}

impl OptionalUsizeArg {
    pub fn into_option(self, key: &str, min: usize) -> Result<Option<usize>, ToolOutcome> {
        match self {
            Self::Value(value) if value >= min => Ok(Some(value)),
            Self::Value(_) => Err(invalid_tool_args(format!(
                "Invalid {key}: must be >= {min}, or use null/\"none\" for no cap"
            ))),
            Self::NoneString(value) if value.eq_ignore_ascii_case("none") => Ok(None),
            Self::NoneString(_) => Err(invalid_tool_args(format!(
                "Invalid {key}: expected int, null, or \"none\""
            ))),
            Self::Null(()) => Ok(None),
        }
    }
}

pub fn deserialize_optional_usize_none<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptionalUsize {
        Int(usize),
        String(String),
        Null,
    }

    match Option::<OptionalUsize>::deserialize(deserializer)? {
        None | Some(OptionalUsize::Null) => Ok(None),
        Some(OptionalUsize::Int(value)) => Ok(Some(value)),
        Some(OptionalUsize::String(value)) if value.eq_ignore_ascii_case("none") => Ok(None),
        Some(OptionalUsize::String(_)) => Err(serde::de::Error::custom(
            "expected integer, null, or \"none\"",
        )),
    }
}

pub fn default_glob_limit() -> OptionalUsizeArg {
    OptionalUsizeArg::Value(100)
}

/// Extract a required non-empty string arg, or return a structured invalid request.
pub fn require_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_tool_args(format!("Missing required parameter: {key}")))
}

/// Parse optional bool arg with a default.
pub fn parse_optional_bool(
    args: &serde_json::Value,
    key: &str,
    default: bool,
) -> Result<bool, ToolOutcome> {
    match args.get(key) {
        None => Ok(default),
        Some(v) if v.is_null() => Ok(default),
        Some(v) => match v.as_bool() {
            Some(b) => Ok(b),
            None => Err(invalid_tool_args(format!("Invalid {key}: expected bool"))),
        },
    }
}

/// Parse an optional positive integer arg.
/// Accepts `null` or `"none"` when `allow_none` is true.
pub fn parse_optional_usize_arg(
    args: &serde_json::Value,
    key: &str,
    default: Option<usize>,
    allow_none: bool,
    min: usize,
) -> Result<Option<usize>, ToolOutcome> {
    match args.get(key) {
        None => Ok(default),
        Some(v) if v.is_null() => {
            if allow_none {
                Ok(None)
            } else {
                Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int >= {min}"
                )))
            }
        }
        Some(v) => {
            if let Some(s) = v.as_str() {
                if allow_none && s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                )));
            }
            let n = v.as_u64().ok_or_else(|| {
                invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                ))
            })? as usize;
            if n < min {
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: must be >= {min}{}",
                    if allow_none {
                        ", or use null/\"none\" for no cap"
                    } else {
                        ""
                    }
                )));
            }
            Ok(Some(n))
        }
    }
}

pub fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn tool_binding(
    module_path: impl IntoIterator<Item = impl Into<String>>,
    operation: impl Into<String>,
    aliases: &[&str],
) -> ToolBinding {
    ToolBinding::new(module_path, operation).with_aliases(aliases.iter().copied())
}

/// Run blocking filesystem work off the async runtime.
pub async fn run_blocking<F>(f: F) -> ToolOutcome
where
    F: FnOnce() -> ToolOutcome + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) => execution_failure(
            "blocking_task_failed",
            format!("blocking task failed: {err}"),
        ),
    }
}

/// Run blocking work off the async runtime and return a typed value.
pub async fn run_blocking_value<F, T>(f: F) -> Result<T, ToolOutcome>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|err| {
        execution_failure(
            "blocking_task_failed",
            format!("blocking task failed: {err}"),
        )
    })
}

pub fn rg_file_list(
    base: &Path,
    show_hidden_entries: bool,
    respect_ignore_files: bool,
    max_depth: Option<usize>,
    globs: &[String],
) -> Result<Vec<PathBuf>, ToolOutcome> {
    if is_default_excluded_entry(base) {
        return Ok(Vec::new());
    }

    let mut builder = ignore::WalkBuilder::new(base);
    builder
        .hidden(!show_hidden_entries)
        .max_depth(max_depth)
        .filter_entry(|entry| !is_default_excluded_entry(entry.path()));

    if respect_ignore_files {
        builder.git_ignore(true).git_exclude(true).git_global(true);
        builder.require_git(true);
    } else {
        builder
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false)
            .ignore(false)
            .parents(false)
            .require_git(false);
    }

    if !globs.is_empty() {
        let mut override_builder = ignore::overrides::OverrideBuilder::new(base);
        for glob in globs {
            override_builder.add(glob).map_err(|err| {
                invalid_request_failure(
                    "invalid_ignore_glob",
                    format!("invalid ignore glob for {}: {err}", base.display()),
                )
            })?;
        }

        let overrides = override_builder.build().map_err(|err| {
            execution_failure(
                "ignore_glob_build_failed",
                format!("failed to build ignore globs for {}: {err}", base.display()),
            )
        })?;
        builder.overrides(overrides);
    }

    let files = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != base)
        .filter(|entry| !is_default_excluded_entry(entry.path()))
        .map(ignore::DirEntry::into_path)
        .collect();
    Ok(files)
}

fn is_default_excluded_entry(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        matches!(name.as_ref(), ".git" | "node_modules")
    })
}

/// Generate a compact unified diff between old and new content.
/// Truncates to `max_lines` lines if the diff is too long.
pub fn compact_diff(old: &str, new: &str, path: &str, max_lines: usize) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let unified = diff
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if unified.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = unified.lines().collect();
    if lines.len() <= max_lines {
        unified
    } else {
        let mut truncated: String = lines[..max_lines].join("\n");
        truncated.push_str(&format!("\n... ({} more lines)", lines.len() - max_lines));
        truncated
    }
}

#[cfg(test)]
mod path_tests {
    use super::{normalize_lexical, resolve_under};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[test]
    fn absolute_paths_are_normalized_without_using_the_base() {
        assert_eq!(
            resolve_under(Path::new("/ignored"), Path::new("/tmp/./alpha/../beta")),
            PathBuf::from("/tmp/beta")
        );
    }

    #[test]
    fn relative_paths_resolve_under_the_supplied_base() {
        assert_eq!(
            resolve_under(Path::new("/workspace/project"), Path::new("src/lib.rs")),
            PathBuf::from("/workspace/project/src/lib.rs")
        );
    }

    #[test]
    fn dot_segments_are_collapsed_lexically() {
        assert_eq!(
            resolve_under(
                Path::new("/workspace/project"),
                Path::new("./src/../Cargo.toml")
            ),
            PathBuf::from("/workspace/project/Cargo.toml")
        );
        assert_eq!(
            normalize_lexical(Path::new("../../src")),
            PathBuf::from("../../src")
        );
        assert_eq!(
            normalize_lexical(Path::new("/../../src")),
            PathBuf::from("/src")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_resolved() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

        assert_eq!(
            resolve_under(dir.path(), Path::new("link/file.txt")),
            dir.path().join("link/file.txt")
        );
    }

    #[test]
    fn missing_paths_and_parents_are_allowed() {
        let dir = TempDir::new().unwrap();
        let resolved = resolve_under(dir.path(), Path::new("missing/parents/file.txt"));

        assert_eq!(resolved, dir.path().join("missing/parents/file.txt"));
        assert!(!resolved.exists());
    }

    #[test]
    fn unicode_components_are_preserved() {
        assert_eq!(
            resolve_under(Path::new("/workspace"), Path::new("données/日本語.txt")),
            PathBuf::from("/workspace/données/日本語.txt")
        );
    }
}
