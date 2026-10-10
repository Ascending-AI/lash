//! The coverage invariants, checked before any tier runs Kir.
//!
//! - [`verify_semantics`]: the semantics table states every op exactly
//!   once, every runtime function of the table is reached by some op, and
//!   every dynamic charge event has an op that emits it.
//! - [`verify_function`]: a [`KirFunction`] is well formed, every cycle of
//!   its CFG passes through a Boundary (so the slice test and the watchdog
//!   are reached on every loop), and every Boundary, each a saveable exit,
//!   has exactly one [`FrameMap`].

use crate::abi::RtFn;
use crate::boundary::{BoundaryKind, CtlStatic, TryPhaseStatic};
use crate::kir::{
    ActionIx, ActionKind, BoundaryIx, CalleeRef, Departure, FieldListIx, KBlockIx, KirFunction,
    LocalRef, LoopIx, LoopKind, Op, OpKind, Operand, ScopeIx, SlotAt, StmtIx, TryIx, VarRef,
};
use crate::maps::{DepartLoc, FrameMap, Loc, LoopLoc};
use crate::sem::{ChargeEvent, OpSpec};

/// Why a semantics table does not cover the ops and the runtime table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverageError {
    #[error("no semantics states op {0:?}")]
    MissingOp(OpKind),
    #[error("entry {at} of the semantics states op {op:?} out of order or twice")]
    Misplaced { at: usize, op: OpKind },
    #[error("no op's semantics calls runtime function {0:?}")]
    UnreachedRtFn(RtFn),
    #[error("no op's semantics emits dynamic charge event {0:?}")]
    UnemittedEvent(ChargeEvent),
}

/// Checks a semantics table against [`OpKind::ALL`], [`RtFn::ALL`] and
/// [`ChargeEvent::ALL`].
pub fn verify_semantics(specs: &[OpSpec]) -> Result<(), CoverageError> {
    for (at, op) in OpKind::ALL.iter().enumerate() {
        if specs.get(at).map(|spec| spec.op) != Some(*op) {
            return Err(if specs.iter().any(|spec| spec.op == *op) {
                CoverageError::Misplaced { at, op: *op }
            } else {
                CoverageError::MissingOp(*op)
            });
        }
    }
    if let Some(extra) = specs.get(OpKind::ALL.len()) {
        return Err(CoverageError::Misplaced {
            at: OpKind::ALL.len(),
            op: extra.op,
        });
    }
    if let Some(f) = RtFn::ALL
        .iter()
        .find(|f| !specs.iter().any(|spec| spec.calls.contains(f)))
    {
        return Err(CoverageError::UnreachedRtFn(*f));
    }
    if let Some(event) = ChargeEvent::ALL
        .iter()
        .filter(|event| !event.is_fixed())
        .find(|event| !specs.iter().any(|spec| spec.events.contains(event)))
    {
        return Err(CoverageError::UnemittedEvent(*event));
    }
    Ok(())
}

/// Where in a [`KirFunction`] a defect is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    Op(u32),
    Block(KBlockIx),
    Boundary(BoundaryIx),
    Loop(LoopIx),
    Try(TryIx),
    Action(ActionIx),
    Scope(ScopeIx),
    Param(u32),
    FieldList(FieldListIx),
    Map(BoundaryIx),
}

/// Why a [`KirFunction`] or its maps are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KirError {
    #[error("the function has no blocks")]
    NoBlocks,
    #[error("block {0:?} is empty or does not start where the previous block ends")]
    BlockRange(KBlockIx),
    #[error("ops follow the last block")]
    TrailingOps,
    #[error("block {0:?} does not end with a terminator")]
    MissingTerminator(KBlockIx),
    #[error("op {op} in block {block:?} is a terminator before the block's end")]
    EarlyTerminator { block: KBlockIx, op: u32 },
    #[error("{at:?} names {operand:?}, past its table")]
    OutOfRange { at: At, operand: Operand },
    #[error("{at:?} names statement {stmt:?}, past the statement table")]
    StmtOutOfRange { at: At, stmt: StmtIx },
    #[error("scope {scope:?} declares slot {slot:?}, past the frame")]
    ScopeSlot { scope: ScopeIx, slot: SlotAt },
    #[error("the map of boundary {boundary:?} names slot {slot:?}, past the frame")]
    MapSlot { boundary: BoundaryIx, slot: SlotAt },
    #[error("{at:?} names local {local:?}, whose sharing disagrees with its slot")]
    LocalShared { at: At, local: LocalRef },
    #[error("op {0} has operands of the wrong count")]
    Arity(u32),
    #[error("the function has {origins} op origins for {ops} ops")]
    Origins { origins: usize, ops: usize },
    #[error("boundary {0:?} awaits an action exactly when its kind does not")]
    BoundaryAwaiting(BoundaryIx),
    #[error("boundary {boundary:?} is the program point of {ops} ops, not one")]
    BoundaryPoints { boundary: BoundaryIx, ops: usize },
    #[error("op {op} of kind {kind:?} cannot be the program point of boundary {boundary:?}")]
    BoundaryKind {
        boundary: BoundaryIx,
        op: u32,
        kind: OpKind,
    },
    #[error("block {0:?} is an exception edge's target but does not start with `Caught`")]
    ExceptionTarget(KBlockIx),
    #[error("a cycle through block {0:?} passes through no Boundary")]
    UncoveredCycle(KBlockIx),
    #[error("map {at} is not in strictly increasing Boundary order")]
    MapOrder { at: usize },
    #[error("boundary {0:?} is a saveable exit with no map")]
    MissingMap(BoundaryIx),
    #[error("the map of boundary {0:?} names a loop or finally its control recipe does not hold")]
    MapControl(BoundaryIx),
}

/// Checks a function and its maps (see the module documentation).
pub fn verify_function(f: &KirFunction, maps: &[FrameMap]) -> Result<(), KirError> {
    blocks(f)?;
    tables(f)?;
    for (at, op) in f.ops.iter().enumerate() {
        let at = at as u32;
        for operand in op.operands() {
            check(f, At::Op(at), operand)?;
        }
        arity(f, at, op)?;
    }
    if f.origins.len() != f.ops.len() {
        return Err(KirError::Origins {
            origins: f.origins.len(),
            ops: f.ops.len(),
        });
    }
    boundaries(f)?;
    exception_targets(f)?;
    cycles(f)?;
    frame_maps(f, maps)
}

fn blocks(f: &KirFunction) -> Result<(), KirError> {
    if f.blocks.is_empty() {
        return Err(KirError::NoBlocks);
    }
    let mut next = 0u32;
    for (ix, block) in f.blocks.iter().enumerate() {
        let ix = KBlockIx(ix as u32);
        let end = u64::from(block.ops.start) + u64::from(block.ops.len);
        if block.ops.start != next || block.ops.len == 0 || end > f.ops.len() as u64 {
            return Err(KirError::BlockRange(ix));
        }
        next = end as u32;
        let last = next - 1;
        for at in block.ops.start..last {
            if f.ops[at as usize].kind().is_terminator() {
                return Err(KirError::EarlyTerminator { block: ix, op: at });
            }
        }
        if !f.ops[last as usize].kind().is_terminator() {
            return Err(KirError::MissingTerminator(ix));
        }
    }
    if next as usize != f.ops.len() {
        return Err(KirError::TrailingOps);
    }
    Ok(())
}

/// Checks the side tables' own indexes.
fn tables(f: &KirFunction) -> Result<(), KirError> {
    for (ix, local) in f.params.iter().enumerate() {
        check(f, At::Param(ix as u32), Operand::Local(*local))?;
    }
    for (ix, block) in f.blocks.iter().enumerate() {
        if let Some(handler) = block.exception {
            check(f, At::Block(KBlockIx(ix as u32)), Operand::Block(handler))?;
        }
    }
    for (ix, list) in f.field_lists.iter().enumerate() {
        for field in list {
            check(
                f,
                At::FieldList(FieldListIx(ix as u32)),
                Operand::Field(*field),
            )?;
        }
    }
    for (ix, scope) in f.scopes.iter().enumerate() {
        if let Some(at) = scope
            .declares
            .iter()
            .find(|at| **at as usize >= f.slots.len())
        {
            return Err(KirError::ScopeSlot {
                scope: ScopeIx(ix as u32),
                slot: SlotAt(*at),
            });
        }
    }
    for (ix, lp) in f.loops.iter().enumerate() {
        let at = At::Loop(LoopIx(ix as u32));
        stmt(f, at, lp.stmt)?;
        check(f, at, Operand::Block(lp.test))?;
        check(f, at, Operand::Block(lp.exit))?;
        if let LoopKind::For { binding } = lp.kind {
            check(f, at, Operand::Local(binding))?;
        }
    }
    for (ix, t) in f.tries.iter().enumerate() {
        let at = At::Try(TryIx(ix as u32));
        stmt(f, at, t.stmt)?;
        if let Some((local, handler)) = t.catch {
            check(f, at, Operand::Local(local))?;
            check(f, at, Operand::Block(handler))?;
        }
        if let Some(finally) = t.finally {
            check(f, at, Operand::Block(finally))?;
        }
    }
    for (ix, action) in f.actions.iter().enumerate() {
        let at = At::Action(ActionIx(ix as u32));
        let callee = |callee: CalleeRef| match callee {
            CalleeRef::Declared(code) => Operand::Code(code),
            CalleeRef::Value(var) => Operand::Var(var),
            CalleeRef::Library(lib) => Operand::Lib(lib),
        };
        let operands = match action.kind {
            ActionKind::Call { callee: c, args } | ActionKind::Spawn { callee: c, args } => {
                vec![callee(c), Operand::Regs(args)]
            }
            ActionKind::Perform { args, .. } => vec![Operand::Regs(args)],
            ActionKind::Sleep(reg)
            | ActionKind::Join(reg)
            | ActionKind::JoinMany(_, reg)
            | ActionKind::Cancel(reg) => vec![Operand::Reg(reg)],
            ActionKind::Yield => Vec::new(),
        };
        for operand in operands {
            check(f, at, operand)?;
        }
    }
    for (ix, boundary) in f.boundaries.iter().enumerate() {
        let at = At::Boundary(BoundaryIx(ix as u32));
        if let Some(awaiting) = boundary.awaiting {
            stmt(f, at, awaiting)?;
        }
        for ctl in &boundary.shape.frames {
            let operand = match *ctl {
                CtlStatic::Block { scope, .. } => Operand::Scope(scope),
                CtlStatic::For { lp } | CtlStatic::While { lp } => Operand::Loop(lp),
                CtlStatic::Try { t, .. } => Operand::Try(t),
            };
            check(f, at, operand)?;
        }
    }
    Ok(())
}

fn stmt(f: &KirFunction, at: At, stmt: StmtIx) -> Result<(), KirError> {
    if stmt.ix() < f.stmts.len() {
        Ok(())
    } else {
        Err(KirError::StmtOutOfRange { at, stmt })
    }
}

fn check(f: &KirFunction, at: At, operand: Operand) -> Result<(), KirError> {
    let local = |local: LocalRef| -> Result<bool, KirError> {
        let Some(slot) = f.slots.get(local.slot as usize) else {
            return Ok(false);
        };
        if local.at as usize >= f.slots.len() {
            return Ok(false);
        }
        if slot.shared != local.shared {
            return Err(KirError::LocalShared { at, local });
        }
        Ok(true)
    };
    let fits = match operand {
        Operand::Reg(reg) => reg.0 < f.regs,
        Operand::Regs(range) => {
            u64::from(range.start.0) + u64::from(range.len) <= u64::from(f.regs)
        }
        Operand::Local(l) | Operand::Var(VarRef::Local(l)) => local(l)?,
        Operand::Var(VarRef::Session(name) | VarRef::Unbound(name)) => name.ix() < f.names.len(),
        Operand::Block(b) => b.ix() < f.blocks.len(),
        Operand::Boundary(b) => b.ix() < f.boundaries.len(),
        Operand::Const(c) => c.ix() < f.consts.len(),
        Operand::Field(field) => field.ix() < f.fields.len(),
        Operand::FieldList(list) => list.ix() < f.field_lists.len(),
        Operand::Code(code) => code.ix() < f.codes.len(),
        Operand::Lib(lib) => lib.ix() < f.libs.len(),
        Operand::Loop(lp) => lp.ix() < f.loops.len(),
        Operand::Try(t) => t.ix() < f.tries.len(),
        Operand::Scope(scope) => scope.ix() < f.scopes.len(),
        Operand::Action(action) => action.ix() < f.actions.len(),
    };
    if fits {
        Ok(())
    } else {
        Err(KirError::OutOfRange { at, operand })
    }
}

fn arity(f: &KirFunction, at: u32, op: &Op) -> Result<(), KirError> {
    let ok = match *op {
        Op::MakeMap { entries, .. } => entries.len % 2 == 0,
        Op::MakeRecord { fields, values, .. } => {
            f.field_lists[fields.ix()].len() == values.len as usize
        }
        _ => true,
    };
    if ok { Ok(()) } else { Err(KirError::Arity(at)) }
}

fn boundaries(f: &KirFunction) -> Result<(), KirError> {
    let mut points = vec![0usize; f.boundaries.len()];
    for (at, op) in f.ops.iter().enumerate() {
        let Some(b) = op.boundary() else { continue };
        points[b.ix()] += 1;
        let kind = f.boundaries[b.ix()].kind;
        let fits = match op.kind() {
            OpKind::Stmt => kind == BoundaryKind::StatementStart,
            OpKind::Step => matches!(kind, BoundaryKind::BlockEnd | BoundaryKind::LoopTest),
            _ => kind.awaits(),
        };
        if !fits {
            return Err(KirError::BoundaryKind {
                boundary: b,
                op: at as u32,
                kind: op.kind(),
            });
        }
    }
    for (ix, boundary) in f.boundaries.iter().enumerate() {
        let b = BoundaryIx(ix as u32);
        if boundary.kind.awaits() != boundary.awaiting.is_some() {
            return Err(KirError::BoundaryAwaiting(b));
        }
        if points[ix] != 1 {
            return Err(KirError::BoundaryPoints {
                boundary: b,
                ops: points[ix],
            });
        }
    }
    Ok(())
}

fn exception_targets(f: &KirFunction) -> Result<(), KirError> {
    for handler in f.blocks.iter().filter_map(|block| block.exception) {
        let first = f.blocks[handler.ix()].ops.start as usize;
        if f.ops[first].kind() != OpKind::Caught {
            return Err(KirError::ExceptionTarget(handler));
        }
    }
    Ok(())
}

/// The blocks control can reach from `block`: its terminator's successors,
/// its exception edge, and, for a block ending a `finally`, the loop
/// targets of every `break` or `continue` that `finally` can resume.
fn edges(f: &KirFunction, block: KBlockIx) -> Vec<KBlockIx> {
    let range = f.blocks[block.ix()].ops;
    let last = &f.ops[(range.start + range.len - 1) as usize];
    let mut out = last.successors();
    out.extend(f.blocks[block.ix()].exception);
    if let Op::FinallyEnd { t, .. } = *last {
        for op in f.ops.iter() {
            match *op {
                Op::FinallyEnter {
                    t: entered,
                    departure: Departure::Break(lp),
                } if entered == t => out.push(f.loops[lp.ix()].exit),
                Op::FinallyEnter {
                    t: entered,
                    departure: Departure::Continue(lp),
                } if entered == t => out.push(f.loops[lp.ix()].test),
                _ => {}
            }
        }
    }
    out
}

/// Every cycle passes through a block holding a Boundary's program point.
fn cycles(f: &KirFunction) -> Result<(), KirError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let covered: Vec<bool> = f
        .blocks
        .iter()
        .map(|block| {
            let range = block.ops.start as usize..(block.ops.start + block.ops.len) as usize;
            f.ops[range].iter().any(|op| op.boundary().is_some())
        })
        .collect();
    let mut marks = vec![Mark::New; f.blocks.len()];
    for root in 0..f.blocks.len() {
        if covered[root] || marks[root] != Mark::New {
            continue;
        }
        marks[root] = Mark::Open;
        let mut stack = vec![(KBlockIx(root as u32), edges(f, KBlockIx(root as u32)), 0)];
        while let Some((block, succ, next)) = stack.last_mut() {
            let Some(to) = succ.get(*next).copied() else {
                marks[block.ix()] = Mark::Done;
                stack.pop();
                continue;
            };
            *next += 1;
            if covered[to.ix()] {
                continue;
            }
            match marks[to.ix()] {
                Mark::Open => return Err(KirError::UncoveredCycle(to)),
                Mark::Done => {}
                Mark::New => {
                    marks[to.ix()] = Mark::Open;
                    stack.push((to, edges(f, to), 0));
                }
            }
        }
    }
    Ok(())
}

fn frame_maps(f: &KirFunction, maps: &[FrameMap]) -> Result<(), KirError> {
    for (at, pair) in maps.windows(2).enumerate() {
        if pair[0].boundary >= pair[1].boundary {
            return Err(KirError::MapOrder { at: at + 1 });
        }
    }
    for (ix, _) in f.boundaries.iter().enumerate() {
        let b = BoundaryIx(ix as u32);
        if maps.get(ix).map(|map| map.boundary) != Some(b) {
            return Err(KirError::MissingMap(b));
        }
    }
    if let Some(extra) = maps.get(f.boundaries.len()) {
        return Err(KirError::OutOfRange {
            at: At::Map(extra.boundary),
            operand: Operand::Boundary(extra.boundary),
        });
    }
    for map in maps {
        let b = map.boundary;
        let at = At::Map(b);
        let loc = |loc: Loc| match loc {
            Loc::Reg(reg) => check(f, at, Operand::Reg(reg)),
            Loc::Const(c) => check(f, at, Operand::Const(c)),
            Loc::Canonical | Loc::Spill(_) | Loc::Empty => Ok(()),
        };
        for (slot, value) in &map.slots {
            if slot.ix() >= f.slots.len() {
                return Err(KirError::MapSlot {
                    boundary: b,
                    slot: *slot,
                });
            }
            loc(*value)?;
        }
        let shape = &f.boundaries[b.ix()].shape.frames;
        for (lp, state) in &map.loops {
            let held = shape.iter().any(|ctl| {
                matches!(*ctl, CtlStatic::For { lp: held } | CtlStatic::While { lp: held } if held == *lp)
            });
            if !held {
                return Err(KirError::MapControl(b));
            }
            if let LoopLoc::Regs { position, started } = *state {
                loc(position)?;
                loc(started)?;
            }
        }
        for (t, departure) in &map.finally {
            let held = shape.iter().any(|ctl| {
                *ctl == CtlStatic::Try {
                    t: *t,
                    phase: TryPhaseStatic::Finally,
                }
            });
            if !held {
                return Err(KirError::MapControl(b));
            }
            if let DepartLoc::Return(value) | DepartLoc::Throw(value) = *departure {
                loc(value)?;
            }
        }
    }
    Ok(())
}
