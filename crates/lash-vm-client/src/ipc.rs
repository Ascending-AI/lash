#[cfg(not(target_os = "linux"))]
compile_error!("Lash VM workers require Linux");

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{PoolConfig, PoolError};
use lash_vm_protocol::{
    FRAME_HEADER_BYTES, FrameCodec, InfrastructureOutcome, ParentFrame, PayloadKind, SequenceFault,
    WorkerFrame, WorkerMessage, WorkerPhase,
};

/// Counters for one measured exchange, used only by the measured transport specialization.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExchangeTiming {
    pub response_started_ns: u64,
    pub response_decode_ns: u64,
    pub parent_decode_ns: u64,
    pub parent_encode_ns: u64,
    pub write_ns: u64,
    pub read_wait_ns: u64,
    pub worker_decode_ns: u64,
    pub worker_encode_ns: u64,
    pub guest_ns: u64,
    pub worker_samples: usize,
}

pub struct Worker {
    pub(crate) exchange_timing: ExchangeTiming,
    child: Child,
    reaped: bool,
    pub(crate) measurements: Option<crate::measurements::SharedMeasurements>,
    pub(crate) process_epoch: Option<String>,
    pub(crate) used: bool,
    inbound: FrameSource,
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
            .arg(
                serde_json::to_string(&Bootstrap::from(config))
                    .map_err(|error| PoolError::payload(PayloadKind::Bootstrap, error))?,
            )
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
        #[cfg(feature = "perf-witness")]
        lash_core_execution::perf_witness::startup::record(
            lash_core_execution::perf_witness::startup::Phase::VmSpawnStarted,
        );
        let child = command
            .spawn()
            .map_err(|error| PoolError::spawn(error, &config.entry.executable))?;
        #[cfg(feature = "perf-witness")]
        lash_core_execution::perf_witness::startup::record_subject(
            lash_core_execution::perf_witness::startup::Phase::VmSpawned,
            child.id(),
        );
        drop(child_pipe);
        #[cfg(target_os = "linux")]
        let process_epoch = std::fs::read_to_string(format!("/proc/{}/stat", child.id()))
            .ok()
            .and_then(|text| {
                text[text.rfind(')')? + 2..]
                    .split_whitespace()
                    .nth(19)
                    .map(str::to_owned)
            });
        #[cfg(not(target_os = "linux"))]
        let process_epoch = None;
        Ok(Self {
            child,
            exchange_timing: ExchangeTiming::default(),
            reaped: false,
            measurements: None,
            process_epoch,
            used: false,
            inbound: FrameSource::with_capacity(config.tuning.inbound_buffer_bytes),
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
        self.send_encoded_measured::<false>(&bytes, timeout)
    }

    /// Sends a frame this worker's codec has already encoded.
    pub(crate) fn send_encoded_measured<const MEASURE: bool>(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), PoolError> {
        let measured = MEASURE.then(Instant::now);
        write_frame(&mut self.pipe, bytes, Instant::now() + timeout)?;
        if let Some(measured) = measured {
            self.exchange_timing.write_ns += measured.elapsed().as_nanos() as u64;
        }
        if let Some(measurements) = &self.measurements {
            let mut measurements = measurements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            measurements.counters.ipc_sent_messages += 1;
            measurements.counters.ipc_sent_bytes += bytes.len() as u64;
        }
        Ok(())
    }

    pub fn receive(&mut self, deadline: Instant) -> Result<WorkerFrame, PoolError> {
        self.receive_measured::<false>(deadline)
    }

    pub(crate) fn receive_measured<const MEASURE: bool>(
        &mut self,
        deadline: Instant,
    ) -> Result<WorkerFrame, PoolError> {
        let measured = MEASURE.then(Instant::now);
        let bytes = match self
            .inbound
            .read_frame(&mut self.pipe, &self.codec, deadline)
        {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                if let Some(measurements) = &self.measurements {
                    measurements
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .counters
                        .crashes += 1;
                }
                return Err(InfrastructureOutcome::WorkerCrashed {
                    evidence: self.exit_evidence(),
                }
                .into());
            }
            result => result?,
        };
        if let Some(measurements) = &self.measurements {
            let mut measurements = measurements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            measurements.counters.ipc_received_messages += 1;
            measurements.counters.ipc_received_bytes += bytes.len() as u64;
        }
        if let Some(measured) = measured {
            self.exchange_timing.read_wait_ns += measured.elapsed().as_nanos() as u64;
        }
        let measured = MEASURE.then(Instant::now);
        let result = self.codec.decode_worker(&bytes).map_err(PoolError::from);
        if let Some(measured) = measured {
            let decode_ns = measured.elapsed().as_nanos() as u64;
            self.exchange_timing.parent_decode_ns += decode_ns;
            if matches!(
                &result,
                Ok(WorkerFrame {
                    message: WorkerMessage::Progress {
                        phase: WorkerPhase::Serializing,
                        ..
                    },
                    ..
                })
            ) {
                self.exchange_timing.response_decode_ns = 0;
            }
            self.exchange_timing.response_decode_ns += decode_ns;
        }
        result
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
    pub serialization: Duration,
    pub tuning: crate::WorkerTuning,
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
            serialization: c.deadlines.serialization,
            tuning: c.tuning,
            cpu_nanos: c
                .deadlines
                .cumulative_cpu
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
        }
    }
}

thread_local! {
    static SOCKET_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The system calls this thread has made on worker sockets: every read,
/// write and timeout change. A count, so a law can bound what one exchange
/// costs without timing it (FIG-4433).
pub fn socket_calls() -> u64 {
    SOCKET_CALLS.with(std::cell::Cell::get)
}

fn count_socket_call() {
    SOCKET_CALLS.with(|calls| calls.set(calls.get() + 1));
}

/// What one read asks the socket for. Larger frames finish in their own
/// allocation.
const INBOUND_BUFFER_BYTES: usize = 16 * 1024;

/// One end's inbound frames (FIG-4433). A read takes whatever has arrived,
/// so a frame's header and payload, and frames written together, cost one
/// read between them. Every reader of a socket must share its source: bytes
/// one reader took past its own frame are the next reader's.
#[derive(Debug, Default)]
pub struct FrameSource {
    buffer: Vec<u8>,
    /// The unread bytes are `buffer[start..end]`.
    start: usize,
    end: usize,
    capacity: Option<std::num::NonZeroUsize>,
}

impl FrameSource {
    pub fn with_capacity(capacity: std::num::NonZeroUsize) -> Self {
        Self {
            capacity: Some(capacity),
            ..Self::default()
        }
    }

    /// The next whole frame. The deadline bounds the wait for all of it, and
    /// a deadline already past refuses a frame that has already arrived.
    pub fn read_frame(
        &mut self,
        pipe: &mut UnixStream,
        codec: &FrameCodec,
        deadline: Instant,
    ) -> Result<Vec<u8>, PoolError> {
        remaining(deadline)?;
        if self.buffer.is_empty() {
            self.buffer = vec![
                0;
                self.capacity
                    .map_or(INBOUND_BUFFER_BYTES, std::num::NonZeroUsize::get)
                    .max(FRAME_HEADER_BYTES)
            ];
        }
        while self.end - self.start < FRAME_HEADER_BYTES {
            if self.start > 0 {
                self.buffer.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            let partial = self.end > 0;
            let read = read_some(pipe, &mut self.buffer[self.end..], deadline, partial)?;
            self.end += read;
        }
        let unread = &self.buffer[self.start..self.end];
        let len = codec
            .frame_len(unread)
            .map_err(PoolError::from)?
            .ok_or_else(|| PoolError::breach(SequenceFault::IncompleteFrameHeader))?;
        if len <= unread.len() {
            let frame = unread[..len].to_vec();
            self.start += len;
            if self.start == self.end {
                self.start = 0;
                self.end = 0;
            }
            return Ok(frame);
        }
        // The rest is read into the frame itself, and no further.
        let mut frame = Vec::with_capacity(len);
        frame.extend_from_slice(unread);
        let arrived = frame.len();
        self.start = 0;
        self.end = 0;
        frame.resize(len, 0);
        read_until(pipe, &mut frame[arrived..], deadline, true)?;
        Ok(frame)
    }
}

/// Reads one frame from a socket no [`FrameSource`] reads.
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
        .ok_or_else(|| PoolError::breach(SequenceFault::IncompleteFrameHeader))?;
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

/// One read of at least one byte, within the deadline.
fn read_some(
    pipe: &mut UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
    partial: bool,
) -> Result<usize, PoolError> {
    loop {
        pipe.set_read_timeout(Some(remaining(deadline)?))
            .map_err(PoolError::io)?;
        count_socket_call();
        count_socket_call();
        match pipe.read(bytes) {
            Ok(0) if partial => return Err(PoolError::breach(SequenceFault::EndInPartialFrame)),
            Ok(0) => return Err(PoolError::eof()),
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(PoolError::io(e)),
        }
    }
}

fn read_until(
    pipe: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
    mut partial: bool,
) -> Result<(), PoolError> {
    while !bytes.is_empty() {
        let read = read_some(pipe, bytes, deadline, partial)?;
        bytes = &mut bytes[read..];
        partial = true;
    }
    Ok(())
}

/// Writes without waiting, for as much as the socket takes at once.
fn write_ready(pipe: &UnixStream, bytes: &[u8]) -> std::io::Result<usize> {
    // Linux send flags keep the write nonblocking and suppress SIGPIPE.
    let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    count_socket_call();
    // SAFETY: the pointer and length describe `bytes`, which outlives the
    // call, and the descriptor is this open stream's.
    #[expect(unsafe_code, reason = "a write that does not wait needs send(2) flags")]
    let sent = unsafe { libc::send(pipe.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), flags) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sent as usize)
}

pub fn write_frame(
    pipe: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), PoolError> {
    while !bytes.is_empty() {
        let wait = remaining(deadline)?;
        // A frame the socket has room for needs no timeout (FIG-4433): only
        // a write that has to wait arms one.
        let written = match write_ready(pipe, bytes) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                pipe.set_write_timeout(Some(wait)).map_err(PoolError::io)?;
                count_socket_call();
                count_socket_call();
                pipe.write(bytes)
            }
            written => written,
        };
        match written {
            Ok(0) => return Err(PoolError::eof()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(PoolError::io(e)),
        }
    }
    Ok(())
}

/// Writes a small frame and the one after it together, so the peer wakes
/// once for both. A larger second frame is written on its own instead of
/// being copied. Each write has `timeout`.
pub fn write_frames(
    pipe: &mut UnixStream,
    mut first: Vec<u8>,
    second: &[u8],
    timeout: Duration,
) -> Result<(), PoolError> {
    if second.len() > INBOUND_BUFFER_BYTES {
        write_frame(pipe, &first, Instant::now() + timeout)?;
        return write_frame(pipe, second, Instant::now() + timeout);
    }
    first.extend_from_slice(second);
    write_frame(pipe, &first, Instant::now() + timeout)
}

/// Shared-machine monotonic clock for measured parent/worker intervals.
/// Ordinary transport specializations never call it.
pub fn monotonic_nanos() -> Result<u64, PoolError> {
    let mut clock = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock points to an initialized timespec owned by this call.
    #[expect(
        unsafe_code,
        reason = "measurement reads the kernel's cross-process monotonic clock"
    )]
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) };
    if result != 0 {
        return Err(PoolError::io(std::io::Error::last_os_error()));
    }
    Ok(clock.tv_sec as u64 * 1_000_000_000 + clock.tv_nsec as u64)
}
