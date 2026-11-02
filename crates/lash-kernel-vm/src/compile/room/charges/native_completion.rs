//! The stream's fixed closes are already paid at a precise native commit.
//! Only its original completion formula remains unpaid.
use super::*;

pub(super) fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    pending: Option<Option<expr::Close>>,
) -> Result<(Vec<Close>, Vec<Formula>, Point), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if range.len != 1 || range.start < stmt.ops.start || range.end() > stmt.ops.end() {
        return Err(Refused::Source);
    }
    // Verify the original source/physical graph, even though this coverage
    // deliberately contains none of its already-completed fixed closes.
    let canonical = match stmt.form {
        compile::Form::While { test, .. }
            if range.start >= test.start && range.end() <= test.end() =>
        {
            test
        }
        _ => stmt.ops,
    };
    canonical_span(exe, code, statement, canonical)?;
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
    let mut regions = Vec::new();
    if let Some(pending) = pending {
        regions =
            super::event_packet::pending_regions(stream, canonical, range.start, pending, false)?;
        if exe.code(code).charged {
            for close in &regions {
                tape.extend(
                    stream
                        .regions
                        .events(close.region)
                        .iter()
                        .map(|event| TapeEvent::Fixed(event.weight)),
                );
            }
        }
    }
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
            regions: regions.into(),
            tape: tape.into(),
        }],
        formulas,
        at,
    ))
}

pub(super) fn depth(
    exe: &Executable,
    statement: compile::StmtId,
    pc: u32,
    packet: bool,
) -> Result<u64, Refused> {
    let (stream, _, _) = exe.statement(statement);
    let lib = match stream.ops.get(pc as usize) {
        Some(Op::Call { lib, .. } | Op::Operator { lib, .. }) => LibId(lib.0),
        _ => return Err(Refused::Source),
    };
    let function = exe.lib(lib);
    let extra = if packet {
        function
            .specialization
            .prepared
            .as_ref()
            .map_or(0, |(entry, _)| entry.depth())
    } else {
        0
    };
    Ok(match (function.operation(), extra) {
        (true, 0 | 1) => 1,
        (true, extra) | (false, extra @ 1..) => u64::from(extra) + 1,
        (false, 0) => 0,
    })
}

impl ChargeProgram {
    pub(crate) fn native_completion(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        pc: u32,
    ) -> Result<Self, Refused> {
        let range = OpRange { start: pc, len: 1 };
        let (closes, formulas, at) = derive(exe, code, statement, range, None)?;
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::NativeCompletion,
            native_depth: Some(depth(exe, statement, pc, false)?),
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
    /// A speculative packet still owes its authenticated original close.
    /// Its debt bills that close before native completion, with strict slice
    /// admission. The supplied physical metadata must equal the source close.
    pub(crate) fn native_packet(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        pc: u32,
        pending: Option<expr::Close>,
    ) -> Result<Self, Refused> {
        let range = OpRange { start: pc, len: 1 };
        let (closes, formulas, at) = derive(exe, code, statement, range, Some(pending))?;
        let fixed = closes
            .iter()
            .flat_map(|close| &close.tape)
            .try_fold(0_u64, |sum, event| match event {
                TapeEvent::Fixed(weight) => sum.checked_add(weight.units()),
                TapeEvent::Formula(_) => Some(sum),
            })
            .ok_or(Refused::Facts)?;
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::NativePacket(pending),
            native_depth: Some(depth(exe, statement, pc, true)?),
            statement,
            range,
            closes: closes.into(),
            formulas: formulas.into(),
            fixed,
            recording: exe.charging(),
            first: at.clone(),
            through: at,
        };
        program.verify(exe, code)?;
        Ok(program)
    }
}
