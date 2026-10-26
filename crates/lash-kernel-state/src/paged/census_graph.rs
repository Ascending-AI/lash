//! The heap as the census traces it (S7): its persistent roots, its
//! entities and the identity edges their bodies name, streamed from
//! directory rows and edge summaries, never from scalar payloads.
use super::census::{CensusGraph, EntityFacts, SweepSet};
use super::directory::{Body, entity_key, ordinal_key};
use super::heap_impl::PagedHeap;
use super::tree::{Access, Cell, Tree};
use super::*;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::*;
use std::ops::ControlFlow;
use std::rc::Rc;

impl CensusGraph for PagedHeap {
    fn value_edges(
        &mut self,
        value: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.stream_edges(value, out)
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.visit_saved_environments(visit)?;
        self.visit_session_bindings(&mut |b| binding_values(b, visit))?;
        visit_tree(&self.joins, &self.access, &mut |join| {
            visit_tree(&join.members, &self.access, &mut |member| {
                visit(&PageValue::Task(*member))
            })
        })?;
        visit_tree(&self.tasks, &self.access, &mut |task| {
            state_values(&task.state, visit)?;
            visit_tree(&task.frames, &self.access, &mut |frame| {
                frame_values(frame, &self.access, visit)
            })
        })
    }
    fn entity_facts(&mut self, entity: EntityId) -> Result<Option<EntityFacts>, HeapFault> {
        Ok(self
            .current_head(entity)?
            .map(|_| EntityFacts { has_edges: true }))
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(head) = self.current_head(entity)? else {
            return Ok(());
        };
        let mut error = None;
        #[cfg(test)]
        TRACED_ROWS.with(|rows| rows.set(rows.get() + head.body.rows()));
        match &head.body {
            Body::Variable(v) => return self.stream_edges(v, out),
            Body::Closure { captures, .. } => captures.visit(
                &self.access,
                &mut |_, id| match out(Identity::Object(id.1)) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                },
            )?,
            Body::List { rows, .. } => {
                rows.visit(&self.access, &mut |_, v| match self.stream_edges(v, out) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                })?
            }
            Body::Table { rows, .. } => rows.visit(&self.access, &mut |_, e| match self
                .stream_edges(&e.key, out)
                .and_then(|()| self.stream_edges(&e.value, out))
            {
                Ok(()) => true,
                Err(e) => {
                    error = Some(e);
                    false
                }
            })?,
        }
        error.map_or(Ok(()), Err)
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(task) = self.tasks.get(&task.0.to_be_bytes(), &self.access)? else {
            return Ok(());
        };
        match &task.state {
            TaskStateRecord::Ended(ended) => match ended.retained() {
                Some(v) => self.stream_edges(v, out),
                None => Ok(()),
            },
            _ => Ok(()),
        }
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        for entity in &unreachable.entities {
            if entity.family != FamilyId::PRIMARY {
                self.sweep_family_member(*entity)?;
            }
            // A working head is the entity's current version: when the
            // entity owns a row, its populations are the head's.
            let working = self.sweep_working(*entity);
            let stored = self.directory.remove(&entity_key(*entity), &self.access)?;
            let head = match (working, stored) {
                (Some((head, true)), _) => Some(Rc::unwrap_or_clone(head)),
                (Some((_, false)), _) => None,
                (None, stored) => stored,
            };
            if let Some(head) = head {
                self.population.closure_entities = self
                    .population
                    .closure_entities
                    .checked_sub((head.kind() == ObjectKind::Closure) as u64)
                    .ok_or_else(|| HeapFault::Malformed("sweep population underflow".into()))?;
                self.population.task_edges = self
                    .population
                    .task_edges
                    .checked_sub(head.task_edges())
                    .ok_or_else(|| HeapFault::Malformed("sweep task-edge underflow".into()))?;
            }
        }
        for task in &unreachable.ended_task_results {
            if let Some(mut row) = self.tasks.get(&task.0.to_be_bytes(), &self.access)? {
                let old_edges = row.task_edges();
                let TaskStateRecord::Ended(ended) = &mut row.state else {
                    return Err(HeapFault::Malformed(
                        "sweep named a running task result".into(),
                    ));
                };
                ended.retire();
                self.population.task_edges = self
                    .population
                    .task_edges
                    .checked_sub(old_edges)
                    .and_then(|value| value.checked_add(row.task_edges()))
                    .ok_or_else(|| {
                        HeapFault::Malformed("retired task population underflow".into())
                    })?;
                self.tasks.write(ordinal_key(task.0), row, &self.access)?;
            }
        }
        self.header = None;
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        // The census enumerates the directory: it receives every edit first.
        self.flush_working()?;
        let mut entities = Vec::new();
        if after.is_none_or(|id| id.family == FamilyId::PRIMARY) {
            let mut key = after.map(entity_key);
            while entities.len() < limit {
                let Some((next, head)) = self.directory.after(key.as_deref(), &self.access)? else {
                    break;
                };
                if head.family != FamilyId::PRIMARY {
                    break;
                }
                let ordinal = u64::from_be_bytes(
                    next.get(8..)
                        .ok_or_else(|| HeapFault::Malformed("identity key malformed".into()))?
                        .try_into()
                        .map_err(|_| HeapFault::Malformed("identity key malformed".into()))?,
                );
                entities.push(EntityId {
                    family: FamilyId::PRIMARY,
                    ordinal,
                });
                key = Some(next.into());
            }
        }
        let mut key = after
            .and_then(|id| id.family.0.checked_sub(1))
            .map(ordinal_key);
        while entities.len() < limit {
            let Some((next, family)) = self.families.after(key.as_deref(), &self.access)? else {
                break;
            };
            let runs = Tree::<MembershipRun>::of_keyed(&family.membership);
            let first = after
                .filter(|id| id.family == family.family)
                .and_then(|id| id.ordinal.checked_add(1))
                .unwrap_or(0);
            let mut rank = runs
                .passed(&first.to_be_bytes(), &self.access)?
                .saturating_sub(1);
            while entities.len() < limit {
                let Some((_, run)) = runs.rank_get(rank, &self.access)? else {
                    break;
                };
                let end = run
                    .start
                    .checked_add(run.len)
                    .ok_or_else(|| HeapFault::Malformed("family run extent overflow".into()))?;
                if end > family.extent {
                    return Err(HeapFault::Corrupt {
                        page: family.base.root.id,
                        reason: "membership exceeds family extent".into(),
                    });
                }
                let count =
                    (end.saturating_sub(run.start.max(first))).min((limit - entities.len()) as u64);
                for ordinal in run.start.max(first)..run.start.max(first) + count {
                    entities.push(EntityId {
                        family: family.family,
                        ordinal,
                    });
                }
                rank += 1;
            }
            key = Some(next.into());
        }
        Ok(entities)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        let mut key = after.map(|t| ordinal_key(t.0));
        let mut tasks = Vec::new();
        while tasks.len() < limit {
            let Some((next, row)) = self.tasks.after(key.as_deref(), &self.access)? else {
                break;
            };
            if !matches!(&row.state, TaskStateRecord::Ended(ended) if ended.retained().is_some()) {
                key = Some(next.into());
                continue;
            }
            tasks.push(TaskId(u64::from_be_bytes(
                next.as_slice()
                    .try_into()
                    .map_err(|_| HeapFault::Malformed("task identity key is not 8 bytes".into()))?,
            )));
            key = Some(next.into());
        }
        Ok(tasks)
    }
}
impl PagedHeap {
    /// Streams nested immutable edges in place, stopping on the first visitor
    /// failure. Scalar payloads are never loaded by a trace.
    pub fn stream_edges(
        &self,
        value: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id)
            | PageValue::Closure(id) => out(Identity::Object(*id))?,
            PageValue::Task(id) => out(Identity::Task(*id))?,
            PageValue::Ref(id) => out(*id)?,
            PageValue::Tuple(t) if t.summary.has_edges => {
                let mut error = None;
                self.tuple_visit(t, &mut |v| match self.stream_edges(v, out) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        error = Some(e);
                        ControlFlow::Break(())
                    }
                })?;
                if let Some(e) = error {
                    return Err(e);
                }
            }
            PageValue::Error(e) if e.has_edges => self.stream_edges(&self.error_data(e)?, out)?,
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// The body rows this thread's census traces have visited (FIG-6425).
    pub(super) static TRACED_ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Body {
    /// The rows a trace visits to stream this body's edges.
    #[cfg(test)]
    fn rows(&self) -> u64 {
        match self {
            Self::Variable(_) => 1,
            Self::Closure { captures, .. } => captures.len(),
            Self::List { rows, .. } => rows.len(),
            Self::Table { rows, .. } => rows.len(),
        }
    }
}

/// Visits the value a binding holds: a shared variable as a ref to its
/// cell.
pub(crate) fn binding_values(
    binding: &BindingRecord,
    visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    match &binding.value {
        BoundRecord::Value(v) => visit(v),
        BoundRecord::Shared(id) => visit(&PageValue::Ref(Identity::Object(*id))),
    }
}

/// Visits what a task's state holds: a value or raise it resumes with, or
/// the task it joins.
pub fn state_values(
    state: &TaskStateRecord,
    visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    match state {
        TaskStateRecord::Resuming {
            incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
            ..
        } => visit(v),
        TaskStateRecord::Resuming {
            incoming: IncomingRecord::Joined(t),
            ..
        }
        | TaskStateRecord::Joining(t) => visit(&PageValue::Task(*t)),
        _ => Ok(()),
    }
}

/// Visits what a frame's row holds: its bindings, what its loops walk,
/// its departing payloads, its call's arguments and its continuation's
/// roles.
pub fn frame_values(
    frame: &FrameRecord,
    access: &Access<'_>,
    visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    visit_tree(&frame.bindings, access, &mut |b| binding_values(b, visit))?;
    visit_tree(&frame.iterated, access, &mut |cursor| visit(&cursor.0))?;
    visit_tree(&frame.departing, access, visit)?;
    if let Some(args) = &frame.arguments {
        visit_tree(args, access, visit)?;
    }
    if let Some(pending) = &frame.pending {
        pending.visit_values(access, visit)?;
    }
    Ok(())
}

pub(crate) fn visit_tree<T: Cell>(
    tree: &Tree<T>,
    access: &Access<'_>,
    visit: &mut dyn FnMut(&T) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    let mut error = None;
    tree.visit(access, &mut |_, v| match visit(v) {
        Ok(()) => true,
        Err(e) => {
            error = Some(e);
            false
        }
    })?;
    error.map_or(Ok(()), Err)
}
