//! The kernel machine: one run of one document on the bytecode core
//! ([`CoreMachine`]), which compiles its own program, runs it on a slot
//! heap and seals it as a site-keyed run record (FIG-7688).
//!
//! The machine keeps the embedding surface the engine it replaced had:
//! [`Machine`] and the inherent entry points below, with the same
//! signatures. No path selects another engine.

use lash_kernel_state::paged::{Checkpoint, PageSource, SealBudget};
use lash_kernel_value::data_limits::KernelDataLimits;

use crate::interface::{
    Bounds, DeliverError, Delivered, Host, Meters, Outcome, Program, StartError, Step, WaitId,
};
use crate::paged::{CellStart, Machine, MachineError, OpenError, SealError, SealedRoot};
use crate::vm::machine::CoreMachine;
use crate::{ChargeTape, ExecutionPath, MachineOptions, SiteCounts};

/// A kernel machine. It implements [`Machine`]; an embedder drives it with
/// `run` and `deliver`.
pub struct KernelMachine {
    core: CoreMachine,
    /// The frozen engine's statement-boundary hook, which the core does not
    /// run: a run with it set refuses (see [`retired`]).
    #[cfg(test)]
    pub(crate) boundary: Option<Box<dyn FnMut(u64) -> super::Boundary>>,
    /// What the laws' observation probes have turned on.
    #[cfg(test)]
    observing: bool,
    #[cfg(test)]
    path: bool,
}

impl std::fmt::Debug for KernelMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let meters = self.core.meters();
        f.debug_struct("KernelMachine")
            .field("charged", &meters.charged)
            .field("live_tasks", &meters.live_tasks)
            .finish_non_exhaustive()
    }
}

impl KernelMachine {
    fn of(core: CoreMachine) -> Self {
        Self {
            core,
            #[cfg(test)]
            boundary: None,
            #[cfg(test)]
            observing: false,
            #[cfg(test)]
            path: false,
        }
    }

    /// [`Machine::start`] under the deployment's data `limits` instead of
    /// the standard ones; a reopened run decodes under the standard ones.
    ///
    /// # Errors
    ///
    /// [`Machine::start`]'s.
    pub fn start_with_data_limits(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        limits: KernelDataLimits,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        CoreMachine::start_with_data_limits(program, bounds, start, limits, source).map(Self::of)
    }

    /// Records every charge the run makes from here on in `tape`, in order
    /// (`K-CHG-002`); recording changes no result, charge or slice return.
    pub fn record_charges(&mut self, tape: ChargeTape) {
        self.core.record_charges(tape);
    }

    /// The bytes the run holds as its memory schedule counts them (R7).
    pub fn memory_ledger(&self) -> u64 {
        self.core.meters().memory
    }

    /// Pages every object the heap can out at each statement boundary:
    /// the laws' forced-cold runs.
    #[cfg(test)]
    pub(crate) fn force_cold(&mut self) {
        self.core.force_cold();
    }
}

impl Machine for KernelMachine {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        CoreMachine::start(program, bounds, start, source).map(Self::of)
    }

    fn open(
        program: Program,
        bounds: Bounds,
        root: &Checkpoint,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        CoreMachine::open(program, bounds, root, source).map(Self::of)
    }

    fn set_options(&mut self, options: MachineOptions) {
        #[cfg(test)]
        {
            self.observing = !options.observation.is_off();
            self.path = options.trace_path;
        }
        self.core.set_options(options);
    }

    fn take_counts(&mut self) -> SiteCounts {
        self.core.take_counts()
    }

    fn take_path(&mut self) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.core.take_path()
    }

    fn take_caught(&mut self) -> Vec<WaitId> {
        self.core.take_caught()
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        #[cfg(test)]
        if self.boundary.is_some() {
            retired("a statement-boundary hook");
        }
        self.core.run(host, slice)
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        self.core.deliver(wait, outcome)
    }

    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        self.core.seal(budget)
    }

    fn meters(&self) -> Meters {
        self.core.meters()
    }
}

#[cfg(any(test, feature = "perf-witness"))]
impl crate::perf_witness::Profiling for KernelMachine {
    /// The core counts no work by function yet: [`Self::work`] reads none.
    fn count_work(&mut self) {}
    fn work(&self) -> Option<crate::observe::WorkReport> {
        None
    }
    /// The core publishes no guest context yet: the context stays inactive.
    fn publish_context(&mut self, context: std::sync::Arc<crate::observe::GuestContext>) {
        drop(context);
    }
    fn guest_symbols(&self) -> crate::observe::GuestSymbols {
        crate::observe::GuestSymbols {
            codes: Vec::new(),
            libs: Vec::new(),
            statements: Vec::new(),
        }
    }
}

/// A law reached a probe of the frozen engine the core replaced. Its owner
/// retargets the law to the core after the flip (FIG-7688 phase 2); until
/// then it fails here, by name.
#[cfg(test)]
#[track_caller]
fn retired(probe: &str) -> ! {
    panic!("{probe} probes the frozen engine; retarget this law to the core (FIG-7688 phase 2)")
}

/// The test-only entry points and probes the laws call. Where the core
/// holds the fact, the probe reads it; where only the frozen engine did,
/// it fails by name ([`retired`]).
#[cfg(test)]
#[allow(clippy::unused_self, clippy::needless_pass_by_value)]
impl KernelMachine {
    pub(crate) fn start_with_census(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        _layout: crate::compile::Layout,
        source: Box<dyn PageSource>,
        _census: Box<dyn lash_kernel_state::paged::AllocationCensus>,
    ) -> Result<Self, StartError> {
        Self::start(program, bounds, start, source)
    }

    /// Opens `root` in `layout`, which the core ignores: it has one.
    pub(crate) fn open_in(
        program: Program,
        bounds: Bounds,
        root: &lash_kernel_state::paged::CheckpointRoot,
        source: Box<dyn PageSource>,
        _layout: crate::compile::Layout,
    ) -> Result<Self, OpenError> {
        let checkpoint = lash_kernel_state::kernel_internals::checkpoint(root.clone());
        Self::open(program, bounds, &checkpoint, source)
    }

    /// The core has one execution path.
    pub(crate) fn force_source_execution(&mut self) {}
    /// The core has one execution path.
    pub(crate) fn force_generic_regions(&mut self) {}

    pub(crate) fn set_observe(&mut self, observe: crate::ObservationMode) {
        self.observing = !observe.is_off();
        self.core.watch(self.observing, self.path);
    }
    pub(crate) fn trace_path(&mut self) {
        self.path = true;
        self.core.watch(self.observing, self.path);
    }
    pub(crate) fn take_ran(&mut self) -> SiteCounts {
        self.take_counts()
    }
    pub(crate) fn take_execution_path(
        &mut self,
    ) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.take_path()
    }
    pub(crate) fn wait_count(&self) -> u64 {
        self.core.wait_count()
    }
    pub(crate) fn count_work(&mut self) {
        crate::perf_witness::Profiling::count_work(self);
    }
    pub(crate) fn work(&self) -> Option<crate::observe::WorkReport> {
        crate::perf_witness::Profiling::work(self)
    }
    pub(crate) fn publish_context(
        &mut self,
        context: std::sync::Arc<crate::observe::GuestContext>,
    ) {
        crate::perf_witness::Profiling::publish_context(self, context);
    }

    pub(crate) fn semantic_checkpoint(&mut self) -> lash_kernel_state::semantic::Checkpoint {
        retired("semantic_checkpoint")
    }
    pub(crate) fn logical_balance(
        &self,
    ) -> Result<
        lash_kernel_state::kernel_internals::logical::Balance,
        lash_kernel_doc::heap::HeapFault,
    > {
        retired("logical_balance")
    }
    pub(crate) fn cold_reads(&self) -> u64 {
        retired("cold_reads")
    }
    pub(crate) fn pending_phases(
        &mut self,
    ) -> Vec<(
        lash_kernel_doc::TaskId,
        usize,
        lash_kernel_state::paged::ContinuationPhase,
    )> {
        retired("pending_phases")
    }
    pub(crate) fn pending_roles(
        &mut self,
    ) -> Vec<(
        lash_kernel_doc::TaskId,
        usize,
        Vec<lash_kernel_state::paged::RoleCoordinate>,
    )> {
        retired("pending_roles")
    }
    pub(crate) fn delivered_tasks(&mut self) -> Vec<lash_kernel_doc::TaskId> {
        retired("delivered_tasks")
    }
    pub(crate) fn loop_headers(&mut self) -> Vec<lash_kernel_state::paged::HeaderRecord> {
        retired("loop_headers")
    }
    pub(crate) fn tamper_task_state(
        &mut self,
        _task: lash_kernel_doc::TaskId,
        _state: lash_kernel_state::paged::TaskStateRecord,
    ) -> Result<(), super::Halt> {
        retired("tamper_task_state")
    }
    pub(crate) fn tamper_pending(
        &mut self,
        _task: lash_kernel_doc::TaskId,
        _index: usize,
        _edit: impl FnOnce(
            &mut Option<lash_kernel_state::paged::ContinuationRecord>,
            &lash_kernel_state::kernel_internals::tree::Access<'_>,
        ),
    ) -> Result<(), String> {
        retired("tamper_pending")
    }
    pub(crate) fn tamper_loop(
        &mut self,
        _task: lash_kernel_doc::TaskId,
        _at: u64,
        _edit: impl FnOnce(
            &mut lash_kernel_state::paged::LoopRecord,
            &lash_kernel_state::kernel_internals::tree::Access<'_>,
        ),
    ) -> Result<lash_kernel_exec::coord::SitePath, String> {
        retired("tamper_loop")
    }
    pub(crate) fn restore_refusal(
        &mut self,
        _task: lash_kernel_doc::TaskId,
        _index: u64,
    ) -> Result<Option<lash_kernel_exec::coord::RestoreRefusal>, String> {
        retired("restore_refusal")
    }
    pub(crate) fn source_call_phase(
        &self,
        _at: &lash_kernel_value::sites::Site,
    ) -> lash_kernel_state::paged::ContinuationPhase {
        retired("source_call_phase")
    }
    pub(crate) fn source_action_site(&mut self) -> Option<Vec<u32>> {
        retired("source_action_site")
    }
    pub(crate) fn source_pending(
        &mut self,
        _task: lash_kernel_doc::TaskId,
        _index: u64,
    ) -> Option<lash_kernel_state::paged::ContinuationRecord> {
        retired("source_pending")
    }
    pub(crate) fn assert_abandoned_native_slots(&self, _id: lash_kernel_value::FunctionId) {
        retired("assert_abandoned_native_slots")
    }
    pub(crate) fn assert_native_boundary_law(&self, _id: lash_kernel_value::FunctionId) {
        retired("assert_native_boundary_law")
    }
    pub(crate) fn assert_external_native_boundary_law(&self, _id: lash_kernel_value::FunctionId) {
        retired("assert_external_native_boundary_law")
    }
    pub(crate) fn check_source_constructor_certificates(
        &self,
        _source: &std::sync::Arc<crate::compile::room::Source>,
        _link: lash_kernel_value::content_ids::LinkId,
        _expression: &str,
    ) -> Vec<String> {
        retired("check_source_constructor_certificates")
    }
    pub(crate) fn check_source_closure_certificates(
        &self,
        _source: &std::sync::Arc<crate::compile::room::Source>,
        _identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        _names: Vec<lash_kernel_value::Name>,
        _code: lash_kernel_doc::arena::LocalCode,
    ) {
        retired("check_source_closure_certificates")
    }
}
