//! Publication (FIG-6318, FIG-6425): the one module that installs the
//! machine's execution roots and statement roots into the heap and decides
//! when a sweep may trace. Every root tree the census reads is installed
//! here, and no other module changes what the machine holds.
use super::directory::execution::TaskRecord;
use super::heap_impl::{PagedHeap, census_fault};
use super::resident::ResidentRoots;
use super::tree::{Cell, SealWork, Tree, checked_sum};
use super::*;
use lash_kernel_doc::heap::*;

/// The running statement's roots.
#[derive(Clone, Copy)]
pub struct StatementRoots<'a> {
    pub registers: &'a [PageValue],
    pub pinned: &'a [PageValue],
    pub borrowed: &'a [PageValue],
}

/// What the machine holds outside the heap's trees: the running
/// statement's roots, and whether the installed roots are its current ones.
pub(super) struct Holdings {
    temporary: Vec<PageValue>,
    borrowed: Vec<PageValue>,
    /// Whether the installed task and join trees are the execution state's
    /// current roots. A machine clears it once its state moves past what it
    /// published; a collection then refuses rather than trace them
    /// (FIG-6318).
    roots_current: bool,
}

impl Default for Holdings {
    /// What a heap holds when it opens: nothing, over current roots.
    fn default() -> Self {
        Self {
            temporary: Vec::new(),
            borrowed: Vec::new(),
            roots_current: true,
        }
    }
}

/// Retiring an installed working root keeps its comparison, not its
/// mutable pages. A cold root defers its edge read to the original failure
/// point; retaining that sealed source cannot pin a working leaf.
struct PreviousTasks {
    version: super::tree::Version<TaskRecord>,
    edges: Option<u64>,
    cold: Option<Tree<TaskRecord>>,
}
impl PreviousTasks {
    fn new(root: Tree<TaskRecord>) -> Self {
        let edges = root.cached_task_edges();
        Self {
            version: root.version(),
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
    /// [`Self::set_execution_roots`]: until the next installation, a
    /// collection refuses rather than trace roots the machine has not
    /// published (FIG-6318).
    pub fn execution_roots_moved(&mut self) {
        if self.unmetered() {
            return;
        }
        self.holdings.roots_current = false;
    }
    /// Sweeps over what the machine holds in memory (FIG-6748): nothing is
    /// projected, published or given an id. The machine walks its
    /// holdings ([`ResidentRoots::walk`]); the census traces them over the
    /// heap's pages and removes what it did not reach. It retires the
    /// results of the ended tasks nothing reaches only when `retires`;
    /// otherwise it keeps every retained result and what it names. A heap
    /// with no census has nothing to sweep.
    ///
    /// # Errors
    ///
    /// A fault of the walk, the trace or the census.
    pub fn refine_resident(
        &mut self,
        resident: &mut dyn ResidentRoots,
        retires: bool,
    ) -> Result<(), HeapFault> {
        if self.census.is_none() {
            return Ok(());
        }
        let walked = match resident.walk() {
            Ok(walked) => walked,
            Err(fault) => {
                resident.finish();
                return Err(fault);
            }
        };
        let swept = self.refine_walked(resident, &walked, retires);
        resident.finish();
        swept
    }
    fn refine_walked(
        &mut self,
        resident: &mut dyn ResidentRoots,
        walked: &super::resident::Walked,
        retires: bool,
    ) -> Result<(), HeapFault> {
        self.holdings.temporary.clear();
        self.holdings.borrowed.clear();
        let mut census = self
            .census
            .take()
            .ok_or_else(|| HeapFault::Malformed("a sweep has no census".into()))?;
        let swept = census
            .refine(
                &mut super::resident::ResidentGraph {
                    heap: self,
                    resident,
                    walked,
                    retires,
                },
                &TransientRoots {
                    borrowed: &[],
                    registers: &[],
                    pinned: &[],
                    native_args: &[],
                    fresh: &[],
                },
            )
            .map_err(census_fault);
        if swept.is_ok() {
            self.collection_entities = Some(census.certificate().entities);
        }
        self.accounting = census.certificate();
        self.header = None;
        self.census = Some(census);
        swept
    }
    /// Releases the running statement's roots, keeping its installed
    /// roots.
    #[inline]
    pub fn release_statement_roots(&mut self) {
        self.holdings.borrowed.clear();
        self.holdings.temporary.clear();
    }
    pub(super) fn install_transient(&mut self, roots: StatementRoots<'_>) {
        if self.unmetered() {
            return;
        }
        let StatementRoots {
            registers,
            pinned,
            borrowed,
        } = roots;
        self.holdings.temporary.clear();
        self.holdings.temporary.extend_from_slice(registers);
        self.holdings.temporary.extend_from_slice(pinned);
        self.holdings.borrowed.clear();
        self.holdings.borrowed.extend_from_slice(borrowed);
    }
    pub(super) fn refine_installed(
        &mut self,
        census: &mut dyn AllocationCensus,
        fresh: &[EntityId],
    ) -> Result<(), HeapFault> {
        let registers = self.holdings.temporary.clone();
        let borrowed = self.holdings.borrowed.clone();
        census
            .refine(
                self,
                &TransientRoots {
                    borrowed: &borrowed,
                    registers: &registers,
                    pinned: &[],
                    native_args: &[],
                    fresh,
                },
            )
            .map_err(census_fault)?;
        self.collection_entities = Some(census.certificate().entities);
        Ok(())
    }
    /// A carry that must classify closures or task edges already walks its
    /// live graph. Reclaim its cell-local garbage too, even when immutable
    /// scalar payloads dominate its pages: keeping those payloads shares
    /// their pages, while keeping dead families rewrites their growing
    /// reference table at each park (FIG-6681).
    ///
    /// Data-only carries collect after entity growth of at least
    /// max(ceil(last live entity estimate / 2), 1). The entity count
    /// includes lazy family members: large scalar payloads cannot starve
    /// their cleanup (FIG-6750). A no-op carry spends no allowance. The
    /// estimate persists across cold opens and checkpoint cuts.
    pub(super) fn collects_at_carry(&self) -> bool {
        self.census.as_ref().is_some_and(|census| {
            self.population.closure_entities > 0
                || self.population.task_edges > 0
                || self.collection_entities.is_some_and(|live| {
                    census.certificate().entities.saturating_sub(live) >= live.div_ceil(2).max(1)
                })
        })
    }
    /// Seed pacing once without tracing the initial, potentially huge,
    /// live population. Later sweeps replace this estimate.
    pub(super) fn carried_collection_estimate(&mut self) {
        if self.collection_entities.is_none() {
            self.collection_entities = Some(self.accounting.entities);
            self.header = None;
        }
    }
    /// One sweep outside pressure: traces from the persistent roots and the
    /// installed statement roots and removes what it did not reach. A heap
    /// with no census has nothing to sweep. It is refused over roots the
    /// machine has not published (FIG-6318).
    pub fn collect(&mut self) -> Result<(), HeapFault> {
        if self.unmetered() {
            return Ok(());
        }
        if !self.holdings.roots_current {
            return Err(HeapFault::Malformed(
                "a collection would trace roots the machine has not published".into(),
            ));
        }
        let Some(mut census) = self.census.take() else {
            return Ok(());
        };
        let result = self.refine_installed(census.as_mut(), &[]);
        if self.accounting != census.certificate() {
            self.accounting = census.certificate();
            self.header = None;
        }
        self.census = Some(census);
        result
    }
    /// Installs an authenticated binding tree in place.
    pub fn set_session_root(
        &mut self,
        bindings: Tree<BindingRecord>,
    ) -> Result<(), HeapWriteError> {
        self.project_session()?;
        let old_tasks = self.session.read_task_edges(&self.access)?;
        self.replace_execution_root(old_tasks, bindings, |heap, root| heap.session = root)
    }
    /// Installs execution and statement roots together.
    pub fn set_execution_roots(
        &mut self,
        tasks: Tree<TaskRecord>,
        joins: Tree<ListJoinRecord>,
        registers: &[PageValue],
        pinned: &[PageValue],
    ) -> Result<(), HeapWriteError> {
        let previous = PreviousTasks::new(self.tasks.clone());
        self.install_execution_roots(
            (previous, tasks),
            joins,
            StatementRoots {
                registers,
                pinned,
                borrowed: &[],
            },
        )
    }
    /// Synchronously consumes obsolete installed task ownership before the
    /// machine updates its authoritative rows. A failed edit leaves roots
    /// unpublished; no sweep may trace a partially updated graph.
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
        // The proposed roots are current: a collection traces them.
        if !self.unmetered() {
            self.holdings.roots_current = true;
        }
        // Republishing the installed trees changes only the statement
        // roots: their task edges are those already counted (FIG-6318).
        if previous.version.matches(&tasks) && joins.same_version(&self.joins) {
            self.tasks = tasks;
            self.install_transient(roots);
            return Ok(());
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
        self.replace_holding(old_tasks, new_tasks, |heap| {
            heap.tasks = tasks.with_task_edges(task_edges);
            heap.joins = joins.with_task_edges(join_edges);
            heap.install_transient(roots);
        })?;
        Ok(())
    }
    fn replace_execution_root<T: Cell>(
        &mut self,
        old_tasks: u64,
        root: Tree<T>,
        install: impl FnOnce(&mut Self, Tree<T>),
    ) -> Result<(), HeapWriteError> {
        let new_tasks = root.read_task_edges(&self.access)?;
        let root = root.with_task_edges(new_tasks);
        self.replace_holding(old_tasks, new_tasks, |heap| install(heap, root))
    }
    /// Installs a root holding `new_tasks` task edges in place of one that
    /// held `old_tasks`.
    pub(super) fn replace_holding(
        &mut self,
        old_tasks: u64,
        new_tasks: u64,
        install: impl FnOnce(&mut Self),
    ) -> Result<(), HeapWriteError> {
        install(self);
        self.population.task_edges = self
            .population
            .task_edges
            .checked_sub(old_tasks)
            .and_then(|v| v.checked_add(new_tasks))
            .ok_or_else(|| HeapFault::Malformed("root task-edge population overflow".into()))?;
        self.header = None;
        Ok(())
    }
}
