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
//! region against its body. A document expands a library body as the
//! library published it, each fused run read as the op it fused, and
//! takes each constant the region reads into its own pool, a function
//! value's in the caller's Link: the region is the document's code.

use std::collections::HashMap;

use lash_kernel_value::names::CallableId;

use super::compile::{Result, coordinate, pool};
use super::op::Op;
use super::program::{
    Block, CallSite, Callee, Constant, InlineSite, LIB, Program, Unit, UnitId, UnitKind,
};

/// The most ops a body's region may hold to be expanded: its guard
/// path's ops and two for each stub.
const BUDGET: u32 = 128;

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
pub(crate) fn inline(program: &mut Program) -> Result<()> {
    #[cfg(test)]
    if DISABLED.with(std::cell::Cell::get) {
        return Ok(());
    }
    let mut taken = HashMap::new();
    for at in 0..program.units.len() {
        let caller = program.own_unit(at);
        let sites: Vec<(usize, u32, UnitId)> = program.units[at]
            .ops
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| match *op {
                Op::Call(site) => match &program.units[at].calls[site as usize] {
                    CallSite {
                        callee: Callee::Unit { unit, .. },
                        ..
                    } if admissible(program, caller, *unit) => Some((pc, site, *unit)),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for (pc, site, callee) in sites {
            if let Some(region) = Region::of(program, callee) {
                expand(program, at, pc, site, callee, &region, &mut taken)?;
            }
        }
    }
    Ok(())
}

/// Whether `callee` is a body the pass may expand into `caller`: a covered
/// library body, not the caller itself, sharing no local with a closure
/// and seen by no observer.  Its region's size is [`Region::of`]'s.
fn admissible(program: &Program, caller: UnitId, callee: UnitId) -> bool {
    callee != caller && expandable(program.unit(callee))
}

/// Whether `body` is one the pass may expand into another unit.
pub(crate) fn expandable(body: &Unit) -> bool {
    body.covered
        && matches!(body.kind, UnitKind::Body(_) | UnitKind::Declared(_))
        && body.boxed.is_empty()
        && body.closures.is_empty()
        && body.points.is_empty()
}

/// `body`'s op at `pc` as it was compiled: a published library body's
/// fused runs and traces stand for the ops it validated with.
fn op_at(body: &Unit, pc: usize) -> Op {
    match body.ops[pc] {
        Op::Fused(n) => body.fused[n as usize].first,
        Op::Trace(n) => Op::Block(body.traces[n as usize].block),
        op => op,
    }
}

/// What a body's region holds: each block reached from its entry, in the
/// body's order, and whether it is on the guard path.
struct Region {
    blocks: Vec<(usize, bool)>,
}

impl Region {
    /// The region of `callee`, `None` when its entry block is off the
    /// guard path, a jump of the path enters no block at its head, the
    /// path reaches no return, so that every run of the region would
    /// deoptimize, or the region would hold more than [`BUDGET`] ops.
    fn of(program: &Program, callee: UnitId) -> Option<Self> {
        let body = program.unit(callee);
        let block_at = |pc: u32| body.blocks.iter().position(|block| block.start == pc);
        let hot = |block: &Block| {
            (block.start as usize + 1..block.end as usize)
                .all(|pc| local(program, body, op_at(body, pc), block.end as usize))
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
                if let Op::Jump(to) | Op::JumpIfFalse(to) = op_at(body, pc) {
                    work.push(block_at(to)?);
                }
            }
            let last = op_at(body, block.end as usize - 1);
            if !matches!(last, Op::Jump(_) | Op::Return | Op::Raise) && k + 1 < body.blocks.len() {
                work.push(k + 1);
            }
        }
        // Disjoint blocks cover at most u32::MAX ops; each cold stub adds
        // at most one extra op per block, so the sum fits u64.
        let size: u64 = reached
            .iter()
            .zip(&body.blocks)
            .map(|(on, block)| match on {
                Some(true) => u64::from(block.end - block.start),
                Some(false) => 2,
                None => 0,
            })
            .sum();
        let returns = reached.iter().zip(&body.blocks).any(|(on, block)| {
            *on == Some(true)
                && (block.start..block.end).any(|pc| op_at(body, pc as usize) == Op::Return)
        });
        if reached[0] != Some(true) || !returns || size > u64::from(BUDGET) {
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
                    let entry = program.native(native);
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
    taken: &mut HashMap<(u32, u32), u32>,
) -> Result<Option<()>> {
    let body = program.unit(callee);
    let caller = &program.units[at];
    let index = coordinate(caller.inlines.len(), "inline sites of one function")?;
    let entry = coordinate(caller.ops.len(), "ops of one function")?;
    let first_block = coordinate(caller.blocks.len(), "blocks of one function")?;
    let first_call = coordinate(caller.calls.len(), "call sites of one function")?;
    // The region's ops stand for the call's expression, where the
    // unit's origin table covers its ops.
    let origin = (caller.origin.len() == caller.ops.len())
        .then(|| Some((caller.origin.get(pc).cloned()?, *caller.stated.get(pc)?)))
        .flatten();
    // Where each reached block of the body starts in the region.
    let mut starts = vec![None; body.blocks.len()];
    let mut next = entry;
    for &(k, on) in &region.blocks {
        starts[k] = Some(next);
        let block = body.blocks[k];
        next = coordinate(
            next as usize
                + if on {
                    (block.end - block.start) as usize
                } else {
                    2
                },
            "ops of one function",
        )?;
    }
    let target = |to: u32| starts[body.blocks.iter().position(|block| block.start == to)?];
    // Region::of admits at most BUDGET (128) ops. Admit the other tables
    // before adding any of its offsets to their u32 coordinates.
    coordinate(
        caller.blocks.len() + region.blocks.len(),
        "blocks of one function",
    )?;
    let extra_calls: usize = region
        .blocks
        .iter()
        .filter(|(_, on)| *on)
        .map(|(k, _)| {
            let block = body.blocks[*k];
            (block.start..block.end)
                .filter(|pc| matches!(op_at(body, *pc as usize), Op::Call(_)))
                .count()
        })
        .sum();
    coordinate(
        caller.calls.len() + extra_calls,
        "call sites of one function",
    )?;
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
                let op = match op_at(body, from) {
                    Op::Jump(to) => {
                        let Some(to) = target(to) else {
                            return Ok(None);
                        };
                        Op::Jump(to)
                    }
                    Op::JumpIfFalse(to) => {
                        let Some(to) = target(to) else {
                            return Ok(None);
                        };
                        Op::JumpIfFalse(to)
                    }
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
    // A library body's constants become the document's own, read as the
    // caller's Link.
    if program.tag == 0 && callee & LIB != 0 {
        let link = program.units[at].link;
        let index = program.link_index(&link).unwrap_or(0);
        for op in &mut ops {
            if let Op::Const(n) | Op::Field(n) = op
                && *n & LIB != 0
            {
                *n = take(program, taken, index, *n)?;
            }
        }
    }
    let end = entry + ops.len() as u32;
    let steps = program.unit(callee).steps;
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
    if let Some((origin, stated)) = origin {
        caller.origin.extend(std::iter::repeat_n(origin, ops.len()));
        caller.stated.extend(std::iter::repeat_n(stated, ops.len()));
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
        steps,
        element: false,
        folds: (0, 0),
        lookup_words: None,
    });
    Ok(Some(()))
}

/// Library constant `n` as the document's own constant, a function value
/// naming the Link of index `link`.
fn take(
    program: &mut Program,
    taken: &mut HashMap<(u32, u32), u32>,
    link: u32,
    n: u32,
) -> Result<u32> {
    if let Some(own) = taken.get(&(link, n)) {
        return Ok(*own);
    }
    let constant = match program.constant(n) {
        Constant::Function(callable) => Constant::Function(CallableId {
            link: program.links[link as usize],
            function: callable.function,
        }),
        other => other.clone(),
    };
    let own = pool(0, program.constants.len(), "constants")?;
    program.constants.push(constant);
    taken.insert((link, n), own);
    Ok(own)
}
