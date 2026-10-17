//! The coverage invariants, checked over lowered streams.
//!
//! - [`verify_semantics`]: the semantics table states every op exactly
//!   once, and every dynamic charge event has an op that emits it.
//! - [`verify_stream`]: every statement's ops are well formed and no op
//!   spans a Boundary: a range ends at its one terminator, a statement's
//!   action is the first op of its statement so that its continuation
//!   starts right after it, and every register a range reads was written
//!   earlier in it. A nested action is a Boundary inside its range; what
//!   its continuation reads, its resume entry loads (`K-EVAL-008`). Every
//!   charge region is headed by its own [`Op::Region`], whose total is the
//!   one its verified summary states (`RegionSummary::verify`), holds only
//!   ops that stand in a region, and states its events at ops from after
//!   its head to the op that closes it.

use crate::charge::RegionTable;
use crate::kir::{Op, OpKind, OpRange, Operand, Reg};
use crate::sem::{ChargeEvent, OpSpec};

/// Why a semantics table does not cover the ops and the runtime table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverageError {
    #[error("no semantics states op {0:?}")]
    MissingOp(OpKind),
    #[error("entry {at} of the semantics states op {op:?} out of order or twice")]
    Misplaced { at: usize, op: OpKind },
    #[error("no op's semantics emits dynamic charge event {0:?}")]
    UnemittedEvent(ChargeEvent),
}

/// Checks a semantics table against [`OpKind::ALL`] and
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
    if let Some(event) = ChargeEvent::ALL
        .iter()
        .filter(|event| !event.is_fixed())
        .find(|event| !specs.iter().any(|spec| spec.events.contains(event)))
    {
        return Err(CoverageError::UnemittedEvent(*event));
    }
    Ok(())
}

/// One statement's ops in a stream: the range a step runs from its start,
/// a `while`'s condition, and how many registers the statement uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StmtRanges {
    pub main: OpRange,
    pub test: Option<OpRange>,
    pub regs: u32,
}

/// The sizes of the tables a stream's ops index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tables {
    pub consts: usize,
    pub names: usize,
    pub fields: usize,
    /// Each field list's length.
    pub field_lists: Vec<usize>,
    pub codes: usize,
    pub libs: usize,
    pub blocks: usize,
    /// The table of nested actions' resume entries.
    pub nested: usize,
    /// The charge regions' side table.
    pub regions: usize,
    /// The property read sites, in all.
    pub sites: usize,
}

/// Why a stream is rejected: the statement, by its position in the
/// statements checked, the op, by its position in the stream, and what is
/// wrong there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KirError {
    #[error("statement {stmt}'s range {range:?} is empty or past the stream")]
    Range { stmt: usize, range: OpRange },
    #[error("statement {stmt}'s range ends at op {op}, which ends no range")]
    MissingTerminator { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} ends its range before the range's end")]
    EarlyTerminator { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} is the wrong terminator for its range")]
    WrongTerminator { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} is an action that is not its statement's first op")]
    ActionNotFirst { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} names {operand:?}, past its table")]
    OutOfRange {
        stmt: usize,
        op: u32,
        operand: Operand,
    },
    #[error("op {op} of statement {stmt} reads register {reg:?}, which its range has not written")]
    Unwritten { stmt: usize, op: u32, reg: Reg },
    #[error("op {op} of statement {stmt} has operands of the wrong count")]
    Arity { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} jumps outside its expression range")]
    InvalidJump { stmt: usize, op: u32 },
    #[error("op {op} of statement {stmt} heads a charge region that is not headed there")]
    RegionHead { stmt: usize, op: u32 },
    #[error("a charge event of the region op {op} of statement {stmt} heads stands outside it")]
    RegionEvent { stmt: usize, op: u32 },
    #[error("the region op {op} of statement {stmt} heads is refused: {error}")]
    Region {
        stmt: usize,
        op: u32,
        error: crate::charge::RegionError,
    },
    #[error(
        "op {op} of statement {stmt} states a region total of {stated}; its summary's is {summary}"
    )]
    RegionTotal {
        stmt: usize,
        op: u32,
        stated: u32,
        summary: u64,
    },
}

/// Checks every statement of a stream (see the module documentation),
/// each with the frame size of the code it belongs to.
pub fn verify_stream(
    ops: &[Op],
    stmts: &[(StmtRanges, usize)],
    tables: &Tables,
    regions: &RegionTable,
) -> Result<(), KirError> {
    let range = |stmt, range, ranges, frame, test| {
        self::range(ops, stmt, range, ranges, frame, tables, test)?;
        self::regions(ops, stmt, range, regions)
    };
    for (stmt, (ranges, frame)) in stmts.iter().enumerate() {
        range(stmt, ranges.main, *ranges, *frame, false)?;
        if let Some(test) = ranges.test {
            range(stmt, test, *ranges, *frame, true)?;
        }
    }
    Ok(())
}

/// Checks every charge region headed in `range`: its head is its own and
/// states the total of its verified summary, its ops stand in a region,
/// and its events stand from after its head to the op that closes it.
fn regions(ops: &[Op], stmt: usize, range: OpRange, regions: &RegionTable) -> Result<(), KirError> {
    for at in range.start..range.end() {
        let Op::Region { region, total } = ops[at as usize] else {
            continue;
        };
        let region = region.0;
        if region as usize >= regions.len() || regions.head(region) != at {
            return Err(KirError::RegionHead { stmt, op: at });
        }
        let refused = |error| KirError::Region {
            stmt,
            op: at,
            error,
        };
        let summary = regions.summary(region).map_err(refused)?;
        summary.verify(regions.events(region)).map_err(refused)?;
        if total != u32::try_from(summary.total()).unwrap_or(u32::MAX) {
            return Err(KirError::RegionTotal {
                stmt,
                op: at,
                stated: total,
                summary: summary.total(),
            });
        }
        let close = (at + 1..range.end())
            .find(|op| !ops[*op as usize].kind().in_region())
            .unwrap_or(range.end());
        if regions.at(region).iter().any(|event| *event > close) {
            return Err(KirError::RegionEvent { stmt, op: at });
        }
    }
    Ok(())
}

fn range(
    ops: &[Op],
    stmt: usize,
    range: OpRange,
    ranges: StmtRanges,
    frame: usize,
    tables: &Tables,
    test: bool,
) -> Result<(), KirError> {
    if range.len == 0 || range.end() as usize > ops.len() {
        return Err(KirError::Range { stmt, range });
    }
    let mut written = vec![false; ranges.regs as usize];
    let mut fallthrough = true;
    let mut incoming = vec![None::<Vec<bool>>; range.len as usize];
    let last = range.end() - 1;
    for at in range.start..range.end() {
        if let Some(other) = incoming[(at - range.start) as usize].take() {
            if fallthrough {
                for (slot, other) in written.iter_mut().zip(other) {
                    *slot &= other;
                }
            } else {
                written = other;
            }
        }
        let op = &ops[at as usize];
        fallthrough = !matches!(op, Op::Jump { .. });
        let kind = op.kind();
        if kind.is_terminator() && at != last {
            return Err(KirError::EarlyTerminator { stmt, op: at });
        }
        if at == last {
            if !kind.is_terminator() {
                return Err(KirError::MissingTerminator { stmt, op: at });
            }
            if test != (kind == OpKind::Test) {
                return Err(KirError::WrongTerminator { stmt, op: at });
            }
        }
        if kind == OpKind::Act && (test || at != range.start) {
            return Err(KirError::ActionNotFirst { stmt, op: at });
        }
        for operand in op.operands() {
            if !fits(operand, ranges.regs, frame, tables) {
                return Err(KirError::OutOfRange {
                    stmt,
                    op: at,
                    operand,
                });
            }
        }
        if let Op::Jump { skip } | Op::JumpIfFalse { skip, .. } = *op {
            let Some(target) = at
                .checked_add(1)
                .and_then(|next| next.checked_add(skip))
                .filter(|target| *target <= last)
            else {
                return Err(KirError::InvalidJump { stmt, op: at });
            };
            let state = &mut incoming[(target - range.start) as usize];
            if let Some(state) = state {
                for (slot, value) in state.iter_mut().zip(&written) {
                    *slot &= value;
                }
            } else {
                *state = Some(written.clone());
            }
        }
        let arity = match *op {
            Op::MakeMap { entries } => entries.len % 2 == 0,
            Op::MakeRecord { fields, values } => {
                tables.field_lists.get(fields.ix()).copied() == Some(values.len as usize)
            }
            _ => true,
        };
        if !arity {
            return Err(KirError::Arity { stmt, op: at });
        }
        for reg in op.reads() {
            if !written.get(reg.ix()).copied().unwrap_or(false) {
                return Err(KirError::Unwritten { stmt, op: at, reg });
            }
        }
        if let Some(reg) = op.writes()
            && let Some(slot) = written.get_mut(reg.ix())
        {
            *slot = true;
        }
    }
    Ok(())
}

fn fits(operand: Operand, regs: u32, frame: usize, tables: &Tables) -> bool {
    match operand {
        Operand::Reg(reg) => reg.0 < regs,
        Operand::Regs(range) => u64::from(range.start.0) + u64::from(range.len) <= u64::from(regs),
        Operand::Local(local) => (local.at as usize) < frame && (local.slot as usize) < frame,
        Operand::Const(c) => c.ix() < tables.consts,
        Operand::Name(name) => name.ix() < tables.names,
        Operand::Field(field) => field.ix() < tables.fields,
        Operand::FieldList(list) => list.ix() < tables.field_lists.len(),
        Operand::Code(code) => code.ix() < tables.codes,
        Operand::Lib(lib) => lib.ix() < tables.libs,
        Operand::Block(block) => block.ix() < tables.blocks,
        Operand::Nested(act) => act.ix() < tables.nested,
        Operand::Region(region) => region.ix() < tables.regions,
        Operand::Site(site) => site.ix() < tables.sites,
    }
}
