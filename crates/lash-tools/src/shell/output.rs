//! Output plumbing for the shell tools: the reader/wait threads that drain a
//! child's stream into a shared buffer, the spill-to-disk path for large
//! output, terminal-escape cleaning, token-based truncation, and the JSON
//! result-record builders shared by `exec`/`start`/`write_stdin`.
//!
//! These are pure helpers over `Arc<Mutex<..>>` buffers and `ProcessState`;
//! they hold no reference to `ShellRuntime`/`StandardShell`, which lets the
//! runtime and surface layers depend on them without a cycle.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Notify;

use lash_core::{ToolFailure, ToolFailureClass, ToolResult, ToolValue};

pub(crate) const MAX_OUTPUT: usize = 512_000;
pub(crate) const SPILL_OUTPUT_THRESHOLD: usize = 50 * 1024;
pub(crate) const OUTPUT_QUIET_PERIOD_MS: u64 = 75;

/// A snapshot of the shared handles needed to observe and steer a running
/// child without holding the process map lock.
#[derive(Clone)]
pub(crate) struct ProcessState {
    pub(crate) buffer: Arc<StdMutex<Vec<u8>>>,
    pub(crate) exit_code: Arc<StdMutex<Option<i32>>>,
    pub(crate) exit_notify: Arc<Notify>,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) killer: Arc<StdMutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>>,
    /// PID of the direct PTY child. Because the PTY child is a session leader
    /// (portable-pty calls `setsid` in its `pre_exec`), this PID is also the
    /// leader of its process group, so SIGKILLing `-pid` reaps backgrounded
    /// descendants too.
    pub(crate) pid: Option<u32>,
}

pub(crate) struct ShellOutputSpill {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
}

pub(crate) enum PollOutcome {
    Running {
        output: String,
        original_token_count: Option<usize>,
        full_output_path: Option<PathBuf>,
    },
    Exited {
        output: String,
        original_token_count: Option<usize>,
        exit_code: i32,
        full_output_path: Option<PathBuf>,
    },
    Cancelled,
}

pub(crate) fn kill_child(state: &ProcessState) {
    kill_process_group_and_reap(state.pid, &state.killer);
}

/// SIGKILL a PTY child's whole process group and then its direct child.
///
/// The PTY child is its own session/process-group leader (portable-pty runs
/// `setsid` in `pre_exec`), so we SIGKILL the whole group first to reap any
/// backgrounded descendants, mirroring the pipe path's `terminate_pipe_process`.
/// The portable-pty killer only signals the direct child, so we still invoke it
/// as a fallback (and on non-unix where group kill is unavailable). The child is
/// reaped by its detached wait thread ([`spawn_wait_thread`]) once the signal
/// lands. Shared by the in-run cancel/timeout path and the [`ShellRuntime`]
/// teardown RAII kill (`crate::shell::runtime`).
pub(crate) fn kill_process_group_and_reap(
    pid: Option<u32>,
    killer: &Arc<StdMutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>>,
) {
    terminate_process_group(pid);
    if let Some(mut killer) = killer.lock().unwrap().take() {
        let _ = killer.kill();
    }
}

#[cfg(unix)]
fn terminate_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let pgid = -(pid as i32);
    unsafe {
        if libc::kill(pgid, libc::SIGKILL) == -1 {
            let _ = libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: Option<u32>) {}

#[cfg(unix)]
pub(crate) fn terminate_pipe_process(pid: Option<u32>) {
    terminate_process_group(pid);
}

#[cfg(not(unix))]
pub(crate) fn terminate_pipe_process(_pid: Option<u32>) {}

pub(crate) fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

pub(crate) async fn wait_for_child_exit(state: &ProcessState, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if state.exit_code.lock().unwrap().is_some() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::select! {
            _ = state.exit_notify.notified() => {
                if state.exit_code.lock().unwrap().is_some() {
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return,
        }
    }
}

pub(crate) fn render_buffer_output(
    id: &str,
    buffer: &Arc<StdMutex<Vec<u8>>>,
    buffer_start: &Arc<StdMutex<usize>>,
    truncated: &AtomicBool,
    spill: &Arc<StdMutex<Option<ShellOutputSpill>>>,
    max_output_tokens: Option<usize>,
) -> (String, Option<usize>, Option<PathBuf>) {
    let buf = buffer.lock().unwrap();
    let start_offset = *buffer_start.lock().unwrap();
    let mut rendered = String::from_utf8_lossy(&buf).to_string();
    if truncated.load(Ordering::SeqCst) || start_offset > 0 {
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str("[truncated]");
    }
    let rendered = clean_terminal_output(&rendered);
    let (rendered, original_token_count, token_truncated) =
        truncate_exec_output(rendered, max_output_tokens);
    let mut spill_guard = spill.lock().unwrap();
    let mut full_output_path = spill_guard.as_ref().map(|spill| spill.path.clone());
    if token_truncated && full_output_path.is_none() {
        full_output_path = activate_spill(id, &buf, &mut spill_guard);
    }
    if let Some(spill) = spill_guard.as_mut() {
        let _ = spill.file.flush();
    }
    (rendered, original_token_count, full_output_path)
}

pub(crate) async fn wait_for_buffer_settle(state: &ProcessState, quiet_period: Duration) {
    let mut last_len = state.buffer.lock().unwrap().len();
    let mut quiet_until = tokio::time::Instant::now() + quiet_period;

    loop {
        tokio::select! {
            _ = state.output_notify.notified() => {
                let buffer_len = state.buffer.lock().unwrap().len();
                if buffer_len != last_len {
                    last_len = buffer_len;
                    quiet_until = tokio::time::Instant::now() + quiet_period;
                }
            }
            _ = tokio::time::sleep_until(quiet_until) => break,
        }
    }
}

pub(crate) fn spawn_reader_thread(
    id: String,
    mut reader: Box<dyn Read + Send>,
    buffer: Arc<StdMutex<Vec<u8>>>,
    buffer_start: Arc<StdMutex<usize>>,
    truncated: Arc<AtomicBool>,
    spill: Arc<StdMutex<Option<ShellOutputSpill>>>,
    output_notify: Arc<Notify>,
) {
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut buf = buffer.lock().unwrap();
                        let mut spill = spill.lock().unwrap();
                        if buf.len() + n > SPILL_OUTPUT_THRESHOLD {
                            let _ = activate_spill(&id, &buf, &mut spill);
                        }
                        let mut clear_spill = false;
                        if let Some(spill_file) = spill.as_mut()
                            && spill_file.file.write_all(&chunk[..n]).is_err()
                        {
                            clear_spill = true;
                        }
                        if clear_spill {
                            *spill = None;
                        }

                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > MAX_OUTPUT {
                            let to_drop = buf.len() - MAX_OUTPUT;
                            buf.drain(..to_drop);
                            *buffer_start.lock().unwrap() += to_drop;
                            truncated.store(true, Ordering::SeqCst);
                        }
                    }
                    output_notify.notify_waiters();
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        output_notify.notify_waiters();
    });
}

pub(crate) fn spawn_async_reader<R>(
    id: String,
    mut reader: R,
    buffer: Arc<StdMutex<Vec<u8>>>,
    buffer_start: Arc<StdMutex<usize>>,
    truncated: Arc<AtomicBool>,
    spill: Arc<StdMutex<Option<ShellOutputSpill>>>,
    output_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut buf = buffer.lock().unwrap();
                        let mut spill = spill.lock().unwrap();
                        if buf.len() + n > SPILL_OUTPUT_THRESHOLD {
                            let _ = activate_spill(&id, &buf, &mut spill);
                        }
                        let mut clear_spill = false;
                        if let Some(spill_file) = spill.as_mut()
                            && spill_file.file.write_all(&chunk[..n]).is_err()
                        {
                            clear_spill = true;
                        }
                        if clear_spill {
                            *spill = None;
                        }

                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > MAX_OUTPUT {
                            let to_drop = buf.len() - MAX_OUTPUT;
                            buf.drain(..to_drop);
                            *buffer_start.lock().unwrap() += to_drop;
                            truncated.store(true, Ordering::SeqCst);
                        }
                    }
                    output_notify.notify_waiters();
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        output_notify.notify_waiters();
    })
}

pub(crate) fn spawn_wait_thread(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_notify: Arc<Notify>,
    output_notify: Arc<Notify>,
) {
    thread::spawn(move || {
        let code = child
            .wait()
            .map(|status| i32::try_from(status.exit_code()).unwrap_or(i32::MAX))
            .unwrap_or(-1);
        *exit_code.lock().unwrap() = Some(code);
        exit_notify.notify_waiters();
        output_notify.notify_waiters();
    });
}

fn shell_output_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("lash-tool-output");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn shell_output_path(id: &str) -> std::io::Result<PathBuf> {
    let dir = shell_output_dir()?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(dir.join(format!("exec_command-{id}-{nonce}.log")))
}

/// Create the spill file owner-readable/writable only (0600). The captured
/// stream can contain command output the agent never meant to share with other
/// users on the host, so we avoid the default world-readable 0644.
///
/// Reaping gap: the spill path is returned to the caller as `full_output_path`
/// so the agent can read the full stream after the tool call finishes. There is
/// therefore no in-process lifecycle point at which the file is both done-with
/// and safe to delete (`ShellRuntime::remove_process` fires while the path is
/// still being handed back). These temp files are left in
/// `${TMPDIR}/lash-tool-output` for OS-level temp cleanup to reclaim; 0600
/// keeps them from leaking to other local users in the meantime.
fn create_spill_file(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::create(path)
    }
}

pub(crate) fn activate_spill(
    id: &str,
    existing_output: &[u8],
    spill: &mut Option<ShellOutputSpill>,
) -> Option<PathBuf> {
    if let Some(spill) = spill.as_ref() {
        return Some(spill.path.clone());
    }

    let path = shell_output_path(id).ok()?;
    let mut file = create_spill_file(&path).ok()?;
    if file.write_all(existing_output).is_err() {
        let _ = fs::remove_file(&path);
        return None;
    }
    *spill = Some(ShellOutputSpill {
        path: path.clone(),
        file,
    });
    Some(path)
}

pub(crate) fn clean_terminal_output(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut previous_was_escape = false;
                    for next in chars.by_ref() {
                        if next == '\x07' || (previous_was_escape && next == '\\') {
                            break;
                        }
                        previous_was_escape = next == '\x1b';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        match ch {
            '\r' => {
                if !matches!(chars.peek(), Some('\n')) {
                    out.push('\n');
                }
            }
            '\x08' => {
                out.pop();
            }
            ch if ch.is_control() && ch != '\n' && ch != '\t' => {}
            ch => out.push(ch),
        }
    }
    out
}

pub(crate) fn truncate_exec_output(
    output: String,
    max_output_tokens: Option<usize>,
) -> (String, Option<usize>, bool) {
    let original_token_count = max_output_tokens.map(|_| estimate_token_count(&output));
    let Some(limit) = max_output_tokens else {
        return (output, original_token_count, false);
    };
    let max_chars = limit.saturating_mul(4);
    let char_count = output.chars().count();
    if char_count <= max_chars {
        return (output, original_token_count, false);
    }
    let truncated = output.chars().take(max_chars).collect::<String>() + "\n[truncated]";
    (truncated, original_token_count, true)
}

fn estimate_token_count(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

pub(crate) fn standard_shell_io_record(
    id: &str,
    output: String,
    exit_code: Option<i32>,
    original_token_count: Option<usize>,
    full_output_path: Option<&Path>,
    wall_time_seconds: f64,
) -> serde_json::Value {
    let running = exit_code.is_none();
    let status = if running { "running" } else { "completed" };
    let session_id = exit_code
        .is_none()
        .then(|| id.parse::<i64>().ok())
        .flatten();
    let mut record = serde_json::Map::new();
    record.insert("output".into(), json!(output));
    record.insert("status".into(), json!(status));
    record.insert("done".into(), json!(!running));
    record.insert("running".into(), json!(running));
    record.insert("wall_time_seconds".into(), json!(wall_time_seconds));
    if let Some(exit_code) = exit_code {
        record.insert("exit_code".into(), json!(exit_code));
    }
    if let Some(session_id) = session_id {
        record.insert("session_id".into(), json!(session_id));
    }
    if let Some(original_token_count) = original_token_count {
        record.insert("original_token_count".into(), json!(original_token_count));
    }
    if let Some(path) = full_output_path {
        record.insert(
            "full_output_path".into(),
            json!(path.to_string_lossy().to_string()),
        );
    }
    serde_json::Value::Object(record)
}

pub(crate) fn shell_io_result(
    id: &str,
    output: String,
    exit_code: Option<i32>,
    original_token_count: Option<usize>,
    full_output_path: Option<&Path>,
    wall_time_seconds: f64,
) -> ToolResult {
    let record = standard_shell_io_record(
        id,
        output,
        exit_code,
        original_token_count,
        full_output_path,
        wall_time_seconds,
    );
    ToolResult::ok(record)
}

pub(crate) fn timed_out_shell_io_result(
    id: &str,
    output: String,
    original_token_count: Option<usize>,
    full_output_path: Option<&Path>,
    wall_time_seconds: f64,
    timeout_ms: u64,
) -> ToolResult {
    let mut record = standard_shell_io_record(
        id,
        output,
        None,
        original_token_count,
        full_output_path,
        wall_time_seconds,
    );
    if let Some(object) = record.as_object_mut() {
        object.insert("status".into(), json!("timed_out"));
        object.insert("done".into(), json!(true));
        object.insert("running".into(), json!(false));
        object.remove("session_id");
        object.insert("timed_out".into(), json!(true));
        object.insert(
            "error".into(),
            json!(format!("Command timed out after {timeout_ms} ms")),
        );
    }
    shell_failure(
        "shell_timeout",
        format!("Command timed out after {timeout_ms} ms"),
        record,
    )
}

fn shell_failure(code: &str, message: impl Into<String>, raw: serde_json::Value) -> ToolResult {
    let mut failure = ToolFailure::tool(ToolFailureClass::Execution, code, message);
    failure.raw = Some(ToolValue::from(raw));
    ToolResult::failure(failure)
}
