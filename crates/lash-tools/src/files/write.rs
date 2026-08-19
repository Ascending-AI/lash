use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use lash_core::{ToolCall, ToolDefinition, ToolOutcome};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt, display_relative,
    execute_typed_tool_result, io_failure, non_empty_string, resolve_under, run_blocking,
};

const WRITE_DESCRIPTION: &str = "Write content to a file. Creates the file if it does not exist, overwrites if it does. Automatically creates parent directories. Use write only for new files or complete rewrites.";

#[derive(Default)]
pub struct Write;

pub fn write_provider() -> StaticToolProvider<Write> {
    StaticToolProvider::new(vec![write_tool_definition()], Write)
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// Path to the file to write (relative or absolute).
    path: String,
    /// Content to write to the file.
    content: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct WriteOutput {
    summary: String,
    path: String,
    bytes: usize,
}

#[async_trait::async_trait]
impl StaticToolExecute for Write {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        execute_typed_tool_result::<WriteArgs, _, _>(call.args, |args| async move {
            if let Err(err) = non_empty_string(&args.path, "path") {
                return err;
            }
            run_blocking(move || write_file(args)).await
        })
        .await
    }
}

fn write_tool_definition() -> ToolDefinition {
    ToolDefinition::typed::<WriteArgs, WriteOutput>("tool:write", "write", WRITE_DESCRIPTION)
        .with_examples(vec![
            r#"await files.write({ path: "hello.txt", content: "hello\n" })?"#.into(),
            r#"await files.write({ path: "src/main.rs", content: "fn main() {}\n" })?"#.into(),
        ])
        .with_lashlang_binding(lash_tool_support::lashlang_binding(
            ["files"],
            "write",
            &["write_file"],
        ))
}

fn write_file(args: WriteArgs) -> ToolOutcome {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            return io_failure(
                "current_dir_failed",
                format!("Failed to determine cwd: {err}"),
            );
        }
    };
    let absolute_path = resolve_under(&cwd, Path::new(&args.path));
    if let Some(parent) = absolute_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return io_failure(
            "create_parent_directories_failed",
            format!("Could not write file: {}. {err}.", args.path),
        );
    }
    if let Err(err) = std::fs::write(&absolute_path, &args.content) {
        return io_failure(
            "write_file_failed",
            format!("Could not write file: {}. {err}.", args.path),
        );
    }

    let display_path = display_relative(&cwd, &absolute_path);
    let bytes = args.content.len();
    lash_tool_support::typed_tool_ok(WriteOutput {
        summary: format!("Successfully wrote {bytes} bytes to {display_path}."),
        path: args.path,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn run_write(dir: &TempDir, path: &str, content: &str) -> ToolOutcome {
        let path = dir.path().join(path).to_string_lossy().to_string();
        write_file(WriteArgs {
            path,
            content: content.to_string(),
        })
    }

    #[test]
    fn write_contract_documents_pi_shape() {
        let definition = write_tool_definition();
        let rendered = definition.compact_contract().render_signature();

        assert!(rendered.contains("path"), "{rendered}");
        assert!(rendered.contains("content"), "{rendered}");
        assert!(
            definition
                .manifest()
                .description
                .contains("Creates the file if it does not exist")
        );
    }

    #[test]
    fn write_creates_parent_directories_and_file() {
        let dir = TempDir::new().unwrap();

        let result = run_write(&dir, "nested/hello.txt", "hello\n");

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/hello.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(result.value_for_projection()["bytes"], json!(6));
    }

    #[test]
    fn write_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "old\n").unwrap();

        let result = run_write(&dir, "hello.txt", "new\n");

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn write_failure_is_structured_io_with_the_path() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let result = write_file(WriteArgs {
            path: target.to_string_lossy().to_string(),
            content: "content".to_string(),
        });
        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("writing a directory must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::Io);
        assert_eq!(failure.code, "write_file_failed");
        assert!(failure.message.contains(target.to_string_lossy().as_ref()));
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }
}
