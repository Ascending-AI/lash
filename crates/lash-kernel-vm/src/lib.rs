//! The lash kernel's machine.
//!
//! In Lash, recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! The machine is a plain library (`docs/kernel/design.md` §9). It runs an
//! admitted document as tasks until none is ready, hands the embedder the
//! effects and sleeps they asked for, and takes their outcomes back one at a
//! time. It does no I/O, starts no thread and keeps no global state: clock,
//! random, projection reads, `print` output and a pending cancel go through
//! the [`Host`] the embedder passes to [`Machine::run`]. The machine parks;
//! the embedder commits.
//!
//! [`Machine::seal`] freezes changed semantic pages and returns a bounded
//! checkpoint root. [`Machine::open`] resumes that root against the admitted
//! document, loading each continuation path as execution needs it.
//!
//! An embedder prepares its registry once as a [`PreparedLibrary`], which
//! lazily compiles library bodies, and hands it to every [`Program`]: a run's
//! starts and opens reuse each live document's admission and compiled code.
//!
//! [`KernelMachine`] is the machine; [`Machine`] is the interface an embedder
//! and the conformance harness drive it through. Its rules are the `K-MACH` rules of
//! `docs/kernel/semantics.md`.

mod compile;
pub(crate) mod conformance;
mod costs;
mod crossing;
mod data;
mod functions;
mod interface;
mod machine;
mod observe;
/// The machine's face after the persistent-heap cutover (FIG-6195):
/// implemented by the paged kernel machine.
mod paged;
#[cfg(any(test, feature = "perf-witness"))]
pub mod perf_witness;
mod save;
mod session_editor;

#[cfg(test)]
mod laws;

#[cfg(test)]
pub(crate) use compile::Layout;
pub use compile::{PreparedLibrary, body_census};
pub use functions::{MachineFunctions, implement_machine_functions, register_machine_functions};
pub use lash_kernel_doc::ExecutionPath;
pub use machine::KernelMachine;
pub use observe::{ChargeTape, ObservationMode, SiteCounts};
pub use paged::SealedRoot;
pub use save::CellSaveContext;
pub use session_editor::{SessionEditor, SessionValue};

/// The machine's implementation, for the other kernel crates only: plans
/// that perturb how a run executes, and the step-structure checks kernel
/// laws pin. Nothing outside `crates/lash-kernel-*` names this module
/// (`scripts/check-kernel-boundary.py`, FIG-6688).
#[doc(hidden)]
pub mod kernel_internals;

pub use interface::{
    Bound, BoundExceeded, Bounds, CellSave, CellStart, Charge, ChargeLimiter, DeliverError,
    Delivered, EffectMode, EffectModes, EffectRequest, End, Finished, Host, HostFault,
    LimitDecision, Machine, MachineError, MachineOptions, MemoryLimiter, MemoryMeter, Meters,
    OpenError, Outcome, Park, Program, Request, RunError, RunObserver, SaveCell, SavedCell,
    SealError, SleepRequest, StartError, Step, Target, WaitId,
};

pub use functions::machine_native;
