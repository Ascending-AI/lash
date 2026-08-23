//! Shared implementation support for Lash tools.

use lash_core::{ToolDefinition, ToolFailure, ToolFailureClass, ToolOutcome};
use serde::{Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

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
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
}

#[cfg(feature = "lashlang")]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self {
        lash_lashlang_runtime::ToolDefinitionBindingExt::with_tool_binding(self, tool_binding)
    }
}

#[cfg(not(feature = "lashlang"))]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, _tool_binding: ToolBinding) -> Self {
        self
    }
}

pub fn invalid_request_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::invalid_request(code, message))
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

/// Deserialize typed tool arguments and report serde failures in the canonical
/// structured invalid-request shape exposed to the model.
pub fn typed_args<Args>(args: &serde_json::Value) -> Result<Args, ToolOutcome>
where
    Args: DeserializeOwned,
{
    serde_path_to_error::deserialize(args)
        .map_err(|err| invalid_tool_args(format!("Invalid tool arguments: {err}")))
}

/// Serialize a typed tool result into the canonical successful tool outcome.
pub fn typed_ok<Output>(output: Output) -> ToolOutcome
where
    Output: Serialize,
{
    match serde_json::to_value(output) {
        Ok(value) => ToolOutcome::ok(value),
        Err(err) => execution_failure(
            "tool_result_serialization_failed",
            format!("Failed to serialize tool result: {err}"),
        ),
    }
}

/// Resolve `path` against `root`, canonicalize both, and reject any result
/// outside the canonical root. Canonicalization makes both lexical `..`
/// traversal and symlink escapes subject to the same component-wise boundary.
/// The target must exist because the filesystem is consulted to resolve every
/// component.
pub fn canonicalize_under(root: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical_path = std::fs::canonicalize(resolve_under(root, path))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("path escapes root: {}", canonical_path.display()),
        ));
    }
    Ok(canonical_path)
}

/// Validate a required string argument without accepting the empty string.
pub fn non_empty_string(value: &str, key: &str) -> Result<(), ToolOutcome> {
    if value.is_empty() {
        Err(invalid_tool_args(format!(
            "Missing required parameter: {key}"
        )))
    } else {
        Ok(())
    }
}

/// Extract a required non-empty string arg, or return a structured invalid request.
pub fn require_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutcome> {
    let value = args
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid_tool_args(format!("Missing required parameter: {key}")))?;
    non_empty_string(value, key)?;
    Ok(value)
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

/// Resolve a possibly-relative path against `base` and collapse `.`/`..`
/// lexically. This is the historical path resolution primitive used by
/// [`canonicalize_under`] before filesystem boundary enforcement.
fn resolve_under(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_under, non_empty_string, typed_args, typed_ok};
    use lash_core::{ToolCallOutcome, ToolFailureClass};
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("lash-tool-support-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct TypedArgs {
        count: usize,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct NestedTypedArgs {
        inner: NestedTypedArgsInner,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct NestedTypedArgsInner {
        count: usize,
    }

    #[derive(Debug, Serialize)]
    struct TypedOutput {
        ok: bool,
    }

    #[test]
    fn typed_args_returns_the_canonical_invalid_args_outcome() {
        let outcome = typed_args::<TypedArgs>(&json!({"count": "not-an-integer"}))
            .expect_err("invalid typed arguments");
        let output = outcome.as_output();
        let ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("typed args must return a structured failure");
        };
        assert_eq!(failure.class, ToolFailureClass::InvalidRequest);
        assert_eq!(failure.code, "invalid_tool_args");
        assert_eq!(failure.source, lash_core::ToolFailureSource::Tool);
        assert_eq!(
            failure.message,
            "Invalid tool arguments: count: invalid type: string \"not-an-integer\", expected usize"
        );
        assert!(
            serde_json::to_value(output)
                .expect("tool outcome serializes")
                .to_string()
                .contains("invalid_tool_args")
        );
    }

    #[test]
    fn typed_args_pins_nested_error_path() {
        let outcome = typed_args::<NestedTypedArgs>(&json!({
            "inner": {"count": "x"}
        }))
        .expect_err("invalid nested typed arguments");
        let output = outcome.as_output();
        let ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("typed args must return a structured failure");
        };
        assert_eq!(
            failure.message,
            "Invalid tool arguments: inner.count: invalid type: string \"x\", expected usize"
        );
    }

    #[test]
    fn typed_ok_returns_a_success_outcome() {
        let outcome = typed_ok(TypedOutput { ok: true });
        assert_eq!(
            outcome.as_output().outcome,
            lash_core::ToolCallOutcome::Success(lash_core::ToolValue::from(json!({"ok": true})))
        );
    }

    #[test]
    fn non_empty_string_rejects_empty_values() {
        let outcome = non_empty_string("", "query").expect_err("empty value");
        assert!(matches!(
            outcome.as_output().outcome,
            lash_core::ToolCallOutcome::Failure(_)
        ));
    }

    #[test]
    fn canonicalize_under_refuses_parent_traversal() {
        let dir = TestDir::new();
        let outside = dir.0.parent().expect("temp parent").join(format!(
            "{}-parent",
            dir.0.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&outside, "outside").expect("write outside file");
        let result = canonicalize_under(
            &dir.0,
            &PathBuf::from("..").join(outside.file_name().expect("outside file name")),
        );
        let _ = std::fs::remove_file(&outside);
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn canonicalize_under_refuses_absolute_paths_outside_root() {
        let dir = TestDir::new();
        let outside = dir.0.parent().expect("temp parent").join(format!(
            "{}-outside",
            dir.0.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&outside, "outside").expect("write outside file");
        let result = canonicalize_under(&dir.0, &outside);
        let _ = std::fs::remove_file(&outside);
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_under_refuses_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new();
        let outside_dir = TestDir::new();
        let outside_file = outside_dir.0.join("secret.txt");
        std::fs::write(&outside_file, "secret").expect("write outside file");
        symlink(&outside_dir.0, dir.0.join("link")).expect("create escape symlink");

        let result = canonicalize_under(&dir.0, Path::new("link/secret.txt"));
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
