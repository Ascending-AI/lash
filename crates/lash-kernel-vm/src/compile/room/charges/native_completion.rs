//! The stream's fixed closes are already paid at a precise native commit.
//! Only its original completion formula remains unpaid.
use super::*;

pub(super) fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
) -> Result<(Vec<Close>, Vec<Formula>, Point), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if range.len != 1 || range.start < stmt.ops.start || range.end() > stmt.ops.end() {
        return Err(Refused::Source);
    }
    // Verify the original source/physical graph, even though this coverage
    // deliberately contains none of its already-completed fixed closes.
    canonical_span(exe, code, statement, stmt.ops)?;
    let op = *stream
        .ops
        .get(range.start as usize)
        .ok_or(Refused::Source)?;
    let (lib, args) = match op {
        Op::Call { lib, args } | Op::Operator { lib, args } => (LibId(lib.0), args),
        _ => return Err(Refused::Source),
    };
    let function = exe.lib(lib);
    if !matches!(function.run, compile::LibRun::Native { .. })
        || function.specialization.resource
            != super::super::CallSource::Native(function.specialization.function)
    {
        return Err(Refused::Source);
    }
    let site = stream
        .source_sites
        .get(range.start as usize)
        .and_then(Option::as_deref)
        .ok_or(Refused::Source)?;
    let at = source_point(stream, site, op);
    if at.phase != super::super::Phase::Commit {
        return Err(Refused::Source);
    }
    let mut formulas = Vec::new();
    let mut tape = Vec::new();
    completion(
        exe,
        lib,
        args.len as usize,
        at.clone(),
        &mut formulas,
        &mut tape,
        exe.code(code).charging,
    )?;
    Ok((
        vec![Close {
            at: at.clone(),
            regions: Box::new([]),
            tape: tape.into(),
        }],
        formulas,
        at,
    ))
}

impl ChargeProgram {
    pub(crate) fn native_completion(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        pc: u32,
    ) -> Result<Self, Refused> {
        let range = OpRange { start: pc, len: 1 };
        let (closes, formulas, at) = derive(exe, code, statement, range)?;
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::NativeCompletion,
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
}
