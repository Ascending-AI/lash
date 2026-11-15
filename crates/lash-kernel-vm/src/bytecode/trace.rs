//! Traces: each block head's straight line of ops, pre-decoded so that a
//! prepaid block runs without the dispatcher's per-op work (cutover
//! contract §3).
//!
//! A prepaid, unobserved block bills no op on its own, tests no slice
//! and sees no point inside it: the dispatcher's exact step is skipped at
//! every op there, and what is left of an op is its own effect. A trace
//! is a block head's ops from the head on, through the heads control
//! falls into and the jumps it takes, as micro-ops that do exactly that
//! effect. Each micro-op stands for one op (a `Local` and the `SetLocal`
//! after it for two) and gives control back to the dispatcher at that op
//! before any effect where it cannot run it the way the dispatcher would
//! on the prepaid path: the dispatcher then runs the op under every rule.
//! A head's micro-op enters its block as the head's op does, and where
//! the block is not prepaid the dispatcher runs the block event by event
//! from the op after the head.
//!
//! Traces are built when a unit is compiled, after fusion, on validated
//! code, and are never saved: a saved frame stands at an op, and the op
//! a head's trace replaces is still `Unit::original`'s.

use lash_kernel_doc::{Value, scalar::ScalarOp};

use super::op::Op;
use super::program::{Constant, Fused, LIB, Operand, Program, Then, Unit};

/// The most micro-ops one trace runs before it gives control back.
const LENGTH: usize = 64;

/// The fewest dispatches a line that does not loop must save: a
/// trace's entry and exit cost about as much as the dispatcher's own
/// work at that many ops. A micro-op does at its op what the
/// dispatcher's arm does, so a line saves only the dispatches.
const SHORTEST: usize = 8;

/// Where a trace starts: the block head it replaces and its first
/// micro-op.
#[derive(Clone, Copy, Debug)]
pub struct Trace {
    /// The head's `Op::Block` operand.
    pub block: u32,
    /// The trace's first micro-op in the unit's `micros`.
    pub start: u32,
}

/// No trace: control goes back to the dispatcher at the op.
pub const NONE: u32 = u32::MAX;

/// A [`Micro::Then`] with no `Pass` after its head.
pub const NO_PASS: u16 = u16::MAX;

/// One op of a trace (`pc` its index), run as the dispatcher runs it on a
/// prepaid, unobserved block. Where a micro-op names `next`, it is the
/// first micro-op of the trace at the op control goes to, or [`NONE`].
#[derive(Clone, Copy, Debug)]
pub enum Micro {
    /// A block head: the block before ends, this one is entered.
    Block {
        pc: u32,
        block: u32,
    },
    /// A loop's head: the block before ends, and this one is entered with
    /// block `then`, the one its fused condition falls into, prepaid
    /// ahead under one guard (`Meter::enter_two`).
    Trip {
        pc: u32,
        block: u32,
        then: u32,
    },
    /// The head of the block a [`Micro::Trip`] prepaid ahead, which its
    /// condition fell into, and the loop's `Pass(pass)` right after it
    /// unless `pass` is [`NO_PASS`]: the block was entered with the trip
    /// where the pair fit, else it is entered here as a head is.
    Then {
        pc: u32,
        block: u32,
        pass: u16,
    },
    Pass {
        pc: u32,
        n: u16,
    },
    Local {
        pc: u32,
        n: u16,
    },
    Const {
        pc: u32,
        n: u32,
    },
    SetLocal {
        pc: u32,
        n: u16,
    },
    /// `Local(from)` then `SetLocal(to)`, at `pc` and the op after it.
    Copy {
        pc: u32,
        from: u16,
        to: u16,
    },
    Pop {
        pc: u32,
    },
    CaptureGet {
        pc: u32,
        n: u16,
    },
    CellGet {
        pc: u32,
        n: u16,
    },
    /// Fused run `n` at `pc`; `next` is the trace at its branch's target.
    Fused {
        pc: u32,
        n: u32,
        next: u32,
    },
    /// Inline site `n` at `pc`.
    Inline {
        pc: u32,
        n: u32,
    },
    /// A `JumpIfFalse` or, when `looping`, a `WhileFalse`.
    Branch {
        pc: u32,
        to: u32,
        looping: bool,
        next: u32,
    },
    Jump {
        pc: u32,
        to: u32,
        next: u32,
    },
    /// The dispatcher runs the op at `pc`.
    Exit {
        pc: u32,
    },
}

impl Micro {
    /// The op the micro-op stands for, its first where it stands for two.
    pub fn pc(self) -> u32 {
        match self {
            Self::Block { pc, .. }
            | Self::Trip { pc, .. }
            | Self::Then { pc, .. }
            | Self::Pass { pc, .. }
            | Self::Local { pc, .. }
            | Self::Const { pc, .. }
            | Self::SetLocal { pc, .. }
            | Self::Copy { pc, .. }
            | Self::Pop { pc }
            | Self::CaptureGet { pc, .. }
            | Self::CellGet { pc, .. }
            | Self::Fused { pc, .. }
            | Self::Inline { pc, .. }
            | Self::Branch { pc, .. }
            | Self::Jump { pc, .. }
            | Self::Exit { pc } => pc,
        }
    }
}

/// What a [`Step`] runs: the runner's handler for its micro-op, by the
/// handler table's index (`vm::trace`). A fused run whose scalar form
/// names its operation runs a handler made for that operation, the
/// operand shape and the op that takes its answer ([`word_kind`]). Other
/// direct operands use a shape handler; the rest run [`Kind::Fused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Block,
    Trip,
    Then,
    Pass,
    Local,
    Const,
    SetLocal,
    Copy,
    Pop,
    CaptureGet,
    CellGet,
    Fused,
    Inline,
    Branch,
    Jump,
    Exit,
}

/// The first fused run's kind made for its operation.
pub const WORDS: usize = 32;

/// The number of slots reserved for primary operation handlers.
pub const PLAIN_KINDS: usize = 512;
pub const OTHER: usize = 16;
pub const SMALL_STRIDE: usize = 32;
pub const GENERIC_BUNDLE: usize = 8192;
pub const SMALL_BUNDLE: usize = GENERIC_BUNDLE + PLAIN_KINDS;
// The low fourteen bits select a handler; the next two hold fixed fee bits.
pub const KINDS: usize = 1 << 14;

/// A zero-terminated sequence of small kinds, four bits per kind. The
/// constant is also the handler's template parameter, so its operations
/// inline without a runtime prefix match.
const fn prefix(kinds: &[Kind]) -> u64 {
    let mut code = 0;
    let mut n = 0;
    while n < kinds.len() {
        code |= (kinds[n] as u64 + 1) << (n * 4);
        n += 1;
    }
    code
}

pub const WORD_PREFIXES: [u64; 13] = [
    prefix(&[]),
    prefix(&[Kind::Block]),
    prefix(&[Kind::Trip]),
    prefix(&[Kind::Then]),
    prefix(&[Kind::Then, Kind::Copy]),
    prefix(&[Kind::Block, Kind::Pass]),
    prefix(&[Kind::Block, Kind::Pass, Kind::Local]),
    prefix(&[Kind::Block, Kind::Pop]),
    prefix(&[Kind::Block, Kind::Local]),
    prefix(&[Kind::Then, Kind::Local]),
    prefix(&[Kind::Block, Kind::Pass, Kind::Copy]),
    prefix(&[Kind::Local]),
    prefix(&[Kind::Block, Kind::SetLocal, Kind::Block]),
];
pub const SMALL_PREFIXES: [u64; 58] = [
    prefix(&[]),
    prefix(&[Kind::Block]),
    prefix(&[Kind::Block, Kind::Local]),
    prefix(&[Kind::Block, Kind::Local, Kind::Local]),
    prefix(&[Kind::Block, Kind::Local, Kind::Const]),
    prefix(&[Kind::Block, Kind::CaptureGet]),
    prefix(&[Kind::Block, Kind::CaptureGet, Kind::Local]),
    prefix(&[Kind::Block, Kind::Local, Kind::CaptureGet]),
    prefix(&[Kind::Block, Kind::CaptureGet, Kind::Local, Kind::Local]),
    prefix(&[Kind::Block, Kind::CaptureGet, Kind::Const]),
    prefix(&[Kind::Block, Kind::Local, Kind::Local, Kind::Local]),
    prefix(&[Kind::Block, Kind::Copy]),
    prefix(&[Kind::Then, Kind::Copy]),
    prefix(&[Kind::Block, Kind::Pass]),
    prefix(&[Kind::Block, Kind::Pass, Kind::Local]),
    prefix(&[
        Kind::Block,
        Kind::Pass,
        Kind::Copy,
        Kind::Copy,
        Kind::Copy,
        Kind::Local,
        Kind::Local,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Pass,
        Kind::Copy,
        Kind::Copy,
        Kind::Copy,
        Kind::Local,
        Kind::Local,
        Kind::Const,
    ]),
    prefix(&[
        Kind::Then,
        Kind::Copy,
        Kind::Copy,
        Kind::Copy,
        Kind::Local,
        Kind::Local,
    ]),
    prefix(&[
        Kind::Then,
        Kind::Copy,
        Kind::Copy,
        Kind::Copy,
        Kind::Local,
        Kind::Local,
        Kind::Const,
    ]),
    prefix(&[Kind::Local]),
    prefix(&[Kind::Local, Kind::Local]),
    prefix(&[Kind::Local, Kind::Const]),
    prefix(&[Kind::CaptureGet]),
    prefix(&[Kind::CaptureGet, Kind::Local]),
    prefix(&[Kind::CaptureGet, Kind::Local, Kind::Local]),
    prefix(&[Kind::Const]),
    prefix(&[Kind::Block, Kind::CellGet]),
    prefix(&[Kind::Block, Kind::CellGet, Kind::Local]),
    prefix(&[Kind::Block, Kind::Local, Kind::Local, Kind::Const]),
    prefix(&[
        Kind::Block,
        Kind::CaptureGet,
        Kind::Local,
        Kind::Local,
        Kind::Local,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Copy,
        Kind::Const,
    ]),
    prefix(&[Kind::Const, Kind::SetLocal, Kind::Const]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Copy,
        Kind::Const,
    ]),
    prefix(&[
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
    ]),
    prefix(&[Kind::Local, Kind::Local, Kind::Local]),
    prefix(&[
        Kind::Block,
        Kind::Pop,
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Pop,
        Kind::Block,
        Kind::Copy,
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Local,
    ]),
    prefix(&[Kind::Block, Kind::Const, Kind::SetLocal, Kind::Block]),
    prefix(&[
        Kind::Block,
        Kind::Copy,
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Local,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Local,
    ]),
    prefix(&[Kind::Block, Kind::Const, Kind::SetLocal, Kind::Local]),
    prefix(&[
        Kind::Block,
        Kind::Copy,
        Kind::Block,
        Kind::Local,
        Kind::Local,
        Kind::Local,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Local,
        Kind::Local,
        Kind::Local,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Copy,
        Kind::Const,
    ]),
    prefix(&[Kind::CellGet, Kind::Local, Kind::Local]),
    prefix(&[Kind::Block, Kind::Pop]),
    prefix(&[Kind::Local, Kind::CaptureGet, Kind::Local, Kind::Local]),
    prefix(&[
        Kind::Block,
        Kind::SetLocal,
        Kind::Local,
        Kind::CaptureGet,
        Kind::Local,
        Kind::Local,
    ]),
    prefix(&[Kind::Block, Kind::SetLocal]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
    ]),
    prefix(&[Kind::Local, Kind::Local, Kind::Const]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Copy,
        Kind::Const,
    ]),
    prefix(&[Kind::Trip]),
    prefix(&[Kind::Block, Kind::SetLocal, Kind::Local, Kind::Const]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
    ]),
    prefix(&[
        Kind::Then,
        Kind::Copy,
        Kind::Const,
        Kind::SetLocal,
        Kind::Local,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::Const,
        Kind::Const,
        Kind::Const,
    ]),
    prefix(&[
        Kind::Block,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
        Kind::SetLocal,
        Kind::Const,
    ]),
];

/// The scalar operations, by their index in a fused run's kind.
pub const OPERATIONS: [ScalarOp; 12] = [
    ScalarOp::Add,
    ScalarOp::Sub,
    ScalarOp::Mul,
    ScalarOp::Div,
    ScalarOp::RemTrunc,
    ScalarOp::RemFloor,
    ScalarOp::FloorDiv,
    ScalarOp::Eq,
    ScalarOp::Lt,
    ScalarOp::Le,
    ScalarOp::Gt,
    ScalarOp::Ge,
];

/// What takes a fused run's answer, by its index in the run's kind.
pub const PUSH: u8 = 0;
pub const SET_LOCAL: u8 = 1;
pub const POP: u8 = 2;
pub const BRANCH: u8 = 3;

/// The kind of a fused run of operation `op` (by [`OPERATIONS`]) that
/// reads operand `shape` and whose answer `then` takes.
pub const fn word_kind(op: u8, shape: u8, then: u8) -> usize {
    WORDS + ((op as usize * 7 + shape as usize) << 2 | then as usize)
}

/// A micro-op as the runner reads it: its handler's kind and its
/// operands, in two words that a handler takes in registers. `small` is a
/// local, a pass, a capture, a cell, a branch's `looping` or a fused
/// run's `SetLocal`; `a` a block, a constant, a fused run, an inline
/// site, a copy's `to` or a target; `b` a trip's `then` or a `next`.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    head: u64,
    tail: u64,
}

impl Step {
    fn new(kind: usize, pc: u32, small: u16, a: u32, b: u32) -> Self {
        debug_assert!(kind < KINDS);
        Self {
            head: kind as u64 | u64::from(small) << 16 | u64::from(pc) << 32,
            tail: u64::from(a) | u64::from(b) << 32,
        }
    }

    /// The handler table's index.
    #[inline(always)]
    pub fn kind(self) -> usize {
        (self.head & (KINDS as u64 - 1)) as usize
    }

    /// The constant input-size bits of a word fee's table index.
    #[inline(always)]
    pub fn fee_base(self) -> usize {
        ((self.head >> 14) & 3) as usize
    }

    fn with_fee_base(mut self, base: u8) -> Self {
        debug_assert!(base < 4);
        self.head = (self.head & !(3 << 14)) | u64::from(base) << 14;
        self
    }

    /// The op the micro-op stands for, its first where it stands for two.
    #[inline(always)]
    pub fn pc(self) -> u32 {
        (self.head >> 32) as u32
    }

    #[inline(always)]
    // Step::new packs small in bits 16..32; the cast extracts that field.
    pub fn small(self) -> u16 {
        (self.head >> 16) as u16
    }

    #[inline(always)]
    // Step::new packs a in the low 32 bits and b in the high 32 bits.
    pub fn a(self) -> u32 {
        self.tail as u32
    }

    #[inline(always)]
    pub fn b(self) -> u32 {
        (self.tail >> 32) as u32
    }

    /// The same step with kind `kind` and `small`.
    pub fn with(self, kind: Kind, small: u16) -> Self {
        Self::new(kind as usize, self.pc(), small, self.a(), self.b())
            // fee_base masks two bits, so its value is at most 3.
            .with_fee_base(self.fee_base() as u8)
    }
}

/// A fused call's operands, decoded once when traces are built. A
/// constant is held only when it is already an immediate value.
#[derive(Clone, Copy, Debug)]
pub struct Operands {
    pub locals: [u16; 2],
    pub values: [Value; 2],
    shape: u8,
    direct: bool,
    fee_base: u8,
}

impl Operands {
    fn new(fused: &Fused, immediate: &impl Fn(u32) -> Option<Value>) -> Self {
        debug_assert!(fused.pushes <= 2);
        let mut decoded = Self {
            locals: [0; 2],
            values: [Value::Absent; 2],
            shape: match fused.pushes {
                2 => {
                    2 * u8::from(matches!(fused.operands[0], Operand::Const(_)))
                        + u8::from(matches!(fused.operands[1], Operand::Const(_)))
                }
                1 => 4 + u8::from(matches!(fused.operands[0], Operand::Const(_))),
                _ => 6,
            },
            direct: true,
            fee_base: 0,
        };
        for (at, operand) in fused
            .operands
            .iter()
            .take(usize::from(fused.pushes))
            .enumerate()
        {
            match *operand {
                Operand::Local(n) => decoded.locals[at] = n,
                Operand::Const(n) => match immediate(n) {
                    Some(value) => {
                        decoded.values[at] = value;
                        if matches!(value, Value::Int(word) if word != 0) {
                            let argument = if fused.pushes == 1 { 1 } else { at };
                            decoded.fee_base |= 1 << argument;
                        }
                    }
                    None => decoded.direct = false,
                },
            }
        }
        decoded
    }
}

fn is_fused(kind: usize) -> bool {
    kind == Kind::Fused as usize
        || (OTHER..OTHER + 7).contains(&kind)
        || (WORDS..PLAIN_KINDS).contains(&kind)
}

/// `micro`'s step in `unit`.
fn step(unit: &Unit, micro: Micro) -> Step {
    let plain = |kind: Kind, pc, small, a, b| Step::new(kind as usize, pc, small, a, b);
    match micro {
        Micro::Block { pc, block } => plain(Kind::Block, pc, 0, block, 0),
        Micro::Trip { pc, block, then } => plain(Kind::Trip, pc, 0, block, then),
        Micro::Then { pc, block, pass } => plain(Kind::Then, pc, pass, block, 0),
        Micro::Pass { pc, n } => plain(Kind::Pass, pc, n, 0, 0),
        Micro::Local { pc, n } => plain(Kind::Local, pc, n, 0, 0),
        Micro::Const { pc, n } => plain(Kind::Const, pc, 0, n, 0),
        Micro::SetLocal { pc, n } => plain(Kind::SetLocal, pc, n, 0, 0),
        Micro::Copy { pc, from, to } => plain(Kind::Copy, pc, from, u32::from(to), 0),
        Micro::Pop { pc } => plain(Kind::Pop, pc, 0, 0, 0),
        Micro::CaptureGet { pc, n } => plain(Kind::CaptureGet, pc, n, 0, 0),
        Micro::CellGet { pc, n } => plain(Kind::CellGet, pc, n, 0, 0),
        Micro::Fused { pc, n, next } => {
            let fused = &unit.fused[n as usize];
            let (then, small) = match fused.then {
                Then::Push => (PUSH, 0),
                Then::SetLocal(k) => (SET_LOCAL, k),
                Then::Pop => (POP, 0),
                Then::Branch { .. } | Then::NegatedBranch(_) => (BRANCH, 0),
            };
            // One operation for both forms, where the run has any.
            let operation = match (&fused.words, &fused.numbers) {
                (Some(words), Some(numbers)) if words.operation != numbers.operation => None,
                (Some(form), _) | (None, Some(form)) => Some(form.operation),
                (None, None) => None,
            };
            let inputs = &unit.trace_operands[n as usize];
            let kind = match operation.and_then(|op| OPERATIONS.iter().position(|&at| at == op)) {
                // ScalarOp has twelve variants (0..=11); shape is 0..=6 and then 0..=3.
                Some(op) if inputs.direct => word_kind(op as u8, inputs.shape, then),
                _ if inputs.direct && fused.words.is_none() => OTHER + usize::from(inputs.shape),
                _ => Kind::Fused as usize,
            };
            Step::new(kind, pc, small, n, next).with_fee_base(if kind >= WORDS {
                inputs.fee_base
            } else {
                0
            })
        }
        Micro::Inline { pc, n } => plain(Kind::Inline, pc, 0, n, 0),
        Micro::Branch {
            pc,
            to,
            looping,
            next,
        } => plain(Kind::Branch, pc, u16::from(looping), to, next),
        Micro::Jump { pc, to, next } => plain(Kind::Jump, pc, 0, to, next),
        Micro::Exit { pc } => plain(Kind::Exit, pc, 0, 0, 0),
    }
}

/// Builds the traces of every unit of `program`, its own code: a
/// document's, when it is linked, or the library's, when it is
/// published. A library body's validated ops stay what inlining and
/// validation read (`Unit::original`).
// Compiler::constant/inline::take admit the combined constant pool below LIB.
// Compiler::sealed and inline::expand admit each op coordinate before tracing.
pub(crate) fn trace(program: &mut Program) -> super::compile::Result<()> {
    let own = if program.tag == LIB {
        program.constants.len()
    } else {
        0
    };
    let readable: Vec<bool> = (0..program.lib.constants.len() + own)
        .map(|k| program.readable(LIB | k as u32))
        .collect();
    let document = program.tag == 0;
    let readable = |n: u32| n & LIB == 0 && document || readable[(n & !LIB) as usize];
    let scalar = |constant: &Constant| match constant {
        Constant::Immediate(value) => Some(*value),
        _ => None,
    };
    let constants: Vec<_> = program.constants.iter().map(scalar).collect();
    let library: Vec<_> = program.lib.constants.iter().map(scalar).collect();
    let tag = program.tag;
    let base = if tag == LIB { library.len() } else { 0 };
    let immediate = |id: u32| {
        let at = (id & !LIB) as usize;
        if id & LIB == tag && at >= base {
            constants.get(at - base).copied().flatten()
        } else if id & LIB != 0 {
            library.get(at).copied().flatten()
        } else {
            None
        }
    };
    for unit in &mut program.units {
        trace_unit(unit, &readable, &immediate)?;
    }
    Ok(())
}

/// Builds `unit`'s traces and puts each in place of its head's op. A
/// constant goes into a line only where it reads without placing
/// (`Program::readable`).
pub(crate) fn trace_unit(
    unit: &mut Unit,
    readable: &impl Fn(u32) -> bool,
    immediate: &impl Fn(u32) -> Option<Value>,
) -> super::compile::Result<()> {
    unit.trace_operands = unit
        .fused
        .iter()
        .map(|fused| Operands::new(fused, immediate))
        .collect();
    let mut at = vec![NONE; unit.ops.len()];
    let mut traces = Vec::new();
    let mut micros = Vec::new();
    for head in 0..at.len() {
        let Op::Block(block) = unit.ops[head] else {
            continue;
        };
        let start = micros.len();
        line(unit, head, readable, &mut micros);
        // A line earns its entry and exit cost where it loops back,
        // staying in the trace for every trip, or where it saves enough
        // dispatches. A run with no word path most often leaves the trace
        // through its native's call, so a line is counted up to it. Any
        // other line leaves the head as it was.
        let line = &micros[start..];
        let mut saved = 0usize;
        for micro in line {
            match micro {
                Micro::Exit { .. } => break,
                Micro::Copy { .. } => saved += 2,
                Micro::Then { pass, .. } if *pass != NO_PASS => saved += 2,
                Micro::Fused { n, .. }
                    if unit.fused[*n as usize].words.is_none()
                        && unit.fused[*n as usize].numbers.is_none()
                        && unit.fused[*n as usize].guarded_numbers.is_none() =>
                {
                    saved += 1;
                    break;
                }
                _ => saved += 1,
            }
        }
        // A loop's back edge to a head traced already, or to its own,
        // keeps the trip in traces.
        let loops = line.iter().any(|micro| {
            matches!(micro, Micro::Jump { to, .. }
                if *to as usize == head || (*to as usize) < head && at[*to as usize] != NONE)
        });
        if !loops && saved < SHORTEST {
            micros.truncate(start);
            continue;
        }
        super::compile::coordinate(micros.len(), "trace steps of one function")?;
        // One trace per head, and each trace contains an exit: these
        // indices are below the admitted micro length (NONE is reserved).
        at[head] = traces.len() as u32;
        traces.push(Trace {
            block,
            start: start as u32,
        });
    }
    // Each target's trace, now that every trace is built.
    let start = |pc: u32| match at.get(pc as usize) {
        Some(&n) if n != NONE => traces[n as usize].start,
        _ => NONE,
    };
    for micro in &mut micros {
        match micro {
            Micro::Jump { to, next, .. } | Micro::Branch { to, next, .. } => *next = start(*to),
            Micro::Fused { n, next, .. } => {
                let target = match unit.fused[*n as usize].then {
                    Then::Branch { to, .. } => Some(to),
                    Then::NegatedBranch(n) => Some(unit.negated_branches[n as usize].to),
                    _ => None,
                };
                if let Some(to) = target {
                    *next = start(to);
                }
            }
            _ => {}
        }
    }
    for (head, &n) in at.iter().enumerate() {
        if n != NONE {
            unit.ops[head] = Op::Trace(n);
        }
    }
    unit.traces = traces;
    let steps: Vec<_> = micros.into_iter().map(|micro| step(unit, micro)).collect();
    bundle(unit, steps);
    Ok(())
}

/// Prefix kinds are encoded at build time. Common sequences select a
/// handler whose leading operations are statically known; long or rare
/// sequences retain the general bundle handler.
fn bundle_kind(kind: usize, prefix: &[Step]) -> usize {
    let code = (prefix.len() <= 16).then(|| {
        prefix.iter().enumerate().fold(0u64, |code, (n, step)| {
            code | ((step.kind() + 1) as u64) << (n * 4)
        })
    });
    let patterns = if kind >= WORDS {
        &WORD_PREFIXES[..]
    } else {
        &SMALL_PREFIXES[..]
    };
    match code.and_then(|code| patterns.iter().position(|&pattern| pattern == code)) {
        Some(n) if kind >= WORDS => PLAIN_KINDS * (n + 1) + kind,
        Some(n) => SMALL_BUNDLE + n * SMALL_STRIDE + kind,
        None => GENERIC_BUNDLE + kind,
    }
}

/// Small steps share the following handler's activation. Their original
/// records remain in `trace_steps`, so a cold read or a failed entry
/// still returns its own op, after exactly the preceding effects.
fn bundle(unit: &mut Unit, mut steps: Vec<Step>) {
    let tiny = |step: Step| step.kind() < Kind::Fused as usize;
    let mut starts = vec![NONE; steps.len()];
    let mut groups = Vec::new();
    let mut first = 0;
    while first < steps.len() {
        let mut main = first;
        while main + 1 < steps.len() && tiny(steps[main]) {
            main += 1;
        }
        let mut end = main + 1;
        // A jump after the answer uses this activation too. It can only
        // follow a successful fallthrough; a branch or bail skips it.
        let falls_through = steps[main].kind() == Kind::Inline as usize
            || (is_fused(steps[main].kind())
                && !matches!(
                    unit.fused[steps[main].a() as usize].then,
                    Then::Branch { .. } | Then::NegatedBranch(_)
                ));
        if falls_through
            && steps
                .get(end)
                .is_some_and(|step| step.kind() == Kind::Jump as usize)
        {
            end += 1;
        }
        starts[first] = groups.len() as u32;
        groups.push((first, main, end));
        first = end;
    }
    let redirect = |next: u32| {
        if next == NONE {
            NONE
        } else {
            let target = starts[next as usize];
            assert_ne!(target, NONE, "a trace targets a group boundary");
            target
        }
    };
    for trace in &mut unit.traces {
        trace.start = redirect(trace.start);
    }
    for step in &mut steps {
        if step.kind() == Kind::Jump as usize
            || step.kind() == Kind::Branch as usize
            || is_fused(step.kind())
        {
            *step = Step::new(
                step.kind(),
                step.pc(),
                step.small(),
                step.a(),
                redirect(step.b()),
            )
            // fee_base masks two bits, so its value is at most 3.
            .with_fee_base(step.fee_base() as u8);
        }
    }
    unit.micros = groups
        .into_iter()
        .map(|(first, main, end)| {
            if first == main && end == main + 1 {
                steps[main]
            } else {
                Step::new(
                    bundle_kind(steps[main].kind(), &steps[first..main]),
                    steps[first].pc(),
                    // end starts at main + 1 and may include one trailing jump.
                    (end - main - 1) as u16,
                    first as u32,
                    main as u32,
                )
            }
        })
        .collect();
    unit.trace_steps = steps;
}

/// The block a loop head at `pc` (block `block`) prepays ahead: where
/// the head's block is one fused condition whose run counts no entry
/// and falls into the next block's head, and both blocks bill a fixed
/// cost, the next's nonzero (`Meter::enter_two` holds it as nonzero).
fn ahead(unit: &Unit, pc: usize, block: u32) -> Option<u32> {
    let Some(&Op::Fused(n)) = unit.ops.get(pc + 1) else {
        return None;
    };
    let fused = &unit.fused[n as usize];
    if fused.entry.is_some() {
        return None;
    }
    let after = match fused.then {
        Then::Branch { .. } => fused.call + 2,
        Then::NegatedBranch(n) => unit.negated_branches[n as usize].not + 2,
        _ => return None,
    };
    let Some(&Op::Block(then)) = unit.ops.get(after as usize) else {
        return None;
    };
    let blocks = &unit.blocks;
    (blocks[block as usize].end == after
        && blocks[block as usize].charge.is_some()
        && blocks[then as usize].charge.is_some_and(|cost| cost > 0))
    .then_some(then)
}

/// The trace from block head `head`.
fn line(unit: &Unit, head: usize, readable: &impl Fn(u32) -> bool, micros: &mut Vec<Micro>) {
    let mut pc = head;
    for step in 0..LENGTH {
        let Some(&op) = unit.ops.get(pc) else {
            break;
        };
        let at = pc as u32;
        let (micro, next) = match op {
            // The block a trip's condition falls into, prepaid ahead.
            Op::Block(block)
                if matches!(micros.last(), Some(Micro::Fused { .. }))
                    && matches!(micros.iter().rev().nth(1),
                        Some(Micro::Trip { then, .. }) if *then == block) =>
            {
                match unit.ops.get(pc + 1) {
                    Some(&Op::Pass(n)) if n != NO_PASS => (
                        Micro::Then {
                            pc: at,
                            block,
                            pass: n,
                        },
                        pc + 2,
                    ),
                    _ => (
                        Micro::Then {
                            pc: at,
                            block,
                            pass: NO_PASS,
                        },
                        pc + 1,
                    ),
                }
            }
            // A trip ends in the line: its pair is entered or given back.
            Op::Block(block) => match ahead(unit, pc, block).filter(|_| LENGTH - step >= 3) {
                Some(then) => (
                    Micro::Trip {
                        pc: at,
                        block,
                        then,
                    },
                    pc + 1,
                ),
                None => (Micro::Block { pc: at, block }, pc + 1),
            },
            Op::Pass(n) => (Micro::Pass { pc: at, n }, pc + 1),
            Op::Local(from) => match unit.ops.get(pc + 1) {
                Some(&Op::SetLocal(to)) => (Micro::Copy { pc: at, from, to }, pc + 2),
                _ => (Micro::Local { pc: at, n: from }, pc + 1),
            },
            Op::Const(n) if readable(n) => (Micro::Const { pc: at, n }, pc + 1),
            Op::SetLocal(n) => (Micro::SetLocal { pc: at, n }, pc + 1),
            Op::Pop => (Micro::Pop { pc: at }, pc + 1),
            Op::CaptureGet(n) => (Micro::CaptureGet { pc: at, n }, pc + 1),
            Op::CellGet(n) => (Micro::CellGet { pc: at, n }, pc + 1),
            // A call that counts an entry, the dispatcher runs whole.
            Op::Fused(n) if unit.fused[n as usize].entry.is_none() => {
                let fused = &unit.fused[n as usize];
                let call = fused.call as usize;
                let after = match fused.then {
                    Then::Push => call + 1,
                    Then::NegatedBranch(n) => unit.negated_branches[n as usize].not as usize + 2,
                    _ => call + 2,
                };
                (
                    Micro::Fused {
                        pc: at,
                        n,
                        next: NONE,
                    },
                    after,
                )
            }
            // A call goes into a line only where a fold may hold, as the
            // dispatcher runs every other call, and where a head follows
            // it, as a held fold leaves the exact path on.
            Op::Inline(n)
                if matches!(unit.ops.get(pc + 1), Some(Op::Block(_)))
                    && unit.inlines[n as usize].folds.0 < unit.inlines[n as usize].folds.1
                    && unit.calls[unit.inlines[n as usize].call as usize]
                        .entry
                        .is_none() =>
            {
                (Micro::Inline { pc: at, n }, pc + 1)
            }
            Op::JumpIfFalse(to) | Op::WhileFalse(to) => {
                let looping = matches!(op, Op::WhileFalse(_));
                let micro = Micro::Branch {
                    pc: at,
                    to,
                    looping,
                    next: NONE,
                };
                (micro, pc + 1)
            }
            Op::Jump(to) => {
                micros.push(Micro::Jump {
                    pc: at,
                    to,
                    next: NONE,
                });
                return;
            }
            _ => break,
        };
        micros.push(micro);
        pc = next;
    }
    micros.push(Micro::Exit { pc: pc as u32 });
}
