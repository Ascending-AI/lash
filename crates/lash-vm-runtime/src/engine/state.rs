//! The lash_vm engine's state: the VM snapshot and the operation it parked
//! on, in one place (ADR 0132 §8; R9; FIG-5198).
//!
//! The process activation stores these bytes as the process's engine state
//! (`lash_exec_snapshots`, `p/<pid>`), so the snapshot revision and the state
//! revision are one row, and the operation the VM issued at its quiet point
//! is admitted in the same `process.advance` transaction that commits the
//! snapshot naming it.

use lash_core::{ProcessId, SettledOutput, StepName};
use lash_vm_protocol::OpaqueVmState;
use serde::{Deserialize, Serialize};

/// The engine step that runs the VM from the committed snapshot to its next
/// quiet point.
pub const VM_RUN_STEP: &str = "vm_run";

/// The version of a lash_vm process's engine state: [`LashVmEngineState`]
/// with the VM snapshot it holds, declared as the engine's state format and
/// part of the process's program identity. Re-exported by the facade's
/// `formats` manifest so a host can read it before wiring a store.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-vm-runtime/src/engine/state.rs",
///         cover(LashVmEngineState, VmRunInput, VmRunOutput),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "LashVmSegmentHandover"
pub const LASH_VM_SEGMENT_STATE_VERSION: u32 = 1;

/// A lash_vm process's engine state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LashVmEngineState {
    /// The start payload, as admitted.
    pub(crate) payload: serde_json::Value,
    /// The executable identity the snapshot was captured under; `None`
    /// before the first quiet point.
    pub(crate) program_hash: Option<String>,
    /// The VM snapshot; `None` before the first quiet point.
    pub(crate) vm: Option<OpaqueVmState>,
    /// How many `vm_run` steps were asked for: names the next one.
    pub(crate) runs: u64,
    /// How many operations the VM issued: the next operation's number.
    pub(crate) operations: u64,
    /// What the process is doing.
    pub(crate) phase: Phase,
}

/// What the process is doing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Phase {
    /// `vm_run` step `step` is in flight, resuming the VM with `inject`.
    Running {
        step: StepName,
        inject: Option<Injection>,
    },
    /// The VM is parked on operation `operation`.
    Parked { operation: u64, wait: Wait },
    /// The process ended.
    Ended,
}

/// What a parked VM's operation waits for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "wait", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Wait {
    /// A resource operation, or an aggregate's leaves: a step each, or a
    /// durable wake for a timer.
    Leaves {
        batch: Option<BatchShape>,
        leaves: Vec<Leaf>,
        /// The leaves that settled, in settlement order.
        settled: Vec<usize>,
    },
    /// A sleep, until its deadline (epoch milliseconds).
    Sleep {
        until_ms: i64,
        /// The node that sleeps and which occurrence of it, when the VM
        /// tracks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_core::StepEffectSite>,
    },
    /// Another process's terminal.
    Process {
        process: ProcessId,
        /// The site that awaits and which occurrence of it, when the VM
        /// tracks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_core::StepEffectSite>,
    },
}

/// How an aggregate consumes its leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchShape {
    pub(crate) consumer: lash_vm::AggregateConsumer,
    pub(crate) settled_value_after: Option<usize>,
}

/// One leaf of a parked operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "leaf", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Leaf {
    /// A step: a catalog tool or a host operation.
    Step {
        step: StepName,
        /// The actor's admitted tool identity, retained with its settlement.
        call_id: Option<lash_core::ToolCallId>,
        outcome: Option<Box<SettledOutput>>,
    },
    /// A timer: the process sleeps until `until_ms` (epoch milliseconds)
    /// on a durable wake, as a plain sleep does, and the timer fulfils
    /// when it wakes. No step body runs for it.
    Timer { until_ms: i64, fired: bool },
    /// Settled when the operation was issued, without a step: a leaf
    /// refused before dispatch, or a language runtime value.
    Settled {
        fulfilled: bool,
        outcome: EncodedOutcome,
    },
}

impl Leaf {
    /// Whether the leaf settled, and if so whether it fulfilled.
    pub(crate) fn fulfilled(&self) -> Option<bool> {
        match self {
            Self::Step {
                outcome: Some(outcome),
                ..
            } => Some(super::injection::fulfilled(outcome)),
            Self::Step { outcome: None, .. } => None,
            Self::Timer { fired, .. } => fired.then_some(true),
            Self::Settled { fulfilled, .. } => Some(*fulfilled),
        }
    }
}

/// What a resumed VM is answered with: the outcome of the operation it
/// parked on, fed back to the operation it issues again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "inject", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Injection {
    /// The leaves of a resource operation or an aggregate, and how the
    /// aggregate was decided.
    Leaves {
        operation: u64,
        decision: Decision,
        leaves: Vec<Leaf>,
    },
    /// The sleep ended.
    Woke { operation: u64 },
    /// The awaited process ended.
    ProcessEnded {
        operation: u64,
        outcome: Box<lash_core::ProcessOutcome>,
    },
}

/// How a parked resource operation or aggregate was decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Decision {
    /// A lone resource operation settled.
    Single,
    /// Every leaf's result, in leaf order.
    AllResults,
    /// One leaf's settlement decided the aggregate.
    Selected { leaf: usize },
    /// Every leaf rejected (`any`).
    ExhaustedRejections,
    /// The plain value after `settled_value_after` decided it.
    SettledValue,
}

/// What `vm_run` answers: the VM parked on one issued operation, or the
/// process's end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum VmRunOutput {
    /// The VM reached a quiet point: its snapshot, and the operation it
    /// issued there.
    Parked {
        program_hash: String,
        vm: OpaqueVmState,
        /// Boxed: an issued operation names its site and loop context.
        issued: Box<IssuedOperation>,
    },
    /// The process ended.
    Ended {
        outcome: Box<lash_core::ProcessOutcome>,
    },
}

/// The input of one `vm_run`: the start payload, the snapshot to resume
/// from (none for a first run), and what the resumed VM is answered with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmRunInput {
    pub(crate) payload: serde_json::Value,
    pub(crate) program_hash: Option<String>,
    pub(crate) vm: Option<OpaqueVmState>,
    pub(crate) inject: Option<Injection>,
}

/// The one operation a VM issued at its quiet point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "issued", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IssuedOperation {
    /// A resource operation (`batch: None`), or an aggregate's leaves.
    Leaves {
        batch: Option<BatchShape>,
        leaves: Vec<IssuedLeaf>,
    },
    /// A sleep until `until_ms`.
    Sleep {
        until_ms: i64,
        /// The node that sleeps and which occurrence of it, when the VM
        /// tracks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_core::StepEffectSite>,
    },
    /// An await of another process's terminal.
    AwaitProcess {
        process: ProcessId,
        /// The site that awaits and which occurrence of it, when the VM
        /// tracks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_core::StepEffectSite>,
    },
}

/// One leaf of an issued resource operation or aggregate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "leaf", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IssuedLeaf {
    /// A catalog tool with its input.
    Tool {
        tool: lash_core::ToolId,
        input: serde_json::Value,
        /// The call's node and occurrence, when the VM tracks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_core::StepEffectSite>,
        language_execution: Option<Box<lash_trace::TraceLanguageExecution>>,
    },

    /// A timer that settles at `until_ms`.
    Timer { until_ms: i64 },
    /// Settled at issue: refused before dispatch, or a runtime value.
    Settled {
        fulfilled: bool,
        outcome: EncodedOutcome,
    },
}

/// A VM outcome settled at issue, in the effect-value codec's MessagePack
/// form: VM values have no JSON form of their own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct EncodedOutcome(Vec<u8>);

impl EncodedOutcome {
    pub(crate) fn encode(
        outcome: &lash_vm::ResourceOperationOutcome,
    ) -> Result<Self, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(outcome).map(Self)
    }

    pub(crate) fn decode(
        &self,
    ) -> Result<lash_vm::ResourceOperationOutcome, rmp_serde::decode::Error> {
        rmp_serde::from_slice(&self.0)
    }
}
