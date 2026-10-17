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

/// Minimum garbage allowance for ratio-paced collection of data-only heaps.
/// Such a heap collects after growth of at least its last live estimate,
/// amortizing tracing against intervening allocation.
pub(super) const CARRY_COLLECTION_BYTES: u64 = 4096 * MIN_ENTITY_COST.0;

#[cfg(test)]
thread_local! {
    pub(super) static RELEASE_DELTA_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Statement roots and the operation register cells they occupy.
#[derive(Clone, Copy)]
pub struct StatementRoots<'a> {
    pub registers: &'a [Value],
    pub pinned: &'a [Value],
    pub borrowed: &'a [Value],
    pub pinned_bytes: u64,
    pub prepaid: u64,
    pub fresh: &'a [EntityId],
}

/// What the machine holds outside the heap's trees: the running
/// statement's roots and holds, what it holds for its frames since it last
/// published, and whether the installed roots are its current ones.
pub(super) struct Holdings {
    temporary: Vec<Value>,
    borrowed: Vec<Value>,
    /// Pinned immediate values need their local cost, but no tracing descriptor.
    scalar_bytes: u64,
    /// Register cells already owned by an operation frame.
    pub(super) prepaid: u64,
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
            borrowed: Vec::new(),
            scalar_bytes: 0,
            prepaid: 0,
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

/// Retiring an installed working root keeps its accounting and comparison,
/// not its mutable pages. A cold root defers its edge read to the original
/// failure point; retaining that sealed source cannot pin a working leaf.
struct PreviousTasks {
    version: super::tree::Version<TaskRecord>,
    cost: u64,
    edges: Option<u64>,
    cold: Option<Tree<TaskRecord>>,
}
impl PreviousTasks {
    fn new(root: Tree<TaskRecord>) -> Self {
        let edges = root.cached_task_edges();
        Self {
            version: root.version(),
            cost: root.cost(),
            edges,
            cold: edges.is_none().then_some(root),
        }
    }
    fn edges(&self, access: &super::tree::Access<'_>) -> Result<u64, HeapFault> {
        match self.edges {
            Some(edges) => Ok(edges),
            None => self
                .cold
                .as_ref()
                .ok_or_else(|| HeapFault::Malformed("missing cold task root".into()))?
                .read_task_edges(access),
        }
    }
}

impl PagedHeap {
    /// Opens the execution roots `root` names over the heap whose header it
    /// names: that header's accounting already prices them, so nothing is
    /// charged.
    pub fn restore_execution_roots(&mut self, root: &CheckpointRoot) {
        self.session = Tree::of_keyed(&root.session);
        self.session_slots = Default::default();
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
        self.project_session()?;
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
        census
            .changed(LogicalBytes(0), LogicalBytes(bytes))
            .map_err(census_fault)?;
        // changed has checked this exact delta. Only U changes for a
        // hold; copying the census certificate repeats facts we have.
        self.accounting.upper.0 += bytes;
        self.header = None;
        let counter = if persistent {
            &mut self.holdings.held
        } else {
            &mut self.holdings.reserved
        };
        *counter = counter
            .checked_add(bytes)
            .ok_or_else(|| HeapFault::Malformed("outstanding hold overflow".into()))?;
        Ok(true)
    }
    /// Transfers an admitted frame's cell allocation from persistent holding
    /// credit to write credit, without changing its already reserved cost.
    pub fn transfer_held_to_reservation(&mut self, bytes: u64) -> Result<(), HeapFault> {
        self.holdings.held = self
            .holdings
            .held
            .checked_sub(bytes)
            .ok_or_else(|| HeapFault::Malformed("operation entry credit underflow".into()))?;
        self.holdings.reserve(bytes)
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
        self.accounting.upper.0 -= self.holdings.reserved;
        self.holdings.reserved = 0;
        self.header = None;
        Ok(())
    }
    pub fn release_statement_roots(&mut self) -> Result<(), HeapFault> {
        self.holdings.borrowed.clear();
        if self.holdings.temporary.is_empty()
            && self.holdings.scalar_bytes == 0
            && self.holdings.reserved == 0
        {
            self.holdings.fresh.clear();
            return Ok(());
        }
        let before = self.accounting;
        if let Some(census) = &mut self.census {
            #[cfg(test)]
            RELEASE_DELTA_CALLS.with(|calls| calls.set(calls.get() + 1));
            let released = checked_sum(
                self.holdings
                    .temporary
                    .iter()
                    .map(value_cost)
                    .chain([self.holdings.scalar_bytes, self.holdings.reserved]),
            )?
            .checked_sub(self.holdings.prepaid)
            .ok_or_else(|| {
                HeapFault::Malformed("register credit exceeds statement roots".into())
            })?;
            census
                .changed(LogicalBytes(released), LogicalBytes(0))
                .map_err(census_fault)?;
            self.accounting.upper.0 -= released;
        }
        self.holdings.temporary.clear();
        self.holdings.scalar_bytes = 0;
        self.holdings.prepaid = 0;
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
    fn transient_reserved(&self) -> Result<LogicalBytes, HeapFault> {
        checked_sum([self.outstanding_reserved(), self.holdings.scalar_bytes].into_iter())
            .map(LogicalBytes)
    }
    pub(super) fn install_transient(&mut self, roots: StatementRoots<'_>) {
        let StatementRoots {
            registers: values,
            pinned,
            pinned_bytes,
            prepaid,
            borrowed,
            fresh,
        } = roots;
        self.holdings.temporary.clear();
        self.holdings.temporary.extend_from_slice(values);
        self.holdings.temporary.extend_from_slice(pinned);
        self.holdings.scalar_bytes = pinned_bytes;
        self.holdings.prepaid = prepaid;
        self.holdings.borrowed.clear();
        self.holdings.borrowed.extend_from_slice(borrowed);
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
        let borrowed = self.holdings.borrowed.clone();
        let reserved = self.transient_reserved()?;
        let mut seeds = self.holdings.fresh.clone();
        seeds.extend_from_slice(fresh);
        let decision = census.refine(
            self,
            &TransientRoots {
                borrowed: &borrowed,
                registers: &registers,
                pinned: &[],
                native_args: &[],
                fresh: &seeds,
                reserved,
            },
            PendingReservation {
                bytes: LogicalBytes(pending),
                overflowed: false,
            },
        );
        if decision.is_ok() {
            self.collection_live = Some(census.certificate().upper);
        }
        match decision {
            Ok(RefinedDecision::Granted(_)) => Ok(()),
            Ok(RefinedDecision::Refused { .. }) => Err(HeapWriteError::Memory),
            Err(error) => Err(census_fault(error).into()),
        }
    }
    /// A carry that must classify closures or task edges already walks its
    /// live graph. Reclaim its cell-local garbage too, even when immutable
    /// scalar payloads make the byte census large: keeping those payloads
    /// shares their pages, while keeping dead families rewrites their growing
    /// reference table at each park (FIG-6681).
    ///
    /// Data-only carries collect after growth of at least max(last live
    /// estimate, 128 KiB). A no-op carry spends none of that allowance. Every
    /// input is persisted in the heap header, independent of checkpoint cuts.
    pub(super) fn collects_at_carry(&self) -> bool {
        self.census.as_ref().is_some_and(|census| {
            let upper = census.certificate().upper.0;
            self.population.closure_entities > 0
                || self.population.task_edges > 0
                || self.collection_live.is_some_and(|live| {
                    upper.saturating_sub(live.0) >= live.0.max(CARRY_COLLECTION_BYTES)
                })
        })
    }
    /// Seed pacing once without tracing the initial, potentially huge,
    /// live population. Later exact refinements replace this estimate.
    pub(super) fn carried_collection_estimate(&mut self) {
        if self.collection_live.is_none() {
            self.collection_live = Some(self.accounting.upper);
            self.header = None;
        }
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
        let borrowed = self.holdings.borrowed.clone();
        values.extend_from_slice(extra);
        self.reserve_logical(
            LogicalBytes(bytes),
            &TransientRoots {
                borrowed: &borrowed,
                registers: &values,
                pinned: &[],
                native_args: &[],
                fresh: &fresh,
                reserved: self.transient_reserved()?,
            },
        )
    }
    /// Installs an authenticated binding tree in place. A precision check sees
    /// the proposed roots, so dropped bindings cannot cause a false refusal.
    pub fn set_session_root(
        &mut self,
        bindings: Tree<BindingRecord>,
    ) -> Result<(), HeapWriteError> {
        self.project_session()?;
        let before = self.session.cost();
        let old_tasks = self.session.read_task_edges(&self.access)?;
        self.replace_execution_root(before, old_tasks, bindings, |heap, root| {
            heap.session = root
        })
    }
    /// Installs execution and statement roots together. `pinned_bytes`
    /// prices immediate pins omitted from `pinned`: it remains a holding
    /// location in both conservative accounting and exact refinement,
    /// never reservation credit for a write.
    pub fn set_execution_roots(
        &mut self,
        tasks: Tree<TaskRecord>,
        joins: Tree<ListJoinRecord>,
        registers: &[Value],
        pinned: &[Value],
        pinned_bytes: u64,
        fresh: &[EntityId],
    ) -> Result<(), HeapWriteError> {
        let previous = PreviousTasks::new(self.tasks.clone());
        self.install_execution_roots(
            (previous, tasks),
            joins,
            StatementRoots {
                registers,
                pinned,
                pinned_bytes,
                prepaid: 0,
                borrowed: &[],
                fresh,
            },
        )
    }
    /// Synchronously consumes obsolete installed task ownership before the
    /// machine updates its authoritative rows. A failed edit leaves roots
    /// unpublished; no refinement may trace a partially updated graph.
    /// `pinned_bytes` has the same holding-location meaning as in
    /// [`Self::set_execution_roots`].
    pub fn update_execution_roots(
        &mut self,
        joins: Tree<ListJoinRecord>,
        roots: StatementRoots<'_>,
        update: impl FnOnce() -> Result<Tree<TaskRecord>, HeapFault>,
    ) -> Result<(), HeapWriteError> {
        self.holdings.roots_current = false;
        let previous = PreviousTasks::new(std::mem::take(&mut self.tasks));
        let tasks = update()?;
        self.install_execution_roots((previous, tasks), joins, roots)
    }
    fn install_execution_roots(
        &mut self,
        (previous, tasks): (PreviousTasks, Tree<TaskRecord>),
        joins: Tree<ListJoinRecord>,
        roots: StatementRoots<'_>,
    ) -> Result<(), HeapWriteError> {
        let StatementRoots {
            registers,
            pinned,
            pinned_bytes,
            prepaid,
            ..
        } = roots;
        // The proposed roots are current: a precision check installing them
        // traces them, and what the machine held for them is their credit.
        self.holdings.roots_current = true;
        let before = checked_sum(
            [previous.cost, self.joins.cost()]
                .into_iter()
                .chain(self.statement_roots().iter().map(value_cost))
                .chain([self.holdings.scalar_bytes]),
        )?
        .checked_sub(self.holdings.prepaid)
        .ok_or_else(|| HeapFault::Malformed("register credit exceeds installed roots".into()))?;
        let after = checked_sum(
            [tasks.cost(), joins.cost()]
                .into_iter()
                .chain(registers.iter().chain(pinned).map(value_cost))
                .chain([pinned_bytes]),
        )?
        .checked_sub(prepaid)
        .ok_or_else(|| HeapFault::Malformed("register credit exceeds proposed roots".into()))?;
        // Republishing the installed trees changes only the statement
        // roots: their task edges are those already counted (FIG-6318).
        if previous.version.matches(&tasks) && joins.same_version(&self.joins) {
            self.replace_holding(before, after, 0, 0, |heap| {
                heap.tasks = tasks;
                heap.install_transient(roots);
            })?;
            return self.release_held();
        }
        let old_tasks = checked_sum(
            [
                previous.edges(&self.access)?,
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
            heap.install_transient(roots);
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
        self.accounting.upper.0 -= self.holdings.held;
        self.holdings.held = 0;
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
    pub(super) fn replace_holding(
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
                    let borrowed = self.holdings.borrowed.clone();
                    let fresh = self.fresh_roots().to_vec();
                    let reserved = self.transient_reserved()?;
                    match census.refine(
                        self,
                        &TransientRoots {
                            borrowed: &borrowed,
                            registers: &registers,
                            pinned: &[],
                            native_args: &[],
                            fresh: &fresh,
                            reserved,
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
