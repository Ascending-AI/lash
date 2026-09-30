use std::path::PathBuf;
use std::time::Duration;

use crate::{PoolError, build_identity};
use lash_vm_protocol::{BuildIdentity, ProtocolBounds, VmLimits};

/// An explicit helper executable, or the host executable with an early entry.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub executable: PathBuf,
    pub args: Vec<String>,
    /// The entry's own compiled identity, never an identity echoed from argv.
    pub build: BuildIdentity,
}

impl WorkerEntry {
    pub fn helper(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            build: build_identity(),
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "host explicitly selects its own early re-exec entry"
    )]
    pub fn reexec(build: BuildIdentity) -> Result<Self, PoolError> {
        Ok(Self {
            executable: std::env::current_exe().map_err(PoolError::io)?,
            args: Vec::new(),
            build,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    pub checkout: Duration,
    pub compute: Duration,
    pub serialization: Duration,
    pub cancel_grace: Duration,
    pub cumulative_cpu: Duration,
    pub max_attempts: u32,
}

impl Deadlines {
    /// CPU and execution deadlines are provisional until the integrated
    /// FIG-4162 benchmark. IPC silence uses the measured FIG-4157 preset.
    pub const fn standard() -> Self {
        Self {
            checkout: Duration::from_secs(5),
            compute: Duration::from_secs(30),
            serialization: Duration::from_secs(5),
            cancel_grace: Duration::from_millis(100),
            cumulative_cpu: Duration::from_secs(10),
            max_attempts: 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub entry: WorkerEntry,
    pub min_workers: usize,
    pub max_workers: usize,
    pub max_queue_items: usize,
    pub max_queue_bytes: usize,
    pub protocol: ProtocolBounds,
    pub vm_limits: VmLimits,
    pub deadlines: Deadlines,
    pub restart_window: Duration,
    pub max_restarts: usize,
}

impl PoolConfig {
    pub fn standard(entry: WorkerEntry) -> Self {
        Self {
            entry,
            min_workers: 1,
            max_workers: 4,
            max_queue_items: 2,
            max_queue_bytes: 8 * 1024 * 1024,
            protocol: ProtocolBounds::standard(),
            vm_limits: VmLimits {
                instruction_budget: Some(50_000_000),
                memory_limit_bytes: Some(64 * 1024 * 1024),
                max_frame_depth: 1024,
            },
            deadlines: Deadlines::standard(),
            restart_window: Duration::from_secs(60),
            max_restarts: 8,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), PoolError> {
        if self.protocol.decode.max_frame_bytes < lash_vm_protocol::FRAME_HEADER_BYTES as u32
            || self.protocol.decode.max_depth == 0
            || self.protocol.decode.max_nodes == 0
            || self.protocol.decode.max_allocation_bytes == 0
            || self.vm_limits.max_frame_depth == 0
            || self.vm_limits.instruction_budget == Some(0)
            || self.vm_limits.memory_limit_bytes == Some(0)
            || self.max_workers == 0
            || self.min_workers > self.max_workers
            || self.max_restarts == 0
            || self.restart_window.is_zero()
            || self.deadlines.checkout.is_zero()
            || self.deadlines.compute.is_zero()
            || self.deadlines.serialization.is_zero()
            || self.deadlines.cancel_grace.is_zero()
            || self.deadlines.cumulative_cpu.is_zero()
            || self.deadlines.max_attempts == 0
            || self.protocol.no_response_watchdog.is_zero()
        {
            return Err(PoolError::InvalidConfiguration);
        }
        Ok(())
    }
}
