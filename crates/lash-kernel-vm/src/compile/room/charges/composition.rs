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
    ) -> Result<Self, Refused> {
        let pc = range.end().checked_sub(1).ok_or(Refused::Path)?;
        if super::super::region::calls::resolved(exe, caller, statement, pc)?.code != callee {
            return Err(Refused::Source);
        }
        let caller_program = ChargeProgram::compile(exe, caller, statement, range)?;
        let body = exe.code(callee);
        let mut programs = Vec::new();
        for &statement in &exe.block(body.body).stmts {
            let (stream, stmt, _) = exe.statement(statement);
            programs.push(ChargeProgram::compile(exe, callee, statement, stmt.ops)?);
            if matches!(stream.ops[stmt.ops.end() as usize - 1], Op::Return { .. }) {
                break;
            }
        }
        Ok(Self {
            caller: caller_program,
            body: programs.into(),
            caller_code: caller,
            callee_code: callee,
            callee: Arc::clone(&body.source_room),
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
