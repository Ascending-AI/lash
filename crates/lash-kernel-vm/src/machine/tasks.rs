//! The canonical task table and its bounded execution-coordinate cache.
use super::frames::Frames;
use super::{Task, Wait};
use crate::compile::Executable;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{IncomingRecord, TaskRecord, TaskStateRecord};
use std::collections::BTreeMap;
use std::sync::Arc;

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
        }
    }
    pub(super) fn take_incoming(&mut self) -> Option<IncomingRecord> {
        if matches!(self.record.state, TaskStateRecord::Resuming(_)) {
            let TaskStateRecord::Resuming(incoming) =
                std::mem::replace(&mut self.record.state, TaskStateRecord::Ready)
            else {
                unreachable!()
            };
            Some(incoming)
        } else {
            None
        }
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
    pub(super) fn wake(&mut self, incoming: IncomingRecord) {
        self.record.state = TaskStateRecord::Resuming(incoming);
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

pub(super) struct Tasks {
    rows: Tree<TaskRecord>,
    cache: BTreeMap<u64, Task>,
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
            cache: BTreeMap::new(),
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
    pub(super) fn get_mut(&mut self, index: usize) -> Result<Option<&mut Task>, HeapFault> {
        let index = index as u64;
        if !self.cache.contains_key(&index) {
            self.make_room()?;
            let Some(record) = self.rows.get(&ordinal_key(index), &self.access)? else {
                return Ok(None);
            };
            let continues = matches!(
                record.state,
                TaskStateRecord::Waiting(_)
                    | TaskStateRecord::Joining(_)
                    | TaskStateRecord::JoiningMany(_)
                    | TaskStateRecord::Resuming(_)
            );
            let frames = Frames::open(
                record.frames.clone(),
                self.access.clone(),
                Arc::clone(&self.exe),
                continues,
            );
            self.cache.insert(index, Task { record, frames });
        }
        Ok(self.cache.get_mut(&index))
    }
    pub(super) fn push(&mut self, task: Task) -> Result<(), HeapFault> {
        self.make_room()?;
        let index = self.rows.len();
        self.rows
            .write(ordinal_key(index), task.record.clone(), &self.access)?;
        self.cache.insert(index, task);
        Ok(())
    }
    pub(super) fn after_refinement(&mut self, rows: Tree<TaskRecord>) -> Result<(), HeapFault> {
        for (index, task) in &mut self.cache {
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
    pub(super) fn roots(&mut self) -> Result<Tree<TaskRecord>, HeapFault> {
        for (index, task) in &mut self.cache {
            task.frames.flush()?;
            task.record.frames = task.frames.rows.clone();
            let key = ordinal_key(*index);
            if !self
                .rows
                .get(&key, &self.access)?
                .is_some_and(|old| old.same_version(&task.record))
            {
                self.rows.write(key, task.record.clone(), &self.access)?;
            }
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
    /// At a complete statement the canonical records are authoritative.
    /// Retire derived frames so the cache cannot grow with task history.
    pub(super) fn cached_frames(&self, id: TaskId) -> Vec<&super::Frame> {
        self.cache
            .get(&id.0)
            .map_or_else(Vec::new, |task| task.frames.cached())
    }
    pub(super) fn evict(&mut self) -> Result<(), HeapFault> {
        self.roots()?;
        Ok(())
    }
    fn make_room(&mut self) -> Result<(), HeapFault> {
        if self.cache.len() < 64 {
            return Ok(());
        }
        let Some(index) = self
            .cache
            .iter()
            .find_map(|(index, task)| (!task.frames.pinned()).then_some(*index))
        else {
            return Err(HeapFault::Malformed(
                "all derived task frames are pinned".into(),
            ));
        };
        let mut task = self
            .cache
            .remove(&index)
            .ok_or_else(|| HeapFault::Malformed("selected cached task is absent".into()))?;
        task.frames.flush()?;
        task.record.frames = task.frames.rows.clone();
        let key = ordinal_key(index);
        if !self
            .rows
            .get(&key, &self.access)?
            .is_some_and(|old| old.same_version(&task.record))
        {
            self.rows.write(key, task.record, &self.access)?;
        }
        Ok(())
    }
}
