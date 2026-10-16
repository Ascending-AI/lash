//! Derived frame coordinates beside the canonical paged frame stack.
//! Slots are mutable while a frame executes. Synchronization publishes only
//! their pending edits to the declaring-site tree, beside its control records.
//!
//! A frame that stands at an action nested in its statement's expression
//! is decoded through the grammar of saved continuations (`K-EVAL-008`):
//! the action and its roles are found by site in the statement's resume
//! entries under this layout, and the phase must agree with the action's
//! kind and with how the task comes to the frame.
use super::loops::{Guard, Header, TextCursor};
use super::{Completion, Control, Cursor, Frame, LibraryCall, SlotState, TryPhase};
use crate::compile::{Executable, Form, LibId, NestedKind, StmtId};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{Site, Unit, Value};
use lash_kernel_exec::ActIx;
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, CursorKind, CursorRecord, EnteredRecord, FinallyRecord,
    FrameRecord, GuardRecord, HeaderRecord, IncomingRecord, IteratedRecord, LoopRecord,
    PendingRecord, PhaseRecord, RoleRecord, TableCursor, TaskStateRecord,
};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

fn malformed(problem: impl Into<String>) -> HeapFault {
    HeapFault::Malformed(problem.into())
}

/// How a task comes to the top frame being decoded: what a saved
/// continuation there may claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParkKind {
    /// The frame stands where it was last pushed or published, and nothing
    /// resumes it: it stands between statements.
    Fresh,
    /// The frame above it has just returned or raised into it.
    Returned,
    /// The task waits on a request, or its outcome has been delivered and
    /// its turn taken.
    Requested,
    /// The outcome of a `then yield` wait has been delivered, by result,
    /// raise or `cancel`, and the task has not yet taken the one more turn
    /// it takes before it continues (`K-TASK-005`).
    Delivered,
    /// A raise has been delivered: a `cancel` at the task's wait or turn.
    Raised,
    /// A value has been delivered: a `spawn`'s handle among them.
    Valued { handle: bool },
    /// The task joins, or a join has decided.
    Joined,
}

impl ParkKind {
    /// How a task in `state` comes to its top frame.
    pub(super) fn of(state: &TaskStateRecord) -> Self {
        match state {
            TaskStateRecord::Resuming { requeue: true, .. } => Self::Delivered,
            TaskStateRecord::Waiting(_)
            | TaskStateRecord::Resuming {
                incoming: IncomingRecord::Outcome(_),
                ..
            } => Self::Requested,
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Raise(_),
                ..
            } => Self::Raised,
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(value),
                ..
            } => Self::Valued {
                handle: matches!(value, Value::Task(_)),
            },
            TaskStateRecord::Joining(_)
            | TaskStateRecord::JoiningMany(_)
            | TaskStateRecord::Resuming {
                incoming: IncomingRecord::Joined(_),
                ..
            } => Self::Joined,
            _ => Self::Fresh,
        }
    }
}

/// Why a saved continuation is refused where a frame is decoded
/// (`K-EVAL-008`): each names the site it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationRefusal {
    /// The saved action site is no action nested in the frame's statement.
    UnknownSite { site: Site },
    /// The phase cannot stand at that action, or where the task is.
    IllegalPhase { site: Site, phase: &'static str },
    /// A role the action's continuation keeps is not saved.
    MissingRole { site: Site },
    /// A saved role is none the action's continuation keeps.
    ExtraRole { site: Site },
    /// The task's state does not agree with the phase.
    TaskState { site: Site },
}

impl fmt::Display for ContinuationRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSite { site } => write!(out, "no nested action at {site}"),
            Self::IllegalPhase { site, phase } => {
                write!(out, "the action at {site} cannot stand at phase {phase}")
            }
            Self::MissingRole { site } => write!(out, "the role {site} is not saved"),
            Self::ExtraRole { site } => write!(out, "the saved role {site} is no role"),
            Self::TaskState { site } => {
                write!(out, "the task's state disagrees with the action at {site}")
            }
        }
    }
}

fn phase_name(phase: PhaseRecord) -> &'static str {
    match phase {
        PhaseRecord::Called => "called",
        PhaseRecord::Waiting => "waiting",
        PhaseRecord::Spawned => "spawned",
        PhaseRecord::Joining => "joining",
    }
}

fn row<T: Cell>(tree: &Tree<T>, index: u64, access: &Access<'_>) -> Result<T, HeapFault> {
    tree.rank_get(index, access)?
        .map(|(_, value)| value)
        .ok_or_else(|| malformed("frame control row is missing"))
}
fn write<T: Cell>(
    tree: &mut Tree<T>,
    index: u64,
    value: T,
    access: &Access<'_>,
) -> Result<(), HeapFault> {
    let key = ordinal_key(index);
    if let Some(old) = tree.get(&key, access)?
        && old.same_version(&value)
    {
        return Ok(());
    }
    tree.write(key, value, access)
}
fn truncate<T: Cell>(tree: &mut Tree<T>, len: u64, access: &Access<'_>) -> Result<(), HeapFault> {
    while tree.len() > len {
        let (key, _) = tree
            .rank_get(tree.len() - 1, access)?
            .ok_or_else(|| malformed("frame control tail is missing"))?;
        tree.remove(&key, access)?;
    }
    Ok(())
}

fn site_ref(
    old: Option<lash_kernel_state::paged::SiteRef>,
    site: &lash_kernel_doc::Site,
    access: &Access<'_>,
) -> Result<lash_kernel_state::paged::SiteRef, HeapFault> {
    if let Some(old) = old
        && old.matches(site, access)?
    {
        return Ok(old);
    }
    lash_kernel_state::paged::SiteRef::new(site, access)
}

/// A slot is decoded on its first read after resume. Pending edits are
/// authoritative until a publication; repeated assignments touch no tree.
#[derive(Debug, Default)]
pub(super) struct Slots {
    values: Vec<Slot>,
    pending: Vec<usize>,
}

#[derive(Debug)]
struct Slot {
    value: Option<SlotState>,
    dirty: bool,
}

impl Slots {
    pub(super) fn fresh(&mut self, len: usize) {
        self.clear();
        self.values.resize_with(len, || Slot {
            value: Some(SlotState::Empty),
            dirty: false,
        });
    }

    fn unloaded(len: usize) -> Self {
        Self {
            values: (0..len)
                .map(|_| Slot {
                    value: None,
                    dirty: false,
                })
                .collect(),
            pending: Vec::new(),
        }
    }

    pub(super) fn clear(&mut self) {
        self.values.clear();
        self.pending.clear();
    }
}

impl Frame {
    /// Borrow the decoded state: a write need not clone the value it replaces.
    pub(super) fn slot(&mut self, at: u32) -> Result<&SlotState, HeapFault> {
        let slot = &mut self.slots.values[at as usize];
        if slot.value.is_none() {
            let code = self.exe.code(self.code);
            let info = &code.slots[code.slots_by_at[at as usize] as usize];
            slot.value = Some(match self.record.bindings.get(&info.key, &self.access)? {
                None => SlotState::Empty,
                Some(BindingRecord {
                    value: BoundRecord::Value(value),
                    ..
                }) => SlotState::Value(value),
                Some(BindingRecord {
                    value: BoundRecord::Shared(id),
                    ..
                }) => SlotState::Cell(id),
            });
        }
        slot.value
            .as_ref()
            .ok_or_else(|| malformed("a decoded slot has no value"))
    }

    pub(super) fn set_slot(&mut self, at: u32, value: SlotState) {
        let at = at as usize;
        let slot = &mut self.slots.values[at];
        slot.value = Some(value);
        if !slot.dirty {
            slot.dirty = true;
            self.slots.pending.push(at);
        }
    }

    fn sync_slots(&mut self) -> Result<(), HeapFault> {
        let code = self.exe.code(self.code);
        for &at in &self.slots.pending {
            let info = &code.slots[code.slots_by_at[at] as usize];
            let slot = &mut self.slots.values[at];
            let value = match slot.value.as_ref() {
                Some(SlotState::Empty) => {
                    self.record.bindings.remove(&info.key, &self.access)?;
                    slot.dirty = false;
                    continue;
                }
                Some(SlotState::Value(value)) => BoundRecord::Value(value.clone()),
                Some(SlotState::Cell(id)) => BoundRecord::Shared(*id),
                None => return Err(malformed("a pending slot has no value")),
            };
            let (name, declared) = match self.record.bindings.get(&info.key, &self.access)? {
                Some(old) => (old.name, old.declared),
                None => (
                    lash_kernel_state::paged::NameRef::new(&info.name, &self.access)?,
                    lash_kernel_state::paged::SiteRef::new(&info.declared, &self.access)?,
                ),
            };
            self.record.bindings.write(
                info.key.clone(),
                BindingRecord {
                    name,
                    declared,
                    value,
                },
                &self.access,
            )?;
            slot.dirty = false;
        }
        self.slots.pending.clear();
        Ok(())
    }
}

impl Frame {
    /// Brings the record up to the frame: its coordinates and its held
    /// values.
    pub(super) fn sync(&mut self) -> Result<(), HeapFault> {
        self.sync_to(true)
    }

    /// Brings the record's held values up to the frame, leaving its
    /// coordinates (statement, loop positions, cleanups) where they were:
    /// what pressure accounting and a trace read (FIG-6318).
    pub(super) fn sync_holdings(&mut self) -> Result<(), HeapFault> {
        self.sync_to(false)
    }

    fn sync_to(&mut self, coordinates: bool) -> Result<(), HeapFault> {
        self.sync_slots()?;
        // A frame stands at its block's next statement, or at the statement
        // it waits in: a `while` waits in its condition above its loop, a
        // `for` in a protocol hook of its header.
        let block = match self.control.as_slice() {
            [.., Control::Block { block, next }] => Some((*block, *next)),
            [
                ..,
                Control::Block { block, next },
                Control::While { .. } | Control::For { .. },
            ] if self.awaiting.is_some() => Some((*block, *next)),
            _ => None,
        };
        if coordinates && let Some((block, next)) = block {
            let index = if self.awaiting.is_some() {
                next.checked_sub(1)
                    .ok_or_else(|| malformed("a frame awaits a statement it has not issued"))?
            } else {
                next
            };
            let statement =
                self.exe.block(block).site.child(
                    u32::try_from(index).map_err(|_| malformed("statement ordinal overflows"))?,
                );
            if !self.record.statement.matches(&statement, &self.access)? {
                self.record.statement =
                    lash_kernel_state::paged::SiteRef::new(&statement, &self.access)?;
            }
        }
        // Expression evaluation may temporarily stand at a loop test or
        // a departure. The coordinate is refreshed at its next safe point;
        // its held values must already be visible to pressure accounting.
        let mut loops = 0;
        let mut iterated = 0;
        let mut finally = 0;
        let mut departing = 0;
        for control in &self.control {
            match control {
                Control::For {
                    stmt,
                    cursor,
                    started,
                    header,
                    guard,
                } => {
                    let Form::For { site, .. } = &self.exe.stmt(*stmt).form else {
                        return Err(malformed("a for cursor stands at another form"));
                    };
                    if coordinates {
                        let saved_site = site_ref(
                            self.record
                                .loops
                                .get(&ordinal_key(loops), &self.access)?
                                .map(|record| record.site),
                            site,
                            &self.access,
                        )?;
                        write(
                            &mut self.record.loops,
                            loops,
                            LoopRecord {
                                site: saved_site,
                                started: *started,
                                header: header_record(header),
                                cursor: cursor_record(cursor, guard.as_deref()),
                            },
                            &self.access,
                        )?;
                    }
                    // What it walks, then the item a hook holds.
                    for value in cursor.walked().into_iter().chain(header.held()) {
                        write(
                            &mut self.record.iterated,
                            iterated,
                            IteratedRecord(value),
                            &self.access,
                        )?;
                        iterated += 1;
                    }
                    loops += 1;
                }
                Control::While { stmt, started } => {
                    let Form::While { site, .. } = &self.exe.stmt(*stmt).form else {
                        return Err(malformed("a while cursor stands at another form"));
                    };
                    if coordinates {
                        let saved_site = site_ref(
                            self.record
                                .loops
                                .get(&ordinal_key(loops), &self.access)?
                                .map(|record| record.site),
                            site,
                            &self.access,
                        )?;
                        write(
                            &mut self.record.loops,
                            loops,
                            LoopRecord {
                                site: saved_site,
                                started: *started,
                                header: HeaderRecord::Body,
                                cursor: None,
                            },
                            &self.access,
                        )?;
                    }
                    loops += 1;
                }
                Control::Try {
                    stmt,
                    phase: TryPhase::Finally(completion),
                } => {
                    let Form::Try { site, .. } = &self.exe.stmt(*stmt).form else {
                        return Err(malformed("a cleanup stands at another form"));
                    };
                    let entered = match completion {
                        Completion::Normal => EnteredRecord::Normal,
                        Completion::Break => EnteredRecord::Break,
                        Completion::Continue => EnteredRecord::Continue,
                        Completion::Return(value) | Completion::Throw(value) => {
                            write(
                                &mut self.record.departing,
                                departing,
                                value.as_ref().clone(),
                                &self.access,
                            )?;
                            departing += 1;
                            if matches!(completion, Completion::Return(_)) {
                                EnteredRecord::Return
                            } else {
                                EnteredRecord::Throw
                            }
                        }
                    };
                    if coordinates {
                        let saved_site = site_ref(
                            self.record
                                .finally
                                .get(&ordinal_key(finally), &self.access)?
                                .map(|record| record.site),
                            site,
                            &self.access,
                        )?;
                        write(
                            &mut self.record.finally,
                            finally,
                            FinallyRecord {
                                site: saved_site,
                                entered,
                            },
                            &self.access,
                        )?;
                    }
                    finally += 1;
                }
                _ => {}
            }
        }
        if coordinates {
            truncate(&mut self.record.loops, loops, &self.access)?;
            truncate(&mut self.record.finally, finally, &self.access)?;
        }
        truncate(&mut self.record.iterated, iterated, &self.access)?;
        truncate(&mut self.record.departing, departing, &self.access)?;
        if let Some(call) = &self.library {
            if self.exe.lib(call.lib).native {
                self.record.arguments = Some(call.args.clone());
            } else {
                self.record.arguments = None;
            }
        } else {
            self.record.arguments = None;
        }
        Ok(())
    }
}

pub(super) struct Frames {
    pub(super) rows: Tree<FrameRecord>,
    len: u64,
    // Resident frames are ordered by depth. Cold lower frames stay in rows
    // until a return reaches them; push and pop always use the resident top.
    cache: Vec<(u64, Box<Frame>)>,
    access: Access<'static>,
    exe: Arc<Executable>,
    parked: ParkKind,
}
impl Frames {
    pub(super) fn new(access: Access<'static>, exe: Arc<Executable>) -> Self {
        Self::open(Tree::new(true), access, exe, ParkKind::Fresh)
    }
    pub(super) fn open(
        rows: Tree<FrameRecord>,
        access: Access<'static>,
        exe: Arc<Executable>,
        parked: ParkKind,
    ) -> Self {
        Self {
            len: rows.len(),
            rows,
            cache: Vec::new(),
            access,
            exe,
            parked,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(super) fn last_mut(&mut self) -> Result<Option<&mut Frame>, HeapFault> {
        if self.is_empty() {
            return Ok(None);
        }
        let index = self.len - 1;
        if self.cache.last().is_none_or(|(at, _)| *at != index) {
            let record = row(&self.rows, index, &self.access)?;
            let frame = restore(
                record,
                self.parked,
                self.access.clone(),
                Arc::clone(&self.exe),
            )?;
            self.cache.push((index, frame));
        }
        Ok(self.cache.last_mut().map(|(_, frame)| frame.as_mut()))
    }
    pub(super) fn push(&mut self, frame: Box<Frame>) {
        // The live cache owns unpublished calls and their callers. Only a
        // reader needs their persistent rows; returning before that reader
        // discards the call without ever writing it.
        self.cache.push((self.len, frame));
        self.len += 1;
        self.parked = ParkKind::Fresh;
    }
    pub(super) fn pop(&mut self) -> Result<Option<Box<Frame>>, HeapFault> {
        if self.is_empty() {
            return Ok(None);
        }
        self.last_mut()?;
        let frame = self.cache.pop().map(|(_, frame)| frame);
        self.len -= 1;
        self.parked = ParkKind::Returned;
        Ok(frame)
    }
    pub(super) fn clear(&mut self) {
        self.rows = Tree::new(true);
        self.len = 0;
        self.cache.clear();
        self.parked = ParkKind::Fresh;
    }
    pub(super) fn flush(&mut self) -> Result<(), HeapFault> {
        truncate(&mut self.rows, self.len, &self.access)?;
        for (index, frame) in &mut self.cache {
            frame.sync()?;
            if !frame
                .published
                .as_ref()
                .is_some_and(|old| old.same_version(&frame.record))
            {
                self.rows
                    .write(ordinal_key(*index), frame.record.clone(), &self.access)?;
                frame.published = Some(frame.record.clone());
            }
        }
        Ok(())
    }
    /// Publishes what the cached frames hold, not where they stand: a
    /// frame whose held values did not change since its row was written
    /// is not written again (FIG-6318). The cache stays the authority for
    /// coordinates until [`Self::flush`].
    pub(super) fn flush_holdings(&mut self) -> Result<(), HeapFault> {
        truncate(&mut self.rows, self.len, &self.access)?;
        for (index, frame) in &mut self.cache {
            frame.sync_holdings()?;
            if !frame
                .published
                .as_ref()
                .is_some_and(|old| old.same_holdings(&frame.record))
            {
                self.rows
                    .write(ordinal_key(*index), frame.record.clone(), &self.access)?;
                frame.published = Some(frame.record.clone());
            }
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
    /// in its decoded frame when the cache holds it, else in its row.
    /// `false` when no frame has that index.
    #[cfg(test)]
    pub(super) fn pending_mut(
        &mut self,
        index: u64,
        edit: &mut dyn FnMut(&mut Option<PendingRecord>, &Access<'_>),
    ) -> Result<bool, HeapFault> {
        let access = self.access.clone();
        if let Some((_, frame)) = self.cache.iter_mut().find(|(at, _)| *at == index) {
            edit(&mut frame.record.pending, &access);
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

    /// The frames the cache holds, by their index in the stack.
    pub(super) fn cached(&self) -> Vec<(usize, &Frame)> {
        self.cache
            .iter()
            .map(|(index, frame)| (*index as usize, frame.as_ref()))
            .collect()
    }
    pub(super) fn loop_coordinates(
        &mut self,
    ) -> Result<Vec<lash_kernel_doc::LoopIteration>, HeapFault> {
        self.flush()?;
        let mut loops = Vec::new();
        for index in 0..self.rows.len() {
            let frame = row(&self.rows, index, &self.access)?;
            for at in 0..frame.loops.len() {
                let record = row(&frame.loops, at, &self.access)?;
                loops.push(lash_kernel_doc::LoopIteration {
                    site: record.site.resolve(&self.access)?.as_ref().clone(),
                    iteration: record.started.saturating_sub(1),
                });
            }
        }
        Ok(loops)
    }
}

fn restore(
    record: FrameRecord,
    parked: ParkKind,
    access: Access<'static>,
    exe: Arc<Executable>,
) -> Result<Box<Frame>, HeapFault> {
    let site = record.statement.resolve(&access)?;
    let code_id = (0..site.path.len())
        .rev()
        .find_map(|length| exe.code_at(&Site::new(site.unit.clone(), &site.path[..length])))
        .ok_or_else(|| malformed("a frame has no admitted code body"))?;
    let code = exe.code(code_id);
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
    loop {
        let index = *site.path.get(depth).ok_or_else(nowhere)? as usize;
        let stmts = &exe.block(block).stmts;
        if depth + 1 == site.path.len() {
            let next = if let Some(pending) = &record.pending {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                let saved = pending.roles(&access)?;
                let act = continuation(&exe, stmt, &site, pending, &saved, parked)
                    .map_err(|refusal| malformed(format!("continuation refused: {refusal}")))?;
                nested = Some(act);
                awaiting = Some(stmt);
                control.push(Control::Block {
                    block,
                    next: index + 1,
                });
                // A `while` waits in its condition: it stands above its loop.
                if let Form::While { site: at, .. } = &exe.stmt(stmt).form {
                    let saved = row(&record.loops, loops, &access)?;
                    loops += 1;
                    if !saved.site.matches(at, &access)? || saved.cursor.is_some() {
                        return Err(malformed("a saved while has another site or a cursor"));
                    }
                    control.push(Control::While {
                        stmt,
                        started: saved.started,
                    });
                }
                break;
            } else if parked != ParkKind::Fresh {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                awaiting = Some(stmt);
                match &exe.stmt(stmt).form {
                    // Only a `then yield` wait owes a turn once delivered.
                    Form::Action(action)
                        if parked == ParkKind::Delivered && !action.kind.then_yield() =>
                    {
                        return Err(malformed(format!(
                            "the task's state disagrees with the action at {site}"
                        )));
                    }
                    Form::Action(_) => {}
                    // A `for` waits in a protocol hook of its header: it
                    // stands above its loop.
                    Form::For { site: at, .. } if parked == ParkKind::Returned => {
                        let saved = row(&record.loops, loops, &access)?;
                        loops += 1;
                        if !saved.site.matches(at, &access)? || saved.header == HeaderRecord::Body {
                            return Err(malformed("a saved header has another site or none"));
                        }
                        control.push(Control::Block {
                            block,
                            next: index + 1,
                        });
                        control.push(saved_for(stmt, &saved, &record, &mut iterated, &access)?);
                        break;
                    }
                    _ => return Err(malformed("a resuming frame has no action")),
                }
                index + 1
            } else if index <= stmts.len() {
                index
            } else {
                return Err(nowhere());
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
                let saved = row(&record.loops, loops, &access)?;
                loops += 1;
                if !saved.site.matches(site, &access)? || saved.header != HeaderRecord::Body {
                    return Err(malformed("a saved loop belongs to another site or header"));
                }
                control.push(saved_for(stmt, &saved, &record, &mut iterated, &access)?);
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
                let saved = row(&record.loops, loops, &access)?;
                loops += 1;
                if !saved.site.matches(site, &access)? || saved.cursor.is_some() {
                    return Err(malformed("a saved while has another site or a cursor"));
                }
                control.push(Control::While {
                    stmt,
                    started: saved.started,
                });
                *body
            }
            Form::Try {
                site,
                body,
                catch,
                finally: cleanup,
            } => {
                let (phase, inner) = if at(*body) {
                    (TryPhase::Body, *body)
                } else if let Some((_, catch)) = catch.filter(|(_, catch)| at(*catch)) {
                    (TryPhase::Catch, catch)
                } else if let Some(cleanup) = cleanup.filter(|cleanup| at(*cleanup)) {
                    let saved = row(&record.finally, finally, &access)?;
                    finally += 1;
                    if !saved.site.matches(site, &access)? {
                        return Err(malformed("a cleanup belongs to another site"));
                    }
                    let mut value = || {
                        let value = row(&record.departing, departing, &access)?;
                        departing += 1;
                        Ok::<_, HeapFault>(value)
                    };
                    let completion = match saved.entered {
                        EnteredRecord::Normal => Completion::Normal,
                        EnteredRecord::Break => Completion::Break,
                        EnteredRecord::Continue => Completion::Continue,
                        EnteredRecord::Throw => Completion::Throw(Box::new(value()?)),
                        EnteredRecord::Return => Completion::Return(Box::new(value()?)),
                    };
                    (TryPhase::Finally(completion), cleanup)
                } else {
                    return Err(nowhere());
                };
                control.push(Control::Try { stmt, phase });
                inner
            }
            _ => return Err(nowhere()),
        };
        depth += 2;
    }
    if loops != record.loops.len()
        || finally != record.finally.len()
        || iterated != record.iterated.len()
        || departing != record.departing.len()
    {
        return Err(malformed("a frame has excess saved control rows"));
    }
    let library = match (&code.site.unit, code.site.path.is_empty()) {
        (Unit::Library(function), true) => {
            let lib = exe
                .lib_of(function)
                .ok_or_else(|| malformed("a frame's library is not admitted"))?;
            let args = match (exe.lib(lib).native, &record.arguments) {
                (true, Some(args)) => args.clone(),
                (false, None) => Tree::new(true),
                _ => return Err(malformed("a library frame has the wrong held arguments")),
            };
            Some(LibraryCall { lib, args })
        }
        _ if record.arguments.is_none() => None,
        _ => return Err(malformed("a non-library frame holds arguments")),
    };
    Ok(Box::new(Frame {
        slots: Slots::unloaded(code.slots_by_at.len()),
        code: code_id,
        published: Some(record.clone()),
        record,
        access,
        exe,
        control,
        awaiting,
        nested,
        library,
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
        Cursor::Text(text) => (CursorKind::Text, text.index),
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

/// The `for` loop of statement `stmt` as `saved` records it, its held
/// values the frame's next `iterated` rows: what it walks, then the item a
/// hook holds (`K-ITER-005`).
fn saved_for(
    stmt: StmtId,
    saved: &LoopRecord,
    record: &FrameRecord,
    iterated: &mut u64,
    access: &Access<'_>,
) -> Result<Control, HeapFault> {
    let mut held = || {
        let value = row(&record.iterated, *iterated, access)?.0;
        *iterated += 1;
        Ok::<_, HeapFault>(value)
    };
    let (cursor, guard) = match (saved.cursor, saved.header) {
        (None, HeaderRecord::Open) => (Cursor::Opening, None),
        (
            Some(CursorRecord {
                kind,
                position,
                guard,
            }),
            header,
        ) if header != HeaderRecord::Open => {
            let cursor = match (kind, held()?) {
                (CursorKind::List, Value::List(id)) => Cursor::List(
                    id,
                    usize::try_from(position).map_err(|_| malformed("list cursor overflows"))?,
                ),
                (CursorKind::Tuple, Value::Tuple(tuple)) if position <= tuple.summary.len => {
                    Cursor::Tuple(tuple, position as usize)
                }
                (CursorKind::Table, Value::Map(id)) => {
                    Cursor::Map(id, (position != u64::MAX).then_some(TableCursor(position)))
                }
                (CursorKind::Table, Value::Set(id)) => {
                    Cursor::Set(id, (position != u64::MAX).then_some(TableCursor(position)))
                }
                (CursorKind::Text, Value::Text(text)) if position <= text.summary.chars => {
                    // A text held in the activation is walked by byte
                    // offset; a paged one by index.
                    let offset = match &text.body {
                        lash_kernel_doc::heap::Payload::Resident(body) => body
                            .char_indices()
                            .nth(position as usize)
                            .map_or(body.len(), |(at, _)| at),
                        lash_kernel_doc::heap::Payload::Paged(_) => 0,
                    };
                    Cursor::Text(Box::new(TextCursor {
                        text,
                        index: position,
                        offset,
                    }))
                }
                (CursorKind::Driver, driver) if position == 0 => Cursor::Driver(Box::new(driver)),
                _ => return Err(malformed("a for cursor has another kind or extent")),
            };
            let guard = match (guard, &cursor) {
                (None, _) => None,
                (Some(GuardRecord { size, remaining }), Cursor::Map(..) | Cursor::Set(..)) => {
                    Some(Box::new(Guard { size, remaining }))
                }
                _ => return Err(malformed("a guard stands on no map or set")),
            };
            (cursor, guard)
        }
        _ => return Err(malformed("a for's header and cursor disagree")),
    };
    let header = match saved.header {
        HeaderRecord::Body => Header::Body,
        HeaderRecord::Open => Header::Open,
        HeaderRecord::Pull if matches!(cursor, Cursor::Driver(_)) => Header::Pull,
        HeaderRecord::Check if guard.is_some() => Header::Check(Box::new(held()?)),
        HeaderRecord::Exhausted if guard.is_some() => Header::Exhausted,
        HeaderRecord::Item => Header::Item(Box::new(held()?)),
        _ => return Err(malformed("a for's header stands where its cursor cannot")),
    };
    Ok(Control::For {
        stmt,
        cursor,
        started: saved.started,
        header,
        guard,
    })
}

/// The resume entry of `pending`, the continuation a frame saved where it
/// stands at an action nested in statement `stmt`, at `site`: when the
/// grammar admits it beside `parked` (`K-EVAL-008`). Its action names one
/// of the statement's nested actions, its phase is that action's and can
/// stand where the task is, and its roles are exactly the action's, as a
/// set. Sites in the record are path suffixes under `site`.
fn continuation(
    exe: &Executable,
    stmt: StmtId,
    site: &Site,
    pending: &PendingRecord,
    saved: &[RoleRecord],
    parked: ParkKind,
) -> Result<ActIx, ContinuationRefusal> {
    let (stream, _, _) = exe.statement(stmt);
    let depth = site.path.len();
    let at = |suffix: &[u32]| {
        Site::new(
            site.unit.clone(),
            site.path.iter().chain(suffix).copied().collect::<Vec<_>>(),
        )
    };
    let action = at(&pending.site);
    // The entry must be the statement's own: an action in a closure
    // written inside it belongs to the closure's statement.
    let Some((act, entry)) = stream
        .nested_at
        .get(&action)
        .map(|act| (*act, &stream.nested[act.ix()]))
        .filter(|(_, entry)| entry.depth as usize == depth)
    else {
        return Err(ContinuationRefusal::UnknownSite { site: action });
    };
    let kind = matches!(
        (pending.phase, &entry.kind),
        (
            PhaseRecord::Called,
            NestedKind::Call(_) | NestedKind::Body(_)
        ) | (
            PhaseRecord::Waiting,
            NestedKind::Perform { .. } | NestedKind::Sleep { .. }
        ) | (PhaseRecord::Spawned, NestedKind::Spawn(_))
            | (
                PhaseRecord::Joining,
                NestedKind::Join | NestedKind::JoinMany(_)
            )
    );
    if !kind || parked == ParkKind::Fresh {
        return Err(ContinuationRefusal::IllegalPhase {
            site: action,
            phase: phase_name(pending.phase),
        });
    }
    let agrees = match pending.phase {
        PhaseRecord::Called => parked == ParkKind::Returned,
        PhaseRecord::Waiting => match parked {
            ParkKind::Requested | ParkKind::Raised => true,
            // Only a `then yield` wait owes a turn once delivered.
            ParkKind::Delivered => entry.kind.then_yield(),
            _ => false,
        },
        PhaseRecord::Spawned => {
            matches!(parked, ParkKind::Valued { handle: true } | ParkKind::Raised)
        }
        PhaseRecord::Joining => matches!(
            parked,
            ParkKind::Joined | ParkKind::Valued { .. } | ParkKind::Raised
        ),
    };
    if !agrees {
        return Err(ContinuationRefusal::TaskState { site: action });
    }
    let mut sites = BTreeSet::new();
    if let Some(twice) = saved
        .iter()
        .find(|role| !sites.insert(role.site.as_slice()))
    {
        return Err(ContinuationRefusal::ExtraRole {
            site: at(&twice.site),
        });
    }
    let saved = sites;
    let expected: BTreeSet<&[u32]> = entry
        .roles
        .iter()
        .map(|(role, _)| &role.path[depth..])
        .collect();
    if let Some(extra) = saved.iter().find(|role| !expected.contains(*role)) {
        return Err(ContinuationRefusal::ExtraRole { site: at(extra) });
    }
    if let Some((missing, _)) = entry
        .roles
        .iter()
        .find(|(role, _)| !saved.contains(&role.path[depth..]))
    {
        return Err(ContinuationRefusal::MissingRole {
            site: missing.clone(),
        });
    }
    Ok(act)
}

#[cfg(test)]
mod laws {
    use crate::laws::embedder::Embedder;

    /// FIG-6470: scalar call results have no payload
    /// to canonicalize; repeated calls must not clone them through the heap.
    #[test]
    fn scalar_call_bindings_do_not_canonicalize_payloads() {
        let mut source = String::from("fn echo(x) { return x } main {");
        for index in 0..64 {
            let value = ["null", "absent", "true", "1.5"][index % 4];
            source.push_str(&format!("let v{index} = call echo({value}) "));
        }
        source.push_str("return v63 }");
        let mut embedder = Embedder::new(&source);
        let before = embedder.machine.storage.binding_canonicalizations;
        use crate::Machine;
        assert!(matches!(
            Machine::run(&mut embedder.machine, &mut embedder.world, u64::MAX).unwrap(),
            crate::Step::Ended(crate::End::Finished(_))
        ));
        assert_eq!(
            embedder.machine.storage.binding_canonicalizations - before,
            0,
            "scalar bindings must bypass payload canonicalization"
        );
    }

    /// FIG-6502: a value is a small handle, so a guest call moves few bytes.
    /// Each call of `echo` moves its argument through registers into the
    /// callee's slot, reads it back to return it, and binds the result in
    /// the caller: seven value moves. At 56 bytes a value they stay within
    /// 448 bytes a call; the 144-byte layout, with an inline 80-byte page
    /// reference in every payload, moved 1,008.
    #[test]
    fn a_guest_call_moves_small_values() {
        const CALLS: usize = 64;
        let mut source = String::from("fn echo(x) { return x } main {");
        for index in 0..CALLS {
            source.push_str(&format!("let v{index} = call echo(\"text {index}\") "));
        }
        source.push_str("return v63 }");
        let mut embedder = Embedder::new(&source);
        let before = embedder.machine.storage.value_moves;
        use crate::Machine;
        assert!(matches!(
            Machine::run(&mut embedder.machine, &mut embedder.world, u64::MAX).unwrap(),
            crate::Step::Ended(crate::End::Finished(_))
        ));
        let moves = (embedder.machine.storage.value_moves - before) / CALLS;
        let bytes = moves * std::mem::size_of::<crate::machine::Value>();
        assert!(
            bytes <= 448,
            "a call moves {moves} values of {} bytes: {bytes} bytes",
            std::mem::size_of::<crate::machine::Value>()
        );
    }

    /// FIG-6480, K-CHG-003: measuring a call's register arguments or its
    /// result borrows each value at its holding location, without a Value copy.
    #[test]
    fn charge_formulas_borrow_register_arguments_and_results() {
        use crate::compile::Plan;
        use crate::machine::Value;
        use lash_kernel_doc::{Formula, Name, Operand, Param, Type};
        let embedder = Embedder::new("main { return null }");
        let args = [Value::text("hello")];
        let result = Value::Null;
        let param = Operand::Param(Name::new("a"));
        let plan = Plan::new(
            &Formula::Sum(vec![
                Formula::Size(param.clone()),
                Formula::DeepSize(param.clone()),
                Formula::Magnitude(param),
                Formula::DeepSize(Operand::Result),
            ]),
            &[Param {
                name: Name::new("a"),
                ty: Type::Any,
                optional: false,
            }],
        );
        embedder
            .machine
            .storage
            .formula_sources
            .borrow_mut()
            .clear();
        assert_eq!(
            embedder
                .machine
                .formula(&plan, &args, Some(&result))
                .unwrap(),
            13
        );
        let sources = embedder.machine.storage.formula_sources.borrow();
        assert_eq!(sources.len(), 4);
        let copied = sources
            .iter()
            .filter(|&&source| {
                source != std::ptr::from_ref(&args[0]).addr()
                    && source != std::ptr::from_ref(&result).addr()
            })
            .count();
        assert_eq!(
            copied * std::mem::size_of::<Value>(),
            0,
            "formula copied {copied} values"
        );
    }
}
