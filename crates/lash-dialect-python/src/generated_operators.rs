//! Native tokens resolve by operand facts, then by a unique full identity.
use lash_kernel_dialect::source_types::{Kind, Types};
use lash_kernel_doc as k;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct Operators {
    functions: BTreeMap<k::FunctionId, k::FunctionName>,
    candidates: BTreeMap<&'static str, Vec<k::FunctionId>>,
}
impl Operators {
    pub(crate) fn new(functions: &BTreeMap<k::FunctionId, k::FunctionName>) -> Self {
        let mut candidates: BTreeMap<&'static str, Vec<k::FunctionId>> = BTreeMap::new();
        for (id, name) in functions {
            if let Some(token) = spelling(name.as_str()) {
                candidates.entry(token).or_default().push(*id);
            }
        }
        Self {
            functions: functions.clone(),
            candidates,
        }
    }
    pub(crate) fn functions(&self) -> &BTreeMap<k::FunctionId, k::FunctionName> {
        &self.functions
    }
    pub(crate) fn id(&self, token: &str, args: &[k::Expr], types: &Types) -> Option<k::FunctionId> {
        let candidates = self.candidates.get(token)?;
        if let [id] = candidates.as_slice() {
            return Some(*id);
        }
        let numeric = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Number);
        let text = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Text);
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|id| match self.functions[id].as_str() {
                "add" | "sub" | "mul" | "neg" | "int.add" | "int.sub" | "int.mul" => numeric,
                "text.concat" => text,
                "py.add" | "py.sub" | "py.mul" | "py.neg" => !numeric && !text,
                _ => true,
            })
            .collect();
        match candidates.as_slice() {
            [id] => Some(**id),
            _ => None,
        }
    }
    pub(crate) fn token(
        &self,
        id: &k::FunctionId,
        args: &[k::Expr],
        types: &Types,
    ) -> Option<&'static str> {
        let token = spelling(self.functions.get(id)?.as_str())?;
        let arity = if matches!(token, "not" | "neg") { 1 } else { 2 };
        (args.len() == arity && self.id(token, args, types) == Some(*id)).then_some(token)
    }
}
fn spelling(label: &str) -> Option<&'static str> {
    Some(match label {
        "add" | "int.add" | "text.concat" | "py.add" => "+",
        "sub" | "int.sub" | "py.sub" => "-",
        "mul" | "int.mul" | "py.mul" => "*",
        "py.truediv" => "/",
        "py.floordiv" => "//",
        "py.mod" => "%",
        "py.pow" => "**",
        "eq" => "==",
        "same" => "is",
        "lt" | "int.lt" => "<",
        "le" | "int.le" => "<=",
        "gt" => ">",
        "ge" => ">=",
        "bool.not" => "not",
        "neg" | "py.neg" => "neg",
        _ => return None,
    })
}
