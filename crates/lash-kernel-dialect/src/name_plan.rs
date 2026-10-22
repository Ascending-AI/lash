//! Deterministic source aliases, projected from the authored document.
use lash_kernel_doc as k;
use lash_vocabulary::{FunctionId, Name};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug)]
pub struct NamePlan {
    pub names: BTreeMap<Name, String>,
    pub helpers: BTreeMap<FunctionId, String>,
}
impl NamePlan {
    pub fn new(document: &k::Document, legal: impl Fn(&str) -> bool) -> Self {
        let roots = document
            .imports
            .values()
            .map(|import| import.name.package.as_slice()[0].as_str())
            .collect::<BTreeSet<_>>();
        let mut ordered = document
            .exports
            .keys()
            .chain(&document.private_bindings)
            .cloned()
            .collect::<BTreeSet<_>>();
        for definition in document.functions.values() {
            ordered.extend(
                definition
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone()),
            );
        }
        let mut pending = document
            .main
            .iter()
            .map(|block| k::Node::Block(block))
            .chain(
                document
                    .functions
                    .values()
                    .filter_map(|function| function.body())
                    .map(|body| k::Node::Block(&body.block)),
            )
            .collect::<Vec<_>>();
        while let Some(node) = pending.pop() {
            match node {
                k::Node::Expr(k::Expr::Variable(name) | k::Expr::Let { name, .. })
                | k::Node::Stmt(
                    k::Stmt::Let { name, .. }
                    | k::Stmt::For { binding: name, .. }
                    | k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        ..
                    },
                ) => {
                    ordered.insert(name.clone());
                }
                k::Node::Stmt(k::Stmt::Try(scope)) => {
                    if let Some(catch) = &scope.catch {
                        ordered.insert(catch.binding.clone());
                    }
                }
                _ => {}
            }
            pending.extend(node.children());
        }
        let mut taken = ordered
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        let mut next = 0u64;
        let names = ordered
            .into_iter()
            .map(|name| {
                let alias = if legal(name.as_str()) && !roots.contains(name.as_str()) {
                    name.to_string()
                } else {
                    loop {
                        next += 1;
                        let candidate = format!("value_{next}");
                        if taken.insert(candidate.clone()) {
                            break candidate;
                        }
                    }
                };
                (name, alias)
            })
            .collect();
        let helpers = document
            .functions
            .keys()
            .map(|id| (*id, format!("function_{id}")))
            .collect();
        Self { names, helpers }
    }
}
