use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::embedded::{PythonRequest, PythonResponse, PythonRuntime};
use crate::{AgentCapabilities, SandboxMessage, ToolCallRecord, ToolImage, ToolProvider};

const REPL_SNAPSHOT_VERSION: u32 = 2;

/// A prompt from the agent asking the user a question.
/// The `response_tx` travels all the way to the TUI, which sends the answer
/// directly back to the REPL bridge thread.
pub struct UserPrompt {
    pub question: String,
    pub options: Vec<String>,
    pub response_tx: std::sync::mpsc::Sender<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("repl execution mode is not available in this build or session")]
    ReplUnavailable,
    #[error("repl runtime exited unexpectedly")]
    RuntimeExited,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("legacy REPL snapshots are no longer supported")]
    LegacySnapshot,
}

enum SessionBackend {
    NativeTools,
    Python(PythonRuntime),
}

pub struct Session {
    backend: SessionBackend,
    tools: Arc<dyn ToolProvider>,
    tool_calls: Vec<ToolCallRecord>,
    tool_images: Vec<ToolImage>,
    message_tx: Option<UnboundedSender<SandboxMessage>>,
    prompt_tx: Option<UnboundedSender<UserPrompt>>,
    final_response: Option<String>,
    scratch_dir: tempfile::TempDir,
}

impl Session {
    pub async fn new(
        tools: Arc<dyn ToolProvider>,
        agent_id: &str,
        headless: bool,
        capabilities: AgentCapabilities,
        execution_mode: crate::ExecutionMode,
    ) -> Result<Self, SessionError> {
        let scratch_dir = tempfile::TempDir::new()?;

        let mut session = Self {
            backend: SessionBackend::NativeTools,
            tools,
            tool_calls: Vec::new(),
            tool_images: Vec::new(),
            message_tx: None,
            prompt_tx: None,
            final_response: None,
            scratch_dir,
        };

        if matches!(execution_mode, crate::ExecutionMode::Repl) {
            let runtime = PythonRuntime::start()?;
            session.backend = SessionBackend::Python(runtime);

            // Send init with tool definitions and agent identity
            let defs: Vec<_> = session
                .tools
                .definitions()
                .into_iter()
                .map(|def| def.project(crate::ExecutionMode::Repl))
                .collect();
            let tools_json = serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string());
            let capabilities_json = serde_json::json!({
                "enabled_capabilities": capabilities
                    .enabled_capabilities
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>(),
                "enabled_tools": capabilities.enabled_tools.iter().collect::<Vec<_>>(),
            })
            .to_string();
            session.runtime()?.send(PythonRequest::Init {
                tools_json,
                agent_id: agent_id.to_string(),
                headless,
                capabilities_json,
            })?;

            // Wait for ready
            match session.runtime()?.recv()? {
                PythonResponse::Ready => {}
                other => {
                    return Err(SessionError::Protocol(format!(
                        "expected ready, got: {:?}",
                        std::mem::discriminant(&other)
                    )));
                }
            }
        }

        Ok(session)
    }

    fn runtime(&self) -> Result<&PythonRuntime, SessionError> {
        match &self.backend {
            SessionBackend::NativeTools => Err(SessionError::ReplUnavailable),
            SessionBackend::Python(runtime) => Ok(runtime),
        }
    }

    pub fn supports_repl(&self) -> bool {
        matches!(self.backend, SessionBackend::Python(_))
    }

    /// Set the message sender for streaming messages during execution.
    pub fn set_message_sender(&mut self, tx: UnboundedSender<SandboxMessage>) {
        self.message_tx = Some(tx);
    }

    /// Clear the message sender (drops the sender, causing receivers to terminate).
    pub fn clear_message_sender(&mut self) {
        self.message_tx = None;
    }

    /// Set the prompt sender for forwarding user prompts during execution.
    pub fn set_prompt_sender(&mut self, tx: UnboundedSender<UserPrompt>) {
        self.prompt_tx = Some(tx);
    }

    /// Clear the prompt sender.
    pub fn clear_prompt_sender(&mut self) {
        self.prompt_tx = None;
    }

    /// Execute code in the persistent Monty REPL.
    pub async fn run_code(&mut self, code: &str) -> Result<ExecResponse, SessionError> {
        self.tool_calls.clear();
        self.tool_images.clear();
        self.final_response = None;
        let start = std::time::Instant::now();
        let id = uuid::Uuid::new_v4().to_string();

        // Strip markdown separator lines (all dashes + whitespace) the LLM
        // sometimes emits between code blocks.
        let clean_code: String = code
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                trimmed.is_empty()
                    || trimmed
                        .trim_matches('-')
                        .chars()
                        .any(|c| !c.is_whitespace())
            })
            .collect::<Vec<_>>()
            .join("\n");

        self.runtime()?.send(PythonRequest::Exec {
            id: id.clone(),
            code: clean_code,
        })?;

        // Read messages until we get exec_result.
        // Tool calls are spawned as concurrent tokio tasks so that Monty's
        // asyncio.gather() can dispatch multiple tools in parallel.
        let mut tool_handles: Vec<tokio::task::JoinHandle<(ToolCallRecord, Vec<ToolImage>)>> =
            Vec::new();

        // Use block_in_place so tokio knows this thread is blocked and can
        // schedule drain tasks (prompt forwarding, message forwarding) on other threads.
        loop {
            let runtime = self.runtime()?;
            let response = tokio::task::block_in_place(|| runtime.recv())
                .map_err(|_| SessionError::RuntimeExited)?;
            match response {
                PythonResponse::ToolCall {
                    id: _call_id,
                    name,
                    args,
                    result_tx,
                } => {
                    let tc_num = tool_handles.len();
                    tracing::info!(
                        "PARALLEL: ToolCall #{tc_num} '{name}' received at t+{:.3}s",
                        start.elapsed().as_secs_f64()
                    );
                    let tools = Arc::clone(&self.tools);
                    let parsed_args: serde_json::Value =
                        serde_json::from_str(&args).unwrap_or(json!({}));
                    let msg_tx = self.message_tx.clone();
                    let run_start = start;

                    let handle = tokio::spawn(async move {
                        tracing::info!(
                            "PARALLEL: task #{tc_num} '{name}' executing at t+{:.3}s",
                            run_start.elapsed().as_secs_f64()
                        );
                        let tool_start = std::time::Instant::now();
                        let mut result = tools
                            .execute_streaming(&name, &parsed_args, msg_tx.as_ref())
                            .await;

                        // Send result back to Python via the oneshot channel
                        let result_json = json!({
                            "success": result.success,
                            "result": serde_json::to_string(&result.result)
                                .unwrap_or_else(|_| "null".to_string()),
                        });
                        let _ = result_tx.send(result_json.to_string());
                        tracing::info!(
                            "PARALLEL: task #{tc_num} '{name}' done at t+{:.3}s",
                            run_start.elapsed().as_secs_f64()
                        );

                        let images = std::mem::take(&mut result.images);
                        let record = ToolCallRecord {
                            tool: name,
                            args: parsed_args,
                            result: result.result,
                            success: result.success,
                            duration_ms: tool_start.elapsed().as_millis() as u64,
                        };
                        (record, images)
                    });
                    tool_handles.push(handle);
                }
                PythonResponse::Message { text, kind } => {
                    if kind == "final" {
                        self.final_response = Some(text.clone());
                    } else if let Some(tx) = &self.message_tx {
                        let _ = tx.send(SandboxMessage { text, kind });
                    }
                }
                PythonResponse::ExecResult {
                    id: _,
                    output,
                    error,
                } => {
                    tracing::info!(
                        "PARALLEL: ExecResult received at t+{:.3}s ({} handles)",
                        start.elapsed().as_secs_f64(),
                        tool_handles.len()
                    );
                    // Collect results from all concurrent tool calls.
                    // By the time Python sends ExecResult, all tool futures have
                    // resolved (asyncio.gather waits), so these awaits are instant.
                    for handle in tool_handles {
                        match handle.await {
                            Ok((record, images)) => {
                                self.tool_calls.push(record);
                                self.tool_images.extend(images);
                            }
                            Err(e) => {
                                self.tool_calls.push(ToolCallRecord {
                                    tool: "unknown".into(),
                                    args: json!({}),
                                    result: json!({"error": format!("task panicked: {e}")}),
                                    success: false,
                                    duration_ms: 0,
                                });
                            }
                        }
                    }

                    let response = self.final_response.clone().unwrap_or_default();
                    return Ok(ExecResponse {
                        output,
                        response,
                        tool_calls: self.tool_calls.clone(),
                        images: std::mem::take(&mut self.tool_images),
                        error,
                        duration_ms: start.elapsed().as_millis() as u64,
                    });
                }
                PythonResponse::Ready => {
                    // Unexpected but harmless
                }
                PythonResponse::AskUser {
                    question,
                    options,
                    result_tx,
                } => {
                    if let Some(tx) = &self.prompt_tx {
                        let _ = tx.send(UserPrompt {
                            question,
                            options,
                            response_tx: result_tx,
                        });
                    } else {
                        // No prompt handler — unblock Python with empty string
                        let _ = result_tx.send(String::new());
                    }
                }
                PythonResponse::SnapshotResult { .. }
                | PythonResponse::ResetResult { .. }
                | PythonResponse::ReconfigureResult { .. }
                | PythonResponse::CheckCompleteResult { .. } => {
                    return Err(SessionError::Protocol(
                        "unexpected response during exec".to_string(),
                    ));
                }
            }
        }
    }

    /// Check if a code string is syntactically complete for the Monty REPL.
    pub fn check_complete(&self, code: &str) -> Result<bool, SessionError> {
        self.runtime()?.send(PythonRequest::CheckComplete {
            code: code.to_string(),
        })?;
        let runtime = self.runtime()?;
        let response = tokio::task::block_in_place(|| runtime.recv())
            .map_err(|_| SessionError::RuntimeExited)?;
        match response {
            PythonResponse::CheckCompleteResult { is_complete } => Ok(is_complete),
            _ => Ok(false),
        }
    }

    /// Reset the Monty REPL namespace and re-register tools.
    pub async fn reset(&mut self) -> Result<(), SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.runtime()?
            .send(PythonRequest::Reset { id: id.clone() })?;

        loop {
            match self.runtime()?.recv()? {
                PythonResponse::ResetResult { .. } => break,
                _ => continue,
            }
        }

        Ok(())
    }

    /// Re-register the current tool definitions and capability payload in the live REPL.
    /// This is intended for turn-boundary runtime reconfiguration.
    pub async fn reconfigure(
        &mut self,
        capabilities_json: String,
        generation: u64,
    ) -> Result<(), SessionError> {
        let defs: Vec<_> = self
            .tools
            .definitions()
            .into_iter()
            .map(|def| def.project(crate::ExecutionMode::Repl))
            .collect();
        let tools_json = serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string());
        self.runtime()?.send(PythonRequest::Reconfigure {
            tools_json,
            capabilities_json,
            generation,
        })?;

        loop {
            match self.runtime()?.recv()? {
                PythonResponse::ReconfigureResult {
                    generation: got_generation,
                    error,
                } => {
                    if got_generation != generation {
                        return Err(SessionError::Protocol(format!(
                            "reconfigure generation mismatch: expected {generation}, got {got_generation}"
                        )));
                    }
                    if let Some(err) = error {
                        return Err(SessionError::Protocol(format!("reconfigure failed: {err}")));
                    }
                    break;
                }
                _ => continue,
            }
        }

        Ok(())
    }

    /// Snapshot the session: REPL state + scratch filesystem.
    pub async fn snapshot(&mut self) -> Result<Vec<u8>, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.runtime()?
            .send(PythonRequest::Snapshot { id: id.clone() })?;

        let data = loop {
            match self.runtime()?.recv()? {
                PythonResponse::SnapshotResult { id: _, data } => break data,
                _ => continue,
            }
        };

        // Collect scratch files
        let files = collect_files(self.scratch_dir.path()).unwrap_or_default();

        let combined = json!({
            "version": REPL_SNAPSHOT_VERSION,
            "engine": "monty",
            "vars": data,
            "files": files,
        });
        Ok(serde_json::to_vec(&combined).unwrap())
    }

    /// Restore a session from a snapshot.
    pub async fn restore(&mut self, data: &[u8]) -> Result<(), SessionError> {
        let parsed: serde_json::Value = serde_json::from_slice(data).unwrap_or(json!({}));

        if parsed.get("version").is_none() || parsed.get("engine").is_none() {
            return Err(SessionError::LegacySnapshot);
        }
        if parsed.get("version").and_then(|v| v.as_u64()) != Some(REPL_SNAPSHOT_VERSION as u64) {
            return Err(SessionError::Protocol(
                "unsupported REPL snapshot version".to_string(),
            ));
        }
        if parsed.get("engine").and_then(|v| v.as_str()) != Some("monty") {
            return Err(SessionError::Protocol(
                "unsupported REPL snapshot engine".to_string(),
            ));
        }

        // `vars` is an opaque Monty snapshot string from the executor thread.
        let vars_str = parsed
            .get("vars")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let id = uuid::Uuid::new_v4().to_string();
        self.runtime()?
            .send(PythonRequest::Restore { id, data: vars_str })?;

        // Wait for acknowledgment (exec_result with optional restore error)
        loop {
            match self.runtime()?.recv()? {
                PythonResponse::ExecResult { error, .. } => {
                    if let Some(err) = error {
                        return Err(SessionError::Protocol(format!(
                            "executor restore failed: {err}"
                        )));
                    }
                    break;
                }
                _ => continue,
            }
        }

        // Restore scratch files
        if let Some(files_val) = parsed.get("files")
            && let Ok(files) = serde_json::from_value::<HashMap<String, String>>(files_val.clone())
        {
            // Clear existing scratch contents
            if let Ok(entries) = std::fs::read_dir(self.scratch_dir.path()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            let _ = restore_files(self.scratch_dir.path(), &files);
        }

        Ok(())
    }

    /// Access the tool provider.
    pub fn tools(&self) -> &Arc<dyn ToolProvider> {
        &self.tools
    }
}

#[derive(Clone, Debug)]
pub struct ExecResponse {
    /// Captured stdout — model's own context (print, auto-printed expressions).
    pub output: String,
    /// User-facing final response from done().
    pub response: String,
    pub tool_calls: Vec<ToolCallRecord>,
    /// Images returned by tools during this execution (e.g. read_file on a PNG).
    pub images: Vec<ToolImage>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Walk a directory recursively and collect all files as relative_path -> contents.
fn collect_files(root: &Path) -> std::io::Result<HashMap<String, String>> {
    let mut files = HashMap::new();
    walk_dir(root, root, &mut files)?;
    Ok(files)
}

fn walk_dir(root: &Path, dir: &Path, files: &mut HashMap<String, String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(root, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            // Best-effort: skip binary files that aren't valid UTF-8
            if let Ok(content) = std::fs::read_to_string(&path) {
                files.insert(rel, content);
            }
        }
    }
    Ok(())
}

/// Recreate files in a directory from a path -> content map.
fn restore_files(root: &Path, files: &HashMap<String, String>) -> std::io::Result<()> {
    for (rel_path, content) in files {
        let full = root.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, content)?;
    }
    Ok(())
}
