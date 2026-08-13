//! Process lifecycle for the shell tools: `ShellRuntime` owns the map of
//! live PTY/pipe child processes, spawns them, drives the poll loops that
//! wait for exit-or-timeout, and feeds incremental output to the surface
//! layer. The output-buffer plumbing it relies on lives in
//! [`crate::shell::output`].

use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::process::Command as TokioCommand;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use lash_core::{ToolFailure, ToolFailureClass};

use crate::shell::output::{
    OUTPUT_QUIET_PERIOD_MS, PollOutcome, ProcessState, ReaderSignals, ShellOutputSpill,
    activate_spill, clean_terminal_output, exit_status_code, kill_child,
    kill_process_group_and_reap, render_buffer_output, shell_reader_died_failure,
    spawn_async_reader, spawn_reader_thread, spawn_wait_thread, terminate_pipe_process,
    truncate_exec_output, wait_for_buffer_settle, wait_for_child_exit,
};

pub(crate) const DEFAULT_EXEC_COMMAND_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const DEFAULT_PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

type ShellResult<T> = Result<T, Box<ToolFailure>>;

fn shell_invalid_request(code: &'static str, message: impl Into<String>) -> Box<ToolFailure> {
    Box::new(ToolFailure::invalid_request(code, message))
}

fn shell_io_failure(code: &'static str, message: impl Into<String>) -> Box<ToolFailure> {
    Box::new(ToolFailure::io(code, message))
}

fn shell_execution_failure(code: &'static str, message: impl Into<String>) -> Box<ToolFailure> {
    Box::new(ToolFailure::tool(
        ToolFailureClass::Execution,
        code,
        message,
    ))
}

struct ShellProcess {
    _master: Box<dyn MasterPty + Send>,
    writer: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
    buffer: Arc<StdMutex<Vec<u8>>>,
    buffer_start: Arc<StdMutex<usize>>,
    truncated: Arc<AtomicBool>,
    read_cursor: Arc<StdMutex<usize>>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_notify: Arc<Notify>,
    output_notify: Arc<Notify>,
    reader_died: Arc<AtomicBool>,
    spill: Arc<StdMutex<Option<ShellOutputSpill>>>,
    killer: Arc<StdMutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    pid: Option<u32>,
}

pub(crate) struct PipeExecProcessRequest<'a> {
    pub(crate) id: &'a str,
    pub(crate) command: &'a str,
    pub(crate) workdir: &'a Path,
    pub(crate) login: bool,
    pub(crate) shell_path: &'a str,
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_output_tokens: Option<usize>,
    pub(crate) cancel: Option<CancellationToken>,
}

#[derive(Clone, Debug)]
pub(crate) struct CommonCommandParams {
    pub(crate) cmd: String,
    pub(crate) workdir: PathBuf,
    pub(crate) shell_path: String,
    pub(crate) login: bool,
    pub(crate) max_output_tokens: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecCommandParams {
    pub(crate) cmd: String,
    pub(crate) workdir: PathBuf,
    pub(crate) shell_path: String,
    pub(crate) login: bool,
    pub(crate) timeout_ms: u64,
    pub(crate) max_output_tokens: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct StartCommandParams {
    pub(crate) cmd: String,
    pub(crate) workdir: PathBuf,
    pub(crate) shell_path: String,
    pub(crate) login: bool,
    pub(crate) max_output_tokens: Option<usize>,
    /// Launch the command as a Detached Command (ADR 0019): its own session,
    /// no PTY retained, host/OS-owned from birth. lash records only an
    /// immediately-terminal audit fact and never tracks it as running.
    pub(crate) detach: bool,
    /// Stable id of the ExternallyOwned audit row produced by the detached
    /// launcher body. Present exactly when `detach` is true.
    pub(crate) detached_process_id: Option<String>,
}

/// Identity of a launched [Detached Command](StartCommandParams::detach) — the
/// only fact lash retains about it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DetachedLaunch {
    pub(crate) pid: u32,
    pub(crate) pgid: u32,
}

/// Owner of the live PTY/pipe process map with deterministic teardown.
///
/// This is the tool-layer RAII / self-fencing seam (ADR 0019): the shell
/// runtime kills its own process groups on teardown. When the last
/// [`ShellRuntime`] clone drops — session close, run end, or the worker's
/// runtime being torn down after a lease loss — this table drops with it and
/// SIGKILLs every still-tracked (non-detached) process group, so a PTY child a
/// dropped run left running does not outlive its owner. Detached commands are
/// never inserted here (they are host/OS property from birth, ADR 0019), so the
/// teardown never touches them.
struct ShellProcessTable {
    processes: StdMutex<HashMap<String, ShellProcess>>,
}

impl ShellProcessTable {
    fn new() -> Self {
        Self {
            processes: StdMutex::new(HashMap::new()),
        }
    }
}

impl Drop for ShellProcessTable {
    fn drop(&mut self) {
        // Drop is sync, and the group SIGKILL is a sync libc call, so no
        // async teardown hook is needed. Each child is reaped by its detached
        // wait thread once the signal lands.
        let mut processes = self.processes.lock_recover();
        for (_, proc) in processes.drain() {
            kill_process_group_and_reap(proc.pid, &proc.killer);
        }
    }
}

#[derive(Clone)]
pub(crate) struct ShellRuntime {
    pub(crate) shell_path: String,
    cwd: PathBuf,
    table: Arc<ShellProcessTable>,
    next_session_id: Arc<AtomicI32>,
}

impl ShellRuntime {
    pub(crate) fn new() -> Self {
        let shell_path = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            shell_path,
            cwd,
            table: Arc::new(ShellProcessTable::new()),
            next_session_id: Arc::new(AtomicI32::new(1)),
        }
    }

    pub(crate) fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    fn shell_name(shell_path: &str) -> &str {
        shell_path.rsplit('/').next().unwrap_or(shell_path)
    }

    pub(crate) fn resolve_workdir(&self, workdir: Option<&str>) -> PathBuf {
        match workdir {
            None => self.cwd.clone(),
            Some(path) => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    self.cwd.join(path)
                }
            }
        }
    }

    fn command_for_spawn(&self, command: &str, _shell_path: &str, pty: bool) -> String {
        let echo_off = if pty {
            // Disable terminal echo so bytes delivered via `shell.write`
            // don't appear in the captured output stream. The PTY allocates
            // with `ECHO` on by default (matching interactive terminals),
            // but agents drive these sessions programmatically and the echo
            // is pure noise. `stty -echo || true` keeps the prefix
            // harmless on environments where `stty` isn't available.
            "stty -echo 2>/dev/null || true\n"
        } else {
            ""
        };
        format!("{echo_off}{command}")
    }

    fn shell_args(
        &self,
        command: &str,
        login: bool,
        shell_path: &str,
        pty: bool,
    ) -> ShellResult<Vec<String>> {
        let command = self.command_for_spawn(command, shell_path, pty);
        if login {
            if !shell_supports_login(Self::shell_name(shell_path)) {
                return Err(shell_invalid_request(
                    "unsupported_login_shell",
                    format!(
                        "Login shell mode is not supported for {}",
                        Self::shell_name(shell_path)
                    ),
                ));
            }
            Ok(vec!["-l".to_string(), "-c".to_string(), command])
        } else {
            Ok(vec!["-c".to_string(), command])
        }
    }

    pub(crate) fn spawn_process(
        &self,
        id: String,
        command: &str,
        workdir: &Path,
        login: bool,
        shell_path: &str,
    ) -> ShellResult<()> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(DEFAULT_PTY_SIZE).map_err(|err| {
            shell_io_failure("open_pty_failed", format!("Failed to open PTY: {err}"))
        })?;

        let mut cmd = CommandBuilder::new(shell_path);
        for arg in self.shell_args(command, login, shell_path, true)? {
            cmd.arg(arg);
        }
        cmd.cwd(workdir.as_os_str());

        let child = pair.slave.spawn_command(cmd).map_err(|err| {
            shell_io_failure(
                "spawn_pty_command_failed",
                format!(
                    "Failed to spawn PTY command with shell `{}` in `{}`: {err}",
                    shell_path,
                    workdir.display()
                ),
            )
        })?;
        let killer = child.clone_killer();
        // Capture the child PID before the child is moved into the wait thread.
        // The PTY child is a session/process-group leader, so we kill the whole
        // group on cancel/timeout to reap backgrounded descendants.
        let pid = child.process_id();
        let reader = pair.master.try_clone_reader().map_err(|err| {
            shell_io_failure(
                "clone_pty_reader_failed",
                format!("Failed to clone PTY reader: {err}"),
            )
        })?;
        let writer = pair.master.take_writer().map_err(|err| {
            shell_io_failure(
                "take_pty_writer_failed",
                format!("Failed to take PTY writer: {err}"),
            )
        })?;
        drop(pair.slave);

        let buffer = Arc::new(StdMutex::new(Vec::new()));
        let buffer_start = Arc::new(StdMutex::new(0usize));
        let truncated = Arc::new(AtomicBool::new(false));
        let read_cursor = Arc::new(StdMutex::new(0usize));
        let exit_code = Arc::new(StdMutex::new(None));
        let exit_notify = Arc::new(Notify::new());
        let output_notify = Arc::new(Notify::new());
        let reader_died = Arc::new(AtomicBool::new(false));
        let spill = Arc::new(StdMutex::new(None));
        let killer = Arc::new(StdMutex::new(Some(killer)));

        let _reader = spawn_reader_thread(
            id.clone(),
            reader,
            Arc::clone(&buffer),
            Arc::clone(&buffer_start),
            Arc::clone(&truncated),
            Arc::clone(&spill),
            ReaderSignals::new(Arc::clone(&output_notify), Arc::clone(&reader_died)),
        );
        spawn_wait_thread(
            child,
            Arc::clone(&exit_code),
            Arc::clone(&exit_notify),
            Arc::clone(&output_notify),
        );

        let process = ShellProcess {
            _master: pair.master,
            writer: Arc::new(StdMutex::new(Some(writer))),
            buffer,
            buffer_start,
            truncated,
            read_cursor,
            exit_code,
            exit_notify,
            output_notify,
            reader_died,
            spill,
            killer,
            pid,
        };
        self.table.processes.lock_recover().insert(id, process);
        Ok(())
    }

    /// Launch a Detached Command (ADR 0019): a process that outlives every lash
    /// host. It is placed in its own session (`setsid`), so it leaves the
    /// worker's process group and controlling terminal — the teardown group-kill
    /// never reaches it and it survives host exit. lash keeps **no** PTY, writer,
    /// or process-map entry, so it will never track, signal, or stop it again;
    /// the returned identity is the whole of what lash retains. Because the child
    /// is its own session leader, its pgid equals its pid.
    pub(crate) fn spawn_detached(
        &self,
        command: &str,
        workdir: &Path,
        login: bool,
        shell_path: &str,
    ) -> ShellResult<DetachedLaunch> {
        let mut cmd = std::process::Command::new(shell_path);
        for arg in self.shell_args(command, login, shell_path, false)? {
            cmd.arg(arg);
        }
        cmd.current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(unix)]
        let (read_fd, write_fd) = {
            let mut fds = [0_i32; 2];
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
                return Err(shell_io_failure(
                    "spawn_detached_command_failed",
                    format!(
                        "Failed to create detached launch identity pipe: {}",
                        std::io::Error::last_os_error()
                    ),
                ));
            }
            (fds[0], fds[1])
        };

        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let detached_pid = libc::fork();
                if detached_pid == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if detached_pid > 0 {
                    let bytes = (detached_pid as u32).to_ne_bytes();
                    let written =
                        libc::write(write_fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
                    libc::_exit(i32::from(written != bytes.len() as isize));
                }
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(read_fd);
                Ok(())
            });
        }

        let child_result = cmd.spawn();
        #[cfg(unix)]
        unsafe {
            libc::close(write_fd);
        }
        let mut child = child_result.map_err(|err| {
            #[cfg(unix)]
            unsafe {
                libc::close(read_fd);
            }
            shell_io_failure(
                "spawn_detached_command_failed",
                format!(
                    "Failed to spawn detached command with shell `{}` in `{}`: {err}",
                    shell_path,
                    workdir.display()
                ),
            )
        })?;

        #[cfg(unix)]
        let pid = {
            let mut bytes = [0_u8; std::mem::size_of::<u32>()];
            let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
            if let Err(error) = reader.read_exact(&mut bytes) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(shell_io_failure(
                    "spawn_detached_command_failed",
                    format!("Detached launcher did not report its child identity: {error}"),
                ));
            }
            let pid = u32::from_ne_bytes(bytes);
            let status = child.wait().map_err(|error| {
                shell_io_failure(
                    "spawn_detached_command_failed",
                    format!("Failed to reap detached launcher: {error}"),
                )
            })?;
            if !status.success() {
                return Err(shell_io_failure(
                    "spawn_detached_command_failed",
                    format!("Detached launcher exited with {status}"),
                ));
            }
            pid
        };

        #[cfg(not(unix))]
        let pid = {
            let pid = child.id();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            pid
        };
        Ok(DetachedLaunch { pid, pgid: pid })
    }

    /// Stop a detached launch when its durable audit row could not be written.
    pub(crate) fn stop_detached(&self, launch: DetachedLaunch) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(launch.pgid as i32), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = launch;
    }

    pub(crate) fn allocate_handle_id(&self) -> String {
        self.next_session_id
            .fetch_add(1, Ordering::SeqCst)
            .to_string()
    }

    /// PID of a tracked PTY/pipe child, for teardown/self-fencing tests.
    #[cfg(test)]
    pub(crate) fn tracked_pid(&self, id: &str) -> Option<u32> {
        self.table
            .processes
            .lock_recover()
            .get(id)
            .and_then(|proc| proc.pid)
    }

    /// Count of tracked (non-detached) processes, for teardown tests.
    #[cfg(test)]
    pub(crate) fn tracked_len(&self) -> usize {
        self.table.processes.lock_recover().len()
    }

    fn process_state(&self, id: &str) -> ShellResult<ProcessState> {
        let procs = self.table.processes.lock_recover();
        let proc = procs.get(id).ok_or_else(|| {
            shell_invalid_request("unknown_shell_process", format!("No process with id: {id}"))
        })?;
        Ok(ProcessState {
            buffer: Arc::clone(&proc.buffer),
            exit_code: Arc::clone(&proc.exit_code),
            exit_notify: Arc::clone(&proc.exit_notify),
            output_notify: Arc::clone(&proc.output_notify),
            reader_died: Arc::clone(&proc.reader_died),
            killer: Arc::clone(&proc.killer),
            pid: proc.pid,
        })
    }

    fn take_incremental_output(
        &self,
        id: &str,
        max_output_tokens: Option<usize>,
    ) -> ShellResult<(String, Option<usize>, Option<PathBuf>)> {
        let (buffer, buffer_start, truncated, read_cursor, spill, reader_died) = {
            let procs = self.table.processes.lock_recover();
            let proc = procs.get(id).ok_or_else(|| {
                shell_invalid_request("unknown_shell_process", format!("Unknown session id {id}"))
            })?;
            (
                Arc::clone(&proc.buffer),
                Arc::clone(&proc.buffer_start),
                Arc::clone(&proc.truncated),
                Arc::clone(&proc.read_cursor),
                Arc::clone(&proc.spill),
                Arc::clone(&proc.reader_died),
            )
        };

        if reader_died.load(Ordering::SeqCst) {
            return Err(shell_reader_died_failure());
        }

        let buf = buffer.lock_recover();
        let start_offset = *buffer_start.lock_recover();
        let end_offset = start_offset + buf.len();
        let mut cursor = read_cursor.lock_recover();
        let had_gap = *cursor < start_offset;
        let start = (*cursor).max(start_offset);
        let relative_start = start.saturating_sub(start_offset);
        let mut rendered =
            String::from_utf8_lossy(buf.get(relative_start..).unwrap_or_default()).to_string();
        *cursor = end_offset;
        if !rendered.is_empty()
            && (had_gap || truncated.load(Ordering::SeqCst) && *cursor == end_offset)
        {
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered.push_str("[truncated]");
        }
        let rendered = clean_terminal_output(&rendered);
        let (rendered, original_token_count, token_truncated) =
            truncate_exec_output(rendered, max_output_tokens);
        let mut spill_guard = spill.lock_recover();
        let mut full_output_path = spill_guard.as_ref().map(|spill| spill.path.clone());
        if token_truncated && full_output_path.is_none() {
            full_output_path = activate_spill(id, &buf, &mut spill_guard);
        }
        Ok((rendered, original_token_count, full_output_path))
    }

    pub(crate) async fn wait_until_exit_or_timeout(
        &self,
        id: &str,
        timeout: Option<Duration>,
        max_output_tokens: Option<usize>,
        cancel: Option<CancellationToken>,
    ) -> ShellResult<PollOutcome> {
        let state = self.process_state(id)?;
        let deadline = timeout.map(|value| tokio::time::Instant::now() + value);
        loop {
            if state.reader_died.load(Ordering::SeqCst) {
                return Err(shell_reader_died_failure());
            }
            if let Some(token) = cancel.as_ref()
                && token.is_cancelled()
            {
                kill_child(&state);
                wait_for_child_exit(&state, Duration::from_millis(500)).await;
                return Ok(PollOutcome::Cancelled);
            }

            let exited = state.exit_code.lock_recover().is_some();
            if exited {
                wait_for_buffer_settle(&state, Duration::from_millis(OUTPUT_QUIET_PERIOD_MS)).await;
                let (output, original_token_count, full_output_path) =
                    self.take_incremental_output(id, max_output_tokens)?;
                let exit_code = state.exit_code.lock_recover().unwrap_or(-1);
                return Ok(PollOutcome::Exited {
                    output,
                    original_token_count,
                    exit_code,
                    full_output_path,
                });
            }

            if let Some(dl) = deadline
                && tokio::time::Instant::now() >= dl
            {
                let exit_code = *state.exit_code.lock_recover();
                if let Some(exit_code) = exit_code {
                    wait_for_buffer_settle(&state, Duration::from_millis(OUTPUT_QUIET_PERIOD_MS))
                        .await;
                    let (output, original_token_count, full_output_path) =
                        self.take_incremental_output(id, max_output_tokens)?;
                    return Ok(PollOutcome::Exited {
                        output,
                        original_token_count,
                        exit_code,
                        full_output_path,
                    });
                }
                let (output, original_token_count, full_output_path) =
                    self.take_incremental_output(id, max_output_tokens)?;
                return Ok(PollOutcome::Running {
                    output,
                    original_token_count,
                    full_output_path,
                });
            }

            let cancel_future = async {
                match cancel.as_ref() {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };

            if let Some(wake_at) = deadline {
                tokio::select! {
                    _ = state.exit_notify.notified() => {}
                    _ = tokio::time::sleep_until(wake_at) => {}
                    _ = cancel_future => {}
                }
            } else {
                tokio::select! {
                    _ = state.exit_notify.notified() => {}
                    _ = cancel_future => {}
                }
            }
        }
    }

    pub(crate) fn remove_process(&self, id: &str) {
        if let Some(proc) = self.table.processes.lock_recover().remove(id)
            && let Some(mut spill) = proc.spill.lock_recover().take()
        {
            // Flush but deliberately do NOT delete the spill here: this hook
            // fires as the same tool call hands `full_output_path` back to the
            // caller for later reading, so reaping now would destroy the
            // artifact. The file is created 0600 (owner-only); see the reaping
            // gap noted in `output::create_spill_file`.
            let _ = spill.file.flush();
        }
    }

    pub(crate) async fn write_stdin(&self, id: &str, input: &str) -> ShellResult<()> {
        let writer = {
            let procs = self.table.processes.lock_recover();
            let proc = procs.get(id).ok_or_else(|| {
                shell_invalid_request("unknown_shell_process", format!("Unknown session id {id}"))
            })?;
            Arc::clone(&proc.writer)
        };
        let input = input.to_string();
        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock_recover();
            let writer = writer.as_mut().ok_or_else(|| {
                shell_execution_failure("shell_stdin_unavailable", "Process stdin not available")
            })?;
            writer.write_all(input.as_bytes()).map_err(|err| {
                shell_io_failure("shell_stdin_write_failed", format!("Write failed: {err}"))
            })?;
            writer.flush().map_err(|err| {
                shell_io_failure("shell_stdin_flush_failed", format!("Flush failed: {err}"))
            })
        })
        .await
        .map_err(|err| {
            shell_execution_failure(
                "shell_stdin_task_failed",
                format!("Write task failed: {err}"),
            )
        })?
    }

    pub(crate) async fn close_stdin(&self, id: &str) -> ShellResult<()> {
        let writer = {
            let procs = self.table.processes.lock_recover();
            let proc = procs.get(id).ok_or_else(|| {
                shell_invalid_request("unknown_shell_process", format!("Unknown session id {id}"))
            })?;
            Arc::clone(&proc.writer)
        };
        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock_recover();
            writer.take();
            Ok(())
        })
        .await
        .map_err(|err| {
            shell_execution_failure(
                "close_shell_stdin_task_failed",
                format!("Close stdin task failed: {err}"),
            )
        })?
    }

    pub(crate) async fn exec_pipe_process(
        &self,
        request: PipeExecProcessRequest<'_>,
    ) -> ShellResult<PollOutcome> {
        let PipeExecProcessRequest {
            id,
            command,
            workdir,
            login,
            shell_path,
            timeout,
            max_output_tokens,
            cancel,
        } = request;
        let mut cmd = TokioCommand::new(shell_path);
        for arg in self.shell_args(command, login, shell_path, false)? {
            cmd.arg(arg);
        }
        cmd.current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|err| {
            shell_io_failure(
                "spawn_shell_command_failed",
                format!(
                    "Failed to spawn command with shell `{}` in `{}`: {err}",
                    shell_path,
                    workdir.display()
                ),
            )
        })?;
        let child_pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let buffer = Arc::new(StdMutex::new(Vec::new()));
        let buffer_start = Arc::new(StdMutex::new(0usize));
        let truncated = Arc::new(AtomicBool::new(false));
        let spill = Arc::new(StdMutex::new(None));
        let output_notify = Arc::new(Notify::new());
        let reader_died = Arc::new(AtomicBool::new(false));
        let mut reader_handles = Vec::new();

        if let Some(stdout) = stdout {
            reader_handles.push(spawn_async_reader(
                id.to_string(),
                stdout,
                Arc::clone(&buffer),
                Arc::clone(&buffer_start),
                Arc::clone(&truncated),
                Arc::clone(&spill),
                ReaderSignals::new(Arc::clone(&output_notify), Arc::clone(&reader_died)),
            ));
        }
        if let Some(stderr) = stderr {
            reader_handles.push(spawn_async_reader(
                id.to_string(),
                stderr,
                Arc::clone(&buffer),
                Arc::clone(&buffer_start),
                Arc::clone(&truncated),
                Arc::clone(&spill),
                ReaderSignals::new(Arc::clone(&output_notify), Arc::clone(&reader_died)),
            ));
        }

        let deadline = timeout.map(|value| tokio::time::Instant::now() + value);
        let wait_handle = tokio::spawn(async move { child.wait().await });
        tokio::pin!(wait_handle);
        loop {
            if let Some(token) = cancel.as_ref()
                && token.is_cancelled()
            {
                terminate_pipe_process(child_pid);
                let _ = tokio::time::timeout(Duration::from_millis(500), &mut wait_handle).await;
                wait_for_pipe_readers(&mut reader_handles).await;
                if reader_died.load(Ordering::SeqCst) {
                    return Err(shell_reader_died_failure());
                }
                return Ok(PollOutcome::Cancelled);
            }

            if let Some(dl) = deadline
                && tokio::time::Instant::now() >= dl
            {
                terminate_pipe_process(child_pid);
                let _ = tokio::time::timeout(Duration::from_millis(500), &mut wait_handle).await;
                wait_for_pipe_readers(&mut reader_handles).await;
                let (output, original_token_count, full_output_path) = render_buffer_output(
                    id,
                    &buffer,
                    &buffer_start,
                    Arc::as_ref(&truncated),
                    &spill,
                    max_output_tokens,
                );
                return Ok(PollOutcome::Running {
                    output,
                    original_token_count,
                    full_output_path,
                });
            }

            let cancel_future = async {
                match cancel.as_ref() {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };

            if let Some(wake_at) = deadline {
                tokio::select! {
                    status = &mut wait_handle => {
                        let exit_code = status
                            .map_err(|err| {
                                shell_execution_failure(
                                    "shell_wait_task_failed",
                                    format!("Wait task failed: {err}"),
                                )
                            })?
                            .map(exit_status_code)
                            .unwrap_or(-1);
                        wait_for_pipe_readers(&mut reader_handles).await;
                        if reader_died.load(Ordering::SeqCst) {
                            return Err(shell_reader_died_failure());
                        }
                        let (output, original_token_count, full_output_path) = render_buffer_output(
                            id,
                            &buffer,
                            &buffer_start,
                            Arc::as_ref(&truncated),
                            &spill,
                            max_output_tokens,
                        );
                        return Ok(PollOutcome::Exited {
                            output,
                            original_token_count,
                            exit_code,
                            full_output_path,
                        });
                    }
                    _ = tokio::time::sleep_until(wake_at) => {}
                    _ = cancel_future => {}
                }
            } else {
                tokio::select! {
                    status = &mut wait_handle => {
                        let exit_code = status
                            .map_err(|err| {
                                shell_execution_failure(
                                    "shell_wait_task_failed",
                                    format!("Wait task failed: {err}"),
                                )
                            })?
                            .map(exit_status_code)
                            .unwrap_or(-1);
                        wait_for_pipe_readers(&mut reader_handles).await;
                        if reader_died.load(Ordering::SeqCst) {
                            return Err(shell_reader_died_failure());
                        }
                        let (output, original_token_count, full_output_path) = render_buffer_output(
                            id,
                            &buffer,
                            &buffer_start,
                            Arc::as_ref(&truncated),
                            &spill,
                            max_output_tokens,
                        );
                        return Ok(PollOutcome::Exited {
                            output,
                            original_token_count,
                            exit_code,
                            full_output_path,
                        });
                    }
                    _ = cancel_future => {}
                }
            }
        }
    }
}

async fn wait_for_pipe_readers(handles: &mut Vec<tokio::task::JoinHandle<()>>) {
    for handle in handles.drain(..) {
        let _ = tokio::time::timeout(Duration::from_millis(500), handle).await;
    }
}

fn shell_supports_login(shell_name: &str) -> bool {
    matches!(shell_name, "bash" | "zsh" | "ksh" | "mksh" | "fish")
}
