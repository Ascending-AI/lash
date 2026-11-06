//! Only active source loop counts survive; placement is rebuilt once at exit.
use super::super::*;
use super::{Running, scalar::ScalarAnswer};
use crate::compile::region_isa::{LoopPhase, Pc, RegionOp};
use crate::compile::{BlockId, Form};
use crate::machine::loops::{Cursor, Header};
use crate::machine::{Control, TryPhase, controls::Controls};
use lash_kernel_state::kernel_internals::logical::Slot;
use lash_kernel_state::paged::TableCursor;

#[derive(Clone, Copy, Default)]
pub(super) struct LoopState {
    pub(super) active: bool,
    pub(super) started: u64,
    /// A plain `for` over a map or a set: the sequence of the entry it
    /// last took.
    pub(super) last: Option<u64>,
    /// A protocol `for`'s own cursor over a list, a tuple or a text: how
    /// many items it has stepped.
    pub(super) position: u64,
    /// A protocol `for`'s guard over a map or a set it checks.
    pub(super) guard: Option<super::call_bind::GuardArguments>,
}
#[derive(Clone, Copy)]
pub(super) struct Edit {
    index: usize,
    next: LoopState,
}

pub(super) fn bind(
    program: &RegionCode,
    canonical: &crate::machine::Frame,
) -> Result<Box<[LoopState]>, RegionError> {
    let mut states = Vec::new();
    while let Some(info) = program.loop_info(states.len() as u32) {
        let mut state = LoopState::default();
        if let Some(found) = canonical.control.iter().find_map(|control| match control {
            Control::While { stmt, started } if *stmt == info.statement => Some(LoopState {
                active: true,
                started: *started,
                ..LoopState::default()
            }),
            Control::For {
                stmt,
                started,
                cursor,
                header,
                guard,
            } if *stmt == info.statement => Some(LoopState {
                active: true,
                started: *started,
                last: match cursor {
                    Cursor::Map(_, last) | Cursor::Set(_, last) => last.map(|last| last.0),
                    _ => None,
                },
                position: match cursor {
                    Cursor::List(_, position) | Cursor::Tuple(_, position) => *position as u64,
                    Cursor::Text(text) => text.position(),
                    _ => 0,
                },
                // A guarded loop's `check` runs with an item held, or at
                // exhaustion.
                guard: guard
                    .as_deref()
                    .map(|guard| super::call_bind::GuardArguments {
                        size: guard.size,
                        remaining: guard.remaining,
                        more: !matches!(header, Header::Exhausted),
                    }),
            }),
            _ => None,
        }) {
            state = found;
        }
        states
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        states.push(state);
    }
    Ok(states.into())
}

pub(super) fn branch(program: &RegionCode, pc: Pc) -> Option<u32> {
    if !matches!(program.op(pc), Some(RegionOp::Branch { .. })) {
        return None;
    }
    let step = program.step(pc)?;
    let mut index = 0;
    while let Some(info) = program.loop_info(index) {
        if info.while_loop
            && step.at.site == info.site
            && step.at.phase == crate::compile::room::Phase::Test
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

impl Running<'_> {
    pub(super) fn prepare_control(
        &self,
        op: &RegionOp,
        answer: &ScalarAnswer,
        branch: Option<u32>,
        looping: Option<&crate::compile::region_isa::LoopDeparture>,
        taken: Option<u64>,
        staged: Option<(u32, LoopState)>,
    ) -> Result<Option<Edit>, RegionError> {
        // A protocol `for`'s header row gives the loop's next state.
        if let Some((index, next)) = staged {
            return Ok(Some(Edit {
                index: self.loop_index(index)?,
                next,
            }));
        }
        // A Break's Catch leaves its loop; a Continue's keeps it active.
        if let Some(crate::compile::region_isa::LoopDeparture::End { exits: Some(index) }) = looping
        {
            let mut next = *self
                .loops
                .get(self.loop_index(*index)?)
                .ok_or_else(|| invalid(Refused::Facts))?;
            next.active = false;
            return Ok(Some(Edit {
                index: self.loop_index(*index)?,
                next,
            }));
        }
        let (index, next) = match op {
            RegionOp::Checked { kernel, .. } => match kernel.kind() {
                crate::compile::region_isa::CheckedKind::LoopControl { stage, .. } => {
                    let current = *self
                        .loops
                        .get(self.loop_index(stage.info)?)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    let next = match stage.phase {
                        LoopPhase::Enter => LoopState {
                            active: true,
                            ..LoopState::default()
                        },
                        LoopPhase::Test if current.active => current,
                        // A protocol `for` enters before `open` gives what
                        // it walks; its Opened row starts the walk.
                        _ => return Err(invalid(Refused::Facts)),
                    };
                    (stage.info, next)
                }
                // A plain `for` starts a pass per item taken; exhausted, it
                // leaves its loop.
                crate::compile::region_isa::CheckedKind::Cursor { at, exhausted } => {
                    let index = self
                        .code
                        .program
                        .loop_at(at)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    let mut next = *self
                        .loops
                        .get(self.loop_index(index)?)
                        .filter(|state| state.active)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    if answer.next == *exhausted {
                        next.active = false;
                    } else {
                        next.started = next
                            .started
                            .checked_add(1)
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        next.last = taken.or(next.last);
                    }
                    (index, next)
                }
                _ => return Ok(None),
            },
            RegionOp::Branch { yes, .. } if branch.is_some() => {
                let index = branch.ok_or_else(|| invalid(Refused::Facts))?;
                let mut next = *self
                    .loops
                    .get(self.loop_index(index)?)
                    .filter(|state| state.active)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if answer.next == *yes {
                    next.started = next
                        .started
                        .checked_add(1)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                } else {
                    next.active = false;
                }
                (index, next)
            }
            _ => return Ok(None),
        };
        Ok(Some(Edit {
            index: self.loop_index(index)?,
            next,
        }))
    }

    pub(super) fn accept_control(&mut self, edit: Option<Edit>) {
        if let Some(edit) = edit {
            self.loops[edit.index] = edit.next;
        }
    }

    pub(super) fn controls_for(
        &self,
        code: &super::Code,
        pc: Pc,
        frame: usize,
        roles: &[(Slot, Value)],
        suspended: &[Suspension],
    ) -> Result<Controls, RegionError> {
        controls_for(
            code,
            pc,
            &self.loops[frame * self.programs.loops..(frame + 1) * self.programs.loops],
            roles,
            suspended,
        )
    }
}

/// An exit the cleanup of the try at a site suspended, as the frame records
/// it.
pub(super) type Suspension = (lash_kernel_value::sites::Site, crate::machine::Completion);

/// The frame's controls at `pc`. A plain `for` walks the value its cursor
/// role holds in `roles`; a cleanup runs with the exit its try suspended,
/// innermost last in `suspended`.
pub(super) fn controls_for(
    code: &super::Code,
    pc: Pc,
    loops: &[LoopState],
    roles: &[(Slot, Value)],
    suspended: &[Suspension],
) -> Result<Controls, RegionError> {
    let point = &code
        .program
        .step(pc)
        .ok_or_else(|| invalid(Refused::Path))?
        .at;
    let mut values = Vec::new();
    let mut taken = 0;
    place(
        code,
        loops,
        roles,
        (suspended, &mut taken),
        code.exe.code(code.program.code()).body,
        point,
        &mut values,
    )?;
    // Each suspended exit stands at a cleanup the point is in, in order.
    if taken != suspended.len() {
        return Err(invalid(Refused::Facts));
    }
    Ok(values.into())
}

fn place(
    code: &super::Code,
    loops: &[LoopState],
    roles: &[(Slot, Value)],
    (suspended, taken): (&[Suspension], &mut usize),
    block: BlockId,
    point: &Point,
    values: &mut Vec<Control>,
) -> Result<(), RegionError> {
    use crate::compile::room::Phase;
    let source = code.exe.block(block);
    if point.site == source.site {
        if point.phase != Phase::Enter {
            values.push(Control::Block {
                block,
                next: source.stmts.len(),
            });
        }
        return Ok(());
    }
    let (index, id, statement) = source
        .stmts
        .iter()
        .enumerate()
        .find_map(|(index, id)| {
            let statement = code.exe.stmt(*id);
            (point.site.unit == statement.site.unit
                && point.site.path.starts_with(&statement.site.path))
            .then_some((index, *id, statement))
        })
        .ok_or_else(|| invalid(Refused::Path))?;
    let nested = statement.form.blocks().into_iter().find(|child| {
        let site = &code.exe.block(*child).site;
        point.site.unit == site.unit && point.site.path.starts_with(&site.path)
    });
    let active = loops.iter().enumerate().find_map(|(index, state)| {
        let info = code.program.loop_info(index as u32)?;
        (info.site == statement.site && state.active).then_some((info, state))
    });
    // A try stands open in its body, at its Catch and in its catch block.
    let catching = point.site == statement.site
        && point.phase == Phase::Catch
        && matches!(statement.form, Form::Try { .. });
    // A suspended exit stands at its try's Restore until it is restored,
    // or at its Supersede until a departure from its cleanup drops it.
    let restoring = point.site == statement.site
        && matches!(point.phase, Phase::Restore | Phase::Supersede)
        && matches!(
            statement.form,
            Form::Try {
                finally: Some(_),
                ..
            }
        )
        && suspended
            .get(*taken)
            .is_some_and(|(site, _)| *site == statement.site);
    values.push(Control::Block {
        block,
        next: index + usize::from(nested.is_some() || active.is_some() || catching || restoring),
    });
    match &statement.form {
        // A cleanup runs with the exit its try suspended, or after its body
        // or its catch block ended normally.
        Form::Try {
            finally: Some(cleanup),
            ..
        } if nested == Some(*cleanup) || restoring => {
            let completion = match suspended.get(*taken) {
                Some((site, completion)) if *site == statement.site => {
                    *taken += 1;
                    completion.clone()
                }
                _ => crate::machine::Completion::Normal,
            };
            values.push(Control::Try {
                stmt: id,
                phase: TryPhase::Finally(completion),
            });
        }
        Form::Try { body, catch, .. } if catching || nested.is_some() => {
            values.push(Control::Try {
                stmt: id,
                phase: if nested.is_some() && nested == catch.as_ref().map(|(_, block)| *block) {
                    TryPhase::Catch
                } else if catching || nested == Some(*body) {
                    TryPhase::Body
                } else {
                    return Err(invalid(Refused::Facts));
                },
            });
        }
        Form::While { body, .. } => {
            if let Some((info, state)) = active
                && (nested.is_none() || nested == Some(*body))
            {
                values.push(Control::While {
                    stmt: info.statement,
                    started: state.started,
                });
            }
        }
        Form::For {
            body, via: None, ..
        } => {
            if let Some((info, state)) = active
                && (nested.is_none() || nested == Some(*body))
            {
                let index = code
                    .program
                    .loop_at(&info.site)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let slot = code
                    .program
                    .loop_cursor_slot(index)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let at = usize::try_from(state.started).map_err(|_| invalid(Refused::Facts))?;
                let cursor = match roles.iter().find(|(held, _)| *held == slot) {
                    Some((_, Value::List(id))) => Cursor::List(*id, at),
                    Some((_, Value::Tuple(tuple))) => Cursor::Tuple(*tuple, at),
                    Some((_, Value::Map(id))) => Cursor::Map(*id, state.last.map(TableCursor)),
                    Some((_, Value::Set(id))) => Cursor::Set(*id, state.last.map(TableCursor)),
                    _ => return Err(invalid(Refused::Facts)),
                };
                values.push(Control::For {
                    stmt: info.statement,
                    cursor,
                    started: state.started,
                    header: Header::Body,
                    guard: None,
                });
            }
        }
        Form::For {
            body, via: Some(_), ..
        } => {
            if let Some((info, state)) = active
                && (nested.is_none() || nested == Some(*body))
            {
                values.push(header_control(
                    code,
                    info,
                    state,
                    roles,
                    (nested.is_none() && point.site == statement.site).then_some(point.phase),
                )?);
            }
        }
        Form::Try { .. } if nested.is_some() => {
            return Err(invalid(Refused::Facts));
        }
        _ => {}
    }
    if let Some(nested) = nested {
        place(
            code,
            loops,
            roles,
            (suspended, taken),
            nested,
            point,
            values,
        )?;
    }
    Ok(())
}

/// A protocol `for` as the frame records it (`K-ITER-005`): where its header
/// stands at `phase` of the loop's own statement, or in its body, and what
/// its cursor walks, which its cursor role holds once `open` has answered.
fn header_control(
    code: &super::Code,
    info: &crate::compile::region_isa::LoopInfo,
    state: &LoopState,
    roles: &[(Slot, Value)],
    phase: Option<crate::compile::room::Phase>,
) -> Result<Control, RegionError> {
    use crate::compile::room::Phase;
    use crate::machine::loops::{Guard, TextCursor};
    let index = code
        .program
        .loop_at(&info.site)
        .ok_or_else(|| invalid(Refused::Facts))?;
    let via = info.via.ok_or_else(|| invalid(Refused::Facts))?;
    let (walked, held) = code
        .program
        .loop_header_slots(index)
        .ok_or_else(|| invalid(Refused::Facts))?;
    let role = |slot: Slot| {
        roles
            .iter()
            .find_map(|(actual, value)| (*actual == slot).then_some(*value))
    };
    let item = || {
        role(held)
            .map(Box::new)
            .ok_or_else(|| invalid(Refused::Facts))
    };
    let header = match phase {
        Some(Phase::Open | Phase::Restore) => Header::Open,
        Some(Phase::Next | Phase::Fallthrough) => Header::Pull,
        Some(Phase::Check | Phase::Supersede) => match role(held) {
            Some(Value::Absent) => Header::Exhausted,
            Some(value) => Header::Check(Box::new(value)),
            None => return Err(invalid(Refused::Facts)),
        },
        Some(Phase::Item) => Header::Item(item()?),
        Some(Phase::Test) if via.item => Header::Item(item()?),
        _ => Header::Body,
    };
    if matches!(header, Header::Open) {
        return Ok(Control::For {
            stmt: info.statement,
            cursor: Cursor::Opening,
            started: state.started,
            header,
            guard: None,
        });
    }
    let position = usize::try_from(state.position).map_err(|_| invalid(Refused::Facts))?;
    let cursor = match role(walked).ok_or_else(|| invalid(Refused::Facts))? {
        Value::List(id) => Cursor::List(id, position),
        Value::Tuple(tuple) => Cursor::Tuple(tuple, position),
        Value::Map(id) => Cursor::Map(id, state.last.map(TableCursor)),
        Value::Set(id) => Cursor::Set(id, state.last.map(TableCursor)),
        text @ (Value::Text(_) | Value::LongText(_)) => {
            Cursor::Text(Box::new(TextCursor::new(text, state.position)))
        }
        driver if via.next => Cursor::Driver(Box::new(driver)),
        _ => return Err(invalid(Refused::Facts)),
    };
    Ok(Control::For {
        stmt: info.statement,
        cursor,
        started: state.started,
        header,
        guard: state.guard.map(|guard| {
            Box::new(Guard {
                size: guard.size,
                remaining: guard.remaining,
            })
        }),
    })
}
