use std::collections::HashMap;
use std::mem::size_of;

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{
    EntityId, FamilyId, HeapFault, HostResource, Identity, LogicalBytes, PageValue,
};

use super::malformed;
use crate::paged::{CensusFault, CensusGraph, MIN_ENTITY_COST, SweepSet, TransientRoots};

const WORDS: usize = 16;
const BLOCK_BITS: u64 = (WORDS * 64) as u64;
const BATCH: usize = 64;
pub const MAX_TRACE_MEMORY_BUDGET: usize = 64 << 20;

/// Activation-local host limits. The default resident trace budget is
/// 8 MiB, and the hard maximum is 64 MiB. It covers bitmap blocks, an
/// overestimate of hash-table storage, work-list capacity, cloned roots,
/// and bounded enumeration/sweep buffers. Growing allocations include
/// both old and replacement buffers. Maintenance is never guest work.
#[derive(Clone, Copy, Debug)]
pub struct TraceLimits {
    pub trace_memory_budget: usize,
    pub work: u64,
}

impl Default for TraceLimits {
    fn default() -> Self {
        Self {
            trace_memory_budget: 8 << 20,
            work: 100_000_000,
        }
    }
}

impl TraceLimits {
    pub(super) fn validate(self) -> Result<(), CensusFault> {
        if self.trace_memory_budget > MAX_TRACE_MEMORY_BUDGET {
            return Err(malformed("trace budget exceeds the 64 MiB hard maximum"));
        }
        Ok(())
    }
}

/// Peak allocated bitmap payload and work-list capacities, in bytes.
/// `budget_peak` additionally includes conservative index and root scratch
/// accounting, including growth overlap. No trace state is checkpointed.
#[derive(Clone, Copy, Debug, Default)]
pub struct TraceStats {
    pub bitmap_peak: usize,
    pub work_list_peak: usize,
    pub budget_peak: usize,
    pub work: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Domain {
    Family(FamilyId),
    Task,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    domain: Domain,
    block: u64,
}

#[derive(Default)]
struct Block {
    seen: [u64; WORDS],
    pending: [u64; WORDS],
    queued: bool,
}

struct Marks {
    blocks: HashMap<Key, Box<Block>>,
    work_list: Vec<Key>,
    limits: TraceLimits,
    stats: TraceStats,
    roots_bytes: usize,
}

fn exhausted(resource: HostResource) -> HeapFault {
    HeapFault::Exhausted { resource }
}
fn key(identity: Identity) -> (Key, usize, u64) {
    let (domain, ordinal) = match identity {
        Identity::Object(entity) => (Domain::Family(entity.family), entity.ordinal),
        Identity::Task(task) => (Domain::Task, task.0),
    };
    let offset = ordinal % BLOCK_BITS;
    (
        Key {
            domain,
            block: ordinal / BLOCK_BITS,
        },
        (offset / 64) as usize,
        1 << (offset % 64),
    )
}

impl Marks {
    fn new(limits: TraceLimits) -> Self {
        Self {
            blocks: HashMap::new(),
            work_list: Vec::new(),
            limits,
            stats: TraceStats::default(),
            roots_bytes: 0,
        }
    }

    fn budget(&mut self, additional: usize) -> Result<(), HeapFault> {
        // std's hash table has spare/control slots. Twice the reported
        // element capacity plus 64 bytes conservatively covers them.
        let index = self
            .blocks
            .capacity()
            .checked_mul(size_of::<(Key, Box<Block>)>() + 1)
            .and_then(|n| n.checked_mul(2))
            .and_then(|n| n.checked_add(64));
        let bytes = index
            .and_then(|n| n.checked_add(self.blocks.len() * size_of::<Block>()))
            .and_then(|n| n.checked_add(self.work_list.capacity() * size_of::<Key>()))
            .and_then(|n| n.checked_add(self.roots_bytes))
            .and_then(|n| n.checked_add(2 * BATCH * (size_of::<EntityId>() + size_of::<TaskId>())))
            .and_then(|n| n.checked_add(additional))
            .ok_or_else(|| exhausted(HostResource::TraceBudget))?;
        if bytes > self.limits.trace_memory_budget {
            return Err(exhausted(HostResource::TraceBudget));
        }
        self.stats.budget_peak = self.stats.budget_peak.max(bytes);
        self.stats.bitmap_peak = self
            .stats
            .bitmap_peak
            .max(self.blocks.len() * size_of::<Block>());
        self.stats.work_list_peak = self
            .stats
            .work_list_peak
            .max(self.work_list.capacity() * size_of::<Key>());
        Ok(())
    }

    fn tick(&mut self) -> Result<(), HeapFault> {
        self.stats.work = self
            .stats
            .work
            .checked_add(1)
            .filter(|n| *n <= self.limits.work)
            .ok_or_else(|| exhausted(HostResource::HostWork))?;
        Ok(())
    }

    fn mark(&mut self, identity: Identity) -> Result<(), HeapFault> {
        self.tick()?;
        let (key, word, bit) = key(identity);
        if !self.blocks.contains_key(&key) {
            if self.blocks.len() == self.blocks.capacity() {
                let grow = (self.blocks.capacity().max(4) * 4)
                    .checked_mul(size_of::<(Key, Box<Block>)>() + 1)
                    .ok_or_else(|| exhausted(HostResource::TraceBudget))?;
                self.budget(grow + size_of::<Block>())?;
                self.blocks
                    .try_reserve(1)
                    .map_err(|_| exhausted(HostResource::TraceBudget))?;
            } else {
                self.budget(size_of::<Block>())?;
            }
            self.blocks.insert(key, Box::default());
        }
        let block = self
            .blocks
            .get_mut(&key)
            .ok_or_else(|| HeapFault::Malformed("inserted bitmap block is absent".into()))?;
        if block.seen[word] & bit != 0 {
            return Ok(());
        }
        block.seen[word] |= bit;
        block.pending[word] |= bit;
        if !block.queued {
            block.queued = true;
            if self.work_list.len() == self.work_list.capacity() {
                let next = self.work_list.capacity().max(4) * 2;
                self.budget(next * size_of::<Key>())?;
                self.work_list
                    .try_reserve_exact(next - self.work_list.len())
                    .map_err(|_| exhausted(HostResource::TraceBudget))?;
            }
            self.work_list.push(key);
        }
        self.budget(0)
    }

    fn contains(&self, identity: Identity) -> bool {
        let (key, word, bit) = key(identity);
        self.blocks
            .get(&key)
            .is_some_and(|block| block.seen[word] & bit != 0)
    }

    fn edges(
        &mut self,
        visit: impl FnOnce(&mut dyn FnMut(Identity) -> Result<(), HeapFault>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        visit(&mut |id| self.mark(id))
    }
}

pub(super) struct Proof {
    pub reachable: LogicalBytes,
    pub entities: u64,
    marks: Marks,
}

impl Proof {
    pub fn sweep(mut self, graph: &mut dyn CensusGraph) -> Result<TraceStats, CensusFault> {
        let mut after = None;
        loop {
            let batch = graph.census_after(after, BATCH)?;
            validate_batch(&batch, after)?;
            if batch.is_empty() {
                break;
            }
            after = batch.last().copied();
            let mut unreachable = SweepSet::default();
            for entity in batch {
                self.marks.tick()?;
                if !self.marks.contains(Identity::Object(entity)) {
                    unreachable.entities.push(entity);
                }
            }
            if !unreachable.entities.is_empty() {
                graph.sweep(&unreachable)?;
            }
        }
        let mut after = None;
        loop {
            let batch = graph.ended_tasks_after(after, BATCH)?;
            validate_batch(&batch, after)?;
            if batch.is_empty() {
                break;
            }
            after = batch.last().copied();
            let mut unreachable = SweepSet::default();
            for task in batch {
                self.marks.tick()?;
                if !self.marks.contains(Identity::Task(task)) {
                    unreachable.ended_task_results.push(task);
                }
            }
            if !unreachable.ended_task_results.is_empty() {
                graph.sweep(&unreachable)?;
            }
        }
        Ok(self.marks.stats)
    }
}

pub(super) fn validate_batch<T: Ord + Copy>(
    batch: &[T],
    after: Option<T>,
) -> Result<(), CensusFault> {
    if batch.len() > BATCH
        || batch.windows(2).any(|w| w[0] >= w[1])
        || batch
            .first()
            .is_some_and(|first| after.is_some_and(|last| *first <= last))
    {
        return Err(malformed(
            "census cursor is not strictly ascending and bounded",
        ));
    }
    Ok(())
}

pub(super) fn trace(
    graph: &mut dyn CensusGraph,
    transient: &TransientRoots<'_>,
    limits: TraceLimits,
) -> Result<Proof, CensusFault> {
    limits.validate()?;
    let mut marks = Marks::new(limits);
    marks.budget(0)?;
    let mut reachable = graph.root_cost()?.0;
    let mut roots = Vec::new();
    graph.persistent_roots(&mut |value| {
        marks.tick()?;
        // Direct identities stream into the bitmap. Only immutable
        // descriptors needing the graph's summary reader wait in scratch.
        let identity = match value {
            PageValue::List(entity)
            | PageValue::Map(entity)
            | PageValue::Set(entity)
            | PageValue::Record(entity)
            | PageValue::Closure(entity) => Some(Identity::Object(*entity)),
            PageValue::Task(task) => Some(Identity::Task(*task)),
            PageValue::Ref(identity) => Some(*identity),
            _ => None,
        };
        if let Some(identity) = identity {
            return marks.mark(identity);
        }
        match value {
            PageValue::Tuple(tuple) if tuple.summary.has_edges => {}
            PageValue::Error(error) if error.has_edges => {}
            _ => return Ok(()),
        }
        if roots.len() == roots.capacity() {
            let next = roots.capacity().max(4) * 2;
            marks.budget(next * size_of::<PageValue>())?;
            roots
                .try_reserve_exact(next - roots.len())
                .map_err(|_| exhausted(HostResource::TraceBudget))?;
            marks.roots_bytes = roots.capacity() * size_of::<PageValue>();
            marks.budget(0)?;
        }
        roots.push(value.clone());
        Ok(())
    })?;
    for value in &roots {
        marks.edges(|out| graph.value_edges(value, out))?;
    }
    drop(roots);
    marks.roots_bytes = 0;
    for value in transient
        .registers
        .iter()
        .chain(transient.pinned)
        .chain(transient.native_args)
    {
        marks.tick()?;
        reachable = add(reachable, graph.value_cost(value)?.0)?;
        marks.edges(|out| graph.value_edges(value, out))?;
    }
    for value in transient.borrowed {
        marks.tick()?;
        marks.edges(|out| graph.value_edges(value, out))?;
    }
    reachable = add(reachable, transient.reserved.0)?;
    for entity in transient.fresh {
        marks.mark(Identity::Object(*entity))?;
    }
    let mut entities = 0_u64;
    while let Some(key) = marks.work_list.pop() {
        let block = marks
            .blocks
            .get_mut(&key)
            .ok_or_else(|| malformed("queued bitmap block is absent"))?;
        let pending = std::mem::take(&mut block.pending);
        block.queued = false;
        for (word, mut bits) in pending.into_iter().enumerate() {
            while bits != 0 {
                let offset = bits.trailing_zeros() as u64;
                bits &= bits - 1;
                marks.tick()?;
                let ordinal = key.block * BLOCK_BITS + word as u64 * 64 + offset;
                match key.domain {
                    Domain::Family(family) => {
                        let entity = EntityId { family, ordinal };
                        let cost = graph
                            .entity_cost(entity)?
                            .ok_or_else(|| malformed("root reaches absent entity"))?;
                        if cost.local < MIN_ENTITY_COST {
                            return Err(malformed("entity below minimum local cost"));
                        }
                        reachable = add(reachable, cost.local.0)?;
                        entities = entities
                            .checked_add(1)
                            .ok_or_else(|| malformed("reached entity count overflow"))?;
                        if cost.has_edges {
                            marks.edges(|out| graph.entity_edges(entity, out))?;
                        }
                    }
                    Domain::Task => {
                        let cost = {
                            let mut cost = LogicalBytes(0);
                            marks.edges(|out| {
                                cost = graph.task_result(TaskId(ordinal), out)?;
                                Ok(())
                            })?;
                            cost
                        };
                        reachable = add(reachable, cost.0)?;
                    }
                }
            }
        }
    }
    Ok(Proof {
        reachable: LogicalBytes(reachable),
        entities,
        marks,
    })
}

fn add(a: u64, b: u64) -> Result<u64, CensusFault> {
    a.checked_add(b)
        .ok_or_else(|| malformed("exact logical cost overflow"))
}

pub(super) fn population_cost(
    graph: &mut dyn CensusGraph,
    limits: TraceLimits,
) -> Result<(LogicalBytes, u64), CensusFault> {
    let mut marks = Marks::new(limits);
    marks.budget(0)?;
    let mut cost = graph.root_cost()?.0;
    let mut entities = 0_u64;
    let mut after = None;
    loop {
        let batch = graph.census_after(after, BATCH)?;
        validate_batch(&batch, after)?;
        if batch.is_empty() {
            break;
        }
        after = batch.last().copied();
        for entity in batch {
            marks.tick()?;
            let local = graph
                .entity_cost(entity)?
                .ok_or_else(|| malformed("census entry has no cost"))?
                .local;
            if local < MIN_ENTITY_COST {
                return Err(malformed("entity below minimum local cost"));
            }
            cost = add(cost, local.0)?;
            entities = entities
                .checked_add(1)
                .ok_or_else(|| malformed("entity count overflow"))?;
        }
    }
    let mut after = None;
    loop {
        let batch = graph.ended_tasks_after(after, BATCH)?;
        validate_batch(&batch, after)?;
        if batch.is_empty() {
            break;
        }
        after = batch.last().copied();
        for task in batch {
            marks.tick()?;
            cost = add(cost, graph.task_result(task, &mut |_| Ok(()))?.0)?;
        }
    }
    Ok((LogicalBytes(cost), entities))
}
