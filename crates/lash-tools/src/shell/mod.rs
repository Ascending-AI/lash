//! Built-in shell tool catalog (`shell.exec` / `shell.start` /
//! `shell.write`).
//!
//! This module is the *surface* layer: tool definitions, argument parsing,
//! the [`StandardShell`] executor, prompt contributions, and the plugin
//! factory. The process-lifecycle machinery lives in [`runtime`] and the
//! output-buffer plumbing in [`output`].

mod output;
mod runtime;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use lash_core::plugin::{
    PluginError, PluginFactory, PluginSessionContext, PluginSpec, PluginSpecFactory, SessionPlugin,
};
use lash_core::runtime::ProcessEventSemanticsSpec;
use lash_core::{
    PreparedToolCall, ProcessEventType, ProcessInput, ProcessStartRequest, PromptContribution,
    SessionToolAccess, ToolCall, ToolDefinition, ToolProvider, ToolResult,
};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt, execution_failure,
    invalid_request_failure, object_schema, parse_optional_bool, parse_optional_usize_arg,
    require_str,
};

use crate::shell::output::{PollOutcome, shell_io_result, timed_out_shell_io_result};
use crate::shell::runtime::{
    CommonCommandParams, DEFAULT_EXEC_COMMAND_TIMEOUT_MS, ExecCommandParams,
    PipeExecProcessRequest, ShellRuntime, StartCommandParams,
};
use std::time::{SystemTime, UNIX_EPOCH};

const SHELL_STDIN_SIGNAL: &str = "stdin";
const SHELL_STDIN_SIGNAL_EVENT: &str = "signal.stdin";

pub fn shell_prompt_contributions() -> Vec<PromptContribution> {
    shell_prompt_contributions_for_access(&SessionToolAccess::default())
}

/// Returns the shell prompt contributions, gating the `shell.write`
/// reference on whether that tool is actually callable in the current
/// session.
pub fn shell_prompt_contributions_for_access(
    access: &SessionToolAccess,
) -> Vec<PromptContribution> {
    let mut command_execution = String::from(
        "Use `shell.exec` for one-shot commands; it returns only after the process exits and successful results include `status: \"completed\"`, `done: true`, and `exit_code`. Use `shell.start` only for interactive or intentionally long-lived processes; it returns a process handle that is visible to `processes.list` and cancellable with `processes.cancel`. On ordinal-addressed Restate tiers, `shell.start`, detached starts, `shell.write`, and `processes.cancel` return typed refusals inside atomic tool attempts; use an explicit process step.",
    );
    if tool_callable_from_authority(access, "write_stdin") {
        command_execution.push_str(" Send stdin to running shell processes with `shell.write`.");
    }
    command_execution.push_str(
        " For builds, installs, tests, migrations, service setup, and verification commands, use `shell.exec` and wait for completion before concluding.",
    );
    vec![
        PromptContribution::guidance("Command Execution", command_execution),
        PromptContribution::guidance(
            "Git Safety",
            "Avoid destructive git commands unless explicitly requested.",
        ),
    ]
}

fn tool_callable_from_authority(access: &SessionToolAccess, name: &str) -> bool {
    if access.hides(name) {
        return false;
    }
    access.tools.is_empty() || access.tools.iter().any(|tool| tool.name() == name)
}

pub struct StandardShell {
    runtime: ShellRuntime,
}

impl StandardShell {
    pub fn new() -> Self {
        Self {
            runtime: ShellRuntime::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.runtime = self.runtime.with_cwd(cwd);
        self
    }

    fn parse_common_command_params(
        &self,
        args: &serde_json::Value,
    ) -> Result<CommonCommandParams, ToolResult> {
        let cmd = require_str(args, "cmd")?.to_string();
        let workdir = self.runtime.resolve_workdir(
            args.get("workdir")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty()),
        );
        let shell_path = args
            .get("shell")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.runtime.shell_path)
            .to_string();
        let login = parse_optional_bool(args, "login", false)?;
        let max_output_tokens = parse_optional_usize_arg(args, "max_output_tokens", None, true, 1)?;

        Ok(CommonCommandParams {
            cmd,
            workdir,
            shell_path,
            login,
            max_output_tokens,
        })
    }

    fn parse_exec_command_params(
        &self,
        args: &serde_json::Value,
    ) -> Result<ExecCommandParams, ToolResult> {
        let common = self.parse_common_command_params(args)?;
        let timeout_ms = parse_optional_usize_arg(args, "timeout_ms", None, false, 1)?
            .map(|value| value as u64)
            .unwrap_or(DEFAULT_EXEC_COMMAND_TIMEOUT_MS);

        Ok(ExecCommandParams {
            cmd: common.cmd,
            workdir: common.workdir,
            shell_path: common.shell_path,
            login: common.login,
            timeout_ms,
            max_output_tokens: common.max_output_tokens,
        })
    }

    fn parse_start_command_params(
        &self,
        args: &serde_json::Value,
    ) -> Result<StartCommandParams, ToolResult> {
        let common = self.parse_common_command_params(args)?;
        let detach = parse_optional_bool(args, "detach", false)?;

        Ok(StartCommandParams {
            cmd: common.cmd,
            workdir: common.workdir,
            shell_path: common.shell_path,
            login: common.login,
            max_output_tokens: common.max_output_tokens,
            detach,
        })
    }

    async fn exec_command(
        &self,
        params: &ExecCommandParams,
        cancel: Option<CancellationToken>,
    ) -> ToolResult {
        let started = Instant::now();
        let handle_id = self.runtime.allocate_handle_id();

        match self
            .runtime
            .exec_pipe_process(PipeExecProcessRequest {
                id: &handle_id,
                command: &params.cmd,
                workdir: &params.workdir,
                login: params.login,
                shell_path: &params.shell_path,
                timeout: Some(Duration::from_millis(params.timeout_ms)),
                max_output_tokens: params.max_output_tokens,
                cancel,
            })
            .await
        {
            Ok(PollOutcome::Running {
                output,
                original_token_count,
                full_output_path,
                ..
            }) => timed_out_shell_io_result(
                &handle_id,
                output,
                original_token_count,
                full_output_path.as_deref(),
                started.elapsed().as_secs_f64(),
                params.timeout_ms,
            ),
            Ok(PollOutcome::Exited {
                output,
                original_token_count,
                exit_code,
                full_output_path,
            }) => shell_io_result(
                &handle_id,
                output,
                Some(exit_code),
                original_token_count,
                full_output_path.as_deref(),
                started.elapsed().as_secs_f64(),
            ),
            Ok(PollOutcome::Cancelled) => ToolResult::cancelled("tool call cancelled"),
            Err(failure) => ToolResult::failure(*failure),
        }
    }

    async fn start_command(
        &self,
        params: &StartCommandParams,
        context: &lash_core::ToolContext<'_>,
        cancel: Option<CancellationToken>,
    ) -> ToolResult {
        if params.detach {
            // A Detached Command is single-phase: it is launched and recorded as
            // an immediately-terminal audit fact, so it never enters the worker
            // run phase below.
            return self.detach_command_process(params, context).await;
        }
        if let Some(process_id) = context.async_process_id() {
            return self
                .run_start_command_process(process_id, params, context, cancel)
                .await;
        }
        self.register_start_command_process(params, context).await
    }

    /// Launch a Detached Command (ADR 0019): double-fork/setsid it out of the
    /// worker's process group, retain no PTY or process-map entry, register the
    /// launch as an Externally-Owned row, and complete it IMMEDIATELY with the
    /// launch identity as a `Success` value. The row is a terminal audit fact
    /// from birth — never a running claim — and lash will not track, signal, or
    /// stop the process afterward; the host/OS owns it.
    async fn detach_command_process(
        &self,
        params: &StartCommandParams,
        context: &lash_core::ToolContext<'_>,
    ) -> ToolResult {
        // Launch first: never register an audit row for a process that failed to
        // start.
        let launch = match self.runtime.spawn_detached(
            &params.cmd,
            &params.workdir,
            params.login,
            &params.shell_path,
        ) {
            Ok(launch) => launch,
            Err(failure) => return ToolResult::failure(*failure),
        };
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        let process_id = context
            .tool_call_id()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("shell:{}", self.runtime.allocate_handle_id()));
        let launch_value = json!({
            "pid": launch.pid,
            "pgid": launch.pgid,
            "command": params.cmd.clone(),
            "started_at": started_at,
        });
        let request = ProcessStartRequest::new(
            process_id.clone(),
            ProcessInput::External {
                metadata: launch_value.clone(),
            },
            // Detached commands are host/OS property from birth: lash never
            // executes or recovers the row (ADR 0019).
            lash_core::RecoveryDisposition::ExternallyOwned,
            lash_core::ProcessOriginator::host(),
        )
        .with_identity(
            lash_core::ProcessIdentity::new("shell").with_label(Some(params.cmd.clone())),
        );
        if let Err(err) = context.processes().start(request).await {
            return execution_failure("detached_process_registration_failed", err.to_string());
        }
        if let Err(err) = context
            .processes()
            .complete_external(
                &process_id,
                lash_core::ProcessAwaitOutput::Success {
                    value: launch_value.clone(),
                    control: None,
                },
            )
            .await
        {
            return execution_failure("detached_process_completion_failed", err.to_string());
        }
        let mut record = launch_value.as_object().cloned().unwrap_or_default();
        record.insert("__handle__".to_string(), json!("process"));
        record.insert("id".to_string(), json!(process_id));
        record.insert("process_id".to_string(), json!(process_id));
        record.insert("status".to_string(), json!("detached"));
        record.insert("done".to_string(), json!(true));
        record.insert("running".to_string(), json!(false));
        ToolResult::ok(serde_json::Value::Object(record))
    }

    async fn register_start_command_process(
        &self,
        params: &StartCommandParams,
        context: &lash_core::ToolContext<'_>,
    ) -> ToolResult {
        let process_id = context
            .tool_call_id()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("shell:{}", self.runtime.allocate_handle_id()));
        let args = start_command_process_args(params);
        let call = PreparedToolCall::from_parts(
            process_id.clone(),
            "tool:start_command",
            "start_command",
            args,
            None,
            serde_json::Value::Null,
        );
        let request = ProcessStartRequest::new(
            process_id.clone(),
            ProcessInput::ToolCall { call },
            // A shell.start row spawns an OS process group and its side effects are
            // not idempotent: recovery must never re-execute a started command, so
            // its contract binds at first start (ADR 0019).
            lash_core::RecoveryDisposition::OwnerBound,
            lash_core::ProcessOriginator::host(),
        )
        .with_identity(
            lash_core::ProcessIdentity::new("shell").with_label(Some(params.cmd.clone())),
        )
        .with_extra_event_types([shell_signal_event_type()]);
        match context.processes().start(request).await {
            Ok(summary) => {
                let mut handle = serde_json::to_value(summary).unwrap_or_else(|_| {
                    lash_core::RuntimeExecutionContext::process_handle_json(&process_id)
                });
                if let Some(object) = handle.as_object_mut() {
                    object.insert("status".to_string(), json!("running"));
                    object.insert("done".to_string(), json!(false));
                    object.insert("running".to_string(), json!(true));
                }
                ToolResult::ok(handle)
            }
            Err(err) => execution_failure("shell_process_registration_failed", err.to_string()),
        }
    }

    async fn run_start_command_process(
        &self,
        process_id: &str,
        params: &StartCommandParams,
        context: &lash_core::ToolContext<'_>,
        cancel: Option<CancellationToken>,
    ) -> ToolResult {
        let started = Instant::now();
        let handle_id = process_id.to_string();

        if let Err(err) = self.runtime.spawn_process(
            handle_id.clone(),
            &params.cmd,
            &params.workdir,
            params.login,
            &params.shell_path,
        ) {
            return ToolResult::failure(*err);
        }

        let signal_done = CancellationToken::new();
        let signal_forwarder =
            self.spawn_stdin_signal_forwarder(handle_id.clone(), context, signal_done.clone());
        match self
            .runtime
            .wait_until_exit_or_timeout(&handle_id, None, params.max_output_tokens, cancel)
            .await
        {
            Ok(PollOutcome::Running { .. }) => {
                signal_done.cancel();
                let _ = signal_forwarder.await;
                self.runtime.remove_process(&handle_id);
                execution_failure(
                    "unexpected_running_shell_process",
                    "background shell process returned running without a timeout",
                )
            }
            Ok(PollOutcome::Exited {
                output,
                original_token_count,
                exit_code,
                full_output_path,
            }) => {
                signal_done.cancel();
                let _ = signal_forwarder.await;
                self.runtime.remove_process(&handle_id);
                shell_io_result(
                    &handle_id,
                    output,
                    Some(exit_code),
                    original_token_count,
                    full_output_path.as_deref(),
                    started.elapsed().as_secs_f64(),
                )
            }
            Ok(PollOutcome::Cancelled) => {
                signal_done.cancel();
                let _ = signal_forwarder.await;
                self.runtime.remove_process(&handle_id);
                ToolResult::cancelled("tool call cancelled")
            }
            Err(failure) => {
                signal_done.cancel();
                let _ = signal_forwarder.await;
                self.runtime.remove_process(&handle_id);
                ToolResult::failure(*failure)
            }
        }
    }

    fn spawn_stdin_signal_forwarder(
        &self,
        process_id: String,
        context: &lash_core::ToolContext<'_>,
        done: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let runtime = self.runtime.clone();
        let events = context.process_events();
        tokio::spawn(async move {
            let mut after_sequence = 0;
            loop {
                let event = tokio::select! {
                    _ = done.cancelled() => break,
                    event = events.wait_event_after(SHELL_STDIN_SIGNAL_EVENT, after_sequence) => event,
                };
                let Ok(event) = event else {
                    break;
                };
                after_sequence = event.sequence;
                if let Some(chars) = event.payload.get("chars").and_then(|value| value.as_str()) {
                    let _ = runtime.write_stdin(&process_id, chars).await;
                }
                if event
                    .payload
                    .get("close_stdin")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    let _ = runtime.close_stdin(&process_id).await;
                }
            }
        })
    }

    async fn write_stdin_call(
        &self,
        args: &serde_json::Value,
        context: &lash_core::ToolContext<'_>,
    ) -> ToolResult {
        let process_id = match parse_process_id(args) {
            Ok(value) => value,
            Err(err) => return err,
        };
        let chars = args
            .get("chars")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let close_stdin = match parse_optional_bool(args, "close_stdin", false) {
            Ok(value) => value,
            Err(err) => return err,
        };
        match context
            .processes()
            .signal(
                &process_id,
                SHELL_STDIN_SIGNAL,
                json!({
                    "chars": chars,
                    "close_stdin": close_stdin,
                }),
            )
            .await
        {
            Ok(event) => ToolResult::ok(json!({
                "process_id": process_id,
                "status": "signalled",
                "sequence": event.sequence,
            })),
            Err(err) => execution_failure("shell_process_signal_failed", err.to_string()),
        }
    }
}

fn start_command_process_args(params: &StartCommandParams) -> serde_json::Value {
    let mut args = serde_json::Map::new();
    args.insert("cmd".to_string(), json!(params.cmd.clone()));
    args.insert(
        "workdir".to_string(),
        json!(params.workdir.to_string_lossy().to_string()),
    );
    args.insert("shell".to_string(), json!(params.shell_path.clone()));
    args.insert("login".to_string(), json!(params.login));
    if let Some(max_output_tokens) = params.max_output_tokens {
        args.insert("max_output_tokens".to_string(), json!(max_output_tokens));
    }
    serde_json::Value::Object(args)
}

fn shell_signal_event_type() -> ProcessEventType {
    ProcessEventType {
        name: SHELL_STDIN_SIGNAL_EVENT.to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }
}

impl Default for StandardShell {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the cached shell tool provider (`shell.exec` / `shell.start`).
pub fn shell_provider(shell: StandardShell) -> StaticToolProvider<StandardShell> {
    let definitions = shell.tool_definitions();
    StaticToolProvider::new(definitions, shell)
}

#[async_trait::async_trait]
impl StaticToolExecute for StandardShell {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        let cancellation_token = call.context.cancellation_token().cloned();
        self.dispatch(call.name, call.args, call.context, cancellation_token)
            .await
    }
}

impl StandardShell {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let exec_command_description = "Run a noninteractive one-shot command with stdin closed and stdout/stderr captured, then wait for it to finish. The command is executed exactly as written by the selected shell; the tool does not add strict-mode prefixes or rewrite pipelines. Completed commands always include `status: \"completed\"`, `done: true`, `running: false`, cleaned `output`, and `exit_code`. Nonzero exit codes are returned as ordinary result data; in Lashlang, `await shell.exec(...)?` does not abort just because the process exited nonzero. Inspect `exit_code` yourself when it matters. Commands time out after 600000 ms by default; set `timeout_ms` to override the hard timeout. Timed-out commands are killed and returned as a tool failure with `status: \"timed_out\"`, `timed_out: true`, and no `exit_code`. Use `shell.start` instead for interactive, TTY-dependent, or intentionally long-lived processes. ANSI/control noise is stripped from returned output. Large or truncated output may also include `full_output_path` pointing at the saved raw stream; prefer that over shell-level `head`/`tail` truncation when you need to inspect more.";
        let start_command_description = "Start an interactive or intentionally long-lived command in a PTY as a durable background process. The command is executed exactly as written by the selected shell. The result is a process handle with `__handle__: \"process\"`, `id`, `process_id`, `status: \"running\"`, `done: false`, and `running: true`; use `processes.list` to see it and `processes.cancel` to stop it. When the process exits, nonzero exit codes are returned as ordinary result data with `exit_code`; in Lashlang, `?` does not abort just because the process exited nonzero. Inspect `exit_code` yourself. Use `shell.exec` for builds, installs, tests, service setup, verification, and other commands that must complete before the next step. Set `detach: true` to launch a fully detached process that the host/OS owns: it runs in its own session, outlives this session and host, and lash will NOT track, signal, or stop it. A detached launch returns immediately with `status: \"detached\"`, `done: true`, `running: false`, and the launch identity `pid`, `pgid`, `command`, and `started_at`; there is no exit code, output, or `processes.cancel` for it — supervision is entirely your/the host's responsibility. On ordinal-addressed Restate tiers, tracked and detached starts return a typed refusal inside an atomic tool attempt; place the start in an explicit process step. Runtime-owned and key-addressed tiers are unaffected.";
        let command_common = |command_description: &str| {
            json!({
                "cmd": {
                    "type": "string",
                    "description": command_description
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional working directory to run the command in; defaults to the turn cwd."
                },
                "shell": {
                    "type": "string",
                    "description": "Shell binary to launch. Defaults to the user's default shell."
                },
                "login": {
                    "type": "boolean",
                    "default": false,
                    "description": "Whether to run the shell with -l semantics. Defaults to false to avoid startup prompts and shell init noise."
                },
                "max_output_tokens": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of tokens to return. Excess output will be truncated."
                }
            })
        };
        vec![
            ToolDefinition::raw(
                "tool:exec_command",
                "exec_command",
                exec_command_description,
                {
                    let mut properties = command_common("Shell command to execute.");
                    properties["timeout_ms"] = json!({
                        "type": "integer",
                        "minimum": 1,
                        "default": DEFAULT_EXEC_COMMAND_TIMEOUT_MS,
                        "description": "Hard timeout in milliseconds. If reached before the command exits, the process is killed and returned as a tool failure with `status: \"timed_out\"` and `timed_out: true`. Defaults to 600000 ms."
                    });
                    object_schema(properties, &["cmd"])
                },
                shell_exec_output_schema(),
            )
            .with_examples(vec![
                r#"await shell.exec({ cmd: "cargo test -p lash-protocol-rlm", timeout_ms: 600000 })?"#.into(),
                r#"probe = await shell.exec({ cmd: "test -f Cargo.lock" })?
finish probe.exit_code == 0"#.into(),
            ])
            .with_lashlang_binding(lash_tool_support::lashlang_binding(
                ["shell"],
                "exec",
                &["shell", "bash"],
            )),
            ToolDefinition::raw(
                "tool:start_command",
                "start_command",
                start_command_description,
                {
                    let mut properties = command_common("Shell command to start.");
                    properties["detach"] = json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Launch the command fully detached (its own session via setsid) so it outlives this session and host. lash records only an immediately-terminal audit fact and never tracks, signals, or stops it. Defaults to false (a tracked PTY process)."
                    });
                    object_schema(properties, &["cmd"])
                },
                shell_start_output_schema(),
            )
            .with_examples(vec![
                r#"await shell.start({ cmd: "python -m http.server 8000" })?"#.into(),
                r#"await shell.start({ cmd: "nohup ./daemon --serve", detach: true })?"#.into(),
            ])
            .with_lashlang_binding(lash_tool_support::lashlang_binding(
                ["shell"],
                "start",
                &["long_running_command", "pty"],
            )),
            ToolDefinition::raw(
                "tool:write_stdin",
                "write_stdin",
                "Send bytes to stdin for a running shell process started by `shell.start`. Use `close_stdin: true` to send EOF. This only acknowledges delivery of the signal; use process lifecycle tools to inspect or cancel the background process. On ordinal-addressed Restate tiers this returns a typed refusal inside an atomic tool attempt; signal from an explicit process step. Runtime-owned and key-addressed tiers are unaffected.",
                object_schema(
                    json!({
                        "process_id": {
                            "type": "string",
                            "description": "Process id returned by `shell.start`."
                        },
                        "chars": {
                            "type": "string",
                            "default": "",
                            "description": "Bytes to write to stdin; may be empty when only closing stdin."
                        },
                        "close_stdin": {
                            "type": "boolean",
                            "default": false,
                            "description": "Close stdin after writing to send EOF to the process."
                        }
                    }),
                    &["process_id"],
                ),
                shell_write_output_schema(),
            )
            .with_examples(vec![
                r#"await shell.write({ process_id: "call-shell-1", chars: "status\n" })?"#.into(),
                r#"await shell.write({ process_id: "call-shell-1", chars: "", close_stdin: true })?"#.into(),
            ])
            .with_lashlang_binding(lash_tool_support::lashlang_binding(
                ["shell"],
                "write",
                &["send_stdin", "poll_command"],
            )),
        ]
    }

    async fn dispatch(
        &self,
        name: &str,
        args: &serde_json::Value,
        context: &lash_core::ToolContext<'_>,
        cancel: Option<CancellationToken>,
    ) -> ToolResult {
        match name {
            "exec_command" => {
                let params = match self.parse_exec_command_params(args) {
                    Ok(params) => params,
                    Err(err) => return err,
                };
                self.exec_command(&params, cancel).await
            }
            "start_command" => {
                let params = match self.parse_start_command_params(args) {
                    Ok(params) => params,
                    Err(err) => return err,
                };
                self.start_command(&params, context, cancel).await
            }
            "write_stdin" => self.write_stdin_call(args, context).await,
            _ => invalid_request_failure("unknown_shell_tool", format!("Unknown tool: {name}")),
        }
    }
}

fn shell_exec_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "output": { "type": "string" },
            "status": { "type": "string", "enum": ["completed", "timed_out"] },
            "done": { "type": "boolean" },
            "running": { "type": "boolean" },
            "wall_time_seconds": { "type": "number", "minimum": 0 },
            "exit_code": { "type": "integer" },
            "timed_out": { "type": "boolean" },
            "error": { "type": "string" },
            "original_token_count": { "type": "integer", "minimum": 0 },
            "full_output_path": { "type": "string" }
        },
        "required": ["output", "status", "done", "running", "wall_time_seconds"],
        "additionalProperties": false
    })
}

fn shell_start_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "__handle__": { "type": "string", "enum": ["process"] },
            "id": { "type": "string" },
            "process_id": { "type": "string" },
            "status": { "type": "string", "enum": ["running", "detached"] },
            "done": { "type": "boolean" },
            "running": { "type": "boolean" },
            "pid": { "type": "integer", "minimum": 0 },
            "pgid": { "type": "integer", "minimum": 0 },
            "command": { "type": "string" },
            "started_at": { "type": "integer", "minimum": 0 }
        },
        "required": ["__handle__", "id", "process_id", "status", "done", "running"],
        "additionalProperties": false
    })
}

fn shell_write_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "process_id": { "type": "string" },
            "status": { "type": "string", "enum": ["signalled"] },
            "sequence": { "type": "integer", "minimum": 0 }
        },
        "required": ["process_id", "status", "sequence"],
        "additionalProperties": false
    })
}

fn parse_process_id(args: &serde_json::Value) -> Result<String, ToolResult> {
    require_str(args, "process_id").map(str::to_string)
}

/// PluginFactory for the built-in shell tool catalog.
///
/// Wires `StandardShell` into the active session with the access-gated
/// `shell.write` mention in the prompt contribution so the model only
/// sees that bullet when the tool is actually callable.
#[derive(Default)]
pub struct StandardShellPluginFactory;

impl StandardShellPluginFactory {
    pub fn new() -> Self {
        Self
    }
}

impl PluginFactory for StandardShellPluginFactory {
    fn id(&self) -> &'static str {
        "shell"
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        let tool_access = ctx.tool_access.clone();
        let provider = Arc::new(shell_provider(StandardShell::new())) as Arc<dyn ToolProvider>;
        PluginSpecFactory::new(
            "shell",
            Arc::new(move |_ctx| {
                let provider = Arc::clone(&provider);
                let tool_access = tool_access.clone();
                Ok(PluginSpec::new()
                    .with_tool_provider(provider)
                    .with_prompt_contributor(Arc::new(move |_ctx| {
                        let tool_access = tool_access.clone();
                        Box::pin(
                            async move { Ok(shell_prompt_contributions_for_access(&tool_access)) },
                        )
                    })))
            }),
        )
        .build(ctx)
    }
}

include!("tests.rs");
