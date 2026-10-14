//! Generated operators are aliases for one full identity in the envelope.
//! Ambiguous labels keep direct helper calls; no installed library is consulted.
use lash_kernel_doc::{FunctionId, FunctionName};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct Operators(BTreeMap<&'static str, FunctionId>);
impl Operators {
    pub(crate) fn new(functions: &BTreeMap<FunctionId, FunctionName>) -> Self {
        let mut candidates: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (id, name) in functions {
            let token = match name.as_str() {
                "add" | "text.concat" | "py.add" => "+",
                "sub" | "py.sub" => "-",
                "mul" | "py.mul" => "*",
                "py.truediv" => "/",
                "py.floordiv" => "//",
                "py.mod" => "%",
                "py.pow" => "**",
                "eq" => "==",
                "same" => "is",
                "lt" => "<",
                "le" => "<=",
                "gt" => ">",
                "ge" => ">=",
                "bool.not" => "not",
                "neg" | "py.neg" => "neg",
                _ => continue,
            };
            candidates.entry(token).or_default().push(*id);
        }
        Self(
            candidates
                .into_iter()
                .filter_map(|(token, ids)| match ids.as_slice() {
                    [id] => Some((token, *id)),
                    _ => None,
                })
                .collect(),
        )
    }
    pub(crate) fn id(&self, token: &str) -> Option<FunctionId> {
        self.0.get(token).copied()
    }
    pub(crate) fn token(&self, id: &FunctionId, arity: usize) -> Option<&'static str> {
        self.0.iter().find_map(|(token, candidate)| {
            let expected = if matches!(*token, "not" | "neg") {
                1
            } else {
                2
            };
            (*candidate == *id && arity == expected).then_some(*token)
        })
    }
}
