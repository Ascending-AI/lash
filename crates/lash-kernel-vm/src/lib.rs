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
//!
//! # Pause admission matrix
//!
//! A **step boundary** is after a statement, or the part of a statement
//! before/after a nested action (`K-MACH-002`). A **run boundary** is a
//! returned [`Step`]. Between calls (also just after start/open) the
//! machine exposes a sealable continuation;
//! a blocked synchronous host/page/native call still has a Rust stack.
//! Safe points retain operand roles, task order, pending departures and
//! exact charge/counts. They never serialize that Rust stack.
//!
//! **Taken** below names the observable result. **Deferred** names the next
//! boundary which can take it. Deferral does not promise an extra slice
//! after completion: [`Step::Ended`] wins when no continuation remains.
//! A fuel slice is cooperative; an execution bound instead ends with
//! [`RunError::Bound`], including inside a native call (`K-BND-001`).
//!
//! | Execution context | Fuel slice / execution bound | Wait / park | Host stop / run cancel | Seal / checkpoint | Upgrade carry |
//! | --- | --- | --- | --- | --- | --- |
//! | Ordinary statement | Deferred to its next step boundary: `Slice` if work remains; a crossed bound is taken as `Ended(Error(Bound))`. | Taken at the action: its task waits; `Parked` when no task is ready. | Cancel deferred to the next slice or park: `Ended(Cancelled)`. Stop defers to the next returned `Step`. | Deferred to the run boundary, then taken as `Some(SealedRoot)`, or deferred as `None` to the next seal. | Deferred to a completed checkpoint, then taken by the carry owner or refused by typed carry/open admission. |
//! | Read-mode host operation | Deferred until the synchronous answer completes the step; bounds retain the ordinary typed verdict. | Taken synchronously as a read result/raise, with no wait (`K-EFF-012`); a later waited action follows the ordinary park rule. | Deferred until the read returns and the next slice/park polls the latched cancel; stop awaits the returned `Step`. | Deferred until the read returns and `run` returns; no native continuation enters the checkpoint. | Deferred to the completed read, run boundary and completed checkpoint. |
//! | Helper or native call | A helper body can slice at its next step; a native defers to its return and that step's boundary. Bounds are taken as `Ended(Error(Bound))`. | Taken in a helper's ordinary waited action. A native has only a synchronous result/error ABI; a later waited action can park. | Deferred to the next slice/park, after any synchronous native returns; stop awaits the returned `Step`. | Deferred to a run boundary; helper frames are saved, native stacks have already returned. | Deferred to a completed checkpoint; missing pinned functions/helpers refuse as typed open/carry errors. |
//! | Unfinished projection or seal | Refused by `MachineError::Projecting` if `run` is called after `seal` returned `None`; retry `seal` first. | Execution is refused by `Projecting`; existing waits remain saved and no new guest action runs until sealing completes. | Deferred to the first slice/park after sealing completes and `run` resumes. The host retains the cancel. Stop can discard the activation and keep its committed root. | Deferred by `Ok(None)` to the next seal; completion yields `Some(root)`. Infrastructure refusal is `SealError::Heap`, including during root projection. | Deferred until `Some(root)`; no candidate checkpoint exists to carry after `None` or an error. |
//! | Finally or run-root cleanup | Taken at ordinary helper/guest step boundaries; held departure survives. A bound ends the run without further cleanup. | Taken at ordinary waits; pending return/raise and cleanup position are saved. | Cancel deferred to the next slice/park and taken without running further cleanup; stop awaits the returned `Step`. | Taken at a run boundary, including while cleanup waits; the held departure and cleanup frames are sealed. | Taken from that completed checkpoint with cleanup coordinates carried; an unmapped coordinate refuses with `ParkedRefusal::SiteNotCarried`. |
//! | Nested process start / task spawn | A process start is an operation, so its selected read/wait mode governs. A guest `spawn` can slice with its parent in `Spawned` phase; bounds are typed. | A waited process start is taken as an ordinary effect request. A guest child runs to its first wait/end before its spawner continues; the whole run parks only with no ready task. | Deferred to the next slice/park; a host cancel ends the whole run. Stopping an activation leaves committed process ownership to the engine. | Deferred to a run boundary; a nested `spawn` saves the child and the parent's continuation, never the process provider's stack. | Deferred to a completed checkpoint; saved spawn origins and pending operand roles must have successors or carry refuses with `SiteNotCarried`. |
//!
//! The matrix describes admission through the API, not concurrent calls
//! into one machine. `run`, `deliver`, and `seal` exclusively borrow it;
//! [`Host`] and the native ABI receive no machine handle. A read/native
//! callback cannot re-enter `seal` to try to save its caller. A native's
//! cold/room retry warms/reserves and retries the call; it is not a durable
//! suspension. A body fallback uses ordinary semantic frames.
//!
//! The host owns a pending run-cancel until [`Host::cancel_requested`]
//! observes it (`K-MACH-006`); it must latch a request made during a read,
//! native or seal. Guest `cancel h` is a different rule: it raises at the
//! task's wait, can be caught and runs cleanup which may itself wait
//! (`K-TASK-017`). A host **stop** withdraws an activation, not the logical
//! run: the engine keeps the committed root and decides redrive. The
//! process driver stops at a slice or completes its park/end; physical
//! worker cancellation is separately acknowledged by `WorkerMessage::Cancelled`.
//! This library neither starts processes nor owns that durable winner.
//!
//! Carry is also outside execution: `lash_kernel_migrate::carry_checkpoint`
//! accepts a completed checkpoint, returns a typed `CarryFault` (including
//! `ParkedRefusal`), and its owner calls [`validate_foreign_checkpoint`]
//! before publication. Open/admission returns [`OpenError`] (including
//! `MissingHelper`, `MissingFunction`, identity errors and `Heap` for
//! malformed continuation data); diagnostic strings describe these typed
//! failures, not an accepted pause. An ended machine refuses `run` with
//! [`MachineError::Ended`] and `seal` with [`SealError::Ended`]. Heap or
//! host infrastructure faults abandon the activation; resume uses its last
//! committed root, not a partially projected live instance.
//!
//! ## Laws for the matrix
//!
//! Paths below are exact test names under this crate's unit-test target,
//! unless another crate is named. These are citations, not a requirement
//! to run all of them for a change to one cell.
//!
//! - Ordinary slices, cancel and park:
//!   `laws::tasks::slices_change_nothing_and_a_host_cancel_ends_the_run`;
//!   `laws::paged::an_issued_request_survives_a_slice_and_is_reported_once`
//!   checks exactly one reported request after slice/seal/reopen.
//! - Read completion and helper read boundaries:
//!   `laws::continuation::body_performs::read_bound_planned_performs_answer_as_their_bodies`
//!   compares sliced/cold/body/planned runs, origin and exact charge;
//!   `laws::tasks::a_cancel_requested_inside_a_read_is_taken_at_the_next_slice`
//!   requires one read, one cancel poll, no subsequent statement, a typed
//!   cancelled end, and matching nonzero site counts and charge.
//! - Helper/native paths and their bounds:
//!   `laws::operations::an_operation_runs_alike_whichever_path_answers`,
//!   `laws::frames::a_slice_lands_inside_an_expression_callee_and_changes_nothing`,
//!   `laws::frames::a_seal_inside_an_expression_callee_is_exact`, and
//!   `laws::bounds::the_memory_bound_holds_inside_a_native_call`.
//! - Partial projection, seal deferral and typed refusal:
//!   `laws::identity::a_seal_out_of_budget_holds_the_run_until_it_completes`
//!   requires multiple incomplete seals, `MachineError::Projecting`, equal
//!   completed state and successful continuation;
//!   `laws::paged::an_exhausted_seal_budget_preserves_both_continuations`;
//!   `laws::paged::a_projection_refusal_preserves_the_typed_heap_cause`
//!   requires a real refused page and `SealError::Heap(LeaseLost)`.
//!   `lash-vm-worker`'s
//!   `in_process::tests::a_refused_seal_reaches_the_broker_with_its_typed_cause`
//!   checks that cause through the threaded broker adapter on SQLite.
//! - Cleanup waits, slices, seals and cancellation:
//!   `laws::tasks::cancel_raises_at_the_wait_and_cleanup_runs_and_may_wait`,
//!   `laws::continuation::body_performs::a_run_cancelled_in_a_body_wait_runs_no_cleanup`,
//!   `laws::continuation::a_return_policy_adopts_once_after_a_cold_cleanup_park`,
//!   `laws::continuation::a_root_cleanup_runs_once_after_source_finalizers_and_resumes_exactly`.
//! - Nested task starts:
//!   `laws::tasks::spawn_runs_the_new_task_to_its_first_wait_and_then_the_spawner`,
//!   `laws::continuation::body_performs::a_library_spawned_task_performs_under_its_spawners_statement`,
//!   `laws::operations::spawned_library_roots_keep_their_formula_arguments`.
//! - Carry admission in `lash-durable-test`, `kernel_step_crash_laws`:
//!   `linked_carry::an_unmapped_linked_spawn_origin_refuses_without_rewriting_on_sqlite`,
//!   `linked_carry::a_foreign_linked_kernel_or_closure_refuses_before_rewriting_on_sqlite`,
//!   `paged_carry::a_paged_carry_commits_one_consistent_side_and_resumes_after_retirement_on_sqlite`.

mod compile;
mod costs;
mod crossing;
mod data;
mod functions;
pub mod interface;
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
pub use machine::KernelMachine;
pub use observe::{ChargeTape, ObservationMode};
pub use paged::{SealedRoot, validate_foreign_checkpoint};
pub use save::CellSaveContext;
pub use session_editor::{SessionEditor, SessionValue};
pub use {
    lash_kernel_value::observation::ExecutionPath, lash_kernel_value::observation::SiteCounts,
};

/// The machine's implementation, for the other kernel crates only: plans
/// that perturb how a run executes, and the step-structure checks kernel
/// laws pin. Nothing outside `crates/lash-kernel-*` names this module
/// (`scripts/check-kernel-boundary.py`, FIG-6688).
#[doc(hidden)]
pub mod kernel_internals;

pub use interface::{
    Bound, BoundExceeded, Bounds, CellSave, CellStart, Charge, ChargeLimiter, DeliverError,
    Delivered, EffectModes, EffectRequest, End, Finished, FunctionOrigins, Host, HostFault,
    LimitDecision, Machine, MachineError, MachineOptions, MemoryLimiter, MemoryMeter, Meters,
    OpenError, Outcome, Park, PreparedInstance, PreparedSlot, Program, Request, RunError,
    RunObserver, SaveCell, SavedCell, SealError, StartError, Step, Target, WaitId,
};

pub use functions::machine_native;

/// Controlled execution strategies for differential embedding laws.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
