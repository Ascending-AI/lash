#[cfg(not(target_os = "linux"))]
compile_error!("Lash VM workers require Linux");

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{PoolConfig, PoolError};
use lash_vm_protocol::{
    FRAME_HEADER_BYTES, FrameCodec, InfrastructureOutcome, ParentFrame, PayloadKind, SequenceFault,
    WorkerFrame,
};

pub struct Worker {
    child: Child,
    reaped: bool,
    pub(crate) measurements: Option<crate::measurements::SharedMeasurements>,
    pub(crate) process_epoch: Option<String>,
    pub(crate) used: bool,
    pub(crate) phases: Option<crate::phases::Scope>,
    inbound: FrameSource,
    pub pipe: UnixStream,
    pub codec: FrameCodec,
    pub cpu_nanos: u64,
    /// How long a worker whose stream ended has to report its exit.
    exit_grace: Duration,
    /// The execution's CPU budget, from which the worker sets its own CPU
    /// ceiling, and the limit a run meets past it.
    cpu_budget: Duration,
    cpu_limit: lash_vm_protocol::WorkerLimit,
    /// The worker's own cgroup, when its host places workers in one;
    /// removed after the worker is reaped.
    cgroup: Option<crate::cgroup::Placement>,
}

impl Worker {
    #[expect(
        clippy::disallowed_methods,
        reason = "supervisor launches only the explicitly configured credential-free entry"
    )]
    pub fn spawn(config: &PoolConfig) -> Result<Self, PoolError> {
        let (pipe, child_pipe) = UnixStream::pair().map_err(PoolError::io)?;
        let fd = child_pipe.as_raw_fd();
        let cgroup = config
            .cgroup
            .as_ref()
            .map(crate::WorkerCgroup::place)
            .transpose()
            .map_err(PoolError::cgroup)?;
        let procs = cgroup.as_ref().and_then(crate::cgroup::Placement::procs);
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
        // SAFETY: the post-fork closure only calls async-signal-safe fcntl
        // and write. child_pipe and the cgroup's descriptor stay owned until
        // spawn has returned. The entry closes every other descriptor
        // immediately after exec, including stdio.
        #[expect(unsafe_code, reason = "the one IPC descriptor must survive exec")]
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The worker enters its cgroup before it execs, so all it
                // allocates is charged there.
                procs.map_or(Ok(()), crate::cgroup::enter)
            });
        }
        #[cfg(feature = "perf-witness")]
        lash_core_execution::perf_witness::startup::record(
            lash_core_execution::perf_witness::startup::Phase::VmSpawnStarted,
        );
        let child = command.spawn().map_err(|error| {
            crate::cgroup::move_refusal(&error).map_or_else(
                || PoolError::spawn(error, &config.entry.executable),
                PoolError::cgroup,
            )
        })?;
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
            reaped: false,
            measurements: None,
            process_epoch,
            used: false,
            phases: None,
            inbound: FrameSource::with_capacity(config.tuning.inbound_buffer_bytes),
            pipe,
            codec: FrameCodec::new(config.protocol.decode)
                .with_detail_limit(config.tuning.diagnostic_bytes),
            cpu_nanos: 0,
            exit_grace: config.deadlines.cancel_grace,
            cpu_budget: config.deadlines.cumulative_cpu,
            cpu_limit: config.deadlines.cpu_limit(),
            cgroup,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, frame: &ParentFrame, timeout: Duration) -> Result<(), PoolError> {
        let bytes = self.codec.encode_parent(frame).map_err(PoolError::from)?;
        self.send_encoded(
            &bytes,
            crate::measurements::IpcTraffic::parent_kind(&frame.message),
            timeout,
        )
    }

    /// Sends a frame this worker's codec has already encoded.
    pub(crate) fn send_encoded(
        &mut self,
        bytes: &[u8],
        kind: usize,
        timeout: Duration,
    ) -> Result<(), PoolError> {
        let observed = self.phases.as_ref().map(|scope| scope.recorder.now());
        let started = Instant::now();
        match write_frame(&mut self.pipe, bytes, started + timeout) {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                return Err(self.crashed());
            }
            result => result.map_err(|error| error.with_silence(started.elapsed()))?,
        }
        if let (Some(scope), Some(start)) = (&self.phases, observed) {
            scope.frame(
                "send",
                start,
                Some("sent"),
                Some(crate::IpcTraffic::SENT_KINDS[kind]),
                Some(bytes.len()),
            );
        }
        if let Some(measurements) = &self.measurements {
            let mut measurements = measurements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            measurements.counters.peak_buffer_bytes = measurements
                .counters
                .peak_buffer_bytes
                .max((bytes.len() + self.inbound.buffer_capacity()) as u64);
            measurements.counters.ipc_sent_messages += 1;
            measurements.counters.ipc_sent_bytes += bytes.len() as u64;
            measurements.counters.ipc.sent(kind, bytes.len());
        }
        Ok(())
    }

    pub fn receive(&mut self, deadline: Instant) -> Result<WorkerFrame, PoolError> {
        let observed = self.phases.as_ref().map(|scope| scope.recorder.now());
        let started = Instant::now();
        let bytes = match self
            .inbound
            .read_frame(&mut self.pipe, &self.codec, deadline)
        {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                return Err(self.crashed());
            }
            result => result.map_err(|error| error.with_silence(started.elapsed()))?,
        };
        let frame = decode_measured(
            &self.codec,
            &bytes,
            bytes.capacity() + self.inbound.buffer_capacity(),
            self.measurements.as_ref(),
        );
        if let (Some(scope), Some(start)) = (&mut self.phases, observed) {
            if let Ok(WorkerFrame {
                message: lash_vm_protocol::WorkerMessage::HostRead { id, .. },
                ..
            }) = &frame
            {
                scope.effect = Some(id.0);
            }
            let kind =
                crate::IpcTraffic::worker_kind(frame.as_ref().ok().map(|frame| &frame.message));
            scope.frame(
                "receive",
                start,
                Some("received"),
                Some(crate::IpcTraffic::RECEIVED_KINDS[kind]),
                Some(bytes.len()),
            );
        }
        frame
    }

    /// The failure of an exchange that met the end of this worker's
    /// stream, writing or reading: whichever it met first, the evidence
    /// names how the worker ended (FIG-6823).
    fn crashed(&mut self) -> PoolError {
        if let Some(measurements) = &self.measurements {
            measurements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .counters
                .crashes += 1;
        }
        self.lost().into()
    }

    /// What the end of this worker's stream is to its run. The worker sets
    /// its own CPU ceiling from the execution's CPU budget, and the kernel
    /// stops it there with `SIGXCPU`: a worker so stopped, with that budget
    /// spent, met the run's CPU limit, which the same run meets again. A
    /// worker the kernel's OOM killer killed in the cgroup its host placed
    /// it in met the run's memory limit, likewise (FIG-6787). Any other
    /// end, a `SIGXCPU` lash did not cause among them, is a crash
    /// (FIG-6523).
    fn lost(&mut self) -> InfrastructureOutcome {
        let evidence = self.exit_evidence();
        if evidence
            == (lash_vm_protocol::SupervisorEvidence::Signalled {
                signal: libc::SIGKILL,
            })
            && let Some(cgroup) = self.cgroup.as_ref().filter(|cgroup| cgroup.oom_killed())
        {
            return InfrastructureOutcome::WorkerLimitExceeded {
                limit: lash_vm_protocol::WorkerLimit::Memory {
                    bound_bytes: cgroup.memory_max_bytes(),
                },
            };
        }
        if evidence
            == (lash_vm_protocol::SupervisorEvidence::Signalled {
                signal: libc::SIGXCPU,
            })
            && Duration::from_nanos(self.terminate()) >= self.cpu_budget
        {
            return InfrastructureOutcome::WorkerLimitExceeded {
                limit: self.cpu_limit,
            };
        }
        InfrastructureOutcome::WorkerCrashed { evidence }
    }

    fn exit_evidence(&self) -> lash_vm_protocol::SupervisorEvidence {
        self.await_exit();
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
            libc::CLD_KILLED | libc::CLD_DUMPED if status == libc::SIGSYS => {
                lash_vm_protocol::SupervisorEvidence::ForbiddenSyscall
            }
            libc::CLD_KILLED | libc::CLD_DUMPED => {
                lash_vm_protocol::SupervisorEvidence::Signalled { signal: status }
            }
            _ => lash_vm_protocol::SupervisorEvidence::EndOfStream,
        }
    }

    /// A worker whose stream ended is exiting, but the kernel closes its
    /// descriptors before it reports the exit. Waits for that report, for
    /// at most the cancel grace, so the evidence names how the worker ended.
    fn await_exit(&self) {
        #[expect(
            unsafe_code,
            reason = "pidfd_open only names this owned, unreaped child"
        )]
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, self.child.id(), 0) };
        let Ok(pidfd) = i32::try_from(pidfd) else {
            return;
        };
        if pidfd < 0 {
            return;
        }
        // SAFETY: pidfd_open returned this new descriptor, owned only here.
        #[expect(unsafe_code, reason = "the new pidfd is owned and closed here")]
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) };
        let mut exit = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = i32::try_from(self.exit_grace.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one pollfd in writable storage for the duration of the call.
        #[expect(unsafe_code, reason = "poll waits on the owned pidfd")]
        unsafe {
            libc::poll(&mut exit, 1, wait)
        };
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
    pub confinement: crate::WorkerConfinement,
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
            confinement: c.confinement,
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

    pub fn buffer_capacity(&self) -> usize {
        self.buffer.capacity()
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

// The existing decoder is also the receive counter producer. A refused frame
// still belongs to the aggregate and to the explicit undecodable bin.
fn decode_measured(
    codec: &FrameCodec,
    bytes: &[u8],
    peak_buffers: usize,
    measurements: Option<&crate::measurements::SharedMeasurements>,
) -> Result<WorkerFrame, PoolError> {
    let frame = codec.decode_worker(bytes).map_err(PoolError::from);
    if let Some(measurements) = measurements {
        let mut measurements = measurements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        measurements
            .counters
            .ipc
            .received(frame.as_ref().ok().map(|f| &f.message), bytes.len());
        measurements.counters.peak_buffer_bytes = measurements
            .counters
            .peak_buffer_bytes
            .max(peak_buffers as u64);
        measurements.counters.ipc_received_messages += 1;
        measurements.counters.ipc_received_bytes += bytes.len() as u64;
    }
    frame
}

#[cfg(test)]
mod traffic_tests {
    use super::*;
    use crate::measurements::{IpcTraffic, Measurements};
    use lash_vm_protocol::{
        DecodeLimits, EncodedPayload, ExecutionLease, FrameEpoch, MessageFence, OwnerEpoch,
        ParentMessage, WorkerMessage,
    };
    use std::sync::{Arc, Mutex};

    /// TRAFFIC-CONSERVATION: socket bundling and frame splitting cannot change
    /// per-kind bytes or frame counts; malformed whole frames remain explicit.
    #[test]
    fn traffic_conservation_bundled_and_split_frames() {
        let codec = FrameCodec::new(DecodeLimits::standard());
        let mut fence = MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1));
        let measurements = Arc::new(Mutex::new(Measurements::default()));
        for capacity in [16 * 1024, FRAME_HEADER_BYTES] {
            let (mut tx, mut rx) = UnixStream::pair().unwrap();
            let messages = [
                WorkerMessage::Started,
                WorkerMessage::Delivered,
                WorkerMessage::Printed {
                    payload: EncodedPayload(vec![0; 32 * 1024]),
                },
            ];
            let frames: Vec<_> = messages
                .iter()
                .map(|message| {
                    codec
                        .encode_worker(&WorkerFrame {
                            header: fence.next_header(),
                            message: message.clone(),
                        })
                        .unwrap()
                })
                .collect();
            let bytes: Vec<_> = frames.iter().flatten().copied().collect();
            let writer = std::thread::spawn(move || tx.write_all(&bytes).unwrap());
            let mut source =
                FrameSource::with_capacity(std::num::NonZeroUsize::new(capacity).unwrap());
            let before = measurements.lock().unwrap().counters;
            for expected in &messages {
                let bytes = source
                    .read_frame(&mut rx, &codec, Instant::now() + Duration::from_secs(10))
                    .unwrap();
                let frame = decode_measured(
                    &codec,
                    &bytes,
                    bytes.capacity() + source.buffer_capacity(),
                    Some(&measurements),
                )
                .unwrap();
                assert_eq!(&frame.message, expected);
            }
            writer.join().unwrap();
            let after = measurements.lock().unwrap().counters;
            let traffic = after.ipc.difference(&before.ipc).unwrap();
            assert_eq!(
                traffic.totals().frames,
                after.ipc_received_messages - before.ipc_received_messages
            );
            assert_eq!(
                traffic.totals().bytes,
                after.ipc_received_bytes - before.ipc_received_bytes
            );
            assert_eq!(traffic.received[4].frames, 1);
            assert_eq!(traffic.received[11].frames, 1);
            assert_eq!(traffic.received[7].bytes, frames[2].len() as u64);
        }
        let mut traffic = IpcTraffic::default();
        for message in [ParentMessage::Cancel, ParentMessage::Reset] {
            let bytes = codec
                .encode_parent(&ParentFrame {
                    header: fence.next_header(),
                    message: message.clone(),
                })
                .unwrap();
            traffic.sent(IpcTraffic::parent_kind(&message), bytes.len());
        }
        assert_eq!(traffic.totals().frames, 2);
        assert_eq!(traffic.sent[7].frames, 1);
        assert_eq!(traffic.sent[8].frames, 1);
        // The borrowed Run producer bundles a delivery; the split producer
        // sends Deliver and Run separately. Count the actual wire envelopes.
        use lash_vm_protocol::{BorrowedParentFrame, BorrowedParentMessage, Delivery};
        let deliveries = [Delivery {
            wait: lash_kernel_doc::testkit::wait_id(1),
            outcome: EncodedPayload(vec![7; 8]),
        }];
        let bundled = codec
            .encode_borrowed(&BorrowedParentFrame {
                header: fence.next_header(),
                message: BorrowedParentMessage::Run {
                    start: None,
                    deliveries: &deliveries,
                    slice: 10,
                    cancel: false,
                    sealed: &[],
                },
            })
            .unwrap();
        let mut bundled_traffic = IpcTraffic::default();
        bundled_traffic.sent(IpcTraffic::RUN_KIND, bundled.len());
        assert_eq!(bundled_traffic.totals().frames, 1);
        assert_eq!(bundled_traffic.totals().bytes, bundled.len() as u64);
        let mut split_traffic = IpcTraffic::default();
        let mut split_bytes = 0;
        for message in [
            ParentMessage::Deliver {
                wait: lash_kernel_doc::testkit::wait_id(1),
                outcome: deliveries[0].outcome.clone(),
            },
            ParentMessage::Run {
                start: None,
                deliveries: vec![],
                slice: 10,
                cancel: false,
                sealed: vec![],
            },
        ] {
            let bytes = codec
                .encode_parent(&ParentFrame {
                    header: fence.next_header(),
                    message: message.clone(),
                })
                .unwrap();
            split_bytes += bytes.len() as u64;
            split_traffic.sent(IpcTraffic::parent_kind(&message), bytes.len());
        }
        assert_eq!(split_traffic.totals().frames, 2);
        assert_eq!(split_traffic.totals().bytes, split_bytes);
        assert_eq!(split_traffic.sent[3].frames, 1);
        assert_eq!(split_traffic.sent[IpcTraffic::RUN_KIND].frames, 1);
        let before = measurements.lock().unwrap().counters;
        let invalid = vec![0; FRAME_HEADER_BYTES];
        assert!(decode_measured(&codec, &invalid, 0, Some(&measurements)).is_err());
        let after = measurements.lock().unwrap().counters;
        assert_eq!(
            after.ipc_received_messages,
            before.ipc_received_messages + 1
        );
        assert_eq!(
            after.ipc_received_bytes,
            before.ipc_received_bytes + invalid.len() as u64
        );
        assert!(after.ipc.difference(&before.ipc).is_none());
        assert_eq!(after.ipc.totals().bytes, after.ipc_received_bytes);
    }
}

#[cfg(test)]
mod loss_tests {
    use super::*;
    use lash_vm_protocol::{
        ExecutionLease, FrameEpoch, MessageFence, OwnerEpoch, ParentMessage, SupervisorEvidence,
    };

    /// FIG-6823: a worker killed from outside is named by its signal when
    /// the parent meets its end writing to it, as when reading from it. A
    /// write after the kill finds the stream closed before the worker's
    /// exit is observed; the evidence still waits for that exit.
    #[test]
    fn a_worker_killed_from_outside_is_named_by_its_signal_on_a_write() {
        // A stand-in worker that holds its end of the stream and waits.
        let mut entry = crate::WorkerEntry::helper("/bin/sh");
        entry.args = vec!["-c".to_owned(), "kill -STOP $$".to_owned(), "sh".to_owned()];
        let mut worker = Worker::spawn(&PoolConfig::standard(entry)).expect("spawn the worker");
        let pid = i32::try_from(worker.pid()).expect("a pid");
        #[expect(unsafe_code, reason = "the law kills its own unreaped child")]
        let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(killed, 0, "kill the worker from outside");
        // Wait for the exit without reaping it, so the stream is closed.
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        #[expect(unsafe_code, reason = "waitid observes the unreaped child")]
        let exited = unsafe {
            libc::waitid(
                libc::P_PID,
                worker.pid(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(exited, 0, "the worker exited");
        let mut fence = MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1));
        let sent = worker.send(
            &ParentFrame {
                header: fence.next_header(),
                message: ParentMessage::Cancel,
            },
            Duration::from_secs(5),
        );
        assert!(
            matches!(
                sent,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerCrashed {
                        evidence: SupervisorEvidence::Signalled {
                            signal: libc::SIGKILL
                        }
                    }
                ))
            ),
            "the write names the signal that ended the worker: {sent:?}"
        );
    }
}
