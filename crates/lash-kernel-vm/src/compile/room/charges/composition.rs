//! A call closes its caller's original region before running the body, then
//! bills the canonical completion formula after the callee's final Leave.
use super::*;
use crate::compile::{CodeId, StmtId};

pub(crate) struct ComposedCharges {
    caller: ChargeProgram,
    body: Box<[ChargeProgram]>,
    caller_code: CodeId,
    callee_code: CodeId,
    callee: Arc<Source>,
    selected: Box<[super::super::Selection]>,
}

/// This borrows the verified source schedule. No consumer supplies a price or
/// edits the order of its fixed closes and completion formulas.
pub(crate) struct ComposedDebt<'a> {
    program: &'a ComposedCharges,
    caller: ChargeDebt<'a>,
    body: Vec<ChargeDebt<'a>>,
    units: u64,
}

impl ComposedCharges {
    pub(crate) fn compile(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        range: OpRange,
        callee: CodeId,
        selected: &[super::super::Selection],
    ) -> Result<Self, Refused> {
        let pc = range.end().checked_sub(1).ok_or(Refused::Path)?;
        if super::super::region::calls::resolved(exe, caller, statement, pc)?.code != callee {
            return Err(Refused::Source);
        }
        let caller_program = ChargeProgram::compile(exe, caller, statement, range)?;
        let body = exe.code(callee);
        let mut programs = Vec::new();
        body_programs(exe, callee, body.body, selected, &mut programs)?;
        Ok(Self {
            caller: caller_program,
            body: programs.into(),
            caller_code: caller,
            callee_code: callee,
            callee: Arc::clone(&body.source_room),
            selected: selected.into(),
        })
    }

    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        self.caller.verify(exe, self.caller_code)?;
        let expected = Self::compile(
            exe,
            self.caller_code,
            self.caller.statement,
            self.caller.range,
            self.callee_code,
            &self.selected,
        )?;
        if !Arc::ptr_eq(&self.callee, &exe.code(self.callee_code).source_room)
            || expected.body.len() != self.body.len()
        {
            return Err(Refused::Source);
        }
        for (actual, expected) in self.body.iter().zip(&expected.body) {
            if actual.statement != expected.statement || actual.range != expected.range {
                return Err(Refused::Exit);
            }
            actual.verify(exe, self.callee_code)?;
        }
        Ok(())
    }

    pub(in crate::compile::room) fn covers(
        &self,
        code: CodeId,
        statement: StmtId,
        first: u32,
        pc: u32,
    ) -> bool {
        self.caller_code == code
            && self.caller.statement == statement
            && self.caller.range.start == first
            && self.caller.range.end().checked_sub(1) == Some(pc)
    }
    pub(crate) fn caller(&self) -> &ChargeProgram {
        &self.caller
    }
    pub(crate) fn body(&self) -> &[ChargeProgram] {
        &self.body
    }

    pub(crate) fn bind<'a>(
        &'a self,
        caller: &[FormulaValues<'_>],
        body: &[Vec<FormulaValues<'_>>],
    ) -> Option<ComposedDebt<'a>> {
        if body.len() != self.body.len() {
            return None;
        }
        let caller = self.caller.bind(caller)?;
        let body: Vec<_> = self
            .body
            .iter()
            .zip(body)
            .map(|(program, values)| program.bind(values))
            .collect::<Option<_>>()?;
        let units = body
            .iter()
            .try_fold(caller.units(), |sum, debt| sum.checked_add(debt.units()))?;
        Some(ComposedDebt {
            program: self,
            caller,
            body,
            units,
        })
    }

    #[cfg(test)]
    pub(crate) fn reorder_for_law(&mut self) {
        self.body.swap(0, 1);
    }
}

impl ComposedDebt<'_> {
    pub(crate) fn units(&self) -> u64 {
        self.units
    }

    pub(crate) fn record(&self, tape: &crate::observe::ChargeTape) {
        let caller = &self.program.caller;
        if !caller.recording {
            return;
        }
        for (index, close) in caller.closes.iter().enumerate() {
            let last = index + 1 == caller.closes.len();
            // Region events charge before the call. Only the canonical
            // completion formula follows its body, even when the body has
            // no charge of its own under K-CHG-007.
            for event in &close.tape {
                if last && matches!(event, TapeEvent::Formula(_)) {
                    continue;
                }
                record_event(event, &self.caller, tape);
            }
            if last {
                for debt in &self.body {
                    debt.record(tape);
                }
                for event in &close.tape {
                    if matches!(event, TapeEvent::Formula(_)) {
                        record_event(event, &self.caller, tape);
                    }
                }
            }
        }
    }
}

fn record_event(event: &TapeEvent, debt: &ChargeDebt<'_>, tape: &crate::observe::ChargeTape) {
    match *event {
        TapeEvent::Fixed(weight) => tape.push(weight),
        TapeEvent::Formula(index) => {
            let units = debt.formulas[index];
            if units != 0 {
                tape.push(Weight::Atomic(units));
            }
        }
    }
}

impl<'a> crate::admission::SourceResources<'a> {
    pub(crate) fn admit_composed<C>(
        self,
        debt: &ComposedDebt<'_>,
        occurrences: &[usize],
        depth: u64,
        safe_point: u64,
        request: C,
    ) -> Result<Option<crate::admission::SourceAccepted<'a, C>>, doc::heap::HeapFault> {
        let need = crate::admission::Allowance {
            occurrences: occurrences.to_vec(),
            before: 0,
            units: debt.units(),
            close: None,
            strict: true,
            depth,
            safe_point,
        };
        let Some(facts) = self.facts(&need) else {
            return Ok(None);
        };
        self.admit_interval(&facts, &need, request)
    }
}

/// A complete statement may contain one verified call and its original caller
/// continuation. Both source schedules retain their canonical close identities.
pub(crate) enum StatementDebt<'a> {
    Scalar(ChargeDebt<'a>),
    PrefixCall {
        prefix: ChargeDebt<'a>,
        call: ComposedDebt<'a>,
        suffix: ChargeDebt<'a>,
    },
    Call {
        call: ComposedDebt<'a>,
        suffix: ChargeDebt<'a>,
    },
}
impl StatementDebt<'_> {
    pub(crate) fn units(&self) -> Option<u64> {
        match self {
            Self::Scalar(debt) => Some(debt.units()),
            Self::PrefixCall {
                prefix,
                call,
                suffix,
            } => prefix
                .units()
                .checked_add(call.units())?
                .checked_add(suffix.units()),
            Self::Call { call, suffix } => call.units().checked_add(suffix.units()),
        }
    }
    pub(crate) fn matches(
        &self,
        source: &Arc<Source>,
        first: &Point,
        through: &Point,
        call_point: Option<&Point>,
        callee: Option<&Arc<Source>>,
    ) -> bool {
        match self {
            Self::Scalar(debt) => {
                call_point.is_none() && callee.is_none() && debt.matches(source, first, through)
            }
            Self::PrefixCall {
                prefix,
                call,
                suffix,
            } => {
                prefix.matches(source, first, &prefix.program.through)
                    && Arc::ptr_eq(source, &call.program.caller.source)
                    && prefix.program.statement == call.program.caller.statement
                    && prefix.program.range.end() == call.program.caller.range.start
                    && call_point.is_some_and(|at| &call.program.caller.through == at)
                    && callee.is_some_and(|source| Arc::ptr_eq(source, &call.program.callee))
                    && suffix.matches(source, &suffix.program.first, through)
                    && suffix.program.statement == call.program.caller.statement
                    && call.program.caller.range.end() == suffix.program.range.start
            }
            Self::Call { call, suffix } => {
                call_point.is_some_and(|at| {
                    call.caller.matches(source, first, at)
                        && source
                            .next_after_commit(&at.site)
                            .ok()
                            .is_some_and(|next| suffix.matches(source, &next, through))
                }) && callee.is_some_and(|source| Arc::ptr_eq(source, &call.program.callee))
            }
        }
    }
    pub(crate) fn record(&self, tape: &crate::observe::ChargeTape) {
        match self {
            Self::Scalar(debt) => debt.record(tape),
            Self::PrefixCall {
                prefix,
                call,
                suffix,
            } => {
                prefix.record(tape);
                call.record(tape);
                suffix.record(tape);
            }
            Self::Call { call, suffix } => {
                call.record(tape);
                suffix.record(tape);
            }
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
            crate::compile::Form::Plain => {}
            _ => return Err(Refused::Source),
        }
        if matches!(stream.ops[stmt.ops.end() as usize - 1], Op::Return { .. }) {
            return Ok(true);
        }
    }
    Ok(false)
}
