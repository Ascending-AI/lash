use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{PoolConfig, PoolError};
use lash_vm_protocol::{
    FRAME_HEADER_BYTES, FrameCodec, InfrastructureOutcome, ParentFrame, WorkerFrame,
};

pub struct Worker {
    child: Child,
    reaped: bool,
    pub pipe: UnixStream,
    pub codec: FrameCodec,
    pub cpu_nanos: u64,
}

impl Worker {
    #[expect(
        clippy::disallowed_methods,
        reason = "supervisor launches only the explicitly configured credential-free entry"
    )]
    pub fn spawn(config: &PoolConfig) -> Result<Self, PoolError> {
        let (pipe, child_pipe) = UnixStream::pair().map_err(PoolError::io)?;
        let fd = child_pipe.as_raw_fd();
        let mut command = Command::new(&config.entry.executable);
        command
            .args(&config.entry.args)
            .arg("--lash-vm-worker")
            .arg(fd.to_string())
            .arg(serde_json::to_string(&Bootstrap::from(config)).map_err(PoolError::protocol)?)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the post-fork closure only calls async-signal-safe fcntl.
        // child_pipe stays owned until spawn has returned. The entry closes
        // every other descriptor immediately after exec, including stdio.
        #[expect(unsafe_code, reason = "the one IPC descriptor must survive exec")]
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(PoolError::io)?;
        drop(child_pipe);
        Ok(Self {
            child,
            reaped: false,
            pipe,
            codec: FrameCodec::new(config.protocol.decode),
            cpu_nanos: 0,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, frame: &ParentFrame, timeout: Duration) -> Result<(), PoolError> {
        let bytes = self.codec.encode_parent(frame).map_err(PoolError::from)?;
        write_frame(&mut self.pipe, &bytes, Instant::now() + timeout)
    }

    pub fn receive(&mut self, deadline: Instant) -> Result<WorkerFrame, PoolError> {
        let bytes = match read_frame(&mut self.pipe, &self.codec, deadline) {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                return Err(InfrastructureOutcome::WorkerCrashed {
                    evidence: self.exit_evidence(),
                }
                .into());
            }
            result => result?,
        };
        self.codec.decode_worker(&bytes).map_err(PoolError::from)
    }

    fn exit_evidence(&self) -> lash_vm_protocol::SupervisorEvidence {
        // Observe without reaping: terminate owns wait4 and its CPU receipt.
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        #[expect(
            unsafe_code,
            reason = "waitid observes only this owned child without reaping it"
        )]
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return lash_vm_protocol::SupervisorEvidence::EndOfStream;
        }
        #[expect(
            unsafe_code,
            reason = "successful waitid initialized siginfo, including its status accessor"
        )]
        let info = unsafe { info.assume_init() };
        #[expect(
            unsafe_code,
            reason = "waitid WEXITED fills the child-status siginfo union"
        )]
        let status = unsafe { info.si_status() };
        match info.si_code {
            libc::CLD_EXITED => lash_vm_protocol::SupervisorEvidence::Exited { code: status },
            libc::CLD_KILLED | libc::CLD_DUMPED => {
                lash_vm_protocol::SupervisorEvidence::Signalled { signal: status }
            }
            _ => lash_vm_protocol::SupervisorEvidence::EndOfStream,
        }
    }

    /// Kill and reap before a replacement can be admitted. wait4 supplies
    /// final process CPU even when no final worker frame was delivered.
    pub fn terminate(&mut self) -> u64 {
        if self.reaped {
            return self.cpu_nanos;
        }
        let _ = self.child.kill();
        let mut status = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        loop {
            // SAFETY: this Worker exclusively owns the Child's wait. Both
            // outputs point to writable storage; successful wait4 initializes it.
            #[expect(unsafe_code, reason = "reaping with CPU evidence needs wait4")]
            let result =
                unsafe { libc::wait4(self.child.id() as i32, &mut status, 0, usage.as_mut_ptr()) };
            if result >= 0 {
                #[expect(unsafe_code, reason = "successful wait4 initialized rusage")]
                let usage = unsafe { usage.assume_init() };
                let nanos = |v: libc::timeval| {
                    (v.tv_sec as u64)
                        .saturating_mul(1_000_000_000)
                        .saturating_add((v.tv_usec as u64).saturating_mul(1000))
                };
                self.reaped = true;
                self.cpu_nanos = self
                    .cpu_nanos
                    .max(nanos(usage.ru_utime).saturating_add(nanos(usage.ru_stime)));
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                self.reaped = true;
                break;
            }
        }
        self.cpu_nanos
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bootstrap {
    pub frame: u32,
    pub depth: u32,
    pub nodes: u64,
    pub allocation: u64,
    pub state: u64,
    pub effect: u64,
    pub source: u64,
    pub cpu_nanos: u64,
}
impl From<&PoolConfig> for Bootstrap {
    fn from(c: &PoolConfig) -> Self {
        Self {
            frame: c.protocol.decode.max_frame_bytes,
            depth: c.protocol.decode.max_depth,
            nodes: c.protocol.decode.max_nodes,
            allocation: c.protocol.decode.max_allocation_bytes,
            state: c.protocol.max_vm_state_bytes,
            effect: c.protocol.max_effect_value_bytes,
            source: c.protocol.max_source_bytes,
            cpu_nanos: c
                .deadlines
                .cumulative_cpu
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
        }
    }
}

pub fn read_frame(
    pipe: &mut UnixStream,
    codec: &FrameCodec,
    deadline: Instant,
) -> Result<Vec<u8>, PoolError> {
    let mut bytes = vec![0; FRAME_HEADER_BYTES];
    read_until(pipe, &mut bytes, deadline, false)?;
    let len = codec
        .frame_len(&bytes)
        .map_err(PoolError::from)?
        .ok_or_else(|| PoolError::protocol("incomplete frame header"))?;
    bytes.resize(len, 0);
    read_until(pipe, &mut bytes[FRAME_HEADER_BYTES..], deadline, true)?;
    Ok(bytes)
}

fn remaining(deadline: Instant) -> Result<Duration, PoolError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(PoolError::Infrastructure(
            InfrastructureOutcome::WorkerUnresponsive { silent_ms: 0 },
        ))
}

fn read_until(
    pipe: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
    mut partial: bool,
) -> Result<(), PoolError> {
    while !bytes.is_empty() {
        pipe.set_read_timeout(Some(remaining(deadline)?))
            .map_err(PoolError::io)?;
        match pipe.read(bytes) {
            Ok(0) if partial => return Err(PoolError::protocol("EOF in a partial frame")),
            Ok(0) => return Err(PoolError::eof()),
            Ok(n) => {
                bytes = &mut bytes[n..];
                partial = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(PoolError::io(e)),
        }
    }
    Ok(())
}

pub fn write_frame(
    pipe: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), PoolError> {
    while !bytes.is_empty() {
        pipe.set_write_timeout(Some(remaining(deadline)?))
            .map_err(PoolError::io)?;
        match pipe.write(bytes) {
            Ok(0) => return Err(PoolError::eof()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(PoolError::io(e)),
        }
    }
    Ok(())
}
