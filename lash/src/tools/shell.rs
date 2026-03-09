use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::Notify;

use crate::{ProgressSender, SandboxMessage, ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::require_str;

/// A running process managed by the Shell.
struct ShellProcess {
    pid: u32,
    stdin: Option<tokio::process::ChildStdin>,
    buffer: Arc<StdMutex<String>>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_notify: Arc<Notify>,
}

/// 512 KB output cap per process.
const MAX_OUTPUT: usize = 512_000;

/// Async process manager. Spawns shell processes and returns handles;
/// hidden tools interact with running processes by ID.
pub struct Shell {
    /// $SHELL or "bash" fallback.
    shell_path: String,
    /// Working directory for spawned processes.
    cwd: PathBuf,
    processes: Arc<StdMutex<HashMap<String, ShellProcess>>>,
}

impl Shell {
    pub fn new() -> Self {
        let shell_path = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            shell_path,
            cwd,
            processes: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// The shell program that will execute commands (e.g. "/bin/zsh", "bash").
    pub fn shell_name(&self) -> &str {
        self.shell_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.shell_path)
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn a background task that reads stdout and stderr into the shared buffer,
/// then waits for the child to exit and stores the exit code.
fn spawn_reader(
    mut child: Child,
    buffer: Arc<StdMutex<String>>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_notify: Arc<Notify>,
) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    tokio::spawn(async move {
        let buf_out = buffer.clone();
        let buf_err = buffer.clone();

        let out_task = tokio::spawn(async move {
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    let mut buf = buf_out.lock().unwrap();
                    if buf.len() < MAX_OUTPUT {
                        buf.push_str(&line);
                    }
                    line.clear();
                }
            }
        });

        let err_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    let mut buf = buf_err.lock().unwrap();
                    if buf.len() < MAX_OUTPUT {
                        buf.push_str(&line);
                    }
                    line.clear();
                }
            }
        });

        let _ = out_task.await;
        let _ = err_task.await;

        let status = child.wait().await;
        let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        *exit_code.lock().unwrap() = Some(code);
        exit_notify.notify_waiters();
    });
}

impl Shell {
    fn spawn_process(&self, command: &str) -> Result<(String, serde_json::Value), String> {
        let mut cmd = tokio::process::Command::new(&self.shell_path);
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Create a new process group so kill(-pgid) catches all children
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;

        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take();
        let buffer = Arc::new(StdMutex::new(String::new()));
        let exit_code = Arc::new(StdMutex::new(None));
        let exit_notify = Arc::new(Notify::new());

        spawn_reader(
            child,
            buffer.clone(),
            exit_code.clone(),
            exit_notify.clone(),
        );

        let id = uuid::Uuid::new_v4().to_string();

        let process = ShellProcess {
            pid,
            stdin,
            buffer,
            exit_code,
            exit_notify,
        };

        self.processes.lock().unwrap().insert(id.clone(), process);

        Ok((id.clone(), json!({"__handle__": "shell", "id": id})))
    }

    #[allow(clippy::type_complexity)]
    fn get_buffer_and_exit(
        &self,
        id: &str,
    ) -> Result<
        (
            Arc<StdMutex<String>>,
            Arc<StdMutex<Option<i32>>>,
            Arc<Notify>,
        ),
        String,
    > {
        let procs = self.processes.lock().unwrap();
        let proc = procs
            .get(id)
            .ok_or_else(|| format!("No process with id: {id}"))?;
        Ok((
            proc.buffer.clone(),
            proc.exit_code.clone(),
            proc.exit_notify.clone(),
        ))
    }

    /// Wait for process exit, streaming progress. Returns final output.
    async fn shell_wait(
        &self,
        id: &str,
        timeout: Option<f64>,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        let (buffer, exit_code, exit_notify) = match self.get_buffer_and_exit(id) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(json!(e)),
        };

        let deadline =
            timeout.map(|t| tokio::time::Instant::now() + std::time::Duration::from_secs_f64(t));

        let mut sent_len = 0usize;

        loop {
            let exited = exit_code.lock().unwrap().is_some();

            // Stream new output to progress sender
            if let Some(tx) = progress {
                let buf = buffer.lock().unwrap();
                if buf.len() > sent_len {
                    let new_chunk = &buf[sent_len..];
                    let _ = tx.send(SandboxMessage {
                        text: new_chunk.to_string(),
                        kind: "tool_output".into(),
                    });
                    sent_len = buf.len();
                }
            }

            if exited {
                break;
            }

            if let Some(dl) = deadline
                && tokio::time::Instant::now() >= dl
            {
                let pid = {
                    let procs = self.processes.lock().unwrap();
                    procs.get(id).map(|p| p.pid)
                };
                if let Some(pid) = pid {
                    // Kill process group then SIGKILL fallback
                    let _ = tokio::process::Command::new("kill")
                        .arg("--")
                        .arg(format!("-{pid}"))
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                    let _ = tokio::process::Command::new("kill")
                        .args(["-9", &pid.to_string()])
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                }
                let output = buffer.lock().unwrap().clone();
                self.processes.lock().unwrap().remove(id);
                let mut result = output;
                result.push_str(
                    "\n[timed out: process exceeded timeout; retry with a larger timeout or split into smaller commands]",
                );
                return ToolResult::err(json!(result));
            }

            tokio::select! {
                _ = exit_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }

        // Process exited — collect final output
        let code = exit_code.lock().unwrap().unwrap_or(-1);
        let mut output = buffer.lock().unwrap().clone();

        if output.len() > MAX_OUTPUT {
            output.truncate(MAX_OUTPUT);
            output.push_str("\n[truncated]");
        }

        if code != 0 {
            output.push_str(&format!("\n[exit code: {code}]"));
        }

        self.processes.lock().unwrap().remove(id);

        ToolResult {
            success: code == 0,
            result: json!(output),
            images: vec![],
        }
    }

    /// Drain accumulated output without waiting (non-blocking).
    fn shell_read(&self, id: &str) -> ToolResult {
        let procs = self.processes.lock().unwrap();
        let proc = match procs.get(id) {
            Some(p) => p,
            None => return ToolResult::err_fmt(format_args!("No process with id: {id}")),
        };
        let mut buf = proc.buffer.lock().unwrap();
        let output = buf.clone();
        buf.clear();
        ToolResult::ok(json!(output))
    }

    /// Write to the process's stdin.
    async fn shell_write(&self, id: &str, input: &str) -> ToolResult {
        let stdin = {
            let mut procs = self.processes.lock().unwrap();
            let proc = match procs.get_mut(id) {
                Some(p) => p,
                None => return ToolResult::err_fmt(format_args!("No process with id: {id}")),
            };
            proc.stdin.take()
        };

        if let Some(mut stdin) = stdin {
            let result = stdin.write_all(input.as_bytes()).await;
            let mut procs = self.processes.lock().unwrap();
            if let Some(proc) = procs.get_mut(id) {
                proc.stdin = Some(stdin);
            }
            match result {
                Ok(_) => ToolResult::ok(json!(null)),
                Err(e) => ToolResult::err_fmt(format_args!("Write failed: {e}")),
            }
        } else {
            ToolResult::err(json!("Process stdin not available"))
        }
    }

    /// Kill the process group (SIGTERM → wait → SIGKILL) and clean up.
    async fn shell_kill(&self, id: &str) -> ToolResult {
        let (pid, exit_notify) = {
            let procs = self.processes.lock().unwrap();
            match procs.get(id) {
                Some(p) => (p.pid, p.exit_notify.clone()),
                None => return ToolResult::err_fmt(format_args!("No process with id: {id}")),
            }
        };

        // Kill the entire process group (negative PID) so child processes die too
        let _ = tokio::process::Command::new("kill")
            .arg("--")
            .arg(format!("-{pid}"))
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        // Wait briefly for graceful exit, then SIGKILL
        let exited =
            tokio::time::timeout(std::time::Duration::from_secs(2), exit_notify.notified()).await;

        if exited.is_err() {
            let _ = tokio::process::Command::new("kill")
                .args(["-9", "--", &format!("-{pid}")])
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            let _ = tokio::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }

        self.processes.lock().unwrap().remove(id);
        ToolResult::ok(json!(null))
    }
}

#[async_trait::async_trait]
impl ToolProvider for Shell {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let shell_name = self.shell_name();
        vec![
            ToolDefinition {
                name: "shell".into(),
                description: vec![crate::ToolText::new(
                    format!(
                        "Run a command via {shell_name}. Returns a plain handle dict; use `shell_wait(id)`, `shell_read(id)`, `shell_write(id, input)`, and `shell_kill(id)` with the returned handle ID."
                    ),
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("command", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "shell_wait".into(),
                description: vec![crate::ToolText::new(
                    "Wait for a shell process to exit and return its output.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("id", "str"),
                    ToolParam::optional("timeout", "float"),
                ],
                returns: "str".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "shell_read".into(),
                description: vec![crate::ToolText::new(
                    "Read and drain accumulated output from a running shell process (non-blocking). Returns empty string if no new output has arrived yet.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("id", "str")],
                returns: "str".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "shell_write".into(),
                description: vec![crate::ToolText::new(
                    "Write input to a shell process's stdin.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("id", "str"),
                    ToolParam::typed("input", "str"),
                ],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "shell_kill".into(),
                description: vec![crate::ToolText::new(
                    "Send SIGTERM to a running shell process.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("id", "str")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        match name {
            "shell" => {
                let command = match require_str(args, "command") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                match self.spawn_process(command) {
                    Ok((_id, handle)) => ToolResult::ok(handle),
                    Err(e) => ToolResult::err_fmt(e),
                }
            }
            "shell_wait" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let timeout = args.get("timeout").and_then(|v| v.as_f64());
                self.shell_wait(id, timeout, None).await
            }
            "shell_read" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                self.shell_read(id)
            }
            "shell_write" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let input = match require_str(args, "input") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                self.shell_write(id, input).await
            }
            "shell_kill" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                self.shell_kill(id).await
            }
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        }
    }

    async fn execute_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        if name == "shell_wait" {
            let id = match require_str(args, "id") {
                Ok(s) => s,
                Err(e) => return e,
            };
            let timeout = args.get("timeout").and_then(|v| v.as_f64());
            self.shell_wait(id, timeout, progress).await
        } else {
            self.execute(name, args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;
    use serde_json::json;

    #[tokio::test]
    async fn test_spawn_and_result() {
        let shell = Shell::default();
        let result = shell
            .execute("shell", &json!({"command": "echo hello"}))
            .await;
        assert!(result.success);
        let handle: serde_json::Value = result.result;
        assert_eq!(handle["__handle__"], "shell");
        let id = handle["id"].as_str().unwrap();

        let result = shell
            .execute("shell_wait", &json!({"id": id, "timeout": 5.0}))
            .await;
        assert!(result.success);
        assert!(result.result.as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn test_nonzero_exit() {
        let shell = Shell::default();
        let handle = shell.execute("shell", &json!({"command": "exit 1"})).await;
        let id = handle.result["id"].as_str().unwrap();

        let result = shell
            .execute("shell_wait", &json!({"id": id, "timeout": 5.0}))
            .await;
        assert!(!result.success);
        assert!(result.result.as_str().unwrap().contains("exit code: 1"));
    }

    #[tokio::test]
    async fn test_timeout() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "sleep 60"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        let result = shell
            .execute("shell_wait", &json!({"id": id, "timeout": 0.2}))
            .await;
        assert!(!result.success);
        assert!(result.result.as_str().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn test_missing_command() {
        let shell = Shell::default();
        let result = shell.execute("shell", &json!({})).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_output_nonblocking() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "echo line1; sleep 10"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let result = shell.execute("shell_read", &json!({"id": id})).await;
        assert!(result.success);
        assert!(result.result.as_str().unwrap().contains("line1"));

        shell.execute("shell_kill", &json!({"id": id})).await;
    }

    #[tokio::test]
    async fn test_write_stdin() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "read line; echo got:$line"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        let write_result = shell
            .execute("shell_write", &json!({"id": id, "input": "hello\n"}))
            .await;
        assert!(write_result.success);

        let result = shell
            .execute("shell_wait", &json!({"id": id, "timeout": 5.0}))
            .await;
        assert!(result.success);
        assert!(result.result.as_str().unwrap().contains("got:hello"));
    }

    #[tokio::test]
    async fn test_kill() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "sleep 60"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        let result = shell.execute("shell_kill", &json!({"id": id})).await;
        assert!(result.success);

        let result = shell.execute("shell_wait", &json!({"id": id})).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_streaming_progress() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "echo streaming_test"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = shell
            .execute_streaming("shell_wait", &json!({"id": id, "timeout": 5.0}), Some(&tx))
            .await;
        assert!(result.success);

        let mut got_output = false;
        while let Ok(msg) = rx.try_recv() {
            if msg.text.contains("streaming_test") {
                got_output = true;
            }
        }
        assert!(got_output);
    }

    #[tokio::test]
    async fn test_definitions_count() {
        let shell = Shell::default();
        let defs = shell.definitions();
        assert_eq!(defs.len(), 5);
        assert_eq!(defs.iter().filter(|d| !d.hidden).count(), 1);
        assert_eq!(defs.iter().filter(|d| d.hidden).count(), 4);
    }

    #[tokio::test]
    async fn test_with_cwd() {
        let shell = Shell::new().with_cwd("/tmp");
        let handle = shell.execute("shell", &json!({"command": "pwd"})).await;
        let id = handle.result["id"].as_str().unwrap();
        let result = shell
            .execute("shell_wait", &json!({"id": id, "timeout": 5.0}))
            .await;
        assert!(result.success);
        assert!(result.result.as_str().unwrap().contains("/tmp"));
    }

    #[tokio::test]
    async fn test_no_timeout_waits_forever() {
        let shell = Shell::default();
        let handle = shell
            .execute("shell", &json!({"command": "echo done"}))
            .await;
        let id = handle.result["id"].as_str().unwrap();

        // No timeout — should still return once process exits
        let result = shell.execute("shell_wait", &json!({"id": id})).await;
        assert!(result.success);
        assert!(result.result.as_str().unwrap().contains("done"));
    }

    #[tokio::test]
    async fn test_shell_name() {
        let shell = Shell::new();
        let name = shell.shell_name();
        // Should be a non-empty basename like "bash", "zsh", "fish"
        assert!(!name.is_empty());
        assert!(!name.contains('/'));
    }
}
