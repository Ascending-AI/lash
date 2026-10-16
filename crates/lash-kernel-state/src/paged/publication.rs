//! Publication (FIG-6318, FIG-6425): the one module that installs the
//! machine's execution roots and statement roots into the census, prices
//! what it holds for them and decides when a refinement may trace. Every
//! root tree the census reads is installed here, and no other module
//! changes what the machine holds.
use super::directory::execution::TaskRecord;
use super::directory::value_cost;
use super::heap_impl::{PagedHeap, census_fault};
use super::tree::{Cell, SealWork, Tree, checked_sum};
use super::*;
use lash_kernel_doc::heap::*;

/// The largest U at which a carry traces to collect the cell's garbage
/// (FIG-6321): a small session sheds what its cell left dead before it
/// rides into every later cell's pages; a larger one collects under
/// pressure only (ADR 0145 §5). U bounds the trace's work, where an entity
/// count does not (FIG-6425): every entity costs at least
/// [`MIN_ENTITY_COST`] and every element, entry or capture the trace walks
/// for edges at least 8 bytes, so a carry under this bound visits at most
/// 4,096 entities and 16,384 rows, however long one scalar list grows.
pub(super) const CARRY_COLLECTION_BYTES: u64 = 4096 * MIN_ENTITY_COST.0;

/// What the machine holds outside the heap's trees: the running
/// statement's roots and holds, what it holds for its frames since it last
/// published, and whether the installed roots are its current ones.
pub(super) struct Holdings {
    temporary: Vec<Value>,
    fresh: Vec<EntityId>,
    /// What the running statement holds: credit for its writes, released
    /// at the statement's end.
    reserved: u64,
    /// What the machine holds for its frames since it last published:
    /// counted in U, and taken as credit when the roots it pays for are
    /// installed (FIG-6318).
    held: u64,
    /// Whether the installed task and join trees are the execution state's
    /// current roots. A machine clears it once its state moves past what it
    /// published; a write that then needs a refinement refuses rather than
    /// trace them (FIG-6318).
    roots_current: bool,
    /// The bytes the last write refused for (FIG-6318): the refusal also
    /// crosses the native face, whose errors cannot name it.
    refused: Option<u64>,
}

impl Default for Holdings {
    /// What a heap holds when it opens: nothing, over current roots.
    fn default() -> Self {
        Self {
            temporary: Vec::new(),
            fresh: Vec::new(),
            reserved: 0,
            held: 0,
            roots_current: true,
            refused: None,
        }
    }
}

impl Holdings {
    /// The credit what the running statement holds gives a write that
    /// grows by `growth`.
    pub(super) fn credit(&self, growth: u64) -> u64 {
        self.reserved.min(growth)
    }
    /// Spends `credit` the running statement held on the write it paid for.
    pub(super) fn spend(&mut self, credit: u64) {
        self.reserved -= credit;
    }
    /// Holds `bytes` more for the running statement, which U already counts.
    pub(super) fn reserve(&mut self, bytes: u64) -> Result<(), HeapFault> {
        self.reserved = self
            .reserved
            .checked_add(bytes)
            .ok_or_else(|| HeapFault::Malformed("outstanding reservation overflow".into()))?;
        Ok(())
    }
}

impl PagedHeap {
    /// Opens the execution roots `root` names over the heap whose header it
    /// names: that header's accounting already prices them, so nothing is
    /// charged.
    pub fn restore_execution_roots(&mut self, root: &CheckpointRoot) {
        self.session = Tree::of_keyed(&root.session);
        self.tasks = Tree::of_keyed(&root.tasks);
        self.joins = Tree::of_keyed(&root.joins);
    }
    /// The installed session bindings.
    pub fn session_root(&self) -> &Tree<BindingRecord> {
        &self.session
    }
    /// The installed task rows.
    pub fn task_root(&self) -> &Tree<TaskRecord> {
        &self.tasks
    }
    /// The installed list joins.
    pub fn join_root(&self) -> &Tree<ListJoinRecord> {
        &self.joins
    }
    /// Seals the installed task rows within `work`; `false` when it ran
    /// out. Sealing changes no cost.
    ///
    /// # Errors
    ///
    /// The faults of the pages.
    pub fn seal_task_root(&mut self, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.tasks.seal(&self.access, work)
    }
    /// Seals the installed session bindings within `work`; `false` when it
    /// ran out. Sealing changes no cost.
    ///
    /// # Errors
    ///
    /// The faults of the pages.
    pub fn seal_session_root(&mut self, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.session.seal(&self.access, work)
    }
    /// The execution state moved past the roots last installed with
    /// [`Self::set_execution_roots`]: until the next installation, a write
    /// that needs an exact refinement refuses with
    /// [`HeapWriteError::NeedsPrecision`] before it changes anything, so no
    /// trace ever runs over roots the machine has not published (FIG-6318).
    pub fn execution_roots_moved(&mut self) {
        self.holdings.roots_current = false;
    }
    /// Holds `bytes` the running statement pays for, as a native
    /// reservation does: U grows by them at once. A statement's hold is
    /// credit for its writes and is released at the statement's end; a
    /// persistent one pays for what the machine keeps in its frames and is
    /// taken as credit when it next publishes them (FIG-6318). `false`, with
    /// nothing held, when the census needs an exact refinement first.
    ///
    /// # Errors
    ///
    /// A fault of the census.
    pub fn hold(&mut self, bytes: u64, persistent: bool) -> Result<bool, HeapFault> {
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("a hold has no census".into()))?;
        if !matches!(
            census.reserve(LogicalBytes(bytes)),
            ReservationDecision::Granted(_)
        ) {
            return Ok(false);
        }
        self.held_now(bytes, persistent)?;
        Ok(true)
    }
    /// Refines exactly over the roots the machine has just published, with
    /// `bytes` pending: after it, a hold or write of `bytes` is granted.
    ///
    /// # Errors
    ///
    /// [`HeapWriteError::Memory`] when what is live, held and pending does
    /// not fit the bound; a fault when the roots are not published.
    pub fn refine(&mut self, bytes: u64) -> Result<(), HeapWriteError> {
        if !self.holdings.roots_current {
            return Err(HeapFault::Malformed(
                "a refinement would trace roots the machine has not published".into(),
            )
            .into());
        }
        let mut census = self
            .census
            .take()
            .ok_or_else(|| HeapFault::Malformed("a refinement has no census".into()))?;
        let result = self.refine_installed(census.as_mut(), &[], bytes);
        self.accounting = census.certificate();
        self.header = None;
        self.census = Some(census);
        result
    }
    fn held_now(&mut self, bytes: u64, persistent: bool) -> Result<(), HeapFault> {
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("a hold has no census".into()))?;
        census
            .changed(LogicalBytes(0), LogicalBytes(bytes))
            .map_err(census_fault)?;
        self.accounting = census.certificate();
        self.header = None;
        let counter = if persistent {
            &mut self.holdings.held
        } else {
            &mut self.holdings.reserved
        };
        *counter = counter
            .checked_add(bytes)
            .ok_or_else(|| HeapFault::Malformed("outstanding hold overflow".into()))?;
        Ok(())
    }
    /// The refusal of a write that needs an exact refinement of `bytes`
    /// while the installed roots are not the machine's: `None` when the
    /// refinement may run now.
    pub(super) fn refuses_precision(&mut self, bytes: u64) -> Option<HeapWriteError> {
        if self.holdings.roots_current {
            return None;
        }
        self.holdings.refused = Some(bytes);
        Some(HeapWriteError::NeedsPrecision { bytes })
    }
    /// The bytes of the refinement the last write refused for, if it
    /// refused with [`HeapWriteError::NeedsPrecision`], however its error
    /// reached the caller; clearing it.
    pub fn take_refusal(&mut self) -> Option<u64> {
        self.holdings.refused.take()
    }
    /// Releases what the running statement holds, keeping its installed
    /// roots: a cell's end seals the session priced exactly, not by the
    /// holds its last statement took on the way (FIG-6318).
    ///
    /// # Errors
    ///
    /// A fault of the census.
    pub fn release_statement_holds(&mut self) -> Result<(), HeapFault> {
        if self.holdings.reserved == 0 {
            return Ok(());
        }
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("a hold has no census".into()))?;
        census
            .changed(LogicalBytes(self.holdings.reserved), LogicalBytes(0))
            .map_err(census_fault)?;
        self.holdings.reserved = 0;
        self.accounting = census.certificate();
        self.header = None;
        Ok(())
    }
    pub fn release_statement_roots(&mut self) -> Result<(), HeapFault> {
        let before = self.accounting;
        if let Some(census) = &mut self.census {
            census
                .changed(
                    LogicalBytes(checked_sum(
                        self.holdings
                            .temporary
                            .iter()
                            .map(value_cost)
                            .chain([self.holdings.reserved]),
                    )?),
                    LogicalBytes(0),
                )
                .map_err(census_fault)?;
            self.accounting = census.certificate();
        }
        self.holdings.temporary.clear();
        self.holdings.fresh.clear();
        self.holdings.reserved = 0;
        if before != self.accounting {
            self.header = None;
        }
        Ok(())
    }
    pub(super) fn statement_roots(&self) -> &[Value] {
        &self.holdings.temporary
    }
    pub(super) fn fresh_roots(&self) -> &[EntityId] {
        &self.holdings.fresh
    }
    pub(super) fn outstanding_reserved(&self) -> u64 {
        self.holdings.reserved.saturating_add(self.holdings.held)
    }
    pub(super) fn install_transient(
        &mut self,
        values: &[Value],
        pinned: &[Value],
        fresh: &[EntityId],
    ) {
        self.holdings.temporary.clear();
        self.holdings.temporary.extend_from_slice(values);
        self.holdings.temporary.extend_from_slice(pinned);
        self.holdings.fresh.clear();
        self.holdings.fresh.extend_from_slice(fresh);
    }
    pub(super) fn refine_installed(
        &mut self,
        census: &mut dyn AllocationCensus,
        fresh: &[EntityId],
        pending: u64,
    ) -> Result<(), HeapWriteError> {
        let registers = self.holdings.temporary.clone();
        let reserved = self.outstanding_reserved();
        let mut seeds = self.holdings.fresh.clone();
        seeds.extend_from_slice(fresh);
        match census.refine(
            self,
            &TransientRoots {
                registers: &registers,
                pinned: &[],
                native_args: &[],
                fresh: &seeds,
                reserved: LogicalBytes(reserved),
            },
            PendingReservation {
                bytes: LogicalBytes(pending),
                overflowed: false,
            },
        ) {
            Ok(RefinedDecision::Granted(_)) => Ok(()),
            Ok(RefinedDecision::Refused { .. }) => Err(HeapWriteError::Memory),
            Err(error) => Err(census_fault(error).into()),
        }
    }
    /// Whether a carry collects: when the census still counts a closure
    /// entity or a task edge, which the carried bindings never reach (a
    /// binding that reaches one is not carried), and U is at most
    /// [`CARRY_COLLECTION_BYTES`], so the trace is bounded by a constant.
    /// Both facts are the heap header's, so a cell's carry decides the same
    /// however often its heap reopened mid-cell; a cell that made no
    /// function or task, a no-op one included, traces nothing, and its
    /// data garbage waits for pressure. Left in place, a dead closure would
    /// make every later carry's classification read every binding.
    pub(super) fn collects_at_carry(&self) -> bool {
        (self.population.closure_entities > 0 || self.population.task_edges > 0)
            && self
                .census
                .as_ref()
                .is_some_and(|census| census.certificate().upper.0 <= CARRY_COLLECTION_BYTES)
    }
    /// One exact refinement outside pressure: traces from the persistent
    /// roots and the installed statement roots, sweeps what it did not
    /// reach and makes U exact. Nothing is pending, so no verdict follows;
    /// a heap with no census has nothing to refine. It is refused over
    /// roots the machine has not published (FIG-6318).
    pub fn collect(&mut self) -> Result<(), HeapFault> {
        if !self.holdings.roots_current {
            return Err(HeapFault::Malformed(
                "a collection would trace roots the machine has not published".into(),
            ));
        }
        let Some(mut census) = self.census.take() else {
            return Ok(());
        };
        let result = match self.refine_installed(census.as_mut(), &[], 0) {
            Ok(()) | Err(HeapWriteError::Memory) => Ok(()),
            Err(HeapWriteError::Fault(fault)) => Err(fault),
            Err(other) => Err(HeapFault::Malformed(format!("collection failed: {other}"))),
        };
        if self.accounting != census.certificate() {
            self.accounting = census.certificate();
            self.header = None;
        }
        self.census = Some(census);
        result
    }
    pub(super) fn admit(&mut self, bytes: u64, extra: &[Value]) -> Result<(), HeapWriteError> {
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("reservation has no census".into()))?;
        if matches!(
            census.reserve(LogicalBytes(bytes)),
            ReservationDecision::Granted(_)
        ) {
            return Ok(());
        }
        if let Some(refusal) = self.refuses_precision(bytes) {
            return Err(refusal);
        }
        let fresh = self.holdings.fresh.clone();
        let mut values = self.holdings.temporary.clone();
        values.extend_from_slice(extra);
        self.reserve_logical(
            LogicalBytes(bytes),
            &TransientRoots {
                registers: &values,
                pinned: &[],
                native_args: &[],
                fresh: &fresh,
                reserved: LogicalBytes(self.outstanding_reserved()),
            },
        )
    }
    /// Installs an authenticated binding tree in place. A precision check sees
    /// the proposed roots, so dropped bindings cannot cause a false refusal.
    pub fn set_session_root(
        &mut self,
        bindings: Tree<BindingRecord>,
    ) -> Result<(), HeapWriteError> {
        let before = self.session.cost();
        let old_tasks = self.session.read_task_edges(&self.access)?;
        self.replace_execution_root(before, old_tasks, bindings, |heap, root| {
            heap.session = root
        })
    }
    pub fn set_execution_roots(
        &mut self,
        tasks: Tree<TaskRecord>,
        joins: Tree<ListJoinRecord>,
        registers: &[Value],
        pinned: &[Value],
        fresh: &[EntityId],
    ) -> Result<(), HeapWriteError> {
        // The proposed roots are current: a precision check installing them
        // traces them, and what the machine held for them is their credit.
        self.holdings.roots_current = true;
        let before = checked_sum(
            [self.tasks.cost(), self.joins.cost()]
                .into_iter()
                .chain(self.statement_roots().iter().map(value_cost)),
        )?;
        let after = checked_sum(
            [tasks.cost(), joins.cost()]
                .into_iter()
                .chain(registers.iter().chain(pinned).map(value_cost)),
        )?;
        // Republishing the installed trees changes only the statement
        // roots: their task edges are those already counted (FIG-6318).
        if tasks.same_version(&self.tasks) && joins.same_version(&self.joins) {
            self.replace_holding(before, after, 0, 0, |heap| {
                heap.install_transient(registers, pinned, fresh);
            })?;
            return self.release_held();
        }
        let old_tasks = checked_sum(
            [
                self.tasks.read_task_edges(&self.access)?,
                self.joins.read_task_edges(&self.access)?,
            ]
            .into_iter(),
        )?;
        let task_edges = tasks.read_task_edges(&self.access)?;
        let join_edges = joins.read_task_edges(&self.access)?;
        let new_tasks = checked_sum([task_edges, join_edges].into_iter())?;
        self.replace_holding(before, after, old_tasks, new_tasks, |heap| {
            heap.tasks = tasks.with_task_edges(task_edges);
            heap.joins = joins.with_task_edges(join_edges);
            heap.install_transient(registers, pinned, fresh);
        })?;
        self.release_held()
    }
    /// Releases what the machine held for its frames beyond what the roots
    /// just installed cost: they are priced exactly now (FIG-6318).
    fn release_held(&mut self) -> Result<(), HeapWriteError> {
        if self.holdings.held == 0 {
            return Ok(());
        }
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("persistent root has no census".into()))?;
        census
            .changed(LogicalBytes(self.holdings.held), LogicalBytes(0))
            .map_err(census_fault)?;
        self.holdings.held = 0;
        self.accounting = census.certificate();
        self.header = None;
        Ok(())
    }
    fn replace_execution_root<T: Cell>(
        &mut self,
        before: u64,
        old_tasks: u64,
        root: Tree<T>,
        install: impl FnOnce(&mut Self, Tree<T>),
    ) -> Result<(), HeapWriteError> {
        let after = root.cost();
        let new_tasks = root.read_task_edges(&self.access)?;
        let root = root.with_task_edges(new_tasks);
        self.replace_holding(before, after, old_tasks, new_tasks, |heap| {
            install(heap, root)
        })
    }
    fn replace_holding(
        &mut self,
        before: u64,
        after: u64,
        old_tasks: u64,
        new_tasks: u64,
        install: impl FnOnce(&mut Self),
    ) -> Result<(), HeapWriteError> {
        let before_certificate = self.accounting;
        let before_population = self.population;
        let mut census = self
            .census
            .take()
            .ok_or_else(|| HeapFault::Malformed("persistent root has no census".into()))?;
        // What the machine held for these roots pays for their growth
        // first, then what the running statement holds.
        let growth = after.saturating_sub(before);
        let from_held = self.holdings.held.min(growth);
        let from_reserved = self.holdings.reserved.min(growth - from_held);
        let credit = from_held + from_reserved;
        let decision = census.reserve(LogicalBytes(growth - credit));
        if let ReservationDecision::NeedsPrecision(pending) = &decision
            && let Some(refusal) = self.refuses_precision(pending.bytes.0)
        {
            self.census = Some(census);
            return Err(refusal);
        }
        let result = (|| {
            census
                .changed(LogicalBytes(credit), LogicalBytes(0))
                .map_err(census_fault)?;
            self.holdings.held -= from_held;
            self.holdings.reserved -= from_reserved;
            install(self);
            self.population.task_edges = self
                .population
                .task_edges
                .checked_sub(old_tasks)
                .and_then(|v| v.checked_add(new_tasks))
                .ok_or_else(|| HeapFault::Malformed("root task-edge population overflow".into()))?;
            match decision {
                ReservationDecision::Granted(_) => census
                    .changed(LogicalBytes(before), LogicalBytes(after))
                    .map_err(census_fault)
                    .map_err(HeapWriteError::from),
                ReservationDecision::NeedsPrecision(_) => {
                    let registers = self.statement_roots().to_vec();
                    let fresh = self.fresh_roots().to_vec();
                    let reserved = self.outstanding_reserved();
                    match census.refine(
                        self,
                        &TransientRoots {
                            registers: &registers,
                            pinned: &[],
                            native_args: &[],
                            fresh: &fresh,
                            reserved: LogicalBytes(reserved),
                        },
                        PendingReservation {
                            bytes: LogicalBytes(0),
                            overflowed: false,
                        },
                    ) {
                        Ok(RefinedDecision::Granted(_)) => Ok(()),
                        Ok(RefinedDecision::Refused { .. }) => Err(HeapWriteError::Memory),
                        Err(error) => Err(census_fault(error).into()),
                    }
                }
            }
        })();
        self.accounting = census.certificate();
        if before_certificate != self.accounting || before_population != self.population {
            self.header = None;
        }
        self.census = Some(census);
        result
    }
}
