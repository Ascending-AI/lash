//! Header prices come from the canonical loop and its kernel cost table.
use super::*;
pub(super) fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    ending: bool,
) -> Result<(Vec<Close>, Vec<Formula>, Point), Refused> {
    let source = &exe.code(code).source_room;
    source.verify()?;
    let stmt = exe.stmt(statement);
    if exe.stmt_at(code, &stmt.site) != Some(statement) {
        return Err(Refused::Source);
    }
    let kernel = match source.owner {
        doc::constant_pool::PoolOwner::Function(function) => {
            exe.lib(exe.library_lib(&function).ok_or(Refused::Source)?)
                .header
                .kernel
        }
        doc::constant_pool::PoolOwner::Document(_) => exe.document.kernel(),
    };
    let cost =
        crate::costs::Costs::of(doc::KernelVersion::of(kernel).ok_or(Refused::Source)?).loop_test;
    let metered = match (source.node_at(&stmt.site), &stmt.form) {
        (
            Some(doc::Node::Stmt(doc::Stmt::For { metered, .. })),
            compile::Form::For {
                metered: actual, ..
            },
        )
        | (
            Some(doc::Node::Stmt(doc::Stmt::While { metered, .. })),
            compile::Form::While {
                metered: actual, ..
            },
        ) if metered == actual => *metered,
        _ => return Err(Refused::Source),
    };
    let at = Point {
        site: stmt.site.clone(),
        phase: if ending {
            super::super::Phase::Commit
        } else {
            super::super::Phase::Test
        },
    };
    let mut tape = Vec::new();
    if exe.code(code).charged && cost != 0 {
        tape.push(TapeEvent::Fixed(Weight::Atomic(cost)));
    }
    if metered != 0 {
        tape.push(TapeEvent::Fixed(Weight::Atomic(metered)));
    }
    Ok((
        vec![Close {
            at: at.clone(),
            regions: Box::default(),
            tape: tape.into(),
        }],
        vec![],
        at,
    ))
}
impl ChargeProgram {
    pub(in crate::compile::room) fn loop_header(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        ending: bool,
    ) -> Result<Arc<Self>, Refused> {
        let (closes, formulas, first) = derive(exe, code, statement, ending)?;
        let fixed = closes
            .iter()
            .flat_map(|c| &c.tape)
            .try_fold(0_u64, |sum, e| match e {
                TapeEvent::Fixed(w) => sum.checked_add(w.units()),
                _ => None,
            })
            .ok_or(Refused::Facts)?;
        let program = Self {
            source: Arc::clone(&exe.code(code).source_room),
            coverage: Coverage::LoopHeader(ending),
            native_depth: None,
            statement,
            range: OpRange {
                start: exe.stmt(statement).ops.start,
                len: 0,
            },
            closes: closes.into(),
            formulas: formulas.into(),
            fixed,
            recording: exe.charging(),
            first: first.clone(),
            through: first,
        };
        program.verify(exe, code)?;
        Ok(Arc::new(program))
    }
}
