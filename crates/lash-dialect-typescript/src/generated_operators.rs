//! Native operator syntax is an alias for an envelope-bound full FunctionId.
//! No live library or JavaScript-equivalence assumption is used to reconstruct it.
use lash_kernel_doc as k;
use std::collections::BTreeMap;
use swc_ecma_ast::{BinaryOp, UnaryOp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operator {
    Binary(BinaryOp),
    Unary(UnaryOp),
}
impl Operator {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Binary(op) => match op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Exp => "**",
                BinaryOp::EqEq => "==",
                BinaryOp::NotEq => "!=",
                BinaryOp::EqEqEq => "===",
                BinaryOp::NotEqEq => "!==",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::In => "in",
                BinaryOp::BitAnd => "&",
                BinaryOp::BitOr => "|",
                BinaryOp::BitXor => "^",
                BinaryOp::LShift => "<<",
                BinaryOp::RShift => ">>",
                BinaryOp::ZeroFillRShift => ">>>",
                _ => unreachable!("only registered operator aliases are rendered"),
            },
            Self::Unary(op) => match op {
                UnaryOp::Minus => "-",
                UnaryOp::Plus => "+",
                UnaryOp::Bang => "!",
                UnaryOp::Tilde => "~",
                _ => unreachable!("only registered unary aliases are rendered"),
            },
        }
    }
    pub(crate) fn precedence(self) -> u8 {
        match self {
            Self::Unary(_) => 15,
            Self::Binary(op) => match op {
                BinaryOp::BitOr => 5,
                BinaryOp::BitXor => 6,
                BinaryOp::BitAnd => 7,
                BinaryOp::EqEq | BinaryOp::NotEq | BinaryOp::EqEqEq | BinaryOp::NotEqEq => 8,
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq | BinaryOp::In => 9,
                BinaryOp::LShift | BinaryOp::RShift | BinaryOp::ZeroFillRShift => 10,
                BinaryOp::Add | BinaryOp::Sub => 11,
                BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 12,
                BinaryOp::Exp => 14,
                _ => unreachable!("only registered operator aliases are rendered"),
            },
        }
    }
}

/// A unique label binds one exact operator alias. Duplicate labels have no
/// unambiguous operator inverse and retain their ordinary helper calls.
pub(crate) fn bindings(manifest: &k::Manifest) -> BTreeMap<Operator, k::FunctionId> {
    let mut by_label = BTreeMap::new();
    for (id, name) in &manifest.functions {
        let ids = by_label.entry(name.as_str()).or_insert_with(Vec::new);
        ids.push(*id);
    }
    let mut result = BTreeMap::new();
    for (label, op) in [
        ("ts.add", BinaryOp::Add),
        ("ts.sub", BinaryOp::Sub),
        ("ts.mul", BinaryOp::Mul),
        ("ts.div", BinaryOp::Div),
        ("ts.rem", BinaryOp::Mod),
        ("ts.pow", BinaryOp::Exp),
        ("ts.loose_equals", BinaryOp::EqEq),
        ("ts.loose_not_equals", BinaryOp::NotEq),
        ("ts.strict_equals", BinaryOp::EqEqEq),
        ("ts.strict_not_equals", BinaryOp::NotEqEq),
        ("ts.lt", BinaryOp::Lt),
        ("ts.le", BinaryOp::LtEq),
        ("ts.gt", BinaryOp::Gt),
        ("ts.ge", BinaryOp::GtEq),
        ("ts.in", BinaryOp::In),
        ("ts.bit_and", BinaryOp::BitAnd),
        ("ts.bit_or", BinaryOp::BitOr),
        ("ts.bit_xor", BinaryOp::BitXor),
        ("ts.shl", BinaryOp::LShift),
        ("ts.shr", BinaryOp::RShift),
        ("ts.ushr", BinaryOp::ZeroFillRShift),
    ] {
        if let Some(ids) = by_label.get(label)
            && let [id] = ids.as_slice()
        {
            result.insert(Operator::Binary(op), *id);
        }
    }
    for (label, op) in [
        ("ts.neg", UnaryOp::Minus),
        ("ts.pos", UnaryOp::Plus),
        ("ts.not", UnaryOp::Bang),
        ("ts.bit_not", UnaryOp::Tilde),
    ] {
        if let Some(ids) = by_label.get(label)
            && let [id] = ids.as_slice()
        {
            result.insert(Operator::Unary(op), *id);
        }
    }
    result
}

impl Ord for Operator {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let key = |op: &Self| match op {
            Self::Binary(op) => (0, *op as u8),
            Self::Unary(op) => (1, *op as u8),
        };
        key(self).cmp(&key(other))
    }
}
impl PartialOrd for Operator {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
