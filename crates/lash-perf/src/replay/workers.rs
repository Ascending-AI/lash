//! The code-mode worker processes a replay's host spawns: which program
//! serves each dialect, and the crash-resume workload's kill.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// What a replay's Python worker runs: this host names Python alone for
/// it, through the builder every host uses.
fn python_embedding(
    _tuning: &lash::vm::WorkerTuning,
) -> Result<lash_vm_worker::Embedding, lash_vm_worker::EmbedError> {
    lash_vm_worker::Embedder::kernel()?
        .dialect(lash_vm_worker::python())?
        .finish()
}

/// Serve as a Python worker when this process was started as one: a pool
/// re-executes `lash-perf` for Python cells. Call before anything else.
///
/// # Errors
///
/// The worker could not start or serve.
pub fn python_worker_entry() -> Result<bool, lash::vm::PoolError> {
    lash_vm_worker::worker_entry(&python_embedding)
}

/// The worker service for `protocol`'s cells, with its run deadlines off the
/// clock: a replay measures the cell, not a deadline. TypeScript runs on
/// the shipped worker (`LASH_VM_WORKER` in tests); Python on this binary.
pub(crate) fn service(
    python: bool,
    _profile_dir: &std::path::Path,
) -> anyhow::Result<lash::vm::WorkerService> {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config =
        crate::cell_bounds::admitting(lash::vm::WorkerService::default().config().clone());
    if python {
        config.entry = lash::vm::WorkerEntry::reexec()
            .map_err(|error| anyhow::anyhow!("locate this binary for Python workers: {error}"))?;
        #[cfg(feature = "dhat-heap")]
        config.entry.args.extend([
            "--heap-profile-dir".to_owned(),
            _profile_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("worker heap-profile path must be UTF-8"))?
                .to_owned(),
        ]);
    }
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    config.deadlines.execution_wall = OFF_THE_CLOCK;
    Ok(lash::vm::WorkerService::new(config))
}

/// This process's live children, read from every thread's `children` list.
fn children() -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Whether child `pid` is a code-mode worker: it was started with the
/// worker entry's flag.
fn is_code_worker(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == b"--lash-vm-worker")
        })
        .unwrap_or(false)
}

/// Workers [`kill_code_workers`] has killed in this process.
static KILLED: AtomicU64 = AtomicU64::new(0);

/// Workers killed so far: the crash-resume workload's evidence.
pub(crate) fn killed() -> u64 {
    KILLED.load(Ordering::Relaxed)
}

/// Kill every code-mode worker child with SIGKILL, as a crashed host
/// machine would lose them. Returns how many were killed.
pub(crate) fn kill_code_workers() -> usize {
    let killed = children()
        .into_iter()
        .filter(|pid| is_code_worker(*pid))
        .filter(|pid| {
            let Ok(pid) = libc::pid_t::try_from(*pid) else {
                return false;
            };
            // SAFETY: kill(2) on a child pid this process just listed; it
            // touches no memory of this process.
            #[expect(unsafe_code, reason = "libc exposes kill(2) only as an FFI call")]
            let killed = unsafe { libc::kill(pid, libc::SIGKILL) } == 0;
            killed
        })
        .count();
    KILLED.fetch_add(killed as u64, Ordering::Relaxed);
    killed
}
