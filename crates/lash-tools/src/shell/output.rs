//! Output plumbing for the shell tools: the reader/wait threads that drain a
//! child's stream into a shared buffer, the spill-to-disk path for large
//! output, terminal-escape cleaning, token-based truncation, and the JSON
//! result-record builders shared by `exec`/`start`/`write_stdin`.
//!
//! These are pure helpers over `Arc<Mutex<..>>` buffers and `ProcessState`;
//! they hold no reference to `ShellRuntime`/`StandardShell`, which lets the
//! runtime and surface layers depend on them without a cycle.

use lash_sansio::sync::MutexExt;
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

use lash_core::{ToolFailure, ToolFailureClass, ToolOutcome, ToolValue};

pub(crate) const MAX_OUTPUT: usize = 512_000;
pub(crate) const SPILL_OUTPUT_THRESHOLD: usize = 50 * 1024;
pub(crate) const OUTPUT_QUIET_PERIOD_MS: u64 = 75;
pub(crate) const SHELL_READER_DIED: &str = "shell output reader died before EOF";

struct ReaderDeathGuard {
    reader_died: Arc<AtomicBool>,
    armed: bool,
}

#[derive(Clone)]
pub(crate) struct ReaderSignals {
    output_notify: Arc<Notify>,
    reader_died: Arc<AtomicBool>,
}

impl ReaderSignals {
    pub(crate) fn new(output_notify: Arc<Notify>, reader_died: Arc<AtomicBool>) -> Self {
        Self {
            output_notify,
            reader_died,
        }
    }
}

impl ReaderDeathGuard {
    fn new(reader_died: Arc<AtomicBool>) -> Self {
        Self {
            reader_died,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReaderDeathGuard {
    fn drop(&mut self) {
        if self.armed {
            self.reader_died.store(true, Ordering::SeqCst);
        }
    }
}

/// A snapshot of the shared handles needed to observe and steer a running
/// child without holding the process map lock.
#[derive(Clone)]
pub(crate) struct ProcessState {
    pub(crate) buffer: Arc<StdMutex<ShellOutputBuffer>>,
    pub(crate) exit_code: Arc<StdMutex<Option<i32>>>,
    pub(crate) exit_notify: Arc<Notify>,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) reader_died: Arc<AtomicBool>,
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

#[derive(Default)]
pub(crate) struct ShellOutputBuffer {
    bytes: Vec<u8>,
    start_offset: usize,
    spill: Option<ShellOutputSpill>,
    read_cursor: usize,
}

struct OutputSnapshot {
    rendered: String,
    full_output_path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum SpillFlush {
    Flush,
    KeepBuffered,
}

impl ShellOutputBuffer {
    pub(crate) fn append(&mut self, id: &str, chunk: &[u8]) {
        if self.bytes.len() + chunk.len() > SPILL_OUTPUT_THRESHOLD {
            let _ = activate_spill(id, &self.bytes, &mut self.spill);
        }
        let mut clear_spill = false;
        if let Some(spill) = self.spill.as_mut()
            && spill.file.write_all(chunk).is_err()
        {
            clear_spill = true;
        }
        if clear_spill {
            self.spill = None;
        }

        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_OUTPUT {
            let to_drop = self.bytes.len() - MAX_OUTPUT;
            self.bytes.drain(..to_drop);
            self.start_offset += to_drop;
        }
    }

    fn render_all(&mut self) -> OutputSnapshot {
        let mut rendered = String::from_utf8_lossy(&self.bytes).to_string();
        if self.start_offset != 0 {
            append_truncation_marker(&mut rendered);
        }
        if let Some(spill) = self.spill.as_mut() {
            let _ = spill.file.flush();
        }
        OutputSnapshot {
            rendered,
            full_output_path: self.spill.as_ref().map(|spill| spill.path.clone()),
        }
    }

    fn take_since(&mut self) -> OutputSnapshot {
        let end_offset = self.start_offset + self.bytes.len();
        let had_gap = self.read_cursor < self.start_offset;
        let start = self.read_cursor.max(self.start_offset);
        let relative_start = start.saturating_sub(self.start_offset);
        let mut rendered =
            String::from_utf8_lossy(self.bytes.get(relative_start..).unwrap_or_default())
                .to_string();
        self.read_cursor = end_offset;
        if !rendered.is_empty() && (had_gap || self.start_offset != 0) {
            append_truncation_marker(&mut rendered);
        }
        OutputSnapshot {
            rendered,
            full_output_path: self.spill.as_ref().map(|spill| spill.path.clone()),
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn activate_spill(&mut self, id: &str) -> Option<PathBuf> {
        activate_spill(id, &self.bytes, &mut self.spill)
    }

    pub(crate) fn take_spill(&mut self) -> Option<ShellOutputSpill> {
        self.spill.take()
    }
}

fn append_truncation_marker(rendered: &mut String) {
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push_str("[truncated]");
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
    if let Some(mut killer) = killer.lock_recover().take() {
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
        if state.exit_code.lock_recover().is_some() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::select! {
            _ = state.exit_notify.notified() => {
                if state.exit_code.lock_recover().is_some() {
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return,
        }
    }
}

pub(crate) fn render_buffer_output(
    id: &str,
    buffer: &Arc<StdMutex<ShellOutputBuffer>>,
    max_output_tokens: Option<usize>,
) -> (String, Option<usize>, Option<PathBuf>) {
    let snapshot = buffer.lock_recover().render_all();
    finalize_output(id, buffer, snapshot, max_output_tokens, SpillFlush::Flush)
}

pub(crate) fn take_buffer_output(
    id: &str,
    buffer: &Arc<StdMutex<ShellOutputBuffer>>,
    max_output_tokens: Option<usize>,
) -> (String, Option<usize>, Option<PathBuf>) {
    let snapshot = buffer.lock_recover().take_since();
    finalize_output(
        id,
        buffer,
        snapshot,
        max_output_tokens,
        SpillFlush::KeepBuffered,
    )
}

fn finalize_output(
    id: &str,
    buffer: &Arc<StdMutex<ShellOutputBuffer>>,
    snapshot: OutputSnapshot,
    max_output_tokens: Option<usize>,
    spill_flush: SpillFlush,
) -> (String, Option<usize>, Option<PathBuf>) {
    let rendered = clean_terminal_output(&snapshot.rendered);
    let (rendered, original_token_count, token_truncated) =
        truncate_exec_output(rendered, max_output_tokens);
    let mut full_output_path = snapshot.full_output_path;
    if token_truncated && full_output_path.is_none() {
        // Keep terminal cleanup and token truncation outside the buffer lock so
        // readers are not held behind string processing. Re-acquiring the lock
        // here is safe: below MAX_OUTPUT, `bytes` is the complete
        // capture, while an append that crosses that threshold activates
        // `spill` under the same lock and makes this call return its path.
        let mut buffer = buffer.lock_recover();
        full_output_path = buffer.activate_spill(id);
        if matches!(spill_flush, SpillFlush::Flush)
            && let Some(spill) = buffer.spill.as_mut()
        {
            let _ = spill.file.flush();
        }
    }
    (rendered, original_token_count, full_output_path)
}

pub(crate) async fn wait_for_buffer_settle(state: &ProcessState, quiet_period: Duration) {
    let mut last_len = state.buffer.lock_recover().len();
    let mut quiet_until = tokio::time::Instant::now() + quiet_period;

    loop {
        tokio::select! {
            _ = state.output_notify.notified() => {
                let buffer_len = state.buffer.lock_recover().len();
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
    buffer: Arc<StdMutex<ShellOutputBuffer>>,
    signals: ReaderSignals,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut death_guard = ReaderDeathGuard::new(signals.reader_died);
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.lock_recover().append(&id, &chunk[..n]);
                    signals.output_notify.notify_waiters();
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        death_guard.disarm();
        signals.output_notify.notify_waiters();
    })
}

pub(crate) fn spawn_async_reader<R>(
    id: String,
    mut reader: R,
    buffer: Arc<StdMutex<ShellOutputBuffer>>,
    signals: ReaderSignals,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut death_guard = ReaderDeathGuard::new(signals.reader_died);
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    buffer.lock_recover().append(&id, &chunk[..n]);
                    signals.output_notify.notify_waiters();
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        death_guard.disarm();
        signals.output_notify.notify_waiters();
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
        *exit_code.lock_recover() = Some(code);
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
) -> ToolOutcome {
    let record = standard_shell_io_record(
        id,
        output,
        exit_code,
        original_token_count,
        full_output_path,
        wall_time_seconds,
    );
    ToolOutcome::ok(record)
}

pub(crate) fn timed_out_shell_io_result(
    id: &str,
    output: String,
    original_token_count: Option<usize>,
    full_output_path: Option<&Path>,
    wall_time_seconds: f64,
    timeout_ms: u64,
) -> ToolOutcome {
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

fn shell_failure(code: &str, message: impl Into<String>, raw: serde_json::Value) -> ToolOutcome {
    let mut failure = ToolFailure::tool(ToolFailureClass::Execution, code, message);
    failure.raw = Some(ToolValue::from(raw));
    ToolOutcome::failure(failure)
}

#[cfg(test)]
fn shell_reader_died_result() -> ToolOutcome {
    ToolOutcome::failure(*shell_reader_died_failure())
}

pub(crate) fn shell_reader_died_failure() -> Box<ToolFailure> {
    let mut failure = ToolFailure::tool(
        ToolFailureClass::Execution,
        "shell_reader_died",
        SHELL_READER_DIED,
    );
    failure.raw = Some(ToolValue::from(json!({ "reader_died": true })));
    Box::new(failure)
}

#[cfg(test)]
mod reader_death_tests {
    use super::*;

    struct PanickingReader;

    impl Read for PanickingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("simulated reader death");
        }
    }

    #[test]
    fn reader_unwind_is_a_typed_failure_not_silent_success() {
        let reader_died = Arc::new(AtomicBool::new(false));
        let handle = spawn_reader_thread(
            "reader-death".to_string(),
            Box::new(PanickingReader),
            Arc::new(StdMutex::new(ShellOutputBuffer::default())),
            ReaderSignals::new(Arc::new(Notify::new()), Arc::clone(&reader_died)),
        );
        assert!(handle.join().is_err());
        assert!(reader_died.load(Ordering::SeqCst));

        let result = shell_reader_died_result();
        let lash_core::ToolOutcome::Done(output) = result else {
            panic!("reader death cannot defer completion");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("reader death must be typed as a failure");
        };
        assert_eq!(failure.code, "shell_reader_died");
    }
}

#[cfg(test)]
mod finalize_output_tests {
    use super::*;

    #[test]
    fn token_truncation_spill_seeds_from_current_buffer_after_snapshot() {
        let buffer = Arc::new(StdMutex::new(ShellOutputBuffer::default()));
        buffer.lock_recover().append("finalize-race", b"before");
        let snapshot = buffer.lock_recover().render_all();
        buffer.lock_recover().append("finalize-race", b"after");

        let (output, _, full_output_path) = finalize_output(
            "finalize-race",
            &buffer,
            snapshot,
            Some(1),
            SpillFlush::Flush,
        );

        assert!(output.ends_with("\n[truncated]"));
        let full_output_path = full_output_path.expect("token truncation must activate spill");
        assert_eq!(
            fs::read(&full_output_path).expect("full output file"),
            b"beforeafter"
        );
        let _ = fs::remove_file(full_output_path);
    }
}
