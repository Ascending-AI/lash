//! Run limits shared by worker transport and recorded plugin results.

use serde::{Deserialize, Serialize};

/// The worker message an encoder refused, without its guest data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFrameKind {
    Refused,
    Progress,
    LimitExceeded,
    Ready,
    Started,
    HostRead,
    Printed,
    Parked,
    Slice,
    Ended,
    Delivered,
    Sealed,
    Cancelled,
    ResetDone,
    Prepared,
}

/// The VM or worker limit a run exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLimit {
    Fuel,
    Heap,
    Depth,
    /// A step's execution observations outgrew the run's heap budget, which
    /// bounds the stream a worker holds and hands its parent, or a single
    /// observation outgrew what one frame carries (FIG-4458).
    Observations,
    /// The encoded effect request or result exceeds its configured bound.
    EffectValue {
        size: u64,
        bound: u64,
    },
    /// A parked run exceeds its configured bound.
    VmState {
        size: u64,
        bound: u64,
    },
    /// A worker message cannot fit in one transport frame.
    Frame {
        kind: WorkerFrameKind,
        size: u64,
        bound: u64,
    },
    /// The execution's CPU time passed its configured budget: the parent's
    /// cumulative accounting across the execution's attempts, or the CPU
    /// ceiling the worker set from that budget (`SIGXCPU`).
    Cpu {
        bound_ms: u64,
    },
    /// One exchange computed or serialized past its configured wall-clock
    /// deadline.
    Deadline {
        bound_ms: u64,
    },
    /// The execution spent its configured replacement attempts.
    Attempts {
        bound: u32,
    },
}

impl WorkerLimit {
    /// Whether this limit is a verdict of the host and the attempt that met
    /// it rather than of the run. Fuel, heap and frame depth are measured by
    /// the VM against the run's own bounds, and a step's observations
    /// against its heap budget, so every execution of the run meets them at
    /// the same point. Encoded effect values, VM state and frames likewise
    /// meet their size bounds on every execution. The CPU budget and the
    /// compute deadline are limits lash set on the run's worker: the same
    /// run on the same inputs meets them again, so they are the run's own
    /// end, never a retry (FIG-6523). Only the replacement attempts are
    /// the host's: they count infrastructure losses, which a replay, or
    /// another host with capacity, answers differently (FIG-4451).
    pub const fn is_host_verdict(self) -> bool {
        matches!(self, Self::Attempts { .. })
    }
}

impl std::fmt::Display for WorkerLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EffectValue { size, bound } => write!(
                f,
                "effect value of {size} bytes exceeds its {bound}-byte bound"
            ),
            Self::VmState { size, bound } => {
                write!(f, "VM state of {size} bytes exceeds its {bound}-byte bound")
            }
            Self::Frame { kind, size, bound } => write!(
                f,
                "{kind:?} frame of {size} bytes exceeds its {bound}-byte bound"
            ),
            Self::Cpu { bound_ms } => write!(f, "CPU time exceeds its {bound_ms} ms budget"),
            Self::Deadline { bound_ms } => {
                write!(f, "computation exceeds its {bound_ms} ms deadline")
            }
            Self::Attempts { bound } => write!(f, "the execution spent its {bound} attempts"),
            other => write!(f, "{other:?}"),
        }
    }
}
