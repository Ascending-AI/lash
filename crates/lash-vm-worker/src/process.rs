use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{PoolConfig, PoolError};
use lash_vm_protocol::{
    FRAME_HEADER_BYTES, FrameCodec, InfrastructureOutcome, ParentFrame, WorkerFrame,
};

pub(crate) struct Worker {
    child: Child,
    reaped: bool,
    pub(crate) pipe: UnixStream,
    pub(crate) codec: FrameCodec,
    pub(crate) cpu_nanos: u64,
}

impl Worker {
    #[expect(
        clippy::disallowed_methods,
        reason = "supervisor launches only the explicitly configured credential-free entry"
    )]
    pub(crate) fn spawn(config: &PoolConfig) -> Result<Self, PoolError> {
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
            codec: FrameCodec::new(config.entry.build.clone(), config.protocol.decode),
            cpu_nanos: 0,
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn send(&mut self, frame: &ParentFrame, timeout: Duration) -> Result<(), PoolError> {
        let bytes = self.codec.encode_parent(frame).map_err(PoolError::from)?;
        write_frame(&mut self.pipe, &bytes, Instant::now() + timeout)
    }

    pub(crate) fn receive(&mut self, deadline: Instant) -> Result<WorkerFrame, PoolError> {
        let bytes = read_frame(&mut self.pipe, &self.codec, deadline)?;
        self.codec.decode_worker(&bytes).map_err(PoolError::from)
    }

    /// Kill and reap before a replacement can be admitted. wait4 supplies
    /// final process CPU even when no final worker frame was delivered.
    pub(crate) fn terminate(&mut self) -> u64 {
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
pub(crate) struct Bootstrap {
    pub frame: u32,
    pub depth: u32,
    pub nodes: u64,
    pub allocation: u64,
    pub state: u64,
    pub effect: u64,
    pub source: u64,
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
        }
    }
}

pub(crate) fn read_frame(
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

pub(crate) fn write_frame(
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

/// Called only in a freshly exec'd entry, before any host initialization.
#[expect(
    unsafe_code,
    reason = "early exec entry takes exclusive ownership of the inherited socket"
)]
pub(crate) unsafe fn inherited_pipe(fd: i32) -> Result<UnixStream, PoolError> {
    if fd < 3 {
        return Err(PoolError::protocol("IPC descriptor must be above stdio"));
    }
    let mut socket_type: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let valid = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if valid != 0 || socket_type != libc::SOCK_STREAM {
        return Err(PoolError::protocol(
            "IPC descriptor is not a valid stream socket",
        ));
    }
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0 {
        return Err(PoolError::io(std::io::Error::last_os_error()));
    }
    #[cfg(target_os = "linux")]
    {
        // close_range avoids a guessed descriptor ceiling, including handles
        // above a subsequently lowered RLIMIT_NOFILE. Linux 5.9 or newer.
        // SAFETY: the entry owns its process; only fd is preserved.
        let first = unsafe { libc::syscall(libc::SYS_close_range, 0_u32, (fd - 1) as u32, 0_u32) };
        let second =
            unsafe { libc::syscall(libc::SYS_close_range, (fd + 1) as u32, u32::MAX, 0_u32) };
        if first != 0 || second != 0 {
            return Err(PoolError::io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(target_os = "macos")]
    {
        // PROC_PIDLISTFDS enumerates actual open handles, including descriptors
        // above a lowered RLIMIT_NOFILE. The early entry has no host threads.
        let pid = unsafe { libc::getpid() };
        let size =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if size <= 0 {
            return Err(PoolError::io(std::io::Error::last_os_error()));
        }
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        let mut slots = (size as usize / width).saturating_add(16);
        loop {
            let mut handles: Vec<libc::proc_fdinfo> = Vec::with_capacity(slots);
            let bytes = slots
                .checked_mul(width)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or(PoolError::InvalidConfiguration)?;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    handles.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if read <= 0 || read as usize % width != 0 {
                return Err(PoolError::protocol(
                    "cannot enumerate inherited descriptors",
                ));
            }
            if read == bytes {
                slots = slots
                    .checked_mul(2)
                    .ok_or(PoolError::InvalidConfiguration)?;
                continue;
            }
            unsafe {
                handles.set_len(read as usize / width);
            }
            for handle in handles {
                if handle.proc_fd != fd {
                    unsafe {
                        libc::close(handle.proc_fd);
                    }
                }
            }
            break;
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(PoolError::UnsupportedPlatform);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}
