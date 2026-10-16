//! Loops: a `for` over a collection or through a dialect's protocol, and a
//! `while` (`K-ITER`, `K-FORM-013`).
//!
//! A `for … via` calls its protocol's hooks at its header (`K-ITER-005`).
//! A hook with a native implementation answers at once. One that runs its
//! kernel body enters a frame above the loop's: the loop's header stands in
//! that hook, saved with the loop, and the hook's value carries the header
//! on when its frame returns ([`KernelMachine::header_answered`]).

use std::sync::Arc;

use lash_kernel_doc::heap::{Payload, TextRef, TupleRef};
use lash_kernel_doc::{EntityId, NativeHeap, TaskId, Value};
use lash_kernel_state::paged::{SemanticHeap, TableCursor};
use num_traits::ToPrimitive;

use super::ops::{Flow, Reg, Status};
use super::stream::{Ran, Running};
use super::{Control, Eval, Halt, KernelMachine, fault, raise};
use crate::compile::{Executable, Form, LibId, StmtId, Via};
use crate::interface::Host;

/// A `for`'s position in what it walks (`K-ITER-002`, `K-ITER-003`,
/// `K-ITER-005`).
#[derive(Clone, Debug)]
pub(super) enum Cursor {
    /// Not walking yet: the protocol's `open` runs.
    Opening,
    List(EntityId, usize),
    Tuple(Arc<TupleRef>, usize),
    /// A map or a set, and the sequence number of the entry last visited.
    Map(EntityId, Option<TableCursor>),
    Set(EntityId, Option<TableCursor>),
    Text(Box<TextCursor>),
    /// A value the protocol's `next` steps.
    Driver(Box<Value>),
}

/// A text, the index of its next code point and, for a text held in the
/// activation, that code point's byte offset.
#[derive(Clone, Debug)]
pub(super) struct TextCursor {
    pub(super) text: TextRef,
    pub(super) index: u64,
    pub(super) offset: usize,
}

impl Cursor {
    /// What the loop walks, which the frame holds.
    pub(super) fn walked(&self) -> Option<Value> {
        Some(match self {
            Self::Opening => return None,
            Self::List(id, _) => Value::List(*id),
            Self::Tuple(tuple, _) => Value::Tuple(Arc::clone(tuple)),
            Self::Map(id, _) => Value::Map(*id),
            Self::Set(id, _) => Value::Set(*id),
            Self::Text(cursor) => Value::Text(cursor.text.clone()),
            Self::Driver(driver) => (**driver).clone(),
        })
    }

    /// The loop's own cursor over `value`, `None` for a value only a
    /// protocol's `next` can step. Text is walked only through a protocol.
    pub(super) fn over(value: Value, text: bool) -> Result<Self, Box<Value>> {
        Ok(match value {
            Value::List(id) => Self::List(id, 0),
            Value::Tuple(tuple) => Self::Tuple(tuple, 0),
            Value::Map(id) => Self::Map(id, None),
            Value::Set(id) => Self::Set(id, None),
            Value::Text(text_value) if text => Self::Text(Box::new(TextCursor {
                text: text_value,
                index: 0,
                offset: 0,
            })),
            other => return Err(Box::new(other)),
        })
    }
}

/// Where a `for`'s header stands (`K-ITER-005`): in its body, or in the
/// protocol hook whose frame runs above the loop's.
#[derive(Clone, Debug)]
pub(super) enum Header {
    Body,
    /// `open` takes the iterated value to what the loop walks.
    Open,
    /// `next` steps the driver.
    Pull,
    /// `check` runs, the stepped item held.
    Check(Box<Value>),
    /// `check` runs at exhaustion.
    Exhausted,
    /// `item` takes the held item to the value bound.
    Item(Box<Value>),
}

impl Header {
    /// The stepped item the header holds while its hook runs.
    pub(super) fn held(&self) -> Option<Value> {
        match self {
            Self::Check(raw) | Self::Item(raw) => Some((**raw).clone()),
            _ => None,
        }
    }
}

/// A map's or a set's size when the loop opened it, and the items the
/// protocol's `check` still allows.
#[derive(Clone, Copy, Debug)]
pub(super) struct Guard {
    pub(super) size: u64,
    pub(super) remaining: u64,
}

/// The protocol of `for` statement `stmt`.
fn via_of(exe: &Executable, stmt: StmtId) -> Option<Via> {
    match &exe.stmt(stmt).form {
        Form::For { via, .. } => *via,
        _ => None,
    }
}

/// The code point of `text` at `index`, its byte offset `offset` when the
/// text is held in the activation; and the offset after it.
fn code_point(
    heap: &lash_kernel_state::paged::PagedHeap,
    text: &TextRef,
    index: u64,
    offset: usize,
) -> Result<(Value, usize), Halt> {
    match &text.body {
        Payload::Resident(body) => {
            let point = body
                .get(offset..)
                .and_then(|rest| rest.chars().next())
                .ok_or_else(|| fault("a text cursor stands off a code point"))?;
            let mut buffer = [0; 4];
            Ok((
                Value::text(&*point.encode_utf8(&mut buffer)),
                offset + point.len_utf8(),
            ))
        }
        Payload::Paged(_) => {
            let start = heap.text_offset(text, index)?;
            let end = heap.text_offset(text, index + 1)?;
            let mut point = String::new();
            heap.payload_visit(&Value::Text(text.clone()), start..end, &mut |chunk| {
                if let lash_kernel_doc::heap::Chunk::Text(chunk) = chunk {
                    point.push_str(chunk);
                }
                std::ops::ControlFlow::Continue(())
            })?;
            Ok((Value::text(point.as_str()), 0))
        }
    }
}

/// The count a protocol hook gives as `check`'s budget.
fn count(value: &Value) -> Option<u64> {
    match value {
        Value::Int(lash_kernel_doc::heap::IntRef::Inline(integer)) => integer.to_u64(),
        _ => None,
    }
}

impl KernelMachine {
    /// Starts a `for` over the value in `iterable`. Without a protocol it
    /// raises `type_error` unless that is a list, a tuple, a map or a set
    /// (`K-ITER-001`); with one, its `open` takes the value to what the
    /// loop walks (`K-ITER-005`). Then the loop takes its first step.
    pub(super) fn for_start(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: StmtId,
        iterable: Reg,
    ) -> Status {
        let value = self.take(iterable);
        let Some(via) = via_of(exe, stmt) else {
            let Ok(cursor) = Cursor::over(value, false) else {
                return raise(
                    "type_error",
                    "`for` iterates a list, a tuple, a map or a set",
                );
            };
            self.frame(task)?.control.push(Control::For {
                stmt,
                cursor,
                started: 0,
                header: Header::Body,
                guard: None,
            });
            return self.next_iteration(task, host, exe);
        };
        self.frame(task)?.control.push(Control::For {
            stmt,
            cursor: Cursor::Opening,
            started: 0,
            header: Header::Open,
            guard: None,
        });
        match self.hook(task, exe, stmt, via.open, vec![value])? {
            Some(opened) => self.opened(task, host, exe, opened),
            None => Ok(Flow::Step),
        }
    }

    /// Calls hook `lib` of the innermost loop, `for` statement `stmt`, with
    /// `args`: its value when it answers at once, `None` when its body's
    /// frame now runs above the loop's.
    fn hook(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        lib: LibId,
        args: Vec<Value>,
    ) -> Eval<Option<Value>> {
        let count = args.len();
        for (at, arg) in args.into_iter().enumerate() {
            self.put(at, arg);
        }
        match self.call_lib(task, exe, lib, 0, count, Some(stmt))? {
            Flow::Done => Ok(Some(self.take(0))),
            Flow::Step => Ok(None),
        }
    }

    /// The innermost loop's `for` statement and its protocol.
    fn innermost_for(&mut self, task: TaskId) -> Result<&mut Control, Halt> {
        match self.frame(task)?.control.last_mut() {
            Some(control @ Control::For { .. }) => Ok(control),
            _ => Err(fault("a `for` header stands in no `for` loop")),
        }
    }

    /// A protocol hook of the innermost loop has answered `value` from its
    /// frame: the header goes on from where it stood (`K-ITER-005`).
    pub(super) fn header_answered(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
    ) -> Status {
        let Control::For { header, .. } = self.innermost_for(task)? else {
            unreachable!("innermost_for gives a `for`");
        };
        match header.clone() {
            Header::Open => self.opened(task, host, exe, value),
            Header::Pull => {
                let raw = self.pulled(value)?;
                self.stepped(task, host, exe, raw)
            }
            Header::Check(_) | Header::Exhausted => self.checked(task, host, exe, value),
            Header::Item(_) => self.bind_pass(task, exe, value),
            Header::Body => Err(fault("a `for` in its body was answered as its header").into()),
        }
    }

    /// Whether the innermost control of the task's top frame is a `for`
    /// whose header waits in a hook.
    pub(super) fn in_header(&mut self, task: TaskId) -> Result<bool, Halt> {
        Ok(matches!(
            self.frame(task)?.control.last(),
            Some(Control::For { header, .. }) if !matches!(header, Header::Body)
        ))
    }

    /// `open` gave `value`, what the loop walks: a list, a tuple, a map, a
    /// set or a text by the loop's own cursor, anything else as a driver its
    /// `next` steps. A map or a set is guarded when the protocol checks.
    fn opened(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
    ) -> Status {
        let Control::For { stmt, .. } = self.innermost_for(task)? else {
            unreachable!("innermost_for gives a `for`");
        };
        let stmt = *stmt;
        let via = via_of(exe, stmt).ok_or_else(|| fault("an opened loop has no protocol"))?;
        let cursor = match Cursor::over(value, true) {
            Ok(cursor) => cursor,
            Err(driver) if via.next.is_some() => Cursor::Driver(driver),
            Err(_) => {
                return raise(
                    "type_error",
                    "the loop's protocol opened a value it has no `next` to step",
                );
            }
        };
        let guard = match (&cursor, via.check) {
            (Cursor::Map(id, _) | Cursor::Set(id, _), Some(_)) => {
                let size = self.heap.len(*id)?;
                Some(Box::new(Guard {
                    size,
                    remaining: size,
                }))
            }
            _ => None,
        };
        let Control::For {
            cursor: held,
            header,
            guard: guarded,
            ..
        } = self.innermost_for(task)?
        else {
            unreachable!("innermost_for gives a `for`");
        };
        *held = cursor;
        *header = Header::Body;
        *guarded = guard;
        self.next_iteration(task, host, exe)
    }

    /// The test of the innermost loop's continuation, charged once
    /// (`K-CHG-002`): a `for` steps its cursor, a `while` tests its
    /// condition. It starts the next iteration, `Step`, or ends the loop:
    /// into its `else` block, `Step`, or `Done` (`K-ITER-002`,
    /// `K-ITER-003`, `K-ITER-005`, `K-FORM-013`).
    pub(super) fn next_iteration(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Status {
        self.charge(self.costs.loop_test)?;
        // Borrowed by field, so that the loop's position can be read
        // against the heap.
        let Some(frame) = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("a running task is missing"))?
            .frames
            .last_mut()?
        else {
            return Err(fault("a running task has no frame").into());
        };
        match frame.control.last_mut() {
            Some(Control::For {
                stmt,
                cursor,
                header,
                ..
            }) => {
                let stmt = *stmt;
                let raw = match cursor {
                    Cursor::List(list, position) => {
                        let element = self.heap.list_get(*list, *position as u64)?;
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Tuple(items, position) => {
                        let element = self.heap.tuple_get(items, *position as u64)?;
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Map(id, last) | Cursor::Set(id, last) => {
                        let next = self.heap.table_after(*id, *last)?;
                        next.map(|(sequence, key)| {
                            *last = Some(sequence);
                            key
                        })
                    }
                    Cursor::Text(cursor) => {
                        if cursor.index < cursor.text.summary.chars {
                            let (point, after) =
                                code_point(&self.heap, &cursor.text, cursor.index, cursor.offset)?;
                            cursor.index += 1;
                            cursor.offset = after;
                            Some(point)
                        } else {
                            None
                        }
                    }
                    Cursor::Driver(driver) => {
                        let driver = (**driver).clone();
                        *header = Header::Pull;
                        let next = via_of(exe, stmt)
                            .and_then(|via| via.next)
                            .ok_or_else(|| fault("a driver's loop has no `next`"))?;
                        return match self.hook(task, exe, stmt, next, vec![driver])? {
                            Some(stepped) => {
                                let raw = self.pulled(stepped)?;
                                self.stepped(task, host, exe, raw)
                            }
                            None => Ok(Flow::Step),
                        };
                    }
                    Cursor::Opening => {
                        return Err(fault("a loop stepped before it was opened").into());
                    }
                };
                self.stepped(task, host, exe, raw)
            }
            Some(Control::While { stmt, .. }) => {
                let running = Running::new(task, exe, *stmt);
                if !self.ran.is_empty()
                    && let Some(statement) = running.stmt.ordinal
                    && let Some(ordinal) = exe.condition_ordinals.get(&statement.get())
                {
                    self.ran[*ordinal as usize] += 1;
                }
                let Form::While { test, .. } = &running.stmt.form else {
                    return Err(fault("a `while` loop is not at a `while`").into());
                };
                match self.run(host, &running, test.start as usize)? {
                    Ran::Tested(value) => self.tested(task, value),
                    // The condition stands at a call made inside it.
                    Ran::Step => Ok(Flow::Step),
                    _ => Err(fault("a `while` condition gave no value").into()),
                }
            }
            _ => Err(fault("no loop is innermost").into()),
        }
    }

    /// What a driver's `next` gave: an empty tuple when it is exhausted,
    /// else a tuple of its one item.
    fn pulled(&mut self, stepped: Value) -> Eval<Option<Value>> {
        if let Value::Tuple(tuple) = &stepped {
            match tuple.summary.len {
                0 => return Ok(None),
                1 => return Ok(self.heap.tuple_get(tuple, 0)?),
                _ => {}
            }
        }
        raise(
            "type_error",
            "a loop protocol's `next` gives an empty tuple or a tuple of one item",
        )
    }

    /// The innermost `for` stepped to `raw`, or is exhausted: a guarded map
    /// or set is checked first, then the item is bound or the loop ends.
    fn stepped(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        raw: Option<Value>,
    ) -> Status {
        let Control::For {
            stmt,
            cursor,
            header,
            guard,
            ..
        } = self.innermost_for(task)?
        else {
            unreachable!("innermost_for gives a `for`");
        };
        let stmt = *stmt;
        let (Some(check), Some(Guard { size, remaining })) = (
            via_of(exe, stmt).and_then(|via| via.check),
            guard.as_deref().copied(),
        ) else {
            *header = Header::Body;
            return self.bind_item(task, exe, raw);
        };
        let walked = cursor
            .walked()
            .ok_or_else(|| fault("a guarded loop walks nothing"))?;
        let more = raw.is_some();
        *header = match raw {
            Some(raw) => Header::Check(Box::new(raw)),
            None => Header::Exhausted,
        };
        let args = vec![
            walked,
            Value::Int(size.into()),
            Value::Int(remaining.into()),
            Value::Bool(more),
        ];
        match self.hook(task, exe, stmt, check, args)? {
            Some(budget) => self.checked(task, host, exe, budget),
            None => Ok(Flow::Step),
        }
    }

    /// `check` gave `budget`, the items the loop may still give: the held
    /// item, if any, is bound, or the loop ends.
    fn checked(
        &mut self,
        task: TaskId,
        _host: &mut dyn Host,
        exe: &Executable,
        budget: Value,
    ) -> Status {
        let Some(budget) = count(&budget) else {
            return raise(
                "type_error",
                "a loop protocol's `check` gives a count of the items still allowed",
            );
        };
        let Control::For { header, guard, .. } = self.innermost_for(task)? else {
            unreachable!("innermost_for gives a `for`");
        };
        let raw = match std::mem::replace(header, Header::Body) {
            Header::Check(raw) => Some(*raw),
            Header::Exhausted => None,
            _ => return Err(fault("a `check` answered a loop not checking").into()),
        };
        if let Some(guard) = guard {
            guard.remaining = budget;
        }
        self.bind_item(task, exe, raw)
    }

    /// Binds the item the innermost `for` stepped to, through its
    /// protocol's `item` when it has one; or, exhausted, ends the loop.
    fn bind_item(&mut self, task: TaskId, exe: &Executable, raw: Option<Value>) -> Status {
        let Control::For {
            stmt,
            cursor,
            header,
            ..
        } = self.innermost_for(task)?
        else {
            unreachable!("innermost_for gives a `for`");
        };
        let stmt = *stmt;
        let Some(raw) = raw else {
            return self.ended(task, exe, stmt);
        };
        let Some(item) = via_of(exe, stmt).and_then(|via| via.item) else {
            return self.bind_pass(task, exe, raw);
        };
        let walked = cursor
            .walked()
            .ok_or_else(|| fault("an opened loop walks nothing"))?;
        *header = Header::Item(Box::new(raw.clone()));
        match self.hook(task, exe, stmt, item, vec![walked, raw])? {
            Some(element) => self.bind_pass(task, exe, element),
            None => Ok(Flow::Step),
        }
    }

    /// Starts the innermost `for`'s next pass with `element` bound.
    fn bind_pass(&mut self, task: TaskId, exe: &Executable, element: Value) -> Status {
        let Control::For {
            stmt,
            header,
            started,
            ..
        } = self.innermost_for(task)?
        else {
            unreachable!("innermost_for gives a `for`");
        };
        let Form::For { binding, body, .. } = &exe.stmt(*stmt).form else {
            return Err(fault("a `for` loop is not at a `for`").into());
        };
        *header = Header::Body;
        *started += 1;
        self.push_block(task, *body)?;
        self.bind(task, binding.at, binding.shared, element)?;
        Ok(Flow::Step)
    }

    /// The innermost loop, statement `stmt`, has ended but by `break`: its
    /// `else` block runs, `Step`, or there is none, `Done`.
    fn ended(&mut self, task: TaskId, exe: &Executable, stmt: StmtId) -> Status {
        self.frame(task)?.control.pop();
        let else_block = match &exe.stmt(stmt).form {
            Form::For { else_block, .. } | Form::While { else_block, .. } => *else_block,
            _ => None,
        };
        match else_block {
            Some(block) => {
                self.push_block(task, block)?;
                Ok(Flow::Step)
            }
            None => Ok(Flow::Done),
        }
    }

    /// The innermost `while` loop's condition gave `value`: its next
    /// iteration starts, `Step`, or it ends (`K-FORM-013`).
    pub(super) fn tested(&mut self, task: TaskId, value: bool) -> Status {
        let frame = self.frame(task)?;
        let Some(Control::While { stmt, started }) = frame.control.last_mut() else {
            return Err(fault("a `while` condition stands in no `while` loop").into());
        };
        let exe = Arc::clone(&frame.exe);
        let stmt = *stmt;
        let Form::While { body, .. } = &exe.stmt(stmt).form else {
            return Err(fault("a `while` loop is not at a `while`").into());
        };
        match value {
            true => {
                *started += 1;
                self.push_block(task, *body)?;
                Ok(Flow::Step)
            }
            false => self.ended(task, &exe, stmt),
        }
    }
}
