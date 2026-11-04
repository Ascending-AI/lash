//! Header prices come from the verified, immutable source loop plan.
use super::*;
use crate::compile::islands::{Block, Kind, Statement};

fn find(block: &Block, id: compile::StmtId) -> Option<&Statement> {
    for statement in &block.statements {
        if statement.id == id {
            return Some(statement);
        }
        let nested = match &statement.kind {
            Kind::While {
                body, otherwise, ..
            }
            | Kind::For {
                body, otherwise, ..
            } => find(body, id).or_else(|| otherwise.as_ref().and_then(|b| find(b, id))),
            Kind::If { then_, else_, .. } => find(then_, id).or_else(|| find(else_, id)),
            Kind::Try {
                body,
                catch,
                finally,
            } => find(body, id)
                .or_else(|| catch.as_ref().and_then(|b| find(b, id)))
                .or_else(|| finally.as_ref().and_then(|b| find(b, id))),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}
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
    let plan = exe
        .island_plan(exe.island_of(code).ok_or(Refused::Source)?)
        .ok_or(Refused::Source)?;
    let statement = find(&plan.body, statement).ok_or(Refused::Source)?;
    let (cost, metered) = match statement.kind {
        Kind::For { cost, metered, .. } | Kind::While { cost, metered, .. } => (cost, metered),
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
            native_occurrence: None,
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
