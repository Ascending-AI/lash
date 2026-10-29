//! The canonical task table and its bounded execution-coordinate cache.
use super::frames::{self, Frames};
use super::project::SharedArena;
use super::{EndedTask, Ending, Halt, Incoming, Joiner, Task, TaskState, Wait, fault, project};
use crate::compile::Executable;
use crate::data::CopyError;
use crate::observe::Event;
use crate::{End, RunError};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_state::arena::Reached;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::resident::{frame_values, state_values};
use lash_kernel_state::kernel_internals::tree::{Access, Tree, Version};
use lash_kernel_state::paged::{
    IncomingRecord, OccurrenceOwner, OccurrenceRecord, TaskRecord, TaskStateRecord,
};
use std::collections::BTreeMap;
use std::sync::Arc;

const CACHED_TASKS: usize = 64;
const WAYS: usize = 2;

fn bucket(index: u64) -> std::ops::Range<usize> {
    let first = index as usize % (CACHED_TASKS / WAYS) * WAYS;
    first..first + WAYS
}

impl Task {
    pub(super) fn fresh(
        identity: lash_kernel_state::paged::TaskIdentityRef,
        spawn_origin: Option<lash_kernel_state::paged::SiteRef>,
        access: Access<'static>,
        arena: SharedArena,
        exe: Arc<Executable>,
    ) -> Self {
        Self {
            counters: Occurrences::new(&exe),
            record: TaskRecord {
                identity,
                state: TaskStateRecord::Ready,
                joiners: Tree::new(true),
                observed: false,
                passed: false,
                occurrences: Tree::new(false),
                frames: Tree::new(true),
                spawn_origin,
            },
            frames: Frames::new(access, arena, exe),
            published: None,
        }
    }
    pub(super) fn take_incoming(&mut self) -> Option<IncomingRecord> {
        if matches!(self.record.state, TaskStateRecord::Resuming { .. }) {
            let TaskStateRecord::Resuming { incoming, .. } =
                std::mem::replace(&mut self.record.state, TaskStateRecord::Ready)
            else {
                unreachable!()
            };
            Some(incoming)
        } else {
            None
        }
    }
    /// Takes the turn a delivered task owes before it continues
    /// (`K-TASK-005`): whether it owed one.
    pub(super) fn take_requeue(&mut self) -> bool {
        match &mut self.record.state {
            TaskStateRecord::Resuming { requeue, .. } if *requeue => {
                *requeue = false;
                true
            }
            _ => false,
        }
    }
    /// Whether the task is delivered and owes its turn.
    pub(super) fn owes_turn(&self) -> bool {
        matches!(
            self.record.state,
            TaskStateRecord::Resuming { requeue: true, .. }
        )
    }
    pub(super) fn wait(&self) -> Option<Wait> {
        match self.record.state {
            TaskStateRecord::Waiting(id) => Some(Wait::Request(
                lash_kernel_doc::kernel_internals::wait_id(id),
            )),
            TaskStateRecord::Joining(id) => Some(Wait::Join(id)),
            TaskStateRecord::JoiningMany(id) => Some(Wait::JoinMany(id)),
            _ => None,
        }
    }
    pub(super) fn suspend(&mut self, wait: Wait) {
        self.record.state = match wait {
            Wait::Request(id) => {
                TaskStateRecord::Waiting(lash_kernel_doc::kernel_internals::wait_number(id))
            }
            Wait::Join(id) => TaskStateRecord::Joining(id),
            Wait::JoinMany(id) => TaskStateRecord::JoiningMany(id),
        };
    }
    /// Makes the task ready to continue with `incoming`, first taking one
    /// more turn of the queue when `requeue` (`K-TASK-005`).
    pub(super) fn wake(&mut self, incoming: IncomingRecord, requeue: bool) {
        self.record.state = TaskStateRecord::Resuming { incoming, requeue };
    }
}
impl std::ops::Deref for Task {
    type Target = TaskRecord;
    fn deref(&self) -> &TaskRecord {
        &self.record
    }
}
impl std::ops::DerefMut for Task {
    fn deref_mut(&mut self) -> &mut TaskRecord {
        &mut self.record
    }
}

/// Working state is task-wide, never frame-local or a persistent tree.
pub(crate) struct Occurrences {
    counts: Box<[u64]>,
    dirty: Vec<usize>,
    marked: Box<[bool]>,
}
impl crate::admission::Counters for Occurrences {
    fn can_advance(&self, occurrences: &[usize]) -> bool {
        self.can_advance(occurrences)
    }
}
impl Occurrences {
    fn new(exe: &Executable) -> Self {
        Self {
            counts: vec![0; exe.occurrence_count()].into_boxed_slice(),
            dirty: Vec::new(),
            marked: vec![false; exe.occurrence_count()].into_boxed_slice(),
        }
    }

    fn open(
        rows: &mut Tree<OccurrenceRecord>,
        exe: &Executable,
        access: &Access<'static>,
    ) -> Result<Self, HeapFault> {
        let mut counters = Self::new(exe);
        let mut problem = None;
        let mut retired = Vec::new();
        rows.visit(access, &mut |key, row| {
            if let OccurrenceOwner::Helper(function) = row.owner
                && let lash_kernel_doc::Selection::Restored(selection) = exe.selection()
                && !selection.pins.contains(&function)
                && !exe.helpers().contains(&function)
            {
                retired.push(key.to_vec());
                return true;
            }
            if let Some(index) = exe.saved_occurrence_index(key)
                && exe.occurrence_owner(index) == row.owner
            {
                counters.counts[index] = row.count;
                return true;
            }
            problem = Some(HeapFault::Malformed(
                "unknown occurrence site or owner".into(),
            ));
            false
        })?;
        if problem.is_none() {
            for key in retired {
                rows.remove(&key, access)?;
            }
        }
        problem.map_or(Ok(counters), Err)
    }

    #[inline]
    pub(crate) fn advance(&mut self, index: usize) -> Result<u64, super::Halt> {
        // A site counted since the step's last projection, short of the
        // counter's end: its count steps in place.
        let Some(&count) = self.counts.get(index) else {
            return Err(fault("unknown occurrence site"));
        };
        if count < u64::MAX && self.marked[index] {
            self.counts[index] = count + 1;
            return Ok(count);
        }
        self.advance_marking(index)
    }

    #[inline(never)]
    fn advance_marking(&mut self, index: usize) -> Result<u64, super::Halt> {
        let count = self.counts[index];
        self.counts[index] = count
            .checked_add(1)
            .ok_or_else(|| super::fault("occurrence counter overflow"))?;
        if !self.marked[index] {
            self.dirty.push(index);
            self.marked[index] = true;
        }
        Ok(count)
    }

    /// A region commits each syntactic call site at most once per
    /// statement. Leave a counter overflow to ordinary action dispatch.
    pub(crate) fn can_advance(&self, sites: &[usize]) -> bool {
        sites.iter().all(|&index| {
            self.counts
                .get(index)
                .is_some_and(|count| *count < u64::MAX)
        })
    }

    pub(super) fn issued(&self, index: usize) -> Option<u64> {
        self.counts
            .get(index)
            .and_then(|count| count.checked_sub(1))
    }

    fn project(
        &mut self,
        rows: &mut Tree<OccurrenceRecord>,
        exe: &Executable,
        access: &Access<'static>,
    ) -> Result<(), HeapFault> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let mut projected = rows.clone();
        self.dirty.sort_unstable();
        for &index in &self.dirty {
            projected.write(
                exe.occurrence_key(index).to_vec(),
                OccurrenceRecord {
                    owner: exe.occurrence_owner(index),
                    count: self.counts[index],
                },
                access,
            )?;
        }
        *rows = projected;
        for index in self.dirty.drain(..) {
            self.marked[index] = false;
        }
        Ok(())
    }
}

/// Equality bookkeeping must not retain a second frame tree, which would
/// keep its binding leaves alive after the real roots are replaced.
pub(super) struct Publication {
    identity: Option<lash_kernel_doc::heap::PageId>,
    state: Vec<u8>,
    observed: bool,
    passed: bool,
    frames: Version<lash_kernel_state::paged::FrameRecord>,
    joiners: Version<lash_kernel_state::paged::JoinerRecord>,
    occurrences: Version<OccurrenceRecord>,
}
impl Publication {
    fn new(record: &TaskRecord, frames: &Tree<lash_kernel_state::paged::FrameRecord>) -> Self {
        Self {
            identity: record.identity.root.map(|p| p.id),
            state: lash_kernel_value::tokens::encode_body(&record.state),
            observed: record.observed,
            passed: record.passed,
            frames: frames.version(),
            joiners: record.joiners.version(),
            occurrences: record.occurrences.version(),
        }
    }
    fn same_version(
        &self,
        record: &TaskRecord,
        frames: &Tree<lash_kernel_state::paged::FrameRecord>,
    ) -> bool {
        self.identity == record.identity.root.map(|p| p.id)
            && self.observed == record.observed
            && self.passed == record.passed
            && self.frames.matches(frames)
            && self.joiners.matches(&record.joiners)
            && self.occurrences.matches(&record.occurrences)
            && self.state == lash_kernel_value::tokens::encode_body(&record.state)
    }
}

pub(super) struct Tasks {
    logical: lash_kernel_state::kernel_internals::logical::Lifecycle,
    rows: Tree<TaskRecord>,
    cache: Vec<Option<(u64, Box<Task>)>>,
    /// Dirty counters outlive eviction of a task's execution coordinates.
    unprojected: BTreeMap<u64, Occurrences>,
    protected: Option<TaskId>,
    /// A validated slot of the protected task. Colliding task reads may
    /// replace the other way, but cannot evict this statement's task.
    active: Option<(u64, usize)>,
    #[cfg(test)]
    cache_probes: usize,
    access: Access<'static>,
    arena: SharedArena,
    exe: Arc<Executable>,
}
impl Tasks {
    pub(super) fn open(
        logical: lash_kernel_state::kernel_internals::logical::Lifecycle,
        rows: Tree<TaskRecord>,
        access: Access<'static>,
        arena: SharedArena,
        exe: Arc<Executable>,
    ) -> Self {
        Self {
            logical,
            rows,
            unprojected: BTreeMap::new(),
            protected: None,
            active: None,
            #[cfg(test)]
            cache_probes: 0,
            cache: (0..CACHED_TASKS).map(|_| None).collect(),
            access,
            arena,
            exe,
        }
    }
    pub(super) fn new(
        logical: lash_kernel_state::kernel_internals::logical::Lifecycle,
        access: Access<'static>,
        arena: SharedArena,
        exe: Arc<Executable>,
    ) -> Self {
        Self::open(logical, Tree::new(false), access, arena, exe)
    }
    /// Visits every arena value the cached tasks' frames hold.
    /// What the canonical walk starts from in the tasks the next
    /// projection writes, by task number (seam C). A task's incoming
    /// outcome or ended result was projected when it came.
    pub(super) fn walk_roots(
        &self,
        coordinates: bool,
        out: &mut Vec<lash_kernel_doc::arena::ArenaHandle>,
    ) {
        let mut cached: Vec<_> = self.cache.iter().flatten().collect();
        cached.sort_unstable_by_key(|(index, _)| *index);
        for (_, task) in cached {
            task.frames.walk_roots(coordinates, out);
        }
    }
    pub(super) fn each_value(&self, visit: &mut dyn FnMut(&lash_kernel_doc::Value)) {
        for (_, task) in self.cache.iter().flatten() {
            task.frames.each_value(visit);
        }
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len() as usize
    }
    #[inline(always)]
    pub(super) fn get_mut(&mut self, index: usize) -> Result<Option<&mut Task>, HeapFault> {
        let index = index as u64;
        if let Some((id, slot)) = self.active
            && id == index
        {
            return Ok(self.cache[slot].as_mut().map(|(_, task)| task.as_mut()));
        }
        // Another task starts a new segment, even when it shares a bucket
        // with the protected task. Resolve both coordinates again on return.
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.task_resolutions += 1);
        self.invalidate_active();
        #[cfg(test)]
        {
            self.cache_probes += 1;
        }
        let range = bucket(index);
        let slot = match self.cache[range.clone()]
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|(id, _)| *id == index))
        {
            Some(way) => range.start + way,
            None => match self.load(index)? {
                Some(slot) => slot,
                None => return Ok(None),
            },
        };
        if self.protected == Some(TaskId(index)) {
            self.active = Some((index, slot));
            if let Some((_, task)) = &mut self.cache[slot] {
                task.frames.invalidate_active();
            }
        }
        Ok(self.cache[slot].as_mut().map(|(_, task)| task.as_mut()))
    }
    /// Whether `task` is the task at the slot the cursor holds: set at
    /// the running task's first lookup in a segment, and dropped where the
    /// slot can move (a load, a push, another task's lookup), at a
    /// refinement's reconciliation and at another task's step. A
    /// refinement moves no task or frame, so the cursor's lookups do not
    /// check for one.
    #[inline(always)]
    pub(super) fn cursor(&self, task: TaskId) -> bool {
        matches!(self.active, Some((id, _)) if id == task.0)
    }
    /// Whether the cursor names `task` and its top frame.
    #[inline(always)]
    pub(super) fn frame_cursor(&self, task: TaskId) -> bool {
        match self.active {
            Some((id, slot)) if id == task.0 => self.cache[slot]
                .as_ref()
                .is_some_and(|(_, task)| task.frames.cursor()),
            _ => false,
        }
    }
    #[inline(always)]
    pub(super) fn cursor_task(&mut self) -> Option<&mut Task> {
        let (_, slot) = self.active?;
        self.cache[slot].as_mut().map(|(_, task)| task.as_mut())
    }
    #[inline(always)]
    pub(super) fn cursor_frame(&mut self) -> Option<&mut super::Frame> {
        self.cursor_task()?.frames.cursor_frame()
    }
    /// Whether the cursor of `task` is where a fresh lookup finds it: the
    /// protected task's slot in its bucket, and its top frame when the
    /// cursor holds one.
    pub(super) fn cursor_is_fresh(&self, task: TaskId) -> bool {
        let Some((id, slot)) = self.active else {
            return false;
        };
        let range = bucket(id);
        let found = self.cache[range.clone()]
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|(at, _)| *at == id))
            .map(|way| range.start + way);
        id == task.0
            && self.protected.is_none_or(|protected| protected == task)
            && found == Some(slot)
            && self.cache[slot]
                .as_ref()
                .is_some_and(|(_, task)| task.frames.cursor_is_fresh())
    }
    /// The running statement may have consumed its incoming state before it
    /// finishes reading another task. Its frame must remain resident until
    /// the step completes: Ready alone cannot restore that in-flight resume.
    /// The cursor outlives the step: only a step of another task starts a
    /// new segment, since whatever moves a slot between steps drops it.
    pub(super) fn protect(&mut self, task: Option<TaskId>) {
        if task.is_some_and(|task| !self.cursor(task)) {
            self.invalidate_active();
        }
        self.protected = task;
    }
    /// End a segment before a task-cache change or another task's step
    /// moves the active location, or at a refinement's reconciliation.
    pub(super) fn invalidate_active(&mut self) {
        if let Some((_, slot)) = self.active.take()
            && let Some((_, task)) = &mut self.cache[slot]
        {
            task.frames.invalidate_active();
        }
    }
    fn replacement(&self, index: u64) -> Result<usize, HeapFault> {
        let range = bucket(index);
        let way = self.cache[range.clone()]
            .iter()
            .position(Option::is_none)
            .or_else(|| {
                self.cache[range.clone()].iter().position(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|(id, _)| Some(TaskId(*id)) != self.protected)
                })
            })
            .ok_or_else(|| HeapFault::Malformed("a task bucket has no unprotected slot".into()))?;
        Ok(range.start + way)
    }
    /// Cold task reads and collisions publish the replaced task before its
    /// slot is reused. Two ways leave room beside the running task.
    #[cold]
    fn load(&mut self, index: u64) -> Result<Option<usize>, HeapFault> {
        let Some(mut record) = self.rows.get(&ordinal_key(index), &self.access)? else {
            return Ok(None);
        };
        let slot = self.replacement(index)?;
        self.evict(slot)?;
        let publication = Publication::new(&record, &record.frames);
        let counters = match self.unprojected.remove(&index) {
            Some(counters) => counters,
            None => Occurrences::open(&mut record.occurrences, &self.exe, &self.access)?,
        };
        let mut frames = Frames::open(
            std::mem::replace(&mut record.frames, Tree::new(true)),
            self.access.clone(),
            std::rc::Rc::clone(&self.arena),
            Arc::clone(&self.exe),
            frames::arrival(&record.state),
        );
        frames.logical_context(TaskId(index), self.logical.clone());
        self.cache[slot] = Some((
            index,
            Box::new(Task {
                published: Some(publication),
                record,
                counters,
                frames,
            }),
        ));
        Ok(Some(slot))
    }
    pub(super) fn push(&mut self, mut task: Task) -> Result<(), HeapFault> {
        self.invalidate_active();
        let index = self.rows.len();
        task.frames
            .logical_context(TaskId(index), self.logical.clone());
        let slot = self.replacement(index)?;
        self.evict(slot)?;
        self.rows
            .write(ordinal_key(index), task.record.clone(), &self.access)?;
        task.published = Some(Publication::new(&task.record, &task.frames.rows));
        self.cache[slot] = Some((index, Box::new(task)));
        Ok(())
    }
    /// Releases a successful cell's execution values while preserving its
    /// task rows and occurrence counters for the final observation.
    pub(super) fn end_execution(&mut self) -> Result<(), HeapFault> {
        for index in 0..self.len() {
            let task = self
                .get_mut(index)?
                .ok_or_else(|| HeapFault::Malformed("a task row is missing".into()))?;
            task.frames.clear();
            match &mut task.record.state {
                TaskStateRecord::Ended(ended) => ended.retire(),
                state => *state = TaskStateRecord::Ended(EndedTask::Returned(None)),
            }
        }
        Ok(())
    }
    pub(super) fn after_refinement(&mut self, rows: Tree<TaskRecord>) -> Result<(), HeapFault> {
        self.invalidate_active();
        for (index, task) in self.cache.iter_mut().flatten() {
            task.frames.invalidate_active();
            // The refined rows replace those last written.
            task.published = None;
            if matches!(task.record.state, TaskStateRecord::Ended(_))
                && let Some(record) = rows.get(&ordinal_key(*index), &self.access)?
                && matches!(&record.state, TaskStateRecord::Ended(ended) if ended.retained().is_none())
            {
                task.record.state = record.state;
            }
        }
        self.rows = rows;
        Ok(())
    }
    /// The task rows with what every cached task's frames hold, where
    /// they stand left as last published: what pressure accounting and a
    /// trace read (FIG-6318).
    pub(super) fn holdings(&mut self) -> Result<Tree<TaskRecord>, HeapFault> {
        let mut cached: Vec<_> = self.cache.iter_mut().flatten().collect();
        cached.sort_unstable_by_key(|(index, _)| *index);
        for (index, task) in cached {
            put(&mut self.rows, *index, task, &self.access, false)?;
        }
        Ok(self.rows.clone())
    }
    pub(super) fn roots(&mut self) -> Result<Tree<TaskRecord>, HeapFault> {
        let mut cached: Vec<_> = self.cache.iter_mut().flatten().collect();
        cached.sort_unstable_by_key(|(index, _)| *index);
        for (index, task) in cached {
            put(&mut self.rows, *index, task, &self.access, true)?;
        }
        Ok(self.rows.clone())
    }
    pub(super) fn sealed_root(
        &mut self,
    ) -> Result<(lash_kernel_state::paged::KeyedRoot, u64), HeapFault> {
        self.roots()?;
        let outstanding = self.rows.live_members(&self.access)?;
        self.rows.seal(
            &self.access,
            &mut lash_kernel_state::kernel_internals::tree::SealWork::new(
                lash_kernel_state::paged::SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                },
            ),
        )?;
        Ok((self.rows.keyed_root()?, outstanding))
    }
    pub(super) fn visit(
        &mut self,
        visitor: &mut dyn FnMut(TaskId, &TaskRecord) -> bool,
    ) -> Result<(), HeapFault> {
        self.roots()?;
        let mut malformed = None;
        self.rows.visit(&self.access, &mut |key, record| {
            let Ok(bytes) = key.try_into() else {
                malformed = Some(HeapFault::Malformed("task key is not an ordinal".into()));
                return false;
            };
            let index = u64::from_be_bytes(bytes);
            visitor(TaskId(index), record)
        })?;
        malformed.map_or(Ok(()), Err)
    }
    /// How many frames `id` has and its resident live frames, by index.
    /// A cold stack loads its frames as execution reaches them. `None` when
    /// the task is not cached.
    pub(super) fn cached_frames(&self, id: TaskId) -> Option<(usize, Vec<(usize, &super::Frame)>)> {
        self.cache[bucket(id.0)]
            .iter()
            .flatten()
            .find(|(index, _)| *index == id.0)
            .map(|(_, task)| (task.frames.len(), task.frames.cached()))
    }
    fn evict(&mut self, slot: usize) -> Result<(), HeapFault> {
        let Some((index, mut task)) = self.cache[slot].take() else {
            return Ok(());
        };
        task.frames.release_control();
        put(&mut self.rows, index, &mut task, &self.access, true)?;
        if !task.counters.dirty.is_empty() {
            self.unprojected.insert(index, task.counters);
        }
        Ok(())
    }

    /// Gives `out` what the task rows hold: an uncached task's row as it
    /// is, a cached task's state and frames as they stand (seam I,
    /// FIG-6748).
    pub(super) fn reach(
        &self,
        arena: &mut lash_kernel_state::arena::Arena,
        reach: &mut lash_kernel_state::arena::Reach,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let cached: Vec<(u64, &Task)> = self
            .cache
            .iter()
            .flatten()
            .map(|(index, task)| (*index, task.as_ref()))
            .collect();
        let mut problem = None;
        self.rows.visit(&self.access, &mut |key, record| {
            if cached
                .iter()
                .any(|(index, _)| ordinal_key(*index).as_slice() == key)
            {
                return true;
            }
            let visited = state_values(&record.state, &mut |value| out(Reached::Page(value)))
                .and_then(|()| {
                    let mut problem = None;
                    record.frames.visit(&self.access, &mut |_, frame| {
                        let visited = frame_values(frame, &self.access, &mut |value| {
                            out(Reached::Page(value))
                        });
                        problem = visited.err();
                        problem.is_none()
                    })?;
                    problem.map_or(Ok(()), Err)
                });
            problem = visited.err();
            problem.is_none()
        })?;
        if let Some(problem) = problem {
            return Err(problem);
        }
        for (_, task) in cached {
            task.frames.reach(arena, reach, out)?;
            state_values(&task.record.state, &mut |value| out(Reached::Page(value)))?;
        }
        Ok(())
    }
    pub(super) fn logical_control_bytes(
        &self,
        arena: &lash_kernel_state::arena::Arena,
    ) -> Result<u64, HeapFault> {
        use lash_kernel_state::kernel_internals::resident::incoming_bytes;
        let mut bytes = 0_u64;
        let mut problem = None;
        self.rows.visit(&self.access, &mut |key, row| {
            let local = (|| {
                let ordinal = u64::from_be_bytes(
                    key.try_into()
                        .map_err(|_| HeapFault::Malformed("task key is not an ordinal".into()))?,
                );
                if let Some(task) = self.cached_task(ordinal) {
                    task.frames
                        .logical_control_bytes(arena)?
                        .checked_add(incoming_bytes(&task.record.state))
                } else {
                    row.frames.cost().checked_add(incoming_bytes(&row.state))
                }
                .ok_or_else(|| HeapFault::Malformed("task holding cost overflow".into()))
            })();
            match local.and_then(|n| {
                bytes
                    .checked_add(n)
                    .ok_or_else(|| HeapFault::Malformed("tasks holding cost overflow".into()))
            }) {
                Ok(n) => {
                    bytes = n;
                    true
                }
                Err(e) => {
                    problem = Some(e);
                    false
                }
            }
        })?;
        if let Some(e) = problem {
            return Err(e);
        }
        // Fresh tasks have no row until a projection first publishes them.
        for (id, task) in self.cache.iter().flatten() {
            if self.rows.get(&ordinal_key(*id), &self.access)?.is_none() {
                bytes = bytes
                    .checked_add(task.frames.logical_control_bytes(arena)?)
                    .and_then(|n| n.checked_add(incoming_bytes(&task.record.state)))
                    .ok_or_else(|| {
                        HeapFault::Malformed("fresh task holding cost overflow".into())
                    })?;
            }
        }
        Ok(bytes)
    }

    /// The bytes the capacity meter counts for the cached tasks' resident
    /// frames (FIG-6791).
    pub(super) fn control_bytes(&self) -> u64 {
        self.cache
            .iter()
            .flatten()
            .map(|(_, task)| task.frames.control_bytes())
            .fold(0, u64::saturating_add)
    }
    /// The result `task` retains, if it ended and retains one.
    pub(super) fn ended_result(&self, task: TaskId) -> Result<Option<PageValue>, HeapFault> {
        let row;
        let record = match self.cached_task(task.0) {
            Some(task) => &task.record,
            None => match self.rows.get(&ordinal_key(task.0), &self.access)? {
                Some(found) => {
                    row = found;
                    &row
                }
                None => return Ok(None),
            },
        };
        Ok(match &record.state {
            TaskStateRecord::Ended(ended) => ended.retained().cloned(),
            _ => None,
        })
    }
    /// Up to `limit` tasks after `after`, ascending, that retain an ended
    /// result.
    pub(super) fn ended_after(
        &self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        let mut key = after.map(|task| ordinal_key(task.0));
        let mut tasks = Vec::new();
        while tasks.len() < limit {
            let Some((next, row)) = self.rows.after(key.as_deref(), &self.access)? else {
                break;
            };
            let index = u64::from_be_bytes(
                next.as_slice()
                    .try_into()
                    .map_err(|_| HeapFault::Malformed("task key is not an ordinal".into()))?,
            );
            let record = self.cached_task(index).map_or(&row, |task| &task.record);
            if matches!(&record.state, TaskStateRecord::Ended(ended) if ended.retained().is_some())
            {
                tasks.push(TaskId(index));
            }
            key = Some(next.into());
        }
        Ok(tasks)
    }
    /// Retires the results of `tasks`, which nothing reaches: each keeps
    /// how it ended.
    pub(super) fn retire(&mut self, tasks: &[TaskId]) -> Result<(), HeapFault> {
        for task in tasks {
            if let Some(cached) = self.cached_task_mut(task.0) {
                retire(&mut cached.record)?;
                continue;
            }
            let key = ordinal_key(task.0);
            if let Some(mut record) = self.rows.get(&key, &self.access)? {
                retire(&mut record)?;
                self.rows.write(key, record, &self.access)?;
            }
        }
        Ok(())
    }
    fn cached_task(&self, index: u64) -> Option<&Task> {
        self.cache[bucket(index)]
            .iter()
            .flatten()
            .find(|(id, _)| *id == index)
            .map(|(_, task)| task.as_ref())
    }
    fn cached_task_mut(&mut self, index: u64) -> Option<&mut Task> {
        self.cache[bucket(index)]
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == index)
            .map(|(_, task)| task.as_mut())
    }

    /// Called only at park, slice return, end and explicit seal. Frame and
    /// holdings publication during execution deliberately leaves counts alone.
    pub(super) fn project_occurrences(&mut self) -> Result<(), HeapFault> {
        let evicted: Vec<_> = self.unprojected.keys().copied().collect();
        for index in evicted {
            if let Some(counters) = self.unprojected.get_mut(&index) {
                self.rows
                    .update(ordinal_key(index), &self.access, |record| {
                        counters.project(&mut record.occurrences, &self.exe, &self.access)?;
                        Ok(true)
                    })?;
            }
            self.unprojected.remove(&index);
        }
        for (_, task) in self.cache.iter_mut().flatten() {
            task.counters
                .project(&mut task.record.occurrences, &self.exe, &self.access)?;
        }
        Ok(())
    }
}

/// Retires the result an ended task's `record` retains, as a sweep does.
fn retire(record: &mut TaskRecord) -> Result<(), HeapFault> {
    let TaskStateRecord::Ended(ended) = &mut record.state else {
        return Err(HeapFault::Malformed(
            "a refinement retired a running task's result".into(),
        ));
    };
    ended.retire();
    Ok(())
}

/// Writes `task`'s record to its row unless the row already holds that
/// version, as its last write left it (FIG-6318).
fn put(
    rows: &mut Tree<TaskRecord>,
    index: u64,
    task: &mut Task,
    access: &Access<'static>,
    coordinates: bool,
) -> Result<(), HeapFault> {
    let sync = |task: &mut Task| task.frames.project(coordinates);
    let same = |task: &Task| {
        task.published
            .as_ref()
            .is_some_and(|old| old.same_version(&task.record, &task.frames.rows))
    };
    // A frame that writes variables writes them into its row inside the
    // task row's update, so the row's frame tree is the only owner.
    let synced = !task.frames.has_edits(coordinates);
    if synced {
        sync(task)?;
        if same(task) {
            return Ok(());
        }
    }
    rows.update(ordinal_key(index), access, |stored| {
        let previous = std::mem::replace(&mut stored.frames, Tree::new(true)).retire();
        if !synced {
            sync(task)?;
        }
        let changed = !same(task);
        if changed {
            *stored = task.record.clone();
            stored.frames = task.frames.rows.clone();
            task.published = Some(Publication::new(&task.record, &task.frames.rows));
        } else {
            stored.frames = previous.restore()?;
        }
        Ok(changed)
    })?;
    Ok(())
}

impl super::KernelMachine {
    /// Marks a task ended, wakes what waits on it and, for `main`, ends
    /// the run (`K-TASK-008`, `K-TASK-018`).
    pub(super) fn end_task(
        &mut self,
        task: TaskId,
        outcome: Result<Value, Value>,
    ) -> Result<(), Halt> {
        use lash_kernel_state::kernel_internals::logical::{Owner, SourceValue};
        let owner = if task == TaskId::MAIN {
            Owner::Incoming(task)
        } else {
            Owner::Ended(task)
        };
        if task != TaskId::MAIN {
            self.logical.replace(Owner::Incoming(task), None);
        }
        self.logical.replace(
            owner,
            Some(SourceValue::Value(
                *outcome.as_ref().unwrap_or_else(|value| value),
            )),
        );
        self.logical_flush()?;
        self.identify_session()?;
        let pages = self.heap.pages().clone();
        let result = {
            let mut arena = self.arena.borrow_mut();
            match &outcome {
                Ok(value) => Ok(project::project(&mut arena, &pages, value)?),
                Err(value) => Err(project::project(&mut arena, &pages, value)?),
            }
        };
        self.observe(Event::TaskEnded(task));
        let ended = self.task(task)?;
        ended.frames.clear();
        ended.state = TaskState::Ended(match result.clone() {
            Ok(value) => EndedTask::Returned(Some(value)),
            Err(value) => EndedTask::Raised(Some(value)),
        });
        let joiners = std::mem::replace(&mut ended.joiners, Tree::new(true));
        let access = self.heap.pages().clone();
        self.live_tasks = self.live_tasks.saturating_sub(1);
        if self.current == Some(task) {
            self.current = None;
        }
        let site = self.raise_ended(task);
        if task == TaskId::MAIN {
            self.end = Some(match outcome {
                Ok(value) => self.conclude(None, Ending::Returned(value))?,
                Err(value) => End::Error(RunError::Uncaught {
                    error: match self.copy_datum(&value) {
                        Ok(value) => value,
                        Err(CopyError::Raised(error)) => error.into_datum(),
                        Err(CopyError::Heap(error)) => return Err(error.into()),
                    },
                    site,
                }),
            });
            return Ok(());
        }
        for at in 0..joiners.len() {
            let (_, joiner) = joiners
                .rank_get(at, &access)?
                .ok_or_else(|| fault("a joiner row is missing"))?;
            match joiner {
                Joiner::Single(joiner) => self.wake(joiner, Incoming::Joined(task))?,
                Joiner::List(order) => {
                    let Some(join) = self.joins.get(
                        &lash_kernel_state::kernel_internals::directory::ordinal_key(order),
                        &access,
                    )?
                    else {
                        continue;
                    };
                    let (mode, members, joiner) = (join.mode, join.members.clone(), join.joiner);
                    if let Some(decision) = self.decide(mode, &members, Some(task))? {
                        self.remove_list_join(order)?;
                        self.pass(&members)?;
                        let incoming = match decision {
                            Ok(value) => Incoming::Value(self.page_form(&value)?),
                            Err(value) => Incoming::Raise(self.page_form(&value)?),
                        };
                        self.wake(joiner, incoming)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Makes a waiting task ready, at the back of the queue (`K-TASK-005`).
    pub(super) fn wake(&mut self, task: TaskId, incoming: Incoming) -> Result<(), Halt> {
        self.logical_incoming(task, &incoming)?;
        let woken = self.task(task)?;
        woken.wake(incoming, false);
        self.ready.push_back(task)?;
        Ok(())
    }

    /// Examines the other tasks at the run's end and gives the end
    /// (`K-TASK-018`). `ender` is the task that ran `finish` or `fail`.
    pub(super) fn conclude(&mut self, ender: Option<TaskId>, ending: Ending) -> Result<End, Halt> {
        let mut unfinished = Vec::new();
        let mut unobserved = Vec::new();
        let access = self.heap.pages().clone();
        let mut origins = Vec::new();
        self.task_table()?.visit(&mut |id, task| {
            if id != TaskId::MAIN && ender != Some(id) && !task.passed {
                match &task.state {
                    TaskState::Ended(ended) if ended.is_raised() && !task.observed => {
                        unobserved.push(task.identity)
                    }
                    TaskState::Ended(_) => return true,
                    _ => unfinished.push(task.identity),
                }
                origins.push((
                    task.identity,
                    task.frames.clone(),
                    task.spawn_origin.clone(),
                ));
            }
            true
        })?;
        if !unfinished.is_empty() || !unobserved.is_empty() {
            let mut sources = Vec::new();
            for (id, frames, origin) in origins {
                let site = source_body(&frames, &self.exe, &access)?.or(origin
                    .map(|origin| origin.resolve(&access))
                    .transpose()?
                    .map(|site| (*site).clone()));
                if let Some(site) = site {
                    sources.push((
                        lash_kernel_value::kernel_internals::task_label(id.resolve(&access)?),
                        site.position(),
                    ));
                }
            }
            let unfinished = unfinished
                .into_iter()
                .map(|id| {
                    id.resolve(&access)
                        .map(lash_kernel_value::kernel_internals::task_label)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let unobserved = unobserved
                .into_iter()
                .map(|id| {
                    id.resolve(&access)
                        .map(lash_kernel_value::kernel_internals::task_label)
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            }));
        }
        Ok(match ending {
            Ending::Returned(value) => match self.copy_datum(&value) {
                Ok(result) => End::Finished(Box::new(self.finished(result, false)?)),
                Err(CopyError::Raised(raised)) => End::Error(RunError::Uncaught {
                    error: raised.into_datum(),
                    site: None,
                }),
                Err(CopyError::Heap(fault)) => return Err(fault.into()),
            },
            Ending::Finished(result) => End::Finished(Box::new(self.finished(result, true)?)),
            Ending::Failed(reason) => End::Failed(reason),
        })
    }
}

/// The outermost document body of a live task, through any library adapter.
/// Its canonical frames already retain these sites across cold opens; no
/// separate task provenance needs to be persisted in the parked machine.
pub(super) fn source_body(
    frames: &Tree<lash_kernel_state::paged::FrameRecord>,
    exe: &Executable,
    access: &Access<'_>,
) -> Result<Option<lash_kernel_value::sites::Site>, HeapFault> {
    for index in 0..frames.len() {
        let (_, frame) = frames
            .rank_get(index, access)?
            .ok_or_else(|| HeapFault::Malformed("a task frame is missing".into()))?;
        let statement = frame.statement.resolve(access)?;
        if !exe.authored_unit(&statement.unit) {
            continue;
        }
        let body = (0..=statement.path.len())
            .rev()
            .find_map(|length| {
                exe.code_at(
                    &lash_kernel_value::sites::Site::new(
                        statement.unit.clone(),
                        &statement.path[..length],
                    ),
                    &frame.capture_names,
                )
            })
            .ok_or_else(|| HeapFault::Malformed("a frame has no admitted code body".into()))?;
        return Ok(Some(exe.code(body).site.clone()));
    }
    Ok(None)
}

#[cfg(test)]
mod laws {
    use crate::laws::embedder::Embedder;
    use crate::{Machine, Step};
    use lash_kernel_doc::TaskId;

    /// K-EFF-008: counter exhaustion keeps its existing machine-fault
    /// classification, rather than becoming a malformed-heap refusal.
    #[test]
    fn occurrence_overflow_keeps_its_machine_fault() {
        let mut run = Embedder::new("main { do yield return null }");
        run.machine
            .tasks
            .get_mut(0)
            .unwrap()
            .unwrap()
            .counters
            .counts
            .fill(u64::MAX);
        let result = Machine::run(&mut run.machine, &mut run.world, u64::MAX);
        assert!(
            matches!(result,
                Err(crate::MachineError::Fault { ref problem })
                    if problem == "occurrence counter overflow"
            ),
            "{result:?}"
        );
    }

    /// K-MACH-007: executing an action and publishing holdings or frame
    /// coordinates does not project occurrences before a slice/park/end.
    #[test]
    fn occurrences_project_only_at_execution_boundaries() {
        let mut run = Embedder::new(
            "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody {  return 1  }\"\n main { let a = call work() return a }",
        );
        loop {
            assert!(matches!(
                run.machine.run_slice(&mut run.world, 1).unwrap(),
                Step::Slice
            ));
            let task = run
                .machine
                .tasks
                .get_mut(TaskId::MAIN.0 as usize)
                .unwrap()
                .unwrap();
            if task.frames.len() == 2 {
                break;
            }
        }
        run.machine.tasks.holdings().unwrap();
        run.machine.tasks.roots().unwrap();
        assert_eq!(
            run.machine
                .tasks
                .get_mut(0)
                .unwrap()
                .unwrap()
                .record
                .occurrences
                .len(),
            0
        );
        Machine::run(&mut run.machine, &mut run.world, 1).unwrap();
        assert_eq!(
            run.machine
                .tasks
                .get_mut(0)
                .unwrap()
                .unwrap()
                .record
                .occurrences
                .len(),
            1
        );
    }
    /// FIG-7169: historical helper spawn counters retire with dead helpers;
    /// unknown authored counters remain corruption, even on a library mismatch.
    #[test]
    fn a_dead_helper_spawn_occurrence_retires_but_an_authored_row_is_refused() {
        use crate::laws::embedder::{Setup, int, result};
        use lash_kernel_doc::{BuildHelpers, LibraryDependencies};
        use lash_kernel_state::paged::{OccurrenceOwner, OccurrenceRecord};
        use std::sync::Arc;
        fn helpers(
            registry: &mut lash_kernel_doc::FunctionRegistry,
            _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
        ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
            let child = registry
                .register(
                    lash_kernel_doc::parse_definition(
                        "function retire.child() -> Any\nkernel 1\ncharge 0\nbody { return 7 }",
                    )
                    .unwrap(),
                    None,
                )
                .unwrap();
            let spawn = registry.register(lash_kernel_doc::parse_definition(&format!(
            "function retire.spawn() -> Any\nkernel 1\ncharge 0\nuse retire.child = @{child}\nbody {{ let task = spawn invoke retire.child() let value = join task return value }}")).unwrap(), None).unwrap();
            vec![("retire.spawn", spawn)]
        }
        for authored in [false, true] {
            let mut embedder = Embedder::with(
                "main { let value = invoke retire.spawn() let tail = perform 0(7) as Any return tail }",
                Setup {
                    probes: Some(helpers),
                    ..Setup::default()
                },
            );
            let helper = embedder.library.ids["retire.spawn"];
            let writer = Arc::new(
                BuildHelpers::new(embedder.library.registry.as_ref().clone(), [helper]).unwrap(),
            );
            let mut document = embedder.document.as_ref().clone();
            document.functions.retain(|id, _| !writer.contains(id));
            document.library = LibraryDependencies([helper].into_iter().collect());
            let mut program = crate::laws::embedder::program(
                Arc::new(document),
                crate::PreparedLibrary::with_helpers(
                    Arc::clone(&embedder.library.registry),
                    writer,
                ),
            );
            embedder.machine = crate::KernelMachine::start(
                program.clone(),
                embedder.bounds,
                Setup::default().start,
                Box::new(embedder.source.clone()),
            )
            .unwrap();
            assert!(matches!(embedder.run(), Step::Parked(_)));
            let access = embedder.machine.heap.pages().clone();
            let task = embedder.machine.tasks.get_mut(0).unwrap().unwrap();
            let mut saved = None;
            task.record
                .occurrences
                .visit(&access, &mut |key, row| {
                    if row.owner == OccurrenceOwner::Helper(helper) {
                        saved = Some((key.to_vec(), row.count));
                    }
                    true
                })
                .unwrap();
            let (key, count) = saved.expect("the real helper spawn issued an occurrence");
            assert_eq!(count, 1);
            if authored {
                task.record
                    .occurrences
                    .write(
                        key.clone(),
                        OccurrenceRecord {
                            owner: OccurrenceOwner::Authored,
                            count,
                        },
                        &access,
                    )
                    .unwrap();
            }
            let checkpoint = embedder.machine.checkpoint().unwrap().checkpoint();
            let reader = BuildHelpers::empty();
            // Simulate a binary that no longer installs either helper body.
            let mut registry = lash_kernel_doc::FunctionRegistry::new();
            let mut pending: std::collections::BTreeMap<_, _> = embedder
                .library
                .registry
                .iter()
                .filter(|(id, _)| !program.helpers().contains(id))
                .collect();
            while !pending.is_empty() {
                let id = *pending
                    .iter()
                    .find(|(id, registered)| {
                        registered
                            .callees()
                            .iter()
                            .all(|callee| callee == **id || registry.get(callee).is_some())
                    })
                    .expect("ordinary fixture definitions remain closed")
                    .0;
                let registered = pending.remove(&id).unwrap();
                registry
                    .register(
                        registered.definition().as_ref().clone(),
                        registered.native().cloned(),
                    )
                    .unwrap();
            }
            let documents = program
                .instances
                .iter()
                .map(|(link, instance)| {
                    (
                        *link,
                        program.documents[&instance.document].as_ref().clone(),
                    )
                })
                .collect();
            let (source, selection) = checkpoint
                .check_helper_compatibility(
                    Box::new(embedder.source.clone()),
                    &documents,
                    &Default::default(),
                    &reader,
                )
                .unwrap();
            program.library = crate::PreparedLibrary::with_helpers(Arc::new(registry), reader);
            program.selection = selection;
            embedder.machine =
                crate::KernelMachine::open(program, embedder.bounds, &checkpoint, source).unwrap();
            let admitted = embedder.machine.validate_foreign_frames();
            if authored {
                assert!(admitted.is_err(), "unknown authored provenance must refuse");
            } else {
                admitted.unwrap();
                let access = embedder.machine.heap.pages().clone();
                assert!(
                    embedder
                        .machine
                        .tasks
                        .get_mut(0)
                        .unwrap()
                        .unwrap()
                        .record
                        .occurrences
                        .get(&key, &access)
                        .unwrap()
                        .is_none()
                );
                embedder.deliver("echo");
                assert_eq!(result(embedder.run_to_end(&[])), int(7));
            }
        }
    }
}
