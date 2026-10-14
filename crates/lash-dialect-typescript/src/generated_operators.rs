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

#[derive(Default)]
pub(crate) struct Operators {
    candidates: BTreeMap<Operator, Vec<k::FunctionId>>,
    functions: BTreeMap<k::FunctionId, k::FunctionName>,
}
pub(crate) fn bindings(manifest: &k::Manifest) -> Operators {
    let mut result: BTreeMap<Operator, Vec<k::FunctionId>> = BTreeMap::new();
    for (label, op) in [
        ("ts.add", BinaryOp::Add),
        ("num.add", BinaryOp::Add),
        ("text.concat", BinaryOp::Add),
        ("num.sub", BinaryOp::Sub),
        ("num.mul", BinaryOp::Mul),
        ("num.div", BinaryOp::Div),
        ("num.rem_trunc", BinaryOp::Mod),
        ("num.lt", BinaryOp::Lt),
        ("num.le", BinaryOp::LtEq),
        ("eq", BinaryOp::EqEqEq),
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
        for (id, name) in &manifest.functions {
            if name.as_str() == label {
                result.entry(Operator::Binary(op)).or_default().push(*id);
            }
        }
    }
    for (label, op) in [
        ("ts.neg", UnaryOp::Minus),
        ("num.neg", UnaryOp::Minus),
        ("bool.not", UnaryOp::Bang),
        ("ts.pos", UnaryOp::Plus),
        ("ts.not", UnaryOp::Bang),
        ("ts.bit_not", UnaryOp::Tilde),
    ] {
        for (id, name) in &manifest.functions {
            if name.as_str() == label {
                result.entry(Operator::Unary(op)).or_default().push(*id);
            }
        }
    }
    Operators {
        candidates: result,
        functions: manifest.functions.clone(),
    }
}
impl Operators {
    pub(crate) fn functions(&self) -> &BTreeMap<k::FunctionId, k::FunctionName> {
        &self.functions
    }
    pub(crate) fn id(
        &self,
        op: Operator,
        args: &[k::Expr],
        types: &lash_kernel_dialect::source_types::Types,
    ) -> Option<k::FunctionId> {
        use lash_kernel_dialect::source_types::Kind;
        let candidates = self.candidates.get(&op)?;
        if let [id] = candidates.as_slice() {
            return Some(*id);
        }
        let numeric = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Number);
        let text = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Text);
        let boolean = args
            .iter()
            .all(|arg| types.expression(arg, &self.functions) == Kind::Bool);
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|id| {
                let label = self.functions[id].as_str();
                if label.starts_with("num.") {
                    numeric
                } else if label == "text.concat" {
                    text
                } else if label == "bool.not" {
                    boolean
                } else if label == "ts.not" {
                    !boolean
                } else if label == "eq" {
                    true
                } else {
                    !numeric && !text
                }
            })
            .collect();
        match candidates.as_slice() {
            [id] => Some(**id),
            _ => None,
        }
    }
    pub(crate) fn operator(
        &self,
        id: &k::FunctionId,
        args: &[k::Expr],
        types: &lash_kernel_dialect::source_types::Types,
    ) -> Option<Operator> {
        self.candidates.keys().copied().find(|op| {
            args.len()
                == if matches!(op, Operator::Unary(_)) {
                    1
                } else {
                    2
                }
                && self.id(*op, args, types) == Some(*id)
        })
    }
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
