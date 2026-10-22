//! Inverse of the captured operation coroutine created by `asyncio.create_task`.
use super::k;
use crate::generated_operators::Operators;
use std::collections::BTreeMap;

pub(super) fn operation(
    document: &k::Document,
    operators: &Operators,
    expression: &k::Expr,
) -> Option<k::ActExpr> {
    let mut bindings = BTreeMap::new();
    let mut tail = expression;
    while let k::Expr::Let { name, value, body } = tail {
        bindings.insert(name, value.as_ref());
        tail = body;
    }
    let k::Expr::Act(spawn) = tail else {
        return None;
    };
    let k::ActKind::Spawn(run) = &spawn.kind else {
        return None;
    };
    if !operators.is(&target(run)?, "py.task.run") {
        return None;
    }
    let [k::Expr::Variable(work)] = spawn.args.as_slice() else {
        return None;
    };
    let k::Expr::Closure(closure) = bindings.get(work)? else {
        return None;
    };
    let definition = document.functions.get(&closure.function)?;
    if !definition.signature.params.is_empty() {
        return None;
    }
    let [
        k::Stmt::Return {
            value: k::Expr::Act(reply),
        },
    ] = definition.body()?.block.as_slice()
    else {
        return None;
    };
    let k::ActKind::Call(callee) = &reply.kind else {
        return None;
    };
    if !operators.is(&target(callee)?, "py.reply") {
        return None;
    }
    let [k::Expr::Act(performed)] = reply.args.as_slice() else {
        return None;
    };
    if !matches!(
        performed.kind,
        k::ActKind::Perform {
            then_yield: false,
            ..
        }
    ) {
        return None;
    }
    let mut operation = performed.as_ref().clone();
    for argument in &mut operation.args {
        if let k::Expr::Variable(name) = argument
            && let Some(value) = bindings.get(name)
        {
            *argument = (*value).clone();
        }
    }
    Some(operation)
}

fn target(callee: &k::Callee) -> Option<k::FunctionTarget> {
    match callee {
        k::Callee::Local(id) => Some(k::FunctionTarget::Local(*id)),
        k::Callee::Import(slot) => Some(k::FunctionTarget::Import(*slot)),
        _ => None,
    }
}
