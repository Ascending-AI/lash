//! The one owner of a VM's guest-derived state (FIG-4158, ADR 0123).
//!
//! Model code runs in a worker that is reset and reused across sessions. A
//! reset is only clean if there is exactly one place guest-derived state can
//! be: this instance. It owns the session's globals, heap and roots (its
//! [`State`]), the recycled execution scratch, the parsed, linked and
//! compiled cell cache, and any run in flight, with its operand stacks,
//! pending handles and parked continuations. Nothing guest-derived lives in a
//! static; `scripts/check-vm-static-state.py` refuses one.
//!
//! [`VmInstance::reset`] drops the instance and installs a pristine one. It
//! clears nothing field by field, so state added to the instance later is
//! covered by the same drop.
//!
//! The instance is also the worker side of the parent-worker split: decoding
//! VM state semantically — a parked continuation, a snapshot, a durable
//! fragment — restores guest values, validates and compiles their regular
//! expressions, and checks them against the program. Those decoders are
//! reachable only through the instance. The parent holds VM state as opaque
//! bytes (`lash_vm_protocol::OpaqueVmState`) and never decodes it.

use std::sync::Arc;

use super::{
    CompiledProgram, ContinuationError, DurableBaseline, ExecutionScratch, LinkedProgramCache,
    Snapshot, SnapshotDecodeError, State, VmContinuation,
};

mod step;

pub use step::{
    VmComplete, VmExecutionStart, VmGuestError, VmInterrupt, VmParkReason, VmParked, VmRequest,
    VmResume, VmRunConfig, VmStep, VmStepError, VmSuspended,
};

/// Everything guest-derived one VM holds.
pub struct VmInstance {
    /// The session's globals, heap and roots between runs. A run in flight
    /// holds them; they come back when it completes.
    state: State,
    /// The buffers a foreground run recycles.
    scratch: ExecutionScratch,
    /// Cells this instance parsed, linked and compiled, keyed by source and
    /// host surface.
    linked_programs: LinkedProgramCache,
    /// The run in flight, if any: its VM, pending handles and suspension.
    running: Option<step::VmExecution>,
}

impl VmInstance {
    /// A pristine instance: the one constructor every fresh and every reset
    /// instance comes from. It builds the instance fresh, which FIG-4157
    /// measured faster than cloning a prebuilt template.
    pub fn pristine() -> Self {
        Self {
            state: State::new(),
            scratch: ExecutionScratch::new(),
            linked_programs: LinkedProgramCache::new(),
            running: None,
        }
    }

    /// Drops this instance and installs a pristine one: the globals, heap,
    /// scratch, compiled cells and any run in flight go with the drop.
    pub fn reset(&mut self) {
        *self = Self::pristine();
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// Replaces the session state, as a cancelled cell's rollback does.
    pub fn replace_state(&mut self, state: State) -> State {
        std::mem::replace(&mut self.state, state)
    }

    pub fn linked_programs(&self) -> &LinkedProgramCache {
        &self.linked_programs
    }

    pub fn linked_programs_mut(&mut self) -> &mut LinkedProgramCache {
        &mut self.linked_programs
    }

    /// Lends the recycled scratch to an in-process run; return it with
    /// [`Self::restore_scratch`].
    pub fn take_scratch(&mut self) -> ExecutionScratch {
        std::mem::take(&mut self.scratch)
    }

    pub fn restore_scratch(&mut self, scratch: ExecutionScratch) {
        self.scratch = scratch;
    }

    /// The session state and scratch at once, for an in-process run that
    /// borrows both.
    pub fn state_and_scratch_mut(&mut self) -> (&mut State, &mut ExecutionScratch) {
        (&mut self.state, &mut self.scratch)
    }

    /// Decodes a parked continuation: the worker-side semantic decode, which
    /// restores guest values and validates their regular expressions.
    pub fn open_continuation(&self, bytes: &[u8]) -> Result<VmContinuation, ContinuationError> {
        VmContinuation::decode(bytes)
    }

    /// Decodes a canonical snapshot written under this build's own format.
    pub fn open_snapshot(&self, bytes: &[u8]) -> Result<Snapshot, SnapshotDecodeError> {
        Snapshot::from_canonical_bytes(bytes)
    }

    /// Decodes a canonical snapshot under the fleet's read window.
    pub fn open_snapshot_for_fleet(
        &self,
        bytes: &[u8],
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<Snapshot, SnapshotDecodeError> {
        Snapshot::from_canonical_bytes_for_fleet(bytes, fleet_format)
    }

    /// Installs a durable header and per-binding fragments as this instance's
    /// session state, answering the baseline the next incremental capture
    /// diffs against.
    pub fn restore_durable_parts<'a>(
        &mut self,
        header: &[u8],
        fragments: impl IntoIterator<Item = (&'a str, &'a [u8])>,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<DurableBaseline, SnapshotDecodeError> {
        let (state, baseline) = State::from_durable_parts(header, fragments, fleet_format)?;
        self.state = state;
        Ok(baseline)
    }

    /// Starts a run of `program` under `config` and drives it to its first
    /// suspension or its end.
    pub fn start(
        &mut self,
        program: Arc<CompiledProgram>,
        start: VmExecutionStart,
        config: VmRunConfig,
    ) -> Result<VmStep, VmStepError> {
        if self.running.is_some() {
            return Err(VmStepError::AlreadyRunning);
        }
        let execution = step::VmExecution::start(
            program,
            start,
            config,
            std::mem::take(&mut self.state),
            std::mem::take(&mut self.scratch),
        )?;
        self.running = Some(execution);
        self.drive()
    }

    /// Answers the run's pending request and drives it to its next suspension
    /// or its end. A resume that does not answer the pending request is
    /// refused and leaves the run suspended.
    pub fn resume(&mut self, resume: VmResume) -> Result<VmStep, VmStepError> {
        let execution = self.running.as_mut().ok_or(VmStepError::NotRunning)?;
        execution.answer(resume)?;
        self.drive()
    }

    /// Whether a run is in flight.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The live interrupt of the run in flight: setting it makes the run's
    /// next cooperative cancellation probe answer cancelled.
    pub fn interrupt(&self) -> Option<VmInterrupt> {
        self.running.as_ref().map(step::VmExecution::interrupt)
    }

    fn drive(&mut self) -> Result<VmStep, VmStepError> {
        let execution = self.running.as_mut().ok_or(VmStepError::NotRunning)?;
        match execution.poll()? {
            step::Polled::Suspended(suspended) => Ok(VmStep::Suspended(suspended)),
            step::Polled::Refused(error, state, scratch) => {
                self.running = None;
                if let Some(state) = state {
                    self.state = state;
                }
                if let Some(scratch) = scratch {
                    self.scratch = scratch;
                }
                Err(VmStepError::ContinuationRefused(error))
            }
            step::Polled::Ended(end) => {
                self.running = None;
                if let Some(state) = end.state {
                    self.state = state;
                }
                if let Some(scratch) = end.scratch {
                    self.scratch = scratch;
                }
                Ok(end.step)
            }
        }
    }
}

impl Default for VmInstance {
    fn default() -> Self {
        Self::pristine()
    }
}

impl std::fmt::Debug for VmInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmInstance")
            .field("state", &self.state)
            .field("linked_programs", &self.linked_programs.stats())
            .field("running", &self.running.is_some())
            .finish_non_exhaustive()
    }
}
