//! A process-wide Linux `instructions:u` counter.
//!
//! Opened before the measured runtime starts, it inherits into every thread
//! created afterwards, so a read sums the user-space instructions of the
//! runtime's workers and blocking threads. Reads are cumulative; a span's
//! cost is the difference of two reads.
#![expect(unsafe_code, reason = "Linux perf_event_open, ioctl and read")]

use anyhow::{Result, ensure};

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

impl InstructionCounter {
    /// Open and enable the counter. Threads created before this call are not counted.
    ///
    /// # Errors
    /// Returns an error when the kernel refuses the counter (paranoid level, no PMU).
    pub fn open_inherited() -> Result<Self> {
        let attr = PerfEventAttr {
            size: 64,
            config: 1, // PERF_COUNT_HW_INSTRUCTIONS
            // inherit, exclude kernel, exclude hypervisor; enabled at open.
            flags: (1 << 1) | (1 << 5) | (1 << 6),
            ..Default::default()
        };
        // SAFETY: `attr` is a valid, fully initialized perf_event_attr prefix of `size` bytes.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0u64) };
        ensure!(
            fd >= 0,
            "perf_event_open: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(i32::try_from(fd)?))
    }

    /// Instructions counted since the counter opened, inherited threads included.
    ///
    /// # Errors
    /// Returns an error when the read fails.
    pub fn read(&self) -> Result<u64> {
        let mut instructions = 0u64;
        // SAFETY: reads exactly eight bytes into a live u64.
        let read = unsafe { libc::read(self.0, (&raw mut instructions).cast(), 8) };
        ensure!(read == 8, "read instructions counter");
        Ok(instructions)
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
