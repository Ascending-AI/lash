//! A constructor authenticates the fixed work still open at its source event.
//! It must not settle the original close before its unexecuted suffix. The
//! downstream native/write/return remains the single owner of that close.
use super::*;

pub(in crate::compile::room::charges) fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    unpaid: u32,
    quote: u64,
) -> Result<(Vec<Close>, Vec<Formula>, Point), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    let canonical = match stmt.form {
        compile::Form::While { test, .. }
            if range.start >= test.start && range.end() <= test.end() =>
        {
            test
        }
        _ => stmt.ops,
    };
    if range.len != 1 || range.start < canonical.start || range.end() > canonical.end() {
        return Err(Refused::Source);
    }
    canonical_span(exe, code, statement, canonical)?;
    let prefix =
        expr::ConstructorPrefix::compile(stream, canonical, range.start).ok_or(Refused::Source)?;
    if prefix.unpaid() != unpaid || expr::units(prefix.pending()) != quote {
        return Err(Refused::Exit);
    }
    let site = stream
        .source_sites
        .get(range.start as usize)
        .and_then(Option::as_deref)
        .ok_or(Refused::Source)?;
    let at = source_point(stream, site, stream.ops[range.start as usize]);
    if at.phase != crate::compile::room::Phase::Commit {
        return Err(Refused::Source);
    }
    Ok((
        vec![Close {
            at: at.clone(),
            regions: prefix.pending().into(),
            tape: Box::new([]),
        }],
        vec![],
        at,
    ))
}

impl ChargeProgram {
    pub(crate) fn constructor_packet(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        prefix: &expr::ConstructorPrefix,
    ) -> Result<Self, Refused> {
        let range = OpRange {
            start: prefix.pc(),
            len: 1,
        };
        let (closes, formulas, at) = derive(
            exe,
            code,
            statement,
            range,
            prefix.unpaid(),
            expr::units(prefix.pending()),
        )?;
        if closes[0].regions.as_ref() != prefix.pending() {
            return Err(Refused::Exit);
        }
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::ConstructorPacket {
                unpaid: prefix.unpaid(),
                quote: expr::units(prefix.pending()),
            },
            native_depth: Some(0),
            native_occurrence: None,
            statement,
            range,
            closes: closes.into(),
            formulas: formulas.into(),
            fixed: 0,
            recording: exe.charging(),
            first: at.clone(),
            through: at,
        };
        program.verify(exe, code)?;
        Ok(program)
    }

    /// Pending fixed work is an admission guard. Its ordered events remain
    /// unpaid until the original close, including when this operation publishes.
    #[cfg(test)]
    pub(crate) fn constructor_quote(&self) -> Option<u64> {
        match self.coverage {
            Coverage::ConstructorPacket { quote, .. } => Some(quote),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn corrupt_constructor_close_for_law(&mut self, duplicate: bool) {
        let close = &mut self.closes[0];
        let mut pending = close.regions.to_vec();
        if duplicate {
            pending.push(pending[0]);
        } else {
            pending.remove(0);
        }
        close.regions = pending.into();
    }
}
