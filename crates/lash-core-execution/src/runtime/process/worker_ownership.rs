//! Durable ownership for one worker. The StartKey recovers one ProcessId;
//! this ledger retains that identity's launch and terminal across host loss.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::worker_engine::WorkerState;
use super::{ProcessId, StartKey};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerIdentity {
    pub pid: NonZeroU32,
    boot: String,
    namespace: PathBuf,
    start: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ownership {
    pub process_id: ProcessId,
    pub start_key: Option<StartKey>,
    pub payload: serde_json::Value,
    pub state: WorkerState,
}

#[derive(Clone)]
pub(super) struct Ledger {
    path: PathBuf,
}

pub(super) struct Locked {
    ledger: Ledger,
    // The separate inode survives replacement of the data file.
    _lock: File,
}

impl Ledger {
    pub fn new(root: &Path, kind: &str, process: &ProcessId) -> Self {
        let digest = Sha256::digest(format!("{kind}\0{process}"));
        Self {
            path: root.join(format!("{digest:x}")),
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the host supplies this durable worker directory"
    )]
    pub fn lock(&self) -> std::io::Result<Locked> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;
        Ok(Locked {
            ledger: self.clone(),
            _lock: lock,
        })
    }

    /// Publish a terminal only once. A recovering supervisor and the original
    /// may both observe exit, but neither may replace a retained terminal.
    pub fn finish(&self, state: WorkerState) -> std::io::Result<WorkerState> {
        let locked = self.lock()?;
        let mut record = locked
            .read()?
            .ok_or_else(|| std::io::Error::other("worker ownership disappeared"))?;
        if record.state.is_terminal() {
            return Ok(record.state);
        }
        record.state = state;
        locked.write(&record)?;
        Ok(record.state)
    }
}

impl Locked {
    #[expect(
        clippy::disallowed_methods,
        reason = "read ownership under the host-supplied directory"
    )]
    pub fn read(&self) -> std::io::Result<Option<Ownership>> {
        let mut file = match File::open(&self.ledger.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(std::io::Error::other)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "atomically persist ownership in the host-supplied directory"
    )]
    pub fn write(&self, record: &Ownership) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        let temporary = self.ledger.path.with_extension("pending");
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(temporary, &self.ledger.path)?;
        let directory = self
            .ledger
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("worker directory has no parent"))?;
        File::open(directory)?.sync_all()
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the physical worker checks kernel process identity, including reboot and PID reuse"
)]
impl WorkerIdentity {
    pub fn read(pid: NonZeroU32) -> std::io::Result<Self> {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let namespace = std::fs::read_link("/proc/self/ns/pid")?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        // comm can contain spaces and closing parentheses; fields after its
        // last ')' begin at field 3, and starttime is field 22.
        let fields = stat
            .rsplit_once(')')
            .ok_or_else(|| std::io::Error::other("invalid process stat"))?
            .1;
        let start = fields
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| std::io::Error::other("missing process start time"))?
            .parse()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            pid,
            boot,
            namespace,
            start,
        })
    }

    /// A different kernel or PID namespace makes the original worker
    /// inaccessible, not proven dead. Keep its ownership unresolved there.
    pub fn check_environment(&self) -> std::io::Result<()> {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let namespace = std::fs::read_link("/proc/self/ns/pid")?;
        if boot != self.boot || namespace != self.namespace {
            tracing::warn!(
                worker_pid = self.pid.get(),
                expected_boot = %self.boot,
                observed_boot = %boot,
                expected_namespace = ?self.namespace,
                observed_namespace = ?namespace,
                decision = "refuse_ownership",
                "the recorded worker's kernel or PID namespace is inaccessible",
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "the recorded worker belongs to another kernel or PID namespace",
            ));
        }
        Ok(())
    }

    pub fn matches(&self) -> std::io::Result<bool> {
        self.check_environment()?;
        match Self::read(self.pid) {
            Ok(found) => Ok(found.boot == self.boot
                && found.namespace == self.namespace
                && found.start == self.start),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// A pidfd pins the kernel process before the identity check, closing the
/// check-to-signal PID reuse race. No raw PID is ever used for a signal.
#[cfg(target_os = "linux")]
pub(super) struct Adopted(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl Adopted {
    pub fn open(identity: &WorkerIdentity) -> std::io::Result<Option<Self>> {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        identity.check_environment()?;
        let raw = i32::try_from(identity.pid.get()).map_err(std::io::Error::other)?;
        let pid = Pid::from_raw(raw).ok_or_else(|| std::io::Error::other("invalid worker PID"))?;
        let fd = match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::SRCH) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if identity.matches()? {
            Ok(Some(Self(fd)))
        } else {
            Ok(None)
        }
    }

    pub fn kill(&self) -> std::io::Result<()> {
        use rustix::process::{Signal, pidfd_send_signal};
        match pidfd_send_signal(&self.0, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn ended(&self) -> std::io::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        poll(&mut fds, Some(&timeout))
            .map(|ready| ready > 0)
            .map_err(Into::into)
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) struct Adopted;

#[cfg(not(target_os = "linux"))]
impl Adopted {
    pub fn open(_: &WorkerIdentity) -> std::io::Result<Option<Self>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "worker adoption requires Linux pidfds",
        ))
    }
    pub fn kill(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "worker adoption requires Linux pidfds",
        ))
    }
    pub fn ended(&self) -> std::io::Result<bool> {
        self.kill().map(|()| false)
    }
}
