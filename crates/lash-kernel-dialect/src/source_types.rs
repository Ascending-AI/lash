//! Deterministic operand facts from source traversal, without library resolution.
use lash_kernel_doc as k;
use std::collections::BTreeMap;

/// Only facts needed to distinguish native operator identities.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Unknown,
    Number,
    Text,
    Bool,
}

/// A function-local environment; printer and reader visit identical stored nodes.
#[derive(Default)]
pub struct Types(pub BTreeMap<k::Name, Kind>);
impl Types {
    pub fn expression(
        &self,
        expr: &k::Expr,
        functions: &BTreeMap<k::FunctionId, k::FunctionName>,
    ) -> Kind {
        match expr {
            k::Expr::Literal(k::Literal::Int(_) | k::Literal::Float(_)) => Kind::Number,
            k::Expr::Literal(k::Literal::Text(_)) => Kind::Text,
            k::Expr::Literal(k::Literal::Bool(_)) => Kind::Bool,
            k::Expr::Variable(name) => self.0.get(name).copied().unwrap_or_default(),
            k::Expr::Call { function, .. } => functions
                .get(function)
                .map_or(Kind::Unknown, |name| result_kind(name.as_str())),
            _ => Kind::Unknown,
        }
    }
    pub fn rhs(
        &self,
        value: &k::Rhs,
        functions: &BTreeMap<k::FunctionId, k::FunctionName>,
    ) -> Kind {
        match value {
            k::Rhs::Expr(expr) => self.expression(expr, functions),
            k::Rhs::Action(k::Action::Perform {
                result: k::Type::Text,
                ..
            }) => Kind::Text,
            k::Rhs::Action(k::Action::Perform {
                result: k::Type::Int | k::Type::Float | k::Type::Number,
                ..
            }) => Kind::Number,
            k::Rhs::Action(k::Action::Perform {
                result: k::Type::Bool,
                ..
            }) => Kind::Bool,
            k::Rhs::Action(k::Action::Call {
                callee: k::Callee::Library(id),
                ..
            }) => functions
                .get(id)
                .map_or(Kind::Unknown, |name| result_kind(name.as_str())),
            _ => Kind::Unknown,
        }
    }
    pub fn statement(
        &mut self,
        stmt: &k::Stmt,
        functions: &BTreeMap<k::FunctionId, k::FunctionName>,
    ) {
        match stmt {
            k::Stmt::Let { name, value }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value,
                ..
            } => {
                let kind = self.rhs(value, functions);
                self.0.insert(name.clone(), kind);
            }
            _ => {}
        }
    }
    /// A `same(kind(x), tag)` or `eq(kind(x), tag)` guard proves x's kind in
    /// its then-block; the caller restores the returned entry afterwards.
    pub fn narrow(
        &mut self,
        condition: &k::Expr,
        functions: &BTreeMap<k::FunctionId, k::FunctionName>,
    ) -> Option<(k::Name, Option<Kind>)> {
        let k::Expr::Call { function, args } = condition else {
            return None;
        };
        if !matches!(
            functions.get(function).map(|name| name.as_str()),
            Some("same" | "eq")
        ) {
            return None;
        }
        let [a, b] = args.as_slice() else {
            return None;
        };
        let (x, tag) = if let (Some(x), Some(tag)) = (kind_call(a, functions), text_tag(b)) {
            (x, tag)
        } else if let (Some(x), Some(tag)) = (kind_call(b, functions), text_tag(a)) {
            (x, tag)
        } else {
            return None;
        };
        let kind = match tag {
            "integer" | "float" => Kind::Number,
            "text" => Kind::Text,
            "bool" => Kind::Bool,
            _ => return None,
        };
        let previous = self.0.insert(x.clone(), kind);
        Some((x.clone(), previous))
    }
    /// Undoes a `narrow`: the guarded name reverts to its prior fact.
    pub fn restore(&mut self, saved: Option<(k::Name, Option<Kind>)>) {
        match saved {
            Some((x, Some(kind))) => {
                self.0.insert(x, kind);
            }
            Some((x, None)) => {
                self.0.remove(&x);
            }
            None => {}
        }
    }
}
fn kind_call<'a>(
    expr: &'a k::Expr,
    functions: &BTreeMap<k::FunctionId, k::FunctionName>,
) -> Option<&'a k::Name> {
    let k::Expr::Call { function, args } = expr else {
        return None;
    };
    if functions
        .get(function)
        .is_some_and(|name| name.as_str() == "kind")
        && let [k::Expr::Variable(x)] = args.as_slice()
    {
        return Some(x);
    }
    None
}
fn text_tag(expr: &k::Expr) -> Option<&str> {
    if let k::Expr::Literal(k::Literal::Text(tag)) = expr {
        return Some(tag.as_str());
    }
    None
}
fn result_kind(label: &str) -> Kind {
    match label {
        "add" | "sub" | "mul" | "neg" | "int.add" | "int.sub" | "int.mul" | "py.int"
        | "num.add" | "num.sub" | "num.mul" | "num.div" | "num.rem_trunc" | "num.neg" => {
            Kind::Number
        }
        "text.concat" | "text.join" | "py.show" | "py.str" | "py.type" => Kind::Text,
        "int.lt" | "int.le" | "eq" | "same" | "lt" | "le" | "gt" | "ge" | "num.lt" | "num.le"
        | "bool.not" => Kind::Bool,
        _ => Kind::Unknown,
    }
}
