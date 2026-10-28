//! S7: the allocation census.
//!
//! The directory is an allocation census, not a liveness claim: it may hold
//! entities no root reaches. The census counts its entities and how many
//! sweeps it has completed. A sweep pauses mutation and traces from the
//! actual current roots, transient ones included, over the pages' edge
//! summaries (never payload bytes), and removes what it did not reach.
//!
//! A sweep also records logical cost components for its reached bodies and
//! supplied root owners. Guest admission currently uses the machine's capacity
//! meter (FIG-6791). Its sweep generation is what the machine reloads its
//! swept task roots on.
//!
//! No collection runs to park, resume or release a worker; no in-progress
//! trace is ever persisted; there is no concurrent collector.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};

/// What a certificate's capacity counts. Changing what the meter counts is
/// a new metric, never a silent reinterpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccountingMetric {
    /// The 1.0 capacity meter (FIG-6791): resident arena value slots at 16
    /// bytes each, with long text and bytes at 16 bytes a slot, and the
    /// control state's frames and their slots.
    Capacity,
    /// The heap was written without a memory meter: no census holds its
    /// entities.
    Unmetered,
}

/// The census's persisted accounting, in the heap root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountingCertificate {
    pub metric: AccountingMetric,
    /// The bytes the capacity meter counted the run holding when it was
    /// last sealed: what a run opened from this heap holds until its first
    /// sweep recounts.
    pub capacity: u64,
    /// How many entities the census holds.
    pub entities: u64,
    /// The number of sweeps this heap has completed: the generation a
    /// machine reloads its swept task roots on. It changes nothing a guest
    /// observes.
    pub refinements: u64,
}

impl AccountingCertificate {
    /// A fresh heap's accounting: nothing allocated, nothing swept.
    pub const FRESH: Self = Self {
        metric: AccountingMetric::Capacity,
        capacity: 0,
        entities: 0,
        refinements: 0,
    };
}

/// One entity's census facts, from its directory row: what a trace reads
/// instead of its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntityFacts {
    /// Full direct logical cost, including cold member occurrences.
    pub local_bytes: u64,
    /// Whether the entity's body names any entity or task.
    pub has_edges: bool,
}

/// Logical cost components from a completed normalization. Borrowed trace roots carry
/// no occurrence charge; mutable bodies are priced once by identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NormalizedHoldings {
    pub entity_local_bytes: u64,
    pub root_and_control_bytes: u64,
}

/// Why the census could not sweep. Neither variant is a guest bound.
#[derive(Clone, Debug, thiserror::Error)]
pub enum CensusFault {
    /// A trace's host work or residency ran out, or a page faulted: an
    /// infrastructure outcome; the guest proceeds from its last root.
    #[error(transparent)]
    Heap(#[from] HeapFault),
    /// An admitted certificate is inconsistent.
    #[error("the accounting certificate is malformed: {0}")]
    Malformed(String),
}

/// Every value the running statement holds outside the heap's persistent
/// roots: registers, pinned values, a native call's arguments and fresh
/// allocations not yet stored.
#[derive(Clone, Copy, Debug)]
pub struct TransientRoots<'a> {
    /// Reachability during an ownership transfer, with no new allocation.
    pub borrowed: &'a [PageValue],
    pub registers: &'a [PageValue],
    pub pinned: &'a [PageValue],
    pub native_args: &'a [PageValue],
    pub fresh: &'a [EntityId],
}

/// The proved-unreachable portion of a quiescent census. Result payloads
/// are retired together with entity membership; task obligation metadata
/// remains owned by the execution records.
#[derive(Clone, Debug, Default)]
pub struct SweepSet {
    pub entities: Vec<EntityId>,
    pub ended_task_results: Vec<TaskId>,
}

/// The graph the census traces, as the paged heap exposes it.
///
/// Implemented by the paged heap (layer hp-heap, FIG-6188).
pub trait CensusGraph {
    /// Current source-owned root occurrences and frame headers. Trace mirrors
    /// and bookkeeping references are excluded.
    fn root_and_control_bytes(&mut self) -> Result<u64, HeapFault>;
    /// Mutable bodies not yet assigned durable identities, reached by the
    /// resident root walk. They are disjoint from entity_facts.
    fn anonymous_entity_bytes(&self) -> u64 {
        0
    }
    /// The local occurrence price of a retained ended result. A result is
    /// charged only when the trace keeps its task.
    fn task_result_bytes(&mut self, task: TaskId) -> Result<u64, HeapFault>;
    /// Identity edges at any immutable nesting depth, including paged
    /// tuples, read from summaries without assembling scalar payloads.
    fn value_edges(
        &mut self,
        value: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// Visits every persistent root: session bindings, every task's frames
    /// and held values, task states, waits' committed outcomes, and the
    /// handles that keep ended tasks' results.
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// An entity's census facts; `None` when the census does not hold it.
    fn entity_facts(&mut self, entity: EntityId) -> Result<Option<EntityFacts>, HeapFault>;
    /// Every identity an entity's body names, from its edge summary pages:
    /// never its payload bytes.
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// What an ended task's result names, when a handle reaches the task.
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// Removes `entities` from the census, updating directory paths and
    /// family membership. Never called with a reached entity.
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault>;
    /// Every entity the census holds, ascending, after `after`.
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault>;
    /// Retained ended-task result payloads, unique and ascending.
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault>;
    /// Whether a sweep of this graph retires the results of the ended
    /// tasks nothing reaches. A graph that does not keeps every retained
    /// result, and what it names, as a root: a sweep the run's capacity
    /// set off, at no point the run's images fix, changes nothing they
    /// show (K-MACH-008, FIG-6791).
    fn retires_results(&self) -> bool {
        true
    }
    /// The entities a run changed in memory since it last projected them,
    /// or gave ids and never projected, ascending: the census does not
    /// hold what they are now. A graph with nothing unprojected has none.
    fn unprojected(&mut self) -> Vec<EntityId> {
        Vec::new()
    }
    /// Forgets the changes to `entities`, which nothing reaches: no
    /// projection writes them, and the run's memory frees them.
    fn forget(&mut self, entities: &[EntityId]) {
        let _ = entities;
    }
}

/// The census.
///
/// Implemented by the accounting layer (hp-memory, FIG-6189); the paged
/// heap holds one and calls it on every allocation.
pub trait AllocationCensus {
    /// The census as an admitted root restores it: nothing is recounted
    /// from resident pages.
    fn restore(certificate: AccountingCertificate) -> Result<Self, CensusFault>
    where
        Self: Sized;
    fn certificate(&self) -> AccountingCertificate;
    /// Records newly allocated census entities in one checked operation.
    /// Lazy families pass their occurrence count.
    fn added(&mut self, entities: u64) -> Result<(), CensusFault>;
    /// Records the capacity the run holds as a seal writes it.
    fn record_capacity(&mut self, capacity: u64);
    /// A sweep: freezes mutation, traces from `graph`'s persistent roots
    /// and `transient`, removes the unreached, recounts the entities and
    /// advances the sweep generation. It charges no guest work, decides
    /// nothing a guest observes and re-executes nothing.
    ///
    /// # Errors
    ///
    /// [`CensusFault`]: never a guest memory failure.
    fn refine(
        &mut self,
        graph: &mut dyn CensusGraph,
        transient: &TransientRoots<'_>,
    ) -> Result<(), CensusFault>;
}
