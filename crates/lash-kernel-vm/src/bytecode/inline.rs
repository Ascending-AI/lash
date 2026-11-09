//! Inlining: a covered library body's guard path runs in its caller
//! (cutover contract §1, §3).
//!
//! A call of a covered kernel-code body costs a frame: the call, the
//! entry, the return and the completion's dispatch around a body that is
//! often a type test, a native answer and a return. The pass expands such
//! a body in place of its call, as a region of ops appended past the
//! caller's own, so no op of the caller moves and every resume point the
//! caller has stays where it was.
//!
//! The region is the body's *guard path*: the blocks reached from its
//! entry whose every op is local (pushes, locals, forward jumps, member
//! reads, constructions, a return or raise, and calls of natives that
//! have no body and no guard, so cannot decline, suspend or call back).
//! Each other block reached becomes a stub that materializes the body's
//! frame at that block's head (`Op::Deopt`): the body then runs as it
//! would have from its call, over the same locals and operands. So the
//! region reaches no action, step, slice point, loop, callback, unit call
//! or decline, and no parked image holds a region pc.
//!
//! The body's logical activation stays. `Op::Inline` checks the call depth
//! at the original entry, admits the body's frame, owes the call's
//! completion bill and lays the body's locals out where its frame would;
//! `Op::Leave` completes and releases it as a return does; a raise in the
//! region completes it first, at the body's position, and unwinds on from
//! the caller. The region's ops carry the body's events and step
//! tables op for op, so the charge, its tape and every refusal fall at the
//! body's own events.
//!
//! The expansion is derived from the body this program compiled: its
//! unit, the original call site (kept in the caller's calls, with its
//! callee, mark and resume point) and the natives it names, each selected
//! under its declared version. Selection follows the body's code, never a
//! function's name.
//!
//! It runs after compilation and before validation, which checks each
//! region against its body.

use super::op::Op;
use super::program::{Block, Callee, InlineSite, Program, Unit, UnitId, UnitKind};

/// The most ops a body may hold to be expanded.
const BUDGET: usize = 64;

#[cfg(test)]
thread_local! {
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The ops the pass has added on this thread, for the laws: proof
    /// that a law's run took the inline path.
    static ADDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The ops the pass has added on this thread so far.
#[cfg(test)]
pub(crate) fn added() -> u64 {
    ADDED.with(std::cell::Cell::get)
}

/// Runs `f` with the pass disabled on this thread: the laws' reference
/// mode, every body called through its frame.
#[cfg(test)]
pub(crate) fn disabled<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            DISABLED.with(|disabled| disabled.set(self.0));
        }
    }
    let _restore = Restore(DISABLED.with(|disabled| disabled.replace(true)));
    f()
}

/// Expands every admissible body call of `program`.
pub(crate) fn inline(program: &mut Program) {
    #[cfg(test)]
    if DISABLED.with(std::cell::Cell::get) {
        return;
    }
    for at in 0..program.units.len() {
        let caller = at as UnitId;
        let sites: Vec<(usize, u32, UnitId)> = program.units[at]
            .ops
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| match *op {
                Op::Call(site) => match program.units[at].calls[site as usize].callee {
                    Callee::Unit { unit, .. } if admissible(program, caller, unit) => {
                        Some((pc, site, unit))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for (pc, site, callee) in sites {
            if let Some(region) = Region::of(program, callee) {
                expand(program, at, pc, site, callee, &region);
            }
        }
    }
}

/// Whether `callee` is a body the pass may expand into `caller`: a covered
/// library body, not the caller itself, small, sharing no local with a
/// closure and seen by no observer.
fn admissible(program: &Program, caller: UnitId, callee: UnitId) -> bool {
    let body = &program.units[callee as usize];
    callee != caller
        && body.covered
        && matches!(body.kind, UnitKind::Body(_) | UnitKind::Declared(_))
        && body.boxed.is_empty()
        && body.closures.is_empty()
        && body.points.is_empty()
        && body.ops.len() <= BUDGET
}

/// What a body's region holds: each block reached from its entry, in the
/// body's order, and whether it is on the guard path.
struct Region {
    blocks: Vec<(usize, bool)>,
}

impl Region {
    /// The region of `callee`, `None` when its entry block is off the
    /// guard path or a jump of the path enters no block at its head.
    fn of(program: &Program, callee: UnitId) -> Option<Self> {
        let body = &program.units[callee as usize];
        let block_at = |pc: u32| body.blocks.iter().position(|block| block.start == pc);
        let hot = |block: &Block| {
            (block.start as usize + 1..block.end as usize)
                .all(|pc| local(program, body, body.ops[pc], block.end as usize))
        };
        let mut reached = vec![None; body.blocks.len()];
        let mut work = vec![0usize];
        while let Some(k) = work.pop() {
            if reached[k].is_some() {
                continue;
            }
            let block = body.blocks[k];
            let on = hot(&block);
            reached[k] = Some(on);
            if !on {
                continue;
            }
            for pc in block.start as usize..block.end as usize {
                if let Op::Jump(to) | Op::JumpIfFalse(to) = body.ops[pc] {
                    work.push(block_at(to)?);
                }
            }
            let last = body.ops[block.end as usize - 1];
            if !matches!(last, Op::Jump(_) | Op::Return | Op::Raise) && k + 1 < body.blocks.len() {
                work.push(k + 1);
            }
        }
        if reached[0] != Some(true) {
            return None;
        }
        let blocks = reached
            .iter()
            .enumerate()
            .filter_map(|(k, on)| on.map(|on| (k, on)))
            .collect();
        Some(Self { blocks })
    }
}

/// Whether `op` of `body`, in a block ending at `end`, runs in a
/// region: it touches only the frame's locals and operands, transfers
/// only forward within the body, and calls only natives that answer or
/// raise.
fn local(program: &Program, body: &Unit, op: Op, end: usize) -> bool {
    match op {
        Op::Nop
        | Op::Const(_)
        | Op::Local(_)
        | Op::SetLocal(_)
        | Op::Pop
        | Op::Index
        | Op::Field(_)
        | Op::MakeTuple(_)
        | Op::MakeList(_)
        | Op::Return
        | Op::Raise => true,
        Op::Jump(to) | Op::JumpIfFalse(to) => to as usize >= end,
        Op::Call(site) => {
            let site = &body.calls[site as usize];
            match site.callee {
                Callee::Native { native, .. } => {
                    let entry = &program.natives[native as usize];
                    entry.body.is_none() && entry.guard.is_none() && site.mark.loops.is_empty()
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Expands call `site` at `pc` of unit `at` into a region of `callee`;
/// leaves the call as it is when a jump of the region has no target in it.
fn expand(
    program: &mut Program,
    at: usize,
    pc: usize,
    site: u32,
    callee: UnitId,
    region: &Region,
) -> Option<()> {
    let body = &program.units[callee as usize];
    let caller = &program.units[at];
    let index = caller.inlines.len() as u32;
    let entry = caller.ops.len() as u32;
    let first_block = caller.blocks.len() as u32;
    let first_call = caller.calls.len() as u32;
    // The region's ops stand for the call's expression, where the
    // unit's origin table covers its ops.
    let origin = (caller.origin.len() == caller.ops.len())
        .then(|| caller.origin.get(pc).cloned())
        .flatten();
    // Where each reached block of the body starts in the region.
    let mut starts = vec![None; body.blocks.len()];
    let mut next = entry;
    for &(k, on) in &region.blocks {
        starts[k] = Some(next);
        let block = body.blocks[k];
        next += if on { block.end - block.start } else { 2 };
    }
    let target = |to: u32| starts[body.blocks.iter().position(|block| block.start == to)?];
    let mut ops = Vec::new();
    let mut event = Vec::new();
    let mut calls = Vec::new();
    let mut blocks = Vec::new();
    for (n, &(k, on)) in region.blocks.iter().enumerate() {
        let block = body.blocks[k];
        let start = entry + ops.len() as u32;
        let head = first_block + n as u32;
        ops.push(Op::Block(head));
        event.push(Default::default());
        if on {
            for from in block.start as usize + 1..block.end as usize {
                let op = match body.ops[from] {
                    Op::Jump(to) => Op::Jump(target(to)?),
                    Op::JumpIfFalse(to) => Op::JumpIfFalse(target(to)?),
                    Op::Return => Op::Leave(index),
                    Op::Call(call) => {
                        calls.push(body.calls[call as usize].clone());
                        Op::Call(first_call + calls.len() as u32 - 1)
                    }
                    op => op,
                };
                ops.push(op);
                event.push(body.event[from]);
            }
        } else {
            ops.push(Op::Deopt(block.start));
            event.push(Default::default());
        }
        blocks.push(start);
    }
    let end = entry + ops.len() as u32;
    #[cfg(test)]
    ADDED.with(|added| added.set(added.get() + ops.len() as u64));
    let caller = &mut program.units[at];
    caller.ops[pc] = Op::Inline(index);
    for (n, &start) in blocks.iter().enumerate() {
        let stop = blocks.get(n + 1).copied().unwrap_or(end);
        let mut charge = Some(0u64);
        let mut sum = 0u64;
        caller.through.push(0);
        for at in (start + 1..stop).map(|pc| (pc - entry) as usize) {
            let cost = event[at].total();
            charge = charge.and_then(|total| total.checked_add(cost));
            sum = sum.saturating_add(cost);
            caller.through.push(sum);
        }
        caller.blocks.push(Block {
            start,
            end: stop,
            charge,
        });
    }
    caller.step.extend(std::iter::repeat_n(false, ops.len()));
    if let Some(origin) = origin {
        caller.origin.extend(std::iter::repeat_n(origin, ops.len()));
    }
    caller.ops.extend(ops);
    caller.event.extend(event);
    caller.calls.extend(calls);
    caller.inlines.push(InlineSite {
        callee,
        call: site,
        at: pc as u32,
        entry,
        end,
        folds: (0, 0),
    });
    Some(())
}
