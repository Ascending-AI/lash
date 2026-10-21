//! An effect's origin (`K-EFF-008`, ORIGIN): the nearest authored
//! statement it was performed under.
//!
//! For a `perform` in document code that is its own statement; for one in a
//! library body, the statement of the document frame that entered the
//! library, through any library calls between; for a recursive declared
//! function, that activation's statement. A caller stands at its call
//! statement while the callee runs (`K-EVAL-008`), and each frame row saves
//! where it stands, so the stack holds the origin and nothing derived from
//! it is saved. The one fact the stack does not hold is where a task that
//! runs no document code was spawned from: its spawner's origin at the
//! spawn, written once into the task's row (`TaskRecord::spawn_origin`).
//!
//! An origin is reported with a request and handed to the host with a
//! read-bound `perform`. It is never part of an effect's identity.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::SiteRef;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::KernelMachine;
use super::actions::Target;
use crate::compile::Executable;

impl KernelMachine {
    /// The origin of what task `task` performs where it stands, asked from
    /// inside a statement: its topmost document frame's statement, else
    /// the origin it was spawned under, else where its root frame stands.
    pub(super) fn origin(&mut self, task: TaskId) -> Result<Site, HeapFault> {
        let access = self.heap.pages().clone();
        let task = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| HeapFault::Malformed("an origin's task is missing".into()))?;
        if let Some(statement) = task.frames.authored()? {
            return Ok(statement);
        }
        if let Some(origin) = &task.record.spawn_origin {
            return Ok(origin.resolve(&access)?.as_ref().clone());
        }
        task.frames
            .root_statement()?
            .ok_or_else(|| HeapFault::Malformed("an origin's task has no frame".into()))
    }

    /// The origin a task spawned at `site` from task `parent` keeps, to
    /// run `target`: the parent's, when the spawn stands in a library body
    /// or the task's root runs library code, so that no frame of the task
    /// says where it came from; none otherwise.
    pub(super) fn spawn_origin(
        &mut self,
        parent: TaskId,
        exe: &Executable,
        site: &Site,
        target: &Target,
    ) -> Result<Option<SiteRef>, HeapFault> {
        let library = |unit: &Unit| !exe.authored_unit(unit);
        let rooted_in_library = match target {
            Target::Library(..) | Target::Export(..) => true,
            Target::Code(code, _, _) => library(&exe.code(*code).site.unit),
        };
        if !library(&site.unit) && !rooted_in_library {
            return Ok(None);
        }
        let origin = self.origin(parent)?;
        Ok(Some(SiteRef::new(&origin, self.heap.pages())?))
    }
}
