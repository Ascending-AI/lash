//! The loop's semantic graph comes from original ops. Ordinary packet
//! replay eligibility does not authorize or forbid a whole source iteration.
use super::*;
use crate::compile::{Atom, Callee as RootCallee, Constant, Form, StmtId, Var, expr};
use lash_kernel_exec::Op;

pub(super) struct LoopSyntax {
    pub(super) id: StmtId,
    pub(super) cond: Expr,
    pub(super) body: Vec<Write>,
    pub(super) declares: Vec<u32>,
}
pub(super) struct Write {
    pub(super) id: StmtId,
    pub(super) at: u32,
    pub(super) bind: bool,
    pub(super) root: bool,
    pub(super) value: Expr,
}
impl LoopSyntax {
    pub(super) fn read(exe: &Executable, id: StmtId) -> Option<Self> {
        let Form::While { test, body, .. } = &exe.stmt(id).form else {
            return None;
        };
        let (stream, _, _) = exe.statement(id);
        let mut parsed = expr::statement(stream, *test, false)?;
        let Op::Test { cond } = parsed.last else {
            return None;
        };
        let cond = parsed.take(cond)?;
        if !parsed.spent() {
            return None;
        }
        let block = exe.block(*body);
        Some(Self {
            id,
            cond,
            body: block
                .stmts
                .iter()
                .map(|id| Write::read(exe, *id))
                .collect::<Option<_>>()?,
            declares: block.declares.clone(),
        })
    }
}
impl Write {
    fn read(exe: &Executable, id: StmtId) -> Option<Self> {
        let (stream, statement, _) = exe.statement(id);
        let root = matches!(statement.form, Form::Action(_));
        let mut parsed = expr::statement(stream, statement.ops, root)?;
        let (local, src, bind) = match parsed.last {
            Op::Bind { local, src } => (local, src, true),
            Op::WriteLocal { local, src } => (local, src, false),
            _ => return None,
        };
        let mut value = parsed.take(src)?;
        if !parsed.spent() {
            return None;
        }
        if root {
            if !matches!(unpin(&value).kind, Expression::Act) {
                return None;
            }
            value = root_call(exe, id)?;
        }
        Some(Self {
            id,
            at: local.at,
            bind,
            root,
            value,
        })
    }
}
fn root_call(exe: &Executable, id: StmtId) -> Option<Expr> {
    let statement = exe.stmt(id);
    let Form::Action(action) = &statement.form else {
        return None;
    };
    let crate::compile::ActionKind::Call { callee, args } = &action.kind else {
        return None;
    };
    let pc = statement.ops.start;
    let args = args
        .iter()
        .enumerate()
        .map(|(reg, atom)| {
            let kind = match atom {
                Atom::Var(Var::Local(local)) => Expression::Local {
                    local: lash_kernel_exec::LocalRef {
                        slot: local.slot,
                        at: local.at,
                    },
                    scoped: false,
                    miss: pc,
                },
                Atom::Literal(Constant::Resident(value @ (Value::Int(_) | Value::Float(_)))) => {
                    Expression::Const(*value)
                }
                _ => return None,
            };
            Some(Expr {
                pc,
                read: None,
                reg: reg as u32,
                kind,
            })
        })
        .collect::<Option<Box<[_]>>>()?;
    let kind = match callee {
        RootCallee::Declared(code) => Expression::Call {
            invoke: expr::Invoke {
                checked: false,
                act: None,
                occurrence: action.occurrence,
                guest: action.guest,
                call: true,
                operator: false,
            },
            callee: expr::Callee::Declared(*code),
            args,
            closes: Box::default(),
            site: u32::MAX,
        },
        RootCallee::Library(lib) | RootCallee::CheckedHelper(lib) => match exe.lib(*lib).run {
            LibRun::Body(code) => Expression::Call {
                invoke: expr::Invoke {
                    checked: matches!(callee, RootCallee::CheckedHelper(_)),
                    act: None,
                    occurrence: action.occurrence,
                    guest: action.guest,
                    call: true,
                    operator: false,
                },
                callee: expr::Callee::Declared(code),
                args,
                closes: Box::default(),
                site: u32::MAX,
            },
            LibRun::Native { .. } => Expression::Operator {
                lib: *lib,
                invoke: None,
                args,
                closes: Box::default(),
            },
            _ => return None,
        },
        _ => return None,
    };
    Some(Expr {
        pc,
        read: None,
        reg: 0,
        kind,
    })
}
