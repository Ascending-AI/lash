//! Worker placement in a cgroup v2 subtree the host delegates (FIG-6787).
//!
//! The pool makes one cgroup per worker under the subtree, writes the
//! worker's limits into it, and the worker moves itself in between fork and
//! exec, so nothing it allocates is charged elsewhere. The worker's
//! address-space ceiling ([`crate::WorkerConfinement`]) holds whether or not
//! a host places it.

use lash_protocol_types::engine_fault::{CgroupController, CgroupRefusal};
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A cgroup v2 subtree the host delegates to a pool, and the limits each
/// worker runs under in it. The pool makes a cgroup of the worker's own in
/// [`Self::subtree`] for each worker it starts and removes it once the
/// worker is reaped. A worker whose resident memory passes
/// [`Self::memory_max_bytes`] is killed by the kernel, and its run ends with
/// [`lash_protocol_types::worker_limit::WorkerLimit::Memory`], as it would again on the same
/// inputs. Swap is closed to the worker where the kernel offers it, so the
/// ceiling bounds resident memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCgroup {
    /// A cgroup v2 directory the host may make cgroups in and move the
    /// pool's workers into. It must offer the `memory` controller, and
    /// `cpu` when [`Self::cpu`] is set; the pool enables them for its
    /// children when they are not yet.
    pub subtree: PathBuf,
    /// The worker's `memory.max`: resident memory past which the kernel
    /// kills it.
    pub memory_max_bytes: u64,
    /// The worker's `memory.high`: resident memory past which the kernel
    /// reclaims from it and slows its allocations. `None` sets none.
    pub memory_high_bytes: Option<u64>,
    /// The worker's `cpu.max`. `None` sets none.
    pub cpu: Option<CpuShare>,
}

/// A worker's CPU share (`cpu.max`): at most `quota` of CPU time across its
/// threads in each `period`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuShare {
    pub quota: Duration,
    pub period: Duration,
}

impl CpuShare {
    /// Whether the kernel admits this share: a period from 1 ms to 1 s and a
    /// quota of at least 1 ms.
    pub(crate) fn is_valid(&self) -> bool {
        (Duration::from_millis(1)..=Duration::from_secs(1)).contains(&self.period)
            && self.quota >= Duration::from_millis(1)
            && micros(self.quota).is_some()
    }
}

fn micros(duration: Duration) -> Option<u64> {
    u64::try_from(duration.as_micros()).ok()
}

/// The pool's cgroups are named for their parent process and a count, so
/// pools in one subtree never share a name.
static NEXT_PLACEMENT: AtomicU64 = AtomicU64::new(0);

/// What the worker's move into its cgroup failed with, as the spawn reports
/// it: the OS error offset past any error `exec` returns.
const MOVE_REFUSED: i32 = 0x4C00_0000;

impl WorkerCgroup {
    /// Whether this host can place workers in [`Self::subtree`] under these
    /// limits: a cgroup v2 directory that hands its children the
    /// controllers they need and takes a worker's cgroup with its limits.
    /// The pool asks once, when it starts.
    pub fn probe(&self) -> Result<(), CgroupRefusal> {
        let offered = read(&self.subtree.join("cgroup.controllers"))
            .map_err(|_| CgroupRefusal::NotCgroupV2)?;
        let needed = self.controllers();
        if let Some(controller) = needed.iter().find(|c| !names(&offered, **c)) {
            return Err(CgroupRefusal::ControllerNotDelegated {
                controller: *controller,
            });
        }
        let enabled = read(&self.subtree.join("cgroup.subtree_control"))
            .map_err(|_| CgroupRefusal::NotCgroupV2)?;
        for controller in needed {
            if !names(&enabled, controller) {
                write(
                    &self.subtree.join("cgroup.subtree_control"),
                    &format!("+{}", name(controller)),
                )
                .map_err(|_| CgroupRefusal::ControllerNotDelegated { controller })?;
            }
        }
        self.place().map(drop)
    }

    fn controllers(&self) -> Vec<CgroupController> {
        let mut controllers = vec![CgroupController::Memory];
        if self.cpu.is_some() {
            controllers.push(CgroupController::Cpu);
        }
        controllers
    }

    /// A fresh cgroup for one worker, its limits written.
    pub(crate) fn place(&self) -> Result<Placement, CgroupRefusal> {
        let directory = self.subtree.join(format!(
            "lash-worker-{}-{}",
            std::process::id(),
            NEXT_PLACEMENT.fetch_add(1, Ordering::Relaxed)
        ));
        create_dir(&directory).map_err(refused)?;
        let mut placement = Placement {
            directory,
            procs: None,
            memory_max_bytes: self.memory_max_bytes,
        };
        placement.limit("memory.max", &self.memory_max_bytes.to_string())?;
        if let Some(high) = self.memory_high_bytes {
            placement.limit("memory.high", &high.to_string())?;
        }
        match write(&placement.directory.join("memory.swap.max"), "0") {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            result => result.map_err(refused)?,
        }
        if let Some(cpu) = self.cpu {
            let (Some(quota), Some(period)) = (micros(cpu.quota), micros(cpu.period)) else {
                return Err(CgroupRefusal::Refused {
                    code: Some(libc::EINVAL),
                });
            };
            placement.limit("cpu.max", &format!("{quota} {period}"))?;
        }
        placement.procs =
            Some(open_write(&placement.directory.join("cgroup.procs")).map_err(refused)?);
        Ok(placement)
    }
}

/// One worker's cgroup, removed when dropped: after its worker is reaped.
#[derive(Debug)]
pub(crate) struct Placement {
    directory: PathBuf,
    procs: Option<std::fs::File>,
    memory_max_bytes: u64,
}

impl Placement {
    fn limit(&self, file: &str, value: &str) -> Result<(), CgroupRefusal> {
        write(&self.directory.join(file), value).map_err(refused)
    }

    /// The descriptor a forked worker writes itself into, before it execs.
    pub(crate) fn procs(&self) -> Option<RawFd> {
        self.procs.as_ref().map(std::fs::File::as_raw_fd)
    }

    pub(crate) fn memory_max_bytes(&self) -> u64 {
        self.memory_max_bytes
    }

    /// Whether the kernel's OOM killer killed a process in this cgroup.
    pub(crate) fn oom_killed(&self) -> bool {
        read(&self.directory.join("memory.events")).is_ok_and(|events| {
            events.lines().any(|line| {
                line.strip_prefix("oom_kill ")
                    .and_then(|count| count.trim().parse::<u64>().ok())
                    .is_some_and(|count| count > 0)
            })
        })
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        self.procs = None;
        // Best effort: a cgroup that still holds a process stays.
        let _ = remove_dir(&self.directory);
    }
}

/// Run between fork and exec in the worker: moves the calling process into
/// the cgroup `procs` names. Only `write` runs, which is async-signal-safe.
pub(crate) fn enter(procs: RawFd) -> std::io::Result<()> {
    // SAFETY: writes one byte from a static buffer to a descriptor the
    // parent opened and keeps open across the spawn.
    #[expect(
        unsafe_code,
        reason = "the forked worker writes itself into its cgroup"
    )]
    let written = unsafe { libc::write(procs, b"0".as_ptr().cast(), 1) };
    if written == 1 {
        return Ok(());
    }
    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    Err(std::io::Error::from_raw_os_error(
        MOVE_REFUSED.saturating_add(code),
    ))
}

/// The refusal a spawn error is, when it is the worker's failed move into
/// its cgroup. Any other spawn error, a missing executable among them, is
/// below the offset and is not one.
pub(crate) fn move_refusal(error: &std::io::Error) -> Option<CgroupRefusal> {
    let code = error
        .raw_os_error()?
        .checked_sub(MOVE_REFUSED)
        .filter(|code| *code >= 0)?;
    Some(CgroupRefusal::Refused {
        code: (code > 0).then_some(code),
    })
}

fn refused(error: std::io::Error) -> CgroupRefusal {
    CgroupRefusal::Refused {
        code: error.raw_os_error(),
    }
}

fn name(controller: CgroupController) -> &'static str {
    match controller {
        CgroupController::Memory => "memory",
        CgroupController::Cpu => "cpu",
    }
}

fn names(list: &str, controller: CgroupController) -> bool {
    list.split_whitespace().any(|word| word == name(controller))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the host names the cgroup subtree in its pool configuration"
)]
fn read(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the host names the cgroup subtree in its pool configuration"
)]
fn open_write(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).open(path)
}

/// One write of `value`: a cgroup interface file takes a value in one write.
fn write(path: &Path, value: &str) -> std::io::Result<()> {
    open_write(path)?.write_all(value.as_bytes())
}

#[expect(
    clippy::disallowed_methods,
    reason = "the host names the cgroup subtree in its pool configuration"
)]
fn create_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the host names the cgroup subtree in its pool configuration"
)]
fn remove_dir(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}
