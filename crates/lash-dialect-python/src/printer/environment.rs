//! An exported function rematerializes the module declarations it closes over.
use super::{Renderer, k};
use lash_kernel_value::Name;

impl Renderer<'_> {
    /// Invert the scope allocator only for slots proved to be generated.
    /// Authored identifiers, including ones ending in `_local1`, are untouched.
    pub(super) fn binding_aliases(&mut self) {
        let mut captured_writes = std::collections::BTreeSet::new();
        for function in self.document.functions.values() {
            let Some(body) = function.body() else {
                continue;
            };
            let mut locals = function
                .signature
                .params
                .iter()
                .map(|p| p.name.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let mut writes = std::collections::BTreeSet::new();
            let mut pending = vec![k::Node::Block(&body.block)];
            while let Some(node) = pending.pop() {
                match node {
                    k::Node::Stmt(
                        k::Stmt::Let { name, .. } | k::Stmt::For { binding: name, .. },
                    )
                    | k::Node::Expr(k::Expr::Let { name, .. }) => {
                        locals.insert(name.clone());
                    }
                    k::Node::Stmt(k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        ..
                    }) => {
                        writes.insert(name.clone());
                    }
                    _ => {}
                }
                pending.extend(node.children());
            }
            captured_writes.extend(
                writes
                    .difference(&locals)
                    .filter(|name| !self.module_binding(name))
                    .cloned(),
            );
        }
        for slot in captured_writes {
            // Only a rebound non-module slot can be assigned across a function
            // boundary. Its owner allocated it with fresh("{source}_local").
            if let Some((source, sequence)) = slot.as_str().rsplit_once("_local")
                && !sequence.is_empty()
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
                && super::attribute_identifier(source)
            {
                self.aliases.insert(slot.clone(), Name::new(source));
            }
        }
    }

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
