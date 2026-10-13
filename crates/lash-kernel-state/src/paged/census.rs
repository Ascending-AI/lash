//! S7: the memory census.
//!
//! The directory is an allocation census, not a liveness claim: it may hold
//! entities no root reaches. The census keeps U, a certified upper bound of
//! the logical cost the census and the current roots hold. A reservation
//! that fits U is granted at once, because the reachable cost L satisfies
//! L <= U. One that does not fit pauses mutation and refines: an exact
//! trace from the actual current roots, transient ones included, over the
//! pages' edge and cost summaries (never payload bytes), sweeps what it
//! did not reach and decides again. Only that exact decision can refuse
//! with `Bound::Memory`.
//!
//! No collection runs to park, resume or release a worker; no in-progress
//! trace is ever persisted; there is no concurrent collector. Retained
//! garbage stays bounded by the guest limit because every accepted
//! allocation leaves U within it and every census entity costs at least
//! [`MIN_ENTITY_COST`].

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, LogicalBytes, Value};

/// The least logical cost of any census entity: an object's header. It
/// bounds how many entities a bounded U can retain.
pub const MIN_ENTITY_COST: LogicalBytes = LogicalBytes(32);

/// The memory metric a certificate is computed under. Changing a constant
/// or a pricing rule is a new metric, never a silent reinterpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccountingMetric {
    /// The 1.0 local-cost metric: each entity its header and the values it
    /// holds at their holding locations, shared entities once by identity,
    /// inline immutable values per holding location (`K-BND-003`).
    LocalCostV1,
}

/// The census's persisted accounting, in the heap root. U is labelled an
/// upper bound: it is never exact live bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountingCertificate {
    pub metric: AccountingMetric,
    /// U: the census's local costs plus the persistent roots' costs.
    pub upper: LogicalBytes,
    /// How many entities the census holds.
    pub entities: u64,
    /// The number of exact refinements this heap has completed; it changes
    /// nothing a guest observes.
    pub refinements: u64,
}

/// One entity's accounting facts, from its directory row: what a trace
/// reads instead of its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntityCost {
    pub local: LogicalBytes,
    /// Whether the entity's body names any entity or task.
    pub has_edges: bool,
}

/// A granted reservation, drawn on by the allocation it was made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalPermit {
    pub bytes: LogicalBytes,
}

/// A reservation U alone could not grant: it waits for refinement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingReservation {
    pub bytes: LogicalBytes,
    /// `U + bytes` overflowed: the conservative sum proves nothing.
    pub overflowed: bool,
}

/// What a reservation's first check decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationDecision {
    Granted(LogicalPermit),
    NeedsPrecision(PendingReservation),
}

/// What exact refinement decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefinedDecision {
    Granted(LogicalPermit),
    /// The exact reachable cost plus the reservation exceeds the bound:
    /// the caller raises `Bound::Memory`.
    Refused {
        reachable: LogicalBytes,
    },
}

/// Why the census could not decide. Neither variant is a guest bound.
#[derive(Clone, Debug, thiserror::Error)]
pub enum CensusFault {
    /// A trace's host work or residency ran out, or a page faulted: an
    /// infrastructure outcome; the guest proceeds from its last root.
    #[error(transparent)]
    Heap(#[from] HeapFault),
    /// An admitted certificate's arithmetic is inconsistent.
    #[error("the accounting certificate is malformed: {0}")]
    Malformed(String),
}

/// Every value the running statement holds outside the heap's persistent
/// roots: registers, pinned values, a native call's arguments, fresh
/// allocations not yet stored, and outstanding reservations.
#[derive(Clone, Copy, Debug)]
pub struct TransientRoots<'a> {
    pub registers: &'a [Value],
    pub pinned: &'a [Value],
    pub native_args: &'a [Value],
    pub fresh: &'a [EntityId],
    pub reserved: LogicalBytes,
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
    /// Persistent root/control holding costs, excluding entity referents
    /// and ended-task results. Validated page summaries supply this total.
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault>;
    /// The local holding cost of a transient value under LocalCostV1.
    fn value_cost(&mut self, value: &Value) -> Result<LogicalBytes, HeapFault>;
    /// Identity edges at any immutable nesting depth, including paged
    /// tuples, read from summaries without assembling scalar payloads.
    fn value_edges(
        &mut self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// Visits every persistent root: session bindings, every task's frames
    /// and held values, task states, waits' committed outcomes, and the
    /// handles that keep ended tasks' results.
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// An entity's cost facts; `None` when the census does not hold it.
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault>;
    /// Every identity an entity's body names, from its edge summary pages:
    /// never its payload bytes.
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// What an ended task's result names and costs, when a handle reaches
    /// the task.
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<LogicalBytes, HeapFault>;
    /// Removes `entities` from the census, updating directory paths, family
    /// membership and cost aggregates. Never called with a reached entity.
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
}

/// The census and its policy.
///
/// Implemented by the accounting layer (hp-memory, FIG-6189); the paged
/// heap holds one and calls it on every allocation and edit.
pub trait AllocationCensus {
    /// The census as an admitted root restores it: U is never recomputed
    /// from resident pages and never reset.
    fn restore(
        certificate: AccountingCertificate,
        bound: LogicalBytes,
    ) -> Result<Self, CensusFault>
    where
        Self: Sized;
    fn certificate(&self) -> AccountingCertificate;
    /// Records newly allocated census entities in one checked operation.
    /// Lazy families pass their validated whole cost and occurrence count.
    fn added(&mut self, cost: LogicalBytes, entities: u64) -> Result<(), CensusFault>;
    /// Records a local cost change of a census entity or a persistent root
    /// (a field replaced, an element removed). Dropping a reference never
    /// subtracts its referent.
    fn changed(&mut self, before: LogicalBytes, after: LogicalBytes) -> Result<(), CensusFault>;
    /// The first, conservative check: checked `U + bytes <= bound`.
    fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision;
    /// Exact refinement: freezes mutation, traces from `graph`'s
    /// persistent roots and `transient`, sweeps the unreached, replaces U
    /// by the exact total and decides `pending` again. It charges no guest
    /// work and re-executes nothing.
    ///
    /// # Errors
    ///
    /// [`CensusFault`]: never a guest memory failure.
    fn refine(
        &mut self,
        graph: &mut dyn CensusGraph,
        transient: &TransientRoots<'_>,
        pending: PendingReservation,
    ) -> Result<RefinedDecision, CensusFault>;
}
