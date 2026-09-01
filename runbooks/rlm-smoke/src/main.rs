use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use clap::Parser;
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::rlm::RlmTurnBuilderExt as _;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolCall, ToolDefinition,
    ToolDefinitionBindingExt as _, ToolOutcome, ToolProvider,
};
use lash::{LashCore, TurnEvent, TurnInput};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;

const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";
const MAX_FILE_BYTES: usize = 64 * 1024;
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Parser)]
#[command(about = "Live-model RLM smoke host with workspace-jailed tools")]
struct Args {
    #[arg(long)]
    scenario: String,
    #[arg(long)]
    scenario_dir: PathBuf,
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    artifact_dir: PathBuf,
    #[arg(long)]
    session_id: String,
    #[arg(long)]
    port: u16,
    #[arg(long)]
    trace_offset: u64,
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: String,
    #[arg(long, env = "RLM_SMOKE_SANDBOX_IMAGE", default_value = "alpine:3.22")]
    sandbox_image: String,
}

#[derive(Debug, Serialize)]
struct HostEvidence {
    scenario: String,
    dialect: String,
    requested_model: String,
    served_models: Vec<String>,
    session_id: String,
    port: u16,
    trace_offset: u64,
    tool_call_count: usize,
    code_languages: Vec<String>,
    turn_succeeded: bool,
}

#[derive(Clone)]
struct WorkspaceTools {
    root: Arc<PathBuf>,
    sandbox_image: Arc<str>,
}

impl WorkspaceTools {
    fn new(root: &Path, sandbox_image: String) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalize workspace {}", root.display()))?;
        ensure!(
            root.is_dir(),
            "workspace {} is not a directory",
            root.display()
        );
        Ok(Self {
            root: Arc::new(root),
            sandbox_image: Arc::from(sandbox_image),
        })
    }

    fn provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(StaticToolProvider::new(tool_definitions(), self.clone()))
    }

    fn relative_path(&self, value: &str) -> Result<PathBuf, String> {
        let path = Path::new(value);
        if value.is_empty() || path.is_absolute() {
            return Err("path must be a non-empty workspace-relative path".to_string());
        }
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err("path must not escape the workspace".to_string());
        }
        Ok(path.to_path_buf())
    }

    fn existing_path(&self, value: &str) -> Result<PathBuf, String> {
        let relative = self.relative_path(value)?;
        let resolved = self
            .root
            .join(relative)
            .canonicalize()
            .map_err(|error| format!("cannot resolve `{value}`: {error}"))?;
        if !resolved.starts_with(self.root.as_ref()) {
            return Err("path escapes the workspace through a symlink".to_string());
        }
        Ok(resolved)
    }

    fn writable_path(&self, value: &str) -> Result<PathBuf, String> {
        let relative = self.relative_path(value)?;
        let candidate = self.root.join(relative);
        let parent = candidate
            .parent()
            .ok_or_else(|| "path has no parent".to_string())?
            .canonicalize()
            .map_err(|error| format!("cannot resolve parent of `{value}`: {error}"))?;
        if !parent.starts_with(self.root.as_ref()) {
            return Err("path escapes the workspace through a symlink".to_string());
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&candidate)
            && metadata.file_type().is_symlink()
        {
            return Err("refusing to write through a symlink".to_string());
        }
        Ok(candidate)
    }

    fn list(&self, path: &str) -> Result<Value, String> {
        let directory = self.existing_path(path)?;
        if !directory.is_dir() {
            return Err(format!("`{path}` is not a directory"));
        }
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("cannot list `{path}`: {error}"))?
            .map(|entry| {
                let entry = entry.map_err(|error| error.to_string())?;
                let kind = entry
                    .file_type()
                    .map_err(|error| error.to_string())
                    .map(|kind| {
                        if kind.is_dir() {
                            "directory"
                        } else if kind.is_file() {
                            "file"
                        } else if kind.is_symlink() {
                            "symlink"
                        } else {
                            "other"
                        }
                    })?;
                Ok(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "kind": kind,
                }))
            })
            .collect::<Result<Vec<_>, String>>()?;
        entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        Ok(json!({ "path": path, "entries": entries }))
    }

    fn read(&self, path: &str) -> Result<Value, String> {
        let resolved = self.existing_path(path)?;
        if !resolved.is_file() {
            return Err(format!("`{path}` is not a file"));
        }
        let bytes =
            std::fs::read(&resolved).map_err(|error| format!("cannot read `{path}`: {error}"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!(
                "`{path}` exceeds the {MAX_FILE_BYTES}-byte smoke limit"
            ));
        }
        let content =
            String::from_utf8(bytes).map_err(|_| format!("`{path}` is not a UTF-8 text file"))?;
        Ok(json!({ "path": path, "content": content }))
    }

    fn write(&self, path: &str, content: &str) -> Result<Value, String> {
        if content.len() > MAX_FILE_BYTES {
            return Err(format!(
                "content exceeds the {MAX_FILE_BYTES}-byte smoke limit"
            ));
        }
        let resolved = self.writable_path(path)?;
        std::fs::write(&resolved, content)
            .map_err(|error| format!("cannot write `{path}`: {error}"))?;
        Ok(json!({ "path": path, "bytes_written": content.len() }))
    }

    async fn run(&self, command: &str) -> Result<Value, String> {
        if command != "sh test.sh" {
            return Err("the jailed exec tool permits only `sh test.sh`".to_string());
        }
        let volume = format!("{}:/workspace:rw", self.root.display());
        let output = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--pids-limit",
                "64",
                "--memory",
                "64m",
                "--volume",
            ])
            .arg(volume)
            .args(["--workdir", "/workspace", self.sandbox_image.as_ref()])
            .args(["sh", "test.sh"])
            .output()
            .await
            .map_err(|error| format!("cannot start the workspace sandbox: {error}"))?;
        Ok(json!({
            "command": command,
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

#[async_trait]
impl StaticToolExecute for WorkspaceTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let result = match call.name {
            "workspace_list" => self.list(optional_string(call.args, "path").unwrap_or(".")),
            "workspace_read" => required_string(call.args, "path").and_then(|path| self.read(path)),
            "workspace_write" => required_string(call.args, "path").and_then(|path| {
                required_string(call.args, "content").and_then(|content| self.write(path, content))
            }),
            "workspace_exec" => match required_string(call.args, "command") {
                Ok(command) => self.run(command).await,
                Err(message) => Err(message),
            },
            other => Err(format!("unknown tool `{other}`")),
        };
        match result {
            Ok(value) => ToolOutcome::ok(value),
            Err(message) => ToolOutcome::err_fmt(message),
        }
    }
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    optional_string(args, field).ok_or_else(|| format!("`{field}` must be a string"))
}

fn optional_string<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "workspace_list",
            ["files"],
            "list",
            "List one directory inside the isolated workspace. Paths are workspace-relative and cannot escape through parent components or symlinks.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "default": "." } },
                "additionalProperties": false
            }),
            json!({ "type": "object" }),
        ),
        definition(
            "workspace_read",
            ["files"],
            "read",
            "Read one UTF-8 text file inside the isolated workspace.",
            string_fields(&["path"]),
            json!({ "type": "object" }),
        ),
        definition(
            "workspace_write",
            ["files"],
            "write",
            "Create or replace one UTF-8 text file inside the isolated workspace. Supply the complete file content.",
            string_fields(&["path", "content"]),
            json!({ "type": "object" }),
        ),
        definition(
            "workspace_exec",
            ["exec"],
            "run",
            "Run the scenario test inside a networkless container whose only writable host mount is the isolated workspace. The only accepted command is `sh test.sh`.",
            string_fields(&["command"]),
            json!({ "type": "object" }),
        ),
    ]
}

fn definition<const N: usize>(
    name: &'static str,
    module: [&'static str; N],
    operation: &'static str,
    description: &'static str,
    input_schema: Value,
    output_schema: Value,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:rlm_smoke_{name}"),
        name,
        description,
        input_schema,
        output_schema,
    )
    .with_tool_binding(ToolBinding::new(module, operation))
}

fn string_fields(fields: &[&str]) -> Value {
    let properties = fields
        .iter()
        .map(|field| ((*field).to_string(), json!({ "type": "string" })))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": fields,
        "additionalProperties": false
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        bail!("OPENROUTER_API_KEY is not set; the live-model smoke row cannot run");
    }
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("create data directory {}", args.data_dir.display()))?;
    std::fs::create_dir_all(&args.artifact_dir)
        .with_context(|| format!("create artifact directory {}", args.artifact_dir.display()))?;
    let workspace = WorkspaceTools::new(&args.workspace, args.sandbox_image.clone())?;
    let dialect = lash::rlm::RlmDialect::from_env()
        .map_err(anyhow::Error::msg)?
        .unwrap_or_default();
    let prompt = std::fs::read_to_string(args.scenario_dir.join("prompt.md"))
        .context("read scenario prompt")?;
    let _port_guard = std::net::TcpListener::bind(("127.0.0.1", args.port))
        .with_context(|| format!("reserve row port {}", args.port))?;

    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    let artifact_store = Arc::new(
        lash_sqlite_store::Store::open(&args.data_dir.join("artifacts.db"))
            .await
            .context("open RLM artifact store")?,
    );
    let protocol = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(45))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        artifact_store.clone(),
    );
    let trace_path = args.artifact_dir.join("trace.jsonl");
    let mut trace_context = lash::tracing::TraceContext {
        run_id: Some(format!("trace-offset-{}", args.trace_offset)),
        ..lash::tracing::TraceContext::default()
    };
    trace_context
        .metadata
        .insert("runbook_trace_offset".to_string(), json!(args.trace_offset));
    let core = LashCore::rlm_builder(lash::TurnBudget::bounded(12), protocol)
        .no_progress_budget(lash::NoProgressBudget::bounded(4))
        .without_queued_work()
        .plugins(lash::plugins::runtime_plugin_stack())
        .provider(provider)
        .model(
            lash::ModelSpec::builder(&args.model)
                .context_window_tokens(200_000)
                .build()
                .context("build model metadata")?,
        )
        .tools(workspace.provider())
        .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            args.data_dir.join("sessions"),
        )))
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            args.data_dir.join("attachments"),
        )))
        .process_env_store(artifact_store)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .trace_jsonl_path(&trace_path)
        .trace_level(lash::tracing::TraceLevel::Extended)
        .trace_context(trace_context)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "rlm-smoke",
            args.session_id.clone(),
        ))
        .context("build RLM smoke core")?;
    let session = core
        .session(&args.session_id)
        .plugin_option(
            lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
            lash::rlm::RlmCreateExtras {
                dialect: Some(dialect),
                ..lash::rlm::RlmCreateExtras::default()
            },
        )
        .context("encode dialect session option")?
        .open()
        .await
        .context("open RLM smoke session")?;
    let output = tokio::time::timeout(
        TURN_TIMEOUT,
        session
            .turn(TurnInput::text(prompt))
            .require_finish()
            .context("require an explicit RLM finish")?
            .run(),
    )
    .await
    .context("RLM smoke turn timed out")?
    .context("run RLM smoke turn")?;
    ensure!(
        output.is_success(),
        "RLM smoke turn did not complete successfully"
    );

    let served_models = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ModelCallRecorded { record } => Some(record),
            _ => None,
        })
        .flat_map(|record| record.attempts.iter())
        .filter_map(|attempt| attempt.evidence.as_ref())
        .filter_map(|evidence| evidence.served_model.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ensure!(
        !served_models.is_empty(),
        "provider did not report a served model for this row"
    );
    let code_languages = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::CodeBlockStarted { language, .. } => Some(language.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ensure!(
        code_languages == vec![dialect.language_id().to_string()],
        "row emitted code languages {code_languages:?}, expected only `{}`",
        dialect.language_id()
    );
    core.flush_trace_sink().context("flush row trace")?;

    let evidence = HostEvidence {
        scenario: args.scenario,
        dialect: dialect.language_id().to_string(),
        requested_model: args.model,
        served_models,
        session_id: args.session_id,
        port: args.port,
        trace_offset: args.trace_offset,
        tool_call_count: output.result.tool_calls.len(),
        code_languages,
        turn_succeeded: true,
    };
    std::fs::write(
        args.artifact_dir.join("host-evidence.json"),
        serde_json::to_vec_pretty(&evidence).context("serialize host evidence")?,
    )
    .context("write host evidence")?;
    println!(
        "HOST scenario={} dialect={} served_model={} session={} trace_offset={}",
        evidence.scenario,
        evidence.dialect,
        evidence.served_models.join(","),
        evidence.session_id,
        evidence.trace_offset
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(root: &Path) -> WorkspaceTools {
        WorkspaceTools::new(root, "unused:test".to_string()).expect("workspace tools")
    }

    #[test]
    fn file_paths_cannot_escape_the_workspace() {
        let directory = tempfile::tempdir().expect("temp directory");
        let tools = tools(directory.path());
        assert!(tools.existing_path("../outside").is_err());
        assert!(tools.writable_path("/tmp/outside").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_paths_cannot_escape_through_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temp directory");
        symlink("/tmp", directory.path().join("outside")).expect("symlink");
        let tools = tools(directory.path());
        assert!(tools.existing_path("outside").is_err());
        assert!(tools.writable_path("outside/file").is_err());
    }

    #[tokio::test]
    async fn exec_refuses_every_command_except_the_fixed_test_command() {
        let directory = tempfile::tempdir().expect("temp directory");
        let tools = tools(directory.path());
        let error = tools.run("cat /etc/passwd").await.expect_err("must refuse");
        assert!(error.contains("only `sh test.sh`"));
    }
}
