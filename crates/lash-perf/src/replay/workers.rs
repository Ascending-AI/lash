//! The code-mode worker processes a replay's host spawns: which program
//! serves each dialect, and the crash-resume workload's kill.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// What a replay's Python worker runs: this host names Python alone for
/// it, through the builder every host uses.
fn python_embedding(
    _tuning: &lash::vm::WorkerTuning,
) -> Result<lash_vm_worker::Embedder, lash_vm_worker::EmbedError> {
    lash_vm_worker::Embedder::new().dialect(lash_vm_worker::python())
}

/// Serve replay workers before host runtime initialization. The harness installs
/// its allocator reader after the worker closes inherited descriptors.
pub fn python_worker_entry() -> Result<bool, lash::vm::PoolError> {
    let args: Vec<_> = std::env::args().collect();
    let counted = args.iter().any(|a| a == "--replay-resources");
    let instructions = args.iter().any(|a| a == "--replay-instructions");
    let python = !args.iter().any(|a| a == "--replay-typescript");
    lash_vm_worker::worker_entry(&move |tuning| {
        if counted {
            super::prepare_schema_engines();
            let mut previous = crate::GLOBAL_ALLOCATOR.stats();
            let counter = instructions
                .then(crate::perf_support::instructions::InstructionCounter::open_threads)
                .transpose()
                .map_err(|e| lash_vm_worker::EmbedError::Registry(e.to_string()))?;
            let mut pmu_before = counter
                .as_ref()
                .map(|c| c.snapshot())
                .transpose()
                .map_err(|e| lash_vm_worker::EmbedError::Registry(e.to_string()))?;
            lash_vm_worker::observe_resources(Box::new(move || {
                let after = crate::GLOBAL_ALLOCATOR.stats();
                let mut resources = lash_vm_worker::WorkerResources {
                    allocations: (after.allocations + after.reallocations
                        - previous.allocations
                        - previous.reallocations) as u64,
                    allocated_bytes: (after.bytes_allocated - previous.bytes_allocated) as u64,
                    ..Default::default()
                };
                previous = after;
                if let Some(counter) = &counter {
                    let now = counter.snapshot().map_err(anyhow::Error::from);
                    match now.and_then(|now| {
                        let before = pmu_before
                            .replace(now)
                            .ok_or_else(|| anyhow::anyhow!("missing PMU start"))?;
                        Ok(now.since(before)?.qualify()?)
                    }) {
                        Ok(delta) => {
                            resources.instructions = delta.instructions;
                            resources.instruction_enabled_ns = delta.time_enabled;
                            resources.instruction_running_ns = delta.time_running;
                        }
                        Err(_) => resources.instruction_refusals = 1,
                    }
                }
                resources
            }));
        }
        if python {
            python_embedding(tuning)
        } else {
            lash_vm_worker::built_dialects(tuning)
        }
    })
}

/// The worker service for `protocol`'s cells, with its run deadlines off the
/// clock: a replay measures the cell, not a deadline. TypeScript runs on
/// the shipped worker (`LASH_VM_WORKER` in tests); Python on this binary.
pub(crate) fn service(
    python: bool,
    _profile_dir: &std::path::Path,
    instruction_limit: Option<u64>,
    resources: bool,
    instructions: bool,
    staging_parent: Option<&std::path::Path>,
) -> anyhow::Result<lash::vm::WorkerService> {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config =
        crate::cell_bounds::admitting(lash::vm::WorkerService::default().config().clone());
    if let Some(limit) = instruction_limit {
        config.run_bounds.charge_limit = config.run_bounds.charge_limit.max(limit);
    }
    if python || resources {
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
    if resources {
        config
            .entry
            .args
            .extend(["--count-heap-work".into(), "--replay-resources".into()]);
        if !python {
            config.entry.args.push("--replay-typescript".into());
        }
        if instructions {
            config.entry.args.push("--replay-instructions".into());
        }
    }
    config.staging_dir = staging_parent.map(|parent| parent.join("spools"));
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
