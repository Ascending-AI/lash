//! Deterministic, injective name and full-identity helper aliases.
use lash_kernel_doc as k;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct NamePlan {
    pub names: BTreeMap<k::Name, String>,
    pub helpers: BTreeMap<k::FunctionId, String>,
}
impl NamePlan {
    /// First collect names in stable order: declaration names/parameters, then
    /// main and declaration bodies in Node child preorder, then metadata names.
    /// Allocate aliases only after reserving all legal original identifiers.
    pub fn new(document: &k::Document, legal: impl Fn(&str) -> bool) -> Self {
        let mut seen = BTreeSet::new();
        let mut ordered = Vec::new();
        let mut add = |name: &k::Name| {
            if seen.insert(name.clone()) {
                ordered.push(name.clone());
            }
        };
        for (name, function) in &document.functions {
            add(name);
            for param in &function.params {
                add(param);
            }
        }
        let mut ids: BTreeSet<_> = document.manifest.functions.keys().copied().collect();
        for root in std::iter::once(k::Node::Block(&document.main))
            .chain(document.functions.values().map(|f| k::Node::Block(&f.body)))
        {
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                match node {
                    k::Node::Stmt(k::Stmt::Let { name, .. }) => add(name),
                    k::Node::Stmt(k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        ..
                    }) => add(name),
                    k::Node::Stmt(k::Stmt::For { binding, .. }) => add(binding),
                    k::Node::Stmt(k::Stmt::Try(scope)) => {
                        if let Some(catch) = &scope.catch {
                            add(&catch.binding);
                        }
                    }
                    k::Node::Expr(
                        k::Expr::Variable(name) | k::Expr::Literal(k::Literal::Function(name)),
                    ) => add(name),
                    k::Node::Expr(k::Expr::Closure(closure)) => {
                        for name in &closure.params {
                            add(name);
                        }
                    }
                    k::Node::Expr(k::Expr::Call { function, .. }) => {
                        ids.insert(*function);
                    }
                    k::Node::Action(action) => {
                        let (callee, atoms): (Option<&k::Callee>, Vec<&k::Atom>) = match action {
                            k::Action::Call { callee, args }
                            | k::Action::Spawn { callee, args } => {
                                (Some(callee), args.iter().collect())
                            }
                            k::Action::Perform { args, .. } => (None, args.iter().collect()),
                            k::Action::Sleep { duration } => (None, vec![duration]),
                            k::Action::Join { task } | k::Action::Cancel { task } => {
                                (None, vec![task])
                            }
                            k::Action::JoinMany { tasks, .. } => (None, vec![tasks]),
                            k::Action::Yield => (None, vec![]),
                        };
                        match callee {
                            Some(k::Callee::Declared(name) | k::Callee::Value(name)) => add(name),
                            Some(k::Callee::Library(id)) => {
                                ids.insert(*id);
                            }
                            None => {}
                        }
                        for atom in atoms {
                            if let k::Atom::Variable(name)
                            | k::Atom::Literal(k::Literal::Function(name)) = atom
                            {
                                add(name);
                            }
                        }
                    }
                    _ => {}
                }
                stack.extend(node.children().into_iter().rev());
            }
        }
        for name in document.entries.keys().chain(&document.private_bindings) {
            add(name);
        }
        let mut used: BTreeSet<String> = ordered
            .iter()
            .filter(|n| legal(n.as_str()))
            .map(|n| n.as_str().to_owned())
            .collect();
        let mut next = 0usize;
        let names = ordered
            .into_iter()
            .map(|name| {
                let alias = if legal(name.as_str()) {
                    name.as_str().to_owned()
                } else {
                    loop {
                        next += 1;
                        let alias = format!("kernel_name_{next}");
                        if used.insert(alias.clone()) {
                            break alias;
                        }
                    }
                };
                (name, alias)
            })
            .collect();
        let mut used = BTreeSet::new();
        let helpers = ids
            .into_iter()
            .map(|id| {
                let label = document
                    .manifest
                    .functions
                    .get(&id)
                    .map_or("function".to_owned(), |name| {
                        name.as_str().replace('.', "_")
                    });
                let base = format!("helper_{label}");
                let mut alias = base.clone();
                let mut suffix = 1usize;
                while !used.insert(alias.clone()) {
                    suffix += 1;
                    alias = format!("{base}_{suffix}");
                }
                (id, alias)
            })
            .collect();
        Self { names, helpers }
    }
}
