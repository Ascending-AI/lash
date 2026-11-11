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

use super::op::Op;
use super::program::{LIB, Program, Then, Unit};

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

/// Builds the traces of every unit of `program`, its own code.
pub(crate) fn trace(program: &mut Program) {
    let readable: Vec<bool> = (0..program.lib.constants.len())
        .map(|k| program.readable(LIB | k as u32))
        .collect();
    let readable = |n: u32| n & LIB == 0 || readable[(n & !LIB) as usize];
    for unit in &mut program.units {
        trace_unit(unit, &readable);
    }
}

/// Builds `unit`'s traces and puts each in place of its head's op. A
/// constant goes into a line only where it reads without placing
/// (`Program::readable`).
pub(crate) fn trace_unit(unit: &mut Unit, readable: &impl Fn(u32) -> bool) {
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
        let mut saved = 0;
        for micro in line {
            match micro {
                Micro::Exit { .. } => break,
                Micro::Copy { .. } => saved += 2,
                Micro::Fused { n, .. } if unit.fused[*n as usize].words.is_none() => {
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
                if let Then::Branch { to, .. } = unit.fused[*n as usize].then {
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
    unit.micros = micros;
}

/// The trace from block head `head`.
fn line(unit: &Unit, head: usize, readable: &impl Fn(u32) -> bool, micros: &mut Vec<Micro>) {
    let mut pc = head;
    for _ in 0..LENGTH {
        let Some(&op) = unit.ops.get(pc) else {
            break;
        };
        let at = pc as u32;
        let (micro, next) = match op {
            Op::Block(block) => (Micro::Block { pc: at, block }, pc + 1),
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
