//! Invert captured task coroutines using exact helper and binding identities.
use super::k;

pub(super) fn target(callee: &k::Callee) -> Option<k::FunctionTarget> {
    match callee {
        k::Callee::Local(id) => Some(k::FunctionTarget::Local(*id)),
        k::Callee::Import(slot) => Some(k::FunctionTarget::Import(*slot)),
        _ => None,
    }
}

impl super::Renderer<'_> {
    /// Resolve an ordered lexical capture without evaluating or renaming it.
    pub(super) fn resolved<'a>(&'a self, mut value: &'a k::Expr) -> Option<&'a k::Expr> {
        let mut seen = std::collections::BTreeSet::new();
        while let k::Expr::Variable(name) = value {
            let Some(bound) = self.substitutions.get(name) else {
                break;
            };
            if !seen.insert(name) {
                return None;
            }
            value = bound;
        }
        Some(value)
    }

    /// A task's synthetic zero-argument closure has exactly one return.
    /// Unwrap it using the captured binding's identity, not its position.
    fn captured_coroutine(&self, spawn: &k::ActExpr) -> Option<k::ActExpr> {
        let k::ActKind::Spawn(callee) = &spawn.kind else {
            return None;
        };
        if !self.operators.is(&target(callee)?, "py.task.run") {
            return None;
        }
        let [work] = spawn.args.as_slice() else {
            return None;
        };
        let k::Expr::Closure(closure) = self.resolved(work)? else {
            return None;
        };
        if closure.presentation.is_some() {
            return None;
        }
        let definition = self.document.functions.get(&closure.function)?;
        if !definition.signature.params.is_empty() {
            return None;
        }
        let [
            k::Stmt::Return {
                value: k::Expr::Act(action),
            },
        ] = definition.body()?.block.as_slice()
        else {
            return None;
        };
        if let k::ActKind::Call(callee) = &action.kind
            && target(callee).is_some_and(|target| self.operators.is(&target, "py.reply"))
        {
            let [k::Expr::Act(performed)] = action.args.as_slice() else {
                return None;
            };
            return Some(*performed.clone());
        }
        Some(*action.clone())
    }

    pub(super) fn task_coroutine(&mut self, spawn: &k::ActExpr) {
        let Some(action) = self.captured_coroutine(spawn) else {
            self.out.refuse(super::PrintError::Invalid(
                "task has no authored coroutine".into(),
            ));
            return;
        };
        if let k::ActKind::Call(callee) = &action.kind
            && target(callee).is_some_and(|target| self.operators.is(&target, "py.call"))
            && self.packed_call(&action.args, false)
        {
            return;
        }
        match &action.kind {
            k::ActKind::Call(k::Callee::Value(name)) => {
                let callee = k::Expr::Variable(name.clone());
                let Some(k::Expr::Variable(name)) = self.resolved(&callee).cloned() else {
                    self.out.refuse(super::PrintError::Invalid(
                        "task callee has no authored name".into(),
                    ));
                    return;
                };
                self.name(&name);
            }
            k::ActKind::Perform {
                slot,
                then_yield: false,
                ..
            } => {
                let Some(import) = self.document.imports.get(slot) else {
                    return;
                };
                self.operation_path(&import.name.to_string());
            }
            k::ActKind::Call(callee)
                if target(callee).is_some_and(|target| self.operators.is(&target, "py.sleep")) =>
            {
                self.out.write("asyncio.sleep");
            }
            _ => {
                self.out.refuse(super::PrintError::Invalid(
                    "task has no authored coroutine call".into(),
                ));
                return;
            }
        }
        self.out.write("(");
        self.authored_args(&action.args, true);
        self.out.write(")");
    }

    pub(super) fn gather(&mut self, args: &[k::Expr]) {
        let [tasks, exceptions] = args else {
            self.out.refuse(super::PrintError::Invalid(
                "gather needs its task list".into(),
            ));
            return;
        };
        self.out.write("await asyncio.gather(");
        match self.resolved(tasks).cloned() {
            Some(k::Expr::List(tasks)) => {
                for (index, task) in tasks.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    if let k::Expr::Act(spawn) = task
                        && self.captured_coroutine(spawn).is_some()
                    {
                        self.task_coroutine(spawn);
                    } else {
                        self.inline_expr(task);
                    }
                }
            }
            Some(_) => {
                self.out.write("*");
                self.inline_expr(tasks);
            }
            None => self
                .out
                .refuse(super::PrintError::Invalid("cyclic task capture".into())),
        }
        if exceptions != &k::Expr::Literal(k::Literal::Bool(false)) {
            self.out.write(", return_exceptions=");
            self.inline_expr(exceptions);
        }
        self.out.write(")");
    }
}
