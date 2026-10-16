//! Derived frame coordinates beside the canonical paged frame stack.
//! Only the top frame is decoded for execution. Bindings stay in their
//! declaring-site tree; syncing a frame updates bounded control records.
use super::{Completion, Control, Cursor, Frame, LibraryCall, TryPhase};
use crate::compile::{Executable, Form};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{Site, Unit, Value};
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{
    EnteredRecord, FinallyRecord, FrameRecord, IteratedRecord, LoopRecord, PendingRecord,
    PhaseRecord,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

fn malformed(problem: impl Into<String>) -> HeapFault {
    HeapFault::Malformed(problem.into())
}

/// How the task that owns a frame stack comes to this decode: what kind
/// of resume a saved continuation may claim (`K-EVAL-006`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ParkKind {
    /// No resume happened: the frame rows stand where they were first
    /// pushed or last published. A saved continuation is impossible.
    Fresh,
    /// The task waits or resumed from a wait; `request` is `true` when
    /// the wait is a request (its outcome or a raise may be delivered).
    Woken { request: bool },
    /// The frame below was ended by a call's return.
    Returned,
}

/// Why a saved continuation record is refused on restore: the grammar
/// the durable resume state must satisfy.
pub(super) enum ContinuationRefusal {
    /// The record names an action site the statement does not have.
    UnknownSite { site: Site },
    /// Its phase cannot stand beside this resume.
    IllegalPhase { site: Site, phase: PhaseRecord },
    /// A role the resume entry expects is missing.
    MissingRole { site: Site },
    /// The record holds a role the layout does not expect.
    ExtraRole { site: Site },
    /// The task's state does not match a waiting action's.
    TaskState { site: Site },
}

impl fmt::Display for ContinuationRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSite { site } => write!(out, "unknown site {site}"),
            Self::IllegalPhase { site, phase } => {
                write!(out, "illegal phase {phase:?} at {site}")
            }
            Self::MissingRole { site } => write!(out, "missing role {site}"),
            Self::ExtraRole { site } => write!(out, "extra role {site}"),
            Self::TaskState { site } => write!(out, "task state refuses the wait at {site}"),
        }
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
        if coordinates && let Some(Control::Block { block, next }) = self.control.last() {
            let index = if self.awaiting.is_some() {
                next.checked_sub(1)
                    .ok_or_else(|| malformed("a frame awaits a statement it has not issued"))?
            } else {
                *next
            };
            let statement =
                self.exe.block(*block).site.child(
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
                } => {
                    let Form::For { site, .. } = &self.exe.stmt(*stmt).form else {
                        return Err(malformed("a for cursor stands at another form"));
                    };
                    let (value, position) = match cursor {
                        Cursor::List(id, at) => (Value::List(*id), *at as u64),
                        Cursor::Tuple(tuple, at) => (Value::Tuple(tuple.clone()), *at as u64),
                        Cursor::Table(value, at) => {
                            (value.clone(), at.map(|at| at.0).unwrap_or(u64::MAX))
                        }
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
                                position: Some(position),
                            },
                            &self.access,
                        )?;
                    }
                    write(
                        &mut self.record.iterated,
                        iterated,
                        IteratedRecord(value),
                        &self.access,
                    )?;
                    loops += 1;
                    iterated += 1;
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
                                position: None,
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
                                value.clone(),
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
    cache: BTreeMap<u64, Frame>,
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
            rows,
            cache: BTreeMap::new(),
            access,
            exe,
            parked,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len() as usize
    }
    pub(super) fn is_empty(&self) -> bool {
        self.rows.len() == 0
    }
    pub(super) fn last_mut(&mut self) -> Result<Option<&mut Frame>, HeapFault> {
        if self.is_empty() {
            return Ok(None);
        }
        let index = self.rows.len() - 1;
        if !self.cache.contains_key(&index) {
            let record = row(&self.rows, index, &self.access)?;
            let frame = restore(
                record,
                self.parked,
                self.access.clone(),
                Arc::clone(&self.exe),
            )?;
            self.cache.insert(index, frame);
        }
        Ok(self.cache.get_mut(&index))
    }
    pub(super) fn push(&mut self, mut frame: Frame) -> Result<(), HeapFault> {
        self.flush()?;
        // Inline calls run inside their caller's expression. Its live
        // cursor cannot be restored from a statement boundary until that
        // expression completes, so retain its derived frame while pinned.
        if !frame.inline && !self.cache.values().any(|frame| frame.inline) {
            self.cache.clear();
        }
        frame.sync()?;
        let index = self.rows.len();
        self.rows
            .write(ordinal_key(index), frame.record.clone(), &self.access)?;
        frame.published = Some(frame.record.clone());
        self.cache.insert(index, frame);
        self.parked = ParkKind::Fresh;
        Ok(())
    }
    pub(super) fn pop(&mut self) -> Result<Option<Frame>, HeapFault> {
        if self.is_empty() {
            return Ok(None);
        }
        self.last_mut()?;
        let index = self.rows.len() - 1;
        let frame = self.cache.remove(&index);
        self.rows.remove(&ordinal_key(index), &self.access)?;
        self.parked = ParkKind::Returned;
        Ok(frame)
    }
    pub(super) fn clear(&mut self) {
        self.rows = Tree::new(true);
        self.cache.clear();
        self.parked = ParkKind::Fresh;
    }
    pub(super) fn flush(&mut self) -> Result<(), HeapFault> {
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
    pub(super) fn pinned(&self) -> bool {
        self.cache.values().any(|frame| frame.inline)
    }
    /// Applies `edit` to the pending-continuation slot of the frame row
    /// at `index`, in the decoded frame when it is cached, else in the
    /// row: what a law's grammar tamper reaches. `false` when the frame
    /// has no row.
    #[cfg(test)]
    pub(super) fn pending_mut(
        &mut self,
        index: u64,
        edit: &mut dyn FnMut(&mut Option<PendingRecord>, &Access<'_>),
    ) -> Result<bool, HeapFault> {
        let access = self.access.clone();
        if self.cache.contains_key(&index) {
            self.last_mut()?;
            if let Some(frame) = self.cache.get_mut(&index) {
                edit(&mut frame.record.pending, &access);
                return Ok(true);
            }
        }
        let row = ordinal_key(index);
        let Some(mut record) = self.rows.get(&row, &access)? else {
            return Ok(false);
        };
        edit(&mut record.pending, &access);
        self.rows.write(row, record, &access)?;
        Ok(true)
    }

    /// The frames the cache holds, by their index in the stack.
    pub(super) fn cached(&self) -> Vec<(usize, &Frame)> {
        self.cache
            .iter()
            .map(|(index, frame)| (*index as usize, frame))
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
) -> Result<Frame, HeapFault> {
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
                nested = Some(validate_continuation(
                    &exe, pending, &site, stmt, parked, &access,
                )?);
                awaiting = Some(stmt);
                index + 1
            } else if !matches!(parked, ParkKind::Fresh) {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                if !matches!(exe.stmt(stmt).form, Form::Action(_)) {
                    return Err(malformed("a resuming frame has no action"));
                }
                awaiting = Some(stmt);
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
                if !saved.site.matches(site, &access)? {
                    return Err(malformed("a saved loop belongs to another site"));
                }
                let value = row(&record.iterated, iterated, &access)?.0;
                iterated += 1;
                let position = saved
                    .position
                    .ok_or_else(|| malformed("a for has no position"))?;
                let cursor = match value {
                    Value::List(id) => Cursor::List(
                        id,
                        usize::try_from(position)
                            .map_err(|_| malformed("list cursor overflows"))?,
                    ),
                    Value::Tuple(tuple) if position <= tuple.summary.len => {
                        Cursor::Tuple(tuple, position as usize)
                    }
                    value @ (Value::Map(_) | Value::Set(_)) => Cursor::Table(
                        value,
                        (position != u64::MAX)
                            .then_some(lash_kernel_state::paged::TableCursor(position)),
                    ),
                    _ => return Err(malformed("a for cursor has another kind or extent")),
                };
                control.push(Control::For {
                    stmt,
                    cursor,
                    started: saved.started,
                });
                *body
            }
            Form::While { site, body, .. } if at(*body) => {
                let saved = row(&record.loops, loops, &access)?;
                loops += 1;
                if !saved.site.matches(site, &access)? || saved.position.is_some() {
                    return Err(malformed("a saved while has another site or a position"));
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
                        EnteredRecord::Throw => Completion::Throw(value()?),
                        EnteredRecord::Return => Completion::Return(value()?),
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
    Ok(Frame {
        code: code_id,
        published: Some(record.clone()),
        record,
        access,
        exe,
        control,
        awaiting,
        nested,
        library,
        inline: false,
    })
}

/// Whether `pending`, the continuation a frame's record saved where it
/// parked at an action nested in statement `stmt`'s expression, is one
/// the durable grammar admits beside `parked`: its site names one of the
/// statement's nested actions, its phase can stand beside the resume,
/// and its roles are exactly the resume entry's. Gives the action's
/// index in the statement's continuation table. Sites in the record are
/// the path suffix under `stmt`'s site.
fn validate_continuation(
    exe: &Executable,
    pending: &PendingRecord,
    site: &Site,
    stmt: crate::compile::StmtId,
    parked: ParkKind,
    access: &Access<'_>,
) -> Result<u32, HeapFault> {
    let refused = |refusal| Err(malformed(format!("continuation refused: {refusal}")));
    let (stream, stmt_row, _) = exe.statement(stmt);
    let prefix = site.path.len();
    let site_of = |suffix: &[u32]| {
        Site::new(
            site.unit.clone(),
            site.path
                .iter()
                .copied()
                .chain(suffix.iter().copied())
                .collect::<Vec<_>>(),
        )
    };
    let Some(act) = stmt_row.nested(stream).find(|act| {
        let entry = &stream.nested[act.ix()];
        entry.site.path[prefix..] == pending.site[..]
    }) else {
        return refused(ContinuationRefusal::UnknownSite {
            site: site_of(&pending.site),
        });
    };
    let entry = &stream.nested[act.ix()];
    let phase_kind = matches!(
        (pending.phase, &entry.kind),
        (PhaseRecord::Called, crate::compile::NestedKind::Call(_))
            | (
                PhaseRecord::Waiting,
                crate::compile::NestedKind::Perform { .. } | crate::compile::NestedKind::Sleep
            )
    );
    let phase_parked = matches!(
        (pending.phase, parked),
        (PhaseRecord::Called, ParkKind::Returned) | (PhaseRecord::Waiting, ParkKind::Woken { .. })
    );
    if matches!(parked, ParkKind::Fresh) || !phase_kind || !phase_parked {
        return refused(ContinuationRefusal::IllegalPhase {
            site: site_of(&pending.site),
            phase: pending.phase,
        });
    }
    if matches!(pending.phase, PhaseRecord::Waiting)
        && let ParkKind::Woken { request: false } = parked
    {
        return refused(ContinuationRefusal::TaskState {
            site: site_of(&pending.site),
        });
    }
    // One pass over the saved roles, then set comparisons by site.
    let mut saved = std::collections::BTreeSet::new();
    let mut key = None;
    while let Some((next, role)) = pending.roles.after(key.as_deref(), access)? {
        saved.insert(role.site);
        key = Some(next);
    }
    let expected: std::collections::BTreeSet<&[u32]> = entry
        .roles
        .iter()
        .map(|(site, _)| &site.path[prefix..])
        .collect();
    if let Some(extra) = saved
        .iter()
        .find(|site| !expected.contains(site.as_slice()))
    {
        return refused(ContinuationRefusal::ExtraRole {
            site: site_of(extra),
        });
    }
    if let Some((missing, _)) = entry
        .roles
        .iter()
        .find(|(site, _)| !saved.contains(&site.path[prefix..]))
    {
        return refused(ContinuationRefusal::MissingRole {
            site: missing.clone(),
        });
    }
    Ok(act.0)
}
