//! The canonical task table and its bounded execution-coordinate cache.
use super::frames::{Frames, ParkKind};
use super::{Task, Wait};
use crate::compile::Executable;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Tree, Version};
use lash_kernel_state::paged::{IncomingRecord, TaskRecord, TaskStateRecord};
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
        access: Access<'static>,
        exe: Arc<Executable>,
    ) -> Self {
        Self {
            record: TaskRecord {
                identity,
                state: TaskStateRecord::Ready,
                result_retained: true,
                joiners: Tree::new(true),
                observed: false,
                passed: false,
                failed: false,
                occurrences: Tree::new(false),
                frames: Tree::new(true),
            },
            frames: Frames::new(access, exe),
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
            TaskStateRecord::Waiting(id) => Some(Wait::Request(lash_kernel_state::WaitId(id))),
            TaskStateRecord::Joining(id) => Some(Wait::Join(id)),
            TaskStateRecord::JoiningMany(id) => Some(Wait::JoinMany(id)),
            _ => None,
        }
    }
    pub(super) fn suspend(&mut self, wait: Wait) {
        self.record.state = match wait {
            Wait::Request(id) => TaskStateRecord::Waiting(id.0),
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

/// Equality bookkeeping must not retain a second frame tree, which would
/// keep its binding leaves alive after the real roots are replaced.
pub(super) struct Publication {
    identity: Option<lash_kernel_doc::heap::PageId>,
    state: Vec<u8>,
    result_retained: bool,
    observed: bool,
    passed: bool,
    failed: bool,
    frames: Version<lash_kernel_state::paged::FrameRecord>,
    joiners: Version<lash_kernel_state::paged::JoinerRecord>,
    occurrences: Version<u64>,
}
impl Publication {
    fn new(record: &TaskRecord, frames: &Tree<lash_kernel_state::paged::FrameRecord>) -> Self {
        Self {
            identity: record.identity.root.map(|p| p.id),
            state: lash_kernel_doc::graph_tokens::encode_body(&record.state),
            result_retained: record.result_retained,
            observed: record.observed,
            passed: record.passed,
            failed: record.failed,
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
            && self.result_retained == record.result_retained
            && self.observed == record.observed
            && self.passed == record.passed
            && self.failed == record.failed
            && self.frames.matches(frames)
            && self.joiners.matches(&record.joiners)
            && self.occurrences.matches(&record.occurrences)
            && self.state == lash_kernel_doc::graph_tokens::encode_body(&record.state)
    }
}

pub(super) struct Tasks {
    rows: Tree<TaskRecord>,
    cache: Vec<Option<(u64, Box<Task>)>>,
    protected: Option<TaskId>,
    /// A validated slot of the protected task. Colliding task reads may
    /// replace the other way, but cannot evict this statement's task.
    active: Option<(u64, usize)>,
    #[cfg(test)]
    cache_probes: usize,
    access: Access<'static>,
    exe: Arc<Executable>,
}
impl Tasks {
    pub(super) fn open(
        rows: Tree<TaskRecord>,
        access: Access<'static>,
        exe: Arc<Executable>,
    ) -> Self {
        Self {
            rows,
            protected: None,
            active: None,
            #[cfg(test)]
            cache_probes: 0,
            cache: (0..CACHED_TASKS).map(|_| None).collect(),
            access,
            exe,
        }
    }
    pub(super) fn new(access: Access<'static>, exe: Arc<Executable>) -> Self {
        Self::open(Tree::new(false), access, exe)
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
    /// The running statement may have consumed its incoming state before it
    /// finishes reading another task. Its frame must remain resident until
    /// the step completes: Ready alone cannot restore that in-flight resume.
    pub(super) fn protect(&mut self, task: Option<TaskId>) {
        self.invalidate_active();
        self.protected = task;
    }
    /// End a segment before a native can refine the heap, or before a
    /// task-cache or statement boundary changes the active location.
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
        let frames = Frames::open(
            std::mem::replace(&mut record.frames, Tree::new(true)),
            self.access.clone(),
            Arc::clone(&self.exe),
            ParkKind::of(&record.state),
        );
        self.cache[slot] = Some((
            index,
            Box::new(Task {
                published: Some(Publication::new(&record, &frames.rows)),
                record,
                frames,
            }),
        ));
        Ok(Some(slot))
    }
    pub(super) fn push(&mut self, mut task: Task) -> Result<(), HeapFault> {
        self.invalidate_active();
        let index = self.rows.len();
        let slot = self.replacement(index)?;
        self.evict(slot)?;
        self.rows
            .write(ordinal_key(index), task.record.clone(), &self.access)?;
        task.published = Some(Publication::new(&task.record, &task.frames.rows));
        self.cache[slot] = Some((index, Box::new(task)));
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
                && !record.result_retained
            {
                task.record.result_retained = false;
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
            &mut lash_kernel_state::paged::tree::SealWork::new(
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
        put(&mut self.rows, index, &mut task, &self.access, true)
    }
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
    let sync = |task: &mut Task| {
        if coordinates {
            task.frames.flush()
        } else {
            task.frames.flush_holdings()
        }
    };
    let same = |task: &Task| {
        task.published
            .as_ref()
            .is_some_and(|old| old.same_version(&task.record, &task.frames.rows))
    };
    let synced = !task.frames.has_pending_bindings();
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

#[cfg(test)]
mod laws {
    use super::*;
    use crate::laws::embedder::Embedder;
    use lash_kernel_state::paged::{SiteRef, TaskIdentityRef};

    /// FIG-6480: a resident task takes one indexed probe, even when the
    /// execution cache is full; the canonical task tree stays cold.
    #[test]
    fn a_warm_task_read_uses_one_cache_probe_at_capacity() {
        let mut embedder = Embedder::new("main { return null }");
        let machine = &mut embedder.machine;
        let access = machine.heap.pages().clone();
        let site = SiteRef::new(&machine.exe.code(machine.exe.main).site, &access).unwrap();
        for index in 1..CACHED_TASKS {
            let identity =
                TaskIdentityRef::spawn(TaskIdentityRef::MAIN, site.clone(), index as u64, &access)
                    .unwrap();
            machine
                .tasks
                .push(Task::fresh(
                    identity,
                    access.clone(),
                    Arc::clone(&machine.exe),
                ))
                .unwrap();
        }
        let before = machine.tasks.cache_probes;
        for _ in 0..128 {
            assert!(machine.tasks.get_mut(0).unwrap().is_some());
        }
        assert_eq!(
            machine.tasks.cache_probes - before,
            128,
            "a resident task must take one cache probe per read"
        );
    }

    /// FIG-6588: local operand count does not multiply task or top-frame
    /// cache resolution within a non-yielding statement.
    #[test]
    fn local_operands_reuse_the_statements_validated_frame() {
        let probes = |operands: usize| {
            let values = vec!["value"; operands].join(", ");
            let mut embedder = Embedder::new(&format!(
                "fn f(value) {{ let out = ({values}) do perform echo(out) as Any }} main {{ do call f(7) }}"
            ));
            let task_probes = embedder.machine.tasks.cache_probes;
            let frame_probes = embedder.machine.tasks.cache[bucket(0)]
                .iter()
                .flatten()
                .find(|(id, _)| *id == 0)
                .unwrap()
                .1
                .frames
                .cache_probes;
            assert!(matches!(embedder.run(), crate::Step::Parked(_)));
            let machine = &embedder.machine;
            let task = &machine.tasks.cache[bucket(0)]
                .iter()
                .flatten()
                .find(|(id, _)| *id == 0)
                .unwrap()
                .1;
            (
                machine.tasks.cache_probes - task_probes,
                task.frames.cache_probes - frame_probes,
                machine.boundaries as usize,
            )
        };
        let few = probes(2);
        let many = probes(128);
        assert_eq!(many, few, "local operands must reuse validated coordinates");
        // Each statement validates once. Publishing the park after the
        // protected step ends may resolve the location once more.
        let segments = many.2 + 1;
        assert!(many.0 <= segments, "task resolutions: {many:?}");
        assert!(many.1 <= segments, "frame resolutions: {many:?}");
    }
}
