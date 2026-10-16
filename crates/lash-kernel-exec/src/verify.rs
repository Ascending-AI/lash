//! The coverage invariants, checked over lowered streams.
//!
//! - [`verify_semantics`]: the semantics table states every op exactly
//!   once, and every dynamic charge event has an op that emits it.
//! - [`verify_stream`]: every statement's ops are well formed and no op
//!   spans a Boundary: a range ends at its one terminator, an action is the
//!   first op of its statement so that its continuation starts right after
//!   it, and every register a range reads was written earlier in it, so no
//!   register is live at a Boundary (`K-STMT-004`).

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
    /// The nested-action table's length.
    pub nested: usize,
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
}

/// Checks every statement of a stream (see the module documentation),
/// each with the frame size of the code it belongs to.
pub fn verify_stream(
    ops: &[Op],
    stmts: &[(StmtRanges, usize)],
    tables: &Tables,
) -> Result<(), KirError> {
    for (stmt, (ranges, frame)) in stmts.iter().enumerate() {
        range(ops, stmt, ranges.main, *ranges, *frame, tables, false)?;
        if let Some(test) = ranges.test {
            range(ops, stmt, test, *ranges, *frame, tables, true)?;
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
    let last = range.end() - 1;
    for at in range.start..range.end() {
        let op = &ops[at as usize];
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
        Operand::Nested(nested) => nested.ix() < tables.nested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kir::ConstIx;
    use crate::sem::SEMANTICS;

    /// The semantics table states every op once, and every dynamic charge
    /// event has an op that emits it.
    #[test]
    fn the_semantics_table_covers_every_op_and_event() {
        assert_eq!(verify_semantics(SEMANTICS), Ok(()));
    }

    /// No range spans a Boundary (`K-STMT-004`): a step ends at its
    /// statement's one terminator, an action's continuation starts right
    /// after it, and no register is read that its range did not write.
    #[test]
    fn a_range_that_spans_a_boundary_is_rejected() {
        let tables = Tables {
            consts: 1,
            ..Tables::default()
        };
        let c = |dst| Op::Const {
            dst: Reg(dst),
            c: ConstIx(0),
        };
        let check = |ops: &[Op], len: u32| {
            let main = OpRange { start: 0, len };
            let ranges = StmtRanges {
                main,
                test: None,
                regs: 2,
            };
            verify_stream(ops, &[(ranges, 0)], &tables)
        };
        assert_eq!(check(&[c(0), Op::End], 2), Ok(()));
        assert_eq!(
            check(&[c(0), Op::End, c(1), Op::End], 4),
            Err(KirError::EarlyTerminator { stmt: 0, op: 1 })
        );
        assert_eq!(
            check(&[c(0), c(1)], 2),
            Err(KirError::MissingTerminator { stmt: 0, op: 1 })
        );
        assert_eq!(
            check(&[c(0), Op::Act, Op::End], 3),
            Err(KirError::ActionNotFirst { stmt: 0, op: 1 })
        );
        assert_eq!(
            check(&[Op::Print { src: Reg(1) }], 1),
            Err(KirError::Unwritten {
                stmt: 0,
                op: 0,
                reg: Reg(1)
            })
        );
        assert_eq!(
            check(&[c(0), Op::Test { cond: Reg(0) }], 2),
            Err(KirError::WrongTerminator { stmt: 0, op: 1 })
        );
    }
}
