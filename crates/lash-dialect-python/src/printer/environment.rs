//! An exported function rematerializes the module declarations it closes over.
use super::{Renderer, k};
use lash_kernel_value::Name;

impl Renderer<'_> {
    pub(super) fn module_environment(&mut self, body: &[k::Stmt]) -> usize {
        let mut pending = vec![k::Node::Block(body)];
        while let Some(node) = pending.pop() {
            if let k::Node::Expr(k::Expr::Act(act)) = node
                && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, "py.name_check"))
                && let [
                    k::Expr::Variable(kernel),
                    k::Expr::Literal(k::Literal::Text(source)),
                    _,
                ] = act.args.as_slice()
                && kernel
                    .as_str()
                    .strip_prefix("module")
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    })
            {
                self.aliases.insert(kernel.clone(), Name::new(source));
            }
            pending.extend(node.children());
        }
        let mut initialized = std::collections::BTreeSet::new();
        let main = self.document.main.as_deref().unwrap_or_default();
        body.iter().take_while(|statement| match statement {
            k::Stmt::Let { name, value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)) } => self.aliases.contains_key(name),
            k::Stmt::Assign { place: k::Place::Variable(name), value, .. } => self.aliases.get(name).is_some_and(|source| {
                initialized.insert(name.clone()) && main.iter().any(|statement| matches!(statement, k::Stmt::Let { name, value: original } if name == source && value == original))
            }),
            _ => false,
        }).count()
    }
}
