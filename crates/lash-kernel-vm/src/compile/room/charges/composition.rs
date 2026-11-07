//! A statement's original charge program, admitted as one statement debt.
use super::*;
use crate::compile::CodeId;

/// One statement's debt under its original charge program.
pub(crate) enum StatementDebt<'a> {
    Scalar(ChargeDebt<'a>),
}
impl StatementDebt<'_> {
    pub(crate) fn units(&self) -> Option<u64> {
        match self {
            Self::Scalar(debt) => Some(debt.units()),
        }
    }
    pub(crate) fn matches(&self, source: &Arc<Source>, first: &Point, through: &Point) -> bool {
        match self {
            Self::Scalar(debt) => debt.matches(source, first, through),
        }
    }
    pub(crate) fn record(&self, tape: &crate::observe::ChargeTape) {
        match self {
            Self::Scalar(debt) => debt.record(tape),
        }
    }
}
impl<'a> crate::admission::SourceResources<'a> {
    pub(crate) fn admit_statement<C>(
        self,
        debt: &StatementDebt<'_>,
        occurrences: &[usize],
        depth: u64,
        request: C,
    ) -> Result<Option<crate::admission::SourceAccepted<'a, C>>, doc::heap::HeapFault> {
        let Some(units) = debt.units() else {
            return Ok(None);
        };
        let need = crate::admission::Allowance {
            occurrences: occurrences.to_vec(),
            before: 0,
            units,
            close: None,
            strict: true,
            depth,
            safe_point: self.safe_point,
        };
        let Some(facts) = self.facts(&need) else {
            return Ok(None);
        };
        self.admit_interval(&facts, &need, request)
    }
}

/// Statement and branch order comes from the defining body's compiled blocks.
/// Every selected test and returned statement keeps its original charge span.
fn body_programs(
    exe: &Executable,
    code: CodeId,
    block: crate::compile::BlockId,
    selected: &[super::super::Selection],
    programs: &mut Vec<ChargeProgram>,
) -> Result<bool, Refused> {
    for &statement in &exe.block(block).stmts {
        let (stream, stmt, _) = exe.statement(statement);
        programs.push(ChargeProgram::compile(exe, code, statement, stmt.ops)?);
        match stmt.form {
            crate::compile::Form::If {
                then_block,
                else_block,
            } => {
                let taken = selected
                    .iter()
                    .find_map(|selection| match selection {
                        super::super::Selection::Branch { at, taken } if *at == stmt.site => {
                            Some(*taken)
                        }
                        _ => None,
                    })
                    .ok_or(Refused::Path)?;
                if body_programs(
                    exe,
                    code,
                    if taken { then_block } else { else_block },
                    selected,
                    programs,
                )? {
                    return Ok(true);
                }
            }
            crate::compile::Form::Plain | crate::compile::Form::Action(_) => {}
            _ => return Err(Refused::Source),
        }
        if matches!(stream.ops[stmt.ops.end() as usize - 1], Op::Return { .. }) {
            return Ok(true);
        }
    }
    Ok(false)
}
