//! A packet event may still owe its original fixed region close.
use super::*;
pub(super) mod constructor;
pub(in crate::compile::room) mod prefix;

pub(super) fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    pending: Option<expr::Close>,
) -> Result<(Vec<Close>, Vec<Formula>, Point), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if range.len != 1 || range.start < stmt.ops.start || range.end() > stmt.ops.end() {
        return Err(Refused::Source);
    }
    let canonical = match stmt.form {
        compile::Form::While { test, .. }
            if range.start >= test.start && range.end() <= test.end() =>
        {
            test
        }
        _ => stmt.ops,
    };
    canonical_span(exe, code, statement, canonical)?;
    let op = stream.ops[range.start as usize];
    if !matches!(
        op,
        Op::WriteLocal { .. }
            | Op::Bind { .. }
            | Op::BindSession { .. }
            | Op::WriteSession { .. }
            | Op::BindShared { .. }
            | Op::LetBind { .. }
            | Op::LetBindShared { .. }
            | Op::SetIndex { .. }
            | Op::RemoveIndex { .. }
            | Op::SetField { .. }
            | Op::RemoveField { .. }
            | Op::MakeList { .. }
            | Op::MakeTuple { .. }
            | Op::MakeMap { .. }
            | Op::MakeSet { .. }
            | Op::MakeRecord { .. }
            | Op::Closure { .. }
            | Op::Const { .. }
    ) {
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
    let regions = pending_regions(stream, canonical, range.start, pending, true)?;
    let tape = if exe.code(code).charged {
        regions
            .iter()
            .flat_map(|close| {
                stream
                    .regions
                    .events(close.region)
                    .iter()
                    .map(|event| TapeEvent::Fixed(event.weight))
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok((
        vec![Close {
            at: at.clone(),
            regions: regions.into(),
            tape: tape.into(),
        }],
        vec![],
        at,
    ))
}

pub(super) fn pending_regions(
    stream: &compile::Stream,
    canonical: OpRange,
    pc: u32,
    pending: Option<expr::Close>,
    last_only: bool,
) -> Result<Vec<expr::Close>, Refused> {
    let mut regions = Vec::new();
    for index in canonical.start..pc {
        match stream.ops[index as usize] {
            Op::Region { region, total } => {
                let close = expr::Close {
                    region: region.0,
                    head: index,
                    total,
                };
                compile::islands::verify_close(Some(&close), stream).map_err(|_| Refused::Exit)?;
                regions.push(close);
            }
            op if !op.kind().in_region() => regions.clear(),
            _ => {}
        }
    }
    // Ordinary event sites retain only their current close. Earlier operand
    // closes have already completed in the packet prefix.
    if last_only && regions.len() > 1 {
        regions.drain(..regions.len() - 1);
    }
    if regions.as_slice() != pending.as_slice() {
        return Err(Refused::Exit);
    }
    Ok(regions)
}

impl ChargeProgram {
    pub(crate) fn event_packet(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        pc: u32,
        pending: Option<expr::Close>,
    ) -> Result<Self, Refused> {
        let range = OpRange { start: pc, len: 1 };
        let (closes, formulas, at) = derive(exe, code, statement, range, pending)?;
        let fixed = closes
            .iter()
            .flat_map(|close| close.tape.iter())
            .try_fold(0_u64, |sum, event| match event {
                TapeEvent::Fixed(weight) => sum.checked_add(weight.units()),
                TapeEvent::Formula(_) => None,
            })
            .ok_or(Refused::Facts)?;
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::EventPacket(pending),
            native_depth: Some(0),
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
