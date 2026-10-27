//! A process-wide Linux `instructions:u` counter.
//!
//! Opened before the measured runtime starts, it inherits into every thread
//! created afterwards, so a read sums the user-space instructions of the
//! runtime's workers and blocking threads. Reads are cumulative; a span's
//! cost is the difference of two reads.
#![expect(unsafe_code, reason = "Linux perf_event_open, ioctl and read")]

use std::io;

type Result<T> = std::result::Result<T, io::Error>;

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{message}: {}",
            io::Error::last_os_error()
        )))
    }
}

#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

/// `instructions:u` for this thread and every thread it creates after opening.
pub struct InstructionCounter(i32);

/// One unscaled hardware counter snapshot, including PMU scheduling time.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
#[repr(C)]
pub struct InstructionReading {
    pub instructions: u64,
    pub time_enabled: u64,
    pub time_running: u64,
}
impl InstructionReading {
    /// Qualify one interval. Counts are never silently scaled for multiplexing.
    pub fn since(self, before: Self) -> Result<Self> {
        let subtract = |after: u64, before: u64| {
            after
                .checked_sub(before)
                .ok_or_else(|| io::Error::other("PMU counter moved backwards"))
        };
        Self {
            instructions: subtract(self.instructions, before.instructions)?,
            time_enabled: subtract(self.time_enabled, before.time_enabled)?,
            time_running: subtract(self.time_running, before.time_running)?,
        }
        .qualify()
    }
    /// Require a positive, unscaled interval with at least 99% PMU running time.
    pub fn qualify(self) -> Result<Self> {
        if self.instructions == 0
            || self.time_enabled == 0
            || self.time_running > self.time_enabled
            || self.running_fraction() < 0.99
        {
            return Err(io::Error::other(format!(
                "unqualified instructions:u: {self:?}; requires >=99% time running"
            )));
        }
        Ok(self)
    }
    pub fn running_fraction(self) -> f64 {
        self.time_running as f64 / self.time_enabled as f64
    }
}

impl InstructionCounter {
    /// Open and enable the counter. Threads created before this call are not counted.
    ///
    /// # Errors
    /// Returns an error when the kernel refuses the counter (paranoid level, no PMU).
    pub fn open_inherited() -> Result<Self> {
        Self::open(false)
    }

    /// Count subsequently created threads without also counting child processes.
    pub fn open_threads() -> Result<Self> {
        Self::open(true)
    }

    fn open(threads_only: bool) -> Result<Self> {
        let attr = PerfEventAttr {
            size: 64,
            config: 1,      // PERF_COUNT_HW_INSTRUCTIONS
            read_format: 3, // PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING
            // inherit, exclude kernel, exclude hypervisor; enabled at open.
            flags: (1 << 1) | (1 << 5) | (1 << 6) | if threads_only { 1 << 35 } else { 0 },
            ..Default::default()
        };
        // SAFETY: `attr` is a valid, fully initialized perf_event_attr prefix of `size` bytes.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 8u64) };
        require(fd >= 0, "perf_event_open instructions:u")?;
        Ok(Self(fd as i32))
    }

    /// Instructions counted since the counter opened, inherited threads included.
    ///
    /// # Errors
    /// Returns an error when the read fails.
    pub fn read(&self) -> Result<u64> {
        Ok(self.snapshot()?.instructions)
    }

    /// Read count and scheduling times together.
    pub fn snapshot(&self) -> Result<InstructionReading> {
        self.try_snapshot().ok_or_else(|| {
            io::Error::other(format!(
                "read instructions counter and scheduling times: {}",
                io::Error::last_os_error()
            ))
        })
    }

    /// A signal handler must not format or allocate an error on a failed read.
    pub(super) fn try_snapshot(&self) -> Option<InstructionReading> {
        let mut reading = InstructionReading::default();
        let size = std::mem::size_of::<InstructionReading>();
        // SAFETY: the kernel writes the three u64 fields selected by read_format.
        let read = unsafe { libc::read(self.0, (&raw mut reading).cast(), size) };
        (read == size as isize).then_some(reading)
    }
}

impl Drop for InstructionCounter {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned by this counter and closed once.
        unsafe {
            libc::close(self.0);
        }
    }
}
