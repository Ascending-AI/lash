//! Running frames: plain mutable memory over cold descriptors
//! (`docs/design/kernel-end-state.md`, seams A, E and G).
//!
//! A resident frame's slots, blocks, loops, cleanups, saved continuation
//! and retained arguments are plain values, written as it runs. A frame
//! below its task's top stays a cold descriptor, its saved [`FrameRecord`]
//! row, until a return reaches it. At a projection point the frames that
//! changed since the last projection, and the top, are projected into
//! their rows; nothing else is written.
//!
//! A frame restored from its row is admitted through the coordinate
//! grammar (`lash_kernel_exec::coord`, `K-EVAL-008`): the action nested in
//! its statement that it stands at, with that action's phase and roles,
//! its loops, its cleanups and its retained arguments, each against the
//! code's side table under this layout and against how the task comes to
//! the frame. Restore fails closed with the [`RestoreRefusal`] that names
//! what it refuses.
use super::loops::{Guard, Header, TextCursor};
use super::project::{SharedArena, admit, admit_cell, project, project_cell};
use super::{Completion, Control, Cursor, Frame, LibraryCall, SlotState, TryPhase};
use crate::compile::{Executable, Form, LibId, NestedKind, StmtId};
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_doc::{Site, Unit, Value};
use lash_kernel_exec::ActIx;
use lash_kernel_exec::coord::{
    self, ActionEntry, ActionKind, ActionPhase, Arrival, CleanupCoordinate, Departure,
    LoopCoordinate, LoopPhase, RestoreRefusal, SitePath, StatementCoordinate,
};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, CursorKind, CursorRecord, EnteredRecord, FinallyRecord,
    FrameRecord, GuardRecord, HeaderRecord, IncomingRecord, IteratedRecord, LoopRecord, NameRef,
    PendingRecord, PhaseRecord, RoleRecord, SiteRef, TableCursor, TaskStateRecord,
};
use std::sync::Arc;

fn malformed(problem: impl Into<String>) -> HeapFault {
    HeapFault::Malformed(problem.into())
}

/// How a task in `state` comes to its top frame.
pub(super) fn arrival(state: &TaskStateRecord) -> Arrival {
    match state {
        TaskStateRecord::Resuming { requeue: true, .. } => Arrival::Delivered,
        TaskStateRecord::Waiting(_)
        | TaskStateRecord::Resuming {
            incoming: IncomingRecord::Outcome(_),
            ..
        } => Arrival::Requested,
        TaskStateRecord::Resuming {
            incoming: IncomingRecord::Raise(_),
            ..
        } => Arrival::Raised,
        TaskStateRecord::Resuming {
            incoming: IncomingRecord::Value(value),
            ..
        } => Arrival::Valued {
            handle: matches!(value, PageValue::Task(_)),
        },
        TaskStateRecord::Joining(_)
        | TaskStateRecord::JoiningMany(_)
        | TaskStateRecord::Resuming {
            incoming: IncomingRecord::Joined(_),
            ..
        } => Arrival::Joined,
        _ => Arrival::Fresh,
    }
}

/// Why a saved frame does not restore: its coordinate is refused, or a
/// page it needs faulted.
enum Unrestored {
    Refused(RestoreRefusal),
    Fault(HeapFault),
}

impl From<HeapFault> for Unrestored {
    fn from(fault: HeapFault) -> Self {
        Self::Fault(fault)
    }
}

impl From<RestoreRefusal> for Unrestored {
    fn from(refusal: RestoreRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<Unrestored> for HeapFault {
    fn from(unrestored: Unrestored) -> Self {
        match unrestored {
            Unrestored::Refused(refusal) => malformed(format!("continuation refused: {refusal}")),
            Unrestored::Fault(fault) => fault,
        }
    }
}

fn row<T: Cell>(tree: &Tree<T>, index: u64, access: &Access<'_>) -> Result<T, HeapFault> {
    tree.rank_get(index, access)?
        .map(|(_, value)| value)
        .ok_or_else(|| malformed("frame control row is missing"))
}

/// Writes `value` at `index` unless the row holds that version: whether
/// it wrote.
fn write<T: Cell>(
    tree: &mut Tree<T>,
    index: u64,
    value: T,
    access: &Access<'_>,
) -> Result<bool, HeapFault> {
    let key = ordinal_key(index);
    if let Some(old) = tree.get(&key, access)?
        && old.same_version(&value)
    {
        return Ok(false);
    }
    tree.write(key, value, access)?;
    Ok(true)
}

/// Removes the rows from `len` on: whether there were any.
fn truncate<T: Cell>(tree: &mut Tree<T>, len: u64, access: &Access<'_>) -> Result<bool, HeapFault> {
    let longer = tree.len() > len;
    while tree.len() > len {
        let (key, _) = tree
            .rank_get(tree.len() - 1, access)?
            .ok_or_else(|| malformed("frame control tail is missing"))?;
        tree.remove(&key, access)?;
    }
    Ok(longer)
}

fn site_ref(old: Option<SiteRef>, site: &Site, access: &Access<'_>) -> Result<SiteRef, HeapFault> {
    if let Some(old) = old
        && old.matches(site, access)?
    {
        return Ok(old);
    }
    SiteRef::new(site, access)
}

/// A frame's variables, by slot. Each assignment marks its slot changed
/// until the frame is next projected.
#[derive(Debug, Default)]
pub(super) struct Slots {
    values: Vec<SlotState>,
    changed: Vec<bool>,
    edits: Vec<usize>,
}

impl Slots {
    pub(super) fn fresh(&mut self, len: usize) {
        self.clear();
        self.values.resize(len, SlotState::Empty);
        self.changed.resize(len, false);
    }

    pub(super) fn clear(&mut self) {
        self.values.clear();
        self.changed.clear();
        self.edits.clear();
    }
}

/// The continuation a frame saves where it stands at an action nested in
/// its statement (`K-EVAL-008`): the action by its site's path suffix
/// under the statement's, its phase, and the operands its statement had
/// evaluated as roles by site, in evaluation order.
#[derive(Clone, Debug)]
pub(super) struct Pending {
    pub(super) site: Vec<u32>,
    pub(super) phase: PhaseRecord,
    pub(super) roles: Vec<Role>,
}

/// An operand the statement evaluated before the action it stands at, by
/// its site: an arena value until a projection writes the frame, so a
/// continuation left before the next projection point gives no id.
#[derive(Clone, Debug)]
pub(super) struct Role {
    pub(super) site: Vec<u32>,
    pub(super) value: Value,
}

/// What a frame's row holds of its continuation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Saved {
    /// No continuation.
    #[default]
    Nothing,
    /// The frame's current continuation.
    This,
    /// A continuation the frame has since left.
    Another,
}

/// A frame's saved continuation, and what its row holds of it: a
/// continuation saved and left again between two projections leaves the
/// row as it was.
#[derive(Debug, Default)]
pub(super) struct Continuation {
    pending: Option<Pending>,
    saved: Saved,
}

impl Continuation {
    pub(super) fn set(&mut self, pending: Pending) {
        self.pending = Some(pending);
        if self.saved == Saved::This {
            self.saved = Saved::Another;
        }
    }

    /// The continuation the frame saved, if any.
    pub(super) fn peek(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    pub(super) fn take(&mut self) -> Option<Pending> {
        let pending = self.pending.take();
        if self.saved == Saved::This {
            self.saved = Saved::Another;
        }
        pending
    }

    /// Whether the row holds the continuation as it is.
    fn projected(&self) -> bool {
        match self.saved {
            Saved::Nothing => self.pending.is_none(),
            Saved::This => true,
            Saved::Another => false,
        }
    }

    /// Clears a recycled frame's continuation: the next frame's row is
    /// written afresh.
    pub(super) fn clear(&mut self) {
        self.pending = None;
        self.saved = Saved::Nothing;
    }
}

impl Frame {
    pub(super) fn slot(&self, at: u32) -> &SlotState {
        &self.slots.values[at as usize]
    }

    pub(super) fn set_slot(&mut self, at: u32, value: SlotState) {
        let at = at as usize;
        self.slots.values[at] = value;
        if !self.slots.changed[at] {
            self.slots.changed[at] = true;
            self.slots.edits.push(at);
        }
    }

    /// Whether a projection has variables of this frame to write.
    fn has_edits(&self) -> bool {
        !self.owned || !self.slots.edits.is_empty()
    }

    /// Visits what the frame holds outside its variables, in control
    /// order, until `visit` declines: what each `for` walks and the item a
    /// hook holds, each `finally`'s departing payload, and the departure a
    /// run-root cleanup holds.
    fn visit_held(&self, visit: &mut dyn FnMut(&Value) -> bool) {
        for control in &self.control {
            let more = match control {
                Control::For { cursor, header, .. } => cursor
                    .walked()
                    .into_iter()
                    .chain(header.held())
                    .all(|value| visit(&value)),
                Control::Try {
                    phase:
                        TryPhase::Finally(
                            Completion::Return(value)
                            | Completion::ReturnVia { value, .. }
                            | Completion::Throw(value),
                        ),
                    ..
                } => visit(value),
                _ => true,
            };
            if !more {
                return;
            }
        }
        if let Some(Ok(value) | Err(value)) = &self.cleaning {
            visit(value);
        }
    }

    /// Visits every arena value the frame holds: what an arena collection
    /// keeps. A shared variable's cell is visited as a ref.
    fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        for slot in &self.slots.values {
            match slot {
                SlotState::Value(value) => visit(value),
                SlotState::Cell(cell) => visit(&Value::ObjectRef(*cell)),
                SlotState::Empty => {}
            }
        }
        if let Some(pending) = &self.pending.pending {
            pending.roles.iter().for_each(|role| visit(&role.value));
        }
        self.visit_held(&mut |value| {
            visit(value);
            true
        });
        if let Some(call) = &self.library {
            call.args.iter().for_each(&mut *visit);
        }
    }

    /// Whether the frame's row holds all the frame holds: no variable is
    /// assigned and no continuation saved since its last projection, and
    /// it holds the values it held then.
    fn holds_as_projected(&self) -> bool {
        if !self.slots.edits.is_empty() || !self.pending.projected() {
            return false;
        }
        let mut at = 0;
        let mut same = true;
        self.visit_held(&mut |value| {
            same = self.held.get(at).is_some_and(|held| held.same(value));
            at += 1;
            same
        });
        same && at == self.held.len()
    }

    /// What the canonical walk starts from in this frame (seam C), in
    /// order: its bindings by name, its retained arguments by position,
    /// then what each loop walks and holds and each cleanup's departing
    /// payload, in control order, with the roles of the continuation it
    /// saved after its arguments.
    fn walk_roots(&self, exe: &Executable, out: &mut Vec<ArenaHandle>) {
        for &at in &exe.code(self.code).ats_by_name {
            match &self.slots.values[at as usize] {
                SlotState::Value(value) => out.extend(value.handle()),
                SlotState::Cell(cell) => out.push(*cell),
                SlotState::Empty => {}
            }
        }
        if let Some(call) = &self.library {
            out.extend(call.args.iter().filter_map(Value::handle));
        }
        if let Some(pending) = &self.pending.pending {
            out.extend(pending.roles.iter().filter_map(|role| role.value.handle()));
        }
        self.visit_held(&mut |value| {
            out.extend(value.handle());
            true
        });
    }

    /// Projects this frame into `stored`, its row as last written, or a
    /// fresh record when the row is not its own: its variables and held
    /// values always, and where it stands when `coordinates`. Whether the
    /// row changed.
    fn project(
        &mut self,
        stored: &mut FrameRecord,
        coordinates: bool,
        exe: &Executable,
        access: &Access<'_>,
        arena: &mut Arena,
    ) -> Result<bool, HeapFault> {
        let code = exe.code(self.code);
        let mut changed = !self.owned;
        // A fresh record takes every variable the frame binds; its own
        // row, only those assigned since.
        let mut edits = std::mem::take(&mut self.slots.edits);
        if !self.owned {
            edits.clear();
            edits.extend(0..self.slots.values.len());
        }
        for &at in &edits {
            self.slots.changed[at] = false;
            let info = &code.slots[code.slots_by_at[at] as usize];
            let value = match &self.slots.values[at] {
                SlotState::Empty => {
                    changed |= stored.bindings.remove(&info.key, access)?.is_some();
                    continue;
                }
                SlotState::Value(value) => BoundRecord::Value(project(arena, access, value)?),
                SlotState::Cell(cell) => BoundRecord::Shared(project_cell(arena, *cell)?),
            };
            let (name, declared) = match stored.bindings.get(&info.key, access)? {
                Some(old) => (old.name, old.declared),
                None => (
                    NameRef::new(&info.name, access)?,
                    SiteRef::new(&info.declared, access)?,
                ),
            };
            let binding = BindingRecord {
                name,
                declared,
                value,
            };
            stored.bindings.write(info.key.clone(), binding, access)?;
            changed = true;
        }
        // Keep the edit list's capacity for the next assignments.
        edits.clear();
        self.slots.edits = edits;
        if coordinates {
            changed |= self.project_statement(stored, exe, access)?;
        }
        // Expression evaluation may stand at a loop test or a departure.
        // Where the frame stands is projected at the next projection point
        // that projects coordinates; its held values at every one.
        let mut loops = 0;
        let mut iterated = 0;
        let mut finally = 0;
        let mut departing = 0;
        self.held.clear();
        for control in &self.control {
            match control {
                Control::For {
                    stmt,
                    cursor,
                    started,
                    header,
                    guard,
                } => {
                    let Form::For { site, .. } = &exe.stmt(*stmt).form else {
                        return Err(malformed("a for cursor stands at another form"));
                    };
                    if coordinates {
                        let saved = stored.loops.get(&ordinal_key(loops), access)?;
                        let record = LoopRecord {
                            site: site_ref(saved.map(|record| record.site), site, access)?,
                            started: *started,
                            header: header_record(header),
                            cursor: cursor_record(cursor, guard.as_deref()),
                        };
                        changed |= write(&mut stored.loops, loops, record, access)?;
                    }
                    // What it walks, then the item a hook holds.
                    for value in cursor.walked().into_iter().chain(header.held()) {
                        self.held.push(value);
                        let held = IteratedRecord(project(arena, access, &value)?);
                        changed |= write(&mut stored.iterated, iterated, held, access)?;
                        iterated += 1;
                    }
                    loops += 1;
                }
                Control::While { stmt, started } => {
                    let Form::While { site, .. } = &exe.stmt(*stmt).form else {
                        return Err(malformed("a while cursor stands at another form"));
                    };
                    if coordinates {
                        let saved = stored.loops.get(&ordinal_key(loops), access)?;
                        let record = LoopRecord {
                            site: site_ref(saved.map(|record| record.site), site, access)?,
                            started: *started,
                            header: HeaderRecord::Body,
                            cursor: None,
                        };
                        changed |= write(&mut stored.loops, loops, record, access)?;
                    }
                    loops += 1;
                }
                Control::Try {
                    stmt,
                    phase: TryPhase::Finally(completion),
                } => {
                    let Form::Try { site, .. } = &exe.stmt(*stmt).form else {
                        return Err(malformed("a cleanup stands at another form"));
                    };
                    let entered = match completion {
                        Completion::Normal => EnteredRecord::Normal,
                        Completion::Break => EnteredRecord::Break,
                        Completion::Continue => EnteredRecord::Continue,
                        Completion::Return(value)
                        | Completion::ReturnVia { value, .. }
                        | Completion::Throw(value) => {
                            let value = **value;
                            self.held.push(value);
                            let value = project(arena, access, &value)?;
                            changed |= write(&mut stored.departing, departing, value, access)?;
                            departing += 1;
                            if let Completion::ReturnVia { stmt, .. } = completion {
                                let Form::ReturnVia { site, .. } = &exe.stmt(*stmt).form else {
                                    return Err(malformed("a deferred return has no policy"));
                                };
                                EnteredRecord::ReturnVia(Box::new(SiteRef::new(site, access)?))
                            } else if matches!(completion, Completion::Return(_)) {
                                EnteredRecord::Return
                            } else {
                                EnteredRecord::Throw
                            }
                        }
                    };
                    if coordinates {
                        let saved = stored.finally.get(&ordinal_key(finally), access)?;
                        let site = site_ref(saved.map(|record| record.site), site, access)?;
                        let record = FinallyRecord { site, entered };
                        changed |= write(&mut stored.finally, finally, record, access)?;
                    }
                    finally += 1;
                }
                _ => {}
            }
        }
        // A run-root cleanup runs at the body, outside every source scope.
        if let Some(departed) = &self.cleaning {
            let (value, entered) = match departed {
                Ok(value) => (*value, EnteredRecord::Return),
                Err(value) => (*value, EnteredRecord::Throw),
            };
            self.held.push(value);
            let value = project(arena, access, &value)?;
            changed |= write(&mut stored.departing, departing, value, access)?;
            departing += 1;
            if coordinates {
                let saved = stored.finally.get(&ordinal_key(finally), access)?;
                let site = site_ref(saved.map(|record| record.site), &code.site, access)?;
                let record = FinallyRecord { site, entered };
                changed |= write(&mut stored.finally, finally, record, access)?;
            }
            finally += 1;
        }
        if coordinates {
            changed |= truncate(&mut stored.loops, loops, access)?;
            changed |= truncate(&mut stored.finally, finally, access)?;
        }
        changed |= truncate(&mut stored.iterated, iterated, access)?;
        changed |= truncate(&mut stored.departing, departing, access)?;
        match (&self.library, &stored.arguments) {
            // A call's arguments are the frame's for its whole life.
            (Some(_), Some(_)) | (None, None) => {}
            (Some(call), None) => {
                let mut args = Tree::new(true);
                for (at, value) in call.args.iter().enumerate() {
                    args.write(
                        ordinal_key(at as u64),
                        project(arena, access, value)?,
                        access,
                    )?;
                }
                stored.arguments = Some(args);
                changed = true;
            }
            (None, Some(_)) => {
                stored.arguments = None;
                changed = true;
            }
        }
        // A fresh record holds no continuation.
        if !self.owned {
            self.pending.saved = Saved::Nothing;
        }
        if !self.pending.projected() {
            stored.pending = match &self.pending.pending {
                Some(pending) => Some(PendingRecord::new(
                    pending.site.clone(),
                    pending.phase,
                    role_records(arena, access, &pending.roles)?,
                    access,
                )?),
                None => None,
            };
            self.pending.saved = if self.pending.pending.is_some() {
                Saved::This
            } else {
                Saved::Nothing
            };
            changed = true;
        }
        self.owned = true;
        Ok(changed)
    }

    /// Projects the statement the frame stands at: its block's next, or
    /// the one it waits in (a `while` waits in its condition above its
    /// loop, a `for` in a protocol hook of its header). With no block it
    /// stands where it was put: its body's entry, a return policy's site,
    /// or its body while a run-root cleanup runs.
    fn project_statement(
        &self,
        stored: &mut FrameRecord,
        exe: &Executable,
        access: &Access<'_>,
    ) -> Result<bool, HeapFault> {
        let block = match self.control.as_slice() {
            [.., Control::Block { block, next }] => Some((*block, *next)),
            [
                ..,
                Control::Block { block, next },
                Control::While { .. } | Control::For { .. },
            ] if self.awaiting.is_some() => Some((*block, *next)),
            [] => None,
            // Between a loop test or a departure and its block, the frame
            // stands where it was last projected.
            _ if self.owned => return Ok(false),
            _ => None,
        };
        let statement = match block {
            Some((block, next)) => {
                let index = if self.awaiting.is_some() {
                    next.checked_sub(1)
                        .ok_or_else(|| malformed("a frame awaits a statement it has not issued"))?
                } else {
                    next
                };
                let index =
                    u32::try_from(index).map_err(|_| malformed("statement ordinal overflows"))?;
                let statement = exe.block(block).site.child(index);
                if stored.statement.matches(&statement, access)? {
                    return Ok(false);
                }
                SiteRef::new(&statement, access)?
            }
            None if stored.statement.same_version(&self.statement) => return Ok(false),
            None => self.statement.clone(),
        };
        stored.statement = statement;
        Ok(true)
    }
}

/// A task's frames: the resident ones by depth, over the rows of the
/// cold ones and of those last projected.
pub(super) struct Frames {
    pub(super) rows: Tree<FrameRecord>,
    len: u64,
    // Resident frames are ordered by depth. Cold lower frames stay in rows
    // until a return reaches them; push and pop always use the resident top.
    cache: Vec<(u64, Box<Frame>)>,
    /// The resident top validated for this segment. Stack edits
    /// and the task cache's segment boundaries invalidate this index.
    active: Option<usize>,
    /// The rows below this index hold what their frames hold: only a frame
    /// from here up, the top among them, can have changed since.
    held: u64,
    /// The rows below this index also stand where their frames stand.
    placed: u64,
    access: Access<'static>,
    arena: SharedArena,
    exe: Arc<Executable>,
    arrival: Arrival,
}

#[cfg(test)]
thread_local! {
    /// The most frames one projection on this thread has visited.
    static MOST_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The most frames one projection on this thread has visited since the
/// last call: what the frame-projection visit law bounds.
#[cfg(test)]
pub(crate) fn take_most_visited() -> usize {
    MOST_VISITED.replace(0)
}

impl Frames {
    pub(super) fn new(access: Access<'static>, arena: SharedArena, exe: Arc<Executable>) -> Self {
        Self::open(Tree::new(true), access, arena, exe, Arrival::Fresh)
    }
    pub(super) fn open(
        rows: Tree<FrameRecord>,
        access: Access<'static>,
        arena: SharedArena,
        exe: Arc<Executable>,
        arrival: Arrival,
    ) -> Self {
        let len = rows.len();
        Self {
            len,
            rows,
            cache: Vec::new(),
            active: None,
            held: len,
            placed: len,
            access,
            arena,
            exe,
            arrival,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(super) fn invalidate_active(&mut self) {
        self.active = None;
    }
    /// Whether the resident top is validated for this segment.
    #[inline(always)]
    pub(super) fn cursor(&self) -> bool {
        self.active.is_some()
    }
    #[inline(always)]
    pub(super) fn cursor_frame(&mut self) -> Option<&mut Frame> {
        let at = self.active?;
        self.cache.get_mut(at).map(|(_, frame)| frame.as_mut())
    }
    /// Whether the validated top is the stack's top, as a fresh
    /// [`Self::last_mut`] finds it; true when none is validated.
    pub(super) fn cursor_is_fresh(&self) -> bool {
        self.active
            .is_none_or(|at| at + 1 == self.cache.len() && self.cache[at].0 + 1 == self.len)
    }
    #[inline(always)]
    pub(super) fn last_mut(&mut self) -> Result<Option<&mut Frame>, HeapFault> {
        if let Some(at) = self.active {
            return Ok(Some(self.cache[at].1.as_mut()));
        }
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.frame_resolutions += 1);
        if self.is_empty() {
            return Ok(None);
        }
        let index = self.len - 1;
        if self.cache.last().is_none_or(|(at, _)| *at != index) {
            let record = row(&self.rows, index, &self.access)?;
            let frame = restore(
                &record,
                self.arrival,
                &self.access,
                &self.exe,
                &mut self.arena.borrow_mut(),
            )?;
            self.cache.push((index, frame));
        }
        // The top is projected at every projection point.
        self.held = self.held.min(index);
        self.placed = self.placed.min(index);
        self.active = Some(self.cache.len() - 1);
        Ok(self.cache.last_mut().map(|(_, frame)| frame.as_mut()))
    }
    pub(super) fn push(&mut self, frame: Box<Frame>) {
        self.invalidate_active();
        // The live cache owns the new call; its row is written at the next
        // projection point, and never if it returns before one.
        self.cache.push((self.len, frame));
        // The push itself proves the new top resident; discard the old
        // location and seed the new segment without resolving it again.
        self.active = Some(self.cache.len() - 1);
        self.len += 1;
        self.arrival = Arrival::Fresh;
    }
    pub(super) fn pop(&mut self) -> Result<Option<Box<Frame>>, HeapFault> {
        if self.is_empty() {
            return Ok(None);
        }
        self.last_mut()?;
        self.invalidate_active();
        let frame = self.cache.pop().map(|(_, frame)| frame);
        self.len -= 1;
        let top = self.len.saturating_sub(1);
        self.held = self.held.min(top);
        self.placed = self.placed.min(top);
        self.arrival = Arrival::Returned;
        Ok(frame)
    }
    pub(super) fn clear(&mut self) {
        self.invalidate_active();
        self.rows = Tree::new(true);
        self.len = 0;
        self.cache.clear();
        self.held = 0;
        self.placed = 0;
        self.arrival = Arrival::Fresh;
    }
    /// Whether a projection has variables to write: a frame it projects
    /// has assigned some since, or has no row of its own.
    pub(super) fn has_edits(&self, coordinates: bool) -> bool {
        let from = if coordinates { self.placed } else { self.held };
        self.cache
            .iter()
            .any(|(index, frame)| *index >= from && frame.has_edits())
    }
    /// What the canonical walk starts from in the frames the next
    /// projection writes, from the bottom up (seam C).
    pub(super) fn walk_roots(&self, coordinates: bool, out: &mut Vec<ArenaHandle>) {
        let from = if coordinates { self.placed } else { self.held };
        for (index, frame) in &self.cache {
            if *index < from || (frame.owned && !coordinates && frame.holds_as_projected()) {
                continue;
            }
            frame.walk_roots(&self.exe, out);
        }
    }

    /// Projects the resident frames that may have changed since the last
    /// projection, and the top, into their rows: what they hold, and where
    /// they stand when `coordinates`. Cold frames' rows are as they stand.
    pub(super) fn project(&mut self, coordinates: bool) -> Result<(), HeapFault> {
        truncate(&mut self.rows, self.len, &self.access)?;
        let from = if coordinates { self.placed } else { self.held };
        let mut arena = self.arena.borrow_mut();
        #[cfg(test)]
        let mut visited = 0;
        for (index, frame) in &mut self.cache {
            if *index < from {
                continue;
            }
            #[cfg(test)]
            {
                visited += 1;
            }
            if frame.owned {
                // What a frame holds is projected where it changed: a
                // frame whose row holds all it holds is not written.
                if !coordinates && frame.holds_as_projected() {
                    continue;
                }
                self.rows
                    .update(ordinal_key(*index), &self.access, |stored| {
                        frame.project(stored, coordinates, &self.exe, &self.access, &mut arena)
                    })?;
            } else {
                let mut record = FrameRecord::fresh(frame.statement.clone());
                frame.project(
                    &mut record,
                    coordinates,
                    &self.exe,
                    &self.access,
                    &mut arena,
                )?;
                self.rows.write(ordinal_key(*index), record, &self.access)?;
            }
        }
        #[cfg(test)]
        MOST_VISITED.set(MOST_VISITED.get().max(visited));
        let top = self.len.saturating_sub(1);
        self.held = top;
        if coordinates {
            self.placed = top;
        }
        Ok(())
    }
    /// The operation the guest called, where the top of the stack runs
    /// its body: the outermost of the uncharged library bodies above the
    /// topmost charged frame. A bound passed there is the operation's, as
    /// when its native passes it (`K-LIB-006`).
    pub(super) fn operation(&self) -> Result<Option<LibId>, HeapFault> {
        let mut operation = None;
        for index in (0..self.len).rev() {
            let resident = self.cache.iter().find(|(at, _)| *at == index);
            let code = match resident {
                Some((_, frame)) => frame.code,
                None => {
                    let site = row(&self.rows, index, &self.access)?
                        .statement
                        .resolve(&self.access)?;
                    (0..site.path.len())
                        .rev()
                        .find_map(|length| {
                            self.exe
                                .code_at(&Site::new(site.unit.clone(), &site.path[..length]))
                        })
                        .ok_or_else(|| malformed("a frame has no admitted code body"))?
                }
            };
            let code = self.exe.code(code);
            match (&code.site.unit, code.charged || !code.site.path.is_empty()) {
                (Unit::Library(function), false) => operation = self.exe.lib_of(function),
                _ => break,
            }
        }
        Ok(operation)
    }
    /// Applies `edit` to the saved continuation of the frame at `index`:
    /// in the resident frame when the cache holds it, else in its row.
    /// `false` when no frame has that index.
    #[cfg(test)]
    pub(super) fn pending_mut(
        &mut self,
        index: u64,
        edit: &mut dyn FnMut(&mut Option<PendingRecord>, &Access<'_>),
    ) -> Result<bool, HeapFault> {
        let access = self.access.clone();
        if let Some((_, frame)) = self.cache.iter_mut().find(|(at, _)| *at == index) {
            let mut arena = self.arena.borrow_mut();
            let mut record = match frame.pending.take() {
                Some(pending) => Some(PendingRecord::new(
                    pending.site,
                    pending.phase,
                    role_records(&mut arena, &access, &pending.roles)?,
                    &access,
                )?),
                None => None,
            };
            edit(&mut record, &access);
            if let Some(record) = record {
                frame.pending.set(Pending {
                    roles: admit_roles(&mut arena, &access, record.roles(&access)?)?,
                    site: record.site,
                    phase: record.phase,
                });
            }
            frame.pending.saved = Saved::Another;
            self.held = self.held.min(index);
            self.placed = self.placed.min(index);
            return Ok(true);
        }
        let key = ordinal_key(index);
        let Some(mut record) = self.rows.get(&key, &access)? else {
            return Ok(false);
        };
        edit(&mut record.pending, &access);
        self.rows.write(key, record, &access)?;
        Ok(true)
    }

    /// Applies `edit` to the saved loop row `at` of the top frame, cold:
    /// a continuation law's tamper with the grammar. The loop's site as
    /// it stood.
    #[cfg(test)]
    pub(super) fn loop_mut(
        &mut self,
        at: u64,
        edit: impl FnOnce(&mut LoopRecord, &Access<'_>),
    ) -> Result<SitePath, HeapFault> {
        let index = self.len - 1;
        if self.cache.iter().any(|(resident, _)| *resident == index) {
            return Err(malformed("the top frame is resident"));
        }
        let key = ordinal_key(index);
        let mut record = row(&self.rows, index, &self.access)?;
        let mut saved = row(&record.loops, at, &self.access)?;
        let site = site_path(saved.site.resolve(&self.access)?.as_ref());
        edit(&mut saved, &self.access);
        record.loops.write(ordinal_key(at), saved, &self.access)?;
        self.rows.write(key, record, &self.access)?;
        Ok(site)
    }

    /// Restores the row of the frame at `index` afresh, as a cold open
    /// does when execution reaches it, the top by how the task comes to it
    /// and a lower frame by a return: the refusal its coordinate meets.
    #[cfg(test)]
    pub(super) fn refusal(&self, index: u64) -> Result<Option<RestoreRefusal>, HeapFault> {
        let record = row(&self.rows, index, &self.access)?;
        let arrival = if index + 1 == self.len {
            self.arrival
        } else {
            Arrival::Returned
        };
        // A scratch arena: the probe leaves the run's arena as it is.
        let mut scratch = Arena::new();
        match restore(&record, arrival, &self.access, &self.exe, &mut scratch) {
            Ok(_) => Ok(None),
            Err(Unrestored::Refused(refusal)) => Ok(Some(refusal)),
            Err(Unrestored::Fault(fault)) => Err(fault),
        }
    }

    /// Visits every arena value the resident frames hold: what an arena
    /// collection keeps. A cold frame's row holds page forms only.
    pub(super) fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        for (_, frame) in &self.cache {
            frame.each_value(visit);
        }
    }

    /// The frames the cache holds, by their index in the stack.
    pub(super) fn cached(&self) -> Vec<(usize, &Frame)> {
        self.cache
            .iter()
            .map(|(index, frame)| (*index as usize, frame.as_ref()))
            .collect()
    }
    /// The loops every frame stands in, from the bottom frame up and in
    /// each outermost first, with the iteration each has started: a
    /// resident frame's from its control, a cold one's from its row.
    pub(super) fn loop_coordinates(
        &self,
    ) -> Result<Vec<lash_kernel_doc::kernel_internals::LoopIteration>, HeapFault> {
        let mut loops = Vec::new();
        let mut resident = self.cache.iter().peekable();
        for index in 0..self.len {
            if let Some((_, frame)) = resident.next_if(|(at, _)| *at == index) {
                for control in &frame.control {
                    let (stmt, started) = match control {
                        Control::For { stmt, started, .. } | Control::While { stmt, started } => {
                            (*stmt, *started)
                        }
                        _ => continue,
                    };
                    let (Form::For { site, .. } | Form::While { site, .. }) =
                        &self.exe.stmt(stmt).form
                    else {
                        return Err(malformed("a loop stands at another form"));
                    };
                    loops.push(lash_kernel_doc::kernel_internals::LoopIteration {
                        site: site.clone(),
                        iteration: started.saturating_sub(1),
                    });
                }
                continue;
            }
            let frame = row(&self.rows, index, &self.access)?;
            for at in 0..frame.loops.len() {
                let record = row(&frame.loops, at, &self.access)?;
                loops.push(lash_kernel_doc::kernel_internals::LoopIteration {
                    site: record.site.resolve(&self.access)?.as_ref().clone(),
                    iteration: record.started.saturating_sub(1),
                });
            }
        }
        Ok(loops)
    }
}

/// `saved`'s path as a coordinate of a frame of `unit`: a site of another
/// unit is no site of the frame, and names none of its loops or cleanups.
fn path_in(saved: &Site, unit: &Unit) -> Result<SitePath, SitePath> {
    let path = SitePath(saved.path.as_slice().into());
    if saved.unit == *unit {
        Ok(path)
    } else {
        Err(path)
    }
}

fn site_path(site: &Site) -> SitePath {
    SitePath(site.path.as_slice().into())
}

/// The loop coordinate a saved loop row names, its site resolved.
fn loop_coordinate(
    record: &LoopRecord,
    unit: &Unit,
    access: &Access<'_>,
) -> Result<LoopCoordinate, Unrestored> {
    let site = path_in(record.site.resolve(access)?.as_ref(), unit)
        .map_err(|site| RestoreRefusal::UnknownLoop { site })?;
    let guard = record.cursor.and_then(|cursor| cursor.guard);
    Ok(LoopCoordinate {
        site,
        started: record.started,
        phase: match record.header {
            HeaderRecord::Body => LoopPhase::Body,
            HeaderRecord::Open => LoopPhase::Open,
            HeaderRecord::Pull => LoopPhase::Pull,
            HeaderRecord::Check => LoopPhase::Check,
            HeaderRecord::Exhausted => LoopPhase::Exhausted,
            HeaderRecord::Item => LoopPhase::Item,
        },
        cursor: record.cursor.map(|cursor| match cursor.kind {
            CursorKind::List | CursorKind::Tuple => coord::Cursor::Sequence {
                next: cursor.position,
            },
            CursorKind::Table => coord::Cursor::Table {
                last: (cursor.position != u64::MAX).then_some(cursor.position),
                size: guard.map_or(0, |guard| guard.size),
            },
            CursorKind::Text => coord::Cursor::Text {
                next: cursor.position,
            },
            CursorKind::Driver => coord::Cursor::Driver,
        }),
        remaining: guard.map(|guard| guard.remaining),
    })
}

/// Admits the frame's saved loop row `at` where restore finds a loop at
/// `site`: a `for` when `walks`, else a `while`, standing in its header
/// when `in_header`.
fn admitted_loop(
    record: &FrameRecord,
    at: u64,
    site: &Site,
    walks: bool,
    in_header: bool,
    access: &Access<'_>,
) -> Result<LoopRecord, Unrestored> {
    let saved = row(&record.loops, at, access)?;
    let coordinate = loop_coordinate(&saved, &site.unit, access)?;
    coord::admit_loop(&site_path(site), walks, in_header, &coordinate)?;
    Ok(saved)
}

/// Admits the frame's saved cleanup row `saved` where restore finds the
/// `try` at `site` in its `finally`.
fn admit_finally(
    saved: &FinallyRecord,
    site: &Site,
    departure: Departure,
    access: &Access<'_>,
) -> Result<(), Unrestored> {
    let saved = path_in(saved.site.resolve(access)?.as_ref(), &site.unit)
        .map_err(|site| RestoreRefusal::UnknownCleanup { site })?;
    let saved = CleanupCoordinate::Finally {
        site: saved,
        departure,
    };
    Ok(coord::admit_cleanup(&site_path(site), &saved)?)
}

/// The action kind the side table states for a nested action: `None` for
/// a `cancel`, which never stands.
fn action_kind(kind: &NestedKind) -> Option<ActionKind> {
    Some(match kind {
        NestedKind::Yield => ActionKind::Yield,
        NestedKind::Cancel => return None,
        NestedKind::Call(_) => ActionKind::Call,
        NestedKind::Body(_) => ActionKind::Body,
        NestedKind::Perform { then_yield, .. } => ActionKind::Perform {
            then_yield: *then_yield,
        },
        NestedKind::Sleep { then_yield } => ActionKind::Sleep {
            then_yield: *then_yield,
        },
        NestedKind::Spawn(_) => ActionKind::Spawn,
        NestedKind::Join => ActionKind::Join,
        NestedKind::JoinMany(_) => ActionKind::JoinMany,
    })
}

/// The page forms of `roles`, in their order: what a projection writes.
fn role_records(
    arena: &mut Arena,
    access: &Access<'_>,
    roles: &[Role],
) -> Result<Vec<RoleRecord>, HeapFault> {
    roles
        .iter()
        .map(|role| {
            Ok(RoleRecord {
                site: role.site.clone(),
                value: project(arena, access, &role.value)?,
            })
        })
        .collect()
}

/// The arena values of the saved `roles`, in their order.
fn admit_roles(
    arena: &mut Arena,
    access: &Access<'_>,
    roles: Vec<RoleRecord>,
) -> Result<Vec<Role>, HeapFault> {
    roles
        .into_iter()
        .map(|role| {
            Ok(Role {
                value: admit(arena, access, &role.value)?,
                site: role.site,
            })
        })
        .collect()
}

fn action_phase(phase: PhaseRecord) -> ActionPhase {
    match phase {
        PhaseRecord::Yielded => ActionPhase::Yielded,
        PhaseRecord::Called => ActionPhase::Called,
        PhaseRecord::Waiting => ActionPhase::Waiting,
        PhaseRecord::Spawned => ActionPhase::Spawned,
        PhaseRecord::Joining => ActionPhase::Joining,
    }
}

/// The resume entry of `pending`, the continuation a frame saved where it
/// stands at an action nested in statement `stmt`, at `site`, admitted by
/// the coordinate grammar beside `arrival` (`K-EVAL-008`): its action names
/// one of the statement's own nested actions under this layout.
fn continuation(
    exe: &Executable,
    stmt: StmtId,
    site: &Site,
    pending: &Pending,
    arrival: Arrival,
) -> Result<ActIx, RestoreRefusal> {
    let (stream, _, _) = exe.statement(stmt);
    let depth = site.path.len();
    let action = SitePath(pending.site.as_slice().into());
    let full = Site::new(
        site.unit.clone(),
        site.path
            .iter()
            .chain(&pending.site)
            .copied()
            .collect::<Vec<_>>(),
    );
    // The entry must be the statement's own: an action in a closure
    // written inside it belongs to the closure's statement.
    let Some((act, nested)) = stream
        .nested_at
        .get(&full)
        .map(|act| (*act, &stream.nested[act.ix()]))
        .filter(|(_, nested)| nested.depth as usize == depth)
    else {
        return Err(RestoreRefusal::UnknownSite { action });
    };
    let phase = action_phase(pending.phase);
    let Some(kind) = action_kind(&nested.kind) else {
        return Err(RestoreRefusal::IllegalPhase { action, phase });
    };
    let entry = ActionEntry {
        action: action.clone(),
        kind,
        roles: nested
            .roles
            .iter()
            .map(|(role, _)| SitePath(role.path[depth..].into()))
            .collect(),
    };
    let saved = StatementCoordinate::Nested {
        action,
        phase,
        roles: pending
            .roles
            .iter()
            .map(|role| SitePath(role.site.as_slice().into()))
            .collect(),
    };
    coord::admit(&entry, &saved, arrival)?;
    Ok(act)
}

/// How many arguments a saved call retains, if any.
fn retained(record: &FrameRecord) -> Option<u32> {
    record
        .arguments
        .as_ref()
        .map(|args| u32::try_from(args.len()).unwrap_or(u32::MAX))
}

/// Decodes the saved `record` of a frame the task comes to by `arrival`
/// into a resident frame, admitting each of its coordinates.
fn restore(
    record: &FrameRecord,
    arrival: Arrival,
    access: &Access<'static>,
    exe: &Arc<Executable>,
    arena: &mut Arena,
) -> Result<Box<Frame>, Unrestored> {
    let site = record.statement.resolve(access)?;
    let code_id = (0..=site.path.len())
        .rev()
        .find_map(|length| exe.code_at(&Site::new(site.unit.clone(), &site.path[..length])))
        .ok_or_else(|| malformed("a frame has no admitted code body"))?;
    let code = exe.code(code_id);
    let pending = match &record.pending {
        Some(pending) => Some(Pending {
            site: pending.site.clone(),
            phase: pending.phase,
            roles: admit_roles(arena, access, pending.roles(access)?)?,
        }),
        None => None,
    };
    let mut depth = code.site.path.len();
    let mut block = code.body;
    let mut control = Vec::new();
    let mut loops = 0;
    let mut finally = 0;
    let mut iterated = 0;
    let mut departing = 0;
    let mut awaiting = None;
    let mut nested = None;
    let nowhere = || malformed(format!("{} is no statement of the admitted document", site));
    // A policy call has already left every source scope. Rebuilding the
    // path to its return would incorrectly enter those scopes again.
    let adopting = exe.stmt_at(code_id, &site).filter(|stmt| {
        arrival == Arrival::Returned
            && pending.is_none()
            && matches!(exe.stmt(*stmt).form, Form::ReturnVia { .. })
    });
    if let Some(stmt) = adopting {
        awaiting = Some(stmt);
    }
    // A run-root cleanup has left every source scope: the frame stands at
    // its body, holding the departure the cleanup returns to.
    let cleaning = if *site == code.site
        && arrival == Arrival::Returned
        && pending.is_none()
        && exe.root_cleanup(code_id).is_some()
    {
        let saved = row(&record.finally, 0, access)?;
        let value = admit(arena, access, &row(&record.departing, 0, access)?)?;
        (finally, departing) = (1, 1);
        let (cleaned, departure) = match saved.entered {
            EnteredRecord::Return => (Ok(value), Departure::Return),
            EnteredRecord::Throw => (Err(value), Departure::Throw),
            _ => return Err(malformed("a root cleanup holds no return or raise").into()),
        };
        admit_finally(&saved, &code.site, departure, access)?;
        Some(cleaned)
    } else {
        None
    };
    while adopting.is_none() && cleaning.is_none() {
        let index = *site.path.get(depth).ok_or_else(nowhere)? as usize;
        let stmts = &exe.block(block).stmts;
        if depth + 1 == site.path.len() {
            let next = if let Some(pending) = &pending {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                nested = Some(continuation(exe, stmt, &site, pending, arrival)?);
                awaiting = Some(stmt);
                control.push(Control::Block {
                    block,
                    next: index + 1,
                });
                // A `while` waits in its condition: it stands above its loop.
                if let Form::While { site: at, .. } = &exe.stmt(stmt).form {
                    let saved = admitted_loop(record, loops, at, false, false, access)?;
                    loops += 1;
                    control.push(Control::While {
                        stmt,
                        started: saved.started,
                    });
                }
                break;
            } else if arrival != Arrival::Fresh {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                awaiting = Some(stmt);
                match &exe.stmt(stmt).form {
                    // Only a `then yield` wait owes a turn once delivered.
                    Form::Action(action)
                        if arrival == Arrival::Delivered && !action.kind.then_yield() =>
                    {
                        let action = site_path(&site);
                        return Err(RestoreRefusal::TaskState { action }.into());
                    }
                    Form::Action(_) => {}
                    // A `for` waits in a protocol hook of its header: it
                    // stands above its loop.
                    Form::For { site: at, .. } if arrival == Arrival::Returned => {
                        let saved = admitted_loop(record, loops, at, true, true, access)?;
                        loops += 1;
                        control.push(Control::Block {
                            block,
                            next: index + 1,
                        });
                        control.push(saved_for(
                            stmt,
                            &saved,
                            record,
                            &mut iterated,
                            access,
                            arena,
                        )?);
                        break;
                    }
                    _ => return Err(malformed("a resuming frame has no action").into()),
                }
                index + 1
            } else if index <= stmts.len() {
                index
            } else {
                return Err(nowhere().into());
            };
            control.push(Control::Block { block, next });
            break;
        }
        let stmt = *stmts.get(index).ok_or_else(nowhere)?;
        control.push(Control::Block {
            block,
            next: index + 1,
        });
        let inner = Site::new(site.unit.clone(), &site.path[..depth + 2]);
        let at = |candidate| exe.block(candidate).site == inner;
        block = match &exe.stmt(stmt).form {
            Form::If {
                then_block,
                else_block,
            } => [*then_block, *else_block]
                .into_iter()
                .find(|candidate| at(*candidate))
                .ok_or_else(nowhere)?,
            Form::For { site, body, .. } if at(*body) => {
                let saved = admitted_loop(record, loops, site, true, false, access)?;
                loops += 1;
                control.push(saved_for(
                    stmt,
                    &saved,
                    record,
                    &mut iterated,
                    access,
                    arena,
                )?);
                *body
            }
            // A loop's `else` runs once the loop has left the control.
            Form::For {
                else_block: Some(else_block),
                ..
            }
            | Form::While {
                else_block: Some(else_block),
                ..
            } if at(*else_block) => *else_block,
            Form::While { site, body, .. } if at(*body) => {
                let saved = admitted_loop(record, loops, site, false, false, access)?;
                loops += 1;
                control.push(Control::While {
                    stmt,
                    started: saved.started,
                });
                *body
            }
            Form::Try {
                site: try_site,
                body,
                catch,
                finally: cleanup,
            } => {
                let (phase, inner) = if at(*body) {
                    (TryPhase::Body, *body)
                } else if let Some((_, catch)) = catch.filter(|(_, catch)| at(*catch)) {
                    (TryPhase::Catch, catch)
                } else if let Some(cleanup) = cleanup.filter(|cleanup| at(*cleanup)) {
                    let saved = row(&record.finally, finally, access)?;
                    finally += 1;
                    let mut value = || {
                        let value = row(&record.departing, departing, access)?;
                        departing += 1;
                        admit(arena, access, &value)
                    };
                    let (completion, departure) = match &saved.entered {
                        EnteredRecord::Normal => (Completion::Normal, Departure::Normal),
                        EnteredRecord::Break => (Completion::Break, Departure::Break),
                        EnteredRecord::Continue => (Completion::Continue, Departure::Continue),
                        EnteredRecord::Throw => {
                            (Completion::Throw(Box::new(value()?)), Departure::Throw)
                        }
                        EnteredRecord::Return => {
                            (Completion::Return(Box::new(value()?)), Departure::Return)
                        }
                        EnteredRecord::ReturnVia(via) => {
                            let via = via.resolve(access)?;
                            let stmt = exe
                                .stmt_at(code_id, &via)
                                .filter(|stmt| {
                                    matches!(exe.stmt(*stmt).form, Form::ReturnVia { .. })
                                })
                                .ok_or_else(|| RestoreRefusal::UnknownSite {
                                    action: site_path(&via),
                                })?;
                            let completion = Completion::ReturnVia {
                                value: Box::new(value()?),
                                stmt,
                            };
                            (completion, Departure::ReturnVia(site_path(&via)))
                        }
                    };
                    admit_finally(&saved, try_site, departure, access)?;
                    (TryPhase::Finally(completion), cleanup)
                } else {
                    return Err(nowhere().into());
                };
                control.push(Control::Try { stmt, phase });
                inner
            }
            _ => return Err(nowhere().into()),
        };
        depth += 2;
    }
    if loops != record.loops.len()
        || finally != record.finally.len()
        || iterated != record.iterated.len()
        || departing != record.departing.len()
    {
        return Err(malformed("a frame has excess saved control rows").into());
    }
    let library = match (&code.site.unit, code.site.path.is_empty()) {
        (Unit::Library(function), true) => {
            let lib = exe
                .lib_of(function)
                .ok_or_else(|| malformed("a frame's library is not admitted"))?;
            let arity = u32::try_from(exe.lib(lib).arity)
                .map_err(|_| malformed("a library function's arity overflows"))?;
            coord::admit_arguments(retained(record), Some(arity))?;
            let mut args = Vec::new();
            if let Some(saved) = &record.arguments {
                for at in 0..saved.len() {
                    args.push(admit(arena, access, &row(saved, at, access)?)?);
                }
            }
            Some(LibraryCall {
                lib,
                args: args.into(),
            })
        }
        _ => {
            coord::admit_arguments(retained(record), None)?;
            None
        }
    };
    let mut slots = Slots::default();
    slots.fresh(code.slots_by_at.len());
    for (at, value) in slots.values.iter_mut().enumerate() {
        let info = &code.slots[code.slots_by_at[at] as usize];
        *value = match record.bindings.get(&info.key, access)? {
            None => SlotState::Empty,
            Some(BindingRecord {
                value: BoundRecord::Value(value),
                ..
            }) => SlotState::Value(admit(arena, access, &value)?),
            Some(BindingRecord {
                value: BoundRecord::Shared(id),
                ..
            }) => SlotState::Cell(admit_cell(arena, id)),
        };
    }
    Ok(Box::new(Frame {
        slots,
        code: code_id,
        statement: record.statement.clone(),
        pending: Continuation {
            saved: if pending.is_some() {
                Saved::This
            } else {
                Saved::Nothing
            },
            pending,
        },
        control,
        awaiting,
        nested,
        library,
        cleaning,
        held: Vec::new(),
        owned: true,
    }))
}

fn header_record(header: &Header) -> HeaderRecord {
    match header {
        Header::Body => HeaderRecord::Body,
        Header::Open => HeaderRecord::Open,
        Header::Pull => HeaderRecord::Pull,
        Header::Check(_) => HeaderRecord::Check,
        Header::Exhausted => HeaderRecord::Exhausted,
        Header::Item(_) => HeaderRecord::Item,
    }
}

fn cursor_record(cursor: &Cursor, guard: Option<&Guard>) -> Option<CursorRecord> {
    let table = |at: &Option<TableCursor>| at.map(|at| at.0).unwrap_or(u64::MAX);
    let (kind, position) = match cursor {
        Cursor::Opening => return None,
        Cursor::List(_, at) => (CursorKind::List, *at as u64),
        Cursor::Tuple(_, at) => (CursorKind::Tuple, *at as u64),
        Cursor::Map(_, at) | Cursor::Set(_, at) => (CursorKind::Table, table(at)),
        Cursor::Text(text) => (CursorKind::Text, text.position()),
        Cursor::Driver(_) => (CursorKind::Driver, 0),
    };
    Some(CursorRecord {
        kind,
        position,
        guard: guard.map(|guard| GuardRecord {
            size: guard.size,
            remaining: guard.remaining,
        }),
    })
}

/// The `for` loop of statement `stmt` as `saved` records it, its
/// coordinate admitted, its held values the frame's next `iterated` rows:
/// what it walks, then the item a hook holds (`K-ITER-005`).
fn saved_for(
    stmt: StmtId,
    saved: &LoopRecord,
    record: &FrameRecord,
    iterated: &mut u64,
    access: &Access<'_>,
    arena: &mut Arena,
) -> Result<Control, HeapFault> {
    let mut held = |arena: &mut Arena| {
        let value = row(&record.iterated, *iterated, access)?.0;
        *iterated += 1;
        admit(arena, access, &value)
    };
    let (cursor, guard) = match saved.cursor {
        None => (Cursor::Opening, None),
        Some(CursorRecord {
            kind,
            position,
            guard,
        }) => {
            let walked = held(arena)?;
            let cursor = match (kind, walked) {
                (CursorKind::List, Value::List(id)) => Cursor::List(
                    id,
                    usize::try_from(position).map_err(|_| malformed("list cursor overflows"))?,
                ),
                (CursorKind::Tuple, Value::Tuple(tuple))
                    if arena.len(tuple).is_ok_and(|len| position <= len) =>
                {
                    Cursor::Tuple(tuple, position as usize)
                }
                (CursorKind::Table, Value::Map(id)) => {
                    Cursor::Map(id, (position != u64::MAX).then_some(TableCursor(position)))
                }
                (CursorKind::Table, Value::Set(id)) => {
                    Cursor::Set(id, (position != u64::MAX).then_some(TableCursor(position)))
                }
                (CursorKind::Text, text @ (Value::Text(_) | Value::LongText(_)))
                    if position <= text_chars(arena, &text) =>
                {
                    Cursor::Text(Box::new(TextCursor::new(text, position)))
                }
                (CursorKind::Driver, driver) if position == 0 => Cursor::Driver(Box::new(driver)),
                _ => return Err(malformed("a for cursor has another kind or extent")),
            };
            let guard =
                guard.map(|GuardRecord { size, remaining }| Box::new(Guard { size, remaining }));
            (cursor, guard)
        }
    };
    let header = match saved.header {
        HeaderRecord::Body => Header::Body,
        HeaderRecord::Open => Header::Open,
        HeaderRecord::Pull => Header::Pull,
        HeaderRecord::Check => Header::Check(Box::new(held(arena)?)),
        HeaderRecord::Exhausted => Header::Exhausted,
        HeaderRecord::Item => Header::Item(Box::new(held(arena)?)),
    };
    Ok(Control::For {
        stmt,
        cursor,
        started: saved.started,
        header,
        guard,
    })
}

/// How many scalars the text `text` holds.
fn text_chars(arena: &Arena, text: &Value) -> u64 {
    match text {
        Value::Text(short) => short.as_str().chars().count() as u64,
        Value::LongText(handle) => match arena.body(*handle) {
            Ok(lash_kernel_doc::arena::Body::LongText(long)) => long.summary.chars,
            _ => 0,
        },
        _ => 0,
    }
}
