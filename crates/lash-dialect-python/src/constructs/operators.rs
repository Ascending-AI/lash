//! Source operations, their admission facts and their identity-checked inverses.
use crate::scope::Ty;
use ruff_python_ast::Operator;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eligibility {
    Numbers,
    Text,
    Dynamic,
    Always,
    NumericAssignment,
    Assignment,
}
#[derive(Clone, Copy)]
pub(crate) enum ResultType {
    Number,
    Text,
    Float,
    Int,
    Unknown,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    Native,
    Action,
    SymbolAction,
}
#[derive(Clone, Copy)]
pub(crate) enum Refusal {
    MatrixProduct,
}
#[derive(Clone, Copy)]
pub(crate) enum Inverse {
    Operator(&'static str),
    Assignment(&'static str),
    Refused(Refusal),
}
#[derive(Clone, Copy)]
pub(crate) struct Alternative {
    pub source: Operator,
    pub eligible: Eligibility,
    pub helper: &'static str,
    pub call: Call,
    pub result: ResultType,
    pub inverse: Inverse,
    pub aliases: &'static [&'static str],
}
macro_rules! op {
    ($source:ident, $eligible:ident, $helper:literal, $call:ident, $result:ident, $token:literal) => {
        Alternative {
            source: Operator::$source,
            eligible: Eligibility::$eligible,
            helper: $helper,
            call: Call::$call,
            result: ResultType::$result,
            inverse: Inverse::Operator($token),
            aliases: &[],
        }
    };
}
pub(crate) const BINARY: &[Alternative] = &[
    Alternative {
        aliases: &["int.add"],
        ..op!(Add, Numbers, "add", Native, Number, "+")
    },
    op!(Add, Text, "text.concat", Native, Text, "+"),
    Alternative {
        eligible: Eligibility::NumericAssignment,
        helper: "py.number.iadd",
        inverse: Inverse::Assignment("+"),
        ..op!(Add, Always, "py.add", Native, Unknown, "+")
    },
    Alternative {
        eligible: Eligibility::Assignment,
        helper: "py.iadd",
        inverse: Inverse::Assignment("+"),
        ..op!(Add, Always, "py.add", Action, Unknown, "+")
    },
    op!(Add, Dynamic, "py.add", Action, Unknown, "+"),
    Alternative {
        aliases: &["int.sub"],
        ..op!(Sub, Numbers, "sub", Native, Number, "-")
    },
    op!(Sub, Dynamic, "py.sub", Action, Unknown, "-"),
    Alternative {
        aliases: &["int.mul"],
        ..op!(Mult, Numbers, "mul", Native, Number, "*")
    },
    op!(Mult, Dynamic, "py.mul", Action, Unknown, "*"),
    op!(Div, Always, "py.truediv", Action, Float, "/"),
    op!(FloorDiv, Always, "py.floordiv", Action, Number, "//"),
    op!(Mod, Always, "py.mod", Action, Number, "%"),
    op!(Pow, Always, "py.pow", Action, Unknown, "**"),
    op!(BitOr, Always, "py.bitwise", SymbolAction, Unknown, "|"),
    op!(BitAnd, Always, "py.bitwise", SymbolAction, Unknown, "&"),
    op!(BitXor, Always, "py.bitwise", SymbolAction, Unknown, "^"),
    op!(LShift, Always, "py.shift", SymbolAction, Int, "<<"),
    op!(RShift, Always, "py.shift", SymbolAction, Int, ">>"),
    Alternative {
        inverse: Inverse::Refused(Refusal::MatrixProduct),
        ..op!(MatMult, Always, "", Native, Unknown, "@")
    },
];

impl Alternative {
    pub fn result(&self, left: Ty, right: Ty) -> Ty {
        match self.result {
            ResultType::Number if left == Ty::Int && right == Ty::Int => Ty::Int,
            ResultType::Number if left.is_number() && right.is_number() => Ty::Float,
            ResultType::Number | ResultType::Unknown => Ty::Unknown,
            ResultType::Text => Ty::Str,
            ResultType::Float => Ty::Float,
            ResultType::Int => Ty::Int,
        }
    }
    pub fn token(&self) -> Option<&'static str> {
        match self.inverse {
            Inverse::Operator(token) => Some(token),
            _ => None,
        }
    }
}

pub(crate) fn select(
    op: Operator,
    left: Ty,
    right: Ty,
    numeric_assignment: Option<bool>,
) -> Option<&'static Alternative> {
    let numbers = left.is_number() && right.is_number();
    let text = left == Ty::Str && right == Ty::Str;
    BINARY.iter().find(|entry| {
        entry.source == op
            && match entry.eligible {
                Eligibility::Numbers => numbers,
                Eligibility::Text => text,
                Eligibility::Dynamic | Eligibility::Always => true,
                Eligibility::NumericAssignment => {
                    !numbers && !text && numeric_assignment == Some(true)
                }
                Eligibility::Assignment => !numbers && !text && numeric_assignment.is_some(),
            }
    })
}

pub(crate) fn spelling(helper: &str) -> Option<&'static str> {
    BINARY
        .iter()
        .find(|entry| {
            (entry.helper == helper || entry.aliases.contains(&helper))
                && entry.call != Call::SymbolAction
        })
        .and_then(Alternative::token)
        .or_else(|| {
            COMPARISONS
                .iter()
                .find(|entry| {
                    !entry.negated
                        && !entry.swapped
                        && (entry.helper == helper || entry.aliases.contains(&helper))
                })
                .map(|entry| entry.inverse)
        })
        .or_else(|| {
            UNARY
                .iter()
                .find(|entry| {
                    entry.native == Some(helper)
                        || (entry.dynamic == helper
                            && entry.source == ruff_python_ast::UnaryOp::USub)
                })
                .map(|entry| entry.token)
        })
}

pub(crate) fn inverse_eligible(helper: &str, numbers: bool, text: bool) -> bool {
    BINARY
        .iter()
        .find(|entry| entry.helper == helper || entry.aliases.contains(&helper))
        .map(|entry| entry.eligible)
        .or_else(|| {
            UNARY.iter().find_map(|entry| {
                if entry.native == Some(helper) {
                    Some(entry.native_eligible)
                } else if entry.dynamic == helper {
                    Some(entry.dynamic_inverse_eligible)
                } else {
                    None
                }
            })
        })
        .is_none_or(|eligible| eligible.inverse_matches(numbers, text))
}

impl Eligibility {
    fn inverse_matches(self, numbers: bool, text: bool) -> bool {
        match self {
            Self::Numbers => numbers,
            Self::Text => text,
            Self::Dynamic => !numbers && !text,
            _ => true,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Comparison {
    pub source: ruff_python_ast::CmpOp,
    pub helper: &'static str,
    pub native: bool,
    pub swapped: bool,
    pub negated: bool,
    pub inverse: &'static str,
    pub aliases: &'static [&'static str],
}
impl Comparison {
    pub fn for_receiver(mut self, proven: Ty) -> Self {
        if self.swapped {
            let native = match proven {
                Ty::Dict => Some("map.contains"),
                Ty::Set => Some("set.contains"),
                Ty::List => Some("list.contains"),
                _ => None,
            };
            if let Some(helper) = native {
                self.helper = helper;
                self.native = true;
            }
        }
        self
    }
}
macro_rules! comparison {
    ($source:ident, $helper:literal, $native:literal, $swapped:literal, $negated:literal, $inverse:literal, [$($alias:literal),*]) => {
        Comparison { source: ruff_python_ast::CmpOp::$source, helper: $helper, native: $native, swapped: $swapped, negated: $negated, inverse: $inverse, aliases: &[$($alias),*] }
    };
}
pub(crate) const COMPARISONS: &[Comparison] = &[
    comparison!(Eq, "py.equal", false, false, false, "==", ["eq"]),
    comparison!(NotEq, "py.equal", false, false, true, "!=", []),
    comparison!(Lt, "lt", true, false, false, "<", ["int.lt"]),
    comparison!(LtE, "le", true, false, false, "<=", ["int.le"]),
    comparison!(Gt, "gt", true, false, false, ">", []),
    comparison!(GtE, "ge", true, false, false, ">=", []),
    comparison!(Is, "py.same", false, false, false, "is", ["same"]),
    comparison!(IsNot, "py.same", false, false, true, "is not", []),
    comparison!(In, "py.contains", false, true, false, "in", []),
    comparison!(NotIn, "py.contains", false, true, true, "not in", []),
];

#[derive(Clone, Copy)]
pub(crate) struct Unary {
    pub source: ruff_python_ast::UnaryOp,
    pub token: &'static str,
    pub native: Option<&'static str>,
    pub native_eligible: Eligibility,
    pub dynamic: &'static str,
    pub dynamic_inverse_eligible: Eligibility,
}
pub(crate) const UNARY: &[Unary] = &[
    Unary {
        source: ruff_python_ast::UnaryOp::Not,
        token: "not",
        native: Some("bool.not"),
        native_eligible: Eligibility::Always,
        dynamic: "py.truth",
        dynamic_inverse_eligible: Eligibility::Always,
    },
    Unary {
        source: ruff_python_ast::UnaryOp::USub,
        token: "neg",
        native: Some("neg"),
        native_eligible: Eligibility::Numbers,
        dynamic: "py.neg",
        dynamic_inverse_eligible: Eligibility::Dynamic,
    },
    Unary {
        source: ruff_python_ast::UnaryOp::UAdd,
        token: "+",
        native: None,
        native_eligible: Eligibility::Always,
        dynamic: "py.pos",
        dynamic_inverse_eligible: Eligibility::Always,
    },
    Unary {
        source: ruff_python_ast::UnaryOp::Invert,
        token: "~",
        native: None,
        native_eligible: Eligibility::Always,
        dynamic: "py.invert",
        dynamic_inverse_eligible: Eligibility::Always,
    },
];

impl Unary {
    pub fn native_for(&self, ty: Ty) -> Option<&'static str> {
        self.native.filter(|_| {
            self.native_eligible
                .inverse_matches(ty.is_number(), ty == Ty::Str)
        })
    }
}

pub(crate) fn unary(source: ruff_python_ast::UnaryOp) -> &'static Unary {
    UNARY
        .iter()
        .find(|entry| entry.source == source)
        .unwrap_or_else(|| unreachable!("every unary construct is declared"))
}
