//! The runtime compiled library code calls (spike, FIG-5848).
//!
//! Every function here does what the interpreter does for one operation,
//! through the machine's own methods where it can: the same values, the
//! same charges, the same reservations, pins and raises in the same order.
//! Compiled code holds an operation's values in a temporary array, by
//! index, as the interpreter holds them on its own stack: none of them is
//! a root, as none of the interpreter's is.
#![expect(
    unsafe_code,
    reason = "spike: compiled code calls these functions with the context it was entered with"
)]

use std::mem::{offset_of, replace};

use lash_kernel_doc::{TaskId, Value};

use super::{
    Completion, Control, Cursor, Eval, Interrupt, KernelMachine, SlotState, TryPhase, bound, fault,
    raise,
};
use crate::compile::{BlockId, Executable, Expr, LibId, Local, Stmt, StmtId, Var};
use crate::heap::{Heap, MAX_VALUE_DEPTH, Obj, value_bytes, within_depth};
use crate::interface::{Bound, Host};
use crate::jit::{CompiledCode, EXIT_DEOPT, EXIT_OUTCOME, EXIT_STEP, Level};

/// What the machine keeps for compiled code between entries.
#[derive(Debug, Default)]
pub(crate) struct JitState {
    /// Where the current call of `run` began, and its slice.
    pub(crate) before: u64,
    pub(crate) slice: u64,
    temps: Vec<Value>,
    scratch: Vec<Control>,
    /// When set, every entry is counted by its code and how it left.
    pub(crate) counting: bool,
    pub(crate) counts: std::collections::BTreeMap<String, [u64; 4]>,
}

struct HostCell<'h>(&'h mut dyn Host);

/// The context compiled code is entered with. The first fields are read by
/// the compiled code at fixed offsets.
#[repr(C)]
pub(crate) struct Ctx {
    charged: *mut u64,
    bound: u64,
    before: u64,
    slice: u64,
    loop_test: u64,
    entry: u64,
    machine: *mut KernelMachine,
    exe: *const Executable,
    host: *mut HostCell<'static>,
    task: TaskId,
    /// The compiled code's frame: an action may push a frame above it.
    frame: usize,
    code: *const CompiledCode,
    temps: Vec<Value>,
    outcome: Option<Eval<Completion>>,
}

pub(crate) const OFFSET_CHARGED: i32 = offset_of!(Ctx, charged) as i32;
pub(crate) const OFFSET_BOUND: i32 = offset_of!(Ctx, bound) as i32;
pub(crate) const OFFSET_BEFORE: i32 = offset_of!(Ctx, before) as i32;
pub(crate) const OFFSET_SLICE: i32 = offset_of!(Ctx, slice) as i32;
pub(crate) const OFFSET_LOOP_TEST: i32 = offset_of!(Ctx, loop_test) as i32;
pub(crate) const OFFSET_ENTRY: i32 = offset_of!(Ctx, entry) as i32;

impl KernelMachine {
    /// Runs the compiled code of the task's top frame from the statement
    /// its control stack is at, when there is compiled code for it. `None`
    /// is a step the interpreter takes.
    pub(super) fn try_compiled(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Option<Eval<Completion>> {
        let jit = exe.jit()?;
        if self.inline_depth > 0 {
            return None;
        }
        let frames = &mut self.tasks.get_mut(task.0 as usize)?.frames;
        let frame_index = frames.len().checked_sub(1)?;
        let frame = frames.last_mut()?;
        if frame.awaiting.is_some() {
            return None;
        }
        let Some(Control::Block { block, next }) = frame.control.last() else {
            return None;
        };
        let compiled = jit.code(frame.code)?;
        let entry = compiled.entry_at(*block, *next)?;
        self.charging = true;
        frame
            .control
            .retain(|control| !matches!(control, Control::Block { .. }));
        // The temporaries are kept from entry to entry: a stale value is
        // overwritten before it is read, and none is a root.
        let mut temps = std::mem::take(&mut self.jit.temps);
        if temps.len() < compiled.temps as usize {
            temps.resize(compiled.temps as usize, Value::Null);
        }
        let mut cell = HostCell(host);
        let machine: *mut KernelMachine = self;
        let mut ctx = Ctx {
            // SAFETY: the machine outlives the call; compiled code writes
            // the charge only between runtime calls.
            charged: unsafe { &raw mut (*machine).charged },
            bound: self.bounds.charge,
            before: self.jit.before,
            slice: self.jit.slice,
            loop_test: self.costs.loop_test,
            entry: u64::from(entry),
            machine,
            exe,
            host: (&raw mut cell).cast::<HostCell<'static>>(),
            task,
            frame: frame_index,
            code: compiled,
            temps,
            outcome: None,
        };
        // SAFETY: the entry was compiled for this code and takes this
        // context.
        let exit = unsafe { (compiled.entry)((&raw mut ctx).cast::<u8>()) };
        if self.jit.counting {
            let row = self
                .jit
                .counts
                .entry(compiled.stats.name.clone())
                .or_default();
            row[0] += 1;
            if let Some(slot) = row.get_mut(exit as usize + 1) {
                *slot += 1;
            }
        }
        self.jit.temps = std::mem::take(&mut ctx.temps);
        match exit {
            EXIT_STEP => Some(Ok(Completion::Normal)),
            EXIT_DEOPT => None,
            EXIT_OUTCOME => Some(
                ctx.outcome
                    .take()
                    .unwrap_or_else(|| Err(fault("compiled code left with no outcome").into())),
            ),
            _ => Some(Err(fault("compiled code left with an unknown exit").into())),
        }
    }
}

struct Parts<'a> {
    ctx: &'a mut Ctx,
    machine: &'a mut KernelMachine,
    exe: &'a Executable,
    host: &'a mut dyn Host,
    task: TaskId,
}

/// # Safety
/// `ctx` is the context the compiled code was entered with.
unsafe fn parts<'a>(ctx: *mut Ctx) -> Parts<'a> {
    // SAFETY: the caller's contract; the context's pointers name the live
    // machine, executable and host of the entry.
    unsafe {
        let ctx = &mut *ctx;
        let machine = &mut *ctx.machine;
        let exe = &*ctx.exe;
        let host = &mut *(*ctx.host).0;
        let task = ctx.task;
        Parts {
            ctx,
            machine,
            exe,
            host,
            task,
        }
    }
}

impl Parts<'_> {
    fn take(&mut self, temp: u64) -> Value {
        replace(&mut self.ctx.temps[temp as usize], Value::Null)
    }

    fn put(&mut self, temp: u64, value: Value) {
        self.ctx.temps[temp as usize] = value;
    }

    /// Records an interrupt and gives the status that says so.
    fn fail(&mut self, interrupt: Interrupt) -> u32 {
        self.ctx.outcome = Some(Err(interrupt));
        1
    }

    fn status(&mut self, result: Eval<()>) -> u32 {
        match result {
            Ok(()) => 0,
            Err(interrupt) => self.fail(interrupt),
        }
    }

    /// The compiled code's own frame, which an action may have put a
    /// callee's frame above.
    fn frame(&mut self) -> &mut super::Frame {
        let task = self.task;
        let index = self.ctx.frame;
        &mut self.machine.tasks[task.0 as usize].frames[index]
    }
}

pub(crate) extern "C" fn rt_step(ctx: *mut Ctx) {
    // SAFETY: called by compiled code with its context.
    let parts = unsafe { parts(ctx) };
    parts.machine.pins.clear();
    parts.machine.fresh.clear();
}

pub(crate) extern "C" fn rt_charge_fail(ctx: *mut Ctx) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let limit = parts.machine.bounds.charge;
    parts.fail(bound(Bound::Charge, limit).into());
}

pub(crate) extern "C" fn rt_materialize(ctx: *mut Ctx, position: u64) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    // SAFETY: the code the context was entered for.
    let code = unsafe { &*parts.ctx.code };
    let levels = &code.positions[position as usize];
    let mut scratch = std::mem::take(&mut parts.machine.jit.scratch);
    scratch.clear();
    let frame = parts.frame();
    let mut kept = std::mem::take(&mut frame.control);
    {
        let mut keep = kept.drain(..);
        for level in levels.iter() {
            match level {
                Level::Block { block, next } => scratch.push(Control::Block {
                    block: *block,
                    next: *next as usize,
                }),
                Level::Keep => {
                    if let Some(control) = keep.next() {
                        scratch.push(control);
                    }
                }
            }
        }
    }
    frame.control = scratch;
    parts.machine.jit.scratch = kept;
}

pub(crate) extern "C" fn rt_lit(ctx: *mut Ctx, dst: u64, value: u64) {
    // SAFETY: called by compiled code with its context and a literal of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let value = unsafe { &*(value as usize as *const Value) }.clone();
    parts.put(dst, value);
}

pub(crate) extern "C" fn rt_var(ctx: *mut Ctx, dst: u64, var: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a variable of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let var = unsafe { &*(var as usize as *const Var) };
    match parts.machine.read_var(parts.task, parts.exe, var) {
        Ok(value) => {
            parts.put(dst, value);
            0
        }
        Err(interrupt) => parts.fail(interrupt),
    }
}

/// `read_member` for a field.
pub(crate) extern "C" fn rt_field(ctx: *mut Ctx, dst: u64, src: u64, field: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a field name of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let field = unsafe { &*(field as usize as *const String) };
    let target = parts.take(src);
    let value = match target {
        Value::Record(record) => parts
            .machine
            .heap
            .record(record)
            .and_then(|fields| fields.iter().find(|(name, _)| name == field))
            .map_or(Value::Absent, |(_, value)| value.clone()),
        Value::Error(error) => match field.as_str() {
            "kind" => Value::text(error.kind.as_str()),
            "message" => Value::text(error.message.as_str()),
            "data" => error.data.clone(),
            _ => Value::Absent,
        },
        _ => {
            let result: Eval<()> = raise("type_error", "only a record or an error has fields");
            return parts.status(result);
        }
    };
    parts.put(dst, value);
    0
}

/// `read_member` for an index.
pub(crate) extern "C" fn rt_index(ctx: *mut Ctx, dst: u64, target: u64, index: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let target = parts.take(target);
    let index = parts.take(index);
    let heap = &parts.machine.heap;
    let element = |items: &[Value]| match super::eval::position(&index)? {
        Some(position) if position < items.len() => Ok(items[position].clone()),
        _ => raise(
            "index_out_of_range",
            format!("no element there; the length is {}", items.len()),
        ),
    };
    let result: Eval<Value> = match &target {
        Value::Tuple(items) => element(items),
        Value::List(list) => element(heap.list(*list).map_or(&[], Vec::as_slice)),
        Value::Record(record) => match &index {
            Value::Text(field) => Ok(heap
                .record(*record)
                .and_then(|fields| fields.iter().find(|(name, _)| name == field.as_ref()))
                .map_or(Value::Absent, |(_, value)| value.clone())),
            _ => raise("type_error", "a record index must be text"),
        },
        Value::Map(map) => super::eval::key(&index).and_then(|key| {
            match heap.table(*map).and_then(|table| table.get(&key)) {
                Some(value) => Ok(value.clone()),
                None => raise("key_missing", "the map has no entry under that key"),
            }
        }),
        Value::Set(set) => super::eval::key(&index)
            .map(|key| Value::Bool(heap.table(*set).is_some_and(|table| table.contains(&key)))),
        _ => raise(
            "type_error",
            "only a list, a tuple, a record, a map or a set is read by index",
        ),
    };
    match result {
        Ok(value) => {
            parts.put(dst, value);
            0
        }
        Err(interrupt) => parts.fail(interrupt),
    }
}

/// A call of a native or machine function inside an expression
/// (`call_expr`).
pub(crate) extern "C" fn rt_native(
    ctx: *mut Ctx,
    dst: u64,
    lib: u64,
    first: u64,
    count: u64,
) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let base = parts.machine.storage.args.len();
    for offset in 0..count {
        let value = parts.take(first + offset);
        parts.machine.storage.args.push(value);
    }
    let result = parts
        .machine
        .call_library(parts.task, parts.exe, LibId(lib as u32), base);
    parts.machine.storage.args.truncate(base);
    match result {
        Ok(Some(value)) => {
            parts.put(dst, value);
            0
        }
        Ok(None) => parts.fail(fault("a body ran as a native").into()),
        Err(interrupt) => parts.fail(interrupt),
    }
}

/// A native function's call whose arguments are exactly its parameters
/// and that states no guard: `call_library` without the argument stack,
/// the arity check and the guard. The native reads its arguments where the
/// compiled code left them; like the argument stack, they are no root.
pub(crate) extern "C" fn rt_native_direct(
    ctx: *mut Ctx,
    dst: u64,
    lib: u64,
    first: u64,
    count: u64,
) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let exe = parts.exe;
    let function = exe.lib(LibId(lib as u32));
    let crate::compile::LibRun::Native(native) = &function.run else {
        return parts.fail(fault("a direct native call of no native").into());
    };
    let range = first as usize..(first + count) as usize;
    let machine = &mut *parts.machine;
    let args = &parts.ctx.temps[range];
    let mut attempt = 0;
    let result = loop {
        let mut counter = lash_kernel_doc::WorkCounter::new(None);
        let mut view = crate::heap::NativeView {
            heap: &mut machine.heap,
            bound: machine.bounds.memory,
            reserved: 0,
        };
        let call = lash_kernel_doc::NativeCall {
            args,
            heap: &mut view,
            counter: &mut counter,
        };
        match native.call(call) {
            Ok(value) => {
                if !within_depth(&value, MAX_VALUE_DEPTH) {
                    break Err(super::error(
                        "too_deep",
                        format!("a value nests more than {MAX_VALUE_DEPTH} levels"),
                    ));
                }
                if let Err(halt) = machine.pin(&value) {
                    let halt = halt.in_function(&function.definition.name);
                    parts.ctx.outcome = Some(Err(halt.into()));
                    return 1;
                }
                break Ok(value);
            }
            Err(lash_kernel_doc::NativeError::Raised(raised)) => {
                break Err(Value::Error(std::sync::Arc::new(raised)));
            }
            Err(lash_kernel_doc::NativeError::Guard(guard)) => {
                let halt = bound(
                    Bound::Guard {
                        function: function.id,
                    },
                    guard.limit,
                )
                .in_function(&function.definition.name);
                parts.ctx.outcome = Some(Err(halt.into()));
                return 1;
            }
            Err(lash_kernel_doc::NativeError::Memory) if attempt == 0 => {
                attempt += 1;
                machine.collect();
            }
            Err(lash_kernel_doc::NativeError::Memory) => {
                let halt = bound(Bound::Memory, machine.bounds.memory)
                    .in_function(&function.definition.name);
                parts.ctx.outcome = Some(Err(halt.into()));
                return 1;
            }
        }
    };
    let units = machine.call_units(exe, LibId(lib as u32), args, result.as_ref().ok());
    if let Err(halt) = machine.charge_call(exe, LibId(lib as u32), units) {
        parts.ctx.outcome = Some(Err(halt.into()));
        return 1;
    }
    match result {
        Ok(value) => {
            parts.put(dst, value);
            0
        }
        Err(value) => parts.fail(Interrupt::Raise(value)),
    }
}

/// The kernel primitives the compiled tier runs in place (`prim`): their
/// result, charge and reservation computed here for the operand kinds the
/// fast path takes, which are what the native computes for them. Any other
/// operand takes the native.
pub(crate) const PRIM_KIND: u64 = 1;
pub(crate) const PRIM_SAME: u64 = 2;
pub(crate) const PRIM_LT: u64 = 3;
pub(crate) const PRIM_LE: u64 = 4;
pub(crate) const PRIM_NOT: u64 = 5;
pub(crate) const PRIM_LIST_LEN: u64 = 6;

fn kind_text(value: &Value) -> &'static str {
    use lash_kernel_doc::ValueKind;
    match value.kind() {
        ValueKind::Null => "null",
        ValueKind::Absent => "absent",
        ValueKind::Bool => "bool",
        ValueKind::Int => "integer",
        ValueKind::Float => "float",
        ValueKind::Text => "text",
        ValueKind::Bytes => "bytes",
        ValueKind::Timestamp => "timestamp",
        ValueKind::Tuple => "tuple",
        ValueKind::List => "list",
        ValueKind::Map => "map",
        ValueKind::Set => "set",
        ValueKind::Record => "record",
        ValueKind::Closure => "closure",
        ValueKind::Error => "error",
        ValueKind::Task => "task",
        ValueKind::Function => "function",
        ValueKind::Handle => "handle",
        ValueKind::Ref => "ref",
    }
}

thread_local! {
    /// Each kind's text, made once: `kind` answers a clone.
    static KIND_TEXTS: std::cell::RefCell<std::collections::HashMap<&'static str, std::sync::Arc<str>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn composite(value: &Value) -> bool {
    matches!(value, Value::Tuple(_) | Value::Error(_))
}

/// A primitive's result and the units of its definition's formula, for
/// the operands its fast path takes; `None` for any other.
pub(crate) fn prim_fast(heap: &Heap, op: u64, args: &[Value]) -> Option<(Value, u64)> {
    let a = args.first()?;
    match op {
        PRIM_KIND => {
            let name = kind_text(a);
            let text = KIND_TEXTS.with(|texts| {
                std::sync::Arc::clone(
                    texts
                        .borrow_mut()
                        .entry(name)
                        .or_insert_with(|| std::sync::Arc::from(name)),
                )
            });
            // 1 + 0 + deep_size(result): a text's size is its length + 1.
            let units = 2 + name.len() as u64;
            Some((Value::Text(text), units))
        }
        PRIM_SAME => {
            let b = args.get(1)?;
            if composite(a) || composite(b) {
                None
            } else {
                // 1 + min(size a, size b) + nested sizes (0 for what holds
                // nothing immutable) + deep_size(bool) (1).
                let units =
                    2u64.saturating_add(crate::data::size(heap, a).min(crate::data::size(heap, b)));
                Some((Value::Bool(a == b), units))
            }
        }
        PRIM_LT | PRIM_LE => {
            let b = args.get(1)?;
            let order = match (a, b) {
                (Value::Float(x), Value::Float(y)) => Some(x.get().partial_cmp(&y.get())),
                (Value::Int(x), Value::Int(y)) => Some(Some(x.cmp(y))),
                _ => None,
            };
            order.map(|order| {
                let result = if op == PRIM_LT {
                    order == Some(std::cmp::Ordering::Less)
                } else {
                    matches!(
                        order,
                        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                    )
                };
                // 1 + (deep a + deep b)^2 + deep_size(bool) (1); a number's
                // deep size is its size.
                let sum = crate::data::size(heap, a).saturating_add(crate::data::size(heap, b));
                (
                    Value::Bool(result),
                    2u64.saturating_add(sum.saturating_mul(sum)),
                )
            })
        }
        PRIM_NOT => match a {
            Value::Bool(flag) => Some((Value::Bool(!flag), 1)),
            _ => None,
        },
        PRIM_LIST_LEN => match a {
            Value::List(list) => match heap.get(*list) {
                Some(Obj::List(items)) => i64::try_from(items.len())
                    .ok()
                    .map(|n| (Value::Int(lash_kernel_doc::Integer::from(n)), 1)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

pub(crate) extern "C" fn rt_prim(ctx: *mut Ctx, dst: u64, lib: u64, first: u64, op: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let fast = prim_fast(&parts.machine.heap, op, &parts.ctx.temps[first as usize..]);
    let Some((value, units)) = fast else {
        let count = match op {
            PRIM_SAME | PRIM_LT | PRIM_LE => 2,
            _ => 1,
        };
        return rt_native_direct(ctx, dst, lib, first, count);
    };
    let exe = parts.exe;
    let machine = &mut *parts.machine;
    let lib = LibId(lib as u32);
    // What `call_units` gives: nothing inside a native body.
    let units = if machine.charging && exe.lib(lib).native {
        units
    } else {
        0
    };
    if let Err(halt) = machine.pin(&value) {
        let halt = halt.in_function(&exe.lib(lib).definition.name);
        parts.ctx.outcome = Some(Err(halt.into()));
        return 1;
    }
    if let Err(halt) = machine.charge_call(exe, lib, units) {
        parts.ctx.outcome = Some(Err(halt.into()));
        return 1;
    }
    parts.put(dst, value);
    0
}

pub(crate) extern "C" fn rt_list(ctx: *mut Ctx, dst: u64, first: u64, count: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let items: Vec<Value> = (first..first + count)
        .map(|temp| parts.take(temp))
        .collect();
    match parts.machine.alloc(Obj::List(items)) {
        Ok(id) => {
            parts.put(dst, Value::List(id));
            0
        }
        Err(halt) => parts.fail(halt.into()),
    }
}

pub(crate) extern "C" fn rt_tuple(ctx: *mut Ctx, dst: u64, first: u64, count: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let members: Vec<Value> = (first..first + count)
        .map(|temp| parts.take(temp))
        .collect();
    let tuple = Value::Tuple(members.into());
    if !within_depth(&tuple, MAX_VALUE_DEPTH) {
        let result: Eval<()> = raise(
            "too_deep",
            format!("a tuple nests more than {MAX_VALUE_DEPTH} levels"),
        );
        return parts.status(result);
    }
    if let Err(halt) = parts.machine.pin(&tuple) {
        return parts.fail(halt.into());
    }
    parts.put(dst, tuple);
    0
}

pub(crate) extern "C" fn rt_record(ctx: *mut Ctx, dst: u64, first: u64, expr: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a record
    // expression of its code.
    let mut parts = unsafe { parts(ctx) };
    let Expr::Record(entries) = (unsafe { &*(expr as usize as *const Expr) }) else {
        return parts.fail(fault("a record built from no record").into());
    };
    let mut fields = Vec::with_capacity(entries.len());
    for (offset, (name, _)) in entries.iter().enumerate() {
        let value = parts.take(first + offset as u64);
        fields.push((name.clone(), value));
    }
    match parts.machine.alloc(Obj::Record(fields)) {
        Ok(id) => {
            parts.put(dst, Value::Record(id));
            0
        }
        Err(halt) => parts.fail(halt.into()),
    }
}

/// An expression the compiled code leaves to the interpreter.
pub(crate) extern "C" fn rt_eval(ctx: *mut Ctx, dst: u64, expr: u64) -> u32 {
    // SAFETY: called by compiled code with its context and an expression
    // of its code.
    let mut parts = unsafe { parts(ctx) };
    let expr = unsafe { &*(expr as usize as *const Expr) };
    match parts.machine.eval(parts.task, parts.host, parts.exe, expr) {
        Ok(value) => {
            parts.put(dst, value);
            0
        }
        Err(interrupt) => parts.fail(interrupt),
    }
}

/// A condition: 1 true, 0 false, 2 a raise.
pub(crate) extern "C" fn rt_truth(ctx: *mut Ctx, src: u64, which: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    match parts.take(src) {
        Value::Bool(true) => 1,
        Value::Bool(false) => 0,
        _ => {
            let message = if which == 0 {
                "an `if` condition must be a bool"
            } else {
                "a `while` condition must be a bool"
            };
            let result: Eval<()> = raise("type_error", message);
            parts.status(result);
            2
        }
    }
}

pub(crate) extern "C" fn rt_bind(ctx: *mut Ctx, at: u64, shared: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let value = parts.take(src);
    let local = Local {
        slot: 0,
        at: at as u32,
        shared: shared != 0,
    };
    let result = parts.machine.bind(parts.task, local, value);
    parts.status(result.map_err(Interrupt::from))
}

pub(crate) extern "C" fn rt_write_var(ctx: *mut Ctx, var: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a variable of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let var = unsafe { &*(var as usize as *const Var) };
    let value = parts.take(src);
    let result = parts.machine.write_var(parts.task, parts.exe, var, value);
    parts.status(result)
}

/// Completes a statement with its right-hand side's value, as the
/// interpreter does (`complete_stmt`).
pub(crate) extern "C" fn rt_complete(ctx: *mut Ctx, stmt: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a statement of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let stmt = unsafe { &*(stmt as usize as *const Stmt) };
    let value = parts.take(src);
    let result = parts
        .machine
        .complete_stmt(parts.task, parts.host, parts.exe, stmt, value);
    parts.status(result)
}

pub(crate) extern "C" fn rt_pin(ctx: *mut Ctx, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let value = parts.ctx.temps[src as usize].clone();
    let result = parts.machine.pin(&value);
    parts.status(result.map_err(Interrupt::from))
}

/// `write_member` for a field, once the value is pinned and the target
/// evaluated.
pub(crate) extern "C" fn rt_set_field(ctx: *mut Ctx, target: u64, field: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context and a field name of
    // its code.
    let mut parts = unsafe { parts(ctx) };
    let field = unsafe { &*(field as usize as *const String) };
    let target = parts.take(target);
    let value = parts.take(src);
    let Value::Record(record) = target else {
        let result: Eval<()> = raise("type_error", "only a record has fields to assign");
        return parts.status(result);
    };
    let result = parts.machine.write_record_field(record, field, value);
    parts.status(result)
}

/// `write_member` for an index, once the value is pinned and the target
/// and the index evaluated.
pub(crate) extern "C" fn rt_set_index(ctx: *mut Ctx, target: u64, index: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let target = parts.take(target);
    let index = parts.take(index);
    let value = parts.take(src);
    let machine = &mut *parts.machine;
    let result: Eval<()> = (|| {
        match target {
            Value::List(list) => {
                let length = machine.heap.list(list).map_or(0, Vec::len);
                let position =
                    super::eval::position(&index)?.filter(|position| *position <= length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} is assigned at 0 to {length}"),
                    );
                };
                machine.reserve(value_bytes(&value))?;
                if let Some(Obj::List(items)) = machine.heap.get_mut(list) {
                    match items.get_mut(position) {
                        Some(held) => *held = value,
                        None => items.push(value),
                    }
                }
            }
            Value::Record(record) => {
                let Value::Text(field) = index else {
                    return raise("type_error", "a record index must be text");
                };
                machine.write_record_field(record, &field, value)?;
            }
            Value::Map(map) => {
                let key = super::eval::key(&index)?;
                machine.reserve(value_bytes(&value).saturating_add(value_bytes(&index)))?;
                if let Some(Obj::Map(table)) = machine.heap.get_mut(map) {
                    table.insert(key, index, value);
                }
            }
            Value::Set(set) => {
                let key = super::eval::key(&index)?;
                let Value::Bool(member) = value else {
                    return raise(
                        "type_error",
                        "a set's member is assigned `true` to add it or `false` to remove it",
                    );
                };
                machine.reserve(value_bytes(&index))?;
                if let Some(Obj::Set(table)) = machine.heap.get_mut(set) {
                    if member {
                        table.insert(key, index, Value::Null);
                    } else {
                        table.remove(&key);
                    }
                }
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a record, a map or a set is assigned by index",
                );
            }
        }
        Ok(())
    })();
    parts.status(result)
}

/// An action statement, as `exec` runs it: 0 completed, 1 a raise or a
/// halt, 2 the step ends here (a frame was pushed, the task waits, or a
/// `do` left its statement awaited).
pub(crate) extern "C" fn rt_action(ctx: *mut Ctx, stmt: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let id = StmtId(stmt as u32);
    let exe = parts.exe;
    let task = parts.task;
    match exe.stmt(id) {
        Stmt::Let {
            value: crate::compile::Rhs::Action(action),
            ..
        }
        | Stmt::Assign {
            value: crate::compile::Rhs::Action(action),
            ..
        } => match parts.machine.act(task, exe, id, action) {
            Ok(Some(value)) => {
                parts.frame().awaiting = Some(id);
                let result = parts.machine.finish_action(task, parts.host, exe, value);
                parts.status(result)
            }
            Ok(None) => 2,
            Err(interrupt) => parts.fail(interrupt),
        },
        Stmt::Do(action) => match parts.machine.act(task, exe, id, action) {
            Ok(Some(_)) => {
                if parts.frame().awaiting.is_some() {
                    2
                } else {
                    0
                }
            }
            Ok(None) => 2,
            Err(interrupt) => parts.fail(interrupt),
        },
        _ => parts.fail(fault("an action statement is no action").into()),
    }
}

pub(crate) extern "C" fn rt_clear(ctx: *mut Ctx, block: u64) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let exe = parts.exe;
    let frame = parts.frame();
    for at in &exe.block(BlockId(block as u32)).declares {
        frame.slots[*at as usize] = SlotState::Empty;
    }
}

pub(crate) extern "C" fn rt_push_while(ctx: *mut Ctx, stmt: u64) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    parts.frame().control.push(Control::While {
        stmt: StmtId(stmt as u32),
        started: 0,
    });
}

pub(crate) extern "C" fn rt_started(ctx: *mut Ctx) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    if let Some(Control::While { started, .. }) = parts.frame().control.last_mut() {
        *started += 1;
    }
}

pub(crate) extern "C" fn rt_pop(ctx: *mut Ctx) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    parts.frame().control.pop();
}

pub(crate) extern "C" fn rt_push_try(ctx: *mut Ctx, stmt: u64) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    parts.frame().control.push(Control::Try {
        stmt: StmtId(stmt as u32),
        phase: TryPhase::Body,
    });
}

/// A `for` statement's start, once its iterable is evaluated.
pub(crate) extern "C" fn rt_for_start(ctx: *mut Ctx, stmt: u64, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let cursor = match parts.take(src) {
        Value::List(id) => Cursor::List(id, 0),
        Value::Tuple(items) => Cursor::Tuple(items, 0),
        Value::Map(id) | Value::Set(id) => Cursor::Table(id, None),
        _ => {
            let result: Eval<()> = raise(
                "type_error",
                "`for` iterates a list, a tuple, a map or a set",
            );
            return parts.status(result);
        }
    };
    parts.frame().control.push(Control::For {
        stmt: StmtId(stmt as u32),
        cursor,
        started: 0,
    });
    0
}

/// A `for` loop's next iteration (`next_iteration`): 1 an element is bound,
/// 0 the loop ended, 2 a halt.
pub(crate) extern "C" fn rt_for_next(ctx: *mut Ctx) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let cost = parts.machine.costs.loop_test;
    if let Err(halt) = parts.machine.charge(cost) {
        parts.fail(halt.into());
        return 2;
    }
    let exe = parts.exe;
    let task = parts.task;
    let machine = &mut *parts.machine;
    let Some(frame) = machine
        .tasks
        .get_mut(task.0 as usize)
        .and_then(|task| task.frames.last_mut())
    else {
        parts.fail(fault("a running task has no frame").into());
        return 2;
    };
    let Some(Control::For {
        stmt,
        cursor,
        started,
    }) = frame.control.last_mut()
    else {
        parts.fail(fault("no loop is innermost").into());
        return 2;
    };
    let Stmt::For { binding, .. } = exe.stmt(*stmt) else {
        parts.fail(fault("a `for` loop is not at a `for`").into());
        return 2;
    };
    let element = match cursor {
        Cursor::List(list, position) => {
            let element = machine
                .heap
                .list(*list)
                .and_then(|items| items.get(*position))
                .cloned();
            *position += usize::from(element.is_some());
            element
        }
        Cursor::Tuple(items, position) => {
            let element = items.get(*position).cloned();
            *position += usize::from(element.is_some());
            element
        }
        Cursor::Table(table, last) => {
            let next = machine
                .heap
                .table(*table)
                .and_then(|table| table.after(*last))
                .map(|(sequence, key)| (sequence, key.clone()));
            next.map(|(sequence, key)| {
                *last = Some(sequence);
                key
            })
        }
    };
    match element {
        Some(element) => {
            *started += 1;
            let binding = *binding;
            match machine.bind(task, binding, element) {
                Ok(()) => 1,
                Err(halt) => {
                    parts.fail(halt.into());
                    2
                }
            }
        }
        None => {
            frame.control.pop();
            0
        }
    }
}

pub(crate) extern "C" fn rt_return(ctx: *mut Ctx, src: u64) -> u32 {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let value = parts.take(src);
    let task = parts.task;
    if task == TaskId::MAIN
        && parts.machine.tasks[0].frames.len() == 1
        && let Err(interrupt) = parts.machine.copy_out(&value)
    {
        return parts.fail(interrupt);
    }
    parts.ctx.outcome = Some(Ok(Completion::Return(value)));
    0
}

pub(crate) extern "C" fn rt_throw(ctx: *mut Ctx, src: u64) {
    // SAFETY: called by compiled code with its context.
    let mut parts = unsafe { parts(ctx) };
    let value = parts.take(src);
    parts.ctx.outcome = Some(Err(Interrupt::Raise(value)));
}

pub(crate) extern "C" fn rt_return_null(ctx: *mut Ctx) {
    // SAFETY: called by compiled code with its context.
    let parts = unsafe { parts(ctx) };
    parts.ctx.outcome = Some(Ok(Completion::Return(Value::Null)));
}
