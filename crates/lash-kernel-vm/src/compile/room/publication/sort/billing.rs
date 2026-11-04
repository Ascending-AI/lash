//! Canonical caller closes surround the debt owned by the actual call scope.
use super::*;
use crate::compile::room::charges::{ChargeDebt, ChargeProgram, FormulaValues};
use crate::compile::room::publication::boundary::{
    BoundaryNeeds, CompiledPublication,
    schedule::{StatementSite, StatementWork},
};
use lash_kernel_exec::OpRange;

pub(super) enum Piece {
    Scalar(Arc<ChargeProgram>),
    Call {
        prefix: Option<Arc<ChargeProgram>>,
        suffix: Option<Arc<ChargeProgram>>,
    },
}
pub(super) struct Billing {
    pub(super) plan: Arc<CompiledPublication>,
    pieces: Box<[Piece]>,
}
impl Billing {
    pub(super) fn compile(
        callback: &CallbackProgram,
        exe: &Executable,
        route: &caller::CallerRoute,
        branches: &[(Site, bool)],
        depth: u64,
    ) -> Result<Self, Refused> {
        let source = &exe.code(callback.code).source_room;
        let mut pieces = Vec::new();
        let mut sites = Vec::new();
        for event in route.interval.events() {
            if !matches!(event.point.phase, Phase::Commit | Phase::Test) {
                continue;
            }
            let Some(statement) = exe.stmt_at(callback.code, &event.point.site) else {
                continue;
            };
            let stmt = exe.stmt(statement);
            if event.point.phase == Phase::Commit
                && matches!(stmt.form, crate::compile::Form::If { .. })
            {
                continue;
            }
            if statement == callback.statement {
                let start = callback.pc.checked_add(1).ok_or(Refused::Path)?;
                let suffix = if start == stmt.ops.end() {
                    None
                } else {
                    Some(Arc::new(ChargeProgram::compile_callback(
                        exe,
                        callback.code,
                        statement,
                        OpRange {
                            start,
                            len: stmt.ops.end().checked_sub(start).ok_or(Refused::Path)?,
                        },
                        callback.call.callee(),
                    )?))
                };
                // The composed call debt owns the canonical statement/argument
                // prefix. The suffix starts after the actual call close.
                let prefix = None;
                sites.push(StatementSite::Call {
                    first: source.first_event(&stmt.site)?,
                    through: route.call_through.clone(),
                    call: Arc::clone(&callback.call),
                    continuation: route.continuation.clone(),
                    prefix: prefix.clone(),
                    suffix: suffix.clone(),
                });
                pieces.push(Piece::Call { prefix, suffix });
            } else {
                let range = match stmt.form {
                    crate::compile::Form::While { test, .. }
                        if event.point.phase == Phase::Test =>
                    {
                        test
                    }
                    _ => stmt.ops,
                };
                let program = Arc::new(ChargeProgram::compile(
                    exe,
                    callback.code,
                    statement,
                    range,
                )?);
                sites.push(StatementSite::Scalar(Arc::clone(&program)));
                pieces.push(Piece::Scalar(program));
            }
        }
        if pieces
            .iter()
            .filter(|piece| matches!(piece, Piece::Call { .. }))
            .count()
            != 1
        {
            return Err(Refused::Source);
        }
        let first = route
            .interval
            .events()
            .next()
            .ok_or(Refused::Path)?
            .point
            .clone();
        let through = route
            .interval
            .events()
            .last()
            .ok_or(Refused::Path)?
            .point
            .clone();
        let plan = CompiledPublication::compile_statements(
            exe,
            callback.code,
            first,
            through,
            Successor::Normal,
            sites,
            BoundaryNeeds {
                depth,
                ..Default::default()
            },
            branches,
        )?;
        Ok(Self {
            plan,
            pieces: pieces.into(),
        })
    }
    pub(super) fn bind<'plan>(
        &'plan self,
        formulas: &std::collections::BTreeMap<Point, (FunctionId, Vec<Value>, Value)>,
        arena: &Arena,
    ) -> Option<Vec<StatementWork<'plan>>> {
        self.pieces
            .iter()
            .map(|piece| match piece {
                Piece::Scalar(program) => {
                    Some(StatementWork::Scalar(bind(program, formulas, arena)?))
                }
                Piece::Call { prefix, suffix } => Some(StatementWork::Call {
                    prefix: match prefix {
                        Some(p) => Some(bind(p, formulas, arena)?),
                        None => None,
                    },
                    suffix: match suffix {
                        Some(program) => Some(bind(program, formulas, arena)?),
                        None => None,
                    },
                }),
            })
            .collect()
    }
}
fn bind<'plan>(
    program: &'plan ChargeProgram,
    formulas: &std::collections::BTreeMap<Point, (FunctionId, Vec<Value>, Value)>,
    arena: &Arena,
) -> Option<ChargeDebt<'plan>> {
    let calls: Vec<_> = program
        .formulas()
        .map(|(at, function, count)| {
            let call = formulas.get(at)?;
            (call.0 == function && call.1.len() == count).then_some(call)
        })
        .collect::<Option<_>>()?;
    let values: Vec<_> = calls
        .iter()
        .map(|call| FormulaValues {
            arguments: &call.1,
            result: call.2,
        })
        .collect();
    program.bind_measured(&values, |index, from, measure| {
        let call = calls[index];
        let value = match from {
            crate::compile::Source::Arg(at) => call.1.get(at),
            crate::compile::Source::Result => Some(&call.2),
            crate::compile::Source::Nothing => None,
        };
        value.map_or(Some(0), |value| match measure {
            doc::Measure::Size => crate::data::size(arena, value).ok(),
            doc::Measure::DeepSize => crate::data::deep_size(arena, value).ok(),
            doc::Measure::NestedSize => crate::data::nested_size(arena, value).ok(),
            doc::Measure::Magnitude => Some(crate::data::magnitude(arena, value)),
        })
    })
}
