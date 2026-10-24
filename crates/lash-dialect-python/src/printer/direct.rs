//! Recover the one authored body shared by an exact call and its packed adapter.
use super::{Renderer, k};
use lash_vocabulary::{FunctionId, Name};

fn binding(statement: &k::Stmt) -> Option<&Name> {
    match statement {
        k::Stmt::Let { name, .. }
        | k::Stmt::Assign {
            place: k::Place::Variable(name),
            ..
        } => Some(name),
        _ => None,
    }
}

impl Renderer<'_> {
    pub(super) fn direct_definition(&mut self, statements: &[k::Stmt]) -> usize {
        let (slot, body, source, prefix) = match statements {
            // Canonical declarations drop an unused absent initialization.
            [
                k::Stmt::Let {
                    name: temporary,
                    value: k::Rhs::Expr(k::Expr::Closure(body)),
                },
                k::Stmt::Let {
                    name: slot,
                    value: k::Rhs::Expr(k::Expr::Variable(value)),
                },
                source,
                ..,
            ] if value == temporary => (slot, body, source, 2),
            // A recursive closure captures its cell before the cell is filled.
            [
                k::Stmt::Let {
                    name: slot,
                    value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
                },
                k::Stmt::Let {
                    name: temporary,
                    value: k::Rhs::Expr(k::Expr::Closure(body)),
                },
                k::Stmt::Assign {
                    place: k::Place::Variable(target),
                    value: k::Rhs::Expr(k::Expr::Variable(value)),
                    ..
                },
                source,
                ..,
            ] if target == slot && value == temporary => (slot, body, source, 3),
            _ => return 0,
        };
        let Some(name) = binding(source) else {
            return 0;
        };
        let previous = self
            .direct
            .insert(slot.clone(), (name.clone(), body.function));
        for _ in 0..prefix {
            self.implicit();
        }
        if self.packed_definition(&statements[prefix..]) == 0 {
            match previous {
                Some(previous) => {
                    self.direct.insert(slot.clone(), previous);
                }
                None => {
                    self.direct.remove(slot);
                }
            }
            self.out.refuse(super::PrintError::Invalid(
                "direct body has no packed adapter".into(),
            ));
        }
        prefix + 1
    }

    pub(super) fn direct_body(&self, body: &[k::Stmt]) -> Option<FunctionId> {
        let [
            k::Stmt::Return {
                value: k::Expr::Act(call),
            },
        ] = body
        else {
            return None;
        };
        let k::ActKind::Call(k::Callee::Value(slot)) = &call.kind else {
            return None;
        };
        let (_, function) = self.direct.get(slot)?;
        let definition = self.document.functions.get(function)?;
        definition
            .signature
            .params
            .iter()
            .map(|param| k::Expr::Variable(param.name.clone()))
            .eq(call.args.iter().cloned())
            .then_some(*function)
    }

    pub(super) fn direct_call(
        &mut self,
        callee: &k::Callee,
        args: &[k::Expr],
        awaited: bool,
    ) -> bool {
        let k::Callee::Value(slot) = callee else {
            return false;
        };
        let Some((name, function)) = self.direct.get(slot).cloned() else {
            return false;
        };
        let Some(definition) = self.document.functions.get(&function) else {
            return false;
        };
        if args.len() != definition.signature.params.len() {
            return false;
        }
        let Some((_, positional)) = args.split_last() else {
            return false;
        };
        if awaited && self.asynchronous.contains(&name) {
            self.out.write("await ");
        }
        self.name(&name);
        self.out.write("(");
        self.authored_args(positional, true);
        self.out.write(")");
        true
    }
}
