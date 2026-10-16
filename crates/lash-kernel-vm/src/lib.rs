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
//! compiles every library body, and hands it to every [`Program`]: a run's
//! starts and opens reuse each live document's admission and compiled code.
//!
//! [`KernelMachine`] is the machine; [`Machine`] is the interface an embedder
//! and the conformance harness drive it through. Its rules are the `K-MACH` rules of
//! `docs/kernel/semantics.md`.

mod compile;
mod costs;
mod crossing;
mod data;
mod functions;
mod heap;
mod interface;
mod machine;
pub mod observe;
/// The machine's face after the persistent-heap cutover (FIG-6195):
/// implemented by the paged kernel machine.
pub mod paged;
#[cfg(any(test, feature = "perf-witness"))]
pub mod perf_witness;
mod session_editor;

#[cfg(test)]
mod laws;

pub use compile::{Layout, PreparedLibrary};
pub use functions::{MachineFunctions, register_machine_functions};
pub use machine::KernelMachine;
pub use observe::{Observe, RanDelta, StatementPath};
pub use paged::{TaskFilter, TaskPage};
pub use session_editor::{SessionEditor, SessionValue};

pub use interface::{
    Bound, BoundExceeded, Bounds, CellSave, CellStart, DeliverError, Delivered, EffectMode,
    EffectModes, EffectRequest, End, Finished, Host, HostFault, Machine, MachineError, Meters,
    OpenError, Outcome, Park, Program, Request, RunError, SaveCell, SavedCell, SealError,
    SleepRequest, StartError, Step, Target, WaitId,
};
